//! Information operators: `showname`, `showtimestamp`, `griddes`, `sinfo`.
//!
//! `showname`, `showtimestamp` and `griddes` reproduce cdo 2.6.0's stdout byte for byte
//! (`src/operators/Showinfo.cc`, `src/grid_print.cc`). HEALPix grids are printed as cdo 2.6.0
//! prints them (`gridtype = projection` with the grid-mapping attributes). `sinfo` prints a
//! cdo-like summary as text, and with `--json` a stable machine-readable description.

use crate::chain::Options;
use crate::error::Result;
use crate::io::ChunkSource;
use crate::model::{AttrValue, Dataset, DimRole, Grid, GridKind, VarKind};
use serde_json::{Value, json};
use std::fmt::Write;

/// C's `%.*g`.
pub fn fmt_g(v: f64, prec: usize) -> String {
    fmt_g_impl(v, prec, false)
}

fn fmt_g_impl(v: f64, prec: usize, alt: bool) -> String {
    if v.is_nan() {
        return if v.is_sign_negative() { "-nan" } else { "nan" }.into();
    }
    if v.is_infinite() {
        return if v < 0.0 { "-inf" } else { "inf" }.into();
    }
    let p = prec.max(1);
    let e = format!("{:.*e}", p - 1, v);
    let (mant, exp) = e.split_once('e').expect("exponent");
    let x: i32 = exp.parse().expect("exponent value");
    let mut s = if (x as i64) < p as i64 && x >= -4 {
        format!("{:.*}", (p as i64 - 1 - x as i64) as usize, v)
    } else {
        let sign = if x < 0 { '-' } else { '+' };
        format!("{mant}e{sign}{:02}", x.abs())
    };
    if alt {
        if !s.contains('.') {
            match s.find('e') {
                Some(i) => s.insert(i, '.'),
                None => s.push('.'),
            }
        }
        return s;
    }
    // strip trailing zeros of the fraction (no '#' flag)
    let (num, tail) = match s.find('e') {
        Some(i) => (s[..i].to_owned(), s[i..].to_owned()),
        None => (s.clone(), String::new()),
    };
    if num.contains('.') {
        let t = num.trim_end_matches('0').trim_end_matches('.');
        format!("{t}{tail}")
    } else {
        s
    }
}

/// cdo's `double_to_att_str`: `%#.*g` followed by `trim_flt` (keeps a trailing '.').
fn att_flt(v: f64, digits: usize) -> String {
    let s = fmt_g_impl(v, digits, true);
    let b = s.as_bytes();
    let mut i = usize::from(b.first() == Some(&b'-'));
    while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
        i += 1;
    }
    if i == 0 || b[i - 1] == b'.' {
        return s;
    }
    let mut j = i;
    while j > 0 && b[j - 1] == b'0' {
        j -= 1;
    }
    if j == i {
        return s;
    }
    format!("{}{}", &s[..j], &s[i..])
}

/// `showname`: variable names separated by blanks.
pub fn showname(ds: &Dataset) -> String {
    let names: Vec<&str> = ds.data_vars().map(|v| v.name.as_str()).collect();
    format!("{}\n", names.join(" "))
}

/// `showtimestamp`: ` YYYY-MM-DDThh:mm:ss` per step; without `-s` a line break after every
/// fourth, as in cdo.
pub fn showtimestamp(ds: &Dataset, silent: bool) -> String {
    let mut out = String::new();
    let Some(t) = &ds.time else {
        return out;
    };
    if t.steps.is_empty() || !ds.data_vars().any(|v| v.has_time()) {
        return out;
    }
    const MAX_OUT: usize = 4;
    let mut nout = 0;
    for s in &t.steps {
        nout += 1;
        out.push(' ');
        out.push_str(&s.datetime.cdo_string());
        if !silent && nout % MAX_OUT == 0 {
            out.push('\n');
        }
    }
    if silent || nout % MAX_OUT != 0 {
        out.push('\n');
    }
    out
}

const MAX_LEN: usize = 120;

fn print_dbls(out: &mut String, dig: usize, prefix: &str, vals: &[f64]) {
    let n0 = prefix.len();
    out.push_str(prefix);
    let mut nbyte = n0;
    for &v in vals {
        if nbyte > MAX_LEN {
            out.push('\n');
            out.push_str(&" ".repeat(n0));
            nbyte = n0;
        }
        let s = fmt_g(v, dig);
        out.push_str(&s);
        out.push(' ');
        nbyte += s.len() + 1;
    }
    out.push('\n');
}

fn print_bounds(out: &mut String, dig: usize, prefix: &str, n: usize, nv: usize, b: &[f64]) {
    out.push_str(prefix);
    for i in 0..n {
        if i > 0 {
            out.push('\n');
            out.push_str(&" ".repeat(prefix.len()));
        }
        for v in &b[i * nv..(i + 1) * nv] {
            out.push_str(&fmt_g(*v, dig));
            out.push(' ');
        }
    }
    out.push('\n');
}

fn print_axis(out: &mut String, c: &str, ax: &crate::model::CoordAxis) {
    let _ = writeln!(out, "{c}name     = {}", ax.var);
    if ax.dim != ax.var {
        let _ = writeln!(out, "{c}dimname  = {}", ax.dim);
    }
    if let Some(l) = ax.long_name.as_deref().filter(|s| !s.is_empty()) {
        let _ = writeln!(out, "{c}longname = \"{l}\"");
    }
    if let Some(u) = ax.units.as_deref().filter(|s| !s.is_empty()) {
        let _ = writeln!(out, "{c}units    = \"{u}\"");
    }
}

fn print_attrs(out: &mut String, attrs: &crate::model::Attrs) {
    for (k, v) in attrs.iter() {
        if k == "grid_mapping_name" {
            continue;
        }
        match v {
            AttrValue::Text(s) if s.is_empty() => {}
            AttrValue::Text(s) => {
                if s.contains('"') {
                    let _ = writeln!(out, "{k} = '{s}'");
                } else {
                    let _ = writeln!(out, "{k} = \"{s}\"");
                }
            }
            AttrValue::Ints(x) => {
                let _ = write!(out, "{k} =");
                for i in x {
                    let _ = write!(out, " {i}");
                }
                out.push('\n');
            }
            AttrValue::F32s(x) | AttrValue::F64s(x) => {
                let dig = if matches!(v, AttrValue::F32s(_)) {
                    7
                } else {
                    15
                };
                let _ = write!(out, "{k} =");
                for f in x {
                    let _ = write!(out, " {}", att_flt(*f, dig));
                }
                out.push('\n');
            }
        }
    }
}

/// Bounds values of a grid axis, if its bounds variable exists.
fn read_bounds(
    src: &dyn ChunkSource,
    ax: Option<&crate::model::CoordAxis>,
) -> Result<Option<Vec<f64>>> {
    match ax.and_then(|a| a.bounds_var.as_deref()) {
        Some(b) if src.dataset().var(b).is_some() => Ok(Some(src.read_var(b)?)),
        _ => Ok(None),
    }
}

fn griddes_one(out: &mut String, src: &dyn ChunkSource, g: &Grid) -> Result<()> {
    let is_f32 = g.x.as_ref().is_some_and(|x| x.is_f32);
    let dig = if is_f32 { 7 } else { 15 };
    let gridtype = match g.kind {
        GridKind::Regular => "lonlat",
        GridKind::Gaussian => "gaussian",
        GridKind::Curvilinear => "curvilinear",
        GridKind::Unstructured => "unstructured",
        // cdo 2.6.0 reads HEALPix from netCDF as a projection with a healpix grid mapping
        GridKind::Healpix => "projection",
        GridKind::Generic => "generic",
    };
    let _ = writeln!(out, "gridtype  = {gridtype}");
    let _ = writeln!(out, "gridsize  = {}", g.size);
    if is_f32 {
        out.push_str("datatype  = float\n");
    }
    if g.kind != GridKind::Unstructured {
        if g.xsize > 0 {
            let _ = writeln!(out, "xsize     = {}", g.xsize);
        }
        if g.ysize > 0 {
            let _ = writeln!(out, "ysize     = {}", g.ysize);
        }
    }
    match &g.x {
        Some(x) => print_axis(out, "x", x),
        None => {
            if let Some(d) = g.dims.last() {
                let _ = writeln!(out, "xdimname  = {d}");
            }
        }
    }
    match &g.y {
        Some(y) => print_axis(out, "y", y),
        None => {
            if g.ysize > 0 && g.dims.len() == 2 {
                let _ = writeln!(out, "ydimname  = {}", g.dims[0]);
            }
        }
    }
    if matches!(g.kind, GridKind::Unstructured | GridKind::Curvilinear)
        && let Some(v) = &g.vdim
    {
        let _ = writeln!(out, "vdimname  = {v}");
    }
    if g.kind == GridKind::Unstructured
        && let Some(n) = g.nvertex
    {
        let _ = writeln!(out, "nvertex   = {n}");
    }
    if g.kind == GridKind::Unstructured
        && let Some(r) = &g.reference
    {
        if let Some(n) = r.number.filter(|&n| n > 0) {
            let _ = writeln!(out, "number    = {n}");
            if r.position >= 0 {
                let _ = writeln!(out, "position  = {}", r.position);
            }
        }
        if let Some(u) = &r.uri {
            let _ = writeln!(out, "uri       = {u}");
        }
    }
    match g.kind {
        GridKind::Regular | GridKind::Gaussian => {
            if g.kind == GridKind::Gaussian {
                let _ = writeln!(out, "numLPE    = {}", g.gaussian_np.unwrap_or(g.ysize / 2));
            }
            let xv = g.xvals.as_deref().unwrap_or_default();
            let yv = g.yvals.as_deref().unwrap_or_default();
            let xinc = crate::model::grid::calc_increment(xv);
            if xinc != 0.0 {
                let _ = writeln!(out, "xfirst    = {}", fmt_g(xv[0], dig));
                let _ = writeln!(out, "xinc      = {}", fmt_g(xinc, dig));
            } else if !xv.is_empty() {
                print_dbls(out, dig, "xvals     = ", xv);
            }
            if let Some(b) = read_bounds(src, g.x.as_ref())?
                && b.len() == 2 * xv.len()
            {
                print_bounds(out, dig, "xbounds   = ", xv.len(), 2, &b);
            }
            let yinc = if g.kind == GridKind::Regular {
                crate::model::grid::calc_increment(yv)
            } else {
                0.0
            };
            if yinc != 0.0 {
                let _ = writeln!(out, "yfirst    = {}", fmt_g(yv[0], dig));
                let _ = writeln!(out, "yinc      = {}", fmt_g(yinc, dig));
            } else if !yv.is_empty() {
                print_dbls(out, dig, "yvals     = ", yv);
            }
            if let Some(b) = read_bounds(src, g.y.as_ref())?
                && b.len() == 2 * yv.len()
            {
                print_bounds(out, dig, "ybounds   = ", yv.len(), 2, &b);
            }
        }
        GridKind::Curvilinear | GridKind::Unstructured => {
            let nv = g.nvertex.unwrap_or(0);
            for (c, ax) in [("x", &g.x), ("y", &g.y)] {
                let Some(ax) = ax else { continue };
                let vals = src.read_var(&ax.var)?;
                print_dbls(out, dig, &format!("{c}vals     = "), &vals);
                if nv > 0
                    && let Some(b) = read_bounds(src, Some(ax))?
                    && b.len() == nv * g.size
                {
                    print_bounds(out, dig, &format!("{c}bounds   = "), g.size, nv, &b);
                }
            }
        }
        GridKind::Healpix => {
            if let Some(m) = &g.mapping {
                let _ = writeln!(out, "grid_mapping = {}", m.var);
                let _ = writeln!(out, "grid_mapping_name = {}", m.name);
                print_attrs(out, &m.attrs);
            }
        }
        GridKind::Generic => {}
    }
    if let Some(u) = g.reference.as_ref().and_then(|r| r.uuid.as_ref()) {
        let _ = writeln!(out, "uuid      = {u}");
    }
    Ok(())
}

/// `griddes`: every grid in cdo's grid description format.
pub fn griddes(src: &dyn ChunkSource) -> Result<String> {
    let mut out = String::new();
    for (i, g) in src.dataset().grids.iter().enumerate() {
        let _ = write!(out, "#\n# gridID {}\n#\n", i + 1);
        griddes_one(&mut out, src, g)?;
    }
    Ok(out)
}

fn grid_json(i: usize, g: &Grid) -> Value {
    let mut v = json!({
        "id": i,
        "kind": g.kind.name(),
        "size": g.size,
        "dims": g.dims,
    });
    let m = v.as_object_mut().expect("object");
    if matches!(
        g.kind,
        GridKind::Regular | GridKind::Gaussian | GridKind::Curvilinear | GridKind::Generic
    ) {
        m.insert("xsize".into(), json!(g.xsize));
        m.insert("ysize".into(), json!(g.ysize));
    }
    if let Some(x) = &g.x {
        m.insert("lon".into(), json!(x.var));
        m.insert("lon_units".into(), json!(x.units));
    }
    if let Some(y) = &g.y {
        m.insert("lat".into(), json!(y.var));
        m.insert("lat_units".into(), json!(y.units));
    }
    if g.radians {
        m.insert("radians".into(), json!(true));
    }
    if let Some(n) = g.nvertex {
        m.insert("nvertex".into(), json!(n));
    }
    if let (Some(x), Some(y)) = (&g.xvals, &g.yvals)
        && !x.is_empty()
        && !y.is_empty()
    {
        m.insert("lon_range".into(), json!([x[0], x[x.len() - 1]]));
        m.insert("lat_range".into(), json!([y[0], y[y.len() - 1]]));
    }
    if let Some(np) = g.gaussian_np {
        m.insert("gaussian_np".into(), json!(np));
    }
    if let Some(h) = &g.healpix {
        m.insert("nside".into(), json!(h.nside));
        m.insert("order".into(), json!(h.order));
        if let Some(iv) = &h.index_var {
            m.insert("index_var".into(), json!(iv));
        }
    }
    if let Some(gm) = &g.mapping {
        m.insert("grid_mapping".into(), json!(gm.var));
    }
    v
}

/// `sinfo --json`: a stable machine-readable description of the dataset.
pub fn sinfo_json(src: &dyn ChunkSource) -> Result<Value> {
    let ds = src.dataset();
    let vars: Vec<Value> = ds
        .data_vars()
        .map(|v| {
            let mut o = json!({
                "name": v.name,
                "dims": v.dims.iter().map(|d| json!({"name": d.name, "size": d.size, "role": d.role})).collect::<Vec<_>>(),
                "shape": v.shape(),
                "dtype": v.dtype.name(),
                "units": v.units(),
                "long_name": v.long_name(),
                "standard_name": v.attrs.get_str("standard_name"),
                "grid": v.grid,
                "zaxis": v.zaxis,
                "chunks": v.chunks,
            });
            let m = o.as_object_mut().expect("object");
            if !v.encoding.missing.is_empty() {
                m.insert("missing_values".into(), json!(v.encoding.missing));
            }
            if v.encoding.is_packed() {
                m.insert("scale_factor".into(), json!(v.encoding.scale_factor));
                m.insert("add_offset".into(), json!(v.encoding.add_offset));
            }
            if let Ok(g) = src.chunk_grid(&v.name) {
                m.insert("nchunks".into(), json!(g.num_chunks()));
            }
            if let Some(c) = src.codecs(&v.name) {
                m.insert("codecs".into(), json!(c));
            }
            if v.dims.iter().any(|d| d.role == DimRole::Other) {
                m.insert(
                    "other_dims".into(),
                    json!(v.dims.iter().filter(|d| d.role == DimRole::Other).map(|d| &d.name).collect::<Vec<_>>()),
                );
            }
            o
        })
        .collect();
    let zaxes: Vec<Value> = ds
        .zaxes
        .iter()
        .enumerate()
        .map(|(i, z)| {
            json!({
                "id": i,
                "name": z.var,
                "dim": z.dim,
                "size": z.len(),
                "units": z.units,
                "positive": z.positive,
                "levels": z.values,
                "has_bounds": z.bounds.is_some(),
            })
        })
        .collect();
    let time = ds.time.as_ref().map(|t| {
        json!({
            "var": t.var,
            "dim": t.dim,
            "units": t.units_attr,
            "calendar": t.calendar.cf_name(),
            "count": t.len(),
            "first": t.steps.first().map(|s| s.datetime.iso()),
            "last": t.steps.last().map(|s| s.datetime.iso()),
            "has_bounds": t.bounds.is_some(),
        })
    });
    let coords: Vec<&str> = ds
        .vars
        .iter()
        .filter(|v| v.kind != VarKind::Data)
        .map(|v| v.name.as_str())
        .collect();
    Ok(json!({
        "source": ds.source,
        "format": ds.format.name(),
        "variables": vars,
        "grids": ds.grids.iter().enumerate().map(|(i, g)| grid_json(i, g)).collect::<Vec<_>>(),
        "zaxes": zaxes,
        "time": time,
        "coordinates": coords,
        "attributes": ds.attrs.to_json(),
    }))
}

/// `sinfo` as text: a cdo-like summary (not byte-compatible with cdo).
pub fn sinfo_text(src: &dyn ChunkSource) -> String {
    let ds = src.dataset();
    let mut out = String::new();
    let fmt = match ds.format {
        crate::model::Format::NetCdf => "NetCDF",
        crate::model::Format::Zarr2 => "Zarr v2",
        crate::model::Format::Zarr3 => "Zarr v3",
    };
    let _ = writeln!(out, "   File format : {fmt}");
    let _ = writeln!(
        out,
        "    -1 : T Levels    Points Dtype   Chunks               : Parameter name"
    );
    for (i, v) in ds.data_vars().enumerate() {
        let levels = v.zaxis.map_or(1, |z| ds.zaxes[z].len());
        let points = v.grid.map_or(1, |g| ds.grids[g].size);
        let chunks = v
            .chunks
            .iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join("x");
        let _ = writeln!(
            out,
            "{:6} : {} {:6} {:9} {:7} {:20} : {}",
            i + 1,
            if v.has_time() { 'v' } else { 'c' },
            levels,
            points,
            v.dtype.name(),
            chunks,
            v.name
        );
    }
    let _ = writeln!(out, "   Grid coordinates :");
    for (i, g) in ds.grids.iter().enumerate() {
        let desc = match g.kind {
            GridKind::Healpix => {
                let h = g.healpix.as_ref().expect("healpix");
                format!("points={} nside={} order={:?}", g.size, h.nside, h.order)
            }
            GridKind::Unstructured => {
                format!("points={} nvertex={}", g.size, g.nvertex.unwrap_or(0))
            }
            _ => format!("points={} ({}x{})", g.size, g.xsize, g.ysize),
        };
        let _ = writeln!(out, "{:6} : {:24} : {desc}", i + 1, g.kind.name());
    }
    let _ = writeln!(out, "   Vertical coordinates :");
    for (i, z) in ds.zaxes.iter().enumerate() {
        let _ = writeln!(
            out,
            "{:6} : {:24} : levels={} units={}",
            i + 1,
            z.var,
            z.len(),
            z.units.as_deref().unwrap_or("")
        );
    }
    if let Some(t) = &ds.time {
        let _ = writeln!(out, "   Time coordinate :");
        let _ = writeln!(out, "{:>33} : {} steps", t.var, t.len());
        let _ = writeln!(
            out,
            "     Units = {}  Calendar = {}",
            t.units_attr,
            t.calendar.cf_name()
        );
        if let (Some(a), Some(b)) = (t.steps.first(), t.steps.last()) {
            let _ = writeln!(
                out,
                "     First = {}  Last = {}",
                a.datetime.cdo_string().trim_start(),
                b.datetime.cdo_string().trim_start()
            );
        }
    }
    out
}

/// Runs an information operator and returns its stdout.
pub fn run(name: &str, src: &dyn ChunkSource, opts: &Options) -> Result<String> {
    let ds = src.dataset();
    Ok(match name {
        "showname" => showname(ds),
        "showtimestamp" => showtimestamp(ds, opts.silent),
        "griddes" => griddes(src)?,
        "sinfo" => {
            if opts.json {
                format!("{}\n", sinfo_json(src)?)
            } else {
                sinfo_text(src)
            }
        }
        other => {
            return Err(crate::error::Error::internal(format!(
                "'{other}' is not an information operator"
            )));
        }
    })
}
