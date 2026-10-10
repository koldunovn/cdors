//! gribscan's references to GRIB files (`gribscan-build`, as made for the EERIE IFS-FESOM data on
//! Levante): the horizontal grid of the GRIB messages restored when the store is opened.
//!
//! gribscan stores every field as one GRIB message per chunk (codec `gribscan.rawgrib`, decoded
//! in [`crate::io::grib`]) and flattens every grid to one dimension (`value`), with the latitude
//! and longitude of each point in `lat(value)` and `lon(value)`, which the data variables name in
//! `coordinates`. Read as stored, every grid would be an unstructured grid without cell bounds,
//! whose field means can only weight all cells equally. cdors looks at the coordinates instead:
//! - **regular** (rows of equal latitude, all with the same longitudes: GRIB's `regular_ll` and
//!   `regular_gg`): the variables get the dimensions `lat` and `lon` (still one chunk per
//!   message) and `lat(lat)`, `lon(lon)` become 1-D coordinates, a regular or Gaussian grid as
//!   cdo reads it from the GRIB file;
//! - **reduced** (rows of equal latitude, each with its own number of longitudes equally spaced
//!   around the globe: GRIB's `reduced_gg` and `reduced_ll`): an unstructured grid that knows its
//!   rows, from which the cell areas are computed ([`crate::model::area`]);
//! - anything else stays an unstructured grid with cell centres only.
//!
//! Pressure levels (`typeOfLevel = isobaricInhPa`, stored in hPa without units) are given in Pa,
//! as CDI reads GRIB pressure levels.

use super::ZarrSource;
use crate::error::{Error, Result};
use crate::io::ChunkSource;
use crate::model::{AttrValue, ReducedRows, VarDim};
use std::collections::HashMap;
use std::sync::Arc;

/// How the points of a grid are laid out, judged from the latitude and longitude of each point.
#[derive(Debug, PartialEq)]
pub(crate) enum Layout {
    /// One row per latitude, every row with these longitudes: `lats.len() * lons.len()` points.
    Regular { lats: Vec<f64>, lons: Vec<f64> },
    /// Rows of equal latitude with their own numbers of equally spaced longitudes.
    Reduced(ReducedRows),
    /// Anything else.
    Points,
}

/// Longitudes spaced by within this many degrees of 360°/n count as equally spaced.
const LON_TOL: f64 = 1e-6;

/// Longitudes made continuous: each one moved by multiples of 360° to within 180° of the one
/// before (gribscan writes 0 ... 179.75, -180 ... -0.25 for a row the GRIB message gives as
/// 0 ... 359.75).
fn unwrap(lon: &[f64]) -> Vec<f64> {
    let mut out: Vec<f64> = Vec::with_capacity(lon.len());
    for &x in lon {
        out.push(match out.last() {
            Some(&p) => x - 360.0 * ((x - p) / 360.0).round(),
            None => x,
        });
    }
    out
}

/// The layout of points given row by row (as GRIB stores them, rows of equal latitude one after
/// the other). Latitudes must be strictly monotonic from row to row; longitudes strictly monotonic
/// within a row, modulo 360°.
pub(crate) fn layout(lat: &[f64], lon: &[f64]) -> Layout {
    let n = lat.len();
    if n < 2 || lon.len() != n || lat.iter().chain(lon).any(|x| !x.is_finite()) {
        return Layout::Points;
    }
    let mut starts = vec![0];
    starts.extend((1..n).filter(|&k| lat[k] != lat[k - 1]));
    starts.push(n);
    let counts: Vec<usize> = starts.windows(2).map(|w| w[1] - w[0]).collect();
    let lats: Vec<f64> = starts[..counts.len()].iter().map(|&s| lat[s]).collect();
    let monotonic =
        |v: &[f64]| v.windows(2).all(|w| w[1] > w[0]) || v.windows(2).all(|w| w[1] < w[0]);
    if !monotonic(&lats) || lats.iter().any(|x| x.abs() > 90.0) {
        return Layout::Points;
    }
    let rows = || starts.windows(2).map(|w| &lon[w[0]..w[1]]);
    let nx = counts[0];
    if counts.iter().all(|&c| c == nx) {
        let first = &lon[..nx];
        let lons = unwrap(first);
        if (nx == 1 || monotonic(&lons)) && rows().all(|r| r == first) {
            return Layout::Regular { lats, lons };
        }
    }
    let equally_spaced = |r: &[f64]| {
        let step = 360.0 / r.len() as f64;
        r.iter().enumerate().all(|(i, &x)| {
            let d = (x - (r[0] + i as f64 * step)).rem_euclid(360.0);
            d <= LON_TOL || 360.0 - d <= LON_TOL
        })
    };
    if rows().all(equally_spaced) {
        return Layout::Reduced(ReducedRows { lats, counts });
    }
    Layout::Points
}

/// Restores the grids of the GRIB arrays `grib` of `src` before the dataset is classified.
/// Returns the reduced grids found, by dimension name, for [`set_reduced`].
pub(super) fn restore(src: &mut ZarrSource, grib: &[String]) -> Result<Vec<(String, ReducedRows)>> {
    let mut reduced = Vec::new();
    let mut hdims: Vec<String> = Vec::new();
    for v in src.ds.vars.iter().filter(|v| grib.contains(&v.name)) {
        if let Some(d) = v.dims.last()
            && !hdims.contains(&d.name)
        {
            hdims.push(d.name.clone());
        }
    }
    for dim in hdims {
        let over = |pred: fn(&crate::model::Variable) -> bool| {
            src.ds
                .vars
                .iter()
                .find(|c| {
                    !grib.contains(&c.name) && c.dims.len() == 1 && c.dims[0].name == dim && pred(c)
                })
                .map(|c| c.name.clone())
        };
        let (Some(latn), Some(lonn)) = (over(is_lat), over(is_lon)) else {
            continue;
        };
        let (lat, lon) = (src.read_var(&latn)?, src.read_var(&lonn)?);
        match layout(&lat, &lon) {
            Layout::Regular { lats, lons } => reshape(src, grib, &dim, &latn, &lonn, lats, lons)?,
            Layout::Reduced(rows) => reduced.push((dim, rows)),
            Layout::Points => {}
        }
    }
    pressure_levels(src, grib)?;
    Ok(reduced)
}

/// Gives the unstructured grids over the dimensions of `reduced` their rows (after
/// classification).
pub(super) fn set_reduced(src: &mut ZarrSource, reduced: Vec<(String, ReducedRows)>) {
    for (dim, rows) in reduced {
        for g in &mut src.ds.grids {
            if g.kind == crate::model::GridKind::Unstructured
                && g.dims == [dim.as_str()]
                && rows.counts.iter().sum::<usize>() == g.size
            {
                g.reduced = Some(rows.clone());
            }
        }
    }
}

fn is_lat(v: &crate::model::Variable) -> bool {
    v.attrs.get_str("standard_name") == Some("latitude")
        || v.attrs.get_str("units") == Some("degrees_north")
        || matches!(v.name.as_str(), "lat" | "latitude")
}

fn is_lon(v: &crate::model::Variable) -> bool {
    v.attrs.get_str("standard_name") == Some("longitude")
        || v.attrs.get_str("units") == Some("degrees_east")
        || matches!(v.name.as_str(), "lon" | "longitude")
}

/// A regular grid: dimension `dim` of the GRIB arrays becomes `latn, lonn`, and the coordinates
/// `latn(dim)`, `lonn(dim)` become `latn(latn)`, `lonn(lonn)` with the row latitudes and the
/// column longitudes. Left as it is when an array that is not a GRIB array uses `dim`, when a
/// GRIB array has `dim` elsewhere than last or split into several chunks, or when the names are
/// taken.
fn reshape(
    src: &mut ZarrSource,
    grib: &[String],
    dim: &str,
    latn: &str,
    lonn: &str,
    lats: Vec<f64>,
    lons: Vec<f64>,
) -> Result<()> {
    let (ny, nx) = (lats.len(), lons.len());
    let n = ny * nx;
    let fits = src.ds.vars.iter().all(|v| {
        let uses = v.dims.iter().any(|d| d.name == dim);
        if !uses || v.name == latn || v.name == lonn {
            return true;
        }
        grib.contains(&v.name)
            && v.dims.iter().position(|d| d.name == dim) == Some(v.dims.len() - 1)
            && v.chunks.last() == Some(&n)
    }) && !src.ds.dims.iter().any(|(d, _)| d == latn || d == lonn);
    if !fits {
        return Ok(());
    }
    for v in &mut src.ds.vars {
        if v.name == latn || v.name == lonn {
            let size = if v.name == latn { ny } else { nx };
            v.dims = vec![VarDim {
                name: v.name.clone(),
                size,
                role: v.dims[0].role,
            }];
            v.chunks = vec![size];
            continue;
        }
        if !v.dims.last().is_some_and(|d| d.name == dim) {
            continue;
        }
        let last = v.dims.len() - 1;
        let role = v.dims[last].role;
        v.dims.splice(
            last..,
            [(latn, ny), (lonn, nx)].map(|(name, size)| VarDim {
                name: name.to_owned(),
                size,
                role,
            }),
        );
        v.chunks.splice(last.., [ny, nx]);
        // `coordinates = "lon lat"` would make the grid curvilinear-like again
        let kept: Vec<&str> = v
            .attrs
            .get_str("coordinates")
            .unwrap_or("")
            .split_whitespace()
            .filter(|c| *c != latn && *c != lonn)
            .collect();
        let kept = kept.join(" ");
        v.attrs.0.retain(|(k, _)| k != "coordinates");
        if !kept.is_empty() {
            v.attrs
                .0
                .push(("coordinates".to_owned(), AttrValue::Text(kept)));
        }
        let zv = src
            .vars
            .get_mut(&v.name)
            .and_then(Arc::get_mut)
            .ok_or_else(|| Error::internal(format!("array '{}' is shared", v.name)))?;
        zv.grid.shape.splice(last.., [ny, nx]);
        zv.grid.chunk_shape.splice(last.., [ny, nx]);
        zv.split = true;
    }
    let pos = src
        .ds
        .dims
        .iter()
        .position(|(d, _)| d == dim)
        .unwrap_or(src.ds.dims.len());
    src.ds.dims.retain(|(d, _)| d != dim);
    let at = pos.min(src.ds.dims.len());
    src.ds
        .dims
        .splice(at..at, [(latn.to_owned(), ny), (lonn.to_owned(), nx)]);
    src.synthetic.insert(latn.to_owned(), lats);
    src.synthetic.insert(lonn.to_owned(), lons);
    Ok(())
}

/// Levels of `isobaricInhPa` fields in Pa: a dimension coordinate without units over which only
/// such GRIB arrays lie.
fn pressure_levels(src: &mut ZarrSource, grib: &[String]) -> Result<()> {
    let mut levels: HashMap<String, bool> = HashMap::new();
    for v in &src.ds.vars {
        for d in v.dims.iter().take(v.dims.len().saturating_sub(1)) {
            let Some(c) = src
                .ds
                .vars
                .iter()
                .find(|c| c.name == d.name && c.dims.len() == 1)
            else {
                continue;
            };
            if c.attrs.get("units").is_some() || c.name == v.name {
                continue;
            }
            let isobaric =
                grib.contains(&v.name) && v.attrs.get_str("typeOfLevel") == Some("isobaricInhPa");
            *levels.entry(d.name.clone()).or_insert(true) &= isobaric;
        }
    }
    for (name, isobaric) in levels {
        if !isobaric || src.synthetic.contains_key(&name) {
            continue;
        }
        let pa: Vec<f64> = src.read_var(&name)?.iter().map(|x| x * 100.0).collect();
        if let Some(c) = src.ds.vars.iter_mut().find(|c| c.name == name) {
            for (k, v) in [
                ("standard_name", "air_pressure"),
                ("long_name", "pressure"),
                ("units", "Pa"),
                ("positive", "down"),
                ("axis", "Z"),
            ] {
                if c.attrs.get(k).is_none() {
                    c.attrs
                        .0
                        .push((k.to_owned(), AttrValue::Text(v.to_owned())));
                }
            }
        }
        src.synthetic.insert(name, pa);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn product(lats: &[f64], lons: &[f64]) -> (Vec<f64>, Vec<f64>) {
        let lat = lats
            .iter()
            .flat_map(|&y| lons.iter().map(move |_| y))
            .collect();
        let lon = lats.iter().flat_map(|_| lons.iter().copied()).collect();
        (lat, lon)
    }

    fn lat8() -> [f64; 8] {
        [60.0, 60.0, 0.0, 0.0, 0.0, 0.0, -60.0, -60.0]
    }

    #[test]
    fn layouts() {
        let (lat, lon) = product(&[90.0, 0.0, -90.0], &[0.0, 90.0, 180.0, 270.0]);
        assert_eq!(
            layout(&lat, &lon),
            Layout::Regular {
                lats: vec![90.0, 0.0, -90.0],
                lons: vec![0.0, 90.0, 180.0, 270.0]
            }
        );
        // rows of 2, 4, 2 equally spaced longitudes: reduced
        let lat = lat8();
        let lon = [0.0, 180.0, 0.0, 90.0, 180.0, 270.0, 45.0, 225.0];
        assert_eq!(
            layout(&lat, &lon),
            Layout::Reduced(ReducedRows {
                lats: vec![60.0, 0.0, -60.0],
                counts: vec![2, 4, 2]
            })
        );
        // longitudes from -180 in the second half of each row (gribscan): continuous, as in GRIB
        let (lat, lon) = product(&[45.0, -45.0], &[0.0, 90.0, -180.0, -90.0]);
        assert_eq!(
            layout(&lat, &lon),
            Layout::Regular {
                lats: vec![45.0, -45.0],
                lons: vec![0.0, 90.0, 180.0, 270.0]
            }
        );
        let lon_wrapped = [0.0, -180.0, 0.0, 90.0, -180.0, -90.0, 45.0, -135.0];
        assert!(matches!(layout(&lat8(), &lon_wrapped), Layout::Reduced(_)));
        // a row that does not go round the globe, a latitude that comes back, column order
        let lon_short = [0.0, 90.0, 0.0, 90.0, 180.0, 270.0, 45.0, 225.0];
        assert_eq!(layout(&lat, &lon_short), Layout::Points);
        let (lat, lon) = product(&[0.0, 10.0, 0.0], &[0.0, 10.0]);
        assert_eq!(layout(&lat, &lon), Layout::Points);
        let (lon, lat) = product(&[0.0, 10.0, 20.0], &[50.0, 60.0]);
        assert_eq!(layout(&lat, &lon), Layout::Points);
    }
}
