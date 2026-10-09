//! Executor: runs a plan and writes the output.
//!
//! [`run`] plans the command, refuses an existing output without `-O`, creates the writer on a
//! temporary name in the output's directory, streams the stage through [`pipeline::run`] and
//! renames the result into place on success. On failure it removes its own temporary output and
//! the error says how far the run got (`stage`, `chunks_done`, `chunks_total`).
//!
//! Before any output is created, the planned decoded bytes are checked against `--max-read`
//! (default 64 GB on login nodes, none inside Slurm jobs; `plan::explain`).

pub mod pipeline;
pub mod progress;
pub mod publish;
pub mod threads;

use crate::chain::{Command, Precision};
use crate::error::{Error, ErrorCode, Result};
use crate::io::Values;
use crate::model::{AttrValue, Attrs, DType, DimRole};
use crate::plan::{self, OutKind, Plan, VarDesc};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Layout of one written data variable.
#[derive(Debug, Clone)]
pub struct OutVar {
    pub name: String,
    pub dims: Vec<String>,
    pub shape: Vec<usize>,
    /// Output chunk shape (also the unit the pipeline hands to the writer).
    pub chunks: Vec<usize>,
    /// Written type.
    pub dtype: DType,
    pub missval: f64,
}

impl OutVar {
    pub fn chunk_counts(&self) -> Vec<usize> {
        self.shape
            .iter()
            .zip(&self.chunks)
            .map(|(&n, &c)| n.div_ceil(c.max(1)))
            .collect()
    }
}

/// An output file being written. `write` gets complete output chunks (trimmed at the array
/// edge, NaN for missing values).
pub trait Writer: Send + Sync {
    /// Whether chunks must be written one at a time in canonical order (NetCDF); otherwise
    /// `write` may be called from several threads at once (Zarr).
    fn ordered(&self) -> bool;
    fn write(&self, var: usize, origin: &[usize], shape: &[usize], data: Values) -> Result<()>;
    fn finish(&self) -> Result<()>;
    /// The value that missing values (NaN) of variable `var` become in the file, if the writer
    /// replaces them (NetCDF: the variable's missing value). The pipeline replaces them while
    /// it assembles output chunks on the compute threads and hands the chunks to
    /// [`Writer::write_ready`].
    fn missing_as(&self, _var: usize) -> Option<f64> {
        None
    }
    /// `write` for a chunk whose missing values are already replaced (see
    /// [`Writer::missing_as`]).
    fn write_ready(
        &self,
        var: usize,
        origin: &[usize],
        shape: &[usize],
        data: Values,
    ) -> Result<()> {
        self.write(var, origin, shape, data)
    }
}

/// Compute threads: `-P`, else all cores of the allocation, capped at 16 outside Slurm jobs
/// (shared login nodes).
pub fn default_threads(cmd: &Command) -> usize {
    let avail = std::thread::available_parallelism().map_or(4, |n| n.get());
    let n = cmd.options.threads.unwrap_or(avail);
    if std::env::var_os("SLURM_JOB_ID").is_none() {
        n.min(16)
    } else {
        n
    }
}

/// Blocking reads in flight: `--io-threads`, else 64, in Slurm jobs and on login nodes alike
/// (cold Lustre reads are latency-bound: on a login node W1 decodes about 5.5 GB/s with 32
/// reads in flight and 9-10 GB/s with 64, see `docs/baseline.md`).
pub fn default_io_threads(cmd: &Command) -> usize {
    cmd.options.io_threads.unwrap_or(64)
}

/// Output chunks: `--chunks dim=n` where given; otherwise one timestep per chunk, and for Zarr
/// about 4 MiB per chunk filled from the last dimension, for NetCDF one field (all horizontal
/// points of one level), as cdo writes.
pub fn out_chunks(
    v: &VarDesc,
    kind: OutKind,
    size: usize,
    spec: Option<&[(String, usize)]>,
) -> Vec<usize> {
    let mut c = vec![1usize; v.dims.len()];
    match kind {
        OutKind::Zarr2 | OutKind::Zarr3 => {
            let mut room = (4 << 20) / size.max(1);
            for d in (0..v.dims.len()).rev() {
                if v.dims[d].role == DimRole::Time {
                    continue;
                }
                let n = v.dims[d].size.max(1);
                c[d] = n.min(room.max(1));
                room /= c[d];
            }
        }
        OutKind::Nc4 | OutKind::NcClassic => {
            for (cd, d) in c.iter_mut().zip(&v.dims) {
                if d.role == DimRole::Horizontal {
                    *cd = d.size.max(1);
                }
            }
        }
    }
    if let Some(spec) = spec {
        for (name, n) in spec {
            if let Some(d) = v.dims.iter().position(|d| &d.name == name) {
                c[d] = (*n).min(v.dims[d].size).max(1);
            }
        }
    }
    c
}

/// Data-variable layouts of a plan's output. When the output stage runs in lane waves, the
/// output chunks follow the lanes (`plan::schedule`), unless `--chunks` says otherwise.
pub fn layout(plan: &Plan, cmd: &Command) -> Vec<OutVar> {
    let last = plan.stages.last();
    plan.desc
        .vars
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let lane_chunks = last.and_then(|st| {
                let sv = st.vars.get(i)?;
                plan::schedule::out_chunks_for(sv, st.sched.vars.get(i)?, &v.dims)
            });
            let dtype = match cmd.options.precision {
                Some(Precision::F32) => DType::F32,
                Some(Precision::F64) => DType::F64,
                None if v.dtype == DType::F32 => DType::F32,
                None => DType::F64,
            };
            OutVar {
                name: v.name.clone(),
                dims: v.dims.iter().map(|d| d.name.clone()).collect(),
                shape: v.shape(),
                chunks: match lane_chunks {
                    Some(c) if cmd.options.chunks.is_none() => c,
                    _ => out_chunks(
                        v,
                        plan.out_kind,
                        dtype.size(),
                        cmd.options.chunks.as_deref(),
                    ),
                },
                dtype,
                missval: v.missval,
            }
        })
        .collect()
}

/// The `history` line: date, program and command line.
fn history_line() -> String {
    let args: Vec<String> = std::env::args().collect();
    let now = std::process::Command::new("date")
        .arg("+%a %b %d %H:%M:%S %Y")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .unwrap_or_default();
    let prog = Path::new(args.first().map_or("cdors", String::as_str))
        .file_name()
        .map_or_else(|| "cdors".into(), |s| s.to_string_lossy().into_owned());
    format!(
        "{now}: {prog} {} (cdors {})",
        args[1..].join(" "),
        crate::VERSION
    )
}

/// Everything the command reads that the output must not replace: input paths and glob
/// patterns, and operator arguments naming existing files (grids, weights).
fn input_paths(cmd: &Command) -> Vec<&str> {
    fn args<'a>(n: &'a crate::chain::OpNode, out: &mut Vec<&'a str>) {
        out.extend(
            n.args
                .iter()
                .map(String::as_str)
                .filter(|a| Path::new(a).exists()),
        );
        for i in &n.inputs {
            if let crate::chain::Input::Op(o) = i {
                args(o, out);
            }
        }
    }
    let mut v = cmd.root.paths();
    args(&cmd.root, &mut v);
    v
}

/// Runs the stages of `plan`: inner stages first, each into its in-memory intermediate, then
/// the output stage into `writer` (laid out as `lay`; not finished here). Each intermediate is
/// freed as soon as the stage reading it has finished.
pub fn run_stages(plan: &Plan, lay: &[OutVar], writer: Arc<dyn Writer>) -> Result<()> {
    for (i, stage) in plan.stages.iter().enumerate() {
        progress::set_stage(i);
        let settings = pipeline::Settings {
            threads: plan.threads,
            io_threads: plan.io_threads,
            window: stage.sched.window,
            any_order: stage.sched.any_order(),
        };
        match plan.intermediates.get(i) {
            Some(im) => {
                pipeline::run(stage, &im.lay, im.writer.clone(), settings)?;
                im.writer.finish()?;
            }
            None => pipeline::run(stage, lay, writer.clone(), settings)?,
        }
        for im in plan
            .intermediates
            .iter()
            .filter(|im| im.consumer_stage == i)
        {
            im.release();
        }
    }
    Ok(())
}

/// `--max-read` against the plan's estimate. A plan made without cdo reads whole source grids
/// for remappings whose weights are not cached yet (the weights would narrow the reads).
pub fn check_read_limit(plan: &Plan, cmd: &Command) -> Result<()> {
    let stats = plan::explain::read_stats(plan);
    plan::explain::check_read_limit(
        plan::explain::source_bytes(plan, &stats),
        plan::explain::read_limit(cmd),
    )
    .map_err(|mut e| {
        if plan.deferred {
            let h = e.hint.take().unwrap_or_default();
            e.hint = Some(format!(
                "{h}; remapping reads only the source cells its weights use once they are cached: \
                 a small first run (e.g. with -seltimestep,1) makes them"
            ));
        }
        e
    })
}

/// Plans and runs a command that writes one output.
pub fn run(cmd: &Command) -> Result<String> {
    use crate::io::netcdf4_index::{hold_cache_writes, release_cache_writes};
    if cmd.options.plan {
        // nothing is written into $CDORS_CACHE (no cdo, no NetCDF-4 chunk indexes)
        hold_cache_writes();
        let plan = plan::build(cmd, plan::CdoUse::Never)?;
        // pools sized to the work while planning (`plan::build`)
        let p = plan::explain::to_json(&plan, cmd, plan.threads, plan.io_threads);
        return Ok(if cmd.options.json {
            format!("{p}\n")
        } else {
            plan::explain::to_text(&p)
        });
    }
    // Refusals come first, before planning runs cdo (remapping templates and weights) or writes
    // into $CDORS_CACHE: the output checks, then --max-read on a plan made without cdo (the
    // estimate --plan shows).
    let output = cmd
        .outputs
        .first()
        .ok_or_else(|| Error::bad_arguments("no output file given"))?;
    let out = PathBuf::from(output);
    publish::check_not_input(&out, &input_paths(cmd))?;
    publish::check_output(&out, cmd.options.overwrite)?;
    if let Some(dir) = out.parent().filter(|p| !p.as_os_str().is_empty())
        && !dir.is_dir()
    {
        return Err(Error::bad_arguments(format!(
            "the directory of output '{output}' does not exist"
        ))
        .with("path", output.clone()));
    }
    hold_cache_writes();
    let plan = plan::build(cmd, plan::CdoUse::Defer)?;
    check_read_limit(&plan, cmd)?;
    release_cache_writes(true);
    // planned again with cdo's grids and weights (which also narrow the reads)
    let plan = if plan.deferred {
        plan::build(cmd, plan::CdoUse::Now)?
    } else {
        plan
    };
    let stats = plan::explain::read_stats(&plan);
    let bytes_decoded: u64 = stats.iter().map(|s| s.bytes_decoded).sum();
    let is_dir = matches!(plan.out_kind, OutKind::Zarr3 | OutKind::Zarr2);
    if is_dir {
        // the output name is reserved (exclusive mkdir) before anything is written
        publish::reserve_dir(&out, cmd.options.overwrite)?;
    }
    let lay = layout(&plan, cmd);
    let history = (!cmd.options.no_history).then(history_line);
    let tmp = publish::temp_path(&out);
    progress::start(plan.stages.len(), stats.iter().map(|s| s.chunks_read).sum());
    let reporter = cmd
        .options
        .progress_json
        .then(|| progress::Reporter::spawn(bytes_decoded));
    let result = (|| -> Result<()> {
        let writer: Arc<dyn Writer> = match plan.out_kind {
            OutKind::Nc4 | OutKind::NcClassic => {
                Arc::new(crate::io::write_netcdf::NcWriter::create(
                    &tmp,
                    &plan,
                    &lay,
                    plan.out_kind == OutKind::NcClassic,
                    history.as_deref(),
                )?)
            }
            OutKind::Zarr3 | OutKind::Zarr2 => Arc::new(crate::io::write_zarr::ZarrWriter::create(
                &tmp,
                &plan,
                &lay,
                plan.out_kind == OutKind::Zarr2,
                history.as_deref(),
            )?),
        };
        run_stages(&plan, &lay, writer.clone())?;
        writer.finish()
    })();
    let result = result.and_then(|()| publish::publish(&tmp, &out, is_dir, cmd.options.overwrite));
    match result {
        Ok(()) => {
            if let Some(r) = reporter {
                r.finish("ok", None, &plan.output);
            }
            Ok(String::new())
        }
        Err(e) => {
            let removed = publish::remove_own(&tmp, &out);
            let (stage, done, total, _) = progress::snapshot();
            let e = e
                .with("stage", stage)
                .with("chunks_done", done)
                .with("chunks_total", total)
                .with("temporary_output_removed", removed);
            if let Some(r) = reporter {
                r.finish("failed", Some(e.code.as_str()), &plan.output);
            }
            Err(e)
        }
    }
}

/// A coordinate (or bounds, or grid-mapping) variable to write, with its values.
#[derive(Debug, Clone)]
pub struct OutCoord {
    pub name: String,
    pub dims: Vec<String>,
    pub shape: Vec<usize>,
    /// F32, F64 or I32 (grid mapping).
    pub dtype: DType,
    pub values: Vec<f64>,
    pub attrs: Attrs,
}

/// Everything a writer needs besides the data: dimensions, coordinates and attributes.
#[derive(Debug, Clone)]
pub struct OutMeta {
    /// (name, size, unlimited) in definition order.
    pub dims: Vec<(String, usize, bool)>,
    pub coords: Vec<OutCoord>,
    /// Attributes of each data variable (same order as the layout).
    pub var_attrs: Vec<Attrs>,
    pub global: Attrs,
}

fn copy_attrs(a: &Attrs, drop: &[&str]) -> Attrs {
    Attrs(
        a.iter()
            .filter(|(k, _)| {
                !drop.contains(&k.as_str())
                    && !matches!(
                        k.as_str(),
                        "_FillValue"
                            | "missing_value"
                            | "_ARRAY_DIMENSIONS"
                            | "_Netcdf4Dimid"
                            | "_Netcdf4Coordinates"
                    )
            })
            .cloned()
            .collect(),
    )
}

fn set_attr(a: &mut Attrs, k: &str, v: AttrValue) {
    match a.0.iter_mut().find(|(n, _)| n == k) {
        Some(e) => e.1 = v,
        None => a.0.push((k.to_owned(), v)),
    }
}

fn text(s: &str) -> AttrValue {
    AttrValue::Text(s.to_owned())
}

/// Builds dimensions, coordinate variables and attributes of the output.
pub fn out_meta(plan: &Plan, lay: &[OutVar], history: Option<&str>) -> Result<OutMeta> {
    let desc = &plan.desc;
    let mut dims: Vec<(String, usize, bool)> = Vec::new();
    let add_dim =
        |dims: &mut Vec<(String, usize, bool)>, name: &str, n: usize, unl: bool| -> Result<()> {
            match dims.iter().find(|d| d.0 == name) {
                Some(d) if d.1 != n => Err(Error::new(
                    ErrorCode::UnsupportedDimension,
                    format!(
                        "dimension '{name}' would have two sizes ({} and {n}) in the output",
                        d.1
                    ),
                )),
                Some(_) => Ok(()),
                None => {
                    dims.push((name.to_owned(), n, unl));
                    Ok(())
                }
            }
        };
    for v in &desc.vars {
        for d in &v.dims {
            add_dim(&mut dims, &d.name, d.size, d.role == DimRole::Time)?;
        }
    }
    let mut coords: Vec<OutCoord> = Vec::new();
    let push = |coords: &mut Vec<OutCoord>, c: OutCoord| {
        if !coords.iter().any(|x| x.name == c.name) {
            coords.push(c);
        }
    };
    // time
    if let Some(t) = &desc.time
        && desc.vars.iter().any(|v| v.dim_of(DimRole::Time).is_some())
    {
        let tdim = t.axis.dim.clone();
        let mut attrs = copy_attrs(&t.attrs, &["bounds"]);
        if let Some(b) = &t.raw_bounds {
            add_dim(&mut dims, "bnds", 2, false)?;
            let bname = t
                .axis
                .bounds_var
                .clone()
                .unwrap_or_else(|| format!("{}_bnds", t.axis.var));
            set_attr(&mut attrs, "bounds", text(&bname));
            push(
                &mut coords,
                OutCoord {
                    name: bname,
                    dims: vec![tdim.clone(), "bnds".into()],
                    shape: vec![b.len(), 2],
                    dtype: DType::F64,
                    values: b.iter().flat_map(|p| [p[0], p[1]]).collect(),
                    attrs: Attrs::default(),
                },
            );
        }
        coords.insert(
            0,
            OutCoord {
                name: t.axis.var.clone(),
                dims: vec![tdim],
                shape: vec![t.len()],
                dtype: DType::F64,
                values: t.raw.clone(),
                attrs,
            },
        );
    }
    // vertical axes
    for (zi, z) in desc.zaxes.iter().enumerate() {
        if !desc.vars.iter().any(|v| v.zaxis == Some(zi)) {
            continue;
        }
        let mut attrs = copy_attrs(&z.attrs, &["bounds"]);
        if let Some(b) = &z.axis.bounds {
            add_dim(&mut dims, "bnds", 2, false)?;
            let bname = z
                .bounds_var
                .clone()
                .unwrap_or_else(|| format!("{}_bnds", z.axis.var));
            set_attr(&mut attrs, "bounds", text(&bname));
            push(
                &mut coords,
                OutCoord {
                    name: bname,
                    dims: vec![z.axis.dim.clone(), "bnds".into()],
                    shape: vec![b.len(), 2],
                    dtype: DType::F64,
                    values: b.iter().flat_map(|p| [p[0], p[1]]).collect(),
                    attrs: Attrs::default(),
                },
            );
        }
        push(
            &mut coords,
            OutCoord {
                name: z.axis.var.clone(),
                dims: vec![z.axis.dim.clone()],
                shape: vec![z.axis.len()],
                dtype: DType::F64,
                values: z.axis.values.clone(),
                attrs,
            },
        );
    }
    // grids
    let mut var_attrs: Vec<Attrs> = desc.vars.iter().map(|v| v.attrs.clone()).collect();
    for (gi, g) in desc.grids.iter().enumerate() {
        let users: Vec<usize> = (0..desc.vars.len())
            .filter(|&i| desc.vars[i].grid == Some(gi))
            .collect();
        let Some(&first) = users.first() else {
            continue;
        };
        let v = &desc.vars[first];
        let hd: Vec<String> = v.hdims().iter().map(|&i| v.dims[i].name.clone()).collect();
        let sattrs = |name: &str| match &g.fixed {
            Some(f) if g.base.x.as_ref().is_some_and(|x| x.var == name) => f.xattrs.clone(),
            Some(f) => f.yattrs.clone(),
            None => g
                .src
                .dataset()
                .var(name)
                .map(|v| copy_attrs(&v.attrs, &["bounds"]))
                .unwrap_or_default(),
        };
        let mut extra: Vec<(String, AttrValue)> = Vec::new();
        if g.is_healpix() {
            if let Some(m) = &g.base.mapping {
                push(
                    &mut coords,
                    OutCoord {
                        name: m.var.clone(),
                        dims: vec![],
                        shape: vec![],
                        dtype: DType::I32,
                        values: vec![0.0],
                        attrs: copy_attrs(&m.attrs, &[]),
                    },
                );
                extra.push(("grid_mapping".into(), text(&m.var)));
            }
        } else if let Some(c) = g.coords()? {
            let (xname, yname, xa, ya, f32c) = if g.base.kind == crate::model::GridKind::Healpix {
                let mut xa = Attrs::default();
                set_attr(&mut xa, "standard_name", text("longitude"));
                set_attr(&mut xa, "long_name", text("longitude"));
                set_attr(&mut xa, "units", text(&c.xunits));
                let mut ya = Attrs::default();
                set_attr(&mut ya, "standard_name", text("latitude"));
                set_attr(&mut ya, "long_name", text("latitude"));
                set_attr(&mut ya, "units", text(&c.yunits));
                ("lon".to_owned(), "lat".to_owned(), xa, ya, true)
            } else {
                let (x, y) = (
                    g.base.x.as_ref().expect("x axis"),
                    g.base.y.as_ref().expect("y axis"),
                );
                (
                    x.var.clone(),
                    y.var.clone(),
                    sattrs(&x.var),
                    sattrs(&y.var),
                    x.is_f32,
                )
            };
            let dt = if f32c { DType::F32 } else { DType::F64 };
            let (xd, yd, xs, ys): (Vec<String>, Vec<String>, Vec<usize>, Vec<usize>) = match g.kind
            {
                crate::model::GridKind::Regular | crate::model::GridKind::Gaussian => (
                    vec![hd[1].clone()],
                    vec![hd[0].clone()],
                    vec![c.xvals.len()],
                    vec![c.yvals.len()],
                ),
                _ => {
                    let shape: Vec<usize> = hd
                        .iter()
                        .map(|n| v.dims.iter().find(|d| &d.name == n).map_or(0, |d| d.size))
                        .collect();
                    (hd.clone(), hd.clone(), shape.clone(), shape)
                }
            };
            let (mut xa, mut ya) = (xa, ya);
            if let (Some(xb), Some(yb)) = (&c.xbounds, &c.ybounds) {
                let vdim = if c.nv == 2
                    && matches!(
                        g.kind,
                        crate::model::GridKind::Regular | crate::model::GridKind::Gaussian
                    ) {
                    "bnds".to_owned()
                } else {
                    g.base.vdim.clone().unwrap_or_else(|| "nv".into())
                };
                add_dim(&mut dims, &vdim, c.nv, false)?;
                for (cn, a, b, d, s) in [
                    (&xname, &mut xa, xb, &xd, &xs),
                    (&yname, &mut ya, yb, &yd, &ys),
                ] {
                    let bname = format!("{cn}_bnds");
                    set_attr(a, "bounds", text(&bname));
                    let mut bd = d.clone();
                    bd.push(vdim.clone());
                    let mut bs = s.clone();
                    bs.push(c.nv);
                    push(
                        &mut coords,
                        OutCoord {
                            name: bname,
                            dims: bd,
                            shape: bs,
                            dtype: dt,
                            values: b.clone(),
                            attrs: Attrs::default(),
                        },
                    );
                }
            }
            push(
                &mut coords,
                OutCoord {
                    name: xname.clone(),
                    dims: xd,
                    shape: xs,
                    dtype: dt,
                    values: c.xvals,
                    attrs: xa,
                },
            );
            push(
                &mut coords,
                OutCoord {
                    name: yname.clone(),
                    dims: yd,
                    shape: ys,
                    dtype: dt,
                    values: c.yvals,
                    attrs: ya,
                },
            );
            if !matches!(
                g.kind,
                crate::model::GridKind::Regular | crate::model::GridKind::Gaussian
            ) {
                extra.push(("coordinates".into(), text(&format!("{yname} {xname}"))));
            }
            if g.kind == crate::model::GridKind::Unstructured {
                extra.push(("CDI_grid_type".into(), text("unstructured")));
            }
        }
        for &u in &users {
            for (k, a) in &extra {
                set_attr(&mut var_attrs[u], k, a.clone());
            }
        }
    }
    // dimensions first used by coordinates only keep their order after the data dimensions
    let _ = lay;
    let mut global = desc.attrs.clone();
    if let Some(h) = history {
        let line = match global.get_str("history") {
            Some(old) if !old.is_empty() => format!("{h}\n{old}"),
            _ => h.to_owned(),
        };
        set_attr(&mut global, "history", text(&line));
    }
    Ok(OutMeta {
        dims,
        coords,
        var_attrs,
        global,
    })
}
