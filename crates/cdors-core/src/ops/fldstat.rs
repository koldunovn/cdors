//! Space statistics: `fld*` over all horizontal cells, `zon*` per latitude row (regular grids)
//! or iso-latitude ring (HEALPix), `vert*` over the levels.
//!
//! cdo semantics (`Fldstat.cc`, `Zonstat.cc`, `Vertstat.cc`; weights in [`crate::model::area`]):
//! - `fldmean`, `fldavg`, `fldstd`, `fldstd1`, `fldvar`, `fldvar1` weight by the normalised cell
//!   areas (Σw·x/Σw over the non-missing cells); `fldmin`, `fldmax`, `fldrange`, `fldsum` are
//!   unweighted; every `zon*` is unweighted within its row; `vertmean`-like operators weight by
//!   the layer thickness (constant without layer bounds), `vertint` sums thickness·x
//!   (`vertint,z0/z1`: the layers cut at those depths); on a model mesh the thickness of the
//!   mesh's layers, each cell's dry levels being missing ([`super::vertical`]); `bottomvalue`
//!   keeps the valid value nearest the bottom (cdo `Selsurface.cc`; on a mesh the cell's own);
//! - missing values are skipped; a result without any valid value is missing; the `avg`
//!   variants are missing as soon as one value is missing (`varray_weighted_avg_mv`);
//! - output grids as cdo writes them: `fld*` a 1-point lon-lat grid at (0, 0) (an unstructured
//!   1-point grid for unstructured input, `gen_target_gridpoint`); `zon*` a lon-lat grid with
//!   one longitude (0) and the latitudes of the rows or HEALPix rings; `vert*` drops the
//!   vertical axis;
//! - `zonmean`, `zonsum`, `zonint` with cdo's latitude-band description `zonal_<DY>` (any grid
//!   with cell corners: unstructured, curvilinear, HEALPix, regular): bands of DY degrees from
//!   the south pole, a lon-lat grid with one longitude (0, bounds −180/180) and the band centres
//!   with bounds; `zonmean` is the area-weighted mean of the cells' parts inside the band,
//!   `zonint` Σ part area·R²·x, `zonsum` the plain sum over the cells whose centre is in the
//!   band ([`band_matrix`]). cdo 2.6.0 accepts `zonal_<DY>` with `zonmean` only, 2.6.5 also
//!   with `zonsum` and `zonint` (which has no row version in cdors).
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
use crate::ops::vertical;
use crate::plan::stage::{FoldKernel, FoldState, Tile};
use crate::plan::tiling::TileBox;
use crate::plan::{Desc, FixedGrid, Fold, GridCoords, GridDesc, IndexMap, Sources, VarDesc};
use cdors_mesh::Csr;
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
            let name = format!("{fam}{s}");
            let mut desc = what.replace("{}", d);
            if takes_zonaldes(&name) {
                desc.push_str(
                    "; zonal_<DY>: per latitude band of DY degrees, any grid with corners",
                );
            }
            out.push((name, desc));
        }
    }
    out.push((
        "fldint".into(),
        "Field integral (sum of cell area times value, m2 times the unit)".into(),
    ));
    out.push((
        "vertint".into(),
        "Vertical integral (sum of layer thickness times value; vertint,<z0>/<z1>: the layers \
         cut at depths z0 and z1; on a model mesh the mesh's layers, dry levels skipped)"
            .into(),
    ));
    out.push((
        "bottomvalue".into(),
        "Value at the bottom level: the valid value nearest the bottom (CF attribute positive; \
         on a model mesh each cell's own bottom)"
            .into(),
    ));
    out.push((
        "zonint".into(),
        "Zonal integral (sum of cell area times value) per latitude band zonal_<DY>".into(),
    ));
    out
}

/// Whether `name` accepts cdo's latitude-band description `zonal_<DY>` (`Zonstat.cc`:
/// `operSupportsUnstruct`).
pub fn takes_zonaldes(name: &str) -> bool {
    matches!(name, "zonmean" | "zonsum" | "zonint")
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
pub(crate) enum Stat {
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
    /// Σ area·value (`fldint`).
    Int,
    /// Σ thickness·value over the valid values of the layers inside the depth range
    /// (`vertint`): levels of weight 0 do not count.
    VertInt,
    /// The valid value of the level with the largest weight (`bottomvalue`: the weights rank the
    /// levels from the top).
    Bottom,
}

impl Stat {
    fn cell_method(self) -> &'static str {
        match self {
            Self::Mean | Self::Avg => "mean",
            Self::Min => "minimum",
            Self::Max => "maximum",
            Self::Range => "range",
            Self::Sum | Self::Int | Self::VertInt => "sum",
            Self::Bottom => "point",
            Self::Var | Self::Var1 => "variance",
            Self::Std | Self::Std1 => "standard_deviation",
        }
    }
}

fn parse(name: &str) -> Option<(Family, Stat)> {
    if name == "vertint" {
        return Some((Family::Vert, Stat::VertInt));
    }
    if name == "bottomvalue" {
        return Some((Family::Vert, Stat::Bottom));
    }
    if name == "fldint" {
        return Some((Family::Fld, Stat::Int));
    }
    if name == "zonint" {
        return Some((Family::Zon, Stat::Sum));
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
pub(crate) trait Acc: Copy + Send + 'static {
    fn new() -> Self;
    fn add(&mut self, w: f64, x: f64);
    fn value(&self, stat: Stat) -> f64;
}

/// Σw·x / Σw over the valid values (`varray_weighted_mean_mv`).
#[derive(Clone, Copy)]
pub(crate) struct MeanAcc {
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
pub(crate) struct SumAcc {
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

/// Σw·x over the valid values whose weight is not 0 (the levels inside a depth range); missing
/// without any.
#[derive(Clone, Copy)]
struct CutSumAcc {
    sum: f64,
    n: u64,
}

impl Acc for CutSumAcc {
    fn new() -> Self {
        Self { sum: 0.0, n: 0 }
    }
    #[inline(always)]
    fn add(&mut self, w: f64, x: f64) {
        let valid = !x.is_nan() && w != 0.0;
        self.sum += if valid { w * x } else { -0.0 };
        self.n += u64::from(valid);
    }
    fn value(&self, _: Stat) -> f64 {
        if self.n == 0 { f64::NAN } else { self.sum }
    }
}

/// The valid value of the largest weight seen (ties: the first).
#[derive(Clone, Copy)]
struct RankAcc {
    rank: f64,
    x: f64,
}

impl Acc for RankAcc {
    fn new() -> Self {
        Self {
            rank: f64::NEG_INFINITY,
            x: f64::NAN,
        }
    }
    #[inline(always)]
    fn add(&mut self, w: f64, x: f64) {
        if !x.is_nan() && w > self.rank {
            self.rank = w;
            self.x = x;
        }
    }
    fn value(&self, _: Stat) -> f64 {
        self.x
    }
}

#[derive(Clone, Copy)]
pub(crate) struct MinMaxAcc {
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
pub(crate) struct VarAcc(WeightedSums);

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
pub(crate) enum Weights {
    Const(f64),
    Each(Arc<Vec<f64>>),
    /// Field statistics of a variable with levels on a mesh place with per-level areas (FESOM2
    /// nodes): at each level the cells weigh that level's areas.
    PerLevel(Arc<LevelWeights>),
}

/// Per-level cell weights: at output level `j` of the variable's level dimension `dim`, cell
/// `f` of the block weighs `area_k[cells[f]] * scale_k`, where `k = levels[j]` is the mesh level
/// and `scale_k` normalises the level's weights to sum 1 (all weighted `fld*` but `fldint`, as
/// cdo's 2-D weights). The areas are the mesh's own arrays (whole place) or a gathered copy
/// (a subset), loaded when a lane first needs them, so `--plan` loads nothing.
pub(crate) struct LevelWeights {
    dim: usize,
    pub(crate) mesh: Arc<cdors_mesh::Mesh>,
    pub(crate) place: String,
    pub(crate) levels: IndexMap,
    /// Stored cells of the block, `None` for the whole place in order.
    cells: Option<Arc<Vec<usize>>>,
    normalise: bool,
    loaded: Vec<std::sync::OnceLock<(Arc<Vec<f64>>, f64)>>,
}

impl std::fmt::Debug for LevelWeights {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "LevelWeights(dim {}, {} levels of '{}' of {})",
            self.dim,
            self.levels.len(),
            self.place,
            self.mesh.name
        )
    }
}

impl LevelWeights {
    /// Areas and scale of output level `j`.
    pub(crate) fn level(&self, j: usize) -> Result<(Arc<Vec<f64>>, f64)> {
        if let Some(v) = self.loaded[j].get() {
            return Ok(v.clone());
        }
        let k = self.levels.get(j);
        let lv = self
            .mesh
            .place(&self.place)
            .and_then(|p| p.area_per_level.as_ref())
            .ok_or_else(|| Error::internal(format!("no per-level areas of '{}'", self.place)))?;
        let all = lv.level(k).map_err(|e| {
            Error::new(
                ErrorCode::BadData,
                format!("per-level areas of the mesh {}: {e}", self.mesh.name),
            )
        })?;
        let a = match &self.cells {
            None => all,
            Some(c) => Arc::new(c.iter().map(|&i| all[i]).collect()),
        };
        let scale = if self.normalise {
            let t: f64 = a.iter().sum();
            if t > 0.0 { 1.0 / t } else { 1.0 }
        } else {
            1.0
        };
        let _ = self.loaded[j].set((a, scale));
        Ok(self.loaded[j].get().expect("set").clone())
    }
}

/// The per-level weights a lane uses: the lane's levels along the level dimension, before
/// (`outer`) or after the folded block, and the level of each outer or inner index.
struct LaneLevels {
    outer: bool,
    /// Lane-local level of each outer (or inner) index.
    of: Vec<usize>,
    /// Weights and scale of each lane-local level.
    w: Vec<(Arc<Vec<f64>>, f64)>,
}

impl LaneLevels {
    fn new(lw: &LevelWeights, p: &VarPlan, lane: &TileBox) -> Result<Self> {
        let shape = lane.shape();
        let outer = lw.dim < p.b0;
        let (lo, hi) = if outer {
            (0, p.b0)
        } else {
            (p.b0 + p.nb, shape.len())
        };
        let n: usize = shape[lo..hi].iter().product();
        let stride: usize = shape[lw.dim + 1..hi].iter().product();
        let of = (0..n).map(|x| (x / stride) % shape[lw.dim]).collect();
        let w = lane.ranges[lw.dim]
            .clone()
            .map(|j| lw.level(j))
            .collect::<Result<_>>()?;
        Ok(Self { outer, of, w })
    }
}

/// Output row of each cell of the folded block.
#[derive(Debug, Clone)]
enum RowMap {
    One,
    /// Regular grids: row = flat index / nx.
    Div(usize),
    /// HEALPix rings.
    Each(Arc<Vec<u32>>),
    /// Latitude bands (`zonal_<DY>`): cell `f` adds to the bands `col[range(f)]` with the
    /// weights `w[range(f)]` (cells × bands, in band order).
    Bands(Arc<Csr>),
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
            Stat::Sum | Stat::Int => Box::new(State::<SumAcc>::new(var, p, self.stat, lane)),
            Stat::VertInt => Box::new(State::<CutSumAcc>::new(var, p, self.stat, lane)),
            Stat::Bottom => Box::new(State::<RankAcc>::new(var, p, self.stat, lane)),
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
    /// Per-level weights of the lane, loaded with its first tile.
    levels: Option<LaneLevels>,
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
            levels: None,
            p,
        }
    }

    /// Loads the lane's per-level weights (once).
    fn ensure_levels(&mut self) -> Result<()> {
        if let (None, Weights::PerLevel(lw)) = (&self.levels, &self.p.weights) {
            self.levels = Some(LaneLevels::new(lw, &self.p, &self.lane)?);
        }
        Ok(())
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
                let lv = self.levels.as_ref();
                match &t.values {
                    Values::F32(v) => fold_cells(v, &c, p, lv, &mut self.acc),
                    Values::F64(v) => fold_cells(v, &c, p, lv, &mut self.acc),
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
        fold_cells(v, &c, p, self.levels.as_ref(), &mut self.acc);
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
fn fold_cells<T: Copy + Into<f64>, A: Acc>(
    v: &[T],
    c: &Cells,
    p: &VarPlan,
    lv: Option<&LaneLevels>,
    acc: &mut [A],
) {
    let (o_n, i_n, n, f0) = (c.o_n, c.i_n, c.n, c.f0);
    // a single band still maps only some cells
    let one_row = p.nrows == 1 && !matches!(p.rows, RowMap::Bands(_));
    if let Some(lv) = lv {
        // per-level weights (field statistics, one output row): each accumulator adds its
        // cells in order with the weights of its level
        for o in 0..o_n {
            for i in 0..i_n {
                let (ws, s) = &lv.w[lv.of[if lv.outer { o } else { i }]];
                let a = &mut acc[o * i_n + i];
                let start = c.base + o * c.ostride + i;
                for (k, &w) in ws[f0..f0 + n].iter().enumerate() {
                    a.add(w * s, v[start + k * i_n].into());
                }
            }
        }
    } else if one_row && i_n == 1 {
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
    } else if one_row {
        // vertical statistics: cells (inner) side by side, levels in order
        for o in 0..o_n {
            let acc = &mut acc[o * i_n..(o + 1) * i_n];
            for k in 0..n {
                let w = match &p.weights {
                    Weights::Each(ws) => ws[f0 + k],
                    Weights::Const(w) => *w,
                    Weights::PerLevel(_) => unreachable!("per-level weights fold above"),
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
        Weights::PerLevel(_) => unreachable!("per-level weights fold in fold_cells"),
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
        (RowMap::Bands(m), _) => {
            let w = m.w.as_deref().expect("band weights");
            for k in 0..n {
                let x = val(k);
                for e in m.range(f0 + k) {
                    acc[m.col[e] as usize].add(w[e], x);
                }
            }
        }
        (rows, w) => {
            for k in 0..n {
                let f = f0 + k;
                let r = match rows {
                    RowMap::One | RowMap::Bands(_) => 0,
                    RowMap::Div(nx) => f / nx,
                    RowMap::Each(m) => m[f] as usize,
                };
                let wk = match w {
                    Weights::Const(c) => *c,
                    Weights::Each(ws) => ws[f],
                    Weights::PerLevel(_) => unreachable!("per-level weights fold in fold_cells"),
                };
                acc[r].add(wk, val(k));
            }
        }
    }
}

impl<A: Acc> FoldState for State<A> {
    fn push(&mut self, tile: Tile) -> Result<Vec<Tile>> {
        self.ensure_levels()?;
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
        if let Err(e) = self.ensure_levels() {
            return Some(Err(e));
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

/// A grid made by the operator: a lon-lat grid with one longitude (0) and the latitudes `lats`
/// (with `lat_bounds`, two per latitude, also the longitude bounds −180, 180, as cdo writes
/// `zonal_<DY>`), or (`unstructured_dim`) a 1-point unstructured grid over that dimension.
pub(crate) fn made_grid(
    g: &GridDesc,
    lats: Vec<f64>,
    lat_bounds: Option<Vec<f64>>,
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
            xbounds: lat_bounds.as_ref().map(|_| vec![-180.0, 180.0]),
            nv: if lat_bounds.is_some() { 2 } else { 0 },
            ybounds: lat_bounds,
            xunits: "degrees_east".into(),
            yunits: "degrees_north".into(),
        },
        xattrs,
        yattrs,
        ..FixedGrid::default()
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
pub(crate) fn selected_cells(g: &GridDesc) -> Vec<usize> {
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
/// with where they come from (`None`: a single cell). `fldint` (`areas`): the cell areas (m²,
/// `gridarea`) of the selected cells, not normalised.
pub(crate) fn fld_weights(
    g: &GridDesc,
    var: &VarDesc,
    srcs: &Sources,
    areas: bool,
) -> Result<(Weights, Option<area::AreaSource>)> {
    let n = g.size();
    if n <= 1 && !areas {
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
    if areas {
        let a = area::cell_areas(g.src.dataset(), dvar, &g.base, &read)?;
        let w: Vec<f64> = selected_cells(g).iter().map(|&c| a.values[c]).collect();
        return Ok((Weights::Each(Arc::new(w)), Some(a.source)));
    }
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
pub(crate) fn equal_weights_warning(op: &str, g: &GridDesc, vars: &[String]) -> String {
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
fn zonal(g: &GridDesc, op: &str) -> Result<(RowMap, Vec<f64>)> {
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
        .with_hint(if takes_zonaldes(op) {
            format!(
                "give latitude bands: {op},zonal_<DY> (DY the band width in degrees, e.g. \
                 {op},zonal_10), or remap to a regular grid first (remapcon/remapbil)"
            )
        } else {
            "remap to a regular grid first (remapcon/remapbil)".to_owned()
        })),
    }
}

// ---------------------------------------------------------------------------------------------
// Latitude bands: zonmean, zonsum, zonint with zonal_<DY>
// ---------------------------------------------------------------------------------------------

/// The latitude increment of cdo's predefined zonal description `zonal_<DY>`
/// (`generate_grid_zonal`, `grid_from_name.cc:63`; an increment below 1e-9 becomes 1). cdo takes
/// it as a grid description (`zonmean,zonal_10`); `zonaldes=zonal_10` is accepted as well.
fn zonal_increment(op: &str, arg: &str) -> Result<f64> {
    let bad = || {
        Error::bad_arguments(format!(
            "{op}: '{arg}' is not a latitude-band description zonal_<DY>"
        ))
        .with("operator", op.to_owned())
        .with_hint(format!(
            "{op},zonal_<DY> with DY the band width in degrees, e.g. {op},zonal_10 (other cdo \
             grid descriptions are not supported here)"
        ))
    };
    let v = arg.strip_prefix("zonaldes=").unwrap_or(arg);
    let inc: f64 = v
        .strip_prefix("zonal_")
        .and_then(|d| d.parse().ok())
        .filter(|d: &f64| d.is_finite())
        .ok_or_else(bad)?;
    let inc = if inc < 1e-9 { 1.0 } else { inc };
    if inc > 180.0 {
        return Err(Error::bad_arguments(format!(
            "{op}: zonal latitude increment {inc} out of range (max=180)"
        )));
    }
    Ok(inc)
}

/// Latitudes and their bounds (degrees, two per band) of `zonal_<DY>` as cdo makes them: round(180/DY)
/// bands with centres −90 + DY/2 + i·DY and bounds between the centres (`grid_gen_bounds`, so the
/// last bound passes 90° when DY does not divide 180: cdo then works with its sine, i.e. the
/// band ends at 180° − bound).
fn band_lats(inc: f64) -> (Vec<f64>, Vec<f64>) {
    let n = ((90.0 - -90.0) / inc + 0.5) as usize;
    let lats: Vec<f64> = (0..n).map(|i| -90.0 + inc / 2.0 + i as f64 * inc).collect();
    let bnds = if n > 1 {
        area::gen_axis_bounds(&lats)
    } else {
        vec![-90.0, 90.0]
    };
    (lats, bnds)
}

/// Corners (radians) of the written cells, `nv` per cell, as cdo's `generate_full_cell_grid`
/// makes them: stored bounds (unstructured, curvilinear), the lon-lat boxes of regular and
/// Gaussian grids, the HEALPix corners.
fn cell_corners(g: &GridDesc, op: &str) -> Result<(usize, Vec<f64>, Vec<f64>)> {
    let missing = || {
        Error::new(
            ErrorCode::NoCoordinates,
            format!("{op} with zonal_<DY> needs the cell corners, and this grid has none"),
        )
        .with("grid", g.kind.name())
        .with_hint("give the grid with cell bounds (-setgrid,<grid file>)")
    };
    match g.base.kind {
        GridKind::Healpix => {
            let c = g.healpix_cell_corners()?;
            let lon = c.iter().flat_map(|q| q.map(|p| p.0)).collect();
            let lat = c.iter().flat_map(|q| q.map(|p| p.1)).collect();
            Ok((4, lon, lat))
        }
        GridKind::Regular | GridKind::Gaussian => {
            let read = |name: &str| g.src.read_var(name);
            let (xb, yb) =
                area::reg2d_bounds(g.src.dataset(), &g.base, &read)?.ok_or_else(missing)?;
            let (ys, xs) = (g.sel[0].to_vec(), g.sel[1].to_vec());
            let (mut lon, mut lat) = (Vec::new(), Vec::new());
            for &j in &ys {
                for &i in &xs {
                    let (x0, x1, y0, y1) = (xb[2 * i], xb[2 * i + 1], yb[2 * j], yb[2 * j + 1]);
                    lon.extend([x0, x1, x1, x0]);
                    lat.extend([y0, y0, y1, y1]);
                }
            }
            Ok((4, lon, lat))
        }
        GridKind::Unstructured | GridKind::Curvilinear => {
            let c = g.coords()?.ok_or_else(missing)?;
            let (Some(mut lon), Some(mut lat)) = (c.xbounds, c.ybounds) else {
                return Err(missing());
            };
            let d2r = std::f64::consts::PI / 180.0;
            if area::is_degrees(Some(&c.xunits)) {
                lon.iter_mut().for_each(|v| *v *= d2r);
            }
            if area::is_degrees(Some(&c.yunits)) {
                lat.iter_mut().for_each(|v| *v *= d2r);
            }
            Ok((c.nv, lon, lat))
        }
        GridKind::Generic => Err(missing()),
    }
}

/// Centre latitudes (radians) of the written cells.
fn cell_center_lats(g: &GridDesc, op: &str) -> Result<Vec<f64>> {
    let d2r = std::f64::consts::PI / 180.0;
    match g.base.kind {
        GridKind::Healpix => Ok(g.healpix_cell_centers()?.1),
        GridKind::Regular | GridKind::Gaussian => {
            let (nx, _) = g.xy_size();
            let lats = g.base.yvals.clone().unwrap_or_default();
            let deg = area::is_degrees(g.base.y.as_ref().and_then(|a| a.units.as_deref()));
            Ok(g.sel[0]
                .to_vec()
                .iter()
                .flat_map(|&j| std::iter::repeat_n(lats[j], nx))
                .map(|y| if deg { y * d2r } else { y })
                .collect())
        }
        _ => {
            let c = g.coords()?.ok_or_else(|| {
                Error::new(
                    ErrorCode::NoCoordinates,
                    format!(
                        "{op} with zonal_<DY> needs the cell latitudes, and this grid has none"
                    ),
                )
            })?;
            let deg = area::is_degrees(Some(&c.yunits));
            Ok(c.yvals
                .into_iter()
                .map(|y| if deg { y * d2r } else { y })
                .collect())
        }
    }
}

/// Whether cdo treats the cells as lon-lat cells, edges along meridians and circles of latitude
/// (`get_lonlat_circle_index`, `remap_conserv.cc:120`): four corners in one of the two lon-lat
/// patterns, checked on a sample of the cells. `Some(true)` when the longitude edges are the
/// corners (1,2) and (3,0), `Some(false)` when (0,1) and (2,3).
fn lonlat_cells(nv: usize, lon: &[f64], lat: &[f64]) -> Option<bool> {
    if nv != 4 {
        return None;
    }
    let n = lon.len() / 4;
    let incr = if n < 100 { 1 } else { n / 30 - 1 };
    let (mut num, mut eq1, mut eq0) = (0, 0, 0);
    for i in (0..n).step_by(incr) {
        let (x, y) = (&lon[4 * i..4 * i + 4], &lat[4 * i..4 * i + 4]);
        num += 1;
        if x[1] == x[2] && x[3] == x[0] && y[0] == y[1] && y[2] == y[3] {
            eq1 += 1;
        } else if x[0] == x[1] && x[2] == x[3] && y[1] == y[2] && y[3] == y[0] {
            eq0 += 1;
        }
    }
    if num == eq1 {
        Some(true)
    } else if num == eq0 {
        Some(false)
    } else {
        None
    }
}

/// The band matrix of `zonal_<DY>` (bands × written cells) as cdo's `Zonstat.cc` builds it:
/// - `zonmean`, `zonint` (`remap_weights_zonal`, `remap_conserv.cc:768`): the cells whose corner
///   latitudes reach into the band (compared in units of 1e-9 rad, truncated as cdo does) with
///   the area of their part inside the band (`yac_cell_lat_clipping`; here
///   [`cdors_mesh::geom::band_area`] and [`cdors_mesh::geom::lonlat_band_area`], equal up to
///   rounding), scaled so that they add up to the band's area (`correct_weights`), positive
///   ones only; `zonint` multiplies them by R²;
/// - `zonsum` (`gen_indices_zonal`): the cells whose centre latitude lies in [lo, hi), weight 1.
///
/// Missing values drop out when the matrix is applied: `zonmean` renormalises over the valid
/// cells, `zonint` and `zonsum` sum them; a band without valid cells is missing.
fn band_matrix(g: &GridDesc, op: &str, bnds: &[f64]) -> Result<Csr> {
    let d2r = std::f64::consts::PI / 180.0;
    let nb = bnds.len() / 2;
    let n = g.size();
    // calc_remap_indices: int latBounds = 1e9 * radians (truncated), swapped into order
    let scale = 1e9;
    let ibound = |b: usize| -> (i32, i32) {
        let (a, c) = (
            (scale * bnds[2 * b] * d2r) as i32,
            (scale * bnds[2 * b + 1] * d2r) as i32,
        );
        (a.min(c), a.max(c))
    };
    let mut rows: Vec<Vec<(u32, f64)>> = vec![Vec::new(); nb];
    if op == "zonsum" {
        let lat = cell_center_lats(g, op)?;
        for (b, row) in rows.iter_mut().enumerate() {
            let (lo, hi) = ibound(b);
            for (c, &y) in lat.iter().enumerate() {
                let iy = (scale * y) as i32;
                if iy < hi && iy >= lo {
                    row.push((c as u32, 1.0));
                }
            }
        }
    } else {
        let (nv, lon, lat) = cell_corners(g, op)?;
        if nv < 2 || lon.len() != n * nv {
            return Err(Error::new(
                ErrorCode::NoCoordinates,
                format!("{op} with zonal_<DY>: the grid has no usable cell corners"),
            ));
        }
        let ll = lonlat_cells(nv, &lon, &lat);
        let ilat: Vec<(i32, i32)> = (0..n)
            .map(|c| {
                let y = &lat[c * nv..(c + 1) * nv];
                let (lo, hi) = y
                    .iter()
                    .fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), &v| {
                        (a.min(v), b.max(v))
                    });
                ((scale * lo) as i32, (scale * hi) as i32)
            })
            .collect();
        let xyz: Vec<[f64; 3]> = lon
            .iter()
            .zip(&lat)
            .map(|(&x, &y)| cdors_mesh::geom::xyz(x, y))
            .collect();
        let radius = area::planet_radius(&crate::model::Grid::new(GridKind::Generic, vec![], 0, 0));
        let factor = if op == "zonint" { radius * radius } else { 1.0 };
        let half_pi = std::f64::consts::FRAC_PI_2;
        for (b, row) in rows.iter_mut().enumerate() {
            let (lo, hi) = ibound(b);
            let (mut y0, mut y1) = (bnds[2 * b] * d2r, bnds[2 * b + 1] * d2r);
            if y0 > y1 {
                std::mem::swap(&mut y0, &mut y1);
            }
            // latitude_area: sphere segments 2π(1 − cos(π/2 − φ))
            let seg = |phi: f64| 2.0 * std::f64::consts::PI * (1.0 - (half_pi - phi).cos());
            let band = seg(y0) - seg(y1);
            let (z0, z1) = (y0.sin(), y1.sin());
            for c in 0..n {
                let (cl, ch) = ilat[c];
                if !(cl < hi && ch >= lo) {
                    continue;
                }
                let a = match ll {
                    Some(lonfirst) => {
                        let x = &lon[c * 4..c * 4 + 4];
                        let y = &lat[c * 4..c * 4 + 4];
                        let dlon = if lonfirst { x[1] - x[0] } else { x[2] - x[1] };
                        let dlon = (dlon + std::f64::consts::PI)
                            .rem_euclid(2.0 * std::f64::consts::PI)
                            - std::f64::consts::PI;
                        let (ya, yb) = if lonfirst { (y[0], y[2]) } else { (y[1], y[3]) };
                        cdors_mesh::geom::lonlat_band_area(dlon, ya, yb, z0, z1)
                    }
                    None => cdors_mesh::geom::band_area(&xyz[c * nv..(c + 1) * nv], z0, z1),
                };
                // drop touching cells (exactly 0 in yac; rounding here)
                if a > 1e-14 * band {
                    row.push((c as u32, a));
                }
            }
            // correct_weights: w / A, yac_correct_weights (rescaled to sum 1, up to 10 times),
            // then × A
            let mut w: Vec<f64> = row.iter().map(|e| e.1 / band).collect();
            for _ in 0..10 {
                let sum: f64 = w.iter().sum();
                if (1.0 - sum).abs() < 1e-12 || sum.abs() < 1e-12 {
                    break;
                }
                let s = 1.0 / sum;
                w.iter_mut().for_each(|x| *x *= s);
            }
            for (e, x) in row.iter_mut().zip(w) {
                e.1 = x * band * factor;
            }
            row.retain(|e| e.1 > 0.0);
        }
    }
    let mut row_ptr = vec![0usize];
    let (mut col, mut w) = (Vec::new(), Vec::new());
    for r in &rows {
        for &(c, x) in r {
            col.push(c);
            w.push(x);
        }
        row_ptr.push(col.len());
    }
    Csr::new(row_ptr, col, Some(w), n).map_err(|e| Error::internal(e.to_string()))
}

/// Bands of `zonal_<DY>`: (row map of the cells, latitudes, latitude bounds).
fn zonal_bands(g: &GridDesc, op: &str, inc: f64) -> Result<(RowMap, Vec<f64>, Vec<f64>)> {
    let (lats, bnds) = band_lats(inc);
    let m = band_matrix(g, op, &bnds)?;
    Ok((RowMap::Bands(Arc::new(m.transpose())), lats, bnds))
}

pub(crate) fn add_cell_method(v: &mut VarDesc, method: &str) {
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

pub(crate) fn block_of(v: &VarDesc, role: DimRole) -> Result<(usize, usize)> {
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

/// Space statistics of variables with levels on a mesh with a vertical: `fld*` and `vert*` work
/// on the levels whose dry cells `setgrid` masked (`fld*` with per-level areas where the place
/// has them, `vert*` with the mesh's layers, each cell's dry levels missing); `zon*` (latitude
/// bands) are not implemented on mesh levels yet, and levels the mesh's rules do not recognise
/// (no dry-level mask) are refused. 2-D variables are fine.
fn refuse_mesh_levels(op: &str, fam: Family, input: &Desc) -> Result<()> {
    for v in &input.vars {
        let Some(m) = v.grid.and_then(|g| input.grids[g].base.mesh.as_ref()) else {
            continue;
        };
        if m.vertical.is_some()
            && let Some(z) = v.dims.iter().find(|d| d.role == DimRole::Vertical)
        {
            if fam != Family::Zon && v.expr.wet_mask().is_some() {
                continue;
            }
            let (what, hint) = if fam != Family::Zon {
                (
                    format!(
                        "its levels ('{}', {}) are not the mesh's layers or interfaces, so the \
                         dry cells of each level are not known",
                        z.name, z.size
                    ),
                    "select the variable with the mesh's level dimension (FESOM2: nz1 or nz)",
                )
            } else {
                (
                    format!(
                        "{op} of variables with levels ('{}') on a mesh is not implemented yet",
                        z.name
                    ),
                    "fld* and vert* statistics work on the mesh's levels; for 2-D variables \
                     all statistics work",
                )
            };
            return Err(Error::new(
                ErrorCode::NotImplemented,
                format!("{op} of '{}' on the mesh {}: {what}", v.name, m.name),
            )
            .with("variable", v.name.clone())
            .with_hint(hint));
        }
    }
    Ok(())
}

/// `bottomvalue`: weights that rank the levels from the top, so that the valid value of the
/// largest weight is the one nearest the bottom. On a mesh the deeper mesh level ranks higher (so
/// each cell's own bottom, its dry levels being missing); otherwise cdo's rule (`Selsurface.cc`):
/// low level values are the bottom, high values when the levels decrease, and the other way
/// round when the levels are not negative and the axis is `positive = down`.
fn bottom_ranks(
    za: Option<&crate::model::ZAxis>,
    nlev: usize,
    mesh: Option<&vertical::MeshLevels>,
) -> Weights {
    if let Some(m) = mesh {
        return Weights::Each(Arc::new(m.levels.iter().map(|&k| (k + 1) as f64).collect()));
    }
    let Some(lev) = za
        .map(|z| &z.values)
        .filter(|l| l.len() == nlev && nlev > 1)
    else {
        return Weights::Const(1.0);
    };
    let positive = lev[0] >= 0.0 || lev[nlev - 1] >= 0.0;
    let reverse = lev[0] > lev[nlev - 1];
    let down = za.and_then(|z| z.positive.as_deref()) == Some("down");
    // bottom at the first index (cdo: layer_value_min) or at the last (layer_value_max)
    let first = !reverse != (positive && down);
    Weights::Each(Arc::new(
        (0..nlev)
            .map(|j| {
                if first {
                    (nlev - j) as f64
                } else {
                    (j + 1) as f64
                }
            })
            .collect(),
    ))
}

/// Per-level weights of `v` on grid `g` if it has levels on a mesh place with per-level areas;
/// with the bytes of the mesh levels it reads that `loaded` (mesh name, place, level) does not
/// hold yet.
pub(crate) fn level_weights(
    g: &GridDesc,
    v: &VarDesc,
    normalise: bool,
    loaded: &mut std::collections::HashSet<(String, String, usize)>,
) -> Option<(Arc<LevelWeights>, u64)> {
    let mesh = g.base.mesh.clone()?;
    let place = g.base.mesh_place()?;
    let lv = place.area_per_level.as_ref()?;
    let (mask, dim, levels) = v.expr.wet_mask()?;
    if mask.place != place.name {
        return None;
    }
    let cells = selected_cells(g);
    let whole = cells.len() == place.size && cells.iter().enumerate().all(|(k, &c)| k == c);
    let n = levels.len();
    let mut bytes = 0;
    for j in 0..n {
        let k = levels.get(j);
        if k < lv.nlev() && loaded.insert((mesh.name.clone(), place.name.clone(), k)) {
            bytes += 8 * place.size as u64;
        }
    }
    if !whole {
        bytes += 8 * (n * cells.len()) as u64;
    }
    let lw = LevelWeights {
        dim,
        place: place.name.clone(),
        levels,
        cells: (!whole).then(|| Arc::new(cells)),
        normalise,
        loaded: (0..n).map(|_| std::sync::OnceLock::new()).collect(),
        mesh,
    };
    Some((Arc::new(lw), bytes))
}

/// Output description of a space statistic.
pub fn describe(node: &OpNode, mut inputs: Vec<Desc>, srcs: &Sources) -> Result<Desc> {
    let name = node.name.as_str();
    let (fam, stat) =
        parse(name).ok_or_else(|| Error::internal(format!("'{name}' is not a space statistic")))?;
    let input = inputs.remove(0);
    refuse_mesh_levels(name, fam, &input)?;
    let mut out = input.clone();
    let mut plans = Vec::with_capacity(out.vars.len());
    let mut info = None;
    let zonaldes = match node.args.first() {
        Some(a) if fam == Family::Zon => Some(zonal_increment(name, a)?),
        _ => None,
    };
    if name == "zonint" && zonaldes.is_none() {
        return Err(Error::bad_arguments(
            "zonint needs latitude bands zonal_<DY> (cdo 2.6.5 also integrates the rows of \
             regular grids; cdors does not)",
        )
        .with("operator", name.to_owned())
        .with_hint("zonint,zonal_<DY> with DY the band width in degrees, e.g. zonint,zonal_10"));
    }
    match fam {
        Family::Fld | Family::Zon => {
            let weighting = (fam == Family::Fld)
                .then(|| area::fldstat_weighting(name))
                .flatten();
            let weighted = matches!(
                weighting,
                Some(SpaceWeighting::Weights | SpaceWeighting::Areas)
            );
            let by_area = weighting == Some(SpaceWeighting::Areas);
            // per grid: the grid made, its dimensions, rows; weights per (grid, area variable)
            let mut made: HashMap<usize, (GridDesc, Vec<VarDim>, RowMap, usize)> = HashMap::new();
            type Cached = (Weights, Option<area::AreaSource>);
            let mut wcache: HashMap<(usize, Option<String>), Cached> = HashMap::new();
            // `--plan`: where each variable's weights come from; variables weighted equally by grid
            let mut winfo = Vec::new();
            let mut equal: Vec<(usize, Vec<String>)> = Vec::new();
            // per-level mesh areas: what each variable loads (`--plan` memory)
            let mut minfo = Vec::new();
            let mut mloaded = std::collections::HashSet::new();
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
                            let (gd, dims) = made_grid(g, vec![0.0], None, ud.as_deref());
                            (gd, dims, RowMap::One, 1)
                        }
                        _ => {
                            let (rows, lats, bnds) = match zonaldes {
                                Some(inc) => {
                                    let (r, l, b) = zonal_bands(g, name, inc)?;
                                    (r, l, Some(b))
                                }
                                None => {
                                    let (r, l) = zonal(g, name)?;
                                    (r, l, None)
                                }
                            };
                            let n = lats.len();
                            let (gd, dims) = made_grid(g, lats, bnds, None);
                            (gd, dims, rows, n)
                        }
                    };
                    slot.insert(entry);
                }
                let (_, dims, rows, nrows) = &made[&gi];
                let per_level = if weighted && fam == Family::Fld {
                    level_weights(g, v, !by_area, &mut mloaded)
                } else {
                    None
                };
                let weights = if let Some((lw, bytes)) = per_level {
                    let src = format!(
                        "areas of each level of place '{}' of the mesh {} (level k: the area at \
                         its top interface)",
                        lw.place, lw.mesh.name
                    );
                    winfo.push(serde_json::json!({
                        "variable": v.name,
                        "area_weights": "mesh_per_level",
                        "description": src,
                    }));
                    minfo.push(serde_json::json!({
                        "variable": v.name,
                        "weights": "per_level",
                        "mesh": lw.mesh.name,
                        "place": lw.place,
                        "levels": lw.levels.len(),
                        "bytes": bytes,
                    }));
                    Weights::PerLevel(lw)
                } else if weighted {
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
                            let c = fld_weights(g, v, srcs, by_area)?;
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
            if let (Some(i), false) = (info.as_mut(), minfo.is_empty()) {
                i["mesh_weights"] = serde_json::json!(minfo);
            }
            for (gi, (gd, _, _, _)) in made {
                out.grids[gi] = gd;
            }
        }
        Family::Vert => {
            let weighting = area::vertstat_weighting(name).unwrap_or(VertWeighting::None);
            let range = match node.args.first() {
                Some(a) if name == "vertint" => Some(vertical::depth_range(name, a)?),
                _ => None,
            };
            for v in &mut out.vars {
                let (b0, nb) = block_of(v, DimRole::Vertical)?;
                let nlev = if nb == 1 { v.dims[b0].size } else { 1 };
                let za = v.zaxis.map(|z| &input.zaxes[z].axis);
                let zdim = v.dims.get(b0).map_or(String::new(), |d| d.name.clone());
                let mesh = (nb == 1).then(|| vertical::mesh_levels(v, b0)).flatten();
                let lw = area::layer_weights(za, nlev, true, false);
                let weights = if stat == Stat::Bottom {
                    bottom_ranks(za, nlev, mesh.as_ref())
                } else if weighting == VertWeighting::None || nb == 0 {
                    Weights::Const(1.0)
                } else if let Some(m) = &mesh {
                    // the mesh's layers; dry levels are missing and drop out
                    let layers = m
                        .layers()
                        .ok_or_else(|| vertical::no_layers(name, v, &zdim, Some(m)))?;
                    let dz = vertical::cut_thickness(&layers, range);
                    if weighting == VertWeighting::Weights {
                        let mean = dz.iter().sum::<f64>() / nlev as f64;
                        Weights::Each(Arc::new(dz.iter().map(|t| t / mean).collect()))
                    } else {
                        Weights::Each(Arc::new(dz))
                    }
                } else if range.is_some() {
                    let layers = vertical::axis_layers(za, nlev)
                        .ok_or_else(|| vertical::no_layers(name, v, &zdim, None))?;
                    Weights::Each(Arc::new(vertical::cut_thickness(&layers, range)))
                } else {
                    match weighting {
                        VertWeighting::Weights => Weights::Each(Arc::new(lw.weights)),
                        _ => Weights::Each(Arc::new(lw.thickness)),
                    }
                };
                if lw.status == 0
                    && nlev > 1
                    && weighting != VertWeighting::None
                    && mesh.is_none()
                    && stat != Stat::Bottom
                {
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
                    v.dims.remove(b0);
                    v.zaxis = None;
                    if stat != Stat::Bottom {
                        add_cell_method(v, &format!("{zdim}: {}", stat.cell_method()));
                    }
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
