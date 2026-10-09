//! Target grids described by cdors itself, for planning without cdo.
//!
//! A real run takes its output grid from cdo's template (`cdo -f nc4 const,0,<grid>`, see
//! [`super::WeightCache::grid_template`]). `--plan`, and the first planning pass of a run (which
//! evaluates `--max-read`, `--max-values` and the output checks before cdo runs), describe the
//! target grid here instead, as an in-memory dataset laid out like that template and classified
//! by the ordinary reader code (`model::classify`):
//!
//! - grid names (`src/grid_from_name.cc` of cdo 2.6.5): `r<nx>x<ny>` (also `r<nx>/<ny>`,
//!   `r<nx>_<ny>`), `global` and `global_<inc>`, `hpz<zoom>[_nested|_ring]`,
//!   `hp<nside>[_nested|_ring]` and `lon=<x>_lat=<y>` (also `/` or `x` as separator). Names are
//!   case-insensitive, as in cdo. The coordinates are computed with cdo's and CDI's formulas
//!   (`generate_grid_lonlat`, `gridGenXvals`, `gridGenYvals`) and equal the template's bit for
//!   bit (checked by the `native_grids_equal_cdo_templates` test); like the templates, they carry
//!   no bounds;
//! - a text grid description file (`cdo griddes` format): lon-lat and Gaussian grids with their
//!   coordinates, HEALPix grids, and the shape of any other grid;
//! - a dataset (NetCDF file or Zarr store): opened with the ordinary reader.
//!
//! Anything else (Gaussian `F`/`N`/`O`/`t` names, icosphere, GME, `dcw:` regions, ...) is not
//! described here; `None` lets the caller fall back to cdo's template.

use crate::error::{Error, Result};
use crate::io::{ChunkGrid, ChunkSource, RawChunk};
use crate::model::{AttrValue, Attrs, DType, Dataset, DimRole, Format, VarDim, VarKind, Variable};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// An in-memory dataset holding one target grid: coordinates and an empty data variable.
pub struct GridSource {
    ds: Dataset,
    values: HashMap<String, Vec<f64>>,
}

impl ChunkSource for GridSource {
    fn dataset(&self) -> &Dataset {
        &self.ds
    }

    fn chunk_grid(&self, var: &str) -> Result<ChunkGrid> {
        let v = self
            .ds
            .var(var)
            .ok_or_else(|| Error::internal(format!("grid description has no variable '{var}'")))?;
        Ok(ChunkGrid {
            shape: v.shape(),
            chunk_shape: v.shape().iter().map(|&n| n.max(1)).collect(),
        })
    }

    fn read_chunk(&self, var: &str, _indices: &[u64]) -> Result<RawChunk> {
        Err(Error::internal(format!(
            "the grid description of {} holds no data ('{var}')",
            self.ds.source
        )))
    }

    fn read_var(&self, var: &str) -> Result<Vec<f64>> {
        self.values.get(var).cloned().ok_or_else(|| {
            Error::internal(format!(
                "the grid description of {} has no values for '{var}'",
                self.ds.source
            ))
        })
    }
}

fn text(s: &str) -> AttrValue {
    AttrValue::Text(s.to_owned())
}

fn variable(name: &str, dtype: DType, dims: &[(&str, usize)], attrs: Attrs) -> Variable {
    let dims: Vec<VarDim> = dims
        .iter()
        .map(|&(n, size)| VarDim {
            name: n.to_owned(),
            size,
            role: DimRole::Other,
        })
        .collect();
    let encoding = crate::io::encoding_from_attrs(&attrs, dtype, None);
    Variable {
        name: name.to_owned(),
        kind: VarKind::Data,
        dtype,
        chunks: dims.iter().map(|d| d.size).collect(),
        dims,
        attrs,
        encoding,
        grid: None,
        zaxis: None,
    }
}

/// Classifies the variables like the readers do and wraps them as a source.
fn finish(
    origin: &str,
    dims: Vec<(String, usize)>,
    vars: Vec<Variable>,
    values: HashMap<String, Vec<f64>>,
) -> Result<GridSource> {
    let mut ds = Dataset {
        source: origin.to_owned(),
        format: Format::NetCdf,
        attrs: Attrs::default(),
        dims,
        vars,
        grids: Vec::new(),
        zaxes: Vec::new(),
        time: None,
    };
    crate::model::classify(&mut ds, &|n| {
        values
            .get(n)
            .cloned()
            .ok_or_else(|| Error::internal(format!("no values for '{n}'")))
    })?;
    Ok(GridSource { ds, values })
}

/// One coordinate axis as cdo writes it.
struct Axis {
    name: String,
    long_name: String,
    units: String,
    vals: Vec<f64>,
}

impl Axis {
    fn lon(vals: Vec<f64>) -> Self {
        Self {
            name: "lon".into(),
            long_name: "longitude".into(),
            units: "degrees_east".into(),
            vals,
        }
    }

    fn lat(vals: Vec<f64>) -> Self {
        Self {
            name: "lat".into(),
            long_name: "latitude".into(),
            units: "degrees_north".into(),
            vals,
        }
    }
}

/// A lon-lat (or Gaussian) grid laid out like cdo's template: `lon(lon)`, `lat(lat)` (doubles
/// with `standard_name`, `long_name`, `units`, `axis`) and `const(lat, lon)`.
fn lonlat(origin: &str, x: Axis, y: Axis) -> Result<GridSource> {
    let (nx, ny) = (x.vals.len(), y.vals.len());
    let attrs = |a: &Axis, std: &str, axis: &str| {
        Attrs(vec![
            ("standard_name".into(), text(std)),
            ("long_name".into(), text(&a.long_name)),
            ("units".into(), text(&a.units)),
            ("axis".into(), text(axis)),
        ])
    };
    let vars = vec![
        variable(
            &x.name,
            DType::F64,
            &[(&x.name, nx)],
            attrs(&x, "longitude", "X"),
        ),
        variable(
            &y.name,
            DType::F64,
            &[(&y.name, ny)],
            attrs(&y, "latitude", "Y"),
        ),
        variable(
            "const",
            DType::F32,
            &[(&y.name, ny), (&x.name, nx)],
            Attrs::default(),
        ),
    ];
    let dims = vec![(x.name.clone(), nx), (y.name.clone(), ny)];
    let values = HashMap::from([(x.name, x.vals), (y.name, y.vals)]);
    finish(origin, dims, vars, values)
}

/// A HEALPix grid as cdo writes it: a grid-mapping variable `healpix` and
/// `const(healpix_index)`. `zoom` grids (`hpz`) carry `healpix_nside`/`healpix_order`, the
/// others (`hp`) CF's `refinement_level`/`indexing_scheme`.
fn healpix(origin: &str, nside: u64, ring: bool, zoom: bool) -> Result<GridSource> {
    let order = if ring { "ring" } else { "nested" };
    let mut m = vec![("grid_mapping_name".to_owned(), text("healpix"))];
    if zoom {
        m.push(("healpix_nside".into(), AttrValue::Ints(vec![nside as i64])));
        m.push(("healpix_order".into(), text(order)));
    } else {
        m.push((
            "refinement_level".into(),
            AttrValue::Ints(vec![i64::from(nside.trailing_zeros())]),
        ));
        m.push(("indexing_scheme".into(), text(order)));
    }
    let n = (12 * nside * nside) as usize;
    let vars = vec![
        variable("healpix", DType::I32, &[], Attrs(m)),
        variable(
            "const",
            DType::F32,
            &[("healpix_index", n)],
            Attrs(vec![("grid_mapping".into(), text("healpix"))]),
        ),
    ];
    finish(
        origin,
        vec![("healpix_index".into(), n)],
        vars,
        HashMap::new(),
    )
}

/// A grid known by its shape only (no coordinates): `const(y, x)` or `const(ncells)`.
fn shape_only(origin: &str, xsize: usize, ysize: usize) -> Result<GridSource> {
    let vdims: Vec<(&str, usize)> = if ysize > 0 {
        vec![("y", ysize), ("x", xsize)]
    } else {
        vec![("ncells", xsize)]
    };
    // dimensions in first-use order of the file: x before y
    let dims = vdims
        .iter()
        .rev()
        .map(|&(n, s)| (n.to_owned(), s))
        .collect();
    let vars = vec![variable("const", DType::F32, &vdims, Attrs::default())];
    finish(origin, dims, vars, HashMap::new())
}

// ------------------------------------------------------------------ CDI's coordinate formulas

/// CDI `gridGenXvals` (libcdi/src/grid.c).
fn gen_xvals(n: usize, first: f64, mut last: f64, mut inc: f64) -> Vec<f64> {
    if inc.abs() <= 0.0 && n > 1 {
        if first >= last {
            while first >= last {
                last += 360.0;
            }
            inc = (last - first) / n as f64;
        } else {
            inc = (last - first) / (n - 1) as f64;
        }
    }
    (0..n).map(|i| first + i as f64 * inc).collect()
}

/// CDI `gridGenYvalsRegular` (libcdi/src/grid.c).
#[allow(clippy::float_cmp)] // CDI compares exactly
fn gen_yvals(n: usize, mut first: f64, mut last: f64, mut inc: f64) -> Vec<f64> {
    if inc.abs() <= 0.0 && n > 1 {
        if first == last && first != 0.0 {
            last *= -1.0;
        }
        if first > last {
            inc = (first - last) / (n - 1) as f64;
        } else if first < last {
            inc = (last - first) / (n - 1) as f64;
        } else if !n.is_multiple_of(2) {
            inc = 180.0 / (n - 1) as f64;
            first = -90.0;
        } else {
            inc = 180.0 / n as f64;
            first = -90.0 + inc / 2.0;
        }
    }
    if first > last && inc > 0.0 {
        inc = -inc;
    }
    (0..n).map(|i| first + i as f64 * inc).collect()
}

// ------------------------------------------------------------------ sscanf-like scanning

/// `%d`: an optionally signed decimal integer at the start of `s`.
fn scan_int(s: &str) -> Option<(i64, &str)> {
    let b = s.as_bytes();
    let mut i = usize::from(matches!(b.first(), Some(b'+' | b'-')));
    let start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return None;
    }
    Some((s[..i].parse().ok()?, &s[i..]))
}

/// `%lf` for plain decimal numbers (`12`, `-1.5`, `.5`, `2e-1`) at the start of `s`.
fn scan_f64(s: &str) -> Option<(f64, &str)> {
    let b = s.as_bytes();
    let mut i = usize::from(matches!(b.first(), Some(b'+' | b'-')));
    let mut digits = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
        digits += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return None;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        let k = j;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j > k {
            i = j;
        }
    }
    let v: f64 = s[..i].parse().ok()?;
    v.is_finite().then_some((v, &s[i..]))
}

// ------------------------------------------------------------------ grid names

/// `[_<order>]` after a HEALPix level: `None` if malformed, else whether the order is ring.
fn healpix_order(rest: &str) -> Option<bool> {
    if rest.is_empty() {
        return Some(false);
    }
    let o = rest.strip_prefix('_')?;
    match o {
        "ring" => Some(true),
        _ if o.starts_with("nest") => Some(false),
        _ => None,
    }
}

/// cdo's grid for a grid name, laid out as `cdo -f nc4 const,0,<name>` writes it; `None` for
/// names cdors does not describe (and for existing files, which cdo reads instead).
pub fn from_name(name: &str) -> Option<GridSource> {
    if Path::new(name).exists() || name.len() < 2 {
        return None;
    }
    let lc = name.to_ascii_lowercase();
    let sep = |c: char| matches!(c, 'x' | '/' | '_');
    if let Some(r) = lc.strip_prefix("lon=") {
        // generate_grid_point: "lon=%lf%clat=%lf%c"
        let (lon, r) = scan_f64(r)?;
        let r = r.strip_prefix(sep)?.strip_prefix("lat=")?;
        let (lat, r) = scan_f64(r)?;
        if !r.is_empty() {
            return None;
        }
        return lonlat(name, Axis::lon(vec![lon]), Axis::lat(vec![lat])).ok();
    }
    if let Some(r) = lc.strip_prefix("hpz") {
        // generate_proj_healpix (zoom): nside = 2^zoom
        let (zoom, rest) = scan_int(r)?;
        if !(0..=29).contains(&zoom) {
            return None;
        }
        let ring = healpix_order(rest)?;
        return healpix(name, 1u64 << zoom, ring, true).ok();
    }
    if lc.starts_with("hpr") {
        return None;
    }
    if let Some(r) = lc.strip_prefix("hp") {
        // generate_grid_healpix: nside, a power of two (cdo 2.6 aborts on hp1: refinement level
        // 0 counts as undefined)
        let (nside, rest) = scan_int(r)?;
        let nside = u64::try_from(nside).ok()?;
        if !nside.is_power_of_two() || !(2..=1 << 29).contains(&nside) {
            return None;
        }
        let ring = healpix_order(rest)?;
        return healpix(name, nside, ring, false).ok();
    }
    if let Some(r) = lc.strip_prefix("global") {
        // generate_grid_lonlat(params, 1, {-180, 180, -90, 90}) without bounds and grid-type
        // suffixes: "global" or "global_<inc>"
        let inc = if r.is_empty() {
            1.0
        } else {
            let (inc, rest) = scan_f64(r.strip_prefix('_')?)?;
            if !rest.is_empty() {
                return None;
            }
            if inc == 0.0 { 1.0 } else { inc }
        };
        if !(1.0e-9..=180.0).contains(&inc) {
            return None;
        }
        let (lon1, lon2, lat1, lat2) = (-180.0f64, 180.0f64, -90.0f64, 90.0f64);
        let nlon = ((lon2 - lon1) / inc + 0.5) as usize;
        let nlat = ((lat2 - lat1) / inc + 0.5) as usize;
        if nlon == 0 || nlat == 0 {
            return None;
        }
        let xs = (0..nlon)
            .map(|i| lon1 + inc * 0.5 + i as f64 * inc)
            .collect();
        let ys = (0..nlat)
            .map(|i| lat1 + inc * 0.5 + i as f64 * inc)
            .collect();
        return lonlat(name, Axis::lon(xs), Axis::lat(ys)).ok();
    }
    if let Some(r) = lc.strip_prefix('r') {
        // generate_grid_reg2d: "r%d%c%d%c", xfirst = yfirst = 0, no increments
        let (nx, r) = scan_int(r)?;
        let (ny, r) = scan_int(r.strip_prefix(sep)?)?;
        if !r.is_empty() || nx <= 0 || ny <= 0 {
            return None;
        }
        let (nx, ny) = (nx as usize, ny as usize);
        return lonlat(
            name,
            Axis::lon(gen_xvals(nx, 0.0, 0.0, 0.0)),
            Axis::lat(gen_yvals(ny, 0.0, 0.0, 0.0)),
        )
        .ok();
    }
    None
}

// ------------------------------------------------------------------ grid description files

/// Key/value pairs of a `cdo griddes` text file (values of a key may continue on lines without
/// `=`); keys lowercase, quotes removed.
fn griddes_entries(text: &str) -> HashMap<String, String> {
    let mut map: HashMap<String, String> = HashMap::new();
    let mut last: Option<String> = None;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim().to_ascii_lowercase();
            map.insert(k.clone(), v.trim().trim_matches(['"', '\'']).to_owned());
            last = Some(k);
        } else if let Some(k) = &last
            && let Some(v) = map.get_mut(k)
        {
            v.push(' ');
            v.push_str(line);
        }
    }
    map
}

/// The grid of a `cdo griddes` text file; `None` if the file is not one cdors understands.
pub fn from_griddes(path: &Path) -> Option<GridSource> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() > 64 << 20 {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    let e = griddes_entries(&text);
    let origin = path.display().to_string();
    let gridtype = e.get("gridtype")?.to_ascii_lowercase();
    let num = |k: &str| e.get(k).and_then(|v| v.trim().parse::<f64>().ok());
    let size = |k: &str| {
        e.get(k)
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|&n| n > 0)
    };
    let list = |k: &str| -> Option<Vec<f64>> {
        e.get(k)?
            .split_whitespace()
            .map(|t| t.parse::<f64>().ok())
            .collect()
    };
    let is_healpix = gridtype == "healpix"
        || (gridtype == "projection"
            && e.get("grid_mapping_name").map(String::as_str) == Some("healpix"));
    if is_healpix {
        let nside = match num("healpix_nside") {
            Some(n) => n,
            None => 2f64.powf(num("refinement_level")?),
        };
        if !(1.0..=(1u64 << 29) as f64).contains(&nside) || nside.fract() != 0.0 {
            return None;
        }
        let order = e
            .get("healpix_order")
            .or_else(|| e.get("indexing_scheme"))
            .map_or("nested", String::as_str);
        let ring = healpix_order(&format!("_{order}"))?;
        return healpix(&origin, nside as u64, ring, e.contains_key("healpix_nside")).ok();
    }
    let xsize = size("xsize");
    let ysize = size("ysize");
    if gridtype == "lonlat" || gridtype == "gaussian" {
        let (nx, ny) = (xsize?, ysize?);
        let xs = match list("xvals") {
            Some(v) if v.len() == nx => v,
            Some(_) => return None,
            None => gen_xvals(
                nx,
                num("xfirst").unwrap_or(0.0),
                num("xlast").unwrap_or(0.0),
                num("xinc").unwrap_or(0.0),
            ),
        };
        let ys = match list("yvals") {
            Some(v) if v.len() == ny => v,
            Some(_) => return None,
            None if gridtype == "gaussian" => crate::model::grid::gaussian_latitudes(ny),
            None => {
                let first = num("yfirst").unwrap_or(0.0);
                gen_yvals(
                    ny,
                    first,
                    num("ylast").unwrap_or(first),
                    num("yinc").unwrap_or(0.0),
                )
            }
        };
        let mut x = Axis::lon(xs);
        let mut y = Axis::lat(ys);
        for (a, p) in [(&mut x, 'x'), (&mut y, 'y')] {
            if let Some(n) = e.get(&format!("{p}name")) {
                a.name.clone_from(n);
            }
            if let Some(n) = e.get(&format!("{p}longname")) {
                a.long_name.clone_from(n);
            }
            if let Some(n) = e.get(&format!("{p}units")) {
                a.units.clone_from(n);
            }
        }
        if x.name == y.name {
            return None;
        }
        return lonlat(&origin, x, y).ok();
    }
    // any other grid: its shape
    match (gridtype.as_str(), xsize, ysize) {
        ("curvilinear" | "generic" | "projection", Some(nx), Some(ny)) => {
            shape_only(&origin, nx, ny).ok()
        }
        _ => {
            let n = size("gridsize")?;
            shape_only(&origin, n, 0).ok()
        }
    }
}

/// The target grid described without cdo: a grid name, a grid description file, or a dataset
/// (NetCDF file, Zarr store) opened with the ordinary reader. `Ok(None)` when cdors cannot
/// describe it.
pub fn describe(target: &str) -> Result<Option<Arc<dyn ChunkSource>>> {
    let p = Path::new(target);
    if crate::io::is_zarr(target) {
        return crate::io::open(target).map(Some);
    }
    if p.is_file() {
        let mut magic = [0u8; 4];
        let is_dataset = {
            use std::io::Read;
            std::fs::File::open(p)
                .and_then(|mut f| f.read_exact(&mut magic))
                .is_ok()
                && (&magic[..3] == b"CDF" || &magic[1..4] == b"HDF")
        };
        if is_dataset {
            return crate::io::open(target).map(Some);
        }
        return Ok(from_griddes(p).map(|g| Arc::new(g) as Arc<dyn ChunkSource>));
    }
    Ok(from_name(target).map(|g| Arc::new(g) as Arc<dyn ChunkSource>))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(name: &str) -> crate::model::Grid {
        let s = from_name(name).unwrap_or_else(|| panic!("{name} not described"));
        s.ds.grids[0].clone()
    }

    #[test]
    fn names() {
        let g = grid("r18x9");
        assert_eq!((g.xsize, g.ysize), (18, 9));
        assert_eq!(g.xvals.as_ref().unwrap()[1], 20.0);
        assert_eq!(g.yvals.as_ref().unwrap()[0], -90.0);
        assert_eq!(grid("R36/18").yvals.unwrap()[0], -85.0);
        assert_eq!(grid("global_30").xvals.unwrap()[0], -165.0);
        assert_eq!(grid("global").size, 360 * 180);
        let h = grid("hpz2_ring");
        assert_eq!(h.size, 192);
        assert_eq!(h.healpix.as_ref().unwrap().nside, 4);
        assert_eq!(
            grid("hp4").healpix.unwrap().order,
            crate::model::HealpixOrder::Nested
        );
        let p = grid("lon=10.5_lat=-20");
        assert_eq!((p.xvals.unwrap()[0], p.yvals.unwrap()[0]), (10.5, -20.0));
        for bad in [
            "r18",
            "hp1",
            "r0x9",
            "hp3",
            "hpz2_xy",
            "global_1_b",
            "t63grid",
            "n80",
            "lon=1",
        ] {
            assert!(from_name(bad).is_none(), "{bad}");
        }
    }
}
