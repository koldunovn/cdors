//! SCRIP weight files (as written by `cdo gen*`) and their application.
//!
//! # What `cdo remap,<grid>,<weights.nc>` does with missing values (CDO 2.6.5)
//!
//! cdo applies the file's weights unchanged only to fields whose missing-value mask equals the
//! file's `src_grid_imask` (`remap_matches`, `src/operators/Remapgrid.cc`). For any other mask it
//! regenerates the weights with that mask (`remap_init` → `remap_gen_weights`) and applies those.
//! The application itself (`remap_first_order`, `src/remap_vars.cc`) is a plain sum
//! `dst[i] = Σ w·src` per destination cell in link order, in double precision; cells without links
//! keep the missing value. `remapcon` adds `remap_set_fracmin`, a no-op at the default
//! `REMAP_AREA_MIN=0`.
//!
//! The regenerated weights relate to the unmasked ones as follows, so cdors reproduces them from a
//! weight file generated *without* a mask (`gen.rs` always generates such files):
//!
//! * nn and dis (`src/remap_knn.cc:62`, `src/knndata.cc`): the k nearest source points are searched
//!   ignoring the mask, then masked points are dropped (`KnnData::apply_mask`) and the inverse
//!   distance weights `(1/d_i)/Σ(1/d)` are recomputed over the remaining ones; `kmin = 1`, so a
//!   destination is missing only when none remains. Rule: **renormalise over the valid links**
//!   (for nn: the destination is missing when its single source is missing).
//! * bil (`src/remap_bilinear.cc:142`, `remap_check_mask_indices` in `src/remaplib.cc:32`): when any
//!   of the four corners is masked the search result becomes 0 and the destination gets no links.
//!   Rule: **missing if any linked source is missing.** Exception: on regular (2-D) source grids a
//!   destination point outside the square search (near the poles) uses CDO's fallback, a
//!   distance-based average of the valid corners (`num_src_points` zeroes masked corners, then
//!   `renormalize_weights`), which is a renormalisation over the valid links. cdors flags rows as
//!   fallback rows when the destination latitude lies outside the range of source centre latitudes
//!   (the case in which `grid_search_square_reg2d` fails); rows failing the search for other
//!   reasons (non-cyclic regional source grids, longitude out of range) are not detected.
//! * con/ycon (`src/remap_conserv.cc:470-500`): masked source cells are removed from the overlap
//!   list (`remove_unmask_weights`) and, with `normalization = "fracarea"` (the default), the
//!   overlap areas are divided by the sum of the remaining ones. Rule for fracarea:
//!   **renormalise over the valid links**; for `destarea` and `none`: **drop missing links without
//!   renormalising**. A destination with no valid link is missing.
//!
//! These rules give cdo's result whenever every valid input cell is also valid in the weight file
//! (always true for unmasked weight files). For a precomputed file whose `src_grid_imask` excludes
//! cells that are valid in the data, cdo would regenerate weights that also use those cells;
//! cdors cannot, and ignores them ([`RemapWeights::excluded_valid_cells`] counts them).
//!
//! Missing values are NaN on input and output. Destination cells without links are NaN.

use std::path::Path;

use rayon::prelude::*;

use super::RemapError;

/// Interpolation method recorded in a weight file (`map_method`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapMethod {
    /// "Nearest neighbor"
    Nearest,
    /// "Distance weighted avg of nearest neighbors" (k = `num_neighbors`, default 4)
    Distance,
    /// "Bilinear remapping"
    Bilinear,
    /// "Conservative remapping", first order
    Conservative,
}

/// `normalization` attribute of a weight file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Normalization {
    FracArea,
    DestArea,
    None,
}

/// What to do with a destination cell some of whose linked sources are missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MissingRule {
    /// Σ w·x / Σ w over the valid links.
    Renormalise,
    /// Σ w·x over the valid links.
    Drop,
    /// Missing.
    AllOrNothing,
}

/// Field element types accepted and produced by the remapper.
pub trait Value: Copy + Send + Sync + 'static {
    fn to_f64(self) -> f64;
    fn from_f64(v: f64) -> Self;
}

impl Value for f32 {
    #[inline]
    fn to_f64(self) -> f64 {
        f64::from(self)
    }
    #[inline]
    fn from_f64(v: f64) -> Self {
        v as f32
    }
}

impl Value for f64 {
    #[inline]
    fn to_f64(self) -> f64 {
        self
    }
    #[inline]
    fn from_f64(v: f64) -> Self {
        v
    }
}

/// A SCRIP remapping matrix, stored as CSR by destination cell (links keep the file's order).
#[derive(Debug, Clone)]
pub struct RemapWeights {
    method: MapMethod,
    normalization: Normalization,
    src_size: usize,
    dst_size: usize,
    /// Row pointers, `dst_size + 1` entries.
    row_ptr: Vec<usize>,
    /// 0-based source index of each link.
    col: Vec<u32>,
    /// First weight column of each link.
    w: Vec<f64>,
    /// Per-row rule override: bilinear fallback rows that renormalise (empty unless bilinear).
    fallback_rows: Vec<bool>,
    /// `src_grid_imask != 0`.
    src_mask: Vec<bool>,
    rule: MissingRule,
}

/// Total link work below which a single field is remapped on one thread.
const PAR_MIN_WORK: usize = 1 << 16;
/// Destination rows per rayon task when splitting one field.
const ROW_CHUNK: usize = 2048;

impl RemapWeights {
    /// Read a SCRIP weight file written by `cdo gen*` (or any first-order SCRIP file).
    pub fn read(path: &Path) -> Result<Self, RemapError> {
        let nc_err = |source| RemapError::Netcdf {
            path: path.to_owned(),
            source,
        };
        let fmt_err = |msg: String| RemapError::Format {
            path: path.to_owned(),
            msg,
        };
        let _hdf5 = crate::io::hdf5_lock();
        let file = netcdf::open(path).map_err(nc_err)?;

        let text_att = |name: &str| -> Option<String> {
            match file.attribute(name)?.value().ok()? {
                netcdf::AttributeValue::Str(s) => Some(s),
                _ => None,
            }
        };
        let map_method = text_att("map_method").unwrap_or_default();
        let remap_order = match file.attribute("remap_order").and_then(|a| a.value().ok()) {
            Some(netcdf::AttributeValue::Int(i)) => i,
            Some(netcdf::AttributeValue::Short(i)) => i32::from(i),
            _ => 1,
        };
        let method = if map_method.starts_with("Conservative") {
            if remap_order != 1 {
                return Err(RemapError::Unsupported(format!(
                    "{}: second-order conservative weights are not supported",
                    path.display()
                )));
            }
            MapMethod::Conservative
        } else if map_method.starts_with("Bilinear") {
            MapMethod::Bilinear
        } else if map_method.starts_with("Nearest") {
            MapMethod::Nearest
        } else if map_method.starts_with("Distance") {
            MapMethod::Distance
        } else {
            return Err(RemapError::Unsupported(format!(
                "{}: map_method '{map_method}' is not supported (nn, dis, bil, con)",
                path.display()
            )));
        };
        let normalization = match text_att("normalization").as_deref() {
            Some("fracarea") => Normalization::FracArea,
            Some("destarea") => Normalization::DestArea,
            Some("none") | None => Normalization::None,
            Some(other) => return Err(fmt_err(format!("invalid normalization '{other}'"))),
        };

        let dim = |name: &str| -> Result<usize, RemapError> {
            file.dimension(name)
                .map(|d| d.len())
                .ok_or_else(|| fmt_err(format!("dimension {name} missing")))
        };
        let src_size = dim("src_grid_size")?;
        let dst_size = dim("dst_grid_size")?;
        let num_links = file.dimension("num_links").map_or(0, |d| d.len());
        let num_wgts = dim("num_wgts")?;
        if src_size > u32::MAX as usize {
            return Err(fmt_err("source grid larger than 2^32 cells".into()));
        }

        let var = |name: &str| {
            file.variable(name)
                .ok_or_else(|| fmt_err(format!("variable {name} missing")))
        };
        let (src_addr, dst_addr, matrix) = if num_links > 0 {
            let s: Vec<i64> = var("src_address")?.get_values(..).map_err(nc_err)?;
            let d: Vec<i64> = var("dst_address")?.get_values(..).map_err(nc_err)?;
            let m: Vec<f64> = var("remap_matrix")?.get_values(..).map_err(nc_err)?;
            (s, d, m)
        } else {
            (Vec::new(), Vec::new(), Vec::new())
        };
        if src_addr.len() != num_links || dst_addr.len() != num_links {
            return Err(fmt_err("address arrays do not match num_links".into()));
        }
        let src_mask: Vec<bool> = match file.variable("src_grid_imask") {
            Some(v) => v
                .get_values::<i32, _>(..)
                .map_err(nc_err)?
                .into_iter()
                .map(|m| m != 0)
                .collect(),
            None => vec![true; src_size],
        };

        // CSR by destination, stable (file order within a row).
        let mut row_ptr = vec![0usize; dst_size + 1];
        for (n, &d) in dst_addr.iter().enumerate() {
            let s = src_addr[n];
            if d < 1 || d as usize > dst_size || s < 1 || s as usize > src_size {
                return Err(fmt_err(format!(
                    "link {n} out of range (src {s}, dst {d}; 1-based)"
                )));
            }
            row_ptr[d as usize] += 1;
        }
        for i in 0..dst_size {
            row_ptr[i + 1] += row_ptr[i];
        }
        let mut fill = row_ptr.clone();
        let mut col = vec![0u32; num_links];
        let mut w = vec![0f64; num_links];
        for n in 0..num_links {
            let d = dst_addr[n] as usize - 1;
            let k = fill[d];
            fill[d] += 1;
            col[k] = (src_addr[n] - 1) as u32;
            w[k] = matrix[n * num_wgts];
        }

        let rule = match (method, normalization) {
            (MapMethod::Bilinear, _) => MissingRule::AllOrNothing,
            (MapMethod::Conservative, Normalization::FracArea) => MissingRule::Renormalise,
            (MapMethod::Conservative, _) => MissingRule::Drop,
            (MapMethod::Nearest | MapMethod::Distance, _) => MissingRule::Renormalise,
        };

        let fallback_rows = if method == MapMethod::Bilinear {
            bilinear_fallback_rows(&file, dst_size).map_err(nc_err)?
        } else {
            Vec::new()
        };

        Ok(Self {
            method,
            normalization,
            src_size,
            dst_size,
            row_ptr,
            col,
            w,
            fallback_rows,
            src_mask,
            rule,
        })
    }

    pub fn method(&self) -> MapMethod {
        self.method
    }
    pub fn normalization(&self) -> Normalization {
        self.normalization
    }
    pub fn src_size(&self) -> usize {
        self.src_size
    }
    pub fn dst_size(&self) -> usize {
        self.dst_size
    }
    pub fn num_links(&self) -> usize {
        self.col.len()
    }
    /// `src_grid_imask` of the file (true = valid source cell).
    pub fn src_mask(&self) -> &[bool] {
        &self.src_mask
    }

    /// Number of valid (non-NaN) input cells that the weight file's `src_grid_imask` excludes.
    /// Non-zero means cdo would regenerate weights for this field and its result can differ.
    pub fn excluded_valid_cells<T: Value>(&self, src: &[T]) -> usize {
        src.iter()
            .zip(&self.src_mask)
            .filter(|(v, m)| !**m && !v.to_f64().is_nan())
            .count()
    }

    /// Remap one horizontal field (`src.len() == src_size`, `dst.len() == dst_size`).
    /// Large fields are split over destination rows with rayon.
    pub fn apply<T: Value, U: Value>(&self, src: &[T], dst: &mut [U]) -> Result<(), RemapError> {
        self.check(src.len(), dst.len(), 1)?;
        if self.num_links() >= PAR_MIN_WORK {
            dst.par_chunks_mut(ROW_CHUNK)
                .enumerate()
                .for_each(|(c, out)| self.rows(src, c * ROW_CHUNK, out));
        } else {
            self.rows(src, 0, dst);
        }
        Ok(())
    }

    /// Remap a batch of fields stored back to back (`src.len() == n * src_size`,
    /// `dst.len() == n * dst_size`), e.g. all timesteps and levels of a variable. Parallel over
    /// fields when there are enough of them, otherwise over destination rows of each field.
    pub fn apply_batch<T: Value, U: Value>(
        &self,
        src: &[T],
        dst: &mut [U],
    ) -> Result<(), RemapError> {
        if self.src_size == 0 || self.dst_size == 0 {
            return self.check(src.len(), dst.len(), 0);
        }
        let n = src.len() / self.src_size;
        self.check(src.len(), dst.len(), n)?;
        if n >= 2 * rayon::current_num_threads() || self.num_links() < PAR_MIN_WORK {
            src.par_chunks(self.src_size)
                .zip(dst.par_chunks_mut(self.dst_size))
                .with_min_len((PAR_MIN_WORK / self.num_links().max(1)).max(1))
                .for_each(|(s, d)| self.rows(s, 0, d));
        } else {
            for (s, d) in src.chunks(self.src_size).zip(dst.chunks_mut(self.dst_size)) {
                self.apply(s, d)?;
            }
        }
        Ok(())
    }

    fn check(&self, src_len: usize, dst_len: usize, n: usize) -> Result<(), RemapError> {
        if src_len != n * self.src_size || (n == 0 && src_len != 0) {
            return Err(RemapError::Size {
                what: "source",
                expected: self.src_size,
                got: src_len,
            });
        }
        if dst_len != n * self.dst_size {
            return Err(RemapError::Size {
                what: "destination",
                expected: self.dst_size,
                got: dst_len,
            });
        }
        Ok(())
    }

    /// Destination rows `first .. first + out.len()` of one field.
    #[inline]
    fn rows<T: Value, U: Value>(&self, src: &[T], first: usize, out: &mut [U]) {
        for (j, o) in out.iter_mut().enumerate() {
            let i = first + j;
            let (a, b) = (self.row_ptr[i], self.row_ptr[i + 1]);
            if a == b {
                *o = U::from_f64(f64::NAN);
                continue;
            }
            let cols = &self.col[a..b];
            let ws = &self.w[a..b];
            // Fast path: NaN in any linked source propagates into the sum.
            let mut s = 0.0f64;
            for (&c, &w) in cols.iter().zip(ws) {
                s += src[c as usize].to_f64() * w;
            }
            if s.is_nan() {
                s = self.row_with_missing(src, i, cols, ws);
            }
            *o = U::from_f64(s);
        }
    }

    #[cold]
    fn row_with_missing<T: Value>(&self, src: &[T], i: usize, cols: &[u32], ws: &[f64]) -> f64 {
        let rule = if self.fallback_rows.get(i).copied().unwrap_or(false) {
            MissingRule::Renormalise
        } else {
            self.rule
        };
        if rule == MissingRule::AllOrNothing {
            return f64::NAN;
        }
        let (mut sv, mut sw, mut any) = (0.0f64, 0.0f64, false);
        for (&c, &w) in cols.iter().zip(ws) {
            let x = src[c as usize].to_f64();
            if !x.is_nan() {
                sv += x * w;
                sw += w;
                any = true;
            }
        }
        match rule {
            _ if !any => f64::NAN,
            MissingRule::Renormalise if sw != 0.0 => sv / sw,
            MissingRule::Renormalise => f64::NAN,
            _ => sv,
        }
    }
}

/// Bilinear rows whose destination latitude lies outside the source centre latitudes of a 2-D
/// source grid: there cdo's square search fails and it uses a renormalising distance average.
fn bilinear_fallback_rows(
    file: &netcdf::File,
    dst_size: usize,
) -> Result<Vec<bool>, netcdf::Error> {
    let rank = file.dimension("src_grid_rank").map_or(1, |d| d.len());
    let (Some(src_lat), Some(dst_lat)) = (
        file.variable("src_grid_center_lat"),
        file.variable("dst_grid_center_lat"),
    ) else {
        return Ok(Vec::new());
    };
    if rank != 2 {
        return Ok(Vec::new());
    }
    let s: Vec<f64> = src_lat.get_values(..)?;
    let d: Vec<f64> = dst_lat.get_values(..)?;
    let (lo, hi) = s
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &v| {
            (lo.min(v), hi.max(v))
        });
    // Both arrays carry the same units (radians or degrees) in a cdo-written file.
    let mut rows = vec![false; dst_size];
    for (r, &v) in rows.iter_mut().zip(&d) {
        *r = v < lo || v > hi;
    }
    Ok(rows)
}
