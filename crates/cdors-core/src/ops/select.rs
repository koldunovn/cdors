//! Selections: `selname`, `sellevel`, `seltimestep`, `seldate`, `selyear`, `selmon`,
//! `selseason`, `sellonlatbox`. Each turns its arguments into an index map along one dimension
//! (or the horizontal dimensions) and composes it into the leaves of every affected variable;
//! no data is touched.
//!
//! Argument forms and matching rules follow cdo 2.6 (`src/operators/Seltime.cc`, `Selvar.cc`,
//! `Selbox.cc`, `src/param_conversion.cc`).

use crate::chain::OpNode;
use crate::error::{Error, ErrorCode, Result};
use crate::model::{CalDateTime, DimRole, GridKind, VarDim};
use crate::plan::{Desc, GridDesc, IndexMap, healpix_centers};

fn warn(msg: &str) {
    eprintln!("cdors: warning: {msg}");
}

fn bad(op: &str, msg: String) -> Error {
    Error::bad_arguments(msg).with("operator", op.to_owned())
}

/// cdo's `split_intstring`: `a`, `a/b` or `a/b/inc` (also `a/to/b`). Values below zero are
/// counted from the end when `n` is given (`-1` is the last).
fn int_list(op: &str, args: &[String], n: Option<usize>) -> Result<Vec<i64>> {
    let mut out = Vec::new();
    let fix = |v: i64| match n {
        Some(n) if v < 0 => n as i64 + 1 + v,
        _ => v,
    };
    for a in args {
        let parts: Vec<&str> = a.split('/').filter(|p| *p != "to").collect();
        let num = |s: &str| -> Result<i64> {
            s.trim()
                .parse::<i64>()
                .map_err(|_| bad(op, format!("'{a}' is not an integer or a range a/b[/inc]")))
        };
        let (first, last, inc) = match parts.as_slice() {
            [x] => (num(x)?, num(x)?, 1),
            [x, y] => (num(x)?, num(y)?, 1),
            [x, y, z] => (num(x)?, num(y)?, num(z)?),
            _ => {
                return Err(bad(
                    op,
                    format!("'{a}' is not an integer or a range a/b[/inc]"),
                ));
            }
        };
        let (first, last) = (fix(first), fix(last));
        if inc == 0 {
            return Err(bad(op, format!("range '{a}' has increment 0")));
        }
        let mut v = first;
        while (inc > 0 && v <= last) || (inc < 0 && v >= last) {
            out.push(v);
            v += inc;
        }
    }
    Ok(out)
}

/// cdo's encoded date-time `YYYYMMDD.hhmmss` (`datestr_to_double`); `end` sets a missing time to
/// 23:59:59.
fn date_value(op: &str, s: &str, end: bool) -> Result<f64> {
    if !s.contains('-') || (s.starts_with('-') && !s[1..].contains('-')) {
        return s
            .parse::<f64>()
            .map_err(|_| bad(op, format!("invalid date '{s}'")));
    }
    let (date, time) = match s.split_once('T') {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let (neg, date) = match date.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, date),
    };
    let dp: Vec<&str> = date.split('-').collect();
    let p = |x: &str| -> Result<i64> {
        x.parse::<i64>().map_err(|_| {
            bad(
                op,
                format!("invalid date '{s}' (format YYYY-MM-DD[Thh:mm:ss])"),
            )
        })
    };
    if dp.len() != 3 {
        return Err(bad(
            op,
            format!("invalid date '{s}' (format YYYY-MM-DD[Thh:mm:ss])"),
        ));
    }
    let (y, m, d) = (p(dp[0])?, p(dp[1])?, p(dp[2])?);
    let (hh, mm, ss) = match time {
        Some(t) => {
            let tp: Vec<&str> = t.split(':').collect();
            if tp.len() != 3 {
                return Err(bad(op, format!("invalid time in '{s}' (format hh:mm:ss)")));
            }
            (p(tp[0])?, p(tp[1])?, p(tp[2])?)
        }
        None if end => (23, 59, 59),
        None => (0, 0, 0),
    };
    let mut v = (y * 10000 + m * 100 + d) as f64 + (hh * 10000 + mm * 100 + ss) as f64 / 1e6;
    if neg {
        v = -v;
    }
    Ok(v)
}

fn step_value(dt: &CalDateTime) -> f64 {
    let date =
        (dt.year.unsigned_abs() as i64 * 10000 + dt.month as i64 * 100 + dt.day as i64) as f64;
    let t = (dt.hour * 10000 + dt.minute * 100 + dt.second) as f64 / 1e6;
    let v = date + t;
    if dt.year < 0 { -v } else { v }
}

/// Months of a season string (`season_to_months`) or season numbers 1-4.
fn season_months(op: &str, args: &[String]) -> Result<[bool; 13]> {
    let mut m = [false; 13];
    for a in args {
        let a = a.to_ascii_uppercase();
        if let Ok(k) = a.parse::<u32>() {
            let ms: [u32; 3] = match k {
                1 => [12, 1, 2],
                2 => [3, 4, 5],
                3 => [6, 7, 8],
                4 => [9, 10, 11],
                _ => return Err(bad(op, format!("season {k} not available (1-4)"))),
            };
            for x in ms {
                m[x as usize] = true;
            }
        } else if a == "ANN" {
            m[1..].iter_mut().for_each(|x| *x = true);
        } else {
            const S: &str = "JFMAMJJASONDJFMAMJJASOND";
            let pos = S
                .find(&a)
                .filter(|_| !a.is_empty() && a.len() <= 12)
                .ok_or_else(|| bad(op, format!("season '{a}' not available")).with_hint("seasons: DJF, MAM, JJA, SON, ANN or any run of month initials such as JJAS"))?;
            for k in pos..pos + a.len() {
                m[k % 12 + 1] = true;
            }
        }
    }
    Ok(m)
}

/// Applies a time selection to all variables with a time dimension.
pub fn select_time(mut d: Desc, op: &str, idx: Vec<usize>) -> Result<Desc> {
    let Some(t) = d.time.as_mut() else {
        return Err(Error::new(
            ErrorCode::UnsupportedDimension,
            format!("'{op}' needs a time axis, the input has none"),
        )
        .with("operator", op.to_owned()));
    };
    if idx.is_empty() {
        return Err(bad(op, format!("'{op}' selected no timestep"))
            .with_hint("check the arguments against `cdors showtimestamp <input>`"));
    }
    let sel = IndexMap::from_list(idx);
    if sel.is_identity(t.len()) {
        return Ok(d);
    }
    t.select(&sel);
    for v in &mut d.vars {
        if let Some(td) = v.dim_of(DimRole::Time) {
            v.select(td, &sel);
        }
    }
    Ok(d)
}

fn describe_time(node: &OpNode, d: Desc) -> Result<Desc> {
    let op = node.name.as_str();
    let n = d.ntime();
    let steps: Vec<CalDateTime> = d
        .time
        .as_ref()
        .map(|t| t.axis.steps.iter().map(|s| s.datetime).collect())
        .unwrap_or_default();
    let idx: Vec<usize> = match op {
        "seltimestep" => {
            let mut v = int_list(op, &node.args, Some(n))?;
            v.sort_unstable();
            v.dedup();
            let missing: Vec<String> = v
                .iter()
                .filter(|&&x| x < 1 || x as usize > n)
                .map(i64::to_string)
                .collect();
            if !missing.is_empty() {
                warn(&format!("timesteps {} not found", missing.join(",")));
            }
            v.into_iter()
                .filter(|&x| x >= 1 && x as usize <= n)
                .map(|x| x as usize - 1)
                .collect()
        }
        "seldate" => {
            if node.args.len() > 2 {
                return Err(bad(
                    op,
                    "seldate takes a start date and an optional end date".into(),
                ));
            }
            let lo = match node.args[0].as_str() {
                "-" => f64::NEG_INFINITY,
                s => date_value(op, s, false)?,
            };
            let hi = match node.args.get(1).map(String::as_str) {
                Some("-") => f64::INFINITY,
                Some(s) => date_value(op, s, true)?,
                None if node.args[0].contains('-') && !node.args[0].contains('T') => lo + 0.235959,
                None => lo,
            };
            (0..n)
                .filter(|&i| {
                    let v = step_value(&steps[i]);
                    v >= lo && v <= hi
                })
                .collect()
        }
        "selyear" | "selmon" => {
            let v = int_list(op, &node.args, None)?;
            let key = |dt: &CalDateTime| -> i64 {
                if op == "selyear" {
                    dt.year as i64
                } else {
                    dt.month as i64
                }
            };
            for x in &v {
                if !steps.iter().any(|s| key(s) == *x) {
                    warn(&format!(
                        "{} {x} not found",
                        if op == "selyear" { "year" } else { "month" }
                    ));
                }
            }
            (0..n).filter(|&i| v.contains(&key(&steps[i]))).collect()
        }
        "selseason" => {
            let m = season_months(op, &node.args)?;
            (0..n).filter(|&i| m[steps[i].month as usize]).collect()
        }
        _ => unreachable!("time selection"),
    };
    select_time(d, op, idx)
}

fn describe_name(node: &OpNode, mut d: Desc) -> Result<Desc> {
    let pats: Vec<glob::Pattern> = node
        .args
        .iter()
        .map(|a| {
            glob::Pattern::new(a).map_err(|e| bad("selname", format!("bad pattern '{a}': {e}")))
        })
        .collect::<Result<_>>()?;
    for (a, p) in node.args.iter().zip(&pats) {
        if !d.vars.iter().any(|v| p.matches(&v.name)) {
            warn(&format!("variable name {a} not found"));
        }
    }
    d.vars.retain(|v| pats.iter().any(|p| p.matches(&v.name)));
    if d.vars.is_empty() {
        return Err(bad(
            "selname",
            format!("no variable selected by selname,{}", node.args.join(",")),
        )
        .with_hint("list the variables with `cdors showname <input>`"));
    }
    Ok(d)
}

fn describe_level(node: &OpNode, mut d: Desc) -> Result<Desc> {
    let lv: Vec<f64> = node
        .args
        .iter()
        .map(|a| {
            a.parse::<f64>()
                .map_err(|_| bad("sellevel", format!("'{a}' is not a number")))
        })
        .collect::<Result<_>>()?;
    let hit = |x: f64| lv.iter().any(|&l| (l - x).abs() < 0.0001);
    let mut found = vec![false; lv.len()];
    let mut maps: Vec<IndexMap> = Vec::new();
    for z in &mut d.zaxes {
        let idx: Vec<usize> = (0..z.axis.len())
            .filter(|&i| hit(z.axis.values[i]))
            .collect();
        for (k, &l) in lv.iter().enumerate() {
            if z.axis.values.iter().any(|&x| (l - x).abs() < 0.0001) {
                found[k] = true;
            }
        }
        let m = IndexMap::from_list(idx);
        if !m.is_empty() {
            z.select(&m);
        }
        maps.push(m);
    }
    for (k, f) in found.iter().enumerate() {
        if !f && !hit(0.0) {
            warn(&format!("level {} not found", lv[k]));
        }
    }
    let surface = hit(0.0);
    let mut out = Vec::new();
    for mut v in std::mem::take(&mut d.vars) {
        match (v.zaxis, v.dim_of(DimRole::Vertical)) {
            (Some(zi), Some(zd)) => {
                if maps[zi].is_empty() {
                    continue;
                }
                let m = maps[zi].clone();
                v.select(zd, &m);
                out.push(v);
            }
            _ if surface => out.push(v),
            _ => {}
        }
    }
    d.vars = out;
    if d.vars.is_empty() {
        return Err(bad(
            "sellevel",
            format!("no level selected by sellevel,{}", node.args.join(",")),
        )
        .with_hint("show the levels with `cdors sinfo <input>`"));
    }
    Ok(d)
}

fn to_degrees(vals: &mut [f64], units: &str) {
    if units.trim().starts_with("radian") {
        vals.iter_mut().for_each(|x| *x = x.to_degrees());
    }
}

/// cdo's `correct_xvals` for the longitudes of a regular-grid box.
fn correct_xvals(x: &mut [f64]) {
    let n = x.len();
    if n == 0 {
        return;
    }
    let last = n - 1;
    if n > 1 && x[0] == x[last] {
        x[last] += 360.0;
    }
    if x[0] > x[last] {
        for v in x.iter_mut() {
            if *v >= 180.0 {
                *v -= 360.0;
            }
        }
    }
    for v in x.iter_mut() {
        if *v < -180.0 {
            *v += 360.0;
        }
        if *v > 360.0 {
            *v -= 360.0;
        }
    }
    if x[0] > x[last] {
        for i in 1..n {
            if x[i] < x[i - 1] {
                x[i] += 360.0;
            }
        }
    }
}

fn is_equal(a: f64, b: f64) -> bool {
    a == b || (a - b).abs() <= f64::EPSILON * a.abs().max(b.abs())
}

/// `gen_lonlat_selbox_reg2d`: returns (rows, columns) of the box.
fn box_reg2d(
    xvals: &[f64],
    yvals: &[f64],
    lon1: f64,
    lon2: f64,
    lat1: f64,
    lat2: f64,
) -> Result<(Vec<usize>, Vec<usize>)> {
    let (mut xlon1, mut xlon2) = (lon1, lon2);
    if !is_equal(xlon1, xlon2) {
        xlon2 -= 360.0 * ((xlon2 - xlon1) / 360.0).floor();
        if is_equal(xlon1, xlon2) {
            xlon2 += 360.0;
        }
    } else {
        xlon2 += 0.00001;
    }
    let nlon = xvals.len() as i64;
    let nlat = yvals.len() as i64;
    let shift = 360.0 * ((xlon1 - xvals[0]) / 360.0).floor();
    xlon2 -= shift;
    xlon1 -= shift;
    let xv = |i: i64| xvals[i as usize];
    let mut lon21 = 0i64;
    while lon21 < nlon && xv(lon21) < xlon1 {
        lon21 += 1;
    }
    let mut lon22 = lon21;
    while lon22 < nlon && xv(lon22) < xlon2 {
        lon22 += 1;
    }
    if lon22 >= nlon || xv(lon22) > xlon2 {
        lon22 -= 1;
    }
    xlon1 -= 360.0;
    xlon2 -= 360.0;
    let mut lon11 = 0i64;
    while lon11 < nlon && xv(lon11) < xlon1 {
        lon11 += 1;
    }
    let mut lon12 = lon11;
    while lon12 < nlon && xv(lon12) < xlon2 {
        lon12 += 1;
    }
    if lon12 >= nlon || xv(lon12) > xlon2 {
        lon12 -= 1;
    }
    if lon21 < nlon && lon12 >= 0 && is_equal(xv(lon12), xv(lon21)) {
        lon12 -= 1;
    }
    if (lon12 - lon11 + 1) + (lon22 - lon21 + 1) < 1 {
        return Err(Error::bad_arguments(
            "sellonlatbox: longitudinal dimension is too small",
        ));
    }
    let yv = |i: i64| yvals[i as usize];
    let (mut la1, mut la2);
    if yv(0) > yv(nlat - 1) {
        let (a, b) = if lat1 > lat2 {
            (lat1, lat2)
        } else {
            (lat2, lat1)
        };
        la1 = 0;
        while la1 < nlat && yv(la1) > a {
            la1 += 1;
        }
        la2 = nlat - 1;
        while la2 > 0 && yv(la2) < b {
            la2 -= 1;
        }
    } else {
        let (a, b) = if lat1 < lat2 {
            (lat1, lat2)
        } else {
            (lat2, lat1)
        };
        la1 = 0;
        while la1 < nlat && yv(la1) < a {
            la1 += 1;
        }
        la2 = nlat - 1;
        while la2 > 0 && yv(la2) > b {
            la2 -= 1;
        }
    }
    if la2 < la1 {
        return Err(Error::bad_arguments(
            "sellonlatbox: latitudinal dimension is too small",
        ));
    }
    let rows = (la1..=la2).map(|i| i as usize).collect();
    let cols = (lon21..=lon22)
        .chain(lon11..=lon12)
        .map(|i| i as usize)
        .collect();
    Ok((rows, cols))
}

/// `gen_lonlat_selbox_curv`: (rows, columns) of the smallest index box holding the points.
#[allow(clippy::too_many_arguments)]
fn box_curv(
    x: &[f64],
    y: &[f64],
    nx: usize,
    ny: usize,
    lon1: f64,
    lon2: f64,
    lat1: f64,
    lat2: f64,
) -> Result<(Vec<usize>, Vec<usize>)> {
    if lon1 > lon2 {
        return Err(Error::bad_arguments(
            "sellonlatbox: the second longitude has to be greater than the first one",
        ));
    }
    let (lat1, lat2) = if lat1 > lat2 {
        (lat2, lat1)
    } else {
        (lat1, lat2)
    };
    let (nlon, nlat) = (nx as i64, ny as i64);
    let (mut la1, mut la2) = (nlat - 1, 0i64);
    let (mut lon21, mut lon22) = (nlon - 1, 0i64);
    for j in 0..nlat {
        for i in 0..nlon {
            let k = (j * nlon + i) as usize;
            let (xv, yv) = (x[k], y[k]);
            if yv >= lat1
                && yv <= lat2
                && ((xv >= lon1 && xv <= lon2)
                    || (xv - 360.0 >= lon1 && xv - 360.0 <= lon2)
                    || (xv + 360.0 >= lon1 && xv + 360.0 <= lon2))
            {
                lon21 = lon21.min(i);
                lon22 = lon22.max(i);
                la1 = la1.min(j);
                la2 = la2.max(j);
            }
        }
    }
    if la2 < la1 || lon22 < lon21 {
        return Err(Error::bad_arguments(
            "sellonlatbox: no grid points in the box",
        ));
    }
    Ok((
        (la1..=la2).map(|i| i as usize).collect(),
        (lon21..=lon22).map(|i| i as usize).collect(),
    ))
}

fn describe_box(node: &OpNode, mut d: Desc) -> Result<Desc> {
    let a: Vec<f64> = node
        .args
        .iter()
        .map(|s| {
            s.parse::<f64>()
                .map_err(|_| bad("sellonlatbox", format!("'{s}' is not a number")))
        })
        .collect::<Result<_>>()?;
    let (lon1, lon2, lat1, lat2) = (a[0], a[1], a[2], a[3]);
    let used: Vec<usize> = (0..d.grids.len())
        .filter(|&gi| d.vars.iter().any(|v| v.grid == Some(gi)))
        .collect();
    for gi in used {
        let g: &GridDesc = &d.grids[gi];
        if g.size() == 1 {
            continue;
        }
        let (new_sel, new_xvals, new_kind): (Vec<IndexMap>, Option<Vec<f64>>, GridKind) =
            match g.kind {
                GridKind::Regular | GridKind::Gaussian => {
                    let ydim = g.base.y.as_ref().map(|y| y.dim.clone());
                    if g.base.dims.first() != ydim.as_ref() {
                        return Err(Error::new(
                            ErrorCode::UnsupportedGrid,
                            "sellonlatbox: regular grids stored as (lon, lat) are not supported",
                        ));
                    }
                    let c = g
                        .coords()?
                        .ok_or_else(|| Error::internal("regular grid without coordinates"))?;
                    let (mut xv, mut yv) = (c.xvals.clone(), c.yvals.clone());
                    to_degrees(&mut xv, &c.xunits);
                    to_degrees(&mut yv, &c.yunits);
                    let (rows, cols) = box_reg2d(&xv, &yv, lon1, lon2, lat1, lat2)?;
                    let mut nx: Vec<f64> = cols.iter().map(|&i| c.xvals[i]).collect();
                    if c.xunits.starts_with("degree") {
                        correct_xvals(&mut nx);
                    }
                    (
                        vec![IndexMap::from_list(rows), IndexMap::from_list(cols)],
                        Some(nx),
                        g.kind,
                    )
                }
                GridKind::Curvilinear => {
                    let c = g
                        .coords()?
                        .ok_or_else(|| Error::internal("curvilinear grid without coordinates"))?;
                    let (mut xv, mut yv) = (c.xvals.clone(), c.yvals.clone());
                    to_degrees(&mut xv, &c.xunits);
                    to_degrees(&mut yv, &c.yunits);
                    let (nx, ny) = g.xy_size();
                    let (rows, cols) = box_curv(&xv, &yv, nx, ny, lon1, lon2, lat1, lat2)?;
                    (
                        vec![IndexMap::from_list(rows), IndexMap::from_list(cols)],
                        None,
                        g.kind,
                    )
                }
                GridKind::Unstructured | GridKind::Healpix => {
                    let (mut xv, mut yv, xu, yu) = if g.kind == GridKind::Healpix {
                        let hp = g.base.healpix.as_ref().expect("healpix");
                        let cells: Vec<u64> = match &hp.index_var {
                            Some(iv) => {
                                let idx = g.src.read_var(iv)?;
                                g.sel[0].to_vec().iter().map(|&i| idx[i] as u64).collect()
                            }
                            None => g.sel[0].to_vec().iter().map(|&i| i as u64).collect(),
                        };
                        let (x, y) = healpix_centers(hp.nside, hp.order, &cells);
                        (x, y, "radian".to_owned(), "radian".to_owned())
                    } else {
                        let c = g.coords()?.ok_or_else(|| {
                            Error::internal("unstructured grid without coordinates")
                        })?;
                        (c.xvals, c.yvals, c.xunits, c.yunits)
                    };
                    to_degrees(&mut xv, &xu);
                    to_degrees(&mut yv, &yu);
                    let (mut l1, mut l2, mut b1, mut b2) = (lon1, lon2, lat1, lat2);
                    if l1 >= l2 {
                        std::mem::swap(&mut l1, &mut l2);
                    }
                    if b1 >= b2 {
                        std::mem::swap(&mut b1, &mut b2);
                    }
                    let cells: Vec<usize> = (0..xv.len())
                        .filter(|&i| {
                            let (x, y) = (xv[i], yv[i]);
                            y >= b1
                                && y <= b2
                                && ((x >= l1 && x <= l2)
                                    || (x + 360.0 >= l1 && x + 360.0 <= l2)
                                    || (x - 360.0 >= l1 && x - 360.0 <= l2))
                        })
                        .collect();
                    if cells.is_empty() {
                        return Err(bad(
                            "sellonlatbox",
                            "no grid points found in the box".into(),
                        ));
                    }
                    (
                        vec![IndexMap::from_list(cells)],
                        None,
                        GridKind::Unstructured,
                    )
                }
                GridKind::Generic => {
                    return Err(Error::new(
                        ErrorCode::NoCoordinates,
                        "sellonlatbox needs horizontal coordinates; the grid has none",
                    )
                    .with_hint("attach coordinates with -setgrid,<grid or mesh file>"));
                }
            };
        let g = &mut d.grids[gi];
        let was_healpix = g.kind == GridKind::Healpix;
        for (k, m) in new_sel.iter().enumerate() {
            g.sel[k] = g.sel[k].compose(m);
        }
        g.kind = new_kind;
        if new_xvals.is_some() {
            g.xvals = new_xvals;
        }
        for v in &mut d.vars {
            if v.grid != Some(gi) {
                continue;
            }
            let hd = v.hdims();
            for (k, m) in new_sel.iter().enumerate() {
                v.select(hd[k], m);
            }
            if was_healpix {
                // cdo writes the selected HEALPix cells as an unstructured grid over `ncells`
                let VarDim { name, .. } = &mut v.dims[hd[0]];
                *name = "ncells".to_owned();
            }
        }
    }
    Ok(d)
}

/// Output description of a selection operator.
pub fn describe(node: &OpNode, mut inputs: Vec<Desc>) -> Result<Desc> {
    let d = inputs.remove(0);
    match node.name.as_str() {
        "selname" => describe_name(node, d),
        "sellevel" => describe_level(node, d),
        "seltimestep" | "seldate" | "selyear" | "selmon" | "selseason" => describe_time(node, d),
        "sellonlatbox" => describe_box(node, d),
        other => Err(Error::internal(format!("'{other}' is not a selection"))),
    }
}
