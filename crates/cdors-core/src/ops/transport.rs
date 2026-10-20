//! `transport` (cdors operator, not in cdo): the transport of a horizontal vector field across a
//! section of a model mesh, per layer.
//!
//! `transport,<lon0>,<lat0>,<lon1>,<lat1>[,<lon>,<lat>...]` or `transport,file=<GeoJSON>`, two
//! inputs: the eastward and the northward component (FESOM2 `u`, `v` on the elements and the
//! layers `nz1`):
//! - the path is a sequence of great-circle arcs; each element it crosses contributes the flux
//!   across the broken line from the midpoint of the edge where the path enters it, through its
//!   centre, to the midpoint of the edge where it leaves ([`cdors_mesh::section`]: the faces of
//!   FESOM2's median-dual cells, as pyfesom2's `cross_section_transport`);
//! - T(k) = Σ (u_e·(−ΔN_e) + v_e·ΔE_e)·Δz_k over the crossed elements with water in layer k,
//!   (ΔE, ΔN) the line's east and north extent at the element's centre on the sphere of the
//!   model's radius (FESOM2 `r_earth`), Δz_k the layer thickness (|zbar| differences): positive
//!   from the right of the path to its left (northward across a west-to-east path, eastward
//!   across a north-to-south one). In Sv for m/s, else Σ in `<units> m2`;
//! - elements where the path starts or ends (an end in the ocean) are left out, with the warning
//!   `section_open_ends`;
//! - output `transport(time, nz1, lat, lon)`: one point (the path's middle), the input's layers;
//!   a layer no crossed element has water in is missing. `-vertsum` gives the total.
//!
//! **How.** The two components are folded as two variables of one stage ([`FieldKernel`]: each
//! component's part of the flux, adding the elements in path order), kept as an intermediate,
//! and added (`plan::intermediate`).

use super::remap::{FieldKernel, FieldLane, FieldMap, FieldVar};
use crate::chain::OpNode;
use crate::error::{Error, ErrorCode, Result};
use crate::io::Values;
use crate::model::{AttrValue, DType};
use crate::plan::{BinOp, Desc, Expr, Fold, IndexMap, Sources, VarDesc};
use cdors_mesh::models::{self, DimRule};
use cdors_mesh::{sample, section};
use std::ops::Range;
use std::sync::Arc;

/// The incidence table of the elements (rows: elements, columns: their nodes).
const TABLE: &str = "elem_nodes";

pub fn handles(name: &str) -> bool {
    name == "transport"
}

fn usage() -> &'static str {
    "usage: transport,<lon0>,<lat0>,<lon1>,<lat1>[,<lon>,<lat>...] | transport,file=<GeoJSON \
     LineString> -setgrid,<mesh> -selname,u <in> -setgrid,<mesh> -selname,v <in>"
}

fn bad(msg: impl Into<String>) -> Error {
    Error::bad_arguments(msg)
        .with("operator", "transport".to_owned())
        .with_hint(usage())
}

/// The waypoints of the arguments (numbers, or file=).
fn parse_path(args: &[String]) -> Result<Vec<(f64, f64)>> {
    let (mut nums, mut file) = (Vec::new(), None);
    for a in args.iter().map(|a| a.trim()) {
        if let Some(f) = a.strip_prefix("file=") {
            file = Some(f.to_owned());
        } else if let Ok(x) = a.parse::<f64>() {
            nums.push(x);
        } else {
            return Err(bad(format!("transport: unexpected argument '{a}'")));
        }
    }
    match (file, nums.is_empty()) {
        (Some(_), false) => Err(bad("transport: waypoints and file= together; one path per call")),
        (Some(f), true) => {
            let bytes = std::fs::read(&f).map_err(|e| {
                Error::new(ErrorCode::IoError, format!("transport: cannot read {f}: {e}"))
                    .with("operator", "transport".to_owned())
            })?;
            sample::parse_geojson_path(&bytes).map_err(|e| bad(format!("transport: {f}: {e}")))
        }
        (None, _) => sample::waypoints(&nums).map_err(|e| bad(format!("transport: {e}"))),
    }
}

struct TransportMap {
    /// The crossed elements in path order (their index among the cells read), their layers with
    /// water, and per component the coefficient of its value (m): u with −ΔN, v with ΔE.
    elems: Vec<u32>,
    wet: Vec<Range<usize>>,
    coef: [Vec<f64>; 2],
    /// Thickness of each mesh layer (m).
    dz: Vec<f64>,
    /// The output dimension of the layers and the mesh layer of each index along it.
    level: (usize, IndexMap),
    scale: f64,
    info: serde_json::Value,
}

impl TransportMap {
    fn field<T: Copy + Into<f64>>(&self, var: usize, x: &[T], k: usize) -> f64 {
        let (mut s, mut any) = (0.0, false);
        for (i, &e) in self.elems.iter().enumerate() {
            if !self.wet[i].contains(&k) {
                continue;
            }
            let v: f64 = x[e as usize].into();
            if v.is_finite() {
                s += self.coef[var][i] * v;
                any = true;
            }
        }
        if any { s * self.dz[k] * self.scale } else { f64::NAN }
    }
}

impl FieldMap for TransportMap {
    fn apply(
        &self,
        var: usize,
        src: &Values,
        n: usize,
        out: DType,
        lane: &FieldLane,
    ) -> Result<Values> {
        let levels: Vec<usize> = lane
            .index(self.level.0)
            .into_iter()
            .map(|j| self.level.1.get(j))
            .collect();
        let size = src.len() / n.max(1);
        let v: Vec<f64> = (0..n)
            .map(|f| match src {
                Values::F32(x) => self.field(var, &x[f * size..(f + 1) * size], levels[f]),
                Values::F64(x) => self.field(var, &x[f * size..(f + 1) * size], levels[f]),
            })
            .collect();
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
    Error::new(ErrorCode::UnsupportedGrid, format!("transport of '{var}': {what}"))
        .with("operator", "transport".to_owned())
        .with("variable", var.to_owned())
        .with_hint(
            "transport integrates a velocity on the elements and layers of a model mesh: attach \
             the mesh to both inputs (-setgrid,fesom:<dir>) and select the eastward and the \
             northward component (FESOM2: u and v on elem and nz1), without a horizontal \
             selection",
        )
}

/// Whether `units` is a velocity in m/s.
fn is_m_per_s(units: &str) -> bool {
    matches!(
        units.trim(),
        "m/s" | "m s-1" | "m s**-1" | "m.s-1" | "m s^-1" | "m sec-1" | "m/sec"
    )
}

/// The one variable of a component input.
fn component(input: &Desc, which: &str) -> Result<VarDesc> {
    let [v] = input.vars.as_slice() else {
        return Err(Error::bad_arguments(format!(
            "transport takes one variable per input; the {which} input has {} ({})",
            input.vars.len(),
            input.vars.iter().map(|v| v.name.as_str()).collect::<Vec<_>>().join(", ")
        ))
        .with("operator", "transport".to_owned())
        .with_hint("the first input is the eastward component, the second the northward: \
                    -selname,u <in> -selname,v <in>"));
    };
    Ok(v.clone())
}

/// Output description of `transport`.
pub fn describe(node: &OpNode, mut inputs: Vec<Desc>, srcs: &mut Sources) -> Result<Desc> {
    let op = "transport";
    let (Some(dv), Some(du)) = (inputs.pop(), inputs.pop()) else {
        return Err(Error::internal("transport without two inputs"));
    };
    super::remap::no_pending_fold(op, &du)?;
    super::remap::no_pending_fold(op, &dv)?;
    let waypoints = parse_path(&node.args)?;
    let (u, mut v) = (component(&du, "first")?, component(&dv, "second")?);

    // both components on the whole element place of one mesh, on its layers
    let Some(gi) = u.grid else {
        return Err(not_on_mesh(&u.name, "no horizontal grid".into()));
    };
    let g = &du.grids[gi];
    let (Some(mesh), Some(place)) = (g.base.mesh.clone(), g.base.mesh_place()) else {
        return Err(not_on_mesh(&u.name, "the grid is not a model mesh".into()));
    };
    let Some(t) = mesh.table(TABLE) else {
        return Err(not_on_mesh(&u.name, format!("the mesh {} has no {TABLE} table", mesh.name)));
    };
    if place.name != t.rows_place {
        return Err(not_on_mesh(
            &u.name,
            format!("it is on the {} of the mesh {}, not on its {}", place.name, mesh.name, t.rows_place),
        ));
    }
    if g.size() != place.size || !g.sel[0].is_identity(place.size) {
        return Err(not_on_mesh(
            &u.name,
            format!("it covers {} of the {} {}", g.size(), place.size, place.name),
        ));
    }
    let layers = |x: &VarDesc| {
        x.expr
            .wet_mask()
            .filter(|(m, _, _)| m.rule == DimRule::Mid && m.place == place.name && m.mesh.name == mesh.name)
    };
    let Some((_, ldim, levels)) = layers(&u) else {
        return Err(not_on_mesh(&u.name, "its levels are not the mesh's layers (FESOM2: nz1)".into()));
    };
    let vg = v.grid.map(|i| &dv.grids[i]);
    let same_grid = vg.is_some_and(|h| {
        h.base.mesh.as_ref().is_some_and(|m| m.name == mesh.name)
            && h.base.mesh_place().is_some_and(|p| p.name == place.name)
            && h.size() == g.size()
            && h.sel[0].is_identity(place.size)
    });
    let same_levels = layers(&v).is_some_and(|(_, d, l)| {
        d == ldim && l.len() == levels.len() && (0..l.len()).all(|j| l.get(j) == levels.get(j))
    });
    if !same_grid || !same_levels || v.shape() != u.shape() || dv.ntime() != du.ntime() {
        return Err(Error::bad_arguments(format!(
            "transport: the components '{}' {:?} and '{}' {:?} differ in grid, layers or time steps",
            u.name,
            u.shape(),
            v.name,
            v.shape()
        ))
        .with("operator", op.to_owned())
        .with_hint("give both components on the same mesh, layers and time steps"));
    }
    let units = u.attrs.get_str("units").unwrap_or("").trim().to_owned();
    let vunits = v.attrs.get_str("units").unwrap_or("").trim().to_owned();
    let (scale, out_units) = if is_m_per_s(&units) && is_m_per_s(&vunits) {
        (1e-6, "Sv".to_owned())
    } else if units == vunits {
        (1.0, if units.is_empty() { "m2".to_owned() } else { format!("{units} m2") })
    } else {
        return Err(bad(format!("transport: the components have units '{units}' and '{vunits}'")));
    };
    let vertical = mesh.vertical.as_ref();
    let (Some(z), Some(wet)) = (vertical, vertical.and_then(|z| z.wet(&place.name))) else {
        return Err(not_on_mesh(&u.name, format!("the mesh {} has no wet levels of its {}", mesh.name, place.name)));
    };
    let Some(radius) = models::radius_m(&mesh) else {
        return Err(not_on_mesh(&u.name, format!("the mesh {} has no planet radius", mesh.name)));
    };

    // the section
    let nodes = mesh
        .place(&t.cols_place)
        .ok_or_else(|| Error::internal("no node place"))?;
    let en = t.csr.get().map_err(|e| {
        Error::new(ErrorCode::BadData, format!("transport: mesh {}: {e}", mesh.name))
    })?;
    let sec = section::crossings(&nodes.lon, &nodes.lat, &en, &place.lon, &place.lat, &waypoints)
        .map_err(|e| bad(format!("transport: {e}")))?;
    if sec.crossings.is_empty() {
        return Err(bad(format!(
            "transport: the path crosses no element of the mesh {} ({} element(s) where it starts \
             or ends left out)",
            mesh.name, sec.open_ends
        )));
    }
    let token = crate::plan::explain::op_token(node);
    if sec.open_ends > 0 {
        crate::exec::threads::warn_with(
            "section_open_ends",
            &format!(
                "transport: the path starts or ends in the ocean; {} element(s) where it does are \
                 left out (put both ends on land for the whole flow)",
                sec.open_ends
            ),
            serde_json::Map::new(),
        );
        srcs.warnings.push(serde_json::json!({
            "warning": "section_open_ends", "operator": token, "elements": sec.open_ends,
            "message": "the path starts or ends in the ocean; the elements where it does are left out",
        }));
    }
    // only the crossed elements are read (their chunks), renumbered in ascending order
    let mut keep: Vec<usize> = sec.crossings.iter().map(|c| c.elem as usize).collect();
    keep.sort_unstable();
    keep.dedup();
    let at = |e: u32| keep.binary_search(&(e as usize)).expect("kept") as u32;
    let elems: Vec<u32> = sec.crossings.iter().map(|c| at(c.elem)).collect();
    let coef = [
        sec.crossings.iter().map(|c| -c.north * radius).collect::<Vec<_>>(),
        sec.crossings.iter().map(|c| c.east * radius).collect::<Vec<_>>(),
    ];
    let wet_r: Vec<Range<usize>> = sec.crossings.iter().map(|c| wet.range(c.elem as usize)).collect();
    let deepest = wet_r.iter().map(|r| r.end).max().unwrap_or(0);
    let path_text = waypoints
        .iter()
        .map(|(x, y)| format!("{x},{y}"))
        .collect::<Vec<_>>()
        .join(" ");
    let length_km = sec.length * radius / 1e3;
    let centres = models::place_method(&mesh, &place.name).map_or("", |(c, _)| c);
    let mut note = serde_json::json!({
        "operator": token,
        "transport": format!(
            "T(k) = sum over the elements the path crosses of (u_e * -dN_e + v_e * dE_e) * dz_k \
             ({out_units}{}): the flux across the broken line from the midpoint of the edge where \
             the path enters each element, through its centre, to the midpoint of the edge where \
             it leaves (the faces of FESOM2's median-dual cells, as pyfesom2's \
             cross_section_transport); (dE, dN) the line's east and north extent at the element's \
             centre",
            if scale != 1.0 { ", 1 Sv = 1e6 m3/s" } else { "" }
        ),
        "sign": "positive from the right of the path to its left: northward across a west-to-east \
                 path, eastward across a north-to-south one",
        "components": format!(
            "'{}' eastward, '{}' northward, as stored (velocities on a rotated grid must be \
             rotated back first)",
            u.name, v.name
        ),
        "path": format!("great-circle arcs through {path_text}, {length_km:.1} km on a sphere of radius {radius} m (the model's)"),
        "elements": format!("{} of the mesh {} ({centres} as centres), down to layer {deepest}", sec.crossings.len(), mesh.name),
        "layers": "dz from the mesh's interfaces (|zbar| differences), full cells, each element's wet layers",
        "missing": "a layer no crossed element has water in",
    });
    if sec.open_ends > 0 {
        note["open_ends"] = format!(
            "{} element(s) where the path starts or ends in the ocean, left out",
            sec.open_ends
        )
        .into();
    }
    let info = serde_json::json!({ "mesh_weights": [{
        "operator": token, "weights": "section faces", "mesh": mesh.name,
        "elements": elems.len(), "bytes": 36 * elems.len() as u64,
    }]});

    // one stage folds both components; the intermediate is then added
    let mid = waypoints.len() / 2;
    let (mlon, mlat) = if waypoints.len() % 2 == 1 {
        waypoints[mid]
    } else {
        cdors_mesh::geom::interpolate(waypoints[mid - 1], waypoints[mid], 0.5)
    };
    let (mut rg, hdims) = super::fldstat::made_grid(g, vec![mlat], None, None);
    rg.base.xvals = Some(vec![mlon]);
    if let Some(f) = rg.fixed.as_mut() {
        Arc::make_mut(f).coords.xvals = vec![mlon];
    }
    v.grid = u.grid;
    v.zaxis = u.zaxis;
    let sel = IndexMap::from_list(keep.clone());
    let mut grids = du.grids.clone();
    grids[gi].sel[0] = grids[gi].sel[0].compose(&sel);
    let mut u = u;
    for x in [&mut u, &mut v] {
        let hd = x.hdims();
        x.select(hd[0], &sel);
    }
    let text = |s: &str| AttrValue::Text(s.to_owned());
    let mut fvs = Vec::new();
    let mut outs = Vec::new();
    for (x, name) in [(&u, "transport_east"), (&v, "transport_north")] {
        let mut fv = FieldVar::new(op, x, keep.len(), vec![1, 1])?;
        fv.out_dtype = DType::F64;
        let mut o = x.clone();
        o.dims.splice(fv.hfirst..fv.hfirst + fv.hcount, hdims.clone());
        o.name = name.to_owned();
        o.grid = Some(0);
        o.dtype = DType::F64;
        o.attrs.0.retain(|(k, _)| {
            !matches!(
                k.as_str(),
                "location" | "coordinates" | "CDI_grid_type" | "grid_mapping" | "mesh"
                    | "description" | "long_name" | "standard_name" | "units"
            )
        });
        fvs.push(fv);
        outs.push(o);
    }
    let merged = Desc {
        attrs: du.attrs.clone(),
        vars: vec![u.clone(), v.clone()],
        grids,
        zaxes: du.zaxes.clone(),
        time: du.time.clone(),
        fold: None,
        method: Vec::new(),
    };
    let map = TransportMap {
        elems,
        wet: wet_r,
        coef,
        dz: z.layer_thickness(),
        level: (ldim, levels),
        scale,
        info,
    };
    let folded = Desc {
        attrs: du.attrs.clone(),
        vars: outs,
        grids: vec![rg],
        zaxes: du.zaxes.clone(),
        time: du.time.clone(),
        method: Vec::new(),
        fold: Some(Fold {
            input: Box::new(merged),
            kernel: Arc::new(FieldKernel {
                map: Arc::new(map),
                vars: fvs,
            }),
        }),
    };
    let mut d = crate::plan::intermediate::materialize(folded, op, srcs)?;
    let (east, north) = (d.vars[0].clone(), d.vars[1].clone());
    let mut ov = east.clone();
    ov.name = "transport".to_owned();
    ov.expr = Expr::Binary(BinOp::Add, DType::F64, Box::new(east.expr), Box::new(north.expr));
    let missval = if u.dtype == DType::F32 {
        format!("{}", u.missval as f32).parse().unwrap_or(u.missval)
    } else {
        u.missval
    };
    ov.missval = missval;
    ov.attrs.0.extend([
        (
            "long_name".to_owned(),
            text(if scale != 1.0 { "volume transport across the section" } else { "transport across the section" }),
        ),
        ("units".to_owned(), text(&out_units)),
        ("transport_inputs".to_owned(), text(&format!("{} {}", u.name, v.name))),
        ("transport_path".to_owned(), text(&path_text)),
        ("transport_sign".to_owned(), text("positive from the right of the path to its left")),
    ]);
    d.vars = vec![ov];
    d.method = vec![note];
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path() {
        let a = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(parse_path(&a(&["-65", "-55", "-60", "-65"])).unwrap(), vec![(-65.0, -55.0), (-60.0, -65.0)]);
        assert!(parse_path(&a(&["-65", "-55"])).is_err());
        assert!(parse_path(&a(&["-65", "-55", "-60", "x"])).is_err());
        assert!(parse_path(&a(&["file=a.json", "1", "2", "3", "4"])).is_err());
    }
}
