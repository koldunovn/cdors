//! Planner: operator tree -> dataset descriptions -> stages.
//!
//! Every operator maps the description(s) of its input(s) to the description of its output
//! ([`Desc`]) without touching data (`describe`, in `crate::ops`). A description lists the output
//! variables; each variable carries an expression ([`Expr`]) that says how its values are computed
//! from stored variables ("leaves"):
//!
//! - **selections** never copy data: they compose an [`IndexMap`] into every leaf along one
//!   dimension (`seltimestep` along time, `sellevel` along the vertical axis, `sellonlatbox` along
//!   the horizontal dimensions). A leaf therefore knows, per stored dimension, which stored index
//!   every output index reads, and the planner derives from it the chunks to read: chunks that
//!   hold no selected index are never read;
//! - **pointwise** operators wrap the expressions of their inputs ([`Expr::Unary`],
//!   [`Expr::Binary`]); a second input with one timestep is broadcast with an [`IndexMap::Const`]
//!   map along time, as cdo does;
//! - **reductions and whole-extent operators** (Tasks 6-8) end a stage: the expressions so far
//!   produce tiles, which a [`stage::FoldKernel`] consumes in a fixed order.
//!
//! [`build`] turns the parsed command into a [`Plan`]: the opened sources, the output
//! description and the stages. `tiling` splits each output variable into tiles aligned to the
//! chunks of its primary leaf; `crate::exec` runs them.

pub mod explain;
pub mod stage;
pub mod tiling;

use crate::chain::{Command, Input, OpNode};
use crate::error::{Error, ErrorCode, Result};
use crate::io::ChunkSource;
use crate::model::{
    Attrs, DType, DimRole, Grid, GridKind, HealpixOrder, TimeAxis, VarDim, VarKind, ZAxis,
};
use std::sync::Arc;

/// Maps output indices along one dimension to indices of the dimension below.
#[derive(Debug, Clone, PartialEq)]
pub enum IndexMap {
    /// `start, start+1, ..., start+len-1`.
    Range { start: usize, len: usize },
    /// An explicit list (any order, repeats allowed).
    List(Arc<Vec<usize>>),
    /// Every output index reads index `idx` (broadcast).
    Const { idx: usize, len: usize },
}

impl IndexMap {
    pub fn identity(n: usize) -> Self {
        Self::Range { start: 0, len: n }
    }

    pub fn from_list(v: Vec<usize>) -> Self {
        let n = v.len();
        if n > 0 && v.windows(2).all(|w| w[1] == w[0] + 1) {
            return Self::Range {
                start: v[0],
                len: n,
            };
        }
        Self::List(Arc::new(v))
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Range { len, .. } | Self::Const { len, .. } => *len,
            Self::List(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    pub fn get(&self, i: usize) -> usize {
        match self {
            Self::Range { start, .. } => start + i,
            Self::List(v) => v[i],
            Self::Const { idx, .. } => *idx,
        }
    }

    /// `result[i] = self[sel[i]]`: applies a selection `sel` made on the output of `self`.
    pub fn compose(&self, sel: &IndexMap) -> IndexMap {
        match (self, sel) {
            (Self::Range { start, .. }, Self::Range { start: s, len }) => Self::Range {
                start: start + s,
                len: *len,
            },
            (_, Self::Const { idx, len }) => Self::Const {
                idx: self.get(*idx),
                len: *len,
            },
            (Self::Const { idx, .. }, _) => Self::Const {
                idx: *idx,
                len: sel.len(),
            },
            _ => Self::from_list((0..sel.len()).map(|i| self.get(sel.get(i))).collect()),
        }
    }

    pub fn to_vec(&self) -> Vec<usize> {
        (0..self.len()).map(|i| self.get(i)).collect()
    }

    pub fn is_identity(&self, n: usize) -> bool {
        matches!(self, Self::Range { start: 0, len } if *len == n)
    }
}

/// A stored variable read by an expression.
#[derive(Debug, Clone)]
pub struct Leaf {
    /// Index into [`Plan::sources`].
    pub src: usize,
    pub var: String,
    /// One entry per stored dimension (storage order): the output dimension it follows and the
    /// map from output index to stored index. Output dimensions not listed are broadcast.
    pub maps: Vec<(usize, IndexMap)>,
}

/// Pointwise operators with one input.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UnOp {
    AddC(f64),
    SubC(f64),
    MulC(f64),
    DivC(f64),
}

/// Pointwise operators with two inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    /// `ifthen(mask, data)`: data where the mask is non-zero and not missing, else missing.
    IfThen,
}

/// How the values of one output variable are computed. Each node's result is rounded to its
/// `dtype` (cdo computes float32 fields in float32, operator by operator).
#[derive(Debug, Clone)]
pub enum Expr {
    Leaf(Leaf),
    Unary(UnOp, DType, Box<Expr>),
    Binary(BinOp, DType, Box<Expr>, Box<Expr>),
}

impl Expr {
    /// Applies `sel` along output dimension `dim` to every leaf that follows it.
    pub fn select(&mut self, dim: usize, sel: &IndexMap) {
        match self {
            Self::Leaf(l) => {
                for (d, m) in &mut l.maps {
                    if *d == dim {
                        *m = m.compose(sel);
                    }
                }
            }
            Self::Unary(_, _, e) => e.select(dim, sel),
            Self::Binary(_, _, a, b) => {
                a.select(dim, sel);
                b.select(dim, sel);
            }
        }
    }

    /// Renumbers output dimensions: `f(old) = (new, optional extra selection)`.
    pub fn retarget(&mut self, f: &dyn Fn(usize) -> (usize, Option<IndexMap>)) {
        match self {
            Self::Leaf(l) => {
                for (d, m) in &mut l.maps {
                    let (nd, sel) = f(*d);
                    *d = nd;
                    if let Some(s) = sel {
                        *m = m.compose(&s);
                    }
                }
            }
            Self::Unary(_, _, e) => e.retarget(f),
            Self::Binary(_, _, a, b) => {
                a.retarget(f);
                b.retarget(f);
            }
        }
    }

    /// Leaves in depth-first order.
    pub fn leaves(&self) -> Vec<&Leaf> {
        let mut out = Vec::new();
        fn walk<'a>(e: &'a Expr, out: &mut Vec<&'a Leaf>) {
            match e {
                Expr::Leaf(l) => out.push(l),
                Expr::Unary(_, _, a) => walk(a, out),
                Expr::Binary(_, _, a, b) => {
                    walk(a, out);
                    walk(b, out);
                }
            }
        }
        walk(self, &mut out);
        out
    }

    /// Data type of the result.
    pub fn dtype(&self, plan_sources: &[Arc<dyn ChunkSource>]) -> DType {
        match self {
            Self::Leaf(l) => leaf_dtype(plan_sources[l.src].as_ref(), &l.var),
            Self::Unary(_, t, _) | Self::Binary(_, t, _, _) => *t,
        }
    }

    /// Short text form for `--plan`.
    pub fn describe(&self, sources: &[Arc<dyn ChunkSource>]) -> String {
        match self {
            Self::Leaf(l) => format!("{}:{}", sources[l.src].dataset().source, l.var),
            Self::Unary(op, _, e) => {
                let (n, c) = match op {
                    UnOp::AddC(c) => ("addc", c),
                    UnOp::SubC(c) => ("subc", c),
                    UnOp::MulC(c) => ("mulc", c),
                    UnOp::DivC(c) => ("divc", c),
                };
                format!("{n}({}, {c})", e.describe(sources))
            }
            Self::Binary(op, _, a, b) => format!(
                "{}({}, {})",
                format!("{op:?}").to_ascii_lowercase(),
                a.describe(sources),
                b.describe(sources)
            ),
        }
    }
}

/// Type of the values a stored variable decodes to (packed integers unpack to f32 or f64).
pub fn leaf_dtype(src: &dyn ChunkSource, var: &str) -> DType {
    match src.dataset().var(var) {
        Some(v) if v.encoding.unpacked_f32 => DType::F32,
        _ => DType::F64,
    }
}

/// A horizontal grid of an output description: the grid as read from `src`, the selection along
/// each of its dimensions, and the kind written (a HEALPix subset is written as unstructured).
#[derive(Clone)]
pub struct GridDesc {
    pub kind: GridKind,
    pub base: Grid,
    pub src: Arc<dyn ChunkSource>,
    /// One map per horizontal dimension of `base` (`[y, x]` or `[cell]`).
    pub sel: Vec<IndexMap>,
    /// Longitudes of a regular grid after `sellonlatbox` (cdo shifts them to a monotonic range).
    pub xvals: Option<Vec<f64>>,
    /// A grid made by an operator (the point of `fld*`, the latitudes of `zon*`): coordinates
    /// and their attributes as written, instead of reading them from `src`.
    pub fixed: Option<Arc<FixedGrid>>,
}

/// Coordinates of a grid an operator made, as written.
#[derive(Debug, Clone)]
pub struct FixedGrid {
    pub coords: GridCoords,
    pub xattrs: Attrs,
    pub yattrs: Attrs,
}

/// Coordinates of a grid as written: values and optional vertex bounds (`nv` per point).
#[derive(Debug, Clone, Default)]
pub struct GridCoords {
    pub xvals: Vec<f64>,
    pub yvals: Vec<f64>,
    pub xbounds: Option<Vec<f64>>,
    pub ybounds: Option<Vec<f64>>,
    pub nv: usize,
    /// Units of the written coordinates (`degrees_*` or `radian`).
    pub xunits: String,
    pub yunits: String,
}

impl GridDesc {
    pub fn size(&self) -> usize {
        self.sel.iter().map(IndexMap::len).product()
    }

    /// Number of columns and rows (rows = 0 for 1-D grids).
    pub fn xy_size(&self) -> (usize, usize) {
        match self.sel.len() {
            2 => (self.sel[1].len(), self.sel[0].len()),
            _ => (self.sel[0].len(), 0),
        }
    }

    fn is_subset(&self) -> bool {
        let shape: Vec<usize> = if self.base.ysize > 0 && self.sel.len() == 2 {
            vec![self.base.ysize, self.base.xsize]
        } else {
            vec![self.base.size]
        };
        self.sel.iter().zip(shape).any(|(m, n)| !m.is_identity(n))
    }

    /// Reads (and subsets) the coordinates. `None` for HEALPix (analytic) and generic grids.
    pub fn coords(&self) -> Result<Option<GridCoords>> {
        if let Some(f) = &self.fixed {
            return Ok(Some(f.coords.clone()));
        }
        let b = &self.base;
        let units = |ax: &Option<crate::model::CoordAxis>, d: &str| {
            ax.as_ref()
                .and_then(|a| a.units.clone())
                .unwrap_or_else(|| d.to_owned())
        };
        match (b.kind, self.kind) {
            (GridKind::Healpix, GridKind::Unstructured) => {
                let hp = b.healpix.as_ref().expect("healpix grid");
                let cells: Vec<u64> = match &hp.index_var {
                    Some(iv) => {
                        let idx = self.src.read_var(iv)?;
                        self.sel[0]
                            .to_vec()
                            .iter()
                            .map(|&i| idx[i] as u64)
                            .collect()
                    }
                    None => self.sel[0].to_vec().iter().map(|&i| i as u64).collect(),
                };
                let (xs, ys) = healpix_centers(hp.nside, hp.order, &cells);
                Ok(Some(GridCoords {
                    xvals: xs,
                    yvals: ys,
                    xbounds: None,
                    ybounds: None,
                    nv: 0,
                    xunits: "radian".into(),
                    yunits: "radian".into(),
                }))
            }
            (GridKind::Healpix | GridKind::Generic, _) => Ok(None),
            (GridKind::Regular | GridKind::Gaussian, _) => {
                let xall = b.xvals.clone().unwrap_or_default();
                let yall = b.yvals.clone().unwrap_or_default();
                let (ys, xs) = (&self.sel[0], &self.sel[1]);
                let xvals = match &self.xvals {
                    Some(x) => x.clone(),
                    None => xs.to_vec().iter().map(|&i| xall[i]).collect(),
                };
                let yvals = ys.to_vec().iter().map(|&i| yall[i]).collect();
                let rb = |ax: &Option<crate::model::CoordAxis>,
                          sel: &IndexMap|
                 -> Result<Option<Vec<f64>>> {
                    let Some(bv) = ax.as_ref().and_then(|a| a.bounds_var.clone()) else {
                        return Ok(None);
                    };
                    if self.src.dataset().var(&bv).is_none() {
                        return Ok(None);
                    }
                    let all = self.src.read_var(&bv)?;
                    Ok(Some(
                        sel.to_vec()
                            .iter()
                            .flat_map(|&i| [all[2 * i], all[2 * i + 1]])
                            .collect(),
                    ))
                };
                let mut xb = rb(&b.x, xs)?;
                if self.xvals.is_some()
                    && let Some(xbv) = xb.as_mut()
                {
                    // shift bounds along with the shifted centres
                    for (k, i) in xs.to_vec().into_iter().enumerate() {
                        let d = xvals[k] - xall[i];
                        xbv[2 * k] += d;
                        xbv[2 * k + 1] += d;
                    }
                }
                let yb = rb(&b.y, ys)?;
                Ok(Some(GridCoords {
                    nv: if xb.is_some() { 2 } else { 0 },
                    xvals,
                    yvals,
                    xbounds: xb,
                    ybounds: yb,
                    xunits: units(&b.x, "degrees_east"),
                    yunits: units(&b.y, "degrees_north"),
                }))
            }
            (GridKind::Curvilinear | GridKind::Unstructured, _) => {
                let (Some(xa), Some(ya)) = (&b.x, &b.y) else {
                    return Ok(None);
                };
                // flat cell indices of the selection, in output order
                let cells: Vec<usize> = if self.sel.len() == 2 {
                    let (ys, xs) = (self.sel[0].to_vec(), self.sel[1].to_vec());
                    ys.iter()
                        .flat_map(|&j| xs.iter().map(move |&i| j * b.xsize + i))
                        .collect()
                } else {
                    self.sel[0].to_vec()
                };
                let pick = |all: &[f64]| -> Vec<f64> { cells.iter().map(|&c| all[c]).collect() };
                let xvals = pick(&self.src.read_var(&xa.var)?);
                let yvals = pick(&self.src.read_var(&ya.var)?);
                let nv = b.nvertex.unwrap_or(0);
                let bounds = |ax: &crate::model::CoordAxis| -> Result<Option<Vec<f64>>> {
                    match &ax.bounds_var {
                        Some(bv) if nv > 0 && self.src.dataset().var(bv).is_some() => {
                            let all = self.src.read_var(bv)?;
                            Ok(Some(
                                cells
                                    .iter()
                                    .flat_map(|&c| all[c * nv..(c + 1) * nv].iter().copied())
                                    .collect(),
                            ))
                        }
                        _ => Ok(None),
                    }
                };
                let xb = bounds(xa)?;
                let yb = bounds(ya)?;
                Ok(Some(GridCoords {
                    nv: if xb.is_some() { nv } else { 0 },
                    xvals,
                    yvals,
                    xbounds: xb,
                    ybounds: yb,
                    xunits: units(&b.x, "degrees_east"),
                    yunits: units(&b.y, "degrees_north"),
                }))
            }
        }
    }

    /// Whether the written grid keeps the HEALPix grid mapping.
    pub fn is_healpix(&self) -> bool {
        self.kind == GridKind::Healpix && !self.is_subset()
    }
}

/// HEALPix cell centres (radians) of `cells` in the given order.
pub fn healpix_centers(nside: u64, order: HealpixOrder, cells: &[u64]) -> (Vec<f64>, Vec<f64>) {
    let depth = nside.trailing_zeros() as u8;
    let layer = cdshealpix::nested::get(depth);
    let mut xs = Vec::with_capacity(cells.len());
    let mut ys = Vec::with_capacity(cells.len());
    for &c in cells {
        let h = match order {
            HealpixOrder::Nested => c,
            HealpixOrder::Ring => layer.from_ring(c),
        };
        let (lon, lat) = layer.center(h);
        xs.push(lon);
        ys.push(lat);
    }
    (xs, ys)
}

/// The time axis of a description with the raw stored values (written back unchanged).
#[derive(Debug, Clone)]
pub struct TimeDesc {
    pub axis: TimeAxis,
    /// Attributes of the time variable.
    pub attrs: Attrs,
    pub raw: Vec<f64>,
    pub raw_bounds: Option<Vec<[f64; 2]>>,
}

impl TimeDesc {
    pub fn len(&self) -> usize {
        self.raw.len()
    }

    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }

    pub fn select(&mut self, sel: &IndexMap) {
        let idx = sel.to_vec();
        self.axis.steps = idx.iter().map(|&i| self.axis.steps[i]).collect();
        if let Some(b) = &mut self.axis.bounds {
            *b = idx.iter().map(|&i| b[i]).collect();
        }
        self.raw = idx.iter().map(|&i| self.raw[i]).collect();
        if let Some(b) = &mut self.raw_bounds {
            *b = idx.iter().map(|&i| b[i]).collect();
        }
    }
}

/// A vertical axis with the attributes of its coordinate variable.
#[derive(Debug, Clone)]
pub struct ZDesc {
    pub axis: ZAxis,
    pub attrs: Attrs,
    /// Attributes of the bounds variable, if any.
    pub bounds_var: Option<String>,
}

impl ZDesc {
    pub fn select(&mut self, sel: &IndexMap) {
        let idx = sel.to_vec();
        self.axis.values = idx.iter().map(|&i| self.axis.values[i]).collect();
        if let Some(b) = &mut self.axis.bounds {
            *b = idx.iter().map(|&i| b[i]).collect();
        }
    }
}

/// One output variable.
#[derive(Debug, Clone)]
pub struct VarDesc {
    pub name: String,
    /// Attributes to write (encoding attributes removed; the writer adds its own).
    pub attrs: Attrs,
    /// Type of the computed values (the written type unless `-b`).
    pub dtype: DType,
    /// Missing value written for NaN (`_FillValue` and `missing_value`).
    pub missval: f64,
    /// Output dimensions in storage order (sizes after selections).
    pub dims: Vec<VarDim>,
    pub grid: Option<usize>,
    pub zaxis: Option<usize>,
    pub expr: Expr,
}

impl VarDesc {
    pub fn shape(&self) -> Vec<usize> {
        self.dims.iter().map(|d| d.size).collect()
    }

    pub fn dim_of(&self, role: DimRole) -> Option<usize> {
        self.dims.iter().position(|d| d.role == role)
    }

    /// Indices of the horizontal dimensions.
    pub fn hdims(&self) -> Vec<usize> {
        (0..self.dims.len())
            .filter(|&i| self.dims[i].role == DimRole::Horizontal)
            .collect()
    }

    /// Applies a selection along output dimension `dim`.
    pub fn select(&mut self, dim: usize, sel: &IndexMap) {
        self.dims[dim].size = sel.len();
        self.expr.select(dim, sel);
    }
}

/// Description of a dataset flowing between operators (metadata only).
#[derive(Clone)]
pub struct Desc {
    pub attrs: Attrs,
    pub vars: Vec<VarDesc>,
    pub grids: Vec<GridDesc>,
    pub zaxes: Vec<ZDesc>,
    pub time: Option<TimeDesc>,
    /// Set by reductions: the variables are computed by folding the stage of `input`.
    pub fold: Option<Fold>,
}

/// A reduction ending a stage: the description of its input and the kernel that folds it.
/// Output variable `i` is computed from input variable `i`.
#[derive(Clone)]
pub struct Fold {
    pub input: Box<Desc>,
    pub kernel: Arc<dyn stage::FoldKernel>,
}

impl Desc {
    pub fn ntime(&self) -> usize {
        self.time.as_ref().map_or(0, TimeDesc::len)
    }
}

const ENCODING_ATTRS: &[&str] = &[
    "_FillValue",
    "missing_value",
    "scale_factor",
    "add_offset",
    "coordinates",
    "grid_mapping",
    "CDI_grid_type",
    "_ARRAY_DIMENSIONS",
    "_Netcdf4Dimid",
    "_Netcdf4Coordinates",
    "chunksizes",
];

/// Default missing value of cdo.
pub const CDO_MISSVAL: f64 = -9.0e33;

/// The opened inputs of a plan (one per distinct path), and the global options operators need
/// while describing.
#[derive(Default)]
pub struct Sources {
    pub paths: Vec<String>,
    pub srcs: Vec<Arc<dyn ChunkSource>>,
    /// `--timestat_date` as given on the command line.
    pub timestat_date: Option<crate::model::timegroup::TimestatDate>,
}

impl Sources {
    pub fn open(&mut self, path: &str) -> Result<usize> {
        if let Some(i) = self.paths.iter().position(|p| p == path) {
            return Ok(i);
        }
        let s = crate::io::open(path)?;
        self.paths.push(path.to_owned());
        self.srcs.push(s);
        Ok(self.srcs.len() - 1)
    }
}

/// Describes a stored dataset: every data variable becomes a leaf with identity maps.
pub fn describe_source(srcs: &Sources, si: usize) -> Result<Desc> {
    let src = &srcs.srcs[si];
    let ds = src.dataset();
    let grids: Vec<GridDesc> = ds
        .grids
        .iter()
        .map(|g| GridDesc {
            kind: g.kind,
            base: g.clone(),
            src: src.clone(),
            sel: if g.ysize > 0 && g.dims.len() == 2 {
                vec![IndexMap::identity(g.ysize), IndexMap::identity(g.xsize)]
            } else {
                vec![IndexMap::identity(g.size)]
            },
            xvals: None,
            fixed: None,
        })
        .collect();
    let zaxes: Vec<ZDesc> = ds
        .zaxes
        .iter()
        .map(|z| {
            let v = ds.var(&z.var);
            ZDesc {
                axis: z.clone(),
                attrs: v.map(|v| v.attrs.clone()).unwrap_or_default(),
                bounds_var: v.and_then(|v| v.attrs.get_str("bounds").map(str::to_owned)),
            }
        })
        .collect();
    let time = match &ds.time {
        Some(t) => {
            let raw = src.read_var(&t.var)?;
            let raw_bounds = match &t.bounds_var {
                Some(b) if t.bounds.is_some() => {
                    let v = src.read_var(b)?;
                    Some(v.chunks(2).map(|p| [p[0], p[1]]).collect())
                }
                _ => None,
            };
            Some(TimeDesc {
                axis: t.clone(),
                attrs: ds.var(&t.var).map(|v| v.attrs.clone()).unwrap_or_default(),
                raw,
                raw_bounds,
            })
        }
        None => None,
    };
    let mut vars = Vec::new();
    for v in ds.vars.iter().filter(|v| v.kind == VarKind::Data) {
        if !v.dtype.is_numeric() || v.dims.is_empty() {
            continue;
        }
        let attrs = Attrs(
            v.attrs
                .iter()
                .filter(|(k, _)| !ENCODING_ATTRS.contains(&k.as_str()))
                .cloned()
                .collect(),
        );
        let missval = v
            .attrs
            .get_f64("_FillValue")
            .or_else(|| v.attrs.get_f64("missing_value"))
            .filter(|x| !v.encoding.is_packed() && x.is_finite())
            .unwrap_or(CDO_MISSVAL);
        let maps = v
            .dims
            .iter()
            .enumerate()
            .map(|(i, d)| (i, IndexMap::identity(d.size)))
            .collect();
        vars.push(VarDesc {
            name: v.name.clone(),
            attrs,
            dtype: leaf_dtype(src.as_ref(), &v.name),
            missval,
            dims: v.dims.clone(),
            grid: v.grid,
            zaxis: v.zaxis,
            expr: Expr::Leaf(Leaf {
                src: si,
                var: v.name.clone(),
                maps,
            }),
        });
    }
    Ok(Desc {
        attrs: ds.attrs.clone(),
        vars,
        grids,
        zaxes,
        time,
        fold: None,
    })
}

/// Output format of the written file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutKind {
    Nc4,
    NcClassic,
    Zarr3,
    Zarr2,
}

/// A planned run: inputs, output description and stages.
pub struct Plan {
    pub sources: Vec<Arc<dyn ChunkSource>>,
    pub desc: Desc,
    pub stages: Vec<stage::Stage>,
    pub output: String,
    pub out_kind: OutKind,
}

/// Describes an operator tree recursively.
pub fn describe_tree(node: &OpNode, srcs: &mut Sources) -> Result<Desc> {
    let mut inputs = Vec::with_capacity(node.inputs.len());
    for i in &node.inputs {
        inputs.push(match i {
            Input::Path(p) => {
                let si = srcs.open(p)?;
                describe_source(srcs, si)?
            }
            Input::Op(o) => describe_tree(o, srcs)?,
        });
    }
    crate::ops::describe(node, inputs, srcs)
}

/// Output format from `-f` or the output suffix.
pub fn out_kind(cmd: &Command, path: &str) -> OutKind {
    use crate::chain::OutFormat;
    match cmd.options.format {
        Some(OutFormat::Nc) => OutKind::NcClassic,
        Some(OutFormat::Nc4 | OutFormat::Nc4c) => OutKind::Nc4,
        Some(OutFormat::Zarr) => OutKind::Zarr3,
        Some(OutFormat::Zarr2) => OutKind::Zarr2,
        None => {
            if crate::io::is_zarr(path) || path.trim_end_matches('/').ends_with(".zarr") {
                OutKind::Zarr3
            } else {
                OutKind::Nc4
            }
        }
    }
}

/// Builds the plan for a command with one output.
pub fn build(cmd: &Command) -> Result<Plan> {
    // `--plan` needs no output file
    let output = cmd
        .outputs
        .first()
        .cloned()
        .or_else(|| cmd.options.plan.then(String::new))
        .ok_or_else(|| Error::bad_arguments("no output file given"))?;
    let mut srcs = Sources {
        timestat_date: cmd.options.timestat_date.map(|t| {
            use crate::chain::TimestatDate as C;
            use crate::model::timegroup::TimestatDate as T;
            match t {
                C::First => T::First,
                C::Middle => T::Middle,
                C::Midhigh => T::MidHigh,
                C::Last => T::Last,
            }
        }),
        ..Sources::default()
    };
    let desc = describe_tree(&cmd.root, &mut srcs)?;
    for v in &desc.vars {
        if let Some(d) = v.dims.iter().find(|d| d.role == DimRole::Other) {
            return Err(Error::new(
                ErrorCode::UnsupportedDimension,
                format!(
                    "variable '{}' has dimension '{}' that is neither time, vertical nor horizontal",
                    v.name, d.name
                ),
            )
            .with("variable", v.name.clone())
            .with_hint("select other variables with -selname"));
        }
    }
    if desc.vars.is_empty() {
        return Err(Error::bad_arguments("no variables to write"));
    }
    let stages = vec![match &desc.fold {
        Some(f) => stage::Stage {
            kernel: Some(f.kernel.clone()),
            ..stage::Stage::map(&f.input, &srcs.srcs)?
        },
        None => stage::Stage::map(&desc, &srcs.srcs)?,
    }];
    Ok(Plan {
        sources: srcs.srcs,
        desc,
        stages,
        out_kind: out_kind(cmd, &output),
        output,
    })
}
