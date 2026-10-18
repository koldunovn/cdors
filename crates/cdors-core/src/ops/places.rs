//! `toelems` and `tonodes` (cdors operators, not in cdo): a variable on one place of a model mesh
//! mapped to the other through the mesh's `elem_nodes` table.
//!
//! - `toelems`: each element gets the mean of its three nodes;
//! - `tonodes`: each node gets the mean of the elements around it, weighted by their areas
//!   (FESOM2 `elem_area`).
//!
//! Missing values are left out as in remapping (the weights of the valid links renormalised;
//! missing without any), per level; levels outside the wet range of a target cell are missing.
//! The output grid is the target place of the same mesh, so field statistics and other mesh
//! operators can follow. The matrix runs on the remap executor
//! ([`RemapWeights::from_csr`], `remap::FieldKernel`), built on first use.

use super::remap::{FieldKernel, FieldLane, FieldMap, FieldVar, apply_t};
use crate::chain::OpNode;
use crate::error::{Error, ErrorCode, Result};
use crate::io::Values;
use crate::model::{DType, DimRole, VarDim};
use crate::plan::{Desc, Fold, IndexMap};
use crate::remap::{MissingRule, RemapWeights};
use cdors_mesh::models::{self, DimRule};
use cdors_mesh::{Csr, Mesh};
use std::sync::{Arc, OnceLock};

/// Operators handled here.
pub fn handles(name: &str) -> bool {
    matches!(name, "toelems" | "tonodes")
}

/// The incidence table both operators use (rows: elements, columns: their nodes).
const TABLE: &str = "elem_nodes";

struct PlaceMap {
    mesh: Arc<Mesh>,
    to_rows: bool,
    /// Target place.
    target: String,
    weights: OnceLock<std::result::Result<Arc<RemapWeights>, String>>,
    /// Per variable with levels: its level dimension, the mesh level of each index along it, and
    /// mid-levels or interfaces.
    dry: Vec<Option<(usize, IndexMap, DimRule)>>,
    /// `--plan`: rows, links and bytes of the matrix.
    shape: (usize, usize),
    op: String,
}

impl PlaceMap {
    fn weights(&self) -> Result<Arc<RemapWeights>> {
        self.weights
            .get_or_init(|| self.build().map(Arc::new).map_err(|e| e.to_string()))
            .clone()
            .map_err(|m| Error::new(ErrorCode::BadData, format!("{}: {m}", self.op)))
    }

    /// toelems: `elem_nodes` with weight 1/3 per node; tonodes: its transpose with each
    /// element's area over the area of the elements around the node.
    fn build(&self) -> std::result::Result<RemapWeights, String> {
        let t = self.mesh.table(TABLE).ok_or("no elem_nodes table")?;
        let en = t.csr.get().map_err(|e| e.to_string())?;
        let csr = if self.to_rows {
            let w = (0..en.rows())
                .flat_map(|r| {
                    let n = en.range(r).len();
                    std::iter::repeat_n(1.0 / n as f64, n)
                })
                .collect();
            Csr::new(en.row_ptr.clone(), en.col.clone(), Some(w), en.ncols)
        } else {
            let area = &self
                .mesh
                .place(&t.rows_place)
                .ok_or("no element place")?
                .area;
            let ne = en.transpose();
            let mut w = Vec::with_capacity(ne.nnz());
            for r in 0..ne.rows() {
                let total: f64 = ne.row(r).iter().map(|&e| area[e as usize]).sum();
                w.extend(ne.row(r).iter().map(|&e| area[e as usize] / total));
            }
            Csr::new(ne.row_ptr, ne.col, Some(w), ne.ncols)
        }
        .map_err(|e| e.to_string())?;
        RemapWeights::from_csr(&csr, MissingRule::Renormalise).map_err(|e| e.to_string())
    }
}

/// Sets the values of every field at levels outside the wet range of their cell to NaN.
fn mask_fields<T: Copy>(
    v: &mut [T],
    nan: T,
    size: usize,
    levels: &[usize],
    wet: &cdors_mesh::WetLevels,
    rule: DimRule,
) {
    for (f, field) in v.chunks_mut(size).enumerate() {
        let k = levels[f];
        for (c, x) in field.iter_mut().enumerate() {
            if !models::wet_range(wet, rule, c).contains(&k) {
                *x = nan;
            }
        }
    }
}

impl FieldMap for PlaceMap {
    fn apply(
        &self,
        var: usize,
        src: &Values,
        n: usize,
        out: DType,
        lane: &FieldLane,
    ) -> Result<Values> {
        let w = self.weights()?;
        let r = match (src, out) {
            (Values::F32(s), DType::F32) => apply_t::<f32, f32>(&w, s, n, Values::F32),
            (Values::F32(s), _) => apply_t::<f32, f64>(&w, s, n, Values::F64),
            (Values::F64(s), DType::F32) => apply_t::<f64, f32>(&w, s, n, Values::F32),
            (Values::F64(s), _) => apply_t::<f64, f64>(&w, s, n, Values::F64),
        };
        let mut v = r.map_err(|e| Error::new(ErrorCode::BadData, format!("{}: {e}", self.op)))?;
        let wet = self
            .mesh
            .vertical
            .as_ref()
            .and_then(|z| z.wet(&self.target));
        if let (Some((d, map, rule)), Some(wet)) = (&self.dry[var], wet) {
            let levels: Vec<usize> = lane.index(*d).into_iter().map(|j| map.get(j)).collect();
            let size = w.dst_size();
            match &mut v {
                Values::F32(x) => mask_fields(x, f32::NAN, size, &levels, wet, *rule),
                Values::F64(x) => mask_fields(x, f64::NAN, size, &levels, wet, *rule),
            }
        }
        Ok(v)
    }

    fn plan_info(&self) -> Option<serde_json::Value> {
        let (rows, links) = self.shape;
        Some(serde_json::json!({ "mesh_weights": [{
            "operator": self.op,
            "weights": TABLE,
            "mesh": self.mesh.name,
            "target": self.target,
            "rows": rows,
            "links": links,
            "bytes": cdors_mesh::mesh::csr_bytes(rows, links, true),
        }]}))
    }
}

fn not_on_mesh(op: &str, var: &str, what: String) -> Error {
    Error::new(
        ErrorCode::UnsupportedGrid,
        format!("{op} of '{var}': {what}"),
    )
    .with("operator", op.to_owned())
    .with("variable", var.to_owned())
    .with_hint(format!(
        "{op} maps the variables of a model mesh between nodes and elements: attach the mesh \
         first (-setgrid,fesom:<dir>) and select the variables of the other place with -selname, \
         before any horizontal selection"
    ))
}

/// Output description of `toelems` / `tonodes`.
pub fn describe(node: &OpNode, mut inputs: Vec<Desc>) -> Result<Desc> {
    let op = node.name.as_str();
    let to_rows = op == "toelems";
    let input = inputs
        .pop()
        .ok_or_else(|| Error::internal("place map without input"))?;
    super::remap::no_pending_fold(op, &input)?;
    let mut grid: Option<(usize, Arc<Mesh>)> = None;
    for v in &input.vars {
        let Some(gi) = v.grid else {
            return Err(not_on_mesh(op, &v.name, "no horizontal grid".into()));
        };
        let g = &input.grids[gi];
        let (Some(mesh), Some(place)) = (g.base.mesh.as_ref(), g.base.mesh_place()) else {
            return Err(not_on_mesh(
                op,
                &v.name,
                "the grid is not a model mesh".into(),
            ));
        };
        let Some(t) = mesh.table(TABLE) else {
            return Err(not_on_mesh(
                op,
                &v.name,
                format!("the mesh {} has no {TABLE} table", mesh.name),
            ));
        };
        let from = if to_rows {
            &t.cols_place
        } else {
            &t.rows_place
        };
        if &place.name != from {
            return Err(not_on_mesh(
                op,
                &v.name,
                format!(
                    "it is on the {} of the mesh {}, not on its {from}",
                    place.name, mesh.name
                ),
            ));
        }
        if g.size() != place.size || !g.sel[0].is_identity(place.size) {
            return Err(not_on_mesh(
                op,
                &v.name,
                format!("it covers {} of the {} {from}", g.size(), place.size),
            ));
        }
        match &grid {
            Some((g0, _)) if *g0 != gi => {
                return Err(not_on_mesh(
                    op,
                    &v.name,
                    "variables on several grids".into(),
                ));
            }
            Some(_) => {}
            None => grid = Some((gi, mesh.clone())),
        }
    }
    let Some((gi, mesh)) = grid else {
        return Err(Error::bad_arguments(format!("{op}: no variables")));
    };
    let g = &input.grids[gi];
    let t = mesh.table(TABLE).expect("checked");
    let target = if to_rows {
        &t.rows_place
    } else {
        &t.cols_place
    };
    let tp = mesh
        .place(target)
        .ok_or_else(|| Error::internal(format!("no place '{target}'")))?;
    // the target's dimension as the model writes it (FESOM2: elem, nod2)
    let dim = models::model(&mesh.source.model)
        .and_then(|m| {
            m.dims
                .iter()
                .find(|(_, r)| matches!(r, DimRule::Place(p) if *p == tp.name))
        })
        .map_or_else(|| tp.name.clone(), |(d, _)| (*d).to_owned());
    let tgrid = super::files::mesh_grid(&g.src.dataset().source, &mesh, tp, &dim)?;
    let mut fvars = Vec::new();
    let mut out_vars = Vec::new();
    let mut dry = Vec::new();
    for v in &input.vars {
        let fv = FieldVar::new(op, v, g.size(), vec![tp.size])?;
        let (hfirst, hcount) = (fv.hfirst, fv.hcount);
        fvars.push(fv);
        dry.push(v.expr.wet_mask().map(|(m, d, levels)| (d, levels, m.rule)));
        let mut ov = v.clone();
        ov.dims.splice(
            hfirst..hfirst + hcount,
            [VarDim {
                name: dim.clone(),
                size: tp.size,
                role: DimRole::Horizontal,
            }],
        );
        ov.grid = Some(0);
        // a model's own note of the place (FESOM2 `location = "node"`) would be wrong now
        ov.attrs.0.retain(|(k, _)| k != "location");
        out_vars.push(ov);
    }
    // links without loading the table: its size is known (row pointers, 4 bytes per column)
    let erows = mesh.place(&t.rows_place).map_or(0, |p| p.size);
    let links = (t.csr.bytes() as usize).saturating_sub(8 * (erows + 1)) / 4;
    let map = PlaceMap {
        to_rows,
        target: tp.name.clone(),
        weights: OnceLock::new(),
        dry,
        shape: (tp.size, links),
        op: op.to_owned(),
        mesh: mesh.clone(),
    };
    Ok(Desc {
        attrs: input.attrs.clone(),
        vars: out_vars,
        grids: vec![tgrid],
        zaxes: input.zaxes.clone(),
        time: input.time.clone(),
        fold: Some(Fold {
            input: Box::new(input),
            kernel: Arc::new(FieldKernel {
                map: Arc::new(map),
                vars: fvars,
            }),
        }),
    })
}
