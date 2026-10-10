//! Space statistics: `fld*` over all horizontal cells, `zon*` per latitude row (regular grids)
//! or iso-latitude ring (HEALPix), `vert*` over the levels.
//!
//! cdo semantics (`Fldstat.cc`, `Zonstat.cc`, `Vertstat.cc`; weights in [`crate::model::area`]):
//! - `fldmean`, `fldavg`, `fldstd`, `fldstd1`, `fldvar`, `fldvar1` weight by the normalised cell
//!   areas (Σw·x/Σw over the non-missing cells); `fldmin`, `fldmax`, `fldrange`, `fldsum` are
//!   unweighted; every `zon*` is unweighted within its row; `vertmean`-like operators weight by
//!   the layer thickness (constant without layer bounds), `vertint` sums thickness·x;
//! - missing values are skipped; a result without any valid value is missing; the `avg`
//!   variants are missing as soon as one value is missing (`varray_weighted_avg_mv`);
//! - output grids as cdo writes them: `fld*` a 1-point lon-lat grid at (0, 0) (an unstructured
//!   1-point grid for unstructured input, `gen_target_gridpoint`); `zon*` a lon-lat grid with
//!   one longitude (0) and the latitudes of the rows or HEALPix rings; `vert*` drops the
//!   vertical axis.
//!
//! **Fold order.** A lane is one tile's worth of timesteps and levels (`fld*`, `zon*`) or of
//! timesteps and cells (`vert*`); its tiles arrive in order along the folded dimension(s). Every
//! output value is accumulated cell by cell in the canonical index order of the folded block
//! (row-major `y, x` for 2-D grids, the stored cell order for 1-D grids, level order for
//! `vert*`), so the result does not depend on the chunk layout. For 2-D grids whose `x`
//! dimension is split into several tiles the tiles of one row strip are buffered and folded
//! row by row. HEALPix rings are summed in the stored cell order, not in cdo's ring order (last
//! bits only). Weights are computed once per grid when the operator is described.

use crate::chain::OpNode;
use crate::error::{Error, ErrorCode, Result};
use crate::io::Values;
use crate::model::area::{self, SpaceWeighting, VertWeighting, WeightedSums};
use crate::model::{AttrValue, Attrs, CoordAxis, DimRole, Grid, GridKind, VarDim};
use crate::plan::stage::{FoldKernel, FoldState, Tile};
use crate::plan::tiling::TileBox;
use crate::plan::{Desc, FixedGrid, Fold, GridCoords, GridDesc, IndexMap, Sources, VarDesc};
use std::collections::HashMap;
use std::sync::Arc;

/// Operators of this module with their descriptions (registered in `ops::build_registry`).
pub fn operators() -> Vec<(String, String)> {
    let stats = [
        ("mean", "mean"),
        ("avg", "mean (missing if any value is missing)"),
        ("min", "minimum"),
        ("max", "maximum"),
        ("range", "range (max - min)"),
        ("sum", "sum"),
        ("std", "standard deviation (n)"),
        ("std1", "standard deviation (n-1)"),
        ("var", "variance (n)"),
        ("var1", "variance (n-1)"),
    ];
    let mut out = Vec::new();
    for (fam, what) in [
        (
            "fld",
            "Field {} over all cells (area-weighted where cdo weights)",
        ),
        ("zon", "Zonal {} per latitude row or HEALPix ring"),
        (
            "vert",
            "Vertical {} (layer-thickness weights where cdo weights)",
        ),
    ] {
        for (s, d) in stats {
            out.push((format!("{fam}{s}"), what.replace("{}", d)));
        }
    }
    out.push((
        "vertint".into(),
        "Vertical integral (sum of layer thickness times value)".into(),
    ));
    out
}

/// Whether `name` is an operator of this module.
pub fn handles(name: &str) -> bool {
    parse(name).is_some()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Fld,
    Zon,
    Vert,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stat {
    Mean,
    Avg,
    Min,
    Max,
    Range,
    Sum,
    Var,
    Var1,
    Std,
    Std1,
}

impl Stat {
    fn cell_method(self) -> &'static str {
        match self {
            Self::Mean | Self::Avg => "mean",
            Self::Min => "minimum",
            Self::Max => "maximum",
            Self::Range => "range",
            Self::Sum => "sum",
            Self::Var | Self::Var1 => "variance",
            Self::Std | Self::Std1 => "standard_deviation",
        }
    }
}

fn parse(name: &str) -> Option<(Family, Stat)> {
    if name == "vertint" {
        return Some((Family::Vert, Stat::Sum));
    }
    let (fam, rest) = if let Some(r) = name.strip_prefix("fld") {
        (Family::Fld, r)
    } else if let Some(r) = name.strip_prefix("zon") {
        (Family::Zon, r)
    } else {
        (Family::Vert, name.strip_prefix("vert")?)
    };
    let stat = match rest {
        "mean" => Stat::Mean,
        "avg" => Stat::Avg,
        "min" => Stat::Min,
        "max" => Stat::Max,
        "range" => Stat::Range,
        "sum" => Stat::Sum,
        "var" => Stat::Var,
        "var1" => Stat::Var1,
        "std" => Stat::Std,
        "std1" => Stat::Std1,
        _ => return None,
    };
    Some((fam, stat))
}

// ---------------------------------------------------------------------------------------------
// Accumulators
// ---------------------------------------------------------------------------------------------

/// Running state of one output value.
trait Acc: Copy + Send + 'static {
    fn new() -> Self;
    fn add(&mut self, w: f64, x: f64);
    fn value(&self, stat: Stat) -> f64;
}

/// Σw·x / Σw over the valid values (`varray_weighted_mean_mv`).
#[derive(Clone, Copy)]
struct MeanAcc {
    sum: f64,
    sumw: f64,
}

impl Acc for MeanAcc {
    fn new() -> Self {
        Self {
            sum: 0.0,
            sumw: 0.0,
        }
    }
    #[inline(always)]
    fn add(&mut self, w: f64, x: f64) {
        // branch-free: a missing value adds -0.0, which leaves every sum unchanged bitwise
        let valid = !x.is_nan();
        self.sum += if valid { w * x } else { -0.0 };
        self.sumw += if valid { w } else { -0.0 };
    }
    fn value(&self, _: Stat) -> f64 {
        if self.sumw == 0.0 {
            f64::NAN
        } else {
            self.sum / self.sumw
        }
    }
}

/// Σw·x / Σw over all values, missing if any value is missing (`varray_weighted_avg_mv`).
#[derive(Clone, Copy)]
struct AvgAcc {
    sum: f64,
    sumw: f64,
}

impl Acc for AvgAcc {
    fn new() -> Self {
        Self {
            sum: 0.0,
            sumw: 0.0,
        }
    }
    #[inline(always)]
    fn add(&mut self, w: f64, x: f64) {
        // NaN propagates through the sum, as cdo's ADDM/MULM propagate the missing value
        self.sum += w * x;
        self.sumw += w;
    }
    fn value(&self, _: Stat) -> f64 {
        if self.sumw == 0.0 || self.sum.is_nan() {
            f64::NAN
        } else {
            self.sum / self.sumw
        }
    }
}

/// Σw·x over the valid values; missing without any (`varray_sum_mv`).
#[derive(Clone, Copy)]
struct SumAcc {
    sum: f64,
    n: u64,
}

impl Acc for SumAcc {
    fn new() -> Self {
        Self { sum: 0.0, n: 0 }
    }
    #[inline(always)]
    fn add(&mut self, w: f64, x: f64) {
        let valid = !x.is_nan();
        self.sum += if valid { w * x } else { -0.0 };
        self.n += u64::from(valid);
    }
    fn value(&self, _: Stat) -> f64 {
        if self.n == 0 { f64::NAN } else { self.sum }
    }
}

#[derive(Clone, Copy)]
struct MinMaxAcc {
    min: f64,
    max: f64,
}

impl Acc for MinMaxAcc {
    fn new() -> Self {
        Self {
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
        }
    }
    #[inline(always)]
    fn add(&mut self, _: f64, x: f64) {
        let valid = !x.is_nan();
        self.min = if valid { self.min.min(x) } else { self.min };
        self.max = if valid { self.max.max(x) } else { self.max };
    }
    fn value(&self, stat: Stat) -> f64 {
        if self.min > self.max {
            return f64::NAN;
        }
        match stat {
            Stat::Min => self.min,
            Stat::Max => self.max,
            _ => self.max - self.min,
        }
    }
}

#[derive(Clone, Copy)]
struct VarAcc(WeightedSums);

impl Acc for VarAcc {
    fn new() -> Self {
        Self(WeightedSums::default())
    }
    #[inline(always)]
    fn add(&mut self, w: f64, x: f64) {
        self.0.add(w, x);
    }
    fn value(&self, stat: Stat) -> f64 {
        match stat {
            Stat::Var => self.0.var(),
            Stat::Var1 => self.0.var1(),
            Stat::Std => self.0.std(),
            _ => self.0.std1(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Plan of one variable
// ---------------------------------------------------------------------------------------------

/// Weight of each cell (level) of the folded block, by flat block index.
#[derive(Debug, Clone)]
enum Weights {
    Const(f64),
    Each(Arc<Vec<f64>>),
}

/// Output row of each cell of the folded block.
#[derive(Debug, Clone)]
enum RowMap {
    One,
    /// Regular grids: row = flat index / nx.
    Div(usize),
    /// HEALPix rings.
    Each(Arc<Vec<u32>>),
}

/// How the tiles of one variable are folded.
#[derive(Debug)]
struct VarPlan {
    /// First folded dimension of the input variable and the number of folded dimensions (0, 1
    /// or 2, contiguous).
    b0: usize,
    nb: usize,
    /// Full sizes of the folded dimensions.
    bshape: Vec<usize>,
    weights: Weights,
    rows: RowMap,
    nrows: usize,
    /// Sizes of the output dimensions that replace the folded block (product = `nrows`).
    out_block: Vec<usize>,
}

struct SpaceKernel {
    dim: DimRole,
    stat: Stat,
    vars: Vec<Arc<VarPlan>>,
    /// `--plan`: where the area weights come from, and the warnings about them.
    info: Option<serde_json::Value>,
}

impl FoldKernel for SpaceKernel {
    fn fold_dim(&self) -> DimRole {
        self.dim
    }

    fn plan_info(&self) -> Option<serde_json::Value> {
        self.info.clone()
    }

    /// One accumulator and one f64 result per output value; 2-D blocks also hold a row strip
    /// of tiles until it is complete (bounded by the lane's values).
    fn state_bytes(&self, var: usize, lane: &TileBox) -> usize {
        let Some(p) = self.vars.get(var) else {
            return lane.len() * 8;
        };
        if p.nb == 2 {
            return lane.len() * 8;
        }
        let shape = lane.shape();
        let outer: usize = shape[..p.b0].iter().product();
        let inner: usize = shape[p.b0 + p.nb..].iter().product();
        outer * inner * p.nrows * (std::mem::size_of::<VarAcc>() + 8)
    }

    fn start(&self, var: usize, lane: &TileBox) -> Box<dyn FoldState> {
        let p = self.vars[var].clone();
        match self.stat {
            Stat::Mean => Box::new(State::<MeanAcc>::new(var, p, self.stat, lane)),
            Stat::Avg => Box::new(State::<AvgAcc>::new(var, p, self.stat, lane)),
            Stat::Sum => Box::new(State::<SumAcc>::new(var, p, self.stat, lane)),
            Stat::Min | Stat::Max | Stat::Range => {
                Box::new(State::<MinMaxAcc>::new(var, p, self.stat, lane))
            }
            Stat::Var | Stat::Var1 | Stat::Std | Stat::Std1 => {
                Box::new(State::<VarAcc>::new(var, p, self.stat, lane))
            }
        }
    }
}

/// Running state of one lane: one accumulator per (outer index, inner index, output row).
struct State<A: Acc> {
    var: usize,
    p: Arc<VarPlan>,
    stat: Stat,
    lane: TileBox,
    /// Products of the lane's extents before and after the folded block.
    outer: usize,
    inner: usize,
    /// `acc[(o * inner + i) * nrows + r]`.
    acc: Vec<A>,
    /// Tiles of the current row strip (2-D blocks whose `x` is split into several tiles).
    strip: Vec<Tile>,
}

impl<A: Acc> State<A> {
    fn new(var: usize, p: Arc<VarPlan>, stat: Stat, lane: &TileBox) -> Self {
        let shape = lane.shape();
        let outer: usize = shape[..p.b0].iter().product();
        let inner: usize = shape[p.b0 + p.nb..].iter().product();
        Self {
            var,
            stat,
            lane: lane.clone(),
            outer,
            inner,
            acc: vec![A::new(); outer * inner * p.nrows],
            strip: Vec::new(),
            p,
        }
    }

    /// Folds the buffered tiles of one strip (or one tile): rows of the strip in order, within
    /// a row the tiles in order, within a tile the cells in order.
    fn fold(&mut self, tiles: &[Tile]) {
        let p = &*self.p;
        let (o_n, i_n) = (self.outer, self.inner);
        // 2-D blocks: the tiles share the rows `yr`; flat index = y * nx + x
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
                    1 => {
                        let cr = &t.bx.ranges[p.b0];
                        (cr.start, cr.len(), 1, 0)
                    }
                    _ => (0, 1, 1, 0),
                };
                // tile values are [outer][ylen][xn][inner]
                let c = Cells {
                    base: yoff * xn * i_n,
                    ostride: ylen * xn * i_n,
                    n: xn,
                    f0: y * nx + x0,
                    o_n,
                    i_n,
                };
                match &t.values {
                    Values::F32(v) => fold_cells(v, &c, p, &mut self.acc),
                    Values::F64(v) => fold_cells(v, &c, p, &mut self.acc),
                }
            }
        }
    }

    /// Folds one tile of a block with at most one dimension, given by its box and its values
    /// (C order); the same fold as [`Self::fold`] of that tile alone.
    fn fold_values<T: Copy + Into<f64>>(&mut self, bx: &TileBox, v: &[T]) {
        let p = &*self.p;
        let (o_n, i_n) = (self.outer, self.inner);
        let (x0, xn) = match p.nb {
            1 => (bx.ranges[p.b0].start, bx.ranges[p.b0].len()),
            _ => (0, 1),
        };
        let c = Cells {
            base: 0,
            ostride: xn * i_n,
            n: xn,
            f0: x0,
            o_n,
            i_n,
        };
        fold_cells(v, &c, p, &mut self.acc);
    }

    fn output(&self) -> Tile {
        let p = &*self.p;
        let mut ranges = self.lane.ranges[..p.b0].to_vec();
        ranges.extend(p.out_block.iter().map(|&n| 0..n));
        ranges.extend(self.lane.ranges[p.b0 + p.nb..].iter().cloned());
        let (o_n, i_n, nr) = (self.outer, self.inner, p.nrows);
        let mut out = vec![f64::NAN; o_n * nr * i_n];
        for o in 0..o_n {
            for i in 0..i_n {
                for r in 0..nr {
                    out[(o * nr + r) * i_n + i] = self.acc[(o * i_n + i) * nr + r].value(self.stat);
                }
            }
        }
        Tile {
            var: self.var,
            bx: TileBox { ranges },
            values: Values::F64(out),
        }
    }
}

/// `n` consecutive cells of the folded block (flat index `f0..`) in one tile, for every outer
/// and inner index of the lane: value (o, k, i) at `base + o * ostride + k * i_n + i`.
struct Cells {
    base: usize,
    ostride: usize,
    n: usize,
    f0: usize,
    o_n: usize,
    i_n: usize,
}

/// Accumulators folded side by side (independent dependency chains; each still adds its cells
/// in order).
const GROUP: usize = 8;

// the cells of GROUP rows are interleaved by index on purpose
#[allow(clippy::needless_range_loop)]
fn fold_cells<T: Copy + Into<f64>, A: Acc>(v: &[T], c: &Cells, p: &VarPlan, acc: &mut [A]) {
    let (o_n, i_n, n, f0) = (c.o_n, c.i_n, c.n, c.f0);
    if p.nrows == 1 && i_n == 1 {
        // field statistics: one accumulator per timestep (and level), GROUP of them at a time,
        // the leftover ones in groups of 4, 2 and 1
        let mut o = 0;
        while o + GROUP <= o_n {
            fold_group::<T, A, GROUP>(v, c, p, &mut acc[o..o + GROUP], o);
            o += GROUP;
        }
        if o + 4 <= o_n {
            fold_group::<T, A, 4>(v, c, p, &mut acc[o..o + 4], o);
            o += 4;
        }
        if o + 2 <= o_n {
            fold_group::<T, A, 2>(v, c, p, &mut acc[o..o + 2], o);
            o += 2;
        }
        if o < o_n {
            fold_group::<T, A, 1>(v, c, p, &mut acc[o..o + 1], o);
        }
    } else if p.nrows == 1 {
        // vertical statistics: cells (inner) side by side, levels in order
        for o in 0..o_n {
            let acc = &mut acc[o * i_n..(o + 1) * i_n];
            for k in 0..n {
                let w = match &p.weights {
                    Weights::Const(w) => *w,
                    Weights::Each(ws) => ws[f0 + k],
                };
                let row = &v[c.base + o * c.ostride + k * i_n..][..i_n];
                for (a, &x) in acc.iter_mut().zip(row) {
                    a.add(w, x.into());
                }
            }
        }
    } else {
        let nr = p.nrows;
        for o in 0..o_n {
            for i in 0..i_n {
                let a = &mut acc[(o * i_n + i) * nr..(o * i_n + i + 1) * nr];
                fold_row(v, c.base + o * c.ostride + i, i_n, n, f0, p, a);
            }
        }
    }
}

/// Folds the cells of `G` field-statistic accumulators (outer indices `o..o + G`) side by side:
/// `G` independent dependency chains, each adding its cells in order.
#[inline(always)]
#[allow(clippy::needless_range_loop)]
fn fold_group<T: Copy + Into<f64>, A: Acc, const G: usize>(
    v: &[T],
    c: &Cells,
    p: &VarPlan,
    acc: &mut [A],
    o: usize,
) {
    let (n, f0) = (c.n, c.f0);
    let rows: [&[T]; G] = std::array::from_fn(|g| &v[c.base + (o + g) * c.ostride..][..n]);
    let mut a: [A; G] = std::array::from_fn(|g| acc[g]);
    match &p.weights {
        Weights::Const(w) => {
            for k in 0..n {
                for g in 0..G {
                    a[g].add(*w, rows[g][k].into());
                }
            }
        }
        Weights::Each(ws) => {
            for (k, &w) in ws[f0..f0 + n].iter().enumerate() {
                for g in 0..G {
                    a[g].add(w, rows[g][k].into());
                }
            }
        }
    }
    acc[..G].copy_from_slice(&a);
}

/// Folds `n` consecutive cells of the block (flat index `f0..f0+n`) whose values are
/// `v[start + k * stride]`.
#[inline]
fn fold_row<T: Copy + Into<f64>, A: Acc>(
    v: &[T],
    start: usize,
    stride: usize,
    n: usize,
    f0: usize,
    p: &VarPlan,
    acc: &mut [A],
) {
    let val = |k: usize| -> f64 { v[start + k * stride].into() };
    match (&p.rows, &p.weights) {
        (RowMap::One, Weights::Const(w)) => {
            let a = &mut acc[0];
            if stride == 1 {
                for &x in &v[start..start + n] {
                    a.add(*w, x.into());
                }
            } else {
                for k in 0..n {
                    a.add(*w, val(k));
                }
            }
        }
        (RowMap::One, Weights::Each(ws)) => {
            let a = &mut acc[0];
            let ws = &ws[f0..f0 + n];
            if stride == 1 {
                for (&x, &w) in v[start..start + n].iter().zip(ws) {
                    a.add(w, x.into());
                }
            } else {
                for (k, &w) in ws.iter().enumerate() {
                    a.add(w, val(k));
                }
            }
        }
        (rows, w) => {
            for k in 0..n {
                let f = f0 + k;
                let r = match rows {
                    RowMap::One => 0,
                    RowMap::Div(nx) => f / nx,
                    RowMap::Each(m) => m[f] as usize,
                };
                let wk = match w {
                    Weights::Const(c) => *c,
                    Weights::Each(ws) => ws[f],
                };
                acc[r].add(wk, val(k));
            }
        }
    }
}

impl<A: Acc> FoldState for State<A> {
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
            return Err(Error::internal("space statistic: incomplete row strip"));
        }
        Ok(vec![self.output()])
    }

    fn push_slice(
        &mut self,
        bx: &TileBox,
        values: &Values,
        range: std::ops::Range<usize>,
    ) -> Option<Result<Vec<Tile>>> {
        if self.p.nb == 2 {
            return None;
        }
        match values {
            Values::F32(v) => self.fold_values(bx, &v[range]),
            Values::F64(v) => self.fold_values(bx, &v[range]),
        }
        Some(Ok(Vec::new()))
    }
}

// ---------------------------------------------------------------------------------------------
// Descriptions
// ---------------------------------------------------------------------------------------------

fn text(s: &str) -> AttrValue {
    AttrValue::Text(s.to_owned())
}

fn coord_attrs(std_name: &str, units: &str, axis: Option<&str>) -> Attrs {
    let mut a = vec![
        ("standard_name".to_owned(), text(std_name)),
        ("long_name".to_owned(), text(std_name)),
        ("units".to_owned(), text(units)),
    ];
    if let Some(ax) = axis {
        a.push(("axis".to_owned(), text(ax)));
    }
    Attrs(a)
}

fn axis(var: &str, dim: &str, std_name: &str, units: &str) -> CoordAxis {
    CoordAxis {
        var: var.to_owned(),
        dim: dim.to_owned(),
        long_name: Some(std_name.to_owned()),
        units: Some(units.to_owned()),
        standard_name: Some(std_name.to_owned()),
        is_f32: false,
        bounds_var: None,
    }
}

/// A grid made by the operator: a lon-lat grid with one longitude (0) and the latitudes `lats`,
/// or (`unstructured_dim`) a 1-point unstructured grid over that dimension.
fn made_grid(
    g: &GridDesc,
    lats: Vec<f64>,
    unstructured_dim: Option<&str>,
) -> (GridDesc, Vec<VarDim>) {
    let h = |name: &str, size: usize| VarDim {
        name: name.to_owned(),
        size,
        role: DimRole::Horizontal,
    };
    let (kind, base, dims, xattrs, yattrs) = match unstructured_dim {
        Some(cd) => {
            let (xn, yn) = match (&g.base.x, &g.base.y, g.base.kind) {
                (Some(x), Some(y), GridKind::Unstructured) => (x.var.clone(), y.var.clone()),
                _ => ("lon".to_owned(), "lat".to_owned()),
            };
            let mut base = Grid::new(GridKind::Unstructured, vec![cd.to_owned()], 1, 0);
            base.x = Some(axis(&xn, cd, "longitude", "degrees_east"));
            base.y = Some(axis(&yn, cd, "latitude", "degrees_north"));
            (
                GridKind::Unstructured,
                base,
                vec![h(cd, 1)],
                coord_attrs("longitude", "degrees_east", None),
                coord_attrs("latitude", "degrees_north", None),
            )
        }
        None => {
            let ny = lats.len();
            let mut base = Grid::new(GridKind::Regular, vec!["lat".into(), "lon".into()], 1, ny);
            base.x = Some(axis("lon", "lon", "longitude", "degrees_east"));
            base.y = Some(axis("lat", "lat", "latitude", "degrees_north"));
            base.xvals = Some(vec![0.0]);
            base.yvals = Some(lats.clone());
            (
                GridKind::Regular,
                base,
                vec![h("lat", ny), h("lon", 1)],
                coord_attrs("longitude", "degrees_east", Some("X")),
                coord_attrs("latitude", "degrees_north", Some("Y")),
            )
        }
    };
    let sel = if kind == GridKind::Regular {
        vec![IndexMap::identity(lats.len()), IndexMap::identity(1)]
    } else {
        vec![IndexMap::identity(1)]
    };
    let ny = lats.len();
    let fixed = FixedGrid {
        coords: GridCoords {
            xvals: vec![0.0],
            yvals: if kind == GridKind::Regular {
                lats
            } else {
                vec![0.0]
            },
            xbounds: None,
            ybounds: None,
            nv: 0,
            xunits: "degrees_east".into(),
            yunits: "degrees_north".into(),
        },
        xattrs,
        yattrs,
    };
    debug_assert!(kind != GridKind::Regular || ny >= 1);
    (
        GridDesc {
            kind,
            base,
            src: g.src.clone(),
            sel,
            xvals: None,
            fixed: Some(Arc::new(fixed)),
        },
        dims,
    )
}

/// Flat indices into the stored grid of the selected cells, in output order.
fn selected_cells(g: &GridDesc) -> Vec<usize> {
    if g.sel.len() == 2 {
        let (ys, xs) = (g.sel[0].to_vec(), g.sel[1].to_vec());
        let nx = g.base.xsize;
        ys.iter()
            .flat_map(|&j| xs.iter().map(move |&i| j * nx + i))
            .collect()
    } else {
        g.sel[0].to_vec()
    }
}

/// Cell weights of the `fld*` operators for the (selected) grid: cdo's weights of the whole
/// grid, or for a subset the stored-grid weights of the selected cells renormalised to sum 1;
/// with where they come from (`None`: a single cell).
fn fld_weights(
    g: &GridDesc,
    var: &VarDesc,
    srcs: &Sources,
) -> Result<(Weights, Option<area::AreaSource>)> {
    let n = g.size();
    if n <= 1 {
        return Ok((Weights::Const(1.0), None));
    }
    let leaf = var
        .expr
        .leaves()
        .first()
        .map(|l| (*l).clone())
        .ok_or_else(|| Error::internal("variable without stored leaf"))?;
    let lsrc = &srcs.srcs[leaf.src];
    let dvar = lsrc
        .dataset()
        .var(&leaf.var)
        .ok_or_else(|| Error::internal(format!("variable '{}' not found", leaf.var)))?;
    let read = |name: &str| g.src.read_var(name);
    let cw = area::cell_weights(g.src.dataset(), dvar, &g.base, &read)?;
    let source = cw.source.clone();
    let mut w = cw.values;
    if n != g.base.size || selected_cells(g).iter().enumerate().any(|(k, &c)| k != c) {
        let picked: Vec<f64> = selected_cells(g).iter().map(|&c| w[c]).collect();
        let total: f64 = picked.iter().sum();
        w = picked.iter().map(|x| x / total).collect();
    }
    let w = if w.iter().all(|&x| x == w[0]) {
        Weights::Const(w[0])
    } else {
        Weights::Each(Arc::new(w))
    };
    Ok((w, Some(source)))
}

/// The warning when a weighted field statistic has to weight all cells equally: loud, since
/// the result is then only right for grids whose cells all have the same area (cdo warns in one
/// line, "Grid cell bounds not available, using constant grid cell area weights").
fn equal_weights_warning(op: &str, g: &GridDesc, vars: &[String]) -> String {
    let kind = g.base.kind;
    let what = match kind {
        GridKind::Generic => "has no coordinates",
        GridKind::Unstructured => {
            "has cell centres but no cell bounds, no cell-area variable and no layout cdors \
             computes areas for (regular, Gaussian, reduced Gaussian, HEALPix)"
        }
        _ => "has cell centres but no cell bounds and no cell-area variable",
    };
    format!(
        "{op} of {} uses EQUAL WEIGHTS for all {} cells, NOT area weights: the {} grid {what}. \
         The result is only right if all cells have the same area. Fix: give the grid with cell \
         bounds or a cell-area variable (-setgrid,<grid file>).",
        vars.iter()
            .map(|v| format!("'{v}'"))
            .collect::<Vec<_>>()
            .join(", "),
        g.base.size,
        kind.name()
    )
}

/// Rows of the `zon*` operators: (row map, latitudes).
fn zonal(g: &GridDesc) -> Result<(RowMap, Vec<f64>)> {
    match g.kind {
        GridKind::Regular | GridKind::Gaussian => {
            let lats = g.base.yvals.clone().unwrap_or_default();
            let ys = g.sel[0].to_vec();
            let (nx, _) = g.xy_size();
            Ok((RowMap::Div(nx), ys.iter().map(|&j| lats[j]).collect()))
        }
        GridKind::Healpix if g.is_healpix() => {
            let rows = area::zonal_rows(&g.base)?;
            let mut ring = vec![0u32; g.base.size];
            for r in 0..rows.len() {
                for &c in rows.row(r) {
                    ring[c] = r as u32;
                }
            }
            Ok((RowMap::Each(Arc::new(ring)), rows.coords))
        }
        _ => Err(Error::new(
            ErrorCode::UnsupportedGrid,
            format!(
                "zonal statistics need a regular, Gaussian or complete HEALPix grid, not {}",
                if g.base.kind == GridKind::Healpix {
                    "a HEALPix subset"
                } else {
                    g.kind.name()
                }
            ),
        )
        .with_hint("remap to a regular grid first (remapcon/remapbil)")),
    }
}

fn add_cell_method(v: &mut VarDesc, method: &str) {
    let a = &mut v.attrs.0;
    a.retain(|(k, _)| k != "cell_measures");
    match a.iter_mut().find(|(k, _)| k == "cell_methods") {
        Some((_, AttrValue::Text(s))) if !s.is_empty() => {
            s.push(' ');
            s.push_str(method);
        }
        Some(e) => e.1 = text(method),
        None => a.push(("cell_methods".to_owned(), text(method))),
    }
}

fn block_of(v: &VarDesc, role: DimRole) -> Result<(usize, usize)> {
    let idx: Vec<usize> = (0..v.dims.len())
        .filter(|&i| v.dims[i].role == role)
        .collect();
    match idx.first() {
        None => Ok((v.dims.len(), 0)),
        Some(&b0) if idx.iter().enumerate().all(|(k, &i)| i == b0 + k) && idx.len() <= 2 => {
            Ok((b0, idx.len()))
        }
        Some(_) => Err(Error::new(
            ErrorCode::UnsupportedDimension,
            format!(
                "variable '{}': the {} dimensions are not adjacent",
                v.name,
                if role == DimRole::Horizontal {
                    "horizontal"
                } else {
                    "vertical"
                }
            ),
        )),
    }
}

/// Output description of a space statistic.
pub fn describe(node: &OpNode, mut inputs: Vec<Desc>, srcs: &Sources) -> Result<Desc> {
    let name = node.name.as_str();
    let (fam, stat) =
        parse(name).ok_or_else(|| Error::internal(format!("'{name}' is not a space statistic")))?;
    let input = inputs.remove(0);
    let mut out = input.clone();
    let mut plans = Vec::with_capacity(out.vars.len());
    let mut info = None;
    match fam {
        Family::Fld | Family::Zon => {
            let weighted = fam == Family::Fld
                && area::fldstat_weighting(name) == Some(SpaceWeighting::Weights);
            // per grid: the grid made, its dimensions, rows; weights per (grid, area variable)
            let mut made: HashMap<usize, (GridDesc, Vec<VarDim>, RowMap, usize)> = HashMap::new();
            type Cached = (Weights, Option<area::AreaSource>);
            let mut wcache: HashMap<(usize, Option<String>), Cached> = HashMap::new();
            // `--plan`: where each variable's weights come from; variables weighted equally by grid
            let mut winfo = Vec::new();
            let mut equal: Vec<(usize, Vec<String>)> = Vec::new();
            for v in &mut out.vars {
                let (b0, nb) = block_of(v, DimRole::Horizontal)?;
                let bshape: Vec<usize> = v.dims[b0..b0 + nb].iter().map(|d| d.size).collect();
                let Some(gi) = v.grid.filter(|_| nb > 0) else {
                    // no horizontal dimensions: every value is its own field
                    plans.push(Arc::new(VarPlan {
                        b0,
                        nb: 0,
                        bshape,
                        weights: Weights::Const(1.0),
                        rows: RowMap::One,
                        nrows: 1,
                        out_block: vec![],
                    }));
                    continue;
                };
                let g = &input.grids[gi];
                if let std::collections::hash_map::Entry::Vacant(slot) = made.entry(gi) {
                    let entry = match fam {
                        Family::Fld => {
                            let ud =
                                (g.kind == GridKind::Unstructured).then(|| v.dims[b0].name.clone());
                            let (gd, dims) = made_grid(g, vec![0.0], ud.as_deref());
                            (gd, dims, RowMap::One, 1)
                        }
                        _ => {
                            let (rows, lats) = zonal(g)?;
                            let n = lats.len();
                            let (gd, dims) = made_grid(g, lats, None);
                            (gd, dims, rows, n)
                        }
                    };
                    slot.insert(entry);
                }
                let (_, dims, rows, nrows) = &made[&gi];
                let weights = if weighted {
                    let dvar = v.expr.leaves().first().and_then(|l| {
                        srcs.srcs[l.src]
                            .dataset()
                            .var(&l.var)
                            .and_then(|dv| area::area_variable(g.src.dataset(), dv))
                    });
                    let key = (gi, dvar);
                    let (w, source) = match wcache.get(&key) {
                        Some(c) => c.clone(),
                        None => {
                            let c = fld_weights(g, v, srcs)?;
                            wcache.insert(key, c.clone());
                            c
                        }
                    };
                    if let Some(src) = source {
                        let mut e = serde_json::json!({
                            "variable": v.name,
                            "area_weights": src.code(),
                            "description": src.describe(),
                        });
                        if let area::AreaSource::File(a) = &src {
                            e["area_variable"] = serde_json::json!(a);
                        }
                        winfo.push(e);
                        if src == area::AreaSource::Constant {
                            match equal.iter_mut().find(|(i, _)| *i == gi) {
                                Some((_, names)) => names.push(v.name.clone()),
                                None => equal.push((gi, vec![v.name.clone()])),
                            }
                        }
                    }
                    w
                } else {
                    Weights::Const(1.0)
                };
                plans.push(Arc::new(VarPlan {
                    b0,
                    nb,
                    bshape,
                    weights,
                    rows: rows.clone(),
                    nrows: *nrows,
                    out_block: dims.iter().map(|d| d.size).collect(),
                }));
                v.dims.splice(b0..b0 + nb, dims.iter().cloned());
                add_cell_method(
                    v,
                    &format!(
                        "{}: {}",
                        if fam == Family::Fld {
                            "area"
                        } else {
                            "longitude"
                        },
                        stat.cell_method()
                    ),
                );
            }
            let mut warnings = Vec::new();
            for (gi, names) in &equal {
                let msg = equal_weights_warning(name, &input.grids[*gi], names);
                crate::exec::threads::warn_loud("equal_weights", &msg);
                warnings.push(serde_json::json!({"warning": "equal_weights", "message": msg}));
            }
            if !winfo.is_empty() {
                info = Some(serde_json::json!({"area_weights": winfo, "warnings": warnings}));
            }
            for (gi, (gd, _, _, _)) in made {
                out.grids[gi] = gd;
            }
        }
        Family::Vert => {
            let weighting = area::vertstat_weighting(name).unwrap_or(VertWeighting::None);
            for v in &mut out.vars {
                let (b0, nb) = block_of(v, DimRole::Vertical)?;
                let nlev = if nb == 1 { v.dims[b0].size } else { 1 };
                let za = v.zaxis.map(|z| &input.zaxes[z].axis);
                let lw = area::layer_weights(za, nlev, true, false);
                let weights = match weighting {
                    VertWeighting::None => Weights::Const(1.0),
                    VertWeighting::Weights => Weights::Each(Arc::new(lw.weights)),
                    VertWeighting::Thickness => Weights::Each(Arc::new(lw.thickness)),
                };
                if lw.status == 0 && nlev > 1 && weighting != VertWeighting::None {
                    crate::exec::threads::warn(
                        "equal_vertical_weights",
                        &format!(
                            "{name}: layer bounds not available, using constant vertical weights for variable {}",
                            v.name
                        ),
                    );
                }
                plans.push(Arc::new(VarPlan {
                    b0,
                    nb,
                    bshape: vec![nlev; nb],
                    weights,
                    rows: RowMap::One,
                    nrows: 1,
                    out_block: vec![],
                }));
                if nb == 1 {
                    let zdim = v.dims[b0].name.clone();
                    v.dims.remove(b0);
                    v.zaxis = None;
                    add_cell_method(v, &format!("{zdim}: {}", stat.cell_method()));
                }
            }
        }
    }
    out.fold = Some(Fold {
        input: Box::new(input),
        kernel: Arc::new(SpaceKernel {
            dim: if fam == Family::Vert {
                DimRole::Vertical
            } else {
                DimRole::Horizontal
            },
            stat,
            vars: plans,
            info,
        }),
    });
    Ok(out)
}
