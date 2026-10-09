//! Remapping operators: `remap,<grid>,<weights.nc>`, `remapnn`, `remapdis`, `remapbil`,
//! `remapcon` and `remapycon` with a target-grid argument.
//!
//! cdors never builds grids or weights itself (`crate::remap`):
//!
//! - **target grid**: cdo writes a small template on the target grid once
//!   (`cdo -f nc4 const,0,<grid>`, cached as `$CDORS_CACHE/weights/grid-<hash>.nc`) and the output
//!   grid is read from it, so coordinates, bounds and the HEALPix mapping are exactly what cdo
//!   writes. A Zarr store as target is written as such a template by cdors first.
//! - **source grid**: a local NetCDF file whose grid is not subset is handed to `cdo gen<method>`
//!   directly; otherwise (Zarr, kerchunk, remote, selections) cdors writes the grid as a small
//!   NetCDF file (`src-<hash>.nc` in the cache) and hands that over. The weight cache key is a hash
//!   of the source grid's coordinates and bounds (or HEALPix parameters), not of the file path, so
//!   the NetCDF and Zarr copies of one dataset share weights.
//! - weights are generated (or read) lazily, on the first field that needs them, so `--plan` never
//!   runs cdo for weights.
//!
//! **Reads only what the weights touch.** When the weights are at hand while planning (a weight
//! file, cached weights, or — in a run, not under `--plan` — weights generated right away), the
//! source cells that no link reads are dropped like a selection (rows and columns of 2-D grids),
//! so only the chunks holding linked cells are read; the weights are renumbered to the kept
//! cells, which leaves every result bit-identical.
//!
//! Access class: whole extent over space. The stage's tiles are folded along the horizontal
//! dimensions (a lane is one block of timesteps and levels); once a lane holds complete fields
//! they are remapped together (`RemapWeights::apply_batch`), lanes in parallel. Values are summed
//! in double precision and rounded once to the variable's type, as cdo does; missing values follow
//! the per-method rules of `crate::remap::weights`.

use crate::chain::OpNode;
use crate::error::{Error, ErrorCode, Result};
use crate::exec::{OutVar, Writer};
use crate::io::{ChunkSource, Values};
use crate::model::{DType, DimRole, GridKind, HealpixOrder, VarDim};
use crate::plan::stage::{FoldKernel, FoldState, Tile};
use crate::plan::tiling::TileBox;
use crate::plan::{
    CDO_MISSVAL, Desc, Expr, Fold, GridDesc, IndexMap, Leaf, OutKind, Plan, VarDesc,
};
use crate::remap::generate::{Fnv128, TEMPLATE_HINT};
use crate::remap::{
    GenMethod, RemapError, RemapWeights, SourceIdentity, WeightCache, WeightRequest,
};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// Operators handled here.
pub fn handles(name: &str) -> bool {
    matches!(
        name,
        "remap" | "remapnn" | "remapdis" | "remapbil" | "remapcon" | "remapycon"
    )
}

/// cdors error for a failure of the remapping core.
fn rerr(op: &str, e: RemapError) -> Error {
    let code = match &e {
        RemapError::Netcdf { .. } | RemapError::Format { .. } => ErrorCode::BadData,
        RemapError::Io { .. } => ErrorCode::IoError,
        RemapError::CdoFailed { hint, .. } if *hint == TEMPLATE_HINT => ErrorCode::BadArguments,
        _ => ErrorCode::UnsupportedGrid,
    };
    let (msg, hint) = match &e {
        RemapError::CdoMissing { reason, hint } => {
            (format!("cdo not available ({reason})"), Some(*hint))
        }
        RemapError::CdoFailed {
            command,
            stderr,
            hint,
        } => (format!("`{command}` failed: {stderr}"), Some(*hint)),
        _ => (e.to_string(), None),
    };
    let mut err = Error::new(code, msg).with("operator", op.to_owned());
    if let Some(h) = hint {
        err = err.with_hint(h);
    }
    err
}

// ------------------------------------------------------------------ field kernel (shared)

/// Maps complete horizontal fields of one variable to fields on another grid.
pub(crate) trait FieldMap: Send + Sync {
    /// `src` holds `n` fields of the input grid back to back; returns `n` output fields as `out`.
    fn apply(&self, var: usize, src: &Values, n: usize, out: DType) -> Result<Values>;
    /// Facts for `--plan` (weights to generate), see [`FoldKernel::plan_info`].
    fn plan_info(&self) -> Option<serde_json::Value> {
        None
    }
}

/// Shape bookkeeping of one variable folded by a [`FieldKernel`].
#[derive(Debug, Clone)]
pub(crate) struct FieldVar {
    /// Index of the first horizontal dimension (horizontal dimensions are the trailing ones).
    pub hfirst: usize,
    pub src_size: usize,
    /// Sizes of the output horizontal dimensions.
    pub out_hdims: Vec<usize>,
    pub out_dtype: DType,
}

/// A fold kernel over the horizontal dimensions: collects complete fields and maps them.
pub(crate) struct FieldKernel {
    pub map: Arc<dyn FieldMap>,
    pub vars: Vec<FieldVar>,
}

impl FoldKernel for FieldKernel {
    fn fold_dim(&self) -> DimRole {
        DimRole::Horizontal
    }

    fn start(&self, var: usize, lane: &TileBox) -> Box<dyn FoldState> {
        Box::new(FieldState {
            map: self.map.clone(),
            fv: self.vars[var].clone(),
            var,
            lane: lane.clone(),
            whole: None,
            buf: None,
        })
    }

    fn plan_info(&self) -> Option<serde_json::Value> {
        self.map.plan_info()
    }
}

struct FieldState {
    map: Arc<dyn FieldMap>,
    fv: FieldVar,
    var: usize,
    lane: TileBox,
    /// The lane arrived as one tile.
    whole: Option<Values>,
    /// The lane assembled from several tiles.
    buf: Option<Values>,
}

/// Copies a tile into the lane buffer (both boxes in the same output index space).
fn copy_into(src: &Values, sb: &TileBox, dst: &mut Values, db: &TileBox) {
    let nd = sb.ranges.len();
    let sshape = sb.shape();
    let dshape = db.shape();
    let row = sshape[nd - 1];
    let total: usize = sshape.iter().product();
    if total == 0 {
        return;
    }
    let mut idx = vec![0usize; nd];
    let mut so = 0;
    loop {
        let mut off = 0;
        for d in 0..nd {
            off = off * dshape[d] + (sb.ranges[d].start - db.ranges[d].start + idx[d]);
        }
        match (src, &mut *dst) {
            (Values::F32(s), Values::F32(t)) => t[off..off + row].copy_from_slice(&s[so..so + row]),
            (Values::F64(s), Values::F64(t)) => t[off..off + row].copy_from_slice(&s[so..so + row]),
            (Values::F32(s), Values::F64(t)) => {
                for (a, b) in t[off..off + row].iter_mut().zip(&s[so..so + row]) {
                    *a = f64::from(*b);
                }
            }
            (Values::F64(s), Values::F32(t)) => {
                for (a, b) in t[off..off + row].iter_mut().zip(&s[so..so + row]) {
                    *a = *b as f32;
                }
            }
        }
        so += row;
        if nd == 1 {
            return;
        }
        let mut d = nd - 1;
        loop {
            if d == 0 {
                return;
            }
            d -= 1;
            idx[d] += 1;
            if idx[d] < sshape[d] {
                break;
            }
            idx[d] = 0;
        }
    }
}

impl FoldState for FieldState {
    fn push(&mut self, tile: Tile) -> Result<Vec<Tile>> {
        if self.buf.is_none() && self.whole.is_none() && tile.bx == self.lane {
            self.whole = Some(tile.values);
            return Ok(Vec::new());
        }
        let n = self.lane.len();
        let buf = self.buf.get_or_insert_with(|| match &tile.values {
            Values::F32(_) => Values::F32(vec![f32::NAN; n]),
            Values::F64(_) => Values::F64(vec![f64::NAN; n]),
        });
        copy_into(&tile.values, &tile.bx, buf, &self.lane);
        Ok(Vec::new())
    }

    fn finish(&mut self) -> Result<Vec<Tile>> {
        let Some(values) = self.whole.take().or_else(|| self.buf.take()) else {
            return Ok(Vec::new());
        };
        let nfields = values.len() / self.fv.src_size.max(1);
        let out = self
            .map
            .apply(self.var, &values, nfields, self.fv.out_dtype)?;
        let mut ranges: Vec<_> = self.lane.ranges[..self.fv.hfirst].to_vec();
        ranges.extend(self.fv.out_hdims.iter().map(|&n| 0..n));
        Ok(vec![Tile {
            var: self.var,
            bx: TileBox { ranges },
            values: out,
        }])
    }
}

/// Index of the first horizontal dimension of `v`; horizontal dimensions must be trailing.
pub(crate) fn trailing_hdims(op: &str, v: &VarDesc) -> Result<usize> {
    let h = v.hdims();
    let n = v.dims.len();
    if h.is_empty() || h != (n - h.len()..n).collect::<Vec<_>>() {
        return Err(Error::new(
            ErrorCode::UnsupportedDimension,
            format!(
                "variable '{}': {op} needs the horizontal dimensions last (dims: {})",
                v.name,
                v.dims
                    .iter()
                    .map(|d| d.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
        .with("operator", op.to_owned())
        .with("variable", v.name.clone()));
    }
    Ok(n - h.len())
}

/// Rejects an input that is itself the pending output of a reduction (one fold per chain).
pub(crate) fn no_pending_fold(op: &str, d: &Desc) -> Result<()> {
    if d.fold.is_some() {
        return Err(Error::new(
            ErrorCode::NotImplemented,
            format!("'{op}' on the result of another reduction in the same chain"),
        )
        .with("operator", op.to_owned())
        .with_hint("write the intermediate result to a file and run the second step on it"));
    }
    Ok(())
}

// ------------------------------------------------------------------ grids and weights

/// A grid description of a dataset's first grid, read through our reader.
fn grid_of(src: &Arc<dyn ChunkSource>, gi: usize) -> GridDesc {
    let g = src.dataset().grids[gi].clone();
    GridDesc {
        kind: g.kind,
        sel: if g.ysize > 0 && g.dims.len() == 2 {
            vec![IndexMap::identity(g.ysize), IndexMap::identity(g.xsize)]
        } else {
            vec![IndexMap::identity(g.size)]
        },
        base: g,
        src: src.clone(),
        xvals: None,
        fixed: None,
    }
}

fn is_subset(g: &GridDesc) -> bool {
    let shape: Vec<usize> = if g.sel.len() == 2 {
        vec![g.base.ysize, g.base.xsize]
    } else {
        vec![g.base.size]
    };
    g.xvals.is_some() || g.sel.iter().zip(shape).any(|(m, n)| !m.is_identity(n))
}

/// Bytes that identify a grid as written: kind, sizes, and coordinates with bounds (or the
/// HEALPix parameters).
fn grid_identity(g: &GridDesc) -> Result<Vec<u8>> {
    let mut b = Vec::new();
    b.extend_from_slice(b"cdors-grid-id-v1\0");
    b.extend_from_slice(g.kind.name().as_bytes());
    for m in &g.sel {
        b.extend_from_slice(&(m.len() as u64).to_le_bytes());
    }
    if g.is_healpix() {
        let hp = g.base.healpix.as_ref().expect("healpix grid");
        b.extend_from_slice(b"healpix\0");
        b.extend_from_slice(&hp.nside.to_le_bytes());
        b.push(u8::from(hp.order == HealpixOrder::Ring));
        return Ok(b);
    }
    let c = g.coords()?.ok_or_else(|| {
        Error::new(
            ErrorCode::NoCoordinates,
            format!("the {} grid has no coordinates", g.kind.name()),
        )
        .with_hint("attach coordinates with -setgrid,<grid file>")
    })?;
    let mut put = |tag: &[u8], v: &[f64]| {
        b.extend_from_slice(tag);
        b.extend_from_slice(&(v.len() as u64).to_le_bytes());
        for x in v {
            b.extend_from_slice(&x.to_bits().to_le_bytes());
        }
    };
    put(b"x\0", &c.xvals);
    put(b"y\0", &c.yvals);
    put(b"xb\0", c.xbounds.as_deref().unwrap_or(&[]));
    put(b"yb\0", c.ybounds.as_deref().unwrap_or(&[]));
    b.extend_from_slice(c.xunits.as_bytes());
    b.push(0);
    b.extend_from_slice(c.yunits.as_bytes());
    Ok(b)
}

fn hash_hex(bytes: &[u8]) -> String {
    let mut h = Fnv128::new();
    h.update(bytes);
    format!("{:032x}", h.finish())
}

/// Whether `path` is a local NetCDF (classic or HDF5) file, which cdo reads.
fn is_local_netcdf(path: &str) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 4];
    f.read_exact(&mut magic).is_ok() && (&magic[..3] == b"CDF" || magic == *b"\x89HDF")
}

/// Writes `g` as a small NetCDF file with one float variable of zeros on it (dimensions named
/// `hdims`), as cdors writes grids, so that cdo can read the grid. Cached as `<key>.nc`.
fn write_grid_file(
    cache: &WeightCache,
    g: &GridDesc,
    hdims: &[VarDim],
    key: &str,
) -> Result<PathBuf> {
    let path = cache.dir().join(format!("{key}.nc"));
    if path.is_file() {
        return Ok(path);
    }
    let tmp = cache.tmp_path(key).map_err(|e| rerr("remap", e))?;
    let shape: Vec<usize> = hdims.iter().map(|d| d.size).collect();
    let dims: Vec<VarDim> = hdims.to_vec();
    let var = VarDesc {
        name: "cdors_grid".into(),
        attrs: Default::default(),
        dtype: DType::F32,
        missval: CDO_MISSVAL,
        dims: dims.clone(),
        grid: Some(0),
        zaxis: None,
        expr: Expr::Leaf(Leaf {
            src: 0,
            var: String::new(),
            maps: Vec::new(),
        }),
    };
    let desc = Desc {
        attrs: Default::default(),
        vars: vec![var],
        grids: vec![g.clone()],
        zaxes: Vec::new(),
        time: None,
        fold: None,
    };
    let plan = Plan::new(
        vec![g.src.clone()],
        desc,
        tmp.to_string_lossy().into_owned(),
        OutKind::Nc4,
    );
    let lay = vec![OutVar {
        name: "cdors_grid".into(),
        dims: dims.iter().map(|d| d.name.clone()).collect(),
        shape: shape.clone(),
        chunks: shape.clone(),
        dtype: DType::F32,
        missval: CDO_MISSVAL,
    }];
    let w = crate::io::write_netcdf::NcWriter::create(&tmp, &plan, &lay, false, None)?;
    let n: usize = shape.iter().product();
    w.write(0, &vec![0; shape.len()], &shape, Values::F32(vec![0.0; n]))?;
    w.finish()?;
    cache.publish(&tmp, &path).map_err(|e| rerr("remap", e))?;
    Ok(path)
}

/// How the weights of one input grid are obtained.
enum WeightSpec {
    /// Read from the user's file at describe time.
    Ready,
    /// Generated by cdo on first use.
    Gen {
        method: GenMethod,
        /// Target grid argument handed to cdo.
        target: String,
        grid: Box<GridDesc>,
        hdims: Vec<VarDim>,
    },
}

struct GridWeights {
    spec: WeightSpec,
    dst_size: usize,
    cell: OnceLock<Result<Arc<RemapWeights>>>,
}

struct RemapMap {
    op: String,
    /// Weights per input grid.
    grids: Vec<GridWeights>,
    /// Input grid (index into `grids`) of each variable.
    var_grid: Vec<usize>,
}

impl GridWeights {
    /// Whether the weights can be had without running cdo (given, or in the cache).
    fn at_hand(&self, op: &str) -> bool {
        let WeightSpec::Gen {
            method,
            target,
            grid,
            ..
        } = &self.spec
        else {
            return true;
        };
        (|| -> Result<bool> {
            let cache = WeightCache::from_env().map_err(|e| rerr(op, e))?;
            let identity = grid_identity(grid)?;
            let probe = WeightRequest {
                method: *method,
                target,
                source: Path::new(""),
                variable: None,
                identity: SourceIdentity::Bytes(&identity),
            };
            let key = cache.key(&probe).map_err(|e| rerr(op, e))?;
            Ok(cache.dir().join(format!("{key}.nc")).is_file())
        })()
        .unwrap_or(false)
    }

    /// `--plan`: where the weights come from and whether cdo has to generate them.
    fn plan_info(&self, op: &str) -> serde_json::Value {
        let WeightSpec::Gen {
            method,
            target,
            grid,
            ..
        } = &self.spec
        else {
            return serde_json::json!({"operator": op, "weights": "given", "generate": false});
        };
        let cached = (|| -> Result<PathBuf> {
            let cache = WeightCache::from_env().map_err(|e| rerr(op, e))?;
            let identity = grid_identity(grid)?;
            let probe = WeightRequest {
                method: *method,
                target,
                source: Path::new(""),
                variable: None,
                identity: SourceIdentity::Bytes(&identity),
            };
            Ok(cache.dir().join(format!(
                "{}.nc",
                cache.key(&probe).map_err(|e| rerr(op, e))?
            )))
        })();
        let (path, is_cached) = match &cached {
            Ok(p) => (Some(p.display().to_string()), p.is_file()),
            Err(_) => (None, false),
        };
        serde_json::json!({
            "operator": op,
            "generator": format!("cdo {}", method.cdo_operator()),
            "target": target,
            "source_grid": format!("{} ({} cells)", grid.kind.name(), grid.size()),
            "cached": is_cached,
            "generate": !is_cached,
            "path": path,
        })
    }

    fn weights(&self, op: &str) -> Result<Arc<RemapWeights>> {
        self.cell
            .get_or_init(|| self.generate(op).map(Arc::new))
            .clone()
    }

    fn generate(&self, op: &str) -> Result<RemapWeights> {
        let WeightSpec::Gen {
            method,
            target,
            grid,
            hdims,
        } = &self.spec
        else {
            return Err(Error::internal("precomputed weights were not loaded"));
        };
        let cache = WeightCache::from_env().map_err(|e| rerr(op, e))?;
        let identity = grid_identity(grid)?;
        let ds = grid.src.dataset();
        // A local NetCDF file with the grid as stored: cdo reads it directly.
        let direct = (!is_subset(grid) && is_local_netcdf(&ds.source))
            .then(|| {
                ds.data_vars()
                    .find(|v| v.grid.is_some_and(|gi| ds.grids[gi].same_as(&grid.base)))
                    .map(|v| v.name.clone())
            })
            .flatten();
        // With the weights cached already, no source file is needed (the key ignores it).
        let probe = WeightRequest {
            method: *method,
            target,
            source: Path::new(""),
            variable: None,
            identity: SourceIdentity::Bytes(&identity),
        };
        let cached = cache.dir().join(format!(
            "{}.nc",
            cache.key(&probe).map_err(|e| rerr(op, e))?
        ));
        let (source, variable) = match direct {
            Some(v) => (PathBuf::from(&ds.source), Some(v)),
            None if cached.is_file() => (PathBuf::new(), None),
            None => {
                let key = format!("src-{}", hash_hex(&identity));
                (write_grid_file(&cache, grid, hdims, &key)?, None)
            }
        };
        let req = WeightRequest {
            method: *method,
            target,
            source: &source,
            variable: variable.as_deref(),
            identity: SourceIdentity::Bytes(&identity),
        };
        let path = cache.weights_for(&req).map_err(|e| rerr(op, e))?;
        let w = RemapWeights::read(&path).map_err(|e| rerr(op, e))?;
        check_sizes(op, &w, grid.size(), self.dst_size, &path)?;
        Ok(w)
    }
}

fn check_sizes(op: &str, w: &RemapWeights, src: usize, dst: usize, path: &Path) -> Result<()> {
    if w.src_size() != src || w.dst_size() != dst {
        return Err(Error::new(
            ErrorCode::UnsupportedGrid,
            format!(
                "weights {} map {} -> {} cells, but the source grid has {src} and the target grid {dst}",
                path.display(),
                w.src_size(),
                w.dst_size()
            ),
        )
        .with("operator", op.to_owned())
        .with_hint("the weight file must be generated for this source grid and target grid"));
    }
    Ok(())
}

fn apply_t<T: crate::remap::Value, U: crate::remap::Value>(
    w: &RemapWeights,
    src: &[T],
    n: usize,
    wrap: fn(Vec<U>) -> Values,
) -> std::result::Result<Values, RemapError> {
    let mut dst = vec![U::from_f64(f64::NAN); n * w.dst_size()];
    w.apply_batch(src, &mut dst)?;
    Ok(wrap(dst))
}

impl FieldMap for RemapMap {
    fn plan_info(&self) -> Option<serde_json::Value> {
        let w: Vec<serde_json::Value> = self.grids.iter().map(|g| g.plan_info(&self.op)).collect();
        Some(serde_json::json!({ "remap_weights": w }))
    }

    fn apply(&self, var: usize, src: &Values, n: usize, out: DType) -> Result<Values> {
        let w = self.grids[self.var_grid[var]].weights(&self.op)?;
        let r = match (src, out) {
            (Values::F32(s), DType::F32) => apply_t::<f32, f32>(&w, s, n, Values::F32),
            (Values::F32(s), _) => apply_t::<f32, f64>(&w, s, n, Values::F64),
            (Values::F64(s), DType::F32) => apply_t::<f64, f32>(&w, s, n, Values::F32),
            (Values::F64(s), _) => apply_t::<f64, f64>(&w, s, n, Values::F64),
        };
        r.map_err(|e| rerr(&self.op, e))
    }
}

/// Output grid from the target-grid argument: cdo's template, or a Zarr store's grid written by
/// cdors. Returns the grid, its horizontal dimensions and the target argument for `cdo gen*`.
fn target_grid(
    op: &str,
    cache: &WeightCache,
    target: &str,
) -> Result<(GridDesc, Vec<VarDim>, String)> {
    let (path, gen_target) = if crate::io::is_zarr(target) {
        let src = crate::io::open(target)?;
        let ds = src.dataset();
        let v = ds.data_vars().find(|v| v.grid.is_some()).ok_or_else(|| {
            Error::new(
                ErrorCode::NoCoordinates,
                format!("target dataset '{target}' has no variable on a horizontal grid"),
            )
        })?;
        let g = grid_of(&src, v.grid.expect("grid"));
        let hd: Vec<VarDim> = v
            .dims
            .iter()
            .filter(|d| d.role == DimRole::Horizontal)
            .cloned()
            .collect();
        let key = format!("tgt-{}", hash_hex(&grid_identity(&g)?));
        let p = write_grid_file(cache, &g, &hd, &key)?;
        let s = p.to_string_lossy().into_owned();
        (p, s)
    } else {
        (
            cache.grid_template(target).map_err(|e| rerr(op, e))?,
            target.to_owned(),
        )
    };
    let src = crate::io::open(&path.to_string_lossy())?;
    let ds = src.dataset();
    let v = ds.data_vars().find(|v| v.grid.is_some()).ok_or_else(|| {
        Error::internal(format!(
            "grid template {} has no data variable",
            path.display()
        ))
    })?;
    let hd: Vec<VarDim> = v
        .dims
        .iter()
        .filter(|d| d.role == DimRole::Horizontal)
        .cloned()
        .collect();
    let g = grid_of(&src, v.grid.expect("grid"));
    Ok((g, hd, gen_target))
}

/// The selection (one map per horizontal dimension of `g`) that keeps only the source cells the
/// links of `w` read, and the kept cells as flat source indices (ascending); `None` when it would
/// keep more than half of the grid.
fn prune(g: &GridDesc, w: &RemapWeights) -> Option<(Vec<IndexMap>, Vec<usize>)> {
    let used = w.used_sources();
    let n = w.src_size();
    if used.is_empty() {
        // nothing linked: the fields are still read, all results are missing
        return None;
    }
    if g.sel.len() == 2 {
        let nx = g.sel[1].len();
        if nx == 0 || g.sel[0].len() * nx != n {
            return None;
        }
        let mut rows: Vec<usize> = used.iter().map(|&f| f / nx).collect();
        rows.dedup();
        let mut cols: Vec<usize> = used.iter().map(|&f| f % nx).collect();
        cols.sort_unstable();
        cols.dedup();
        if rows.len() * cols.len() * 2 > n {
            return None;
        }
        let cells = rows
            .iter()
            .flat_map(|&y| cols.iter().map(move |&x| y * nx + x))
            .collect();
        Some((
            vec![IndexMap::from_list(rows), IndexMap::from_list(cols)],
            cells,
        ))
    } else {
        if used.len() * 2 > n || g.sel.len() != 1 {
            return None;
        }
        Some((vec![IndexMap::from_list(used.clone())], used))
    }
}

/// Output description of a remapping operator.
pub fn describe(node: &OpNode, mut inputs: Vec<Desc>, plan_only: bool) -> Result<Desc> {
    let op = node.name.as_str();
    let mut input = inputs
        .pop()
        .ok_or_else(|| Error::internal("remap without input"))?;
    no_pending_fold(op, &input)?;
    let target = node.args[0].as_str();
    let cache = WeightCache::from_env().map_err(|e| rerr(op, e))?;
    let (tgrid, thdims, gen_target) = target_grid(op, &cache, target)?;
    let dst_size = tgrid.size();

    let method = match op {
        "remap" => None,
        _ => Some(GenMethod::parse(op).ok_or_else(|| Error::internal("remap method"))?),
    };
    let ready = match method {
        None => {
            let wpath = Path::new(&node.args[1]);
            if !wpath.is_file() {
                return Err(Error::new(
                    ErrorCode::MissingInput,
                    format!("weight file '{}' does not exist", wpath.display()),
                )
                .with("operator", op.to_owned()));
            }
            let w = RemapWeights::read(wpath).map_err(|e| rerr(op, e))?;
            Some((Arc::new(w), wpath.to_owned()))
        }
        Some(_) => None,
    };

    // weights per input grid
    let mut grids: Vec<GridWeights> = Vec::new();
    let mut grid_index: Vec<(usize, usize)> = Vec::new(); // (input grid, entry)
    let mut var_grid = Vec::with_capacity(input.vars.len());
    let mut fvars = Vec::with_capacity(input.vars.len());
    let mut out_vars = Vec::with_capacity(input.vars.len());
    for v in &input.vars {
        let gi = v.grid.ok_or_else(|| {
            Error::new(
                ErrorCode::NoCoordinates,
                format!("variable '{}' has no horizontal grid", v.name),
            )
            .with("operator", op.to_owned())
            .with("variable", v.name.clone())
            .with_hint("select variables on a horizontal grid with -selname")
        })?;
        let hfirst = trailing_hdims(op, v)?;
        let g = &input.grids[gi];
        let entry = match grid_index.iter().find(|(i, _)| *i == gi) {
            Some(&(_, e)) => e,
            None => {
                if g.kind == GridKind::Generic {
                    return Err(Error::new(
                        ErrorCode::NoCoordinates,
                        format!("variable '{}' has no horizontal coordinates", v.name),
                    )
                    .with("operator", op.to_owned())
                    .with_hint("attach coordinates with -setgrid,<grid file>"));
                }
                // cdo's genbil aborts on unstructured sources (also HEALPix subsets, which are
                // written as unstructured)
                if method == Some(GenMethod::Bil) && g.kind == GridKind::Unstructured {
                    return Err(Error::new(
                        ErrorCode::UnsupportedGrid,
                        "remapbil: cdo does not support bilinear interpolation from unstructured \
                         source grids",
                    )
                    .with("operator", op.to_owned())
                    .with("variable", v.name.clone())
                    .with_hint("use remapdis, remapnn or remapcon"));
                }
                let spec = match (&ready, method) {
                    (Some((w, p)), _) => {
                        check_sizes(op, w, g.size(), dst_size, p)?;
                        WeightSpec::Ready
                    }
                    (None, Some(m)) => WeightSpec::Gen {
                        method: m,
                        target: gen_target.clone(),
                        grid: Box::new(g.clone()),
                        hdims: v.dims[hfirst..].to_vec(),
                    },
                    (None, None) => unreachable!("remap has a weight file"),
                };
                let cell = OnceLock::new();
                if let Some((w, _)) = &ready {
                    let _ = cell.set(Ok(w.clone()));
                }
                grids.push(GridWeights {
                    spec,
                    dst_size,
                    cell,
                });
                grid_index.push((gi, grids.len() - 1));
                grids.len() - 1
            }
        };
        var_grid.push(entry);
        fvars.push(FieldVar {
            hfirst,
            src_size: g.size(),
            out_hdims: thdims.iter().map(|d| d.size).collect(),
            out_dtype: v.dtype,
        });
        let mut ov = v.clone();
        ov.dims.truncate(hfirst);
        ov.dims.extend(thdims.iter().cloned());
        ov.grid = Some(0);
        out_vars.push(ov);
    }
    if out_vars.is_empty() {
        return Err(Error::bad_arguments(format!("{op}: no variables to remap")));
    }
    // read only the source cells the weights link
    for &(gi, entry) in &grid_index {
        let gw = &mut grids[entry];
        if plan_only && !gw.at_hand(op) {
            continue;
        }
        let w = gw.weights(op)?;
        let Some((sel, cells)) = prune(&input.grids[gi], &w) else {
            continue;
        };
        let rw = w.restrict_sources(&cells).map_err(|e| rerr(op, e))?;
        gw.cell = OnceLock::new();
        let _ = gw.cell.set(Ok(Arc::new(rw)));
        let g = &mut input.grids[gi];
        if let Some(xv) = &mut g.xvals {
            *xv = sel[sel.len() - 1].to_vec().iter().map(|&i| xv[i]).collect();
        }
        for (k, m) in sel.iter().enumerate() {
            g.sel[k] = g.sel[k].compose(m);
        }
        if g.kind == GridKind::Healpix {
            g.kind = GridKind::Unstructured;
        }
        for (vi, v) in input.vars.iter_mut().enumerate() {
            if v.grid != Some(gi) {
                continue;
            }
            let hd = v.hdims();
            for (k, m) in sel.iter().enumerate() {
                v.select(hd[k], m);
            }
            fvars[vi].src_size = cells.len();
        }
    }
    let kernel = FieldKernel {
        map: Arc::new(RemapMap {
            op: op.to_owned(),
            grids,
            var_grid,
        }),
        vars: fvars,
    };
    Ok(Desc {
        attrs: input.attrs.clone(),
        vars: out_vars,
        grids: vec![tgrid],
        zaxes: input.zaxes.clone(),
        time: input.time.clone(),
        fold: Some(Fold {
            input: Box::new(input),
            kernel: Arc::new(kernel),
        }),
    })
}
