//! Grid-cell areas and the weights of the space and vertical statistics, as CDO 2.6 computes
//! them.
//!
//! References are to the CDO 2.6.5 source (`cdo-2.6.5/src`); cdo 2.6.0 gives the same results
//! on the fixtures (checked by `examples/area_check.rs`).
//!
//! **Cell areas** (`gridcell_areas`, `operators/Gridcell.cc:75`; `gridGenArea`,
//! `grid_area.cc:488`):
//! 1. a cell-area variable named by the data variable's `cell_measures = "area: <var>"` is used
//!    as stored (CDI reads it for regular, Gaussian, curvilinear, unstructured and generic grids,
//!    `libcdi/src/stream_cdf_i.c:1000,2695`); only the first measure of the attribute is looked at;
//! 2. regular and Gaussian grids: rectangles bounded by the longitude/latitude bounds, or by
//!    bounds generated from the midpoints of the centres (`grid_gen_bounds`, outer latitude
//!    bounds beyond ±88° snapped to ±90°, `mpim_grid.cc:271,299`), each split into two
//!    spherical triangles (`gen_gridcellarea_reg2d`, `grid_area.cc:296`);
//! 3. curvilinear and unstructured grids: the spherical polygon of the cell bounds; up to four
//!    vertices as a fan of triangles from vertex 0, more than four as a fan from the cell centre
//!    skipping repeated vertices (`gen_gridcellarea_unstruct`, `grid_area.cc:386`); triangle areas
//!    by L'Huilier's theorem (`mod_tri_area`, `grid_area.cc:138`);
//! 4. HEALPix: 4π / number of cells (`gen_gridcellarea_healpix`, `grid_area.cc:478`; note that
//!    cdo divides by the number of cells *in the file*, also for a subset grid).
//!
//! Computed areas are on the unit sphere and are scaled by R² for `gridarea`; R is the
//! `PLANET_RADIUS` environment variable, else the `earth_radius` attribute of the grid mapping,
//! else 6371000 m (`get_planet_radius_in_meter`, `Gridcell.cc:49`; `constants.h:14`).
//!
//! **Cell weights** (`gridcell_weights`, `mpim_grid/mpim_grid.cc:1412`): the areas divided by
//! their total (`compute_gridcell_weights`, `mpim_grid.cc:1380`). If no area can be computed
//! (curvilinear or unstructured without bounds or centres, generic grids) every weight is
//! 1/N and cdo warns "Grid cell bounds not available, using constant grid cell area weights!"
//! (`Gridcell.cc:349`, `Fldstat.cc:155`). Regular grids with a single row or column and no bounds
//! get a pseudo extent of 0.01 in the missing direction (`gridGenAreaReg2Dweights`,
//! `grid_area.cc:379`).

use super::{Dataset, Grid, GridKind, HealpixOrder, ReadVar, Variable, ZAxis};
use crate::error::{Error, ErrorCode, Result};
use rayon::prelude::*;

/// CDO's default planet radius in metres (`constants.h:14`, `C_EARTH_RADIUS`).
pub const EARTH_RADIUS: f64 = 6_371_000.0;

/// Where cell areas came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AreaSource {
    /// A cell-area variable of the file (`cell_measures`), used as stored.
    File(String),
    /// Spherical polygons of the cell bounds (stored, or generated for regular grids).
    Bounds,
    /// HEALPix: 4π/N.
    Healpix,
    /// Regular grid with one row or column and no bounds: pseudo extent, valid only as weights.
    PseudoBounds,
    /// No areas: constant weights 1/N.
    Constant,
}

/// Why cdo cannot compute cell areas (`gridGenArea` status).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AreaFailure {
    /// Status 1: cell centres or corners missing.
    NoCorners,
    /// Status 2: all areas zero on a grid of fewer than 20 cells.
    Zero,
    /// The grid kind has no areas (generic grids).
    Unsupported,
}

/// Cell areas: in m² (or as stored) from [`cell_areas`], in steradians from [`gen_area`].
#[derive(Debug, Clone)]
pub struct CellAreas {
    pub values: Vec<f64>,
    pub source: AreaSource,
}

/// Cell areas, or why cdo cannot compute them.
pub type AreaResult = std::result::Result<CellAreas, AreaFailure>;

/// Normalised cell weights (sum 1) as `gridcell_weights` returns them.
#[derive(Debug, Clone)]
pub struct CellWeights {
    pub values: Vec<f64>,
    pub source: AreaSource,
}

impl CellWeights {
    /// Whether cdo would warn "Grid cell bounds not available, using constant grid cell area
    /// weights!" (weights are 1/N).
    pub fn constant(&self) -> bool {
        self.source == AreaSource::Constant
    }
}

/// The cell-area variable named in `cell_measures`, as CDI's `cdf_get_cell_varid`
/// (`stream_cdf_i.c:1000`): the first `measure: name` pair, measure starting with `area`, name
/// a variable of the dataset.
pub fn area_variable(ds: &Dataset, var: &Variable) -> Option<String> {
    let s = var.attrs.get_str("cell_measures")?.trim_start();
    let end = s
        .find(|c: char| !c.is_ascii_alphanumeric())
        .unwrap_or(s.len());
    let measure = &s[..end];
    let rest = s.get(end + 1..).unwrap_or("").trim_start();
    let name = rest.split_whitespace().next()?;
    (measure.starts_with("area") && ds.var(name).is_some()).then(|| name.to_owned())
}

/// Planet radius in metres, as `get_planet_radius_in_meter` (`Gridcell.cc:49`): `PLANET_RADIUS`
/// (a number with optional `m`/`km` suffix), else the grid mapping's `earth_radius` if > 1,
/// else [`EARTH_RADIUS`].
pub fn planet_radius(grid: &Grid) -> f64 {
    if let Ok(s) = std::env::var("PLANET_RADIUS") {
        let s = s.trim();
        let (num, scale) = if let Some(n) = s.strip_suffix("km") {
            (n, 1000.0)
        } else {
            (s.strip_suffix('m').unwrap_or(s), 1.0)
        };
        if let Ok(r) = num.trim().parse::<f64>()
            && r != 0.0
        {
            return r * scale;
        }
    }
    grid.mapping
        .as_ref()
        .and_then(|m| m.attrs.get_f64("earth_radius"))
        .filter(|&r| r > 1.0)
        .unwrap_or(EARTH_RADIUS)
}

/// Cell areas as `cdo gridarea` writes them: the file's area variable as stored, otherwise
/// [`gen_area`] × R² (m²). Fails where cdo aborts (`Gridcell.cc:88-89,103`).
pub fn cell_areas(
    ds: &Dataset,
    var: &Variable,
    grid: &Grid,
    read: ReadVar<'_>,
) -> Result<CellAreas> {
    if let Some(a) = file_area(ds, var, grid, read)? {
        return Ok(a);
    }
    match gen_area(ds, grid, read)? {
        Ok(mut a) => {
            let r = planet_radius(grid);
            for x in &mut a.values {
                *x *= r * r;
            }
            Ok(a)
        }
        Err(f) => Err(area_error(f, grid)),
    }
}

/// Normalised cell weights as `gridcell_weights` (`mpim_grid.cc:1412`): areas from the file or
/// [`gen_area`] (regular grids falling back to [`gen_area_reg2d_weights`]) divided by their sum;
/// 1/N where no area can be computed.
pub fn cell_weights(
    ds: &Dataset,
    var: &Variable,
    grid: &Grid,
    read: ReadVar<'_>,
) -> Result<CellWeights> {
    let n = grid.size;
    let areas = match file_area(ds, var, grid, read)? {
        Some(a) => Some(a),
        None => match gen_area(ds, grid, read)? {
            Ok(a) => Some(a),
            Err(_) if matches!(grid.kind, GridKind::Regular | GridKind::Gaussian) => {
                gen_area_reg2d_weights(ds, grid, read)?.ok()
            }
            Err(_) => None,
        },
    };
    Ok(match areas {
        Some(a) => {
            // compute_gridcell_weights, mpim_grid.cc:1395-1406: plain sum in cell order
            let total: f64 = a.values.iter().sum();
            CellWeights {
                values: a.values.iter().map(|x| x / total).collect(),
                source: a.source,
            }
        }
        None => CellWeights {
            values: vec![1.0 / n as f64; n],
            source: AreaSource::Constant,
        },
    })
}

/// The weights of the `fld*` operators (`get_gridcell_weights`, `Fldstat.cc:192`): a one-cell
/// field gets weight 1 without computing areas; otherwise [`cell_weights`].
pub fn fld_weights(
    ds: &Dataset,
    var: &Variable,
    grid: &Grid,
    read: ReadVar<'_>,
) -> Result<CellWeights> {
    if grid.size <= 1 {
        return Ok(CellWeights {
            values: vec![1.0; grid.size],
            source: AreaSource::Bounds,
        });
    }
    cell_weights(ds, var, grid, read)
}

fn area_error(f: AreaFailure, grid: &Grid) -> Error {
    let (code, msg) = match f {
        AreaFailure::NoCorners => (
            ErrorCode::NoCoordinates,
            "cell corner coordinates missing: cannot compute grid cell areas",
        ),
        AreaFailure::Zero => (
            ErrorCode::BadData,
            "can't compute grid cell area for this grid",
        ),
        AreaFailure::Unsupported => (
            ErrorCode::UnsupportedGrid,
            "grid cell areas need a grid with coordinates",
        ),
    };
    Error::new(code, msg).with("grid", grid.kind.name())
}

fn file_area(
    ds: &Dataset,
    var: &Variable,
    grid: &Grid,
    read: ReadVar<'_>,
) -> Result<Option<CellAreas>> {
    let Some(name) = area_variable(ds, var) else {
        return Ok(None);
    };
    let values = read(&name)?;
    if values.len() != grid.size {
        return Ok(None);
    }
    Ok(Some(CellAreas {
        values,
        source: AreaSource::File(name),
    }))
}

/// CDO's unit test `string_to_LonLatUnits` (`mpim_grid.cc:1586`): radians if the units start
/// with `rad`, otherwise degrees.
fn is_degrees(units: Option<&str>) -> bool {
    !units.unwrap_or("").starts_with("rad")
}

/// Areas on the unit sphere (steradians) as `gridGenArea` (`grid_area.cc:488`). The outer
/// `Result` is a read error; the inner one is cdo's failure status.
pub fn gen_area(ds: &Dataset, grid: &Grid, read: ReadVar<'_>) -> Result<AreaResult> {
    let r = match grid.kind {
        GridKind::Regular | GridKind::Gaussian => gen_area_reg2d(ds, grid, read, false)?,
        GridKind::Healpix => {
            let a = 4.0 * std::f64::consts::PI / grid.size as f64;
            Ok(CellAreas {
                values: vec![a; grid.size],
                source: AreaSource::Healpix,
            })
        }
        GridKind::Curvilinear | GridKind::Unstructured => gen_area_unstruct(ds, grid, read)?,
        GridKind::Generic => Err(AreaFailure::Unsupported),
    };
    // grid_area.cc:507
    Ok(r.and_then(|a| {
        if grid.size < 20 && a.values.iter().sum::<f64>() == 0.0 {
            Err(AreaFailure::Zero)
        } else {
            Ok(a)
        }
    }))
}

/// `gridGenAreaReg2Dweights` (`grid_area.cc:379`): like the regular-grid areas, but a single
/// row or column without bounds gets the pseudo extent [0, 0.01]. Only valid as weights.
pub fn gen_area_reg2d_weights(ds: &Dataset, grid: &Grid, read: ReadVar<'_>) -> Result<AreaResult> {
    gen_area_reg2d(ds, grid, read, true)
}

/// Bounds of a regular axis, two per value (`grid_gen_bounds`, `mpim_grid.cc:271`).
pub fn gen_axis_bounds(vals: &[f64]) -> Vec<f64> {
    let n = vals.len();
    let mut b = vec![0.0; 2 * n];
    if n == 0 {
        return b;
    }
    if n == 1 {
        // cdo reads bounds[1] and bounds[2n-2] uninitialised here; a one-value axis never
        // reaches this function in cdo (nlon == 1 / nlat == 1 are handled by the caller).
        b[0] = vals[0];
        b[1] = vals[0];
        return b;
    }
    if vals[0] > vals[n - 1] {
        for i in 0..n - 1 {
            let m = 0.5 * (vals[i] + vals[i + 1]);
            b[2 * i] = m;
            b[2 * (i + 1) + 1] = m;
        }
        b[1] = 2.0 * vals[0] - b[0];
        b[2 * n - 2] = 2.0 * vals[n - 1] - b[2 * n - 1];
    } else {
        for i in 0..n - 1 {
            let m = 0.5 * (vals[i] + vals[i + 1]);
            b[2 * i + 1] = m;
            b[2 * (i + 1)] = m;
        }
        b[0] = 2.0 * vals[0] - b[1];
        b[2 * n - 1] = 2.0 * vals[n - 1] - b[2 * (n - 1)];
    }
    b
}

/// `grid_check_lat_borders` (`mpim_grid.cc:299`): outer generated latitude bounds beyond ±88°
/// become ±90°.
pub fn check_lat_borders(b: &mut [f64]) {
    const YMAX: f64 = 90.0;
    const YLIM: f64 = 88.0;
    let n = b.len();
    if n < 2 {
        return;
    }
    if b[0] > b[n - 1] {
        if b[0] > b[1] {
            if b[0] > YLIM {
                b[0] = YMAX;
            }
            if b[n - 1] < -YLIM {
                b[n - 1] = -YMAX;
            }
        } else {
            if b[1] > YLIM {
                b[1] = YMAX;
            }
            if b[n - 2] < -YLIM {
                b[n - 2] = -YMAX;
            }
        }
    } else if b[0] < b[1] {
        if b[0] < -YLIM {
            b[0] = -YMAX;
        }
        if b[n - 1] > YLIM {
            b[n - 1] = YMAX;
        }
    } else {
        if b[1] < -YLIM {
            b[1] = -YMAX;
        }
        if b[n - 2] > YLIM {
            b[n - 2] = YMAX;
        }
    }
}

/// Stored bounds of an axis, if the axis names a bounds variable that exists and has `len`
/// values.
fn stored_bounds(
    ds: &Dataset,
    grid: &Grid,
    x: bool,
    len: usize,
    read: ReadVar<'_>,
) -> Result<Option<Vec<f64>>> {
    let axis = if x { &grid.x } else { &grid.y };
    let Some(name) = axis.as_ref().and_then(|a| a.bounds_var.as_deref()) else {
        return Ok(None);
    };
    if ds.var(name).is_none() {
        return Ok(None);
    }
    let v = read(name)?;
    Ok((v.len() == len).then_some(v))
}

/// `gen_gridcellarea_reg2d` (`grid_area.cc:296`).
fn gen_area_reg2d(
    ds: &Dataset,
    grid: &Grid,
    read: ReadVar<'_>,
    lweights: bool,
) -> Result<AreaResult> {
    let nlon = grid.xsize.max(1);
    let nlat = grid.ysize.max(1);
    let xvals = grid.xvals.as_deref().unwrap_or(&[]);
    let yvals = grid.yvals.as_deref().unwrap_or(&[]);
    let xb = stored_bounds(ds, grid, true, 2 * nlon, read)?;
    let yb = stored_bounds(ds, grid, false, 2 * nlat, read)?;
    let missing =
        |vals: &[f64], n: usize, b: &Option<Vec<f64>>| n > 1 && vals.len() != n && b.is_none();
    if missing(xvals, nlon, &xb) || missing(yvals, nlat, &yb) {
        return Ok(Err(AreaFailure::NoCorners));
    }
    if !lweights && (xb.is_none() && nlon == 1 || yb.is_none() && nlat == 1) {
        return Ok(Err(AreaFailure::NoCorners));
    }
    // A single row or column without bounds gets the pseudo extent [0, 0.01] (weights only).
    let pseudo = || vec![0.0, 0.01];
    let mut lon = match xb {
        Some(b) => b,
        None if nlon == 1 => pseudo(),
        None => gen_axis_bounds(xvals),
    };
    let mut lat = match yb {
        Some(ref b) => b.clone(),
        None if nlat == 1 => pseudo(),
        None => {
            let mut b = gen_axis_bounds(yvals);
            check_lat_borders(&mut b);
            b
        }
    };
    // cdo keeps one unit string for both axes and the latitude branch always sets it last
    // (grid_area.cc:316-361): the latitude units decide, or "radian" for a pseudo latitude
    // extent. So a single row without bounds leaves the longitudes in degrees.
    let unit = if yb.is_none() && nlat == 1 {
        Some("radian")
    } else {
        grid.y.as_ref().and_then(|a| a.units.as_deref())
    };
    if is_degrees(unit) {
        // grid_to_radian: scale_vec(DEG2RAD)
        let d2r = std::f64::consts::PI / 180.0;
        lon.iter_mut().for_each(|v| *v *= d2r);
        lat.iter_mut().for_each(|v| *v *= d2r);
    }
    let values = (0..nlon * nlat)
        .map(|i| {
            // getLonLatCorner, grid_area.cc:268
            let j = i / nlon;
            let (i2, j2) = ((i - j * nlon) * 2, j * 2);
            let lons = [lon[i2], lon[i2 + 1], lon[i2 + 1], lon[i2]];
            let (lo, hi) = if lat[j2 + 1] > lat[j2] {
                (lat[j2], lat[j2 + 1])
            } else {
                (lat[j2 + 1], lat[j2])
            };
            huiliers_area(&lons, &[lo, lo, hi, hi])
        })
        .collect();
    let source = if lweights && (nlon == 1 || nlat == 1) {
        AreaSource::PseudoBounds
    } else {
        AreaSource::Bounds
    };
    Ok(Ok(CellAreas { values, source }))
}

/// `gen_gridcellarea_unstruct` (`grid_area.cc:386`) for curvilinear (4 vertices) and
/// unstructured grids. ICON grid references (`dereferenceGrid`) are not followed.
fn gen_area_unstruct(ds: &Dataset, grid: &Grid, read: ReadVar<'_>) -> Result<AreaResult> {
    let n = grid.size;
    let nv = if grid.kind == GridKind::Unstructured {
        grid.nvertex.unwrap_or(0)
    } else {
        4
    };
    let (Some(xa), Some(ya)) = (&grid.x, &grid.y) else {
        return Ok(Err(AreaFailure::NoCorners));
    };
    if nv == 0 {
        return Ok(Err(AreaFailure::NoCorners));
    }
    let (Some(mut clon), Some(mut clat)) = (
        stored_bounds(ds, grid, true, nv * n, read)?,
        stored_bounds(ds, grid, false, nv * n, read)?,
    ) else {
        return Ok(Err(AreaFailure::NoCorners));
    };
    let (mut xc, mut yc) = if nv > 4 {
        (read(&xa.var)?, read(&ya.var)?)
    } else {
        (Vec::new(), Vec::new())
    };
    let d2r = std::f64::consts::PI / 180.0;
    if is_degrees(xa.units.as_deref()) {
        clon.iter_mut().chain(xc.iter_mut()).for_each(|v| *v *= d2r);
    }
    if is_degrees(ya.units.as_deref()) {
        clat.iter_mut().chain(yc.iter_mut()).for_each(|v| *v *= d2r);
    }
    // per-cell and independent: computed on the (small) global pool, the result is the same
    // (20 M cells of an ICON R2B8 grid took about 1 s on one thread)
    let values = (0..n)
        .into_par_iter()
        .map(|i| {
            let (lo, la) = (&clon[i * nv..(i + 1) * nv], &clat[i * nv..(i + 1) * nv]);
            if nv <= 4 {
                huiliers_area(lo, la)
            } else {
                huiliers_area_centre(lo, la, xc[i], yc[i])
            }
        })
        .collect();
    Ok(Ok(CellAreas {
        values,
        source: AreaSource::Bounds,
    }))
}

fn xyz(lon: f64, lat: f64) -> [f64; 3] {
    // lonlat_to_xyz, mpim_grid/grid_convert.h:9
    let c = lat.cos();
    [c * lon.cos(), c * lon.sin(), lat.sin()]
}

fn cross_norm(a: &[f64; 3], b: &[f64; 3]) -> f64 {
    let c = [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ];
    (c[0] * c[0] + c[1] * c[1] + c[2] * c[2]).sqrt()
}

/// Spherical triangle area by L'Huilier's theorem (`mod_tri_area`, `grid_area.cc:138`).
fn tri_area(u: &[f64; 3], v: &[f64; 3], w: &[f64; 3]) -> f64 {
    let a = cross_norm(u, v).asin();
    let b = cross_norm(u, w).asin();
    let c = cross_norm(w, v).asin();
    let s = 0.5 * (a + b + c);
    let t = (s * 0.5).tan() * ((s - a) * 0.5).tan() * ((s - b) * 0.5).tan() * ((s - c) * 0.5).tan();
    (4.0 * t.abs().sqrt().atan()).abs()
}

/// Fan of triangles from vertex 0 (`mod_huiliers_area`, `grid_area.cc:180`). Radians.
pub fn huiliers_area(lon: &[f64], lat: &[f64]) -> f64 {
    let n = lon.len();
    if n < 3 {
        return 0.0;
    }
    let p1 = xyz(lon[0], lat[0]);
    let mut p2 = xyz(lon[1], lat[1]);
    let mut sum = 0.0;
    for i in 2..n {
        let p3 = xyz(lon[i], lat[i]);
        sum += tri_area(&p1, &p2, &p3);
        p2 = p3;
    }
    sum
}

/// Fan of triangles from the cell centre, skipping repeated vertices
/// (`mod_huiliers_area2`, `grid_area.cc:210`). Radians.
pub fn huiliers_area_centre(lon: &[f64], lat: &[f64], clon: f64, clat: f64) -> f64 {
    let n = lon.len();
    if n < 3 {
        return 0.0;
    }
    // is_equal (compare.h) is an exact comparison for these values
    let p1 = xyz(clon, clat);
    let mut p2 = xyz(lon[0], lat[0]);
    let mut sum = 0.0;
    for i in 1..n {
        if lon[i] == lon[i - 1] && lat[i] == lat[i - 1] {
            continue;
        }
        let p3 = xyz(lon[i], lat[i]);
        sum += tri_area(&p1, &p2, &p3);
        p2 = p3;
    }
    if !(lon[0] == lon[n - 1] && lat[0] == lat[n - 1]) {
        sum += tri_area(&p1, &p2, &xyz(lon[0], lat[0]));
    }
    sum
}

// ---------------------------------------------------------------------------------------------
// Per-operator rules
// ---------------------------------------------------------------------------------------------

/// The horizontal weighting an operator applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpaceWeighting {
    /// Unweighted over the cells (min, max, range, sum, skew, kurt, median, count, pctl).
    None,
    /// Normalised cell weights ([`fld_weights`] / [`cell_weights`]), statistic Σw·x/Σw.
    Weights,
    /// Values multiplied by the cell areas in m² ([`cell_areas`]) and then summed (`*int`).
    Areas,
}

/// Weighting of the `fld*` operators (`Fldstat.cc:230-245`: flag f2 = 1 → weights; `fldint`
/// multiplies by `gridcell_areas`, `Fldstat.cc:285,367`). `None` for an unknown name.
pub fn fldstat_weighting(op: &str) -> Option<SpaceWeighting> {
    Some(match op {
        "fldmean" | "fldavg" | "fldstd" | "fldstd1" | "fldvar" | "fldvar1" => {
            SpaceWeighting::Weights
        }
        "fldint" => SpaceWeighting::Areas,
        "fldrange" | "fldmin" | "fldmax" | "fldsum" | "fldskew" | "fldkurt" | "fldmedian"
        | "fldcount" | "fldpctl" => SpaceWeighting::None,
        _ => return None,
    })
}

/// Weighting of the `mer*` operators: the global normalised cell weights restricted to each
/// column (`Merstat.cc:41-46,165`; `field_meridional.cc:63`).
pub fn merstat_weighting(op: &str) -> Option<SpaceWeighting> {
    Some(match op {
        "mermean" | "meravg" | "merstd" | "merstd1" | "mervar" | "mervar1" => {
            SpaceWeighting::Weights
        }
        "merrange" | "mermin" | "mermax" | "mersum" | "merskew" | "merkurt" | "mermedian"
        | "merpctl" => SpaceWeighting::None,
        _ => return None,
    })
}

/// Weighting of the `zon*` operators on regular, Gaussian and HEALPix grids: **unweighted**
/// within each latitude row or ring (`Zonstat.cc:141-155`, all f2 = 0; `field_zonal.cc:127`
/// uses `varray_mean`); `zonint` multiplies by the cell areas first (`Zonstat.cc:227,355`).
pub fn zonstat_weighting(op: &str) -> Option<SpaceWeighting> {
    Some(match op {
        "zonint" => SpaceWeighting::Areas,
        "zonrange" | "zonmin" | "zonmax" | "zonsum" | "zonmean" | "zonavg" | "zonstd"
        | "zonstd1" | "zonvar" | "zonvar1" | "zonskew" | "zonkurt" | "zonmedian" | "zonpctl" => {
            SpaceWeighting::None
        }
        _ => return None,
    })
}

/// Rows of a reduction over a subset of cells (zonal rows, meridional columns), in compressed
/// form: row `r` holds cells `index[offsets[r]..offsets[r + 1]]`, in the order cdo sums them.
#[derive(Debug, Clone)]
pub struct Rows {
    pub index: Vec<usize>,
    pub offsets: Vec<usize>,
    /// Coordinate of each output row: latitude (zonal) or longitude (meridional), degrees.
    pub coords: Vec<f64>,
}

impl Rows {
    pub fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Cell indices of row `r`.
    pub fn row(&self, r: usize) -> &[usize] {
        &self.index[self.offsets[r]..self.offsets[r + 1]]
    }
}

/// Zonal rows (`Zonstat.cc:251-340`, `gridToZonal` `mpim_grid.cc:180`): regular, Gaussian and
/// generic grids by latitude row (output latitudes = the grid's; generic: row numbers);
/// HEALPix by iso-latitude ring (4·nside − 1 rings, cells in RING order, latitude of the ring
/// centre, `hp_generate_ring_indices`/`hp_generate_latitudes`, `grid_healpix.cc:170,198`).
/// Curvilinear and unstructured grids need a zonal target grid in cdo (`zonmean,zonal_<dy>`,
/// remapping with `remap_weights_zonal`); not supported here.
pub fn zonal_rows(grid: &Grid) -> Result<Rows> {
    match grid.kind {
        GridKind::Regular | GridKind::Gaussian | GridKind::Generic => {
            let (nx, ny) = (grid.xsize.max(1), grid.ysize.max(1));
            let coords = grid
                .yvals
                .clone()
                .unwrap_or_else(|| (0..ny).map(|j| j as f64).collect());
            Ok(Rows {
                index: (0..nx * ny).collect(),
                offsets: (0..=ny).map(|j| j * nx).collect(),
                coords,
            })
        }
        GridKind::Healpix => {
            let hp = grid.healpix.as_ref().expect("HEALPix grid has parameters");
            let nside = hp.nside;
            let npix = 12 * nside * nside;
            if hp.index_var.is_some() || grid.size as u64 != npix {
                return Err(Error::new(
                    ErrorCode::UnsupportedGrid,
                    "zonal statistics on a HEALPix subset are not supported",
                ));
            }
            let depth = nside.trailing_zeros() as u8;
            let layer = cdshealpix::nested::get(depth);
            let index = (0..npix)
                .map(|k| match hp.order {
                    HealpixOrder::Ring => k as usize,
                    HealpixOrder::Nested => layer.from_ring(k) as usize,
                })
                .collect();
            let nrings = 4 * nside - 1;
            let mut offsets = vec![0usize];
            let mut coords = Vec::with_capacity(nrings as usize);
            for ring in 1..=nrings {
                // num_in_ring, grid_healpix.cc:183
                let k = if ring < nside {
                    4 * ring
                } else if ring < 3 * nside {
                    4 * nside
                } else {
                    4 * (4 * nside - ring)
                };
                offsets.push(offsets.last().unwrap() + k as usize);
                coords.push(healpix_ring_latitude(nside, ring));
            }
            Ok(Rows {
                index,
                offsets,
                coords,
            })
        }
        GridKind::Curvilinear | GridKind::Unstructured => Err(Error::new(
            ErrorCode::UnsupportedGrid,
            format!(
                "zonal statistics need a regular, Gaussian or HEALPix grid, not {}",
                grid.kind.name()
            ),
        )
        .with_hint("remap to a regular grid first (remapcon/remapbil)")),
    }
}

/// Latitude (degrees) of the centres of HEALPix ring `ring` (1-based, north to south).
pub fn healpix_ring_latitude(nside: u64, ring: u64) -> f64 {
    let (n, i) = (nside as f64, ring as f64);
    let z = if ring < nside {
        1.0 - i * i / (3.0 * n * n)
    } else if ring <= 3 * nside {
        4.0 / 3.0 - 2.0 * i / (3.0 * n)
    } else {
        let s = 4.0 * n - i;
        -(1.0 - s * s / (3.0 * n * n))
    };
    z.asin().to_degrees()
}

/// Meridional columns (`gridToMeridional`, `mpim_grid.cc:224`; `varray_copy_meridional`):
/// regular, Gaussian and generic grids only; cells of column i are j·nx + i, j = 0..ny.
pub fn meridional_columns(grid: &Grid) -> Result<Rows> {
    if !matches!(
        grid.kind,
        GridKind::Regular | GridKind::Gaussian | GridKind::Generic
    ) {
        return Err(Error::new(
            ErrorCode::UnsupportedGrid,
            format!(
                "meridional statistics need a regular or Gaussian grid, not {}",
                grid.kind.name()
            ),
        ));
    }
    let (nx, ny) = (grid.xsize.max(1), grid.ysize.max(1));
    let index = (0..nx)
        .flat_map(|i| (0..ny).map(move |j| j * nx + i))
        .collect();
    let coords = grid
        .xvals
        .clone()
        .unwrap_or_else(|| (0..nx).map(|i| i as f64).collect());
    Ok(Rows {
        index,
        offsets: (0..=nx).map(|i| i * ny).collect(),
        coords,
    })
}

// ---------------------------------------------------------------------------------------------
// Vertical
// ---------------------------------------------------------------------------------------------

/// What the `vert*` operators weight by (`Vertstat.cc:88-98`, f2 = 1 → layer weights).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VertWeighting {
    /// Unweighted (vertrange, vertmin, vertmax, vertsum).
    None,
    /// Σw·x/Σw with the layer weights (vertmean, vertavg, vertstd, vertstd1, vertvar, vertvar1).
    Weights,
    /// Σ thickness·x (vertint).
    Thickness,
}

pub fn vertstat_weighting(op: &str) -> Option<VertWeighting> {
    Some(match op {
        "vertmean" | "vertavg" | "vertstd" | "vertstd1" | "vertvar" | "vertvar1" => {
            VertWeighting::Weights
        }
        "vertint" => VertWeighting::Thickness,
        "vertrange" | "vertmin" | "vertmax" | "vertsum" => VertWeighting::None,
        _ => return None,
    })
}

/// Layer thickness and weights of a vertical axis (`get_layer_thickness`, `cdo_zaxis.cc:434`).
#[derive(Debug, Clone)]
pub struct LayerWeights {
    /// |upper − lower| bound per level; 1 without bounds.
    pub thickness: Vec<f64>,
    /// thickness / mean thickness (so they sum to nlev); 1 without bounds.
    pub weights: Vec<f64>,
    /// 0 = no bounds (cdo warns "Layer bounds not available, using constant vertical weights"
    /// for variables with more than one level), 1 = stored bounds, 2 = generated bounds,
    /// 3 = `weights=false` (constant, no warning).
    pub status: u8,
}

/// Layer weights for `nlev` levels of `zaxis` (`None` for a variable without vertical axis).
/// `use_weights` and `gen_bounds` are cdo's `vertmean,weights=…,genbounds=…` parameters
/// (`Vertstat.cc:66-77`); with `use_weights = false` the weights are constant.
pub fn layer_weights(
    zaxis: Option<&ZAxis>,
    nlev: usize,
    use_weights: bool,
    gen_bounds: bool,
) -> LayerWeights {
    let gen_bounds = gen_bounds && use_weights;
    let mut lb = vec![0.0; nlev];
    let mut ub = vec![1.0; nlev];
    let mut status = 0;
    let levels: Vec<f64> = zaxis
        .map(|z| z.values.clone())
        .filter(|v| v.len() == nlev)
        .unwrap_or_else(|| (1..=nlev).map(|l| l as f64).collect());
    if gen_bounds {
        // gen_layer_bounds, cdo_zaxis.cc:413
        status = 2;
        if nlev > 1 {
            lb[0] = levels[0];
            ub[nlev - 1] = levels[nlev - 1];
            for i in 0..nlev - 1 {
                let b = 0.5 * (levels[i] + levels[i + 1]);
                lb[i + 1] = b;
                ub[i] = b;
            }
        }
    } else if use_weights
        && let Some(b) = zaxis
            .and_then(|z| z.bounds.as_ref())
            .filter(|b| b.len() == nlev)
    {
        status = 1;
        for (i, [l, u]) in b.iter().enumerate() {
            lb[i] = *l;
            ub[i] = *u;
        }
    }
    let thickness: Vec<f64> = (0..nlev).map(|i| (ub[i] - lb[i]).abs()).collect();
    let mean = thickness.iter().sum::<f64>() / nlev as f64;
    let weights = thickness.iter().map(|t| t / mean).collect();
    if !use_weights {
        status = 3;
    }
    LayerWeights {
        thickness,
        weights,
        status,
    }
}

// ---------------------------------------------------------------------------------------------
// Weighted sums
// ---------------------------------------------------------------------------------------------

/// Running weighted sums for mean, variance and standard deviation, as `varray_weighted_mean`
/// and `varray_weighted_prevarsum` (`varray.cc:705,1051`). Missing values (NaN) are skipped;
/// accumulation in f64 in the order of [`WeightedSums::add`] calls (cdo uses an OpenMP SIMD
/// reduction, so its order differs in the last bits only).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct WeightedSums {
    pub sum: f64,
    pub sumq: f64,
    pub sumw: f64,
    pub sumwq: f64,
}

impl WeightedSums {
    #[inline]
    pub fn add(&mut self, w: f64, x: f64) {
        // branch-free: a missing value adds -0.0, which leaves every sum unchanged bitwise
        let valid = !x.is_nan();
        let keep = |v: f64| if valid { v } else { -0.0 };
        self.sum += keep(w * x);
        self.sumq += keep(w * x * x);
        self.sumw += keep(w);
        self.sumwq += keep(w * w);
    }

    /// Σw·x / Σw; NaN (missing) if Σw = 0.
    pub fn mean(&self) -> f64 {
        if self.sumw == 0.0 {
            f64::NAN
        } else {
            self.sum / self.sumw
        }
    }

    /// Population variance (`varray_weighted_var`, `varray.cc:1124`): small negative values
    /// above −1e-5 become 0.
    pub fn var(&self) -> f64 {
        let v = if self.sumw != 0.0 {
            (self.sumq * self.sumw - self.sum * self.sum) / (self.sumw * self.sumw)
        } else {
            f64::NAN
        };
        if v < 0.0 && v > -1e-5 { 0.0 } else { v }
    }

    /// Sample variance with weights (`varray_weighted_var_1`, `varray.cc:1147`).
    pub fn var1(&self) -> f64 {
        let v = if self.sumw * self.sumw > self.sumwq {
            (self.sumq * self.sumw - self.sum * self.sum) / (self.sumw * self.sumw - self.sumwq)
        } else {
            f64::NAN
        };
        if v < 0.0 && v > -1e-5 { 0.0 } else { v }
    }

    pub fn std(&self) -> f64 {
        var_to_std(self.var())
    }

    pub fn std1(&self) -> f64 {
        var_to_std(self.var1())
    }
}

/// `var_to_std` (`field.cc:312`): missing for negative variance, 0 for 0, else the root.
pub fn var_to_std(v: f64) -> f64 {
    if v.is_nan() || v < 0.0 {
        f64::NAN
    } else if v == 0.0 {
        0.0
    } else {
        v.sqrt()
    }
}
