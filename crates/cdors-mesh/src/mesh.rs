//! The mesh vocabulary: places, incidence tables and the vertical.
//!
//! A [`Mesh`] is what every grid looks like to the mesh operators:
//! - [`Place`]s: the sets of locations data can live on (FESOM2: `nodes` and `elements`; a
//!   regular, Gaussian, HEALPix or unstructured grid: one place, `cells`), each with centres in
//!   degrees and a 2-D area, optionally per-level areas and cell polygons;
//! - named [`Table`]s: CSR incidence tables between places (`elem_nodes`, `edge_nodes`, ...);
//! - an optional [`Vertical`]: level interfaces and mid-levels, and per place the range of wet
//!   levels of each cell.
//!
//! **Laziness and memory.** Centres, 2-D areas and wet levels are loaded with the mesh (a few
//! arrays of the number of cells). Per-level areas (levels x cells, ~4 GB in f64 on FESOM2 ng5),
//! polygons and topology load on first use through a [`Lazy`] holding a loader closure (usually
//! over an [`crate::ArrayReader`]); once loaded they stay with the mesh. Every component knows
//! its size before it is loaded ([`Lazy::bytes`]), so a planner can add what an operator will
//! touch to its memory estimate ([`Mesh::resident_bytes`], [`Mesh::lazy_bytes`]).
//!
//! `Debug` prints sizes only: a mesh has millions of values.

use crate::{MeshError, Result};
use std::fmt;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

type Loader<T> = Box<dyn Fn() -> Result<T> + Send + Sync>;

/// A component loaded on first use. Its size in bytes is known in advance.
pub struct Lazy<T> {
    bytes: u64,
    cell: OnceLock<Arc<T>>,
    load: Option<Loader<T>>,
    lock: Mutex<()>,
}

impl<T> Lazy<T> {
    /// An already loaded value.
    pub fn ready(value: T, bytes: u64) -> Self {
        let cell = OnceLock::new();
        let _ = cell.set(Arc::new(value));
        Self {
            bytes,
            cell,
            load: None,
            lock: Mutex::new(()),
        }
    }

    /// A value that `load` produces on first use; `bytes` is its size once loaded.
    pub fn deferred(bytes: u64, load: impl Fn() -> Result<T> + Send + Sync + 'static) -> Self {
        Self {
            bytes,
            cell: OnceLock::new(),
            load: Some(Box::new(load)),
            lock: Mutex::new(()),
        }
    }

    /// The value, loading it on the first call (one load even with concurrent callers). A failed
    /// load is not remembered: the next call tries again.
    pub fn get(&self) -> Result<Arc<T>> {
        if let Some(v) = self.cell.get() {
            return Ok(v.clone());
        }
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(v) = self.cell.get() {
            return Ok(v.clone());
        }
        let load = self
            .load
            .as_ref()
            .ok_or_else(|| MeshError::Invalid("lazy component without a loader".into()))?;
        let v = Arc::new(load()?);
        let _ = self.cell.set(v.clone());
        Ok(v)
    }

    /// The value without keeping it: the loaded value if there is one, else a fresh load that
    /// is not remembered (for streaming a component through once, e.g. into the mesh cache,
    /// without leaving gigabytes of per-level areas resident).
    pub fn get_uncached(&self) -> Result<Arc<T>> {
        if let Some(v) = self.cell.get() {
            return Ok(v.clone());
        }
        let load = self
            .load
            .as_ref()
            .ok_or_else(|| MeshError::Invalid("lazy component without a loader".into()))?;
        Ok(Arc::new(load()?))
    }

    pub fn is_loaded(&self) -> bool {
        self.cell.get().is_some()
    }

    /// Size in bytes once loaded.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Bytes held now (0 until loaded).
    pub fn loaded_bytes(&self) -> u64 {
        if self.is_loaded() { self.bytes } else { 0 }
    }
}

impl<T> fmt::Debug for Lazy<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Lazy({} B, {})",
            self.bytes,
            if self.is_loaded() {
                "loaded"
            } else {
                "not loaded"
            }
        )
    }
}

/// Compressed sparse rows: row `r` holds the columns `col[row_ptr[r]..row_ptr[r + 1]]` and, for
/// weight matrices, their weights `w` (incidence tables have none). Used for incidence tables
/// (`elem → nodes`) and for the weights of the mesh operators (targets × cells).
#[derive(Clone, PartialEq)]
pub struct Csr {
    pub row_ptr: Vec<usize>,
    pub col: Vec<u32>,
    pub w: Option<Vec<f64>>,
    /// Number of columns (the size of the place the columns index).
    pub ncols: usize,
}

impl Csr {
    /// Checks the row pointers, the column range and the weight count.
    pub fn new(
        row_ptr: Vec<usize>,
        col: Vec<u32>,
        w: Option<Vec<f64>>,
        ncols: usize,
    ) -> Result<Self> {
        let bad = |m: String| Err(MeshError::Invalid(m));
        if row_ptr.first() != Some(&0) || row_ptr.last() != Some(&col.len()) {
            return bad(format!(
                "CSR row pointers must run from 0 to {} (the number of entries)",
                col.len()
            ));
        }
        if row_ptr.windows(2).any(|p| p[0] > p[1]) {
            return bad("CSR row pointers must not decrease".into());
        }
        if let Some(&c) = col.iter().find(|&&c| c as usize >= ncols) {
            return bad(format!("CSR column {c} out of range 0..{ncols}"));
        }
        if w.as_ref().is_some_and(|w| w.len() != col.len()) {
            return bad("CSR weights and columns differ in length".into());
        }
        Ok(Self {
            row_ptr,
            col,
            w,
            ncols,
        })
    }

    /// An incidence table with `width` columns in every row (triangles: 3), from row-major
    /// `col`.
    pub fn fixed(width: usize, col: Vec<u32>, ncols: usize) -> Result<Self> {
        if width == 0 || !col.len().is_multiple_of(width) {
            return Err(MeshError::Invalid(format!(
                "{} entries do not form rows of {width}",
                col.len()
            )));
        }
        let rows = col.len() / width;
        Self::new((0..=rows).map(|r| r * width).collect(), col, None, ncols)
    }

    pub fn rows(&self) -> usize {
        self.row_ptr.len() - 1
    }

    pub fn nnz(&self) -> usize {
        self.col.len()
    }

    pub fn range(&self, r: usize) -> Range<usize> {
        self.row_ptr[r]..self.row_ptr[r + 1]
    }

    /// Columns of row `r`.
    pub fn row(&self, r: usize) -> &[u32] {
        &self.col[self.range(r)]
    }

    /// Weights of row `r` (`None` for an incidence table).
    pub fn weights(&self, r: usize) -> Option<&[f64]> {
        let rg = self.range(r);
        self.w.as_ref().map(|w| &w[rg])
    }

    /// The transposed table (columns become rows); entries of each new row in ascending order
    /// of the old row, weights carried along.
    pub fn transpose(&self) -> Self {
        let mut row_ptr = vec![0usize; self.ncols + 1];
        for &c in &self.col {
            row_ptr[c as usize + 1] += 1;
        }
        for i in 0..self.ncols {
            row_ptr[i + 1] += row_ptr[i];
        }
        let mut next = row_ptr.clone();
        let mut col = vec![0u32; self.nnz()];
        let mut w = self.w.as_ref().map(|_| vec![0.0; self.nnz()]);
        for r in 0..self.rows() {
            for k in self.range(r) {
                let c = self.col[k] as usize;
                col[next[c]] = r as u32;
                if let (Some(w), Some(src)) = (w.as_mut(), self.w.as_ref()) {
                    w[next[c]] = src[k];
                }
                next[c] += 1;
            }
        }
        Self {
            row_ptr,
            col,
            w,
            ncols: self.rows(),
        }
    }

    pub fn bytes(&self) -> u64 {
        csr_bytes(self.rows(), self.nnz(), self.w.is_some())
    }
}

/// Size in bytes of a CSR table of `rows` rows and `nnz` entries.
pub fn csr_bytes(rows: usize, nnz: usize, weighted: bool) -> u64 {
    let per = 4 + if weighted { 8 } else { 0 };
    (8 * (rows + 1) + per * nnz) as u64
}

impl fmt::Debug for Csr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Csr({} x {}, {} entries{})",
            self.rows(),
            self.ncols,
            self.nnz(),
            if self.w.is_some() { ", weighted" } else { "" }
        )
    }
}

/// Cell polygons of a place: the vertices of cell `i` are `lon/lat[offsets[i]..offsets[i + 1]]`
/// (degrees, in order around the cell).
#[derive(Clone, PartialEq)]
pub struct Polygons {
    pub offsets: Vec<usize>,
    pub lon: Vec<f64>,
    pub lat: Vec<f64>,
}

impl Polygons {
    pub fn bytes(&self) -> u64 {
        (8 * self.offsets.len() + 16 * self.lon.len()) as u64
    }
}

impl fmt::Debug for Polygons {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Polygons({} cells, {} vertices)",
            self.offsets.len().saturating_sub(1),
            self.lon.len()
        )
    }
}

/// Per-level values of a place (areas): one lazily loaded array of `size` values per level, so
/// an operator that needs a few levels reads only those.
pub struct Levels {
    pub size: usize,
    pub levels: Vec<Lazy<Vec<f64>>>,
}

impl Levels {
    /// Levels loaded one at a time by `load(k)`.
    pub fn deferred(
        nlev: usize,
        size: usize,
        load: impl Fn(usize) -> Result<Vec<f64>> + Send + Sync + 'static,
    ) -> Self {
        let load = Arc::new(load);
        let levels = (0..nlev)
            .map(|k| {
                let load = load.clone();
                Lazy::deferred(8 * size as u64, move || {
                    let v = load(k)?;
                    if v.len() != size {
                        return Err(MeshError::Invalid(format!(
                            "level {k}: {} values, expected {size}",
                            v.len()
                        )));
                    }
                    Ok(v)
                })
            })
            .collect();
        Self { size, levels }
    }

    pub fn nlev(&self) -> usize {
        self.levels.len()
    }

    /// Values of level `k` (0-based).
    pub fn level(&self, k: usize) -> Result<Arc<Vec<f64>>> {
        self.levels
            .get(k)
            .ok_or_else(|| MeshError::Invalid(format!("level {k} of {}", self.nlev())))?
            .get()
    }

    /// Values of level `k` without keeping them ([`Lazy::get_uncached`]).
    pub fn level_uncached(&self, k: usize) -> Result<Arc<Vec<f64>>> {
        self.levels
            .get(k)
            .ok_or_else(|| MeshError::Invalid(format!("level {k} of {}", self.nlev())))?
            .get_uncached()
    }

    pub fn bytes(&self) -> u64 {
        self.levels.iter().map(Lazy::bytes).sum()
    }

    pub fn loaded_bytes(&self) -> u64 {
        self.levels.iter().map(Lazy::loaded_bytes).sum()
    }
}

impl fmt::Debug for Levels {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let loaded = self.levels.iter().filter(|l| l.is_loaded()).count();
        write!(
            f,
            "Levels({} x {}, {} loaded)",
            self.nlev(),
            self.size,
            loaded
        )
    }
}

/// One set of locations data can live on.
pub struct Place {
    /// `cells`, `nodes`, `elements`, ...
    pub name: String,
    pub size: usize,
    /// Centres, degrees.
    pub lon: Vec<f64>,
    pub lat: Vec<f64>,
    /// 2-D cell areas (m², or as the source stores them).
    pub area: Vec<f64>,
    /// Areas per level (levels × size), where the model has them (FESOM2 `nod_area`).
    pub area_per_level: Option<Levels>,
    pub polygons: Option<Lazy<Polygons>>,
}

impl Place {
    /// A place with centres and 2-D areas only.
    pub fn new(name: &str, lon: Vec<f64>, lat: Vec<f64>, area: Vec<f64>) -> Result<Self> {
        let size = lon.len();
        if lat.len() != size || area.len() != size {
            return Err(MeshError::Invalid(format!(
                "place '{name}': {} longitudes, {} latitudes, {} areas",
                size,
                lat.len(),
                area.len()
            )));
        }
        Ok(Self {
            name: name.to_owned(),
            size,
            lon,
            lat,
            area,
            area_per_level: None,
            polygons: None,
        })
    }

    /// Bytes of the arrays loaded with the place (centres and 2-D areas).
    pub fn resident_bytes(&self) -> u64 {
        24 * self.size as u64
    }

    /// Bytes of the lazy components: (total once loaded, loaded now).
    pub fn lazy_bytes(&self) -> (u64, u64) {
        let (mut t, mut l) = (0, 0);
        if let Some(a) = &self.area_per_level {
            t += a.bytes();
            l += a.loaded_bytes();
        }
        if let Some(p) = &self.polygons {
            t += p.bytes();
            l += p.loaded_bytes();
        }
        (t, l)
    }
}

impl fmt::Debug for Place {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Place")
            .field("name", &self.name)
            .field("size", &self.size)
            .field("area_per_level", &self.area_per_level)
            .field("polygons", &self.polygons)
            .finish()
    }
}

/// A named incidence table between two places (rows index `rows_place`, columns `cols_place`).
#[derive(Debug)]
pub struct Table {
    /// `elem_nodes`, `edge_nodes`, `edge_elems`, ...
    pub name: String,
    pub rows_place: String,
    pub cols_place: String,
    pub csr: Lazy<Csr>,
}

/// How layer thicknesses are known. Round 1: static full layers (the interfaces); variable
/// (FESOM2 zstar) and computed (IFS Δp) thickness are added with the round that needs them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Thickness {
    /// Full layers between fixed interfaces.
    Static,
}

/// The wet levels of each cell of one place: mid-levels `top[i]..bottom[i]` (0-based, half
/// open) hold water; the others are missing whatever the file stores there.
#[derive(Clone, PartialEq)]
pub struct WetLevels {
    pub place: String,
    pub top: Vec<u16>,
    pub bottom: Vec<u16>,
}

impl WetLevels {
    pub fn range(&self, cell: usize) -> Range<usize> {
        self.top[cell] as usize..self.bottom[cell] as usize
    }

    pub fn is_wet(&self, cell: usize, level: usize) -> bool {
        self.range(cell).contains(&level)
    }

    /// The interfaces (0-based, half open) that bound the wet mid-levels of `cell`: one more
    /// than [`WetLevels::range`]; empty for a cell without wet levels.
    pub fn interfaces(&self, cell: usize) -> Range<usize> {
        let r = self.range(cell);
        if r.is_empty() { r } else { r.start..r.end + 1 }
    }

    /// Smallest and largest number of wet levels.
    pub fn depth_range(&self) -> Option<(usize, usize)> {
        let n = (0..self.top.len()).map(|i| self.range(i).len());
        Some((n.clone().min()?, n.max()?))
    }

    pub fn bytes(&self) -> u64 {
        4 * self.top.len() as u64
    }
}

impl fmt::Debug for WetLevels {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WetLevels({}, {} cells)", self.place, self.top.len())
    }
}

/// Vertical geometry.
#[derive(Clone, PartialEq)]
pub struct Vertical {
    /// Level interfaces (FESOM2 `zbar`), as the source stores them (m).
    pub interfaces: Vec<f64>,
    /// Mid-levels (FESOM2 `Z`), one per layer.
    pub mid: Vec<f64>,
    pub thickness: Thickness,
    /// Wet levels per place (places without an entry have no vertical extent).
    pub wet: Vec<WetLevels>,
}

impl Vertical {
    pub fn nlev(&self) -> usize {
        self.mid.len()
    }

    pub fn wet(&self, place: &str) -> Option<&WetLevels> {
        self.wet.iter().find(|w| w.place == place)
    }

    /// Thickness of each layer: |interfaces[k + 1] − interfaces[k]|.
    pub fn layer_thickness(&self) -> Vec<f64> {
        match self.thickness {
            Thickness::Static => self
                .interfaces
                .windows(2)
                .map(|z| (z[1] - z[0]).abs())
                .collect(),
        }
    }

    pub fn bytes(&self) -> u64 {
        8 * (self.interfaces.len() + self.mid.len()) as u64
            + self.wet.iter().map(WetLevels::bytes).sum::<u64>()
    }
}

impl fmt::Debug for Vertical {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Vertical")
            .field("levels", &self.nlev())
            .field("thickness", &self.thickness)
            .field("wet", &self.wet)
            .finish()
    }
}

/// What a mesh was read from, and how its conventions were decided.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Source {
    /// `grid` for the one-place view of a built-in grid, else the model (`fesom2`, ...).
    pub model: String,
    pub files: Vec<PathBuf>,
    /// How the units of the coordinates were decided (`radians: max |lat| 1.56 <= pi/2 ...`),
    /// for loaders that have to decide.
    pub units: Option<String>,
    /// Other decisions a user may want to see.
    pub notes: Vec<String>,
}

/// A mesh: places, incidence tables, the vertical and where it came from.
pub struct Mesh {
    /// A short name for messages (`fesom2:pi`, `regular 36x18`).
    pub name: String,
    pub places: Vec<Place>,
    pub topology: Vec<Table>,
    pub vertical: Option<Vertical>,
    pub source: Source,
}

impl Mesh {
    /// The one-place view of a grid: its cells with centres and areas.
    pub fn one_place(name: &str, cells: Place, source: Source) -> Self {
        Self {
            name: name.to_owned(),
            places: vec![cells],
            topology: Vec::new(),
            vertical: None,
            source,
        }
    }

    pub fn place(&self, name: &str) -> Option<&Place> {
        self.places.iter().find(|p| p.name == name)
    }

    /// The place with `size` locations, if exactly one has it (how data is matched to a place
    /// when its dimension name does not say).
    pub fn place_of_size(&self, size: usize) -> Option<&Place> {
        let mut it = self.places.iter().filter(|p| p.size == size);
        let p = it.next()?;
        it.next().is_none().then_some(p)
    }

    pub fn table(&self, name: &str) -> Option<&Table> {
        self.topology.iter().find(|t| t.name == name)
    }

    /// Bytes of the arrays loaded with the mesh (centres, 2-D areas, the vertical).
    pub fn resident_bytes(&self) -> u64 {
        self.places.iter().map(Place::resident_bytes).sum::<u64>()
            + self.vertical.as_ref().map_or(0, Vertical::bytes)
    }

    /// Bytes of the lazy components (per-level areas, polygons, topology): (total once all are
    /// loaded, loaded now).
    pub fn lazy_bytes(&self) -> (u64, u64) {
        let (mut t, mut l) = (0, 0);
        for p in &self.places {
            let (a, b) = p.lazy_bytes();
            t += a;
            l += b;
        }
        for tb in &self.topology {
            t += tb.csr.bytes();
            l += tb.csr.loaded_bytes();
        }
        (t, l)
    }
}

impl fmt::Debug for Mesh {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mesh")
            .field("name", &self.name)
            .field("places", &self.places)
            .field("topology", &self.topology)
            .field("vertical", &self.vertical)
            .field("source", &self.source.model)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csr_transpose_and_lazy() {
        // two triangles sharing an edge: elem -> nodes, transposed to node -> elems
        let en = Csr::fixed(3, vec![0, 1, 2, 2, 1, 3], 4).unwrap();
        let ne = en.transpose();
        assert_eq!(ne.row_ptr, vec![0, 1, 3, 5, 6]);
        assert_eq!(ne.col, vec![0, 0, 1, 0, 1, 1]);
        assert!(Csr::fixed(3, vec![0, 1, 4], 4).is_err());
        let lazy = Lazy::deferred(en.bytes(), move || Ok(en.clone()));
        assert!(!lazy.is_loaded() && lazy.loaded_bytes() == 0);
        assert_eq!(lazy.get().unwrap().rows(), 2);
        assert_eq!(lazy.loaded_bytes(), lazy.bytes());
    }
}
