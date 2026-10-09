//! Memory budget, finer tiles, lane waves and passes.
//!
//! **Budget** (`--mem`, default [`default_mem`]) bounds, per stage, the sum of
//! - the **tile window**: tiles in flight (reads, decoded chunks, gathered values) — at most half
//!   the budget, at least two tiles;
//! - the **lane states** of all live lanes ([`super::stage::FoldKernel::state_bytes`]);
//! - the **intermediates** (in-memory results of inner stages, `plan::intermediate`) alive while
//!   the stage runs (each is freed when the stage reading it has finished), and the output an
//!   intermediate-producing stage assembles.
//!
//! **Finer tiles.** A fold stage's tiles are chunk-aligned, so data stored one complete field per
//! chunk gives a time fold a single lane. The lanes are therefore cut finer: the non-folded
//! dimension with the longest segments is split into pieces (a divisor of the segment length, so
//! pieces line up with the chunk grid), until there are about two lanes per compute thread (for
//! short-lived lanes, such as those of space folds, which are open only while their tiles are
//! read: in every tile), or until one lane's state fits its share of the budget. A decoded tile is sliced into its pieces,
//! each pushed into its own lane; every cell still sees its values in fold order, so the result
//! is bit-identical to the unsplit run.
//!
//! **Waves and passes.** When the states of all lanes that live at once do not fit, the lanes
//! (C order over the pieces) are grouped into consecutive **waves** whose states fit, and the
//! stage is dispatched lane-major: all tiles of the lanes of one wave (in fold order), then the
//! next wave. On data chunked in space, waves read disjoint chunks; when one chunk holds the
//! cells of several waves (one field per chunk), it is read once per wave: the number of
//! **passes** over the input is the largest number of waves reading one chunk. In wave mode the
//! output chunks are aligned to the lanes and lie inside one wave, so every output chunk is
//! completed by the wave that writes it and written at once (in any order); they span as many
//! lanes as the output's usual chunks allow ([`out_chunks_for`]). If not even one lane of one cell
//! fits, planning fails with `memory_limit`.

use super::stage::Stage;
use super::tiling::{LeafRead, TileBox, VarTiling};
use crate::error::{Error, ErrorCode, Result};
use crate::model::{DType, DimRole};
use serde_json::{Value, json};
use std::ops::Range;

/// Default memory budget: 60 % of the Slurm allocation (`SLURM_MEM_PER_NODE`, or
/// `SLURM_MEM_PER_CPU` x `SLURM_CPUS_ON_NODE`), otherwise (login nodes, shared) a quarter of
/// `MemAvailable`, at most 4 GB; 2 GB when neither is known.
pub fn default_mem() -> u64 {
    const MB: u64 = 1 << 20;
    let env = |k: &str| {
        std::env::var(k)
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
    };
    if std::env::var_os("SLURM_JOB_ID").is_some() {
        let alloc = env("SLURM_MEM_PER_NODE")
            .or_else(|| Some(env("SLURM_MEM_PER_CPU")? * env("SLURM_CPUS_ON_NODE").unwrap_or(1)));
        if let Some(mb) = alloc.filter(|&m| m > 0) {
            return mb * MB / 10 * 6;
        }
    }
    let avail = std::fs::read_to_string("/proc/meminfo").ok().and_then(|s| {
        s.lines()
            .find(|l| l.starts_with("MemAvailable:"))
            .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
    });
    match avail {
        Some(kb) => (kb * 1024 / 4).min(4 << 30),
        None => 2_000_000_000,
    }
}

/// Lanes, waves and output alignment of one variable of a fold stage.
#[derive(Debug, Clone)]
pub struct VarSched {
    /// Folded dimensions.
    pub fold: Vec<bool>,
    /// Lane pieces per dimension (folded dimensions: one piece, the whole extent).
    pub pieces: Vec<Vec<Range<usize>>>,
    /// Per dimension and tile segment: the pieces inside the segment.
    pub seg_pieces: Vec<Vec<Range<usize>>>,
    /// C-order strides of the lane index over the non-folded dimensions.
    pub stride: Vec<usize>,
    /// Waves: consecutive ranges of lane indices.
    pub waves: Vec<Range<usize>>,
    /// Number of tiles of one lane (product of the folded dimensions' segment counts).
    pub lane_tiles: usize,
    /// Output chunks aligned to the lanes (wave mode), if the lanes allow it.
    pub out_chunks: Option<Vec<usize>>,
    /// Tiles dispatched over all waves (a tile whose lanes lie in several waves is read once
    /// per wave); the number of tiles without waves.
    pub tile_reads: u64,
}

impl VarSched {
    pub fn num_lanes(&self) -> usize {
        self.waves.last().map_or(0, |w| w.end)
    }

    /// Lanes of tile `seg` (segment index per dimension): lane index and lane box restricted to
    /// the tile (folded dimensions: the tile's range).
    pub fn tile_lanes(&self, tiling: &VarTiling, seg: &[usize]) -> Vec<(usize, TileBox)> {
        let nd = seg.len();
        let ranges: Vec<Range<usize>> = (0..nd)
            .map(|d| {
                if self.fold[d] {
                    0..1
                } else {
                    self.seg_pieces[d][seg[d]].clone()
                }
            })
            .collect();
        let mut out = Vec::new();
        let mut idx: Vec<usize> = ranges.iter().map(|r| r.start).collect();
        if ranges.iter().any(|r| r.is_empty()) {
            return out;
        }
        loop {
            let mut lane = 0;
            let mut bx = Vec::with_capacity(nd);
            for d in 0..nd {
                if self.fold[d] {
                    bx.push(tiling.segments[d][seg[d]].clone());
                } else {
                    lane += idx[d] * self.stride[d];
                    bx.push(self.pieces[d][idx[d]].clone());
                }
            }
            out.push((lane, TileBox { ranges: bx }));
            let mut d = nd;
            loop {
                if d == 0 {
                    return out;
                }
                d -= 1;
                idx[d] += 1;
                if idx[d] < ranges[d].end {
                    break;
                }
                idx[d] = ranges[d].start;
            }
        }
    }

    /// Wave of lane `lane`.
    pub fn wave_of(&self, lane: usize) -> usize {
        self.waves.partition_point(|w| w.end <= lane)
    }

    /// Box of lane `lane` with the folded dimensions spanning the whole extent.
    pub fn lane_box(&self, mut lane: usize) -> TileBox {
        let ranges = (0..self.pieces.len())
            .map(|d| {
                if self.fold[d] {
                    self.pieces[d][0].clone()
                } else {
                    let i = lane / self.stride[d];
                    lane %= self.stride[d];
                    self.pieces[d][i].clone()
                }
            })
            .collect();
        TileBox { ranges }
    }
}

/// How a stage runs within the budget.
#[derive(Debug, Clone, Default)]
pub struct Schedule {
    /// Fold stages: one entry per stage variable (empty for map stages).
    pub vars: Vec<VarSched>,
    /// Tiles in flight.
    pub window: usize,
    /// Estimated bytes of one tile in flight (decoded chunks, gathered and computed values).
    pub tile_bytes: u64,
    pub lanes: usize,
    /// Largest number of waves of one variable (1: no lane-major dispatch).
    pub waves: usize,
    /// Largest number of times one chunk is read.
    pub passes: usize,
    /// Decoded bytes read, over all passes (set by the planner from `explain::read_stats`).
    pub bytes_read: u64,
    /// Largest state of the lanes live at once.
    pub state_bytes: u64,
    /// Output held while being assembled (stages that produce an intermediate).
    pub out_hold: u64,
    /// Written output of one wave, held until all lanes of the wave have finished (lane waves:
    /// output chunks span many lanes, [`out_chunks_for`]).
    pub wave_hold: u64,
    /// Budget of the stage (the run's budget minus the intermediates alive while it runs).
    pub budget: u64,
    /// Estimated peak: window + states + held output (+ intermediates, added by the plan).
    pub peak_bytes: u64,
}

impl Schedule {
    /// Whether output chunks may be written in any order (lane-major dispatch).
    pub fn any_order(&self) -> bool {
        self.waves > 1
    }

    pub fn to_json(&self) -> Value {
        json!({
            "window": self.window,
            "tile_bytes": self.tile_bytes,
            "lanes": self.lanes,
            "waves": self.waves,
            "lane_major": self.waves > 1,
            "passes": self.passes,
            "bytes_read": self.bytes_read,
            "state_bytes": self.state_bytes,
            "wave_hold": self.wave_hold,
            "budget": self.budget,
            "peak_bytes": self.peak_bytes,
        })
    }
}

fn esize(t: DType) -> u64 {
    if t == DType::F32 { 4 } else { 8 }
}

/// Decoded bytes the tile reads.
fn read_bytes(stage_var: &super::stage::StageVar, bx: &TileBox) -> u64 {
    stage_var
        .leaves
        .iter()
        .map(|li| {
            let lr = LeafRead::new(li, bx);
            let csize: usize = li.grid.chunk_shape.iter().product();
            lr.num_chunks() as u64
                * csize as u64
                * esize(super::leaf_dtype(li.src.as_ref(), &li.leaf.var))
        })
        .sum()
}

/// Smallest divisor of `n` that is at least `k` (`n` if none is smaller).
fn divisor_at_least(n: usize, k: usize) -> usize {
    (k.max(1)..=n.max(1))
        .find(|q| n.is_multiple_of(*q))
        .unwrap_or(n.max(1))
}

fn too_small(what: String, need: u64, budget: u64) -> Error {
    Error::new(ErrorCode::MemoryLimit, what)
        .with("bytes", need)
        .with("limit", budget)
        .with_hint(format!(
            "raise --mem to at least {}, or select fewer cells, levels or timesteps first \
             (-sellonlatbox, -sellevel, -seltimestep)",
            human(need)
        ))
}

/// Bytes as `12.3M`.
pub fn human(b: u64) -> String {
    let b = b as f64;
    for (u, s) in [(1e12, "T"), (1e9, "G"), (1e6, "M"), (1e3, "K")] {
        if b >= u {
            return format!("{:.1}{s}", b / u);
        }
    }
    format!("{b}")
}

/// Plans the tile window, lanes, waves and passes of `stage` within `budget` bytes, of which
/// `out_hold` are taken by output being assembled. `written`: per stage variable, the bytes of
/// its output if the stage writes the output file (in the written type), else empty; in lane
/// waves, a wave's share of it is held until the wave's lanes have finished.
pub fn schedule(
    stage: &Stage,
    threads: usize,
    io_threads: usize,
    budget: u64,
    out_hold: u64,
    written: &[u64],
) -> Result<Schedule> {
    let threads = threads.max(1);
    // largest tile in flight: decoded chunks + the computed tile (f64 at most)
    const SAMPLE: usize = 4096;
    let mut tile_bytes = 1u64;
    for sv in &stage.vars {
        let n = sv.tiling.num_tiles();
        let step = n.div_ceil(SAMPLE).max(1);
        for t in (0..n).step_by(step) {
            let bx = sv.tiling.tile(t);
            let rb = read_bytes(sv, &bx);
            tile_bytes = tile_bytes.max(rb + 8 * bx.len() as u64);
        }
    }
    let window = ((budget / 2) / tile_bytes).clamp(2, (io_threads + 2 * threads).max(2) as u64);
    let window_bytes = window * tile_bytes;
    let mut s = Schedule {
        window: window as usize,
        tile_bytes,
        budget,
        out_hold,
        passes: 1,
        waves: 1,
        ..Schedule::default()
    };
    let Some(kernel) = &stage.kernel else {
        s.peak_bytes = window_bytes + out_hold;
        return Ok(s);
    };
    let state_budget = budget.saturating_sub(window_bytes + out_hold);
    if state_budget == 0 {
        return Err(too_small(
            format!(
                "the memory budget ({}) does not hold two tiles in flight ({} each){}",
                human(budget),
                human(tile_bytes),
                if out_hold > 0 {
                    format!(" and the intermediate output ({})", human(out_hold))
                } else {
                    String::new()
                }
            ),
            2 * (window_bytes + out_hold),
            budget,
        ));
    }
    let fold_role = kernel.fold_dim();
    // largest wave state of long-lived lanes; largest lane state and largest total state of
    // short-lived lanes
    let (mut long_state, mut short_lane, mut short_all) = (0u64, 0u64, 0u64);
    let mut wave_hold = 0u64;
    for (vi, sv) in stage.vars.iter().enumerate() {
        let nd = sv.dims.len();
        let segs = &sv.tiling.segments;
        let fold: Vec<bool> = sv.dims.iter().map(|d| d.role == fold_role).collect();
        let full = TileBox {
            ranges: sv.dims.iter().map(|d| 0..d.size).collect(),
        };
        let total_state = kernel.state_bytes(sv.var, &full) as u64;
        let fold_extent: usize = (0..nd)
            .filter(|&d| fold[d])
            .map(|d| sv.dims[d].size)
            .product();
        let cells = (full.len() / fold_extent.max(1)).max(1) as u64;
        let per_cell = total_state.div_ceil(cells).max(1);
        let lanes_now: usize = (0..nd)
            .filter(|&d| !fold[d])
            .map(|d| segs[d].len())
            .product();
        // split dimension: the non-folded dimension with the longest segments
        let maxlen = |d: usize| segs[d].iter().map(|r| r.len()).max().unwrap_or(0);
        let sd = (0..nd)
            .filter(|&d| !fold[d] && maxlen(d) > 1)
            .max_by_key(|&d| (maxlen(d), usize::MAX - d));
        // lanes live at once: all of them when the first dimension that is cut is a folded one
        // (time folds: every lane stays open until the last time chunk)
        let lane_tiles: usize = (0..nd)
            .filter(|&d| fold[d])
            .map(|d| segs[d].len())
            .product();
        let first_cut = (0..nd).find(|&d| segs[d].len() > 1 || Some(d) == sd);
        let long_lived = lane_tiles > 1 && first_cut.is_some_and(|d| fold[d]);
        let mut k = 1usize;
        if let Some(sd) = sd {
            let seg_len = maxlen(sd);
            let other: usize = (0..nd)
                .filter(|&d| !fold[d] && d != sd)
                .map(|d| segs[d].iter().map(|r| r.len()).max().unwrap_or(1))
                .product();
            let seg_state = per_cell * (seg_len * other) as u64;
            if lanes_now < 2 * threads {
                k = (2 * threads).div_ceil(lanes_now.max(1));
            }
            if !long_lived {
                // short-lived lanes (space folds: the tiles of one time segment, cells
                // fastest) are open a few at a time, so every tile is cut into enough lanes
                // to keep the compute threads busy
                k = k.max(2 * threads);
            }
            let live_state = if long_lived {
                total_state
            } else {
                seg_state * window
            };
            if live_state > state_budget {
                // one lane should fit about 1/threads of the state budget
                let share = (state_budget / threads as u64).max(1);
                k = k.max(seg_state.div_ceil(share) as usize);
            }
            k = divisor_at_least(seg_len, k.min(seg_len));
        }
        // pieces
        let mut pieces: Vec<Vec<Range<usize>>> = Vec::with_capacity(nd);
        let mut seg_pieces: Vec<Vec<Range<usize>>> = Vec::with_capacity(nd);
        for d in 0..nd {
            if fold[d] {
                pieces.push(std::iter::once(0..sv.dims[d].size).collect());
                seg_pieces.push(vec![0..1; segs[d].len()]);
                continue;
            }
            let len = if Some(d) == sd {
                maxlen(d).div_ceil(k).max(1)
            } else {
                usize::MAX
            };
            let mut p = Vec::new();
            let mut sp = Vec::with_capacity(segs[d].len());
            for r in &segs[d] {
                let p0 = p.len();
                let mut a = r.start;
                while a < r.end {
                    let b = r.end.min(a.saturating_add(len));
                    p.push(a..b);
                    a = b;
                }
                sp.push(p0..p.len());
            }
            pieces.push(p);
            seg_pieces.push(sp);
        }
        let mut stride = vec![0usize; nd];
        let mut nlanes = 1usize;
        for d in (0..nd).rev() {
            if !fold[d] {
                stride[d] = nlanes;
                nlanes *= pieces[d].len();
            }
        }
        let mut vs = VarSched {
            fold: fold.clone(),
            pieces,
            seg_pieces,
            stride,
            waves: std::iter::once(0..nlanes).collect(),
            lane_tiles,
            out_chunks: None,
            tile_reads: 0,
        };
        // lane states and waves
        let lane_state: Vec<u64> = (0..nlanes)
            .map(|l| kernel.state_bytes(sv.var, &vs.lane_box(l)) as u64)
            .collect();
        if let Some((l, &big)) = lane_state.iter().enumerate().max_by_key(|(_, b)| **b)
            && big > state_budget
        {
            let bx = vs.lane_box(l);
            return Err(too_small(
                format!(
                    "the running state of one lane ({} cells) needs {}, more than the {} the \
                     memory budget leaves for it",
                    bx.len() / fold_extent.max(1),
                    human(big),
                    human(state_budget)
                ),
                big + window_bytes + out_hold,
                budget,
            ));
        }
        // short-lived lanes (one tile each, or folded dimension slowest) are bounded by the
        // window; long-lived lanes are all open together unless dispatched in waves
        let all: u64 = lane_state.iter().sum();
        let live = if long_lived {
            all
        } else {
            lane_state.iter().max().copied().unwrap_or(0) * window
        };
        if long_lived && live > state_budget {
            // a wave's share of the written output is held until all lanes of the wave have
            // finished (its chunks span many lanes), so it counts with the lane states
            let out = u128::from(written.get(vi).copied().unwrap_or(0));
            let lane_out: Vec<u64> = (0..nlanes)
                .map(|l| {
                    let lane_cells = (vs.lane_box(l).len() / fold_extent.max(1)) as u128;
                    (out * lane_cells / u128::from(cells)) as u64
                })
                .collect();
            let mut waves = Vec::new();
            let (mut a, mut acc) = (0usize, 0u64);
            for (l, &b) in lane_state.iter().enumerate() {
                let b = b + lane_out[l];
                if acc + b > state_budget && l > a {
                    waves.push(a..l);
                    a = l;
                    acc = 0;
                }
                acc += b;
            }
            waves.push(a..nlanes);
            vs.waves = waves;
            vs.out_chunks = Some(lane_chunks(sv, &vs));
            let held = vs.waves.iter().map(|w| lane_out[w.clone()].iter().sum::<u64>());
            wave_hold = wave_hold.max(held.max().unwrap_or(0));
        }
        let wave_state = vs
            .waves
            .iter()
            .map(|w| lane_state[w.clone()].iter().sum::<u64>())
            .max()
            .unwrap_or(0);
        s.state_bytes = s.state_bytes.max(if long_lived {
            wave_state
        } else {
            live.min(wave_state)
        });
        if long_lived {
            long_state = long_state.max(wave_state);
        } else {
            let lane_max = lane_state.iter().max().copied().unwrap_or(0);
            short_lane = short_lane.max(lane_max);
            short_all = short_all.max(wave_state);
        }
        // reads: every tile is read once per wave that has lanes in it
        let ntiles = sv.tiling.num_tiles();
        vs.tile_reads = ntiles as u64;
        if vs.waves.len() > 1 {
            vs.tile_reads = 0;
            for t in 0..ntiles {
                let seg = sv.tiling.segment_indices(t);
                let mut ws: Vec<usize> = vs
                    .tile_lanes(&sv.tiling, &seg)
                    .iter()
                    .map(|(l, _)| vs.wave_of(*l))
                    .collect();
                ws.dedup();
                s.passes = s.passes.max(ws.len());
                vs.tile_reads += ws.len() as u64;
            }
        }
        s.lanes += nlanes;
        s.waves = s.waves.max(vs.waves.len());
        s.vars.push(vs);
    }
    // Short-lived lanes are open about one per tile in flight, so their states grow with the
    // window: when the estimate exceeds the budget, fewer tiles are kept in flight (at least
    // two); if even that does not fit, planning fails before anything is read.
    s.wave_hold = wave_hold;
    let peak_at = |w: u64| {
        w * tile_bytes + long_state.max((short_lane * w).min(short_all)) + out_hold + wave_hold
    };
    if peak_at(window) > budget {
        let Some(w) = (2..window).rev().find(|&w| peak_at(w) <= budget) else {
            return Err(too_small(
                format!(
                    "the memory budget ({}) does not hold two tiles in flight ({} each) with \
                     the running states of their lanes ({} each){}",
                    human(budget),
                    human(tile_bytes),
                    human(short_lane),
                    if out_hold > 0 {
                        format!(" and the intermediate output ({})", human(out_hold))
                    } else {
                        String::new()
                    }
                ),
                peak_at(2),
                budget,
            ));
        };
        s.window = w as usize;
        s.state_bytes = long_state.max((short_lane * w).min(short_all));
        s.peak_bytes = peak_at(w);
        return Ok(s);
    }
    s.peak_bytes = window_bytes + s.state_bytes + out_hold + wave_hold;
    Ok(s)
}

/// Output chunks of a stage variable aligned to its lanes: along a non-folded dimension whose
/// pieces all have one length `s` (the last may be shorter) and start at multiples of `s`, the
/// chunk is `s`; along the folded dimensions 1; elsewhere the whole extent. Expressed in the
/// input variable's dimensions; [`out_chunks_for`] widens them and maps them to the output
/// variable.
fn lane_chunks(sv: &super::stage::StageVar, vs: &VarSched) -> Vec<usize> {
    (0..sv.dims.len())
        .map(|d| {
            if vs.fold[d] {
                return 1;
            }
            let p = &vs.pieces[d];
            let s = p.first().map_or(1, |r| r.len()).max(1);
            let aligned = p
                .iter()
                .enumerate()
                .all(|(i, r)| r.start % s == 0 && (r.len() == s || i + 1 == p.len()));
            if aligned { s } else { sv.dims[d].size.max(1) }
        })
        .collect()
}

/// Output chunks for output variable `out` of a fold stage in wave mode. Along an input dimension
/// with the same name and size: a multiple of the lane-aligned chunk, as large as `target` (the
/// output's chunks without waves, per output dimension) allows while every chunk stays inside
/// one wave; 1 along time; otherwise the whole extent. Chunks of one lane each would scatter one
/// field over many small chunks across the file (W4 `ydaymean`: 384 chunks of 32 KB per day).
pub fn out_chunks_for(
    sv: &super::stage::StageVar,
    vs: &VarSched,
    out: &[crate::model::VarDim],
    target: &[usize],
) -> Option<Vec<usize>> {
    let lc = vs.out_chunks.as_ref()?;
    let map: Vec<Option<usize>> = out
        .iter()
        .map(|od| {
            sv.dims
                .iter()
                .position(|d| d.name == od.name && d.size == od.size)
                .filter(|&d| !vs.fold[d])
        })
        .collect();
    let mut want = lc.clone();
    for (i, m) in map.iter().enumerate() {
        if let Some(d) = *m {
            want[d] = target.get(i).copied().unwrap_or(lc[d]);
        }
    }
    let sizes: Vec<usize> = sv.dims.iter().map(|d| d.size).collect();
    let chunk = widen_in_waves(&sizes, vs, lc, &want);
    Some(
        out.iter()
            .zip(&map)
            .map(|(od, m)| match m {
                Some(d) => chunk[*d],
                None if od.role == DimRole::Time => 1,
                None => od.size.max(1),
            })
            .collect(),
    )
}

/// Widens the lane-aligned chunks `unit` (input dimensions) towards `want`, last dimension first:
/// along each non-folded dimension the largest multiple of the unit, at most `want` (but at
/// least the unit), with which every chunk still lies inside one wave.
fn widen_in_waves(sizes: &[usize], vs: &VarSched, unit: &[usize], want: &[usize]) -> Vec<usize> {
    let mut chunk = unit.to_vec();
    for d in (0..sizes.len()).rev() {
        if vs.fold[d] {
            continue;
        }
        let (u, size) = (unit[d].max(1), sizes[d].max(1));
        let max_m = (want[d] / u).clamp(1, size.div_ceil(u));
        for m in (2..=max_m).rev() {
            let mut c = chunk.clone();
            c[d] = (m * u).min(size);
            if chunks_within_waves(sizes, vs, &c) {
                chunk = c;
                break;
            }
        }
    }
    chunk
}

/// Whether every chunk of shape `chunk` (input dimensions; folded dimensions ignored) lies inside
/// one wave: waves are consecutive lane ranges, so it is enough that a chunk's first and last
/// lane (C order over the pieces it touches) are in the same wave.
fn chunks_within_waves(sizes: &[usize], vs: &VarSched, chunk: &[usize]) -> bool {
    let dims: Vec<usize> = (0..sizes.len()).filter(|&d| !vs.fold[d]).collect();
    // per non-folded dimension: the first and last piece touched by each of its chunks
    let spans: Vec<Vec<(usize, usize)>> = dims
        .iter()
        .map(|&d| {
            let (p, c) = (&vs.pieces[d], chunk[d].max(1));
            (0..sizes[d].div_ceil(c))
                .map(|k| {
                    let (lo, hi) = (k * c, ((k + 1) * c).min(sizes[d]));
                    let first = p.partition_point(|r| r.end <= lo);
                    let last = p.partition_point(|r| r.start < hi).saturating_sub(1);
                    (first, last)
                })
                .collect()
        })
        .collect();
    if spans.iter().any(Vec::is_empty) {
        return true;
    }
    let mut idx = vec![0usize; dims.len()];
    loop {
        let (mut first, mut last) = (0usize, 0usize);
        for (j, &d) in dims.iter().enumerate() {
            let (f, l) = spans[j][idx[j]];
            first += f * vs.stride[d];
            last += l * vs.stride[d];
        }
        if vs.wave_of(first) != vs.wave_of(last) {
            return false;
        }
        let mut j = dims.len();
        loop {
            if j == 0 {
                return true;
            }
            j -= 1;
            idx[j] += 1;
            if idx[j] < spans[j].len() {
                break;
            }
            idx[j] = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lanes over the non-folded dimensions (pieces of `piece` cells), dimension 0 folded.
    fn sched(sizes: &[usize], piece: &[usize], waves: &[Range<usize>]) -> VarSched {
        let nd = sizes.len();
        let pieces: Vec<Vec<Range<usize>>> = (0..nd)
            .map(|d| {
                if d == 0 {
                    return std::iter::once(0..sizes[0]).collect();
                }
                (0..sizes[d].div_ceil(piece[d]))
                    .map(|k| k * piece[d]..((k + 1) * piece[d]).min(sizes[d]))
                    .collect()
            })
            .collect();
        let mut stride = vec![0; nd];
        let mut s = 1;
        for d in (1..nd).rev() {
            stride[d] = s;
            s *= pieces[d].len();
        }
        VarSched {
            fold: (0..nd).map(|d| d == 0).collect(),
            pieces,
            seg_pieces: Vec::new(),
            stride,
            waves: waves.to_vec(),
            lane_tiles: 1,
            out_chunks: None,
            tile_reads: 0,
        }
    }

    #[test]
    fn output_chunks_span_lanes_within_waves() {
        // W4 ydaymean in small: 48 lanes of 8 cells in two waves -> one chunk per wave
        let vs = sched(&[100, 384], &[1, 8], &[0..24, 24..48]);
        assert_eq!(widen_in_waves(&[100, 384], &vs, &[1, 8], &[1, 384]), vec![1, 192]);
        // a smaller usual chunk: the largest multiple of the lane that divides the waves
        assert_eq!(widen_in_waves(&[100, 384], &vs, &[1, 8], &[1, 100]), vec![1, 96]);
        // W4 timpctl in small: waves of 5 lanes, the last one shorter
        let vs = sched(&[9, 96], &[1, 4], &[0..5, 5..10, 10..15, 15..20, 20..24]);
        assert_eq!(widen_in_waves(&[9, 96], &vs, &[1, 4], &[1, 96]), vec![1, 20]);
        // two horizontal dimensions: rows of lanes, two rows per wave
        let vs = sched(&[10, 8, 18], &[1, 2, 3], &[0..12, 12..24]);
        assert_eq!(widen_in_waves(&[10, 8, 18], &vs, &[1, 2, 3], &[1, 8, 18]), vec![1, 4, 18]);
        // a wave boundary inside a row: x stays split where the waves cut it
        let vs = sched(&[10, 8, 18], &[1, 2, 3], &[0..9, 9..24]);
        assert!(!chunks_within_waves(&[10, 8, 18], &vs, &[1, 2, 18]));
        assert_eq!(widen_in_waves(&[10, 8, 18], &vs, &[1, 2, 3], &[1, 8, 18]), vec![1, 2, 9]);
    }
}
