//! `--plan`: what a run will read and how, before any data is read; and the `--max-read` guard.
//!
//! [`read_stats`] counts, per stage and stored variable ("leaf"), the chunks the pipeline will
//! fetch and the bytes they decode to (every fetch counts: a chunk read by two tiles counts
//! twice, as the pipeline reads it twice). It runs before every data run, for `--max-read` and
//! `--progress`. [`to_json`] adds what only `--plan` needs: compressed bytes (exact sizes of a
//! small sample of chunks, scaled to all chunks, so an estimate), the memory estimate, remap
//! weights that cdo would have to generate, and the thread and I/O settings. [`to_text`] renders
//! that JSON for humans. There is no wall-time estimate.
//!
//! The JSON is the stable interface for agents (`"cdors_plan": 1` is its schema version):
//! top-level `output`, `format`, `inputs` (every file read, at most 100),
//! `inputs_count`, `inputs_truncated`, `stages[]` (`operators`, `ignored_operators`, `fold_kernel`, `passes`,
//! `tiles`, `chunks_read`, `bytes_decoded`, `bytes_compressed`, `kernel` (for `fld*`:
//! `area_weights[]`), `variables[].leaves[]`), `totals`, `memory`, `remap_weights`, `settings`,
//! `read_limit` and `warnings[]` (`warning`, `message`: results that are probably wrong, such as
//! field means over a grid without cell areas). Byte counts are plain integers.

use super::stage::Stage;
use super::tiling::{LeafInfo, LeafRead, VarTiling};
use super::{IndexMap, Plan, leaf_dtype};
use crate::chain::{Command, Input, OpNode};
use crate::error::{Error, ErrorCode, Result};
use crate::model::{DType, DimRole};
use crate::ops::{self, AccessClass};
use serde_json::{Value, json};
use std::ops::Range;

/// Schema version of the `--plan --json` output.
pub const SCHEMA: u32 = 1;
/// Input files listed by `--plan --json` (`inputs`); `inputs_count` gives the total.
const MAX_LISTED_INPUTS: usize = 100;
/// `--max-read` default outside Slurm jobs (shared login nodes): 64 GB.
pub const LOGIN_NODE_MAX_READ: u64 = 64_000_000_000;
/// Chunks per leaf whose stored size is looked up for the compressed-bytes estimate.
const SAMPLE: usize = 8;
/// Stored-size lookups for the whole plan (remote stores answer each with a request); leaves
/// beyond the budget use the compression ratio of the chunks sampled so far.
const SAMPLE_TOTAL: usize = 64;

/// Reads of one stored variable in one stage.
#[derive(Debug, Clone)]
pub struct LeafStats {
    /// Stage variable and leaf index within it.
    pub var: usize,
    pub leaf: usize,
    pub chunks_read: u64,
    pub bytes_decoded: u64,
}

/// Reads of one stage.
#[derive(Debug, Clone, Default)]
pub struct StageStats {
    pub leaves: Vec<LeafStats>,
    pub tiles: usize,
    pub chunks_read: u64,
    pub bytes_decoded: u64,
}

fn elem_size(t: DType) -> u64 {
    if t == DType::F32 { 4 } else { 8 }
}

/// Chunks fetched and bytes decoded by every stage of `plan`.
pub fn read_stats(plan: &Plan) -> Vec<StageStats> {
    plan.stages.iter().map(stage_stats).collect()
}

/// Decoded bytes read from real sources (files, stores, URLs), not from the in-memory
/// intermediates of a multi-stage chain: what `--max-read` limits.
pub fn source_bytes(plan: &Plan, stats: &[StageStats]) -> u64 {
    let ims: Vec<usize> = plan.intermediates.iter().map(|i| i.src).collect();
    stats
        .iter()
        .zip(&plan.stages)
        .flat_map(|(st, stage)| {
            st.leaves
                .iter()
                .filter(|l| !ims.contains(&stage.vars[l.var].leaves[l.leaf].leaf.src))
                .map(|l| l.bytes_decoded)
        })
        .sum()
}

/// Chunks of one stored dimension (chunk length `c`, length `n`) that the output indices `r`
/// read through `map`: their number and summed extent. Same chunk sets as `LeafRead::new`.
fn dim_read(map: &IndexMap, r: Range<usize>, c: usize, n: usize) -> (u64, u64) {
    let ext = |ci: usize| c.min(n.saturating_sub(ci * c)) as u64;
    match map {
        IndexMap::Const { idx, .. } => (1, ext(idx / c)),
        IndexMap::Range { start, .. } => {
            let (lo, hi) = ((start + r.start) / c, (start + r.end - 1) / c);
            ((hi - lo + 1) as u64, (lo..=hi).map(ext).sum())
        }
        IndexMap::List(_) => {
            let mut ids: Vec<usize> = r.map(|i| map.get(i) / c).collect();
            ids.sort_unstable();
            ids.dedup();
            (ids.len() as u64, ids.into_iter().map(ext).sum())
        }
    }
}

/// Chunks fetched and elements decoded for one leaf over all tiles of its variable. Tiles are
/// the cartesian product of per-dimension segments, so the sums factor by output dimension:
/// `prod_d sum_{segment s of d} prod_{stored dims k following d} count_k(s)` (an output
/// dimension the leaf does not follow contributes its number of segments).
fn leaf_reads(li: &LeafInfo, tiling: &VarTiling) -> (u64, u64) {
    let (mut chunks, mut elems) = (1u64, 1u64);
    for (d, segs) in tiling.segments.iter().enumerate() {
        let (mut cs, mut es) = (0u64, 0u64);
        for s in segs {
            let (mut c1, mut e1) = (1u64, 1u64);
            for (k, (od, map)) in li.leaf.maps.iter().enumerate() {
                if *od == d {
                    let (c, e) = dim_read(
                        map,
                        s.clone(),
                        li.grid.chunk_shape[k].max(1),
                        li.grid.shape[k],
                    );
                    c1 *= c;
                    e1 *= e;
                }
            }
            cs += c1;
            es += e1;
        }
        chunks *= cs;
        elems *= es;
    }
    (chunks, elems)
}

/// Reads of one stage. In lane waves (`plan::schedule`) a tile whose lanes lie in several waves
/// is read once per wave; its reads are scaled by the variable's tiles dispatched over all waves.
pub(crate) fn stage_stats(stage: &Stage) -> StageStats {
    let mut st = StageStats::default();
    for (vi, sv) in stage.vars.iter().enumerate() {
        let ntiles = sv.tiling.num_tiles();
        let reads = stage
            .sched
            .vars
            .get(vi)
            .map_or(ntiles as u64, |v| v.tile_reads);
        let scale = |x: u64| (x as u128 * reads as u128 / ntiles.max(1) as u128) as u64;
        st.tiles += reads as usize;
        for (k, li) in sv.leaves.iter().enumerate() {
            let esize = elem_size(leaf_dtype(li.src.as_ref(), &li.leaf.var));
            let (chunks, elems) = leaf_reads(li, &sv.tiling);
            let (chunks, elems) = (scale(chunks), scale(elems));
            let bytes = elems * esize;
            st.chunks_read += chunks;
            st.bytes_decoded += bytes;
            st.leaves.push(LeafStats {
                var: vi,
                leaf: k,
                chunks_read: chunks,
                bytes_decoded: bytes,
            });
        }
    }
    st
}

/// The effective `--max-read` limit and where it comes from.
#[derive(Debug, Clone, Copy)]
pub struct ReadLimit {
    pub limit: Option<u64>,
    pub source: &'static str,
}

/// Whether this process runs inside a Slurm job.
pub fn in_slurm_job() -> bool {
    std::env::var_os("SLURM_JOB_ID").is_some()
}

/// `--max-read`, else 64 GB on login nodes and no limit inside Slurm jobs.
pub fn read_limit(cmd: &Command) -> ReadLimit {
    match cmd.options.max_read {
        Some(u64::MAX) => ReadLimit {
            limit: None,
            source: "--max-read none",
        },
        Some(b) => ReadLimit {
            limit: Some(b),
            source: "--max-read",
        },
        None if in_slurm_job() => ReadLimit {
            limit: None,
            source: "no default limit inside a Slurm job",
        },
        None => ReadLimit {
            limit: Some(LOGIN_NODE_MAX_READ),
            source: "login-node default, no SLURM_JOB_ID",
        },
    }
}

/// The `read_limit` error if `bytes` exceeds the limit.
pub fn check_read_limit(bytes: u64, lim: ReadLimit) -> Result<()> {
    let Some(limit) = lim.limit.filter(|&l| bytes > l) else {
        return Ok(());
    };
    let mut hint = String::from(
        "narrow with seldate/selyear/sellonlatbox/selname or raise --max-read (e.g. --max-read 2T; \
         --max-read none for no limit); check what will be read with --plan",
    );
    if !in_slurm_job() {
        hint.push_str(
            "; heavy runs belong on a compute node (srun or sbatch), where there is no default \
             limit",
        );
    }
    Err(Error::new(
        ErrorCode::ReadLimit,
        format!(
            "the run would read {} (decoded), more than the limit of {} ({})",
            fmt_bytes(bytes),
            fmt_bytes(limit),
            lim.source
        ),
    )
    .with("bytes", bytes)
    .with("limit", limit)
    .with("limit_source", lim.source)
    .with_hint(hint))
}

/// Human-readable size with decimal units (1 GB = 10^9 bytes).
pub fn fmt_bytes(b: u64) -> String {
    const UNITS: [&str; 6] = ["B", "kB", "MB", "GB", "TB", "PB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1000.0 && u + 1 < UNITS.len() {
        v /= 1000.0;
        u += 1;
    }
    if u == 0 {
        format!("{b} B")
    } else if v >= 100.0 {
        format!("{v:.0} {}", UNITS[u])
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// `selname,tas`: an operator with its arguments, as on the command line.
fn op_token(n: &OpNode) -> String {
    if n.args.is_empty() {
        n.name.clone()
    } else {
        format!("{},{}", n.name, n.args.join(","))
    }
}

/// Whether the result of `n` is a pending fold, which its consumer reads back as an
/// intermediate (`plan::intermediate::materialize`): it ends a stage.
fn ends_stage(n: &OpNode) -> bool {
    ops::lookup(&n.name)
        .is_some_and(|s| matches!(s.class, AccessClass::Reduction | AccessClass::WholeExtent))
}

/// Operators of one stage in execution order, and the operators of inputs that an operator of
/// the stage ignores (the min/max inputs of `timpctl,p in -timmin in -timmax in`).
#[derive(Default, Clone)]
struct StageOps {
    ops: Vec<String>,
    ignored: Vec<String>,
}

/// All operator tokens of a subtree, inputs first.
fn subtree_ops(n: &OpNode, out: &mut Vec<String>) {
    for i in &n.inputs {
        if let Input::Op(o) = i {
            subtree_ops(o, out);
        }
    }
    out.push(op_token(n));
}

/// Operator tokens (`selname,tas`) of the tree, split into stages in the order the planner
/// makes them: an operator whose result is a fold ends the stage of its subtree, which becomes
/// an intermediate when its consumer is described (after all inputs of that consumer). Each
/// operator is listed once, in the stage that runs it; operators of ignored inputs are listed
/// separately. Falls back to splitting in tree order when this does not give `nstages` stages.
fn stage_operators(root: &OpNode, nstages: usize) -> Vec<StageOps> {
    fn walk(n: &OpNode, cur: &mut StageOps, stages: &mut Vec<StageOps>) {
        let used = ops::used_inputs(n).min(n.inputs.len());
        let mut pending = Vec::new();
        for i in &n.inputs[..used] {
            if let Input::Op(o) = i {
                if ends_stage(o) {
                    let mut st = StageOps::default();
                    walk(o, &mut st, stages);
                    pending.push(st);
                } else {
                    walk(o, cur, stages);
                }
            }
        }
        stages.extend(pending);
        for i in &n.inputs[used..] {
            if let Input::Op(o) = i {
                subtree_ops(o, &mut cur.ignored);
            }
        }
        cur.ops.push(op_token(n));
    }
    let nstages = nstages.max(1);
    let mut stages = Vec::new();
    let mut last = StageOps::default();
    walk(root, &mut last, &mut stages);
    stages.push(last);
    if stages.len() == nstages {
        return stages;
    }
    // fallback: tree order, a new stage after each fold
    fn flat(n: &OpNode, out: &mut Vec<(String, bool)>) {
        for i in &n.inputs {
            if let Input::Op(o) = i {
                flat(o, out);
            }
        }
        out.push((op_token(n), ends_stage(n)));
    }
    let mut toks = Vec::new();
    flat(root, &mut toks);
    let mut out = vec![StageOps::default()];
    for (tok, ends) in toks {
        out.last_mut().expect("one stage").ops.push(tok);
        if ends && out.len() < nstages {
            out.push(StageOps::default());
        }
    }
    out.resize(nstages, StageOps::default());
    out
}

/// `--plan --json`.
pub fn to_json(plan: &Plan, cmd: &Command, threads: usize, io_threads: usize) -> Value {
    let stats = read_stats(plan);
    let ops_per_stage = stage_operators(&cmd.root, plan.stages.len());

    let mut stages = Vec::new();
    let mut weights = Vec::new();
    let mut warnings = Vec::new();
    let mut total_comp: Option<u64> = Some(0);
    let (mut budget, mut g_stored, mut g_dec) = (SAMPLE_TOTAL, 0u64, 0u64);
    for (si, (stage, st)) in plan.stages.iter().zip(&stats).enumerate() {
        let mut comp_stage: Option<u64> = Some(0);
        let mut sampled = 0usize;
        let mut vars = Vec::new();
        for (vi, sv) in stage.vars.iter().enumerate() {
            let ntiles = sv.tiling.num_tiles();
            let mut leaves = Vec::new();
            for ls in st.leaves.iter().filter(|l| l.var == vi) {
                let li = &sv.leaves[ls.leaf];
                let esize = elem_size(leaf_dtype(li.src.as_ref(), &li.leaf.var));
                // compressed bytes: stored sizes of the first chunk of SAMPLE tiles spread
                // evenly over the stage, scaled by decoded bytes
                let (mut s_stored, mut s_dec, mut known) = (0u64, 0u64, true);
                let mut n_sample = 0;
                let k = SAMPLE.min(ntiles).min(budget);
                if ls.chunks_read > 0 && k > 0 {
                    for j in 0..k {
                        let t = j * ntiles / k;
                        let lr = LeafRead::new(li, &sv.tiling.tile(t));
                        let Some(idx) = lr.chunks().into_iter().next() else {
                            continue;
                        };
                        match li.src.stored_size(&li.leaf.var, &idx) {
                            Some(s) => {
                                s_stored += s;
                                s_dec +=
                                    li.grid.extent(&idx).iter().product::<usize>() as u64 * esize;
                                n_sample += 1;
                            }
                            None => {
                                known = false;
                                break;
                            }
                        }
                    }
                }
                budget -= n_sample;
                if k == 0 && g_dec > 0 {
                    (s_stored, s_dec) = (g_stored, g_dec);
                }
                let comp = if ls.chunks_read == 0 {
                    Some(0)
                } else if known && s_dec > 0 {
                    Some((ls.bytes_decoded as f64 * s_stored as f64 / s_dec as f64).round() as u64)
                } else {
                    None
                };
                if k > 0 {
                    g_stored += s_stored;
                    g_dec += s_dec;
                }
                sampled += n_sample;
                comp_stage = comp_stage.zip(comp).map(|(a, b)| a + b);
                leaves.push(json!({
                    "source": li.src.dataset().source,
                    "variable": li.leaf.var,
                    "codecs": li.src.codecs(&li.leaf.var),
                    "chunk_shape": li.grid.chunk_shape,
                    "chunks_total": li.grid.num_chunks(),
                    "chunks_read": ls.chunks_read,
                    "bytes_decoded": ls.bytes_decoded,
                    "bytes_compressed": comp,
                }));
            }
            vars.push(json!({
                "name": sv.name,
                "dims": sv.dims.iter().map(|d| d.name.clone()).collect::<Vec<_>>(),
                "shape": sv.dims.iter().map(|d| d.size).collect::<Vec<_>>(),
                "expr": sv.expr.describe(&plan.sources),
                "tiles": ntiles,
                "leaves": leaves,
            }));
        }
        total_comp = total_comp.zip(comp_stage).map(|(a, b)| a + b);
        let info = stage.kernel.as_ref().and_then(|k| k.plan_info());
        if let Some(w) = info
            .as_ref()
            .and_then(|i| i.get("remap_weights"))
            .and_then(Value::as_array)
        {
            weights.extend(w.iter().cloned());
        }
        if let Some(w) = info
            .as_ref()
            .and_then(|i| i.get("warnings"))
            .and_then(Value::as_array)
        {
            warnings.extend(w.iter().cloned());
        }
        let fold_dim = stage.kernel.as_ref().map(|k| k.fold_dim());
        let ops = &ops_per_stage[si].ops;
        let ignored = &ops_per_stage[si].ignored;
        let sch = &stage.sched;
        let intermediate = plan.intermediates.get(si).map(|im| {
            json!({
                "name": im.name,
                "bytes": im.bytes,
                "chunk_shapes": im.lay.iter().map(|o| o.chunks.clone()).collect::<Vec<_>>(),
            })
        });
        stages.push(json!({
            "index": si,
            "kind": if stage.kernel.is_some() { "fold" } else { "map" },
            "operators": ops,
            "ignored_operators": ignored,
            "fold_kernel": stage.kernel.as_ref().and_then(|_| ops.last().map(|o| o.split(',').next().unwrap_or(o).to_owned())),
            "fold_dim": fold_dim.map(|d| match d {
                DimRole::Time => "time",
                DimRole::Horizontal => "horizontal",
                DimRole::Vertical => "vertical",
                _ => "other",
            }),
            "passes": sch.passes,
            "lanes": sch.lanes,
            "waves": sch.waves,
            "lane_major": sch.waves > 1,
            "tiles_in_flight": sch.window,
            "tile_bytes": sch.tile_bytes,
            "lane_state_bytes": sch.state_bytes,
            "peak_bytes_estimate": sch.peak_bytes,
            "writes_intermediate": intermediate,
            "tiles": st.tiles,
            "chunks_read": st.chunks_read,
            "bytes_decoded": st.bytes_decoded,
            "bytes_compressed": comp_stage,
            "bytes_compressed_is_estimate": comp_stage.is_some(),
            "compressed_sample_chunks": sampled,
            "kernel": info,
            "variables": vars,
        }));
    }
    let weights_bytes: u64 = weights
        .iter()
        .filter(|w| w["cached"].as_bool() == Some(true))
        .filter_map(|w| w["path"].as_str())
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    // the stage with the largest estimate (with the intermediates alive while it runs) decides
    // the peak
    let top_i = plan.peak_stage();
    let top = top_i
        .map(|i| plan.stages[i].sched.clone())
        .unwrap_or_default();
    let tiles_mem = top.window as u64 * top.tile_bytes;
    let inter_mem = top_i.map_or(0, |i| plan.live_intermediate_bytes(i));
    let peak = plan.peak_bytes() + weights_bytes;
    let window = plan.stages.last().map_or(2, |s| s.sched.window);
    let lim = read_limit(cmd);
    let total_dec: u64 = stats.iter().map(|s| s.bytes_decoded).sum();
    let src_dec = source_bytes(plan, &stats);
    // one entry per file or store read (the files of a glob pattern or `mergetime` each), at
    // most MAX_LISTED_INPUTS of them; `inputs_count` counts all
    let all_inputs: Vec<String> = plan
        .sources
        .iter()
        .flat_map(|s| {
            s.member_paths()
                .unwrap_or_else(|| vec![s.dataset().source.clone()])
        })
        .collect();
    let inputs_count = all_inputs.len();
    let inputs: Vec<String> = all_inputs.into_iter().take(MAX_LISTED_INPUTS).collect();
    json!({
        "cdors_plan": SCHEMA,
        // null when no output file was given (`--plan` needs none)
        "output": (!plan.output.is_empty()).then_some(&plan.output),
        "format": format!("{:?}", plan.out_kind).to_ascii_lowercase(),
        // as `-z` names it (chunks are byte-shuffled first); null: uncompressed
        "compression": plan.compression.map(|c| c.to_string()),
        "inputs": inputs,
        "inputs_count": inputs_count,
        "inputs_truncated": inputs_count > inputs.len(),
        "stages": stages,
        "totals": {
            "stages": plan.stages.len(),
            "passes": plan.passes(),
            "tiles": stats.iter().map(|s| s.tiles).sum::<usize>(),
            "chunks_read": stats.iter().map(|s| s.chunks_read).sum::<u64>(),
            "bytes_decoded": total_dec,
            "bytes_compressed": total_comp,
            "bytes_compressed_is_estimate": total_comp.is_some(),
        },
        "memory": {
            "peak_bytes_estimate": peak,
            "budget_bytes": plan.budget,
            "budget_source": if cmd.options.mem.is_some() { "--mem" } else { "default" },
            "tiles_in_flight": top.window,
            "tile_bytes_max": top.tile_bytes,
            "parts": {
                "tiles_in_flight": tiles_mem,
                "fold_state": top.state_bytes,
                "output_buffers": top.out_hold + top.wave_hold,
                "intermediates": inter_mem,
                "remap_weights": weights_bytes,
            },
        },
        "remap_weights": weights,
        "settings": {
            "threads": threads,
            "io_threads": io_threads,
            "tiles_in_flight": window,
            "slurm_job": in_slurm_job(),
        },
        "read_limit": {
            "limit": lim.limit,
            "source": lim.source,
            "bytes": src_dec,
            "exceeded": lim.limit.is_some_and(|l| src_dec > l),
        },
        "warnings": warnings,
    })
}

/// Plain-text rendering of [`to_json`] for humans.
pub fn to_text(p: &Value) -> String {
    use std::fmt::Write;
    let b = |v: &Value| v.as_u64().map_or_else(|| "unknown".to_owned(), fmt_bytes);
    let mut s = String::new();
    let inputs: Vec<&str> = p["inputs"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let count = p["inputs_count"]
        .as_u64()
        .map_or(inputs.len(), |n| n as usize);
    if count > 6 && inputs.len() > 3 {
        let _ = writeln!(
            s,
            "inputs ({count}): {}, ..., {}",
            inputs[..3].join(", "),
            if count == inputs.len() {
                inputs[inputs.len() - 1]
            } else {
                "..."
            }
        );
    } else {
        let _ = writeln!(s, "inputs: {}", inputs.join(", "));
    }
    let out = p["output"].as_str().unwrap_or("");
    let comp = match p["compression"].as_str() {
        Some(c) => format!(", {c} with shuffle"),
        None => String::new(),
    };
    let _ = writeln!(
        s,
        "output: {} ({}{comp})",
        if out.is_empty() { "(none given)" } else { out },
        p["format"].as_str().unwrap_or("")
    );
    let nst = p["stages"].as_array().map_or(0, Vec::len);
    for st in p["stages"].as_array().into_iter().flatten() {
        let ops: Vec<&str> = st["operators"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let kernel = match st["fold_kernel"].as_str() {
            Some(k) => format!(
                ", folded by {k} along {}",
                st["fold_dim"].as_str().unwrap_or("?")
            ),
            None => String::new(),
        };
        let waves = match st["waves"].as_u64() {
            Some(w) if w > 1 => format!(", {} lanes in {w} waves (lane-major)", st["lanes"]),
            _ => String::new(),
        };
        let inter = match st["writes_intermediate"]["bytes"].as_u64() {
            Some(n) => format!(", result kept in memory ({})", fmt_bytes(n)),
            None => String::new(),
        };
        let ignored: Vec<&str> = st["ignored_operators"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let ignored = if ignored.is_empty() {
            String::new()
        } else {
            format!(
                "; ignored (min/max inputs of {}, not read): {}",
                st["fold_kernel"]
                    .as_str()
                    .unwrap_or("the percentile operator"),
                ignored.join(", ")
            )
        };
        let _ = writeln!(
            s,
            "stage {} of {nst}: {}{kernel}, {} pass(es){waves}{inter}{ignored}",
            st["index"].as_u64().unwrap_or(0) + 1,
            ops.join(" -> "),
            st["passes"]
        );
        let comp = match st["bytes_compressed"].as_u64() {
            Some(c) => format!(
                ", ~{} stored (estimate from {} chunks)",
                fmt_bytes(c),
                st["compressed_sample_chunks"]
            ),
            None => ", stored size unknown".to_owned(),
        };
        let _ = writeln!(
            s,
            "  read: {} chunks, {} decoded{comp}; {} tiles",
            st["chunks_read"],
            b(&st["bytes_decoded"]),
            st["tiles"]
        );
        // area weights of fld* statistics, variables with the same source on one line
        let mut by_source: Vec<(&str, Vec<&str>)> = Vec::new();
        for w in st["kernel"]["area_weights"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let (d, v) = (
                w["description"].as_str().unwrap_or(""),
                w["variable"].as_str().unwrap_or(""),
            );
            match by_source.iter_mut().find(|(k, _)| *k == d) {
                Some((_, vs)) => vs.push(v),
                None => by_source.push((d, vec![v])),
            }
        }
        for (d, vs) in by_source {
            let _ = writeln!(s, "  area weights of {}: {d}", vs.join(", "));
        }
        for v in st["variables"].as_array().into_iter().flatten() {
            for l in v["leaves"].as_array().into_iter().flatten() {
                let _ = writeln!(
                    s,
                    "    {} <- {}:{}  {} chunk reads ({} chunks stored, chunk shape {}), {}",
                    v["name"].as_str().unwrap_or(""),
                    l["source"].as_str().unwrap_or(""),
                    l["variable"].as_str().unwrap_or(""),
                    l["chunks_read"],
                    l["chunks_total"],
                    l["chunk_shape"],
                    b(&l["bytes_decoded"])
                );
            }
        }
    }
    for w in p["remap_weights"].as_array().into_iter().flatten() {
        let what = if w["weights"].as_str() == Some("given") {
            "weights file given".to_owned()
        } else if w["cached"].as_bool() == Some(true) {
            format!("cached ({})", w["path"].as_str().unwrap_or(""))
        } else {
            format!(
                "not cached{}: `{}` will generate them on first use",
                if w["path"].is_null() {
                    " (CDORS_CACHE is not set)"
                } else {
                    ""
                },
                w["generator"].as_str().unwrap_or("cdo")
            )
        };
        let _ = writeln!(
            s,
            "remap weights: {} to {}: {what}",
            w["operator"].as_str().unwrap_or(""),
            w["target"].as_str().unwrap_or("?")
        );
    }
    let m = &p["memory"];
    let _ = writeln!(
        s,
        "memory: ~{} peak of {} budget (estimate: {} tiles in flight of up to {}, lane states {}, intermediates {}, output buffers {})",
        b(&m["peak_bytes_estimate"]),
        b(&m["budget_bytes"]),
        m["tiles_in_flight"],
        b(&m["tile_bytes_max"]),
        b(&m["parts"]["fold_state"]),
        b(&m["parts"]["intermediates"]),
        b(&m["parts"]["output_buffers"])
    );
    let t = &p["totals"];
    let _ = writeln!(
        s,
        "total: {} chunks, {} decoded, {} pass(es)",
        t["chunks_read"],
        b(&t["bytes_decoded"]),
        t["passes"]
    );
    let st = &p["settings"];
    let _ = writeln!(
        s,
        "settings: {} compute threads, {} reads in flight{}",
        st["threads"],
        st["io_threads"],
        if st["slurm_job"].as_bool() == Some(true) {
            " (Slurm job)"
        } else {
            " (login node)"
        }
    );
    let r = &p["read_limit"];
    let _ = writeln!(
        s,
        "read limit: {} ({}){}",
        b(&r["limit"]).replace("unknown", "none"),
        r["source"].as_str().unwrap_or(""),
        if r["exceeded"].as_bool() == Some(true) {
            " -- EXCEEDED: the run would be refused"
        } else {
            ""
        }
    );
    for w in p["warnings"].as_array().into_iter().flatten() {
        let _ = writeln!(s, "WARNING: {}", w["message"].as_str().unwrap_or(""));
    }
    s
}
