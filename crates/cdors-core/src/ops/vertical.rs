//! Vertical geometry: the layers of a variable's levels, depth ranges, `volint`/`volmean`
//! (cdors) and `intlevel` (cdo).
//!
//! **Layers.** On a model mesh (`-setgrid,<mesh spec>`, dry levels masked) level k of a variable
//! on the mesh's mid-levels is the full layer between the mesh interfaces k and k + 1 (FESOM2
//! `zbar`; depth = |interface|, metres); otherwise the layer between the bounds of the vertical
//! axis (absolute values, the axis's units). A depth range `z0/z1` (positive down) cuts the
//! layers: a layer counts with its part inside the range, so a layer cut at 700 m by `0/700`
//! counts with the metres above 700.
//!
//! **volint, volmean** (cdors operators): per timestep Σ area·dz·x over the valid values of all
//! cells and levels, and that divided by the volume of the cells and levels with valid values
//! (Σ area·dz). Areas: on a mesh place with per-level areas (FESOM2 nodes) the area of each level
//! (`nod_area[k]`), otherwise the cell areas `fldint` uses (m²). This is not `fldmean -vertmean`:
//! every wet cell counts with its volume. Two folds: the first, like `fldint`, has lanes of
//! timesteps and levels (so as many cores work as for `fldmean`) and keeps per (timestep, level)
//! the partial sums Σ w·x, Σ w and the count, one accumulator per grid row fed its cells in
//! ascending order across all chunks, rows added in ascending order; a small intermediate
//! (timestep, level, part) carries them to the second fold, which adds the levels in ascending
//! order. The result is the same for every chunk layout and thread count. A depth range is
//! pushed into the read: only the levels whose layer reaches into it are selected, as by
//! `-sellevel` ([`push_down_range`]).
//!
//! **intlevel** (cdo `Intlevel.cc`): linear interpolation between the two levels around each
//! target level, with cdo's float32 weights and missing-value rule (with one neighbour missing
//! the other one is taken if its weight is at least 0.5); targets outside the levels are missing
//! unless `extrapolate=true` (then the nearest level). On a mesh a target above a cell's top
//! interface or below its bottom interface is missing: nothing is interpolated into the ground
//! (cdo's 0.5 rule alone would carry the deepest wet value half a layer down).

use crate::chain::OpNode;
use crate::error::{Error, ErrorCode, Result};
use crate::io::Values;
use crate::model::{DimRole, GridKind, VarDim, ZAxis};
use crate::ops::fldstat::{self, LevelWeights, Weights};
use crate::plan::stage::{FoldKernel, FoldState, Tile};
use crate::plan::tiling::TileBox;
use crate::plan::{Desc, Fold, GridDesc, IndexMap, Sources, VarDesc, WetMask};
use cdors_mesh::models::DimRule;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Operators of this module: (name, description).
pub fn operators() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "volint",
            "Volume integral per timestep: sum of area times layer thickness times value over all \
             cells and levels (the unit times m3); volint,<z0>/<z1>: the layers cut at depths z0 \
             and z1; on a model mesh the area of each level and each cell's wet levels",
        ),
        (
            "volmean",
            "Volume mean per timestep: volint divided by the volume of the cells and levels with \
             values (not fldmean of vertmean); volmean,<z0>/<z1>: the layers cut at z0 and z1",
        ),
        (
            "intlevel",
            "Linear interpolation to the target levels (intlevel,<l1>,<l2>,... or \
             level=<l1>,...[,extrapolate=true]); on a model mesh missing below each cell's bottom",
        ),
    ]
}

pub fn handles(name: &str) -> bool {
    matches!(name, "volint" | "volmean" | "intlevel")
}

// ---------------------------------------------------------------------------------------------
// Layers and depth ranges
// ---------------------------------------------------------------------------------------------

/// The depth range `<z0>/<z1>` of `vertint`, `volint`, `volmean` (0 <= z0 < z1).
pub(crate) fn depth_range(op: &str, arg: &str) -> Result<(f64, f64)> {
    let bad = || {
        Error::bad_arguments(format!("{op}: '{arg}' is not a depth range <z0>/<z1>"))
            .with("operator", op)
            .with_hint(format!(
                "{op},<z0>/<z1> with 0 <= z0 < z1, depths positive down (metres on a model \
                 mesh, otherwise the units of the vertical axis), e.g. {op},0/700"
            ))
    };
    let (a, b) = arg.split_once('/').ok_or_else(bad)?;
    let z0: f64 = a.trim().parse().map_err(|_| bad())?;
    let z1: f64 = b.trim().parse().map_err(|_| bad())?;
    if !(z0.is_finite() && z1.is_finite() && 0.0 <= z0 && z0 < z1) {
        return Err(bad());
    }
    Ok((z0, z1))
}

/// The levels of a variable on a model mesh: its dry-level mask and the mesh level of each level
/// of its vertical dimension.
pub(crate) struct MeshLevels {
    pub mask: Arc<WetMask>,
    pub levels: Vec<usize>,
}

/// The mesh levels of `v` if its vertical dimension `zd` is the level dimension of a dry-level
/// mask (`-setgrid,<mesh spec>`); `None` off a mesh.
pub(crate) fn mesh_levels(v: &VarDesc, zd: usize) -> Option<MeshLevels> {
    let (mask, d, levels) = v.expr.wet_mask()?;
    (d == zd && mask.mesh.vertical.is_some()).then(|| MeshLevels {
        levels: levels.to_vec(),
        mask,
    })
}

impl MeshLevels {
    fn interfaces(&self) -> &[f64] {
        &self
            .mask
            .mesh
            .vertical
            .as_ref()
            .expect("checked")
            .interfaces
    }

    /// (top, bottom) depth of the layer of each level; `None` for levels on the interfaces.
    pub fn layers(&self) -> Option<Vec<(f64, f64)>> {
        if self.mask.rule != DimRule::Mid {
            return None;
        }
        let z = self.interfaces();
        Some(
            self.levels
                .iter()
                .map(|&k| depth_pair(z[k], z[k + 1]))
                .collect(),
        )
    }

    /// Depth of the top and the bottom interface of the wet levels of each cell of the place.
    fn columns(&self) -> Vec<(f64, f64)> {
        let z = self.interfaces();
        let wet = self
            .mask
            .mesh
            .vertical
            .as_ref()
            .and_then(|v| v.wet(&self.mask.place));
        let Some(wet) = wet else {
            return Vec::new();
        };
        (0..wet.top.len())
            .map(|c| {
                let r = wet.range(c);
                if r.is_empty() {
                    (f64::INFINITY, f64::NEG_INFINITY)
                } else {
                    depth_pair(z[r.start], z[r.end])
                }
            })
            .collect()
    }
}

fn depth_pair(a: f64, b: f64) -> (f64, f64) {
    let (a, b) = (a.abs(), b.abs());
    (a.min(b), a.max(b))
}

/// (top, bottom) depth of each of `nlev` levels from the bounds of the vertical axis; `None`
/// without bounds.
pub(crate) fn axis_layers(za: Option<&ZAxis>, nlev: usize) -> Option<Vec<(f64, f64)>> {
    let b = za?.bounds.as_ref().filter(|b| b.len() == nlev)?;
    Some(b.iter().map(|[l, u]| depth_pair(*l, *u)).collect())
}

/// Thickness of the part of each layer inside the depth range (all of it without a range).
pub(crate) fn cut_thickness(layers: &[(f64, f64)], range: Option<(f64, f64)>) -> Vec<f64> {
    layers
        .iter()
        .map(|&(t, b)| match range {
            None => b - t,
            Some((z0, z1)) => (b.min(z1) - t.max(z0)).max(0.0),
        })
        .collect()
}

/// The error when the levels of `v` have no layers: levels on mesh interfaces, or an axis
/// without bounds.
pub(crate) fn no_layers(op: &str, v: &VarDesc, zdim: &str, mesh: Option<&MeshLevels>) -> Error {
    match mesh {
        Some(m) => Error::new(
            ErrorCode::NotImplemented,
            format!(
                "{op} of '{}' on the mesh {}: its levels ('{zdim}') are the mesh's interfaces, \
                 which have no layer thickness",
                v.name, m.mask.mesh.name
            ),
        )
        .with("variable", v.name.clone())
        .with_hint(
            "use a variable on the mesh's mid-levels (FESOM2: nz1), or an unweighted statistic \
             (vertsum, vertmin, vertmax, bottomvalue)",
        ),
        None => Error::new(
            ErrorCode::NoCoordinates,
            format!(
                "{op} of '{}' needs layer bounds of its vertical axis '{zdim}', and the axis has \
                 none",
                v.name
            ),
        )
        .with("variable", v.name.clone())
        .with_hint(
            "attach the model mesh (-setgrid,fesom:<mesh dir>), or give the vertical axis bounds \
             (cdo -setzaxis,<z-axis description with bounds>)",
        ),
    }
}

/// The refusal of a variable with levels on a mesh whose levels are not the mesh's (no dry-level
/// mask): its dry cells are not known.
fn unknown_mesh_levels(op: &str, v: &VarDesc, g: &GridDesc, zdim: &str) -> Option<Error> {
    let m = g.base.mesh.as_ref()?;
    m.vertical.as_ref()?;
    Some(
        Error::new(
            ErrorCode::NotImplemented,
            format!(
                "{op} of '{}' on the mesh {}: its levels ('{zdim}') are not the mesh's layers or \
                 interfaces, so the dry cells of each level are not known",
                v.name, m.name
            ),
        )
        .with("variable", v.name.clone())
        .with_hint("select the variable with the mesh's level dimension (FESOM2: nz1 or nz)"),
    )
}

// ---------------------------------------------------------------------------------------------
// volint, volmean
// ---------------------------------------------------------------------------------------------

/// The levels a depth range touches: `input`'s variables with levels keep only the levels whose
/// layer intersects [z0, z1], so the levels outside are never read (nor their per-level areas
/// loaded), as with `-sellevel`. Variables whose layers are not known are left as they are (the
/// operator refuses them later); a range that misses every level keeps them all (all missing).
pub(crate) fn push_down_range(input: &mut Desc, range: (f64, f64)) {
    let mut made: HashMap<(usize, Vec<usize>), usize> = HashMap::new();
    for i in 0..input.vars.len() {
        let v = &input.vars[i];
        let Some(zd) = v.dim_of(DimRole::Vertical) else {
            continue;
        };
        let nlev = v.dims[zd].size;
        let layers = match mesh_levels(v, zd) {
            Some(m) => m.layers(),
            None => axis_layers(v.zaxis.map(|z| &input.zaxes[z].axis), nlev),
        };
        let Some(layers) = layers else {
            continue;
        };
        let keep: Vec<usize> = cut_thickness(&layers, Some(range))
            .iter()
            .enumerate()
            .filter(|(_, t)| **t > 0.0)
            .map(|(k, _)| k)
            .collect();
        if keep.is_empty() || keep.len() == nlev {
            continue;
        }
        let zi = v.zaxis.map(|z| {
            *made.entry((z, keep.clone())).or_insert_with(|| {
                let mut nz = input.zaxes[z].clone();
                nz.select(&IndexMap::from_list(keep.clone()));
                input.zaxes.push(nz);
                input.zaxes.len() - 1
            })
        });
        let v = &mut input.vars[i];
        v.select(zd, &IndexMap::from_list(keep));
        v.zaxis = zi;
    }
}

/// Cell areas of a variable.
enum Area {
    Const(f64),
    Each(Arc<Vec<f64>>),
    PerLevel(Arc<LevelWeights>),
}

/// What a dimension of the input variable is to the per-level fold.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Role {
    /// Neither levels nor cells (time, ...): kept.
    Outer,
    Level,
    /// The first horizontal dimension (rows of a 2-D grid, the cells of a 1-D one).
    H0,
    /// The second horizontal dimension of a 2-D grid.
    H1,
}

/// Components of the per-level partial sums: Σ w·x, Σ w, number of valid values.
const PARTS: usize = 3;

struct LevelPlan {
    roles: Vec<Role>,
    zd: usize,
    /// Cells per row of a 2-D grid (0 for 1-D grids) and rows (1 for 1-D grids).
    nx: usize,
    nrows: usize,
    dz: Vec<f64>,
    area: Area,
}

/// Stage 1 of `volint`/`volmean`: per (timestep, level) the partial sums over the cells, folded
/// like `fldint` (lanes of timesteps and levels, cells in order), written as a small
/// intermediate (outer dimensions, level, part).
struct LevelKernel {
    vars: Vec<Arc<LevelPlan>>,
    info: Option<serde_json::Value>,
}

/// Products of a lane's extents along its outer dimensions and along its levels.
fn lane_sizes(p: &LevelPlan, lane: &TileBox) -> (usize, usize) {
    let outer = (0..lane.ranges.len())
        .filter(|&d| p.roles[d] == Role::Outer)
        .map(|d| lane.ranges[d].len())
        .product();
    (outer, lane.ranges[p.zd].len())
}

impl FoldKernel for LevelKernel {
    fn fold_dim(&self) -> DimRole {
        DimRole::Horizontal
    }

    fn plan_info(&self) -> Option<serde_json::Value> {
        self.info.clone()
    }

    fn state_bytes(&self, var: usize, lane: &TileBox) -> usize {
        let p = &self.vars[var];
        let (outer, nlev) = lane_sizes(p, lane);
        outer * nlev * (p.nrows * 24 + PARTS * 8)
    }

    fn start(&self, var: usize, lane: &TileBox) -> Box<dyn FoldState> {
        let p = self.vars[var].clone();
        let (outer, nlev) = lane_sizes(&p, lane);
        let nd = lane.ranges.len();
        let mut ostride = vec![0usize; nd];
        let mut s = 1;
        for d in (0..nd).rev() {
            if p.roles[d] == Role::Outer {
                ostride[d] = s;
                s *= lane.ranges[d].len();
            }
        }
        Box::new(LevelState {
            var,
            lane: lane.clone(),
            outer,
            nlev,
            ostride,
            acc: vec![(0.0, 0.0, 0); outer * nlev * p.nrows],
            areas: None,
            p,
        })
    }
}

struct LevelState {
    var: usize,
    p: Arc<LevelPlan>,
    lane: TileBox,
    outer: usize,
    /// Levels of the lane.
    nlev: usize,
    /// Stride of each outer dimension in the lane's outer index (0 for the others).
    ostride: Vec<usize>,
    /// (Σ w·x, Σ w, valid values) per (outer, lane level, row).
    acc: Vec<(f64, f64, u64)>,
    /// Per-level areas of the lane's levels, loaded with its first tile.
    areas: Option<Vec<Arc<Vec<f64>>>>,
}

impl LevelState {
    fn ensure_areas(&mut self) -> Result<()> {
        if let (None, Area::PerLevel(lw)) = (&self.areas, &self.p.area) {
            self.areas = Some(
                self.lane.ranges[self.p.zd]
                    .clone()
                    .map(|j| lw.level(j).map(|(a, _)| a))
                    .collect::<Result<_>>()?,
            );
        }
        Ok(())
    }

    /// Folds a tile (values in C order over its box): every value goes to the accumulator of its
    /// (outer, level, row) in the order of the tile, i.e. its cells ascending.
    fn fold<T: Copy + Into<f64>>(&mut self, bx: &TileBox, v: &[T]) {
        let p = &*self.p;
        let nd = bx.ranges.len();
        if nd == 0 || bx.is_empty() {
            return;
        }
        let last = nd - 1;
        let n = bx.ranges[last].len();
        let k0 = self.lane.ranges[p.zd].start;
        // (outer, lane level, cell, row) of index i along dimension d, added up
        let contrib = |d: usize, i: usize, a: &mut [usize; 4]| match p.roles[d] {
            Role::Outer => a[0] += (i - self.lane.ranges[d].start) * self.ostride[d],
            Role::Level => a[1] = i - k0,
            Role::H0 if p.nx > 0 => {
                a[2] += i * p.nx;
                a[3] = i;
            }
            Role::H0 | Role::H1 => a[2] += i,
        };
        let mut idx: Vec<usize> = bx.ranges.iter().map(|r| r.start).collect();
        for row in v.chunks_exact(n) {
            let mut base = [0usize; 4];
            for (d, &i) in idx[..last].iter().enumerate() {
                contrib(d, i, &mut base);
            }
            for (k, &x) in row.iter().enumerate() {
                let mut a = base;
                contrib(last, bx.ranges[last].start + k, &mut a);
                let [o, j, f, r] = a;
                let area = match &p.area {
                    Area::Const(c) => *c,
                    Area::Each(w) => w[f],
                    Area::PerLevel(_) => self.areas.as_ref().expect("loaded")[j][f],
                };
                let w = area * p.dz[k0 + j];
                let x: f64 = x.into();
                if !x.is_nan() && w != 0.0 {
                    let e = &mut self.acc[(o * self.nlev + j) * p.nrows + r];
                    e.0 += w * x;
                    e.1 += w;
                    e.2 += 1;
                }
            }
            let mut d = last;
            while d > 0 {
                d -= 1;
                idx[d] += 1;
                if idx[d] < bx.ranges[d].end {
                    break;
                }
                idx[d] = bx.ranges[d].start;
            }
        }
    }
}

impl FoldState for LevelState {
    fn push(&mut self, tile: Tile) -> Result<Vec<Tile>> {
        self.ensure_areas()?;
        match &tile.values {
            Values::F32(v) => self.fold(&tile.bx, v),
            Values::F64(v) => self.fold(&tile.bx, v),
        }
        Ok(Vec::new())
    }

    /// (outer, level, part): the rows of each level added in ascending order.
    fn finish(&mut self) -> Result<Vec<Tile>> {
        let p = &*self.p;
        let mut out = Vec::with_capacity(self.outer * self.nlev * PARTS);
        for rows in self.acc.chunks_exact(p.nrows) {
            let (mut s, mut w, mut n) = (0.0, 0.0, 0u64);
            for e in rows {
                s += e.0;
                w += e.1;
                n += e.2;
            }
            out.extend([s, w, n as f64]);
        }
        let mut ranges: Vec<_> = (0..self.lane.ranges.len())
            .filter(|&d| p.roles[d] == Role::Outer)
            .map(|d| self.lane.ranges[d].clone())
            .collect();
        ranges.push(self.lane.ranges[p.zd].clone());
        ranges.push(0..PARTS);
        Ok(vec![Tile {
            var: self.var,
            bx: TileBox { ranges },
            values: Values::F64(out),
        }])
    }
}

/// Stage 2: the partial sums of the levels added in ascending level order per timestep, then
/// volint = Σ w·x, volmean = Σ w·x / Σ w (missing without valid values).
struct SumKernel {
    mean: bool,
    /// Per variable, for each output dimension the intermediate's dimension it keeps (`None`:
    /// the 1-point grid).
    out_dims: Vec<Vec<Option<usize>>>,
}

impl FoldKernel for SumKernel {
    fn fold_dim(&self) -> DimRole {
        DimRole::Vertical
    }

    fn folds(&self, role: DimRole) -> bool {
        matches!(role, DimRole::Vertical | DimRole::Other)
    }

    fn state_bytes(&self, _var: usize, lane: &TileBox) -> usize {
        lane.len() / lane.ranges.last().map_or(1, |r| r.len().max(1)) * 32
    }

    fn start(&self, var: usize, lane: &TileBox) -> Box<dyn FoldState> {
        let nd = lane.ranges.len();
        let outer: usize = lane.ranges[..nd - 2].iter().map(|r| r.len()).product();
        Box::new(SumState {
            var,
            mean: self.mean,
            lane: lane.clone(),
            out_dims: self.out_dims[var].clone(),
            acc: vec![[0.0; PARTS]; outer],
        })
    }
}

struct SumState {
    var: usize,
    mean: bool,
    lane: TileBox,
    out_dims: Vec<Option<usize>>,
    acc: Vec<[f64; PARTS]>,
}

impl FoldState for SumState {
    fn push(&mut self, tile: Tile) -> Result<Vec<Tile>> {
        let nd = tile.bx.ranges.len();
        let (nk, nc) = (tile.bx.ranges[nd - 2].len(), tile.bx.ranges[nd - 1].len());
        let c0 = tile.bx.ranges[nd - 1].start;
        let v = match &tile.values {
            Values::F64(v) => v.clone(),
            Values::F32(v) => v.iter().map(|&x| f64::from(x)).collect(),
        };
        for (o, a) in self.acc.iter_mut().enumerate() {
            for k in 0..nk {
                for c in 0..nc {
                    a[c0 + c] += v[(o * nk + k) * nc + c];
                }
            }
        }
        Ok(Vec::new())
    }

    fn finish(&mut self) -> Result<Vec<Tile>> {
        let out = self
            .acc
            .iter()
            .map(|&[s, w, n]| match (self.mean, n == 0.0) {
                (_, true) => f64::NAN,
                (true, _) => s / w,
                (false, _) => s,
            })
            .collect();
        let ranges = self
            .out_dims
            .iter()
            .map(|d| match d {
                Some(d) => self.lane.ranges[*d].clone(),
                None => 0..1,
            })
            .collect();
        Ok(vec![Tile {
            var: self.var,
            bx: TileBox { ranges },
            values: Values::F64(out),
        }])
    }
}

fn volume(node: &OpNode, mut input: Desc, srcs: &mut Sources) -> Result<Desc> {
    let op = node.name.as_str();
    let mean = op == "volmean";
    let range = node.args.first().map(|a| depth_range(op, a)).transpose()?;
    if let Some(r) = range {
        push_down_range(&mut input, r);
    }
    let mut out = input.clone();
    // stage 1 writes (outer..., level, part) per variable
    let mut mid = input.clone();
    let mut plans = Vec::with_capacity(out.vars.len());
    let mut out_dims = Vec::with_capacity(out.vars.len());
    let mut made: HashMap<usize, (GridDesc, Vec<VarDim>)> = HashMap::new();
    let (mut winfo, mut minfo) = (Vec::new(), Vec::new());
    let mut loaded = HashSet::new();
    for (v, mv) in out.vars.iter_mut().zip(&mut mid.vars) {
        let zs: Vec<usize> = (0..v.dims.len())
            .filter(|&d| v.dims[d].role == DimRole::Vertical)
            .collect();
        let (h0, nh) = fldstat::block_of(v, DimRole::Horizontal)?;
        let gi = v.grid.filter(|_| nh > 0);
        let (&[zd], Some(gi)) = (zs.as_slice(), gi) else {
            return Err(Error::new(
                ErrorCode::UnsupportedDimension,
                format!(
                    "{op} needs variables with one vertical and horizontal dimensions; '{}' has \
                     dimensions ({})",
                    v.name,
                    v.dims
                        .iter()
                        .map(|d| d.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )
            .with("variable", v.name.clone())
            .with_hint("select the variables with levels first (-selname,<names>)"));
        };
        let g = &input.grids[gi];
        let zdim = v.dims[zd].name.clone();
        let nlev = v.dims[zd].size;
        let ml = mesh_levels(v, zd);
        if ml.is_none()
            && let Some(e) = unknown_mesh_levels(op, v, g, &zdim)
        {
            return Err(e);
        }
        let za = v.zaxis.map(|z| &input.zaxes[z].axis);
        let layers = match &ml {
            Some(m) => m.layers(),
            None => axis_layers(za, nlev),
        }
        .ok_or_else(|| no_layers(op, v, &zdim, ml.as_ref()))?;
        let dz = cut_thickness(&layers, range);
        let area = if let Some((lw, bytes)) = fldstat::level_weights(g, v, false, &mut loaded) {
            winfo.push(serde_json::json!({
                "variable": v.name,
                "area_weights": "mesh_per_level",
                "description": fldstat::per_level_description(&lw),
            }));
            minfo.push(serde_json::json!({
                "variable": v.name,
                "weights": "per_level",
                "mesh": lw.mesh.name,
                "place": lw.place,
                "levels": lw.levels.len(),
                "bytes": bytes,
            }));
            Area::PerLevel(lw)
        } else {
            let (w, source) = fldstat::fld_weights(g, v, srcs, true)?;
            if let Some(src) = source {
                winfo.push(serde_json::json!({
                    "variable": v.name,
                    "area_weights": src.code(),
                    "description": src.describe(),
                }));
            }
            match w {
                Weights::Const(c) => Area::Const(c),
                Weights::Each(a) => Area::Each(a),
                Weights::PerLevel(lw) => Area::PerLevel(lw),
            }
        };
        let mut thickness = serde_json::json!({
            "source": match &ml {
                Some(m) => format!("full layers between the interfaces of the mesh {}", m.mask.mesh.name),
                None => format!("bounds of the vertical axis '{zdim}'"),
            },
            "range": range.map(|(a, b)| vec![a, b]),
            "levels": nlev,
            "thickness": dz.iter().sum::<f64>(),
        });
        if range.is_some() {
            thickness["cut"] = serde_json::json!(
                "a layer that a limit of the range cuts counts with the part inside the range"
            );
        }
        if let Some(w) = winfo.last_mut() {
            w["layers"] = thickness;
        }
        let bshape: Vec<usize> = v.dims[h0..h0 + nh].iter().map(|d| d.size).collect();
        let (nx, nrows) = if nh == 2 {
            (bshape[1], bshape[0])
        } else {
            (0, 1)
        };
        let made_dims = made
            .entry(gi)
            .or_insert_with(|| {
                let ud = (g.kind == GridKind::Unstructured).then(|| v.dims[h0].name.clone());
                fldstat::made_grid(g, vec![0.0], None, ud.as_deref())
            })
            .1
            .clone();
        let mut roles = Vec::with_capacity(v.dims.len());
        let mut mdims = Vec::new();
        let mut dims = Vec::new();
        let mut od = Vec::new();
        for (d, vd) in v.dims.iter().enumerate() {
            match vd.role {
                DimRole::Vertical => roles.push(Role::Level),
                DimRole::Horizontal => {
                    roles.push(if d == h0 { Role::H0 } else { Role::H1 });
                    if d == h0 {
                        for m in &made_dims {
                            dims.push(m.clone());
                            od.push(None);
                        }
                    }
                }
                _ => {
                    roles.push(Role::Outer);
                    od.push(Some(mdims.len()));
                    mdims.push(vd.clone());
                    dims.push(vd.clone());
                }
            }
        }
        mdims.push(v.dims[zd].clone());
        mdims.push(VarDim {
            name: format!("{}_parts", v.name),
            size: PARTS,
            role: DimRole::Other,
        });
        plans.push(Arc::new(LevelPlan {
            roles,
            zd,
            nx,
            nrows,
            dz,
            area,
        }));
        out_dims.push(od);
        mv.dims = mdims;
        mv.grid = None;
        mv.zaxis = None;
        mv.dtype = crate::model::DType::F64;
        v.dims = dims;
        v.zaxis = None;
        fldstat::add_cell_method(
            v,
            &format!("area: {zdim}: {}", if mean { "mean" } else { "sum" }),
        );
        if !mean {
            fldstat::integral_output(v, "m3");
        }
    }
    for (gi, (gd, _)) in made {
        out.grids[gi] = gd;
    }
    let mut info = serde_json::json!({ "area_weights": winfo });
    if !minfo.is_empty() {
        info["mesh_weights"] = serde_json::json!(minfo);
    }
    // the weights are the inner (level) fold's: the method note of `out` names them
    out.method.extend(super::fold_note(node, Some(info.clone())));
    mid.fold = Some(Fold {
        input: Box::new(input),
        kernel: Arc::new(LevelKernel {
            vars: plans,
            info: Some(info),
        }),
    });
    let mid = crate::plan::intermediate::materialize(mid, op, srcs)?;
    out.fold = Some(Fold {
        input: Box::new(mid),
        kernel: Arc::new(SumKernel { mean, out_dims }),
    });
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// intlevel
// ---------------------------------------------------------------------------------------------

/// `vert_gen_weights`' codes for targets beyond the first and the last level.
const BOTTOM: i32 = 32000;
const TOP: i32 = 32001;

fn level_dir(lev: &[f64], up: bool) -> bool {
    let n = lev.len();
    let ok = n > 1 && if up { lev[1] > lev[0] } else { lev[1] < lev[0] };
    ok && (1..n.saturating_sub(1)).all(|k| {
        if up {
            lev[k + 1] > lev[k]
        } else {
            lev[k + 1] < lev[k]
        }
    })
}

/// cdo's `vert_gen_weights` for the source levels `lev` (with the sentinels ±1e33 at both ends)
/// and the targets: per target the index code (`BOTTOM`, `TOP`, or ±the index of the level with
/// the lower value) and its float weight. `None` for non-monotonic levels.
fn gen_weights(lev: &[f64], targets: &[f64], extrapolate: bool) -> Option<Vec<(i32, f32)>> {
    let up = level_dir(lev, true);
    if !up && !level_dir(lev, false) {
        return None;
    }
    let (s0, s1) = if up { (-1e33, 1e33) } else { (1e33, -1e33) };
    let l1: Vec<f64> = std::iter::once(s0)
        .chain(lev.iter().copied())
        .chain(std::iter::once(s1))
        .collect();
    let nl = l1.len();
    targets
        .iter()
        .map(|&t| {
            let (i1, idx1, idx2) = (1..nl).find_map(|i1| {
                let (a, b) = if l1[i1 - 1] < l1[i1] {
                    (i1 - 1, i1)
                } else {
                    (i1, i1 - 1)
                };
                (t > l1[a] && t <= l1[b]).then_some((i1, a, b))
            })?;
            let edge = f32::from(u8::from(extrapolate || t == l1[idx2]));
            Some(if i1 == 1 {
                (BOTTOM, edge)
            } else if i1 == nl - 1 {
                (TOP, edge)
            } else {
                let idx = idx1 as i32 - 1;
                let w = ((l1[idx2] - t) / (l1[idx2] - l1[idx1])) as f32;
                (if idx1 > idx2 { -idx } else { idx }, w)
            })
        })
        .collect()
}

/// `restore_index_and_weights`: the two source levels and their weights.
fn restore(nlev: usize, idx: i32, w: f32) -> (usize, usize, f32, f32) {
    match idx {
        BOTTOM => (0, 0, 0.0, w),
        TOP => (nlev - 1, nlev - 1, w, 0.0),
        _ => {
            let i1 = idx.unsigned_abs() as usize;
            let i2 = if idx < 0 { i1 - 1 } else { i1 + 1 };
            (i1, i2, w, 1.0 - w)
        }
    }
}

/// `vert_interp_lev_kernel`.
#[inline]
fn interp(w1: f32, w2: f32, a: f64, b: f64) -> f64 {
    let w1 = if a.is_nan() { 0.0 } else { w1 };
    let w2 = if b.is_nan() { 0.0 } else { w2 };
    if w1 == 0.0 && w2 == 0.0 {
        f64::NAN
    } else if w1 == 0.0 {
        if w2 >= 0.5 { b } else { f64::NAN }
    } else if w2 == 0.0 {
        if w1 >= 0.5 { a } else { f64::NAN }
    } else {
        a * f64::from(w1) + b * f64::from(w2)
    }
}

/// The column of each cell on a mesh: target depths outside it are missing.
struct Columns {
    /// Output dimension of the cells, map from its index to the stored cell.
    dim: usize,
    cells: IndexMap,
    /// (top, bottom) depth of each stored cell.
    depth: Arc<Vec<(f64, f64)>>,
}

struct IntPlan {
    zd: usize,
    nlev: usize,
    /// Per target: source levels and weights, and its depth (|level|).
    targets: Vec<(usize, usize, f32, f32, f64)>,
    columns: Option<Columns>,
}

struct IntKernel {
    /// `None`: the variable passes unchanged (no levels to interpolate).
    vars: Vec<Option<Arc<IntPlan>>>,
}

impl FoldKernel for IntKernel {
    fn fold_dim(&self) -> DimRole {
        DimRole::Vertical
    }

    fn state_bytes(&self, var: usize, lane: &TileBox) -> usize {
        match &self.vars[var] {
            // the lane's values and the interpolated ones
            Some(p) => lane.len() * 8 + lane.len() / p.nlev.max(1) * p.targets.len() * 8,
            None => lane.len() * 8,
        }
    }

    fn start(&self, var: usize, lane: &TileBox) -> Box<dyn FoldState> {
        let buf = match &self.vars[var] {
            Some(_) => vec![f64::NAN; lane.len()],
            None => Vec::new(),
        };
        Box::new(IntState {
            var,
            p: self.vars[var].clone(),
            lane: lane.clone(),
            buf,
        })
    }
}

struct IntState {
    var: usize,
    p: Option<Arc<IntPlan>>,
    lane: TileBox,
    /// The lane's values (C order over the lane box).
    buf: Vec<f64>,
}

impl IntState {
    fn copy<T: Copy + Into<f64>>(&mut self, p: &IntPlan, bx: &TileBox, v: &[T]) {
        let shape = self.lane.shape();
        let outer: usize = shape[..p.zd].iter().product();
        let inner: usize = shape[p.zd + 1..].iter().product();
        let kr = &bx.ranges[p.zd];
        let tn = kr.len();
        for o in 0..outer {
            for (t, k) in kr.clone().enumerate() {
                let src = &v[(o * tn + t) * inner..][..inner];
                let dst = &mut self.buf[(o * p.nlev + k) * inner..][..inner];
                for (d, &s) in dst.iter_mut().zip(src) {
                    *d = s.into();
                }
            }
        }
    }
}

impl FoldState for IntState {
    fn push(&mut self, tile: Tile) -> Result<Vec<Tile>> {
        let Some(p) = self.p.clone() else {
            return Ok(vec![Tile {
                var: self.var,
                ..tile
            }]);
        };
        match &tile.values {
            Values::F32(v) => self.copy(&p, &tile.bx, v),
            Values::F64(v) => self.copy(&p, &tile.bx, v),
        }
        Ok(Vec::new())
    }

    fn finish(&mut self) -> Result<Vec<Tile>> {
        let Some(p) = self.p.clone() else {
            return Ok(Vec::new());
        };
        let shape = self.lane.shape();
        let outer: usize = shape[..p.zd].iter().product();
        let inner: usize = shape[p.zd + 1..].iter().product();
        let nt = p.targets.len();
        // stored cell of each outer (or inner) index when the cells lie before (after) the levels
        let cell_of: Option<(bool, Vec<usize>)> = p.columns.as_ref().map(|c| {
            let before = c.dim < p.zd;
            let (lo, hi) = if before {
                (0, p.zd)
            } else {
                (p.zd + 1, shape.len())
            };
            let n: usize = shape[lo..hi].iter().product();
            let stride: usize = shape[c.dim + 1..hi].iter().product();
            let start = self.lane.ranges[c.dim].start;
            let map = (0..n)
                .map(|x| c.cells.get(start + (x / stride) % shape[c.dim]))
                .collect();
            (before, map)
        });
        let mut out = vec![f64::NAN; outer * nt * inner];
        for o in 0..outer {
            for (j, &(i1, i2, w1, w2, depth)) in p.targets.iter().enumerate() {
                let a = &self.buf[(o * p.nlev + i1) * inner..][..inner];
                let b = &self.buf[(o * p.nlev + i2) * inner..][..inner];
                let y = &mut out[(o * nt + j) * inner..][..inner];
                for i in 0..inner {
                    let wet = match (&cell_of, &p.columns) {
                        (Some((before, map)), Some(c)) => {
                            let (top, bottom) = c.depth[map[if *before { o } else { i }]];
                            top <= depth && depth <= bottom
                        }
                        _ => true,
                    };
                    y[i] = if wet {
                        interp(w1, w2, a[i], b[i])
                    } else {
                        f64::NAN
                    };
                }
            }
        }
        let mut ranges = self.lane.ranges.clone();
        ranges[p.zd] = 0..nt;
        Ok(vec![Tile {
            var: self.var,
            bx: TileBox { ranges },
            values: Values::F64(out),
        }])
    }
}

/// Target levels and `extrapolate` of `intlevel,<l1>,...` / `intlevel,level=<l1>,...,extrapolate=<bool>`.
fn intlevel_args(args: &[String]) -> Result<(Vec<f64>, bool)> {
    let usage = "intlevel,<l1>,<l2>,... or intlevel,level=<l1>,<l2>,...[,extrapolate=true] (the \
                 target levels in the units of the vertical axis)";
    let num = |s: &str| -> Result<f64> {
        s.trim().parse::<f64>().map_err(|_| {
            Error::bad_arguments(format!("intlevel: '{s}' is not a level"))
                .with("operator", "intlevel")
                .with_hint(usage)
        })
    };
    let mut levels = Vec::new();
    let mut extrapolate = false;
    if args
        .first()
        .is_some_and(|a| a.starts_with(|c: char| c.is_ascii_digit()))
    {
        for a in args {
            levels.push(num(a)?);
        }
    } else {
        let mut key = String::new();
        for a in args {
            let val = match a.split_once('=') {
                Some((k, v)) => {
                    key = k.to_owned();
                    v
                }
                None => a.as_str(),
            };
            match key.as_str() {
                "level" => levels.push(num(val)?),
                "extrapolate" => {
                    extrapolate = match val.to_ascii_lowercase().as_str() {
                        "true" | "t" | "1" => true,
                        "false" | "f" | "0" => false,
                        _ => {
                            return Err(Error::bad_arguments(format!(
                                "intlevel: extrapolate={val} is not true or false"
                            ))
                            .with_hint(usage));
                        }
                    }
                }
                "zdescription" | "zvarname" => {
                    return Err(Error::new(
                        ErrorCode::NotImplemented,
                        format!("intlevel: {key}= is not implemented in cdors"),
                    )
                    .with("operator", "intlevel")
                    .with_hint(format!("give the target levels: {usage}")));
                }
                _ => {
                    return Err(
                        Error::bad_arguments(format!("intlevel: invalid parameter '{a}'"))
                            .with("operator", "intlevel")
                            .with_hint(usage),
                    );
                }
            }
        }
    }
    if levels.is_empty() {
        return Err(Error::bad_arguments("intlevel: no target levels")
            .with("operator", "intlevel")
            .with_hint(usage));
    }
    Ok((levels, extrapolate))
}

fn intlevel(node: &OpNode, input: Desc) -> Result<Desc> {
    let (targets, extrapolate) = intlevel_args(&node.args)?;
    let mut out = input.clone();
    let mut plans = Vec::with_capacity(out.vars.len());
    // output z-axis made for each input z-axis
    let mut zmap: HashMap<usize, usize> = HashMap::new();
    let mut columns: HashMap<String, Arc<Vec<(f64, f64)>>> = HashMap::new();
    for v in &mut out.vars {
        let zd = v.dim_of(DimRole::Vertical);
        let (Some(zd), Some(zi)) = (zd, v.zaxis) else {
            plans.push(None);
            continue;
        };
        let nlev = v.dims[zd].size;
        let lev = &input.zaxes[zi].axis.values;
        if nlev < 2 || lev.len() != nlev {
            plans.push(None);
            continue;
        }
        let zdim = v.dims[zd].name.clone();
        let ml = mesh_levels(v, zd);
        if ml.is_none()
            && let Some(g) = v.grid.map(|g| &input.grids[g])
            && let Some(e) = unknown_mesh_levels("intlevel", v, g, &zdim)
        {
            return Err(e);
        }
        let w = gen_weights(lev, &targets, extrapolate).ok_or_else(|| {
            Error::new(
                ErrorCode::BadData,
                format!(
                    "intlevel: the levels of '{zdim}' of '{}' are not monotonic",
                    v.name
                ),
            )
        })?;
        let cols = match &ml {
            Some(m) => {
                let (dim, cells) = m.mask.cell.clone();
                let depth = columns
                    .entry(format!("{}/{}", m.mask.mesh.name, m.mask.place))
                    .or_insert_with(|| Arc::new(m.columns()))
                    .clone();
                Some(Columns { dim, cells, depth })
            }
            None => None,
        };
        plans.push(Some(Arc::new(IntPlan {
            zd,
            nlev,
            targets: w
                .iter()
                .zip(&targets)
                .map(|(&(idx, wt), &t)| {
                    let (i1, i2, w1, w2) = restore(nlev, idx, wt);
                    (i1, i2, w1, w2, t.abs())
                })
                .collect(),
            columns: cols,
        })));
        let nz = *zmap.entry(zi).or_insert_with(|| {
            let mut z = input.zaxes[zi].clone();
            z.axis.values = targets.clone();
            z.axis.bounds = None;
            z.bounds_var = None;
            z.attrs.0.retain(|(k, _)| k != "bounds");
            out.zaxes.push(z);
            out.zaxes.len() - 1
        });
        v.zaxis = Some(nz);
        v.dims[zd].size = targets.len();
        // the levels are no longer the mesh's: each target depth is in the layer that contains
        // it, whose areas and dry cells the result's statistics use (intlevel has set the cells
        // without water at that depth missing already)
        v.expr.unmask();
        if let Some(m) = &ml {
            let layers = containing_layers(m.interfaces(), &targets);
            let mask = WetMask {
                mesh: m.mask.mesh.clone(),
                place: m.mask.place.clone(),
                rule: DimRule::Mid,
                level: (zd, IndexMap::from_list(layers)),
                cell: m.mask.cell.clone(),
            };
            let e = std::mem::replace(
                &mut v.expr,
                crate::plan::Expr::Leaf(crate::plan::Leaf {
                    src: 0,
                    var: String::new(),
                    maps: Vec::new(),
                }),
            );
            v.expr = crate::plan::Expr::Mask(Arc::new(mask), Box::new(e));
        }
    }
    out.fold = Some(Fold {
        input: Box::new(input),
        kernel: Arc::new(IntKernel { vars: plans }),
    });
    Ok(out)
}

/// The layer that contains each depth: `k` with `|z[k]| <= d < |z[k + 1]|` for the interfaces
/// `z` (a depth on an interface is in the layer below it; above the surface the first layer,
/// below the deepest interface the last one).
fn containing_layers(interfaces: &[f64], depths: &[f64]) -> Vec<usize> {
    let nlev = interfaces.len().saturating_sub(1).max(1);
    depths
        .iter()
        .map(|&d| {
            let d = d.abs();
            interfaces
                .iter()
                .skip(1)
                .position(|z| d < z.abs())
                .unwrap_or(nlev - 1)
        })
        .collect()
}

/// Output description of `volint`, `volmean`, `intlevel`.
pub fn describe(node: &OpNode, mut inputs: Vec<Desc>, srcs: &mut Sources) -> Result<Desc> {
    let input = inputs.remove(0);
    match node.name.as_str() {
        "intlevel" => intlevel(node, input),
        _ => volume(node, input, srcs),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_of_depth() {
        // interfaces 0, 10, 30, 60: 100 m is below the bottom (last layer)
        let z = [0.0, -10.0, -30.0, -60.0];
        assert_eq!(
            containing_layers(&z, &[0.0, 5.0, 10.0, 59.9, 100.0]),
            vec![0, 0, 1, 2, 2]
        );
    }

    #[test]
    fn cut_layers() {
        // layers 0-10, 10-30, 30-60 cut at 5/40: 5, 20, 10
        let l = [(0.0, 10.0), (10.0, 30.0), (30.0, 60.0)];
        assert_eq!(cut_thickness(&l, Some((5.0, 40.0))), vec![5.0, 20.0, 10.0]);
        assert_eq!(cut_thickness(&l, Some((60.0, 70.0))), vec![0.0, 0.0, 0.0]);
        assert_eq!(cut_thickness(&l, None), vec![10.0, 20.0, 30.0]);
    }

    #[test]
    fn intlevel_weights() {
        // depths ascending: 15 between 10 and 20 (weight 0.5 each); 5 above the first level
        // (missing), 10 on it (its value), 40 below the last (missing, nearest with extrapolate)
        let lev = [10.0, 20.0, 30.0];
        let w = gen_weights(&lev, &[15.0, 5.0, 10.0, 40.0], false).unwrap();
        let r: Vec<_> = w.iter().map(|&(i, x)| restore(3, i, x)).collect();
        assert_eq!(r[0], (0, 1, 0.5, 0.5));
        assert!(interp(r[1].2, r[1].3, 1.0, 1.0).is_nan());
        assert_eq!(interp(r[2].2, r[2].3, 7.0, 7.0), 7.0);
        assert!(interp(r[3].2, r[3].3, 3.0, 3.0).is_nan());
        let w = gen_weights(&lev, &[40.0], true).unwrap();
        let (a, b, w1, w2) = restore(3, w[0].0, w[0].1);
        assert_eq!((a, b), (2, 2));
        assert_eq!(interp(w1, w2, 3.0, 3.0), 3.0);
        // pressure descending: 92500 between 100000 and 85000
        let w = gen_weights(&[100000.0, 85000.0], &[92500.0], false).unwrap();
        assert_eq!(restore(2, w[0].0, w[0].1), (1, 0, 0.5, 0.5));
        assert!(gen_weights(&[1.0, 3.0, 2.0], &[1.5], false).is_none());
    }
}
