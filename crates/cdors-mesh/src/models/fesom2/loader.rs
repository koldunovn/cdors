//! The FESOM2 loader: `fesom.mesh.diag.nc` → [`Mesh`].
//!
//! The file comes in two layouts, and both are read:
//! - older meshes (pi, core2, dart, farc): dimensions `nod_n`, `elem_n`, `nl`, `nl1`; variables
//!   `zbar`, `Z`, `elem(n3, elem_n)`;
//! - newer meshes (ng5, D3): dimensions `nod2`, `elem`, `nz`, `nz1`; variables `nz`, `nz1`,
//!   `elements(n3, elem)` or `face_nodes(n3, elem)` (with `start_index`), plus `lon`/`lat`.
//!
//! **Units.** `nodes(n2, nod)` holds longitudes and latitudes without a `units` attribute, in
//! radians in some meshes (core2, dart, pi) and in degrees in others (ng5). They are taken as
//! radians if every |lat| ≤ π/2 + ε and every |lon| ≤ 2π + ε, else as degrees; the spec option
//! `units=deg|rad` overrides the test. The decision is recorded in [`Source::units`].
//!
//! **Areas.** `nod_area(nl, nod)` is the area of each node's control volume at each interface;
//! layer k (0-based) uses the area of its top interface, `nod_area[k]`, so the per-level areas of
//! the `nl - 1` layers are its first `nl - 1` rows, read lazily one row at a time. The 2-D node
//! area is the surface row. Elements have one area for all levels (`elem_area`).
//!
//! **Levels.** `nlevels_nod2D` / `nlevels` count the interfaces down to the bottom of each
//! node / element; `ulevels_nod2D` / `ulevels` (newer meshes with cavities) give the first wet
//! layer, 1 without them. Full layers only.
//!
//! **Element centres** are the normalised mean of the unit vectors of the three nodes.

use super::rules;
use crate::models::MeshSpec;
use crate::{
    ArrayReader, Csr, Lazy, Levels, Mesh, MeshError, Place, Result, Source, Table, Thickness,
    Vertical, WetLevels, geom,
};
use std::f64::consts::PI;
use std::ops::Range;
use std::sync::Arc;

/// Tolerance of the radians test.
const EPS: f64 = 1e-6;

const NODE_DIMS: &[&str] = &["nod2", "nod_n"];
const ELEM_DIMS: &[&str] = &["elem", "elem_n"];
const ELEM_VARS: &[&str] = &["elem", "elements", "face_nodes"];
const INTERFACE_VARS: &[&str] = &["zbar", "nz"];
const MID_VARS: &[&str] = &["Z", "nz1"];

/// Loads a FESOM2 mesh from a reader over `fesom.mesh.diag.nc`.
pub fn load(readers: &[Arc<dyn ArrayReader>], spec: &MeshSpec) -> Result<Mesh> {
    let r = readers
        .first()
        .ok_or_else(|| MeshError::Missing("no fesom.mesh.diag.nc reader".into()))?
        .clone();
    let units = match spec.option("units") {
        None => None,
        Some("deg" | "degrees") => Some(false),
        Some("rad" | "radians") => Some(true),
        Some(u) => {
            return Err(MeshError::Invalid(format!(
                "units={u}: expected units=deg or units=rad"
            )));
        }
    };
    let file = r.describe();

    // nodes(n2, nod): longitudes, latitudes
    let ninfo = r.require("nodes")?;
    let (nod_dim, nn) = one_of(&ninfo.dims, &ninfo.shape, NODE_DIMS)
        .ok_or_else(|| invalid(&file, "nodes", "has no nod2/nod_n dimension"))?;
    let xy = r.read("nodes")?;
    let (mut lon, mut lat) = split_rows(&xy, &ninfo.shape, &ninfo.dims, &nod_dim, 2)
        .ok_or_else(|| invalid(&file, "nodes", "is not (2, nodes)"))?;
    let max_abs = |v: &[f64]| {
        v.iter()
            .filter(|x| x.is_finite())
            .fold(0f64, |m, x| m.max(x.abs()))
    };
    let (mlat, mlon) = (max_abs(&lat), max_abs(&lon));
    let auto = mlat <= PI / 2.0 + EPS && mlon <= 2.0 * PI + EPS;
    let radians = units.unwrap_or(auto);
    let decided = match units {
        Some(_) => format!(
            "{} (given: units={})",
            if radians { "radians" } else { "degrees" },
            spec.option("units").unwrap_or("")
        ),
        None if radians => format!(
            "radians: max |lat| {mlat:.6} <= pi/2 and max |lon| {mlon:.6} <= 2 pi \
             (nodes has no units attribute; override with units=deg)"
        ),
        None => format!(
            "degrees: max |lat| {mlat:.6} > pi/2 or max |lon| {mlon:.6} > 2 pi \
             (nodes has no units attribute; override with units=rad)"
        ),
    };
    if radians {
        lon.iter_mut().for_each(|x| *x = x.to_degrees());
        lat.iter_mut().for_each(|x| *x = x.to_degrees());
    }

    // vertical: interfaces, mid-levels
    let interfaces = r.read(first_var(&*r, INTERFACE_VARS).ok_or_else(|| {
        MeshError::Missing(format!("{file}: no level interfaces (zbar or nz)"))
    })?)?;
    let mid = r.read(
        first_var(&*r, MID_VARS)
            .ok_or_else(|| MeshError::Missing(format!("{file}: no mid-levels (Z or nz1)")))?,
    )?;
    let nlev = mid.len();
    if interfaces.len() != nlev + 1 {
        return Err(MeshError::Invalid(format!(
            "{file}: {} interfaces for {nlev} layers",
            interfaces.len()
        )));
    }

    // node areas: per interface; the layers use their top interface
    let ainfo = r.require("nod_area")?;
    if ainfo.dims.len() != 2 || ainfo.dims[1] != nod_dim || ainfo.shape[0] != nlev + 1 {
        return Err(invalid(
            &file,
            "nod_area",
            &format!(
                "is {:?} {:?}, expected (levels, {nod_dim})",
                ainfo.dims, ainfo.shape
            ),
        ));
    }
    let node_area = r.read_range("nod_area", &[0..1, 0..nn])?;
    let ra = r.clone();
    let per_level = Levels::deferred(nlev, nn, move |k| {
        ra.read_range("nod_area", &[k..k + 1, 0..nn])
    });

    // elements: connectivity, areas, centres
    let evar = first_var(&*r, ELEM_VARS).ok_or_else(|| {
        MeshError::Missing(format!(
            "{file}: no element table (elem, elements or face_nodes)"
        ))
    })?;
    let einfo = r.require(evar)?;
    let (elem_dim, ne) = one_of(&einfo.dims, &einfo.shape, ELEM_DIMS)
        .ok_or_else(|| invalid(&file, evar, "has no elem/elem_n dimension"))?;
    let start = r
        .attr(Some(evar), "start_index")
        .and_then(|a| a.as_f64())
        .unwrap_or(1.0);
    let elem_table = {
        let (r, evar, elem_dim, file) =
            (r.clone(), evar.to_owned(), elem_dim.clone(), file.clone());
        move || read_elements(&*r, &evar, &elem_dim, start, nn, &file)
    };
    let en = elem_table()?;
    let mut elon = Vec::with_capacity(ne);
    let mut elat = Vec::with_capacity(ne);
    for e in 0..ne {
        let mut v = [0.0; 3];
        for &n in en.row(e) {
            let u = geom::unit_vector(lon[n as usize], lat[n as usize]);
            (0..3).for_each(|i| v[i] += u[i]);
        }
        let (x, y) = geom::to_lonlat(v);
        elon.push(x);
        elat.push(y);
    }
    let table_bytes = en.bytes();
    drop(en);
    let elem_area = read_1d(&*r, "elem_area", &elem_dim, ne)?;

    // wet levels
    let wet =
        |place: &str, n_var: &str, u_var: &str, dim: &str, size: usize| -> Result<WetLevels> {
            let n = read_1d(&*r, n_var, dim, size)?;
            let u = match r.var(u_var) {
                Some(_) => Some(read_1d(&*r, u_var, dim, size)?),
                None => None,
            };
            let (top, bottom) = rules::wet_ranges(&n, u.as_deref(), nlev, place)?;
            Ok(WetLevels {
                place: place.to_owned(),
                top,
                bottom,
            })
        };
    let wet_nodes = wet("nodes", "nlevels_nod2D", "ulevels_nod2D", &nod_dim, nn)?;
    let wet_elems = wet("elements", "nlevels", "ulevels", &elem_dim, ne)?;

    let mut nodes = Place::new("nodes", lon, lat, node_area)?;
    nodes.area_per_level = Some(per_level);
    let elements = Place::new("elements", elon, elat, elem_area)?;
    let name = spec
        .dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| spec.dir.display().to_string());
    Ok(Mesh {
        name: format!("fesom2:{name}"),
        places: vec![nodes, elements],
        topology: vec![Table {
            name: "elem_nodes".into(),
            rows_place: "elements".into(),
            cols_place: "nodes".into(),
            csr: Lazy::deferred(table_bytes, elem_table),
        }],
        vertical: Some(Vertical {
            interfaces,
            mid,
            thickness: Thickness::Static,
            wet: vec![wet_nodes, wet_elems],
        }),
        source: Source {
            model: "fesom2".into(),
            files: spec.files(),
            units: Some(decided),
            notes: vec![format!(
                "dimensions: {nod_dim} (nodes), {elem_dim} (elements); element table '{evar}' \
                 (start index {start}); layer areas from the top interface of nod_area"
            )],
        },
    })
}

fn invalid(file: &str, var: &str, what: &str) -> MeshError {
    MeshError::Invalid(format!("{file}: variable '{var}' {what}"))
}

/// The first of `names` that is a variable of `r`.
fn first_var(r: &dyn ArrayReader, names: &[&'static str]) -> Option<&'static str> {
    names
        .iter()
        .copied()
        .find(|n| r.var(n).is_some_and(|v| v.dims.len() <= 2))
}

/// The first dimension of `dims` whose name is one of `names`: (name, size).
fn one_of(dims: &[String], shape: &[usize], names: &[&str]) -> Option<(String, usize)> {
    dims.iter()
        .zip(shape)
        .find(|(d, _)| names.contains(&d.as_str()))
        .map(|(d, &s)| (d.clone(), s))
}

/// A 1-D variable over `dim` of `size` values.
fn read_1d(r: &dyn ArrayReader, var: &str, dim: &str, size: usize) -> Result<Vec<f64>> {
    let info = r.require(var)?;
    if info.dims != [dim] || info.shape != [size] {
        return Err(invalid(
            &r.describe(),
            var,
            &format!(
                "is {:?} {:?}, expected ({dim}) of {size}",
                info.dims, info.shape
            ),
        ));
    }
    r.read(var)
}

/// The `width` rows of a `(width, n)` or `(n, width)` variable (`dim` is the `n` dimension).
fn split_rows(
    v: &[f64],
    shape: &[usize],
    dims: &[String],
    dim: &str,
    width: usize,
) -> Option<(Vec<f64>, Vec<f64>)> {
    let rows = rows_of(v, shape, dims, dim, width)?;
    let mut it = rows.into_iter();
    Some((it.next()?, it.next()?))
}

fn rows_of(
    v: &[f64],
    shape: &[usize],
    dims: &[String],
    dim: &str,
    width: usize,
) -> Option<Vec<Vec<f64>>> {
    if shape.len() != 2 {
        return None;
    }
    let row_range = |k: usize, n: usize| -> Range<usize> { k * n..(k + 1) * n };
    if dims[1] == dim && shape[0] == width {
        let n = shape[1];
        Some((0..width).map(|k| v[row_range(k, n)].to_vec()).collect())
    } else if dims[0] == dim && shape[1] == width {
        Some(
            (0..width)
                .map(|k| v.iter().skip(k).step_by(width).copied().collect())
                .collect(),
        )
    } else {
        None
    }
}

/// The element → nodes table (0-based node indices).
fn read_elements(
    r: &dyn ArrayReader,
    var: &str,
    elem_dim: &str,
    start: f64,
    nn: usize,
    file: &str,
) -> Result<Csr> {
    let info = r.require(var)?;
    let (d, s) = (&info.dims, &info.shape);
    // strides of element e and corner k in the stored values
    let (ne, se, sk) = match *s.as_slice() {
        [3, n] if d[1] == elem_dim => (n, 1, n),
        [n, 3] if d[0] == elem_dim => (n, 3, 1),
        _ => return Err(invalid(file, var, "is not (3, elements) or (elements, 3)")),
    };
    let v = r.read(var)?;
    let mut col = Vec::with_capacity(3 * ne);
    for e in 0..ne {
        for k in 0..3 {
            let x = v[e * se + k * sk];
            let i = x - start;
            if !(i >= 0.0 && i < nn as f64) {
                return Err(invalid(
                    file,
                    var,
                    &format!("element {}: node {x} out of range", e + 1),
                ));
            }
            col.push(i as u32);
        }
    }
    Csr::fixed(3, col, nn)
}
