//! Region statistics (cdors operators, not in cdo): `regmin`, `regmax`, `regsum`, `regint`,
//! `regmean`, `regstd`, the `fld*` statistics over the cells of each region of a catalog set.
//!
//! `reg<stat>,regions=<set>[,select=<abbrev,...>][,fill=nearest]`:
//! - membership as `regmask` ([`super::regions`]): one row of member cells per region, cells
//!   ascending; overlapping sets count a cell in every region holding it;
//! - `regsum` is unweighted (as `fldsum`); `regint` is Σ area·value (m², as `fldint`);
//!   `regmean` and `regstd` are weighted by cell area (as `fldmean`, `fldstd`); `regmin` and
//!   `regmax` are unweighted. Missing values are left out; a region without valid cells (or
//!   without cells at all: warning `regions_empty`) is missing;
//! - on a model mesh, a variable with the mesh's levels is weighted at each level by that
//!   level's areas (FESOM2 `nod_area[k]`; elements: `elem_area`), the membership table scaled at
//!   apply time (no matrix per level); dry levels are already missing in the tiles;
//! - the horizontal dimensions become the dimension `region`, with the coordinate `region` (the
//!   region numbers) and the string variables `region_name`, `region_abbrev`; the set's source,
//!   licence and attribution are attributes of every output variable.
//!
//! **Fold order.** The executor's field kernel ([`FieldKernel`]) collects complete fields; each
//! region's cells are then added in ascending order, so results do not depend on chunks or
//! threads.

use super::fldstat::{self, Acc, MeanAcc, MinMaxAcc, Stat, SumAcc, VarAcc};
use super::regions::{self as reg, Spec};
use super::remap::{FieldKernel, FieldLane, FieldMap, FieldVar};
use crate::chain::OpNode;
use crate::error::{Error, ErrorCode, Result};
use crate::io::Values;
use crate::model::{AttrValue, Attrs, CoordAxis, DType, DimRole, Grid, GridKind, VarDim};
use crate::plan::{
    Desc, FixedGrid, Fold, GridCoords, GridDesc, IndexMap, Labels, Sources, VarDesc,
};
use cdors_mesh::{Csr, Mesh};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

/// Operators of this module with their descriptions.
pub fn operators() -> Vec<(String, String)> {
    [
        ("regmin", "Minimum"),
        ("regmax", "Maximum"),
        ("regsum", "Sum (unweighted)"),
        ("regint", "Integral: sum of area times value (m2)"),
        ("regmean", "Area-weighted mean"),
        ("regstd", "Area-weighted standard deviation"),
    ]
    .into_iter()
    .map(|(n, d)| {
        (
            n.to_owned(),
            format!(
                "{d} over the cells of each region: regions=<set>[,select=<abbrev,...>]\
                 [,fill=nearest] (sets as regmask); output dimension region (cdors)"
            ),
        )
    })
    .collect()
}

pub fn handles(name: &str) -> bool {
    stat_of(name).is_some()
}

fn stat_of(name: &str) -> Option<Stat> {
    Some(match name {
        "regmin" => Stat::Min,
        "regmax" => Stat::Max,
        "regsum" => Stat::Sum,
        "regint" => Stat::Int,
        "regmean" => Stat::Mean,
        "regstd" => Stat::Std,
        _ => return None,
    })
}

/// The weight of each cell of a variable's fields.
enum CellWeights {
    /// Every cell weighs 1 (`regsum`, `regmin`, `regmax`).
    One,
    /// One area per cell (2-D).
    Each(Arc<Vec<f64>>),
    /// At the field's level of dimension `dim`, the areas of mesh level `levels[j]` of the
    /// place, picked by `cells` (`None`: the whole place in order); loaded on first use.
    PerLevel {
        dim: usize,
        levels: IndexMap,
        mesh: Arc<Mesh>,
        place: String,
        cells: Option<Arc<Vec<usize>>>,
        loaded: Vec<OnceLock<std::result::Result<Arc<Vec<f64>>, String>>>,
    },
}

impl CellWeights {
    fn level(&self, j: usize) -> Result<Arc<Vec<f64>>> {
        let CellWeights::PerLevel {
            levels,
            mesh,
            place,
            cells,
            loaded,
            ..
        } = self
        else {
            return Err(Error::internal("not per-level weights"));
        };
        loaded[j]
            .get_or_init(|| {
                let k = levels.get(j);
                let lv = mesh
                    .place(place)
                    .and_then(|p| p.area_per_level.as_ref())
                    .ok_or_else(|| format!("no per-level areas of '{place}'"))?;
                let all = lv.level(k).map_err(|e| e.to_string())?;
                Ok(match cells {
                    None => all,
                    Some(c) => Arc::new(c.iter().map(|&i| all[i]).collect()),
                })
            })
            .clone()
            .map_err(|m| Error::new(ErrorCode::BadData, format!("mesh {}: {m}", mesh.name)))
    }
}

/// One variable: its region table and cell weights.
struct RegVar {
    rows: Arc<Csr>,
    weights: CellWeights,
}

struct RegionMap {
    stat: Stat,
    vars: Vec<RegVar>,
    info: serde_json::Value,
}

/// Reduces each region of each field: `fields` holds `n` fields of `size` cells.
fn reduce<A: Acc, T: Copy + Into<f64>>(
    stat: Stat,
    rows: &Csr,
    fields: &[T],
    n: usize,
    size: usize,
    weight: &dyn Fn(usize) -> Result<Option<Arc<Vec<f64>>>>,
) -> Result<Vec<f64>> {
    let nr = rows.rows();
    let mut out = vec![f64::NAN; n * nr];
    for f in 0..n {
        let field = &fields[f * size..(f + 1) * size];
        let w = weight(f)?;
        for r in 0..nr {
            let mut acc = A::new();
            for &c in rows.row(r) {
                let c = c as usize;
                acc.add(w.as_ref().map_or(1.0, |w| w[c]), field[c].into());
            }
            out[f * nr + r] = acc.value(stat);
        }
    }
    Ok(out)
}

impl FieldMap for RegionMap {
    fn apply(
        &self,
        var: usize,
        src: &Values,
        n: usize,
        out: DType,
        lane: &FieldLane,
    ) -> Result<Values> {
        let rv = &self.vars[var];
        let size = rv.rows.ncols;
        // the level of each field, for per-level weights
        let levels: Option<Vec<usize>> = match &rv.weights {
            CellWeights::PerLevel { dim, .. } => Some(lane.index(*dim)),
            _ => None,
        };
        let weight = |f: usize| -> Result<Option<Arc<Vec<f64>>>> {
            match (&rv.weights, &levels) {
                (CellWeights::One, _) => Ok(None),
                (CellWeights::Each(w), _) => Ok(Some(w.clone())),
                (w @ CellWeights::PerLevel { .. }, Some(l)) => w.level(l[f]).map(Some),
                _ => Err(Error::internal("per-level weights without levels")),
            }
        };
        let st = self.stat;
        macro_rules! run {
            ($acc:ty) => {
                match src {
                    Values::F32(x) => reduce::<$acc, f32>(st, &rv.rows, x, n, size, &weight),
                    Values::F64(x) => reduce::<$acc, f64>(st, &rv.rows, x, n, size, &weight),
                }
            };
        }
        let v = match st {
            Stat::Min | Stat::Max => run!(MinMaxAcc),
            Stat::Sum | Stat::Int => run!(SumAcc),
            Stat::Mean => run!(MeanAcc),
            _ => run!(VarAcc),
        }?;
        Ok(match out {
            DType::F32 => Values::F32(v.into_iter().map(|x| x as f32).collect()),
            _ => Values::F64(v),
        })
    }

    fn plan_info(&self) -> Option<serde_json::Value> {
        Some(self.info.clone())
    }
}

/// The `region` axis: a one-axis grid with the region numbers, names and abbreviations.
fn region_grid(src: &GridDesc, set: &cdors_mesh::regions::RegionSet) -> GridDesc {
    let n = set.len();
    let mut base = Grid::new(GridKind::Generic, vec!["region".into()], n, 0);
    base.x = Some(CoordAxis {
        var: "region".into(),
        dim: "region".into(),
        long_name: Some("region number".into()),
        units: None,
        standard_name: None,
        is_f32: false,
        bounds_var: None,
    });
    let text = |s: &str| AttrValue::Text(s.to_owned());
    let label = |name: &str, values: &[String], long: &str| Labels {
        name: name.to_owned(),
        values: values.to_vec(),
        attrs: Attrs(vec![("long_name".into(), text(long))]),
    };
    let fixed = FixedGrid {
        coords: GridCoords {
            xvals: set.numbers.iter().map(|&k| k as f64).collect(),
            ..GridCoords::default()
        },
        xattrs: Attrs(vec![("long_name".into(), text("region number"))]),
        yattrs: Attrs::default(),
        labels: vec![
            label("region_name", &set.names, "region name"),
            label("region_abbrev", &set.abbrevs, "region abbreviation"),
        ],
        ..FixedGrid::default()
    };
    GridDesc {
        kind: GridKind::Generic,
        base,
        src: src.src.clone(),
        sel: vec![IndexMap::identity(n)],
        xvals: None,
        fixed: Some(Arc::new(fixed)),
    }
}

/// Weights of `v` on grid `g` for `stat`, and the bytes of mesh levels it will load.
fn weights(
    op: &str,
    stat: Stat,
    g: &GridDesc,
    v: &VarDesc,
    srcs: &Sources,
) -> Result<(CellWeights, u64)> {
    if matches!(stat, Stat::Min | Stat::Max | Stat::Sum) {
        return Ok((CellWeights::One, 0));
    }
    let has_levels = v.dims.iter().any(|d| d.role == DimRole::Vertical);
    if let (Some(mesh), Some(place)) = (g.base.mesh.clone(), g.base.mesh_place())
        && has_levels
        && mesh.vertical.is_some()
    {
        let Some((mask, dim, levels)) = v.expr.wet_mask() else {
            return Err(Error::new(
                ErrorCode::NotImplemented,
                format!(
                    "{op} of '{}' on the mesh {}: its levels are not the mesh's layers or \
                     interfaces, so the dry cells of each level are not known",
                    v.name, mesh.name
                ),
            )
            .with_hint("select the variable with the mesh's level dimension (FESOM2: nz1 or nz)"));
        };
        if mask.place == place.name && place.area_per_level.is_some() {
            let cells = reg::selected_cells(g);
            let whole = cells.len() == place.size && cells.iter().enumerate().all(|(k, &c)| k == c);
            let n = levels.len();
            // the mesh's rows of the levels, and for a subset the gathered copy of each
            let copy = if whole { 0 } else { n * cells.len() };
            let bytes = 8 * (n * place.size + copy) as u64;
            return Ok((
                CellWeights::PerLevel {
                    dim,
                    levels,
                    place: place.name.clone(),
                    cells: (!whole).then(|| Arc::new(cells)),
                    loaded: (0..n).map(|_| OnceLock::new()).collect(),
                    mesh,
                },
                bytes,
            ));
        }
    }
    // 2-D: the cell areas of fldint (the mesh's areas on a mesh)
    let (w, _) = fldstat::fld_weights(g, v, srcs, true).map_err(|e| e.with("operator", op))?;
    Ok(match w {
        fldstat::Weights::Each(a) => (CellWeights::Each(a), 0),
        fldstat::Weights::Const(c) => (CellWeights::Each(Arc::new(vec![c; g.size()])), 0),
        fldstat::Weights::PerLevel(_) => {
            return Err(Error::internal("per-level weights from fld_weights"));
        }
    })
}

/// Output description of a region statistic.
pub fn describe(node: &OpNode, mut inputs: Vec<Desc>, srcs: &mut Sources) -> Result<Desc> {
    let op = node.name.as_str();
    let stat = stat_of(op).ok_or_else(|| Error::internal(format!("'{op}' is not reg*")))?;
    let input = inputs.remove(0);
    super::remap::no_pending_fold(op, &input)?;
    let spec: Spec = reg::parse_spec(op, &node.args)?.ok_or_else(|| {
        Error::bad_arguments(format!("{op} needs regions=<set>"))
            .with("operator", op.to_owned())
            .with_hint(format!(
                "usage: {op},regions=<set>[,select=<abbrev,...>][,fill=nearest]"
            ))
    })?;
    let set = reg::open_set(op, &spec)?;
    // one membership table per input grid
    let mut tables: HashMap<usize, (Arc<Csr>, Option<usize>)> = HashMap::new();
    let mut out = input.clone();
    out.vars.clear();
    let rgi = out.grids.len();

    let mut fvars = Vec::new();
    let mut rvars = Vec::new();
    let mut minfo = Vec::new();
    for v in &input.vars {
        let Some(gi) = v.grid.filter(|&g| input.grids[g].size() > 1) else {
            return Err(Error::new(
                ErrorCode::UnsupportedDimension,
                format!(
                    "{op}: variable '{}' is not on a grid of more than one cell",
                    v.name
                ),
            )
            .with_hint("select the variables on a grid first (-selname)"));
        };
        let g = &input.grids[gi];
        let (rows, filled) = match tables.get(&gi) {
            Some(t) => t.clone(),
            None => {
                let (lon, lat) = reg::centres(op, g)?;
                let (m, filled) = reg::membership(op, &spec, &set, g, &lon, &lat, srcs);
                minfo.push(serde_json::json!({
                    "operator": op, "weights": "membership", "regions": spec.set,
                    "rows": m.rows(), "links": m.nnz(), "bytes": m.bytes(),
                }));
                let t = (Arc::new(m), filled);
                tables.insert(gi, t.clone());
                t
            }
        };

        let (w, bytes) = weights(op, stat, g, v, srcs)?;
        if bytes > 0 {
            minfo.push(serde_json::json!({
                "operator": op, "variable": v.name, "weights": "per_level",
                "rows": set.len(), "links": rows.nnz(), "bytes": bytes,
            }));
        }
        let mut fv = FieldVar::new(op, v, g.size(), vec![set.len()])?;
        let mut ov = v.clone();
        ov.dims.splice(
            fv.hfirst..fv.hfirst + fv.hcount,
            [VarDim {
                name: "region".into(),
                size: set.len(),
                role: DimRole::Horizontal,
            }],
        );
        ov.grid = Some(rgi);
        ov.attrs.0.retain(|(k, _)| {
            !matches!(
                k.as_str(),
                "location" | "coordinates" | "CDI_grid_type" | "grid_mapping"
            )
        });
        fldstat::add_cell_method(
            &mut ov,
            &format!(
                "area: {} where region",
                match stat {
                    Stat::Min => "minimum",
                    Stat::Max => "maximum",
                    Stat::Sum | Stat::Int => "sum",
                    Stat::Mean => "mean",
                    _ => "standard_deviation",
                }
            ),
        );
        ov.attrs.0.extend(reg::region_attrs(&spec, &set, filled).0);
        if stat == Stat::Int {
            fldstat::integral_output(&mut ov, "m2");
            fv.out_dtype = ov.dtype;
        }
        fvars.push(fv);
        rvars.push(RegVar {
            rows: rows.clone(),
            weights: w,
        });
        out.vars.push(ov);
    }
    if out.vars.is_empty() {
        return Err(Error::bad_arguments(format!("{op}: no variables")));
    }
    let rg = region_grid(&input.grids[input.vars[0].grid.expect("grid")], &set);
    out.grids.push(rg);
    let info = serde_json::json!({ "mesh_weights": minfo });
    let map = RegionMap {
        stat,
        vars: rvars,
        info,
    };
    Ok(Desc {
        fold: Some(Fold {
            input: Box::new(input),
            kernel: Arc::new(FieldKernel {
                map: Arc::new(map),
                vars: fvars,
            }),
        }),
        ..out
    })
}
