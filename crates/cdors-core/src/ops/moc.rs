//! `moc` (cdors operator, not in cdo): the meridional overturning streamfunction of a model mesh
//! from the vertical velocity on its interfaces.
//!
//! `moc[,dlat=<deg>|lat=<lat>[/<lat>...]][,regions=<set>,select=<abbrev,...>]`:
//! - each element of the basin gets the mean of the vertical velocity at its three nodes
//!   (missing nodes left out; a variable on the elements is taken as it is), at the interfaces
//!   that top its wet layers: the sea floor below them is left out, no flow crosses it;
//! - ψ(φ, k) = −Σ w̄·A over the basin's elements whose centre lies on or north of the latitude
//!   line φ, A the element area (FESOM2 `elem_area`), centres as the mesh defines them (FESOM2:
//!   the normalised mean of the nodes' unit vectors): the overturning that closes at the basin's
//!   northern end, as pyfesom2's `xmoc_data`. In Sv for w in m/s, else Σ in `<units> m2`;
//! - the basin: the elements whose centre lies in the selected regions of a catalog set
//!   (`regions=moc,select=Atlantic_MOC`; membership as `regmask`), the whole mesh without
//!   `regions=`;
//! - the lines: every `dlat` degrees from −90 to 90 (default 1), or the latitudes of `lat=`. A
//!   line's value at interface k is missing where no element of the basin that the line crosses
//!   (the latitudes of its vertices span φ) has water at k;
//! - output `moc(time, nz, lat, lon)`: one longitude (0), the lines as latitudes; the levels
//!   are the input's.
//!
//! **Fold order.** Complete fields ([`FieldKernel`]); each line's sum adds the elements in
//! ascending order, then the lines from the north, so results do not depend on chunks or threads.

use super::regions::{self as reg, Spec};
use super::remap::{FieldKernel, FieldLane, FieldMap, FieldVar};
use crate::chain::OpNode;
use crate::error::{Error, ErrorCode, Result};
use crate::io::Values;
use crate::model::{AttrValue, DType};
use crate::plan::{Desc, Fold, IndexMap};
use cdors_mesh::models::{self, DimRule};
use cdors_mesh::Mesh;
use std::ops::Range;
use std::sync::{Arc, OnceLock};

/// The incidence table of the element means (rows: elements, columns: their nodes).
const TABLE: &str = "elem_nodes";

pub fn handles(name: &str) -> bool {
    name == "moc"
}

fn usage() -> &'static str {
    "usage: moc[,dlat=<deg>|lat=<lat>[/<lat>...]][,regions=<set>,select=<abbrev,...>] \
     -setgrid,<mesh> -selname,w <in>"
}

fn bad(msg: impl Into<String>) -> Error {
    Error::bad_arguments(msg)
        .with("operator", "moc".to_owned())
        .with_hint(usage())
}

/// The latitude lines of `dlat=` or `lat=` (ascending, distinct), and the other arguments.
fn parse_lines(args: &[String]) -> Result<(Vec<f64>, Vec<String>)> {
    let (mut lines, mut rest) = (None, Vec::new());
    for a in args.iter().map(|a| a.trim()) {
        let set = |lines: &mut Option<Vec<f64>>, v: Vec<f64>| {
            if lines.replace(v).is_some() {
                return Err(bad("moc: give either dlat= or lat=, once"));
            }
            Ok(())
        };
        if let Some(v) = a.strip_prefix("dlat=") {
            let d: f64 = v
                .parse()
                .ok()
                .filter(|d: &f64| d.is_finite() && *d > 0.0 && *d <= 180.0)
                .ok_or_else(|| bad(format!("moc: dlat={v} is not a step in (0, 180] degrees")))?;
            let n = (180.0 / d + 1e-9).floor() as usize;
            set(&mut lines, (0..=n).map(|j| -90.0 + j as f64 * d).collect())?;
        } else if let Some(v) = a.strip_prefix("lat=") {
            let mut l = Vec::new();
            for s in v.split('/') {
                let x: f64 = s
                    .trim()
                    .parse()
                    .ok()
                    .filter(|x: &f64| (-90.0..=90.0).contains(x))
                    .ok_or_else(|| bad(format!("moc: lat={v}: '{s}' is not a latitude")))?;
                l.push(x);
            }
            l.sort_by(f64::total_cmp);
            l.dedup();
            set(&mut lines, l)?;
        } else {
            rest.push(a.to_owned());
        }
    }
    let lines = lines.unwrap_or_else(|| (0..=180).map(|j| -90.0 + j as f64).collect());
    Ok((lines, rest))
}

/// What the kernel needs of the elements, built on first use: each basin element's nodes
/// (empty for element input) and each line's interfaces with water.
struct Tables {
    nodes: Vec<[u32; 3]>,
    valid: Vec<Range<usize>>,
}

struct MocMap {
    mesh: Arc<Mesh>,
    on_nodes: bool,
    /// The basin's elements, ascending, and the line each adds to last (the northernmost line on
    /// or south of its centre; `u32::MAX`: south of every line).
    elems: Vec<u32>,
    last_line: Vec<u32>,
    lines: Vec<f64>,
    /// The output dimension of the levels and the mesh interface of each index along it.
    level: (usize, IndexMap),
    scale: f64,
    tables: OnceLock<std::result::Result<Arc<Tables>, String>>,
    info: serde_json::Value,
}

impl MocMap {
    fn tables(&self) -> Result<Arc<Tables>> {
        self.tables
            .get_or_init(|| self.build().map(Arc::new))
            .clone()
            .map_err(|m| Error::new(ErrorCode::BadData, format!("moc: mesh {}: {m}", self.mesh.name)))
    }

    fn build(&self) -> std::result::Result<Tables, String> {
        let t = self.mesh.table(TABLE).ok_or("no elem_nodes table")?;
        let en = t.csr.get().map_err(|e| e.to_string())?;
        let nlat = &self.mesh.place(&t.cols_place).ok_or("no node place")?.lat;
        let wet = self
            .mesh
            .vertical
            .as_ref()
            .and_then(|z| z.wet(&t.rows_place))
            .ok_or("no wet levels of the elements")?;
        let mut nodes = Vec::with_capacity(if self.on_nodes { self.elems.len() } else { 0 });
        let mut valid: Vec<Range<usize>> = vec![0..0; self.lines.len()];
        for &e in &self.elems {
            let row = en.row(e as usize);
            if row.len() != 3 {
                return Err(format!("element {e} has {} nodes, not 3", row.len()));
            }
            if self.on_nodes {
                nodes.push([row[0], row[1], row[2]]);
            }
            let (lo, hi) = row.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &n| {
                let y = nlat[n as usize];
                (lo.min(y), hi.max(y))
            });
            let k = wet.interfaces(e as usize);
            if k.is_empty() {
                continue;
            }
            let first = self.lines.partition_point(|&y| y < lo);
            let end = self.lines.partition_point(|&y| y <= hi);
            for v in &mut valid[first..end.max(first)] {
                *v = if v.start == v.end {
                    k.clone()
                } else {
                    v.start.min(k.start)..v.end.max(k.end)
                };
            }
        }
        Ok(Tables { nodes, valid })
    }

    fn field<T: Copy + Into<f64>>(&self, x: &[T], k: usize, tb: &Tables, out: &mut [f64]) {
        let area = &self.mesh.place("elements").expect("checked").area;
        let wet = self
            .mesh
            .vertical
            .as_ref()
            .and_then(|z| z.wet("elements"))
            .expect("checked");
        let mut acc = vec![0.0f64; self.lines.len()];
        for (i, &e) in self.elems.iter().enumerate() {
            let b = self.last_line[i];
            // the interfaces that top the element's wet layers
            if b == u32::MAX || !wet.range(e as usize).contains(&k) {
                continue;
            }
            let w = if self.on_nodes {
                let (mut s, mut n) = (0.0, 0);
                for &c in &tb.nodes[i] {
                    let v: f64 = x[c as usize].into();
                    if v.is_finite() {
                        s += v;
                        n += 1;
                    }
                }
                if n == 0 {
                    continue;
                }
                s / f64::from(n)
            } else {
                x[e as usize].into()
            };
            if w.is_finite() {
                acc[b as usize] += w * area[e as usize];
            }
        }
        let mut s = 0.0;
        for j in (0..self.lines.len()).rev() {
            s += acc[j];
            out[j] = if tb.valid[j].contains(&k) {
                -s * self.scale
            } else {
                f64::NAN
            };
        }
    }
}

impl FieldMap for MocMap {
    fn apply(
        &self,
        _var: usize,
        src: &Values,
        n: usize,
        out: DType,
        lane: &FieldLane,
    ) -> Result<Values> {
        let tb = self.tables()?;
        let levels: Vec<usize> = lane
            .index(self.level.0)
            .into_iter()
            .map(|j| self.level.1.get(j))
            .collect();
        let size = src.len() / n.max(1);
        let m = self.lines.len();
        let mut v = vec![f64::NAN; n * m];
        for f in 0..n {
            let o = &mut v[f * m..(f + 1) * m];
            match src {
                Values::F32(x) => self.field(&x[f * size..(f + 1) * size], levels[f], &tb, o),
                Values::F64(x) => self.field(&x[f * size..(f + 1) * size], levels[f], &tb, o),
            }
        }
        Ok(match out {
            DType::F32 => Values::F32(v.into_iter().map(|x| x as f32).collect()),
            _ => Values::F64(v),
        })
    }

    fn plan_info(&self) -> Option<serde_json::Value> {
        Some(self.info.clone())
    }
}

fn not_on_mesh(var: &str, what: String) -> Error {
    Error::new(ErrorCode::UnsupportedGrid, format!("moc of '{var}': {what}"))
        .with("operator", "moc".to_owned())
        .with("variable", var.to_owned())
        .with_hint(
            "moc integrates the vertical velocity on the interfaces of a model mesh: attach the \
             mesh (-setgrid,fesom:<dir>) and select w (FESOM2: w on nz) with -selname, without a \
             horizontal selection (choose the basin with regions=)",
        )
}

/// Whether `units` is a velocity in m/s.
fn is_m_per_s(units: &str) -> bool {
    matches!(
        units.trim(),
        "m/s" | "m s-1" | "m s**-1" | "m.s-1" | "m s^-1" | "m sec-1" | "m/sec"
    )
}

/// Output description of `moc`.
pub fn describe(node: &OpNode, mut inputs: Vec<Desc>) -> Result<Desc> {
    let op = "moc";
    let input = inputs
        .pop()
        .ok_or_else(|| Error::internal("moc without input"))?;
    super::remap::no_pending_fold(op, &input)?;
    let (lines, rest) = parse_lines(&node.args)?;
    let spec: Option<Spec> = reg::parse_spec(op, &rest)?;
    if rest.iter().any(|a| !a.is_empty()) && spec.is_none() {
        return Err(bad(format!("moc: unexpected argument '{}'", rest[0])));
    }
    if spec.as_ref().is_some_and(|s| s.fill) {
        return Err(bad("moc: fill=nearest does not apply (the basin is the elements whose centre \
                        lies in the regions)"));
    }
    let [v] = input.vars.as_slice() else {
        return Err(Error::bad_arguments(format!(
            "moc takes one variable, the vertical velocity; the input has {} ({})",
            input.vars.len(),
            input.vars.iter().map(|v| v.name.as_str()).collect::<Vec<_>>().join(", ")
        ))
        .with("operator", op.to_owned())
        .with_hint("select it with -selname,w"));
    };
    let Some(gi) = v.grid else {
        return Err(not_on_mesh(&v.name, "no horizontal grid".into()));
    };
    let g = &input.grids[gi];
    let (Some(mesh), Some(place)) = (g.base.mesh.clone(), g.base.mesh_place()) else {
        return Err(not_on_mesh(&v.name, "the grid is not a model mesh".into()));
    };
    let Some(t) = mesh.table(TABLE) else {
        return Err(not_on_mesh(&v.name, format!("the mesh {} has no {TABLE} table", mesh.name)));
    };
    let on_nodes = place.name == t.cols_place;
    if !on_nodes && place.name != t.rows_place {
        return Err(not_on_mesh(
            &v.name,
            format!("it is on the {} of the mesh {}, not on its nodes or elements", place.name, mesh.name),
        ));
    }
    if g.size() != place.size || !g.sel[0].is_identity(place.size) {
        return Err(not_on_mesh(
            &v.name,
            format!("it covers {} of the {} {}", g.size(), place.size, place.name),
        ));
    }
    let interfaces = v
        .expr
        .wet_mask()
        .filter(|(m, _, _)| m.rule == DimRule::Interfaces && m.place == place.name);
    let Some((_, ldim, levels)) = interfaces else {
        return Err(not_on_mesh(
            &v.name,
            "its levels are not the mesh's interfaces (FESOM2: nz)".into(),
        ));
    };
    let elements = mesh
        .place(&t.rows_place)
        .ok_or_else(|| Error::internal("no element place"))?;
    if elements.name != "elements"
        || mesh.vertical.as_ref().and_then(|z| z.wet(&elements.name)).is_none()
    {
        return Err(not_on_mesh(&v.name, format!("the mesh {} has no wet levels of its elements", mesh.name)));
    }

    // the basin, and the last line each element adds to
    let set = spec.as_ref().map(|s| reg::open_set(op, s)).transpose()?;
    let (elems, basin): (Vec<u32>, String) = match (&spec, &set) {
        (Some(s), Some(set)) => {
            let m = set.members(&elements.lon, &elements.lat);
            let mut e: Vec<u32> = (0..m.rows()).flat_map(|r| m.row(r).iter().copied()).collect();
            e.sort_unstable();
            e.dedup();
            if e.is_empty() {
                return Err(Error::new(
                    ErrorCode::BadData,
                    format!("moc: no element centre of the mesh {} lies in {}", mesh.name, s.describe()),
                )
                .with("operator", op.to_owned()));
            }
            let names = set.abbrevs.join(", ");
            (
                e.clone(),
                format!(
                    "the {} of {} elements whose centre lies in {names} of the set {} (regionmask \
                     rules: planar lon/lat; a centre on an outline goes to the region west or south \
                     of it)",
                    e.len(),
                    elements.size,
                    s.set
                ),
            )
        }
        _ => (
            (0..elements.size as u32).collect(),
            format!("all {} elements of the mesh", elements.size),
        ),
    };
    let last_line: Vec<u32> = elems
        .iter()
        .map(|&e| {
            let p = lines.partition_point(|&y| y <= elements.lat[e as usize]);
            if p == 0 { u32::MAX } else { (p - 1) as u32 }
        })
        .collect();
    let units = v.attrs.get_str("units").unwrap_or("").trim().to_owned();
    let (scale, out_units) = if is_m_per_s(&units) {
        (1e-6, "Sv".to_owned())
    } else if units.is_empty() {
        (1.0, "m2".to_owned())
    } else {
        (1.0, format!("{units} m2"))
    };

    let token = crate::plan::explain::op_token(node);
    let centres = models::place_method(&mesh, &elements.name).map_or("", |(c, _)| c);
    let note = serde_json::json!({
        "operator": token,
        "streamfunction": format!(
            "psi(lat, k) = -sum of w_e * A_e over the basin's elements whose centre lies on or north \
             of the line, at each interface k ({out_units}{}): the overturning that closes at the \
             basin's northern end",
            if scale != 1.0 { ", 1 Sv = 1e6 m3/s" } else { "" }
        ),
        "w_e": if on_nodes {
            "the mean of the input at the element's three nodes (missing nodes left out), at the \
             interfaces that top the element's wet layers; not at the sea floor"
        } else {
            "the element's value, at the interfaces that top its wet layers; not at the sea floor"
        },
        "areas": models::place_method(&mesh, &elements.name).map_or("the element areas", |(_, a)| a),
        "centres": format!("elements of the mesh {}: {centres}", mesh.name),
        "basin": basin,
        "lines": lines.len(),
        "missing": "below the deepest element of the basin that the line crosses (the latitudes \
                    of its vertices span the line's)",
    });
    let table_bytes = 4 * 3 * if on_nodes { elems.len() } else { 0 } as u64;
    let info = serde_json::json!({ "mesh_weights": [{
        "operator": token, "weights": "elem_area", "mesh": mesh.name,
        "elements": elems.len(), "lines": lines.len(),
        "bytes": table_bytes + 8 * elems.len() as u64 + 16 * lines.len() as u64,
    }]});

    let (rg, hdims) = super::fldstat::made_grid(g, lines.clone(), None, None);
    let mut fv = FieldVar::new(op, v, g.size(), vec![lines.len(), 1])?;
    let mut ov = v.clone();
    ov.dims.splice(fv.hfirst..fv.hfirst + fv.hcount, hdims);
    ov.name = "moc".to_owned();
    ov.grid = Some(0);
    ov.attrs.0.retain(|(k, _)| {
        !matches!(
            k.as_str(),
            "location" | "coordinates" | "CDI_grid_type" | "grid_mapping" | "mesh" | "description"
                | "long_name" | "standard_name" | "units"
        )
    });
    let text = |s: &str| AttrValue::Text(s.to_owned());
    ov.attrs.0.extend([
        ("long_name".to_owned(), text("meridional overturning streamfunction")),
        ("units".to_owned(), text(&out_units)),
        ("moc_input".to_owned(), text(&v.name)),
        ("moc_basin".to_owned(), text(&spec.as_ref().map_or("whole mesh".to_owned(), |s| s.describe()))),
    ]);
    if let (Some(s), Some(set)) = (&spec, &set) {
        ov.attrs.0.extend(reg::region_attrs(s, set, None).0);
    }
    if v.dtype == DType::F32 {
        ov.missval = format!("{}", v.missval as f32).parse().unwrap_or(v.missval);
    }
    ov.dtype = DType::F64;
    fv.out_dtype = DType::F64;

    let map = MocMap {
        mesh: mesh.clone(),
        on_nodes,
        elems,
        last_line,
        lines,
        level: (ldim, levels),
        scale,
        tables: OnceLock::new(),
        info,
    };
    Ok(Desc {
        attrs: input.attrs.clone(),
        vars: vec![ov],
        grids: vec![rg],
        zaxes: input.zaxes.clone(),
        time: input.time.clone(),
        method: vec![note],
        fold: Some(Fold {
            input: Box::new(input),
            kernel: Arc::new(FieldKernel {
                map: Arc::new(map),
                vars: vec![fv],
            }),
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines() {
        let a = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let (l, r) = parse_lines(&a(&["dlat=45", "regions=moc"])).unwrap();
        assert_eq!(l, vec![-90.0, -45.0, 0.0, 45.0, 90.0]);
        assert_eq!(r, a(&["regions=moc"]));
        assert_eq!(parse_lines(&a(&["lat=30/26.5/30"])).unwrap().0, vec![26.5, 30.0]);
        assert_eq!(parse_lines(&a(&[])).unwrap().0.len(), 181);
        assert_eq!(parse_lines(&a(&["dlat=7"])).unwrap().0.last(), Some(&85.0));
        assert!(parse_lines(&a(&["lat=91"])).is_err());
        assert!(parse_lines(&a(&["dlat=0"])).is_err());
        assert!(parse_lines(&a(&["dlat=1", "lat=3"])).is_err());
    }
}
