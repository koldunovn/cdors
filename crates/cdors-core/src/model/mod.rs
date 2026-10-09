//! Dataset model (metadata only): dataset, variables, grids, vertical axes, CF time.
//!
//! Readers (`crate::io`) list the stored variables with their dimensions, attributes, data types
//! and chunk shapes; [`classify`] then works out, as CDI's netCDF reader does, which variables
//! are data and which are coordinates, the role of every dimension (time, vertical, horizontal,
//! other), the grids, the vertical axes and the decoded time axis.

pub mod area;
pub mod dataset;
pub mod grid;
pub mod hpcoords;
pub mod time;
pub mod timegroup;
pub mod zaxis;

pub use dataset::{
    AttrValue, Attrs, DType, Dataset, DimRole, Encoding, Format, VarDim, VarKind, Variable,
};
pub use grid::{CoordAxis, Grid, GridKind, GridMapping, Healpix, HealpixOrder};
pub use time::{CalDateTime, Calendar, TimeAxis, TimeStep, TimeUnit, TimeUnits};
pub use zaxis::ZAxis;

use crate::error::{Error, Result};
use std::collections::{HashMap, HashSet};

/// Reads all values of a stored variable as f64 (unpacked, missing values as NaN).
pub type ReadVar<'a> = &'a dyn Fn(&str) -> Result<Vec<f64>>;

fn is_lon_units(u: &str) -> bool {
    matches!(
        u.trim(),
        "degrees_east"
            | "degree_east"
            | "degree_E"
            | "degrees_E"
            | "degreeE"
            | "degreesE"
            | "degrees_east "
    )
}

fn is_lat_units(u: &str) -> bool {
    matches!(
        u.trim(),
        "degrees_north" | "degree_north" | "degree_N" | "degrees_N" | "degreeN" | "degreesN"
    )
}

fn is_radian(u: &str) -> bool {
    u.trim().starts_with("radian")
}

/// Longitude test (CDI `is_lon_axis`): units degrees_east, or standard_name longitude, or an
/// `X` axis / common name with degree or radian units.
fn is_lon(v: &Variable) -> bool {
    let units = v.attrs.get_str("units").unwrap_or("");
    let sn = v.attrs.get_str("standard_name").unwrap_or("");
    if is_lon_units(units) || sn == "longitude" {
        return true;
    }
    let deg = units.starts_with("degree") || is_radian(units);
    let name = v.name.to_ascii_lowercase();
    deg && (v.attrs.get_str("axis") == Some("X")
        || matches!(
            name.as_str(),
            "lon" | "longitude" | "clon" | "nav_lon" | "glon"
        ))
}

fn is_lat(v: &Variable) -> bool {
    let units = v.attrs.get_str("units").unwrap_or("");
    let sn = v.attrs.get_str("standard_name").unwrap_or("");
    if is_lat_units(units) || sn == "latitude" {
        return true;
    }
    let deg = units.starts_with("degree") || is_radian(units);
    let name = v.name.to_ascii_lowercase();
    deg && (v.attrs.get_str("axis") == Some("Y")
        || matches!(
            name.as_str(),
            "lat" | "latitude" | "clat" | "nav_lat" | "glat"
        ))
}

const VERTICAL_STD_NAMES: &[&str] = &[
    "air_pressure",
    "height",
    "depth",
    "altitude",
    "model_level_number",
    "atmosphere_hybrid_sigma_pressure_coordinate",
    "atmosphere_hybrid_height_coordinate",
    "atmosphere_sigma_coordinate",
    "atmosphere_ln_pressure_coordinate",
    "ocean_sigma_coordinate",
    "ocean_s_coordinate",
    "ocean_double_sigma_coordinate",
    "ocean_sigma_z_coordinate",
    "height_above_geopotential_datum",
    "height_above_mean_sea_level",
    "depth_below_geoid",
    "depth_below_sea_floor",
    "land_ice_sigma_coordinate",
    "sea_floor_depth_below_sea_surface",
];

const VERTICAL_NAMES: &[&str] = &[
    "lev",
    "level",
    "levels",
    "plev",
    "depth",
    "height",
    "alt",
    "altitude",
    "z",
    "zlev",
    "nz",
    "nz1",
    "lev_2",
    "depth_2",
    "olevel",
    "deptht",
    "depthu",
    "depthv",
    "depthw",
    "pressure",
    "isobaric",
    "height_2m",
    "height_10m",
    "soil_depth",
    "nlev",
    "ilev",
];

fn is_vertical(v: &Variable) -> bool {
    let a = &v.attrs;
    if a.get_str("axis")
        .is_some_and(|x| x.eq_ignore_ascii_case("Z"))
        || a.get("positive").is_some()
    {
        return true;
    }
    if a.get_str("standard_name")
        .is_some_and(|s| VERTICAL_STD_NAMES.contains(&s))
    {
        return true;
    }
    if a.get_str("units").is_some_and(|u| {
        matches!(
            u.trim(),
            "Pa" | "hPa" | "mbar" | "millibar" | "bar" | "dbar"
        )
    }) {
        return true;
    }
    VERTICAL_NAMES.contains(&v.name.to_ascii_lowercase().as_str())
}

/// Names listed in an attribute (`coordinates`, `bounds`, ...); for `grid_mapping` also the
/// extended form `"crs: lat lon"`.
fn attr_names(v: &Variable, attr: &str) -> Vec<String> {
    let Some(s) = v.attrs.get_str(attr) else {
        return Vec::new();
    };
    if attr == "grid_mapping" && s.contains(':') {
        return s
            .split_whitespace()
            .filter_map(|t| t.strip_suffix(':'))
            .map(str::to_owned)
            .collect();
    }
    if attr == "cell_measures" {
        return s
            .split_whitespace()
            .filter(|t| !t.ends_with(':'))
            .map(str::to_owned)
            .collect();
    }
    s.split_whitespace().map(str::to_owned).collect()
}

/// Classifies the stored variables of `ds` (whose `vars` the reader filled in with
/// `kind = Data`, `role = Other`) and builds grids, vertical axes and the time axis.
pub fn classify(ds: &mut Dataset, read: ReadVar<'_>) -> Result<()> {
    let names: HashSet<String> = ds.vars.iter().map(|v| v.name.clone()).collect();
    let mut kind: HashMap<String, VarKind> = HashMap::new();

    // 1. Referenced variables and dimension coordinates.
    for v in &ds.vars {
        for b in attr_names(v, "bounds")
            .into_iter()
            .chain(attr_names(v, "climatology"))
        {
            kind.insert(b, VarKind::Bounds);
        }
        for g in attr_names(v, "grid_mapping") {
            kind.insert(g, VarKind::GridMapping);
        }
        for c in attr_names(v, "cell_measures") {
            kind.entry(c).or_insert(VarKind::Ancillary);
        }
    }
    for v in &ds.vars {
        for c in attr_names(v, "coordinates") {
            if names.contains(&c) {
                kind.entry(c).or_insert(VarKind::Auxiliary);
            }
        }
        // a dimension coordinate, unless it is referenced as bounds or grid mapping (nextGEMS
        // stores `crs` as a 1-element array over a dimension `crs`)
        if v.dims.len() == 1
            && v.dims[0].name == v.name
            && !matches!(
                kind.get(&v.name),
                Some(VarKind::Bounds | VarKind::GridMapping)
            )
        {
            kind.insert(v.name.clone(), VarKind::Coordinate);
        }
        if !v.dtype.is_numeric() {
            kind.entry(v.name.clone()).or_insert(VarKind::Ancillary);
        }
    }
    // Grid-mapping variables are also recognised without a reference.
    for v in &ds.vars {
        if v.dims.is_empty() && v.attrs.get("grid_mapping_name").is_some() {
            kind.insert(v.name.clone(), VarKind::GridMapping);
        }
    }

    // 2. Time axis: a 1-D variable with CF time units, preferring dimension coordinates.
    let is_time_var = |v: &Variable| {
        v.dims.len() == 1
            && v.dtype.is_numeric()
            && kind.get(&v.name) != Some(&VarKind::Bounds)
            && v.attrs
                .get_str("units")
                .is_some_and(TimeUnits::is_time_units)
    };
    let time_var = ds
        .vars
        .iter()
        .filter(|v| is_time_var(v))
        .find(|v| v.dims[0].name == v.name)
        .or_else(|| {
            ds.vars
                .iter()
                .filter(|v| is_time_var(v))
                .find(|v| v.attrs.get_str("axis") == Some("T") || v.name == "time")
        })
        .map(|v| v.name.clone());
    if let Some(tv) = &time_var {
        let v = ds.var(tv).expect("time variable exists");
        let dim = v.dims[0].name.clone();
        let units = v.attrs.get_str("units").unwrap_or_default().to_owned();
        let cal = v.attrs.get_str("calendar").map(str::to_owned);
        let bounds_name = v
            .attrs
            .get_str("bounds")
            .or_else(|| v.attrs.get_str("climatology"))
            .filter(|b| names.contains(*b))
            .map(str::to_owned);
        let values = read(tv)?;
        let bvals = match &bounds_name {
            Some(b) => Some(read(b)?),
            None => None,
        };
        let axis = TimeAxis::decode(
            tv,
            &dim,
            &units,
            cal.as_deref(),
            &values,
            bounds_name.as_deref().zip(bvals.as_deref()),
        )?;
        kind.insert(tv.clone(), VarKind::Coordinate);
        ds.time = Some(axis);
    }
    let time_dim = ds.time.as_ref().map(|t| t.dim.clone());

    // 3. Dimension coordinates by role.
    let mut lon_dims: HashMap<String, String> = HashMap::new(); // dim -> lon var
    let mut lat_dims: HashMap<String, String> = HashMap::new();
    let mut vert_dims: HashMap<String, String> = HashMap::new();
    for v in &ds.vars {
        if kind.get(&v.name) != Some(&VarKind::Coordinate) || v.dims.len() != 1 {
            continue;
        }
        let d = &v.dims[0].name;
        if Some(d) == time_dim.as_ref() {
            continue;
        }
        if is_lon(v) {
            lon_dims.insert(d.clone(), v.name.clone());
        } else if is_lat(v) {
            lat_dims.insert(d.clone(), v.name.clone());
        } else if is_vertical(v) {
            vert_dims.insert(d.clone(), v.name.clone());
        }
    }

    // 4. Data variables.
    let mut grids: Vec<Grid> = Vec::new();
    let mut zaxes: Vec<ZAxis> = Vec::new();
    let mut updates: Vec<(usize, Variable)> = Vec::new();
    for (idx, v) in ds.vars.iter().enumerate() {
        let k = kind.get(&v.name).copied().unwrap_or(VarKind::Data);
        let mut nv = v.clone();
        nv.kind = k;
        if k != VarKind::Data {
            updates.push((idx, nv));
            continue;
        }
        for d in &mut nv.dims {
            if Some(&d.name) == time_dim.as_ref() {
                d.role = DimRole::Time;
            } else if vert_dims.contains_key(&d.name) {
                d.role = DimRole::Vertical;
            }
        }
        // vertical axis
        if let Some(d) = nv.dims.iter().find(|d| d.role == DimRole::Vertical) {
            let zvar = &vert_dims[&d.name];
            let zi = match zaxes.iter().position(|z| &z.var == zvar) {
                Some(i) => i,
                None => {
                    zaxes.push(build_zaxis(ds, zvar, &d.name, read)?);
                    zaxes.len() - 1
                }
            };
            nv.zaxis = Some(zi);
        }
        // horizontal grid
        let free: Vec<usize> = nv
            .dims
            .iter()
            .enumerate()
            .filter(|(_, d)| d.role == DimRole::Other)
            .map(|(i, _)| i)
            .collect();
        if let Some((grid, hdims)) = detect_grid(ds, &nv, &free, &lon_dims, &lat_dims, read)? {
            for i in hdims {
                nv.dims[i].role = DimRole::Horizontal;
            }
            let gi = match grids.iter().position(|g| g.same_as(&grid)) {
                Some(i) => i,
                None => {
                    grids.push(grid);
                    grids.len() - 1
                }
            };
            nv.grid = Some(gi);
        }
        updates.push((idx, nv));
    }
    for (i, v) in updates {
        ds.vars[i] = v;
    }
    // A variable without coordinates over the same horizontal dimensions as a variable with a
    // known grid shares that grid (cdo does the same: nextGEMS variables without `grid_mapping`
    // over `cell` are HEALPix too). Unused generic grids are dropped.
    let remap: Vec<usize> = (0..grids.len())
        .map(|i| {
            if grids[i].kind != GridKind::Generic {
                return i;
            }
            grids
                .iter()
                .position(|g| g.kind != GridKind::Generic && g.dims == grids[i].dims)
                .unwrap_or(i)
        })
        .collect();
    let used: Vec<usize> = (0..grids.len()).filter(|&i| remap[i] == i).collect();
    for v in &mut ds.vars {
        if let Some(g) = v.grid {
            let target = remap[g];
            v.grid = used.iter().position(|&u| u == target);
        }
    }
    ds.grids = used.iter().map(|&i| grids[i].clone()).collect();
    ds.zaxes = zaxes;
    Ok(())
}

fn build_zaxis(ds: &Dataset, var: &str, dim: &str, read: ReadVar<'_>) -> Result<ZAxis> {
    let v = ds.var(var).expect("vertical coordinate exists");
    let values = read(var)?;
    // Only bounds of the expected size (n × 2) are read: some stores carry time-dependent
    // bounds (EERIE `height_3_bnds`, 36890 × 1 × 2 in one-step chunks), which would cost one
    // read per time step, and would be discarded anyway.
    let bounds_ok = |b: &str| {
        ds.var(b)
            .is_some_and(|bv| bv.dims.iter().map(|d| d.size).product::<usize>() == 2 * values.len())
    };
    let bounds = match v.attrs.get_str("bounds").filter(|b| bounds_ok(b)) {
        Some(b) => {
            let bv = read(b)?;
            (bv.len() == 2 * values.len()).then(|| bv.chunks(2).map(|p| [p[0], p[1]]).collect())
        }
        None => None,
    };
    let s = |k: &str| v.attrs.get_str(k).map(str::to_owned);
    Ok(ZAxis {
        var: var.to_owned(),
        dim: dim.to_owned(),
        values,
        units: s("units"),
        long_name: s("long_name"),
        standard_name: s("standard_name"),
        positive: s("positive"),
        bounds,
    })
}

/// Axis description; like CDI, a missing long_name/units defaults to longitude/degrees_east
/// (latitude/degrees_north for `lat`).
fn coord_axis(v: &Variable, dim: &str, lat: bool) -> CoordAxis {
    let s = |k: &str| {
        v.attrs
            .get_str(k)
            .filter(|x| !x.is_empty())
            .map(str::to_owned)
    };
    let (dl, du) = if lat {
        ("latitude", "degrees_north")
    } else {
        ("longitude", "degrees_east")
    };
    CoordAxis {
        var: v.name.clone(),
        dim: dim.to_owned(),
        long_name: s("long_name").or_else(|| Some(dl.to_owned())),
        units: s("units").or_else(|| Some(du.to_owned())),
        standard_name: s("standard_name"),
        is_f32: v.dtype == DType::F32,
        bounds_var: s("bounds"),
    }
}

/// Fills `nvertex`/`vdim` from the bounds variable of the x axis, if it exists.
fn set_vertex_info(ds: &Dataset, g: &mut Grid) {
    let Some(b) = g.x.as_ref().and_then(|x| x.bounds_var.clone()) else {
        return;
    };
    if let Some(bv) = ds.var(&b)
        && let Some(last) = bv.dims.last()
        && bv.dims.len() >= 2
    {
        g.nvertex = Some(last.size);
        g.vdim = Some(last.name.clone());
    }
}

type GridMatch = Option<(Grid, Vec<usize>)>;

/// Detects the horizontal grid of data variable `v` among its dimensions `free`
/// (those not time or vertical). Returns the grid and the indices of its dimensions in `v`.
fn detect_grid(
    ds: &Dataset,
    v: &Variable,
    free: &[usize],
    lon_dims: &HashMap<String, String>,
    lat_dims: &HashMap<String, String>,
    read: ReadVar<'_>,
) -> Result<GridMatch> {
    if free.is_empty() {
        return Ok(None);
    }
    let last = *free.last().expect("non-empty");
    let dname = |i: usize| v.dims[i].name.clone();

    // a) HEALPix via grid_mapping
    for gm in attr_names(v, "grid_mapping") {
        let Some(m) = ds.var(&gm) else { continue };
        let Some(gmn) = m.attrs.get_str("grid_mapping_name") else {
            continue;
        };
        let mapping = GridMapping {
            var: gm.clone(),
            name: gmn.to_owned(),
            attrs: m.attrs.clone(),
        };
        if gmn == "healpix" {
            let size = v.dims[last].size;
            let bad = |msg: String| {
                Err(Error::bad_data(format!("grid mapping '{gm}' of '{}': {msg}", v.name))
                    .with("variable", v.name.clone())
                    .with_hint(
                        "a HEALPix grid mapping needs healpix_nside (or CF refinement_level) and \
                         healpix_order (or CF indexing_scheme) = \"nested\" or \"ring\"",
                    ))
            };
            // healpix_nside (cdo, easygems), else CF's refinement_level (nside = 2^level), else
            // from the size of a complete map
            let nside_f = m.attrs.get_f64("healpix_nside").or_else(|| {
                m.attrs
                    .get_f64("refinement_level")
                    .filter(|l| (0.0..=29.0).contains(l) && l.fract() == 0.0)
                    .map(|l| 2f64.powi(l as i32))
            });
            let nside = match nside_f {
                Some(x) if x >= 1.0 && x <= (1u64 << 29) as f64 && x.fract() == 0.0 => x as u64,
                Some(x) => return bad(format!("invalid nside {x}")),
                None => ((size / 12) as f64).sqrt().round() as u64,
            };
            if nside == 0 || size as u128 > 12 * (nside as u128) * (nside as u128) {
                return bad(format!(
                    "nside {nside} does not fit the dimension of {size} cells"
                ));
            }
            let order_attr = m
                .attrs
                .get_str("healpix_order")
                .or_else(|| m.attrs.get_str("indexing_scheme"));
            let order = match order_attr.map(|o| o.trim().to_ascii_lowercase()) {
                Some(o) if o == "nested" || o == "nest" => HealpixOrder::Nested,
                Some(o) if o == "ring" => HealpixOrder::Ring,
                Some(o) => return bad(format!("unknown HEALPix order '{o}'")),
                None => return bad("no healpix_order or indexing_scheme".into()),
            };
            if order == HealpixOrder::Nested && !nside.is_power_of_two() {
                return bad(format!(
                    "nside {nside} of a nested grid is not a power of 2"
                ));
            }
            let index_var = ds
                .vars
                .iter()
                .find(|c| {
                    c.dims.len() == 1
                        && c.dims[0].name == v.dims[last].name
                        && c.attrs.get_str("standard_name") == Some("healpix_index")
                })
                .map(|c| c.name.clone());
            let mut g = Grid::new(GridKind::Healpix, vec![dname(last)], size, 0);
            g.healpix = Some(Healpix {
                nside,
                order,
                index_var,
            });
            g.mapping = Some(mapping);
            return Ok(Some((g, vec![last])));
        }
    }

    // b) auxiliary coordinates: curvilinear (2-D) or unstructured (1-D)
    let coords: Vec<&Variable> = attr_names(v, "coordinates")
        .iter()
        .filter_map(|c| ds.var(c))
        .collect();
    let lon = coords.iter().find(|c| is_lon(c)).copied();
    let lat = coords.iter().find(|c| is_lat(c)).copied();
    if let (Some(lon), Some(lat)) = (lon, lat) {
        if lon.dims.len() == 2 && free.len() >= 2 {
            let (iy, ix) = (free[free.len() - 2], last);
            if lon.dims[0].name == v.dims[iy].name && lon.dims[1].name == v.dims[ix].name {
                let (nx, ny) = (v.dims[ix].size, v.dims[iy].size);
                let mut g = Grid::new(GridKind::Curvilinear, vec![dname(iy), dname(ix)], nx, ny);
                g.x = Some(coord_axis(lon, &dname(ix), false));
                g.y = Some(coord_axis(lat, &dname(iy), true));
                g.radians = lon.units().is_some_and(is_radian);
                set_vertex_info(ds, &mut g);
                return Ok(Some((g, vec![iy, ix])));
            }
        }
        if lon.dims.len() == 1 && lon.dims[0].name == v.dims[last].name {
            return Ok(Some(unstructured(ds, v, last, lon, lat)));
        }
    }

    // c) regular lon-lat from dimension coordinates
    if free.len() >= 2 {
        let (iy, ix) = (free[free.len() - 2], last);
        let (dy, dx) = (dname(iy), dname(ix));
        let pair = match (lat_dims.get(&dy), lon_dims.get(&dx)) {
            (Some(la), Some(lo)) => Some((la.clone(), lo.clone(), iy, ix)),
            _ => match (lon_dims.get(&dy), lat_dims.get(&dx)) {
                (Some(lo), Some(la)) => Some((la.clone(), lo.clone(), ix, iy)),
                _ => None,
            },
        };
        if let Some((la, lo, iy, ix)) = pair {
            let (lav, lov) = (ds.var(&la).expect("lat"), ds.var(&lo).expect("lon"));
            let yvals = read(&la)?;
            let xvals = read(&lo)?;
            let ny = yvals.len();
            let mut equidistant = ny > 1;
            if ny > 1 {
                let yinc = (yvals[0] - yvals[1]).abs();
                for i in 2..ny {
                    if ((yvals[i - 1] - yvals[i]).abs() - yinc) > yinc / 1000.0 {
                        equidistant = false;
                        break;
                    }
                }
            }
            let gaussian = ny < 10_000 && !equidistant && grid::latitudes_are_gaussian(&yvals);
            let kind = if gaussian {
                GridKind::Gaussian
            } else {
                GridKind::Regular
            };
            // storage order
            let dims = vec![dname(iy.min(ix)), dname(iy.max(ix))];
            let mut g = Grid::new(kind, dims, xvals.len(), ny);
            g.x = Some(coord_axis(lov, &lo, false));
            g.y = Some(coord_axis(lav, &la, true));
            g.radians = lov.units().is_some_and(is_radian);
            if gaussian {
                g.gaussian_np = Some(
                    v.attrs
                        .get_f64("CDI_grid_num_LPE")
                        .map_or(ny / 2, |x| x as usize),
                );
            }
            g.xvals = Some(xvals);
            g.yvals = Some(yvals);
            set_vertex_info(ds, &mut g);
            return Ok(Some((g, vec![iy, ix])));
        }
    }

    // d) unstructured without a coordinates attribute (lon/lat over the cell dimension)
    let cell = &v.dims[last].name;
    let over_cell = |f: fn(&Variable) -> bool| {
        ds.vars
            .iter()
            .find(|c| c.dims.len() == 1 && &c.dims[0].name == cell && c.name != v.name && f(c))
    };
    if let (Some(lon), Some(lat)) = (over_cell(is_lon), over_cell(is_lat)) {
        return Ok(Some(unstructured(ds, v, last, lon, lat)));
    }

    // e) generic: the last one or two free dimensions, without coordinates
    if free.len() >= 2 {
        let (iy, ix) = (free[free.len() - 2], last);
        let g = Grid::new(
            GridKind::Generic,
            vec![dname(iy), dname(ix)],
            v.dims[ix].size,
            v.dims[iy].size,
        );
        return Ok(Some((g, vec![iy, ix])));
    }
    let g = Grid::new(GridKind::Generic, vec![dname(last)], v.dims[last].size, 0);
    Ok(Some((g, vec![last])))
}

fn unstructured(
    ds: &Dataset,
    v: &Variable,
    idx: usize,
    lon: &Variable,
    lat: &Variable,
) -> (Grid, Vec<usize>) {
    let d = v.dims[idx].name.clone();
    let mut g = Grid::new(GridKind::Unstructured, vec![d.clone()], v.dims[idx].size, 0);
    g.x = Some(coord_axis(lon, &d, false));
    g.y = Some(coord_axis(lat, &d, true));
    g.radians = lon.units().is_some_and(is_radian);
    set_vertex_info(ds, &mut g);
    let number = ds
        .attrs
        .get("number_of_grid_used")
        .filter(|a| matches!(a, AttrValue::Ints(_)))
        .and_then(AttrValue::as_f64)
        .map(|x| x as i64);
    let uri = ds
        .attrs
        .get_str("grid_file_uri")
        .or_else(|| ds.attrs.get_str("ICON_grid_file_uri"))
        .map(str::to_owned);
    let uuid = ds
        .attrs
        .get_str("uuidOfHGrid")
        .filter(|u| u.len() == 36)
        .map(str::to_ascii_lowercase);
    if number.is_some() || uri.is_some() || uuid.is_some() {
        g.reference = Some(grid::GridReference {
            number,
            position: v
                .attrs
                .get_f64("number_of_grid_in_reference")
                .map_or(0, |x| x as i64),
            uri,
            uuid,
        });
    }
    (g, vec![idx])
}
