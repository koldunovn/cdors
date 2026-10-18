//! `fldhist`: area-weighted histograms over all horizontal cells (a cdors operator; cdo has
//! none).
//!
//! `fldhist,bins=<lo>/<hi>/<step>[,out=area|fraction]` or
//! `fldhist,edges=<e0>/<e1>/…/<en>[,out=area|fraction]`:
//! - bins are [e_i, e_{i+1}), the last one closed [e_{n−1}, e_n]; `bins=` gives the edges
//!   lo, lo + step, …, hi (hi − lo must be a whole number of steps);
//! - for every input variable `<var>` the output has three variables: `<var>(…, bin)`, the area
//!   of the cells whose value falls in each bin, and `<var>_below(…)`, `<var>_above(…)`, the
//!   area of the cells below e_0 and above e_n, so that nothing is dropped silently;
//! - `out=area` (default): areas in m², the cell areas of `fldint` (`gridcell_areas`: the
//!   file's cell-area variable, else the cell bounds × R²; a grid without either is an error);
//!   `out=fraction`: the same sums with the weights of `fldmean` (normalised areas; equal
//!   weights with the loud `equal_weights` warning where `fldmean` gives it) divided by the
//!   weight of all valid cells, below and above included;
//! - missing values are excluded; a bin without cells is 0, a fraction without any valid cell
//!   missing;
//! - the other dimensions stay (time, levels: one histogram per field, as `fld*`); the
//!   horizontal ones become the dimension `bin`, with the coordinate `bin` (bin centres; the
//!   variable's units if all variables share them) and its bounds `bin_bnds` (the edges).
//!
//! **Fold order.** As the `fld*` statistics ([`super::fldstat`]): the cells of each field are
//! added in the canonical order of the horizontal block, tiles of a row strip of a 2-D grid
//! buffered and folded row by row, so the sums do not depend on the chunk layout or the
//! threads.
//!
//! One fold per input variable: its lanes emit the tiles of all three output variables
//! ([`FoldKernel::outputs`]), so the input is read once.
//!
//! With a model mesh (Task 5 of the round-1 plan) the histogram is per level with per-level
//! areas: [`hist_weights`] is the only place that decides the weights, and its per-cell
//! [`Weights`] become per (level, cell) there, as for `fldmean`.

use super::fldstat::{self, Weights};
use crate::chain::OpNode;
use crate::error::{Error, ErrorCode, Result};
use crate::io::Values;
use crate::model::area::{self, AreaSource};
use crate::model::{AttrValue, Attrs, CoordAxis, DimRole, Grid, GridKind, VarDim};
use crate::plan::stage::{FoldKernel, FoldState, Tile};
use crate::plan::tiling::TileBox;
use crate::plan::{Desc, FixedGrid, Fold, GridCoords, GridDesc, IndexMap, Sources, VarDesc};
use std::collections::HashMap;
use std::sync::Arc;

/// Operators of this module with their descriptions.
pub fn operators() -> Vec<(String, String)> {
    vec![(
        "fldhist".into(),
        "Area-weighted histogram over all cells: bins=<lo>/<hi>/<step> or edges=<e0>/.../<en>, \
         out=area (m2, default) or fraction; <var>_below and <var>_above hold the rest (cdors)"
            .into(),
    )]
}

/// What the output variable holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
enum Part {
    Bins,
    Below,
    Above,
}

/// Parsed arguments.
struct Args {
    edges: Vec<f64>,
    fraction: bool,
}

fn parse_list(op: &str, key: &str, s: &str) -> Result<Vec<f64>> {
    s.split('/')
        .map(|x| {
            x.trim()
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite())
                .ok_or_else(|| {
                    Error::bad_arguments(format!("{op}: '{x}' in {key}={s} is not a number"))
                })
        })
        .collect()
}

fn parse_args(op: &str, args: &[String]) -> Result<Args> {
    let usage = "fldhist,bins=<lo>/<hi>/<step> or fldhist,edges=<e0>/<e1>/.../<en>, optionally \
                 with out=area (default, m2) or out=fraction";
    let bad = |m: String| {
        Error::bad_arguments(m)
            .with("operator", op.to_owned())
            .with_hint(usage)
    };
    let (mut edges, mut fraction) = (None, false);
    for a in args {
        let (k, v) = a
            .split_once('=')
            .ok_or_else(|| bad(format!("{op}: argument '{a}' is not key=value")))?;
        match k.trim() {
            "bins" => {
                let l = parse_list(op, k, v)?;
                let [lo, hi, step] = l[..] else {
                    return Err(bad(format!(
                        "{op}: bins={v} must be <lo>/<hi>/<step> (three numbers); explicit edges \
                         go into edges=<e0>/<e1>/..."
                    )));
                };
                let n = (hi - lo) / step;
                if !(step > 0.0 && hi > lo) || (n - n.round()).abs() > 1e-9 * n.max(1.0) {
                    return Err(bad(format!(
                        "{op}: bins={v}: need lo < hi and a step > 0 that divides hi - lo"
                    )));
                }
                let n = n.round() as usize;
                // lo + i·step, the last edge exactly hi
                let mut e: Vec<f64> = (0..n).map(|i| lo + i as f64 * step).collect();
                e.push(hi);
                edges = Some(e);
            }
            "edges" => {
                let e = parse_list(op, k, v)?;
                if e.len() < 2 || e.windows(2).any(|w| w[0] >= w[1]) {
                    return Err(bad(format!(
                        "{op}: edges={v} must be at least two strictly increasing numbers"
                    )));
                }
                edges = Some(e);
            }
            "out" => {
                fraction = match v.trim() {
                    "area" => false,
                    "fraction" => true,
                    _ => return Err(bad(format!("{op}: out={v}: use out=area or out=fraction"))),
                }
            }
            _ => return Err(bad(format!("{op}: unknown argument '{k}'"))),
        }
    }
    let edges = edges.ok_or_else(|| bad(format!("{op}: bins= or edges= is required")))?;
    Ok(Args { edges, fraction })
}

// ---------------------------------------------------------------------------------------------
// Kernel
// ---------------------------------------------------------------------------------------------

/// How the tiles of one input variable are folded.
#[derive(Debug)]
struct HistPlan {
    /// First horizontal dimension of the input variable and their number (1 or 2).
    b0: usize,
    nb: usize,
    /// Full sizes of the horizontal dimensions.
    bshape: Vec<usize>,
    weights: Weights,
    edges: Arc<Vec<f64>>,
    fraction: bool,
    /// Output variables of the bins, of `_below` and of `_above` (in [`Part`] order).
    outputs: [usize; 3],
}

impl HistPlan {
    /// Accumulators per field: below, the bins, above, all valid cells.
    fn slots(&self) -> usize {
        self.edges.len() + 2
    }

    /// Slot of a (valid) value: 0 below e_0, 1 + i for bin i, n + 1 above e_n.
    #[inline]
    fn slot(&self, x: f64) -> usize {
        let e = &self.edges;
        let n = e.len() - 1;
        if x < e[0] {
            0
        } else if x > e[n] {
            n + 1
        } else {
            // number of edges <= x is in 1..=n+1; x == e_n falls into the last bin
            e.partition_point(|&b| b <= x).min(n)
        }
    }
}

struct HistKernel {
    vars: Vec<Arc<HistPlan>>,
    info: Option<serde_json::Value>,
}

impl FoldKernel for HistKernel {
    fn fold_dim(&self) -> DimRole {
        DimRole::Horizontal
    }

    fn plan_info(&self) -> Option<serde_json::Value> {
        self.info.clone()
    }

    fn outputs(&self, var: usize) -> Vec<usize> {
        self.vars[var].outputs.to_vec()
    }

    fn state_bytes(&self, var: usize, lane: &TileBox) -> usize {
        let p = &self.vars[var];
        if p.nb == 2 {
            return lane.len() * 8;
        }
        let shape = lane.shape();
        let outer: usize = shape[..p.b0].iter().product();
        let inner: usize = shape[p.b0 + p.nb..].iter().product();
        outer * inner * p.slots() * 16
    }

    fn start(&self, var: usize, lane: &TileBox) -> Box<dyn FoldState> {
        let p = self.vars[var].clone();
        let shape = lane.shape();
        let outer: usize = shape[..p.b0].iter().product();
        let inner: usize = shape[p.b0 + p.nb..].iter().product();
        Box::new(State {
            lane: lane.clone(),
            outer,
            inner,
            acc: vec![0.0; outer * inner * p.slots()],
            strip: Vec::new(),
            p,
        })
    }
}

/// Running state of one lane: the slots of every (outer index, inner index).
struct State {
    p: Arc<HistPlan>,
    lane: TileBox,
    outer: usize,
    inner: usize,
    /// `acc[(o * inner + i) * slots + s]`.
    acc: Vec<f64>,
    /// Tiles of the current row strip (2-D grids whose `x` is split into several tiles).
    strip: Vec<Tile>,
}

impl State {
    /// Folds the tiles of one row strip (or one tile): rows in order, within a row the tiles in
    /// order, within a tile the cells in order.
    fn fold(&mut self, tiles: &[Tile]) {
        let p = self.p.clone();
        let (yr, nx) = match p.nb {
            2 => (tiles[0].bx.ranges[p.b0].clone(), p.bshape[1]),
            _ => (0..1, 0),
        };
        for y in yr.clone() {
            for t in tiles {
                let (x0, xn, ylen, yoff) = match p.nb {
                    2 => {
                        let xr = &t.bx.ranges[p.b0 + 1];
                        (xr.start, xr.len(), yr.len(), y - yr.start)
                    }
                    _ => {
                        let cr = &t.bx.ranges[p.b0];
                        (cr.start, cr.len(), 1, 0)
                    }
                };
                // tile values are [outer][ylen][xn][inner]
                let i_n = self.inner;
                let base = yoff * xn * i_n;
                let ostride = ylen * xn * i_n;
                match &t.values {
                    Values::F32(v) => self.cells(v, base, ostride, xn, y * nx + x0),
                    Values::F64(v) => self.cells(v, base, ostride, xn, y * nx + x0),
                }
            }
        }
    }

    /// Adds `n` consecutive cells (flat index `f0..`) of every field of the lane: value (o, k, i)
    /// at `base + o * ostride + k * inner + i`.
    fn cells<T: Copy + Into<f64>>(
        &mut self,
        v: &[T],
        base: usize,
        ostride: usize,
        n: usize,
        f0: usize,
    ) {
        let p = &*self.p;
        let (ns, i_n) = (p.slots(), self.inner);
        for o in 0..self.outer {
            for k in 0..n {
                let w = match &p.weights {
                    Weights::Const(w) => *w,
                    Weights::Each(ws) => ws[f0 + k],
                };
                let row = &v[base + o * ostride + k * i_n..][..i_n];
                for (i, &x) in row.iter().enumerate() {
                    let x: f64 = x.into();
                    if x.is_nan() {
                        continue;
                    }
                    let a = &mut self.acc[(o * i_n + i) * ns..][..ns];
                    a[p.slot(x)] += w;
                    a[ns - 1] += w;
                }
            }
        }
    }

    /// The tile of one part of the histogram (output variable `p.outputs[part]`).
    fn output(&self, part: Part) -> Tile {
        let p = &*self.p;
        let (ns, nbins) = (p.slots(), p.edges.len() - 1);
        let (lo, n) = match part {
            Part::Bins => (1, nbins),
            Part::Below => (0, 1),
            Part::Above => (nbins + 1, 1),
        };
        let mut ranges = self.lane.ranges[..p.b0].to_vec();
        if part == Part::Bins {
            ranges.push(0..nbins);
        }
        ranges.extend(self.lane.ranges[p.b0 + p.nb..].iter().cloned());
        let (o_n, i_n) = (self.outer, self.inner);
        let mut out = vec![f64::NAN; o_n * n * i_n];
        for o in 0..o_n {
            for i in 0..i_n {
                let a = &self.acc[(o * i_n + i) * ns..][..ns];
                let total = a[ns - 1];
                for r in 0..n {
                    let s = a[lo + r];
                    out[(o * n + r) * i_n + i] = if !p.fraction {
                        s
                    } else if total > 0.0 {
                        s / total
                    } else {
                        f64::NAN
                    };
                }
            }
        }
        Tile {
            var: p.outputs[part as usize],
            bx: TileBox { ranges },
            values: Values::F64(out),
        }
    }
}

impl FoldState for State {
    fn push(&mut self, tile: Tile) -> Result<Vec<Tile>> {
        if self.p.nb == 2 {
            let xend = tile.bx.ranges[self.p.b0 + 1].end;
            self.strip.push(tile);
            if xend == self.p.bshape[1] {
                let strip = std::mem::take(&mut self.strip);
                self.fold(&strip);
            }
        } else {
            self.fold(std::slice::from_ref(&tile));
        }
        Ok(Vec::new())
    }

    fn finish(&mut self) -> Result<Vec<Tile>> {
        if !self.strip.is_empty() {
            return Err(Error::internal("fldhist: incomplete row strip"));
        }
        Ok([Part::Bins, Part::Below, Part::Above]
            .map(|part| self.output(part))
            .to_vec())
    }
}

// ---------------------------------------------------------------------------------------------
// Description
// ---------------------------------------------------------------------------------------------

fn text(s: &str) -> AttrValue {
    AttrValue::Text(s.to_owned())
}

/// Weights of the cells of grid `g` for variable `v`: `fldmean`'s normalised weights
/// (fractions) or `fldint`'s cell areas in m² (areas), with where they come from.
fn hist_weights(
    g: &GridDesc,
    v: &VarDesc,
    srcs: &Sources,
    fraction: bool,
) -> Result<(Weights, Option<AreaSource>)> {
    if fraction {
        return fldstat::fld_weights(g, v, srcs, false);
    }
    let leaf = v
        .expr
        .leaves()
        .first()
        .map(|l| (*l).clone())
        .ok_or_else(|| Error::internal("variable without stored leaf"))?;
    let dvar = srcs.srcs[leaf.src]
        .dataset()
        .var(&leaf.var)
        .ok_or_else(|| Error::internal(format!("variable '{}' not found", leaf.var)))?;
    let read = |name: &str| g.src.read_var(name);
    let a = area::cell_areas(g.src.dataset(), dvar, &g.base, &read).map_err(|e| {
        e.with_hint(
            "fldhist,out=area needs cell areas (cell bounds or a cell-area variable, \
             -setgrid,<grid file>); out=fraction falls back to equal weights",
        )
    })?;
    let w: Vec<f64> = fldstat::selected_cells(g)
        .iter()
        .map(|&c| a.values[c])
        .collect();
    let w = if w.iter().all(|&x| x == w[0]) {
        Weights::Const(w[0])
    } else {
        Weights::Each(Arc::new(w))
    };
    Ok((w, Some(a.source)))
}

/// The `bin` axis: a one-axis grid made by the operator, centres and edges as bounds.
fn bin_grid(src: &GridDesc, edges: &[f64], units: Option<String>) -> GridDesc {
    let n = edges.len() - 1;
    let mut base = Grid::new(GridKind::Generic, vec!["bin".into()], n, 0);
    base.x = Some(CoordAxis {
        var: "bin".into(),
        dim: "bin".into(),
        long_name: Some("bin centre".into()),
        units: units.clone(),
        standard_name: None,
        is_f32: false,
        bounds_var: None,
    });
    base.vdim = Some("bnds".into());
    let mut xattrs = Attrs(vec![("long_name".to_owned(), text("bin centre"))]);
    if let Some(u) = &units {
        xattrs.0.push(("units".to_owned(), text(u)));
    }
    let fixed = FixedGrid {
        coords: GridCoords {
            xvals: edges.windows(2).map(|w| 0.5 * (w[0] + w[1])).collect(),
            yvals: Vec::new(),
            xbounds: Some(edges.windows(2).flat_map(|w| [w[0], w[1]]).collect()),
            ybounds: None,
            nv: 2,
            xunits: units.unwrap_or_default(),
            yunits: String::new(),
        },
        xattrs,
        yattrs: Attrs::default(),
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

/// Output description of `fldhist`.
pub fn describe(node: &OpNode, mut inputs: Vec<Desc>, srcs: &Sources) -> Result<Desc> {
    let op = node.name.as_str();
    let args = parse_args(op, &node.args)?;
    let input = inputs.remove(0);
    let edges = Arc::new(args.edges);
    let nbins = edges.len() - 1;
    let (lo, hi) = (edges[0], edges[nbins]);
    let mut out = input.clone();
    out.vars.clear();
    let bin_gi = out.grids.len();
    let mut plans = Vec::new();
    let mut wcache: HashMap<(usize, Option<String>), (Weights, Option<AreaSource>)> =
        HashMap::new();
    let (mut winfo, mut equal): (Vec<serde_json::Value>, Vec<(usize, Vec<String>)>) =
        (Vec::new(), Vec::new());
    let mut units: Vec<Option<String>> = Vec::new();
    for v in &input.vars {
        let (b0, nb) = fldstat::block_of(v, DimRole::Horizontal)?;
        let Some(gi) = v.grid.filter(|_| nb > 0) else {
            return Err(Error::new(
                ErrorCode::UnsupportedDimension,
                format!("{op}: variable '{}' has no horizontal dimensions", v.name),
            )
            .with_hint("select the variables on a grid first (-selname)"));
        };
        let g = &input.grids[gi];
        let avar = v.expr.leaves().first().and_then(|l| {
            srcs.srcs[l.src]
                .dataset()
                .var(&l.var)
                .and_then(|dv| area::area_variable(g.src.dataset(), dv))
        });
        let key = (gi, avar);
        let (weights, source) = match wcache.get(&key) {
            Some(c) => c.clone(),
            None => {
                let c = hist_weights(g, v, srcs, args.fraction)?;
                wcache.insert(key, c.clone());
                c
            }
        };
        if let Some(src) = &source {
            winfo.push(serde_json::json!({
                "variable": v.name,
                "area_weights": src.code(),
                "description": src.describe(),
            }));
            if *src == AreaSource::Constant {
                match equal.iter_mut().find(|(i, _)| *i == gi) {
                    Some((_, names)) => names.push(v.name.clone()),
                    None => equal.push((gi, vec![v.name.clone()])),
                }
            }
        }
        let vunits = v.attrs.get_str("units").map(str::to_owned);
        units.push(vunits.clone());
        let bshape: Vec<usize> = v.dims[b0..b0 + nb].iter().map(|d| d.size).collect();
        let what = if args.fraction {
            "fraction of the area"
        } else {
            "area"
        };
        let src_name = v.attrs.get_str("long_name").unwrap_or(&v.name).to_owned();
        for (part, suffix, long) in [
            (
                Part::Bins,
                "",
                format!("{what} of the cells with {src_name} in each bin"),
            ),
            (
                Part::Below,
                "_below",
                format!("{what} of the cells with {src_name} below {lo}"),
            ),
            (
                Part::Above,
                "_above",
                format!("{what} of the cells with {src_name} above {hi}"),
            ),
        ] {
            let name = format!("{}{suffix}", v.name);
            if input.vars.iter().any(|x| x.name == name) && !suffix.is_empty() {
                return Err(Error::bad_arguments(format!(
                    "{op}: the output variable '{name}' would clash with an input variable"
                )));
            }
            let mut ov = v.clone();
            ov.name = name;
            ov.dtype = crate::model::DType::F64;
            if v.dtype == crate::model::DType::F32 {
                // the float missing value as the double it reads as (-9e33f -> -9e33)
                ov.missval = format!("{}", v.missval as f32).parse().unwrap_or(v.missval);
            }
            let dims: Vec<VarDim> = if part == Part::Bins {
                vec![VarDim {
                    name: "bin".into(),
                    size: nbins,
                    role: DimRole::Horizontal,
                }]
            } else {
                vec![]
            };
            ov.dims.splice(b0..b0 + nb, dims);
            ov.grid = (part == Part::Bins).then_some(bin_gi);
            let a = &mut ov.attrs.0;
            a.retain(|(k, _)| {
                !matches!(
                    k.as_str(),
                    "units" | "standard_name" | "long_name" | "cell_measures" | "valid_range"
                )
            });
            a.push(("long_name".into(), text(&long)));
            a.push(("units".into(), text(if args.fraction { "1" } else { "m2" })));
            fldstat::add_cell_method(
                &mut ov,
                if args.fraction {
                    "area: mean"
                } else {
                    "area: sum"
                },
            );
            out.vars.push(ov);
        }
        let k = out.vars.len();
        plans.push(Arc::new(HistPlan {
            b0,
            nb,
            bshape,
            weights,
            edges: edges.clone(),
            fraction: args.fraction,
            outputs: [k - 3, k - 2, k - 1],
        }));
    }
    let Some(first) = input.vars.iter().find_map(|v| v.grid) else {
        return Err(Error::new(
            ErrorCode::UnsupportedDimension,
            format!("{op}: no variable on a horizontal grid"),
        ));
    };
    let shared_units = match units.first() {
        Some(u) if units.iter().all(|x| x == u) => u.clone(),
        _ => None,
    };
    out.grids
        .push(bin_grid(&input.grids[first], &edges, shared_units));
    let mut warnings = Vec::new();
    for (gi, names) in &equal {
        let msg = fldstat::equal_weights_warning(op, &input.grids[*gi], names);
        crate::exec::threads::warn_loud("equal_weights", &msg);
        warnings.push(serde_json::json!({"warning": "equal_weights", "message": msg}));
    }
    let info = (!winfo.is_empty())
        .then(|| serde_json::json!({"area_weights": winfo, "warnings": warnings}));
    out.fold = Some(Fold {
        input: Box::new(input),
        kernel: Arc::new(HistKernel { vars: plans, info }),
    });
    Ok(out)
}
