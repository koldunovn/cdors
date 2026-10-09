//! The streaming pipeline: fetch -> decode -> compute -> assemble -> write.
//!
//! - **dispatch** (calling thread): walks the tiles of every stage variable in canonical order
//!   (variable, then C order of the tile grid). A tile is dispatched only after it acquires a
//!   permit from the tile window, so at most `window` tiles are in flight (back-pressure; memory
//!   stays bounded).
//! - **fetch** (`io_threads` dedicated threads): blocking `read_chunk` calls (pread for Zarr,
//!   serialised netCDF-C for the fallback). The number of reads in flight is its own knob,
//!   separate from `-P`: on Lustre, I/O concurrency rather than decode threads sets throughput.
//! - **decode + compute** (rayon pool of `-P` threads): every fetched chunk is decoded in its own
//!   task; the task that decodes a tile's last chunk gathers the tile and evaluates the
//!   expression.
//! - **assemble** (the thread that finished a tile, usually a compute thread): copies the tile
//!   into output-chunk buffers (each under its own lock; NaN becomes the writer's missing value
//!   on the way) and releases the tile's permit; a chunk filled completely goes to the writer.
//! - **write** (one writer thread): hands complete output chunks to the writer — in canonical
//!   order for NetCDF (one netCDF-C call at a time anyway), as parallel encode tasks on the
//!   rayon pool for Zarr.

use super::{OutVar, Writer};
use crate::error::{Error, Result};
use crate::io::{DecodedChunk, RawChunk, Values};
use crate::model::DType;
use crate::plan::stage::{FoldState, Stage, StageVar, Tile, eval};
use crate::plan::tiling::{LeafRead, TileBox};
use crossbeam_channel::{Receiver, Sender, unbounded};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

static T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Phase timing on stderr when `CDORS_TRACE` is set (development aid).
pub fn trace(what: &str) {
    if std::env::var_os("CDORS_TRACE").is_some() {
        let t0 = T0.get_or_init(std::time::Instant::now);
        eprintln!("cdors trace {:8.3}s {what}", t0.elapsed().as_secs_f64());
    }
}

/// A counting semaphore.
struct Semaphore {
    n: Mutex<usize>,
    cv: Condvar,
}

impl Semaphore {
    fn new(n: usize) -> Self {
        Self {
            n: Mutex::new(n),
            cv: Condvar::new(),
        }
    }

    fn acquire(&self) {
        let mut n = self.n.lock().expect("semaphore");
        while *n == 0 {
            n = self.cv.wait(n).expect("semaphore");
        }
        *n -= 1;
    }

    fn release(&self) {
        *self.n.lock().expect("semaphore") += 1;
        self.cv.notify_one();
    }

    /// Opens the semaphore for good (no `acquire` blocks any more): the writer is gone and
    /// dispatch must not wait for permits it would release.
    fn close(&self) {
        *self
            .n
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = usize::MAX / 2;
        self.cv.notify_all();
    }

    /// Waits until all `total` permits are back.
    fn wait_all(&self, total: usize) {
        let mut n = self.n.lock().expect("semaphore");
        while *n < total {
            n = self.cv.wait(n).expect("semaphore");
        }
    }
}

/// Pipeline settings.
#[derive(Debug, Clone, Copy)]
pub struct Settings {
    /// Compute threads (`-P`).
    pub threads: usize,
    /// Blocking reads in flight.
    pub io_threads: usize,
    /// Tiles in flight.
    pub window: usize,
    /// Output chunks may be written in any order (lane-major dispatch: each output chunk is
    /// completed by one lane, and the lanes of one wave finish before the next wave starts).
    pub any_order: bool,
}

struct Shared {
    error: Mutex<Option<Error>>,
    cancel: AtomicBool,
    window: Semaphore,
    /// Started after the threads that use them, before dispatch (see [`run`]). Without a
    /// compute pool chunks are decoded on the I/O threads; without a write pool, Zarr chunks
    /// are written by the writer thread.
    compute: OnceLock<rayon::ThreadPool>,
    write: OnceLock<rayon::ThreadPool>,
    asm: Assembler,
}

impl Shared {
    fn fail(&self, e: Error) {
        let mut g = self
            .error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if g.is_none() {
            *g = Some(e);
        }
        self.cancel.store(true, Ordering::SeqCst);
    }
}

/// One tile in flight.
struct TileJob {
    sv: Arc<StageVar>,
    bx: TileBox,
    reads: Vec<LeafRead>,
    /// Decoded chunks per leaf, in the order of `LeafRead::chunks`.
    chunks: Mutex<Vec<Vec<Option<DecodedChunk>>>>,
    remaining: AtomicUsize,
    error: Mutex<Option<Error>>,
    /// Parts of the computed tile and where they go: map stages one part, the whole tile, to the
    /// writer; fold stages one part per lane (finer tiles), each pushed into its lane.
    subs: Vec<Sub>,
    /// Parts not yet consumed by their lanes; the tile's window permit is released at zero.
    pending_subs: Arc<AtomicUsize>,
}

/// A part of a tile: its box and, for fold stages, its lane and position in the lane.
struct Sub {
    bx: TileBox,
    lane: Option<(Arc<Lane>, usize)>,
}

/// A lane of a fold stage: tiles that differ only along the folded dimension(s), pushed into
/// one running state strictly in fold order.
///
/// The lock is held only to queue a tile or to take the next one: the thread that finds the
/// lane idle becomes its drainer and pushes every tile that is next in fold order (the state
/// moved out of the lock), including tiles queued by other threads meanwhile. No thread ever
/// waits for a push of another.
struct Lane {
    inner: Mutex<LaneInner>,
}

struct LaneInner {
    next: usize,
    total: usize,
    /// Tiles waiting for their predecessors, with the counter of their dispatched tile.
    pending: BTreeMap<usize, (LaneTile, Arc<AtomicUsize>)>,
    /// The running state; `None` while a drainer pushes into it, and after the lane finished.
    state: Option<Box<dyn FoldState>>,
    /// A thread is draining the lane.
    busy: bool,
}

/// A tile of a lane: a whole computed tile, or a part of one (finer lanes), shared by the
/// parts' lanes and cut out only if the lane's state cannot fold a contiguous part in place.
enum LaneTile {
    Whole(Tile),
    Part(Arc<Tile>, TileBox),
}

/// Positions of box `bx` in the values of the enclosing box `outer` (C order), if they are
/// contiguous: dimensions after the innermost one that `bx` cuts are whole, and the ones before
/// it have length 1 in `bx`.
fn contiguous(outer: &TileBox, bx: &TileBox) -> Option<std::ops::Range<usize>> {
    let nd = bx.ranges.len();
    let shape = outer.shape();
    let mut inner = nd;
    while inner > 0 && bx.ranges[inner - 1] == outer.ranges[inner - 1] {
        inner -= 1;
    }
    if inner == 0 {
        return Some(0..outer.len());
    }
    let k = inner - 1;
    if (0..k).any(|d| bx.ranges[d].len() != 1) {
        return None;
    }
    let stride: usize = shape[k + 1..].iter().product();
    let mut off = 0;
    for ((n, r), o) in shape.iter().zip(&bx.ranges).zip(&outer.ranges).take(k + 1) {
        off = off * n + (r.start - o.start);
    }
    let start = off * stride;
    Some(start..start + bx.len())
}

/// Pushes a lane tile into a lane state.
fn push_lane_tile(state: &mut dyn FoldState, t: LaneTile) -> Result<Vec<Tile>> {
    match t {
        LaneTile::Whole(t) => state.push(t),
        LaneTile::Part(tile, bx) => {
            if let Some(r) = contiguous(&tile.bx, &bx)
                && let Some(out) = state.push_slice(&bx, &tile.values, r)
            {
                return out;
            }
            let part = slice(&tile, &bx);
            drop(tile);
            state.push(part)
        }
    }
}

struct Fetch {
    tile: Arc<TileJob>,
    leaf: usize,
    slot: usize,
    idx: Vec<u64>,
}

enum Msg {
    /// A tile for the writer to assemble (failed tiles). `input`: it also completes one
    /// dispatched tile, whose window permit the writer releases.
    Out { tile: Result<Tile>, input: bool },
    /// A complete output chunk (variable, linear chunk index).
    Chunk {
        var: usize,
        lin: usize,
        buf: ChunkBuf,
    },
    /// A dispatched tile was consumed by a fold kernel (its permit is already released).
    Consumed,
    /// Dispatch finished after this many tiles.
    Done(usize),
}

fn compute(job: &TileJob) -> Result<Tile> {
    let shape = job.bx.shape();
    let chunks = std::mem::take(&mut *job.chunks.lock().expect("chunk lock"));
    let mut vals = Vec::with_capacity(chunks.len());
    for (lr, cs) in job.reads.iter().zip(chunks) {
        let cs: Vec<DecodedChunk> = cs
            .into_iter()
            .map(|c| c.ok_or_else(|| Error::internal("chunk missing")))
            .collect::<Result<_>>()?;
        vals.push(lr.gather(cs, &shape));
    }
    let values = eval(&job.sv.expr, &mut vals.into_iter());
    Ok(Tile {
        var: job.sv.var,
        bx: job.bx.clone(),
        values,
    })
}

/// The part `bx` of a computed tile.
fn slice(tile: &Tile, bx: &TileBox) -> Tile {
    let values = match &tile.values {
        Values::F32(v) => Values::F32(extract(v, &tile.bx, bx)),
        Values::F64(v) => Values::F64(extract(v, &tile.bx, bx)),
    };
    Tile {
        var: tile.var,
        bx: bx.clone(),
        values,
    }
}

/// The values of box `bx` (inside `src_bx`) of `src`, row by row (no prefill).
pub(crate) fn extract<T: Copy>(src: &[T], src_bx: &TileBox, bx: &TileBox) -> Vec<T> {
    let n = bx.len();
    let mut out = Vec::with_capacity(n);
    let nd = bx.ranges.len();
    if n == 0 || nd == 0 {
        out.extend_from_slice(&src[..n.min(src.len())]);
        return out;
    }
    let sshape = src_bx.shape();
    let mut st = vec![1usize; nd];
    for d in (0..nd - 1).rev() {
        st[d] = st[d + 1] * sshape[d + 1];
    }
    // trailing dimensions covered in full merge into one row
    let mut inner = nd - 1;
    while inner > 0 && bx.ranges[inner] == src_bx.ranges[inner] {
        inner -= 1;
    }
    let lo: Vec<usize> = (0..nd)
        .map(|d| bx.ranges[d].start - src_bx.ranges[d].start)
        .collect();
    let row = bx.ranges[inner].len() * st[inner];
    let mut idx = lo.clone();
    loop {
        let off: usize = (0..=inner).map(|d| idx[d] * st[d]).sum();
        out.extend_from_slice(&src[off..off + row]);
        let mut d = inner;
        loop {
            if d == 0 {
                return out;
            }
            d -= 1;
            idx[d] += 1;
            if idx[d] < lo[d] + bx.ranges[d].len() {
                break;
            }
            idx[d] = lo[d];
        }
    }
}

/// Called after one chunk of `job` finished (decoded or failed).
fn chunk_done(job: &Arc<TileJob>, tx: &Sender<Msg>, shared: &Arc<Shared>) {
    if job.remaining.fetch_sub(1, Ordering::AcqRel) != 1 {
        return;
    }
    let err = job.error.lock().expect("tile error").take();
    let r = match err {
        Some(e) => Err(e),
        None => compute(job),
    };
    let tile = match r {
        Ok(t) if job.subs.iter().all(|s| s.lane.is_some()) => t,
        Ok(t) => {
            // map stage: assembled here, then the tile is done
            deliver(t, tx, shared);
            shared.window.release();
            let _ = tx.send(Msg::Consumed);
            return;
        }
        r => {
            // a failed tile (counted once, its permit released by the writer)
            let _ = tx.send(Msg::Out {
                tile: r,
                input: true,
            });
            return;
        }
    };
    if let [sub] = job.subs.as_slice()
        && sub.bx == tile.bx
    {
        let (lane, seq) = sub.lane.as_ref().expect("fold part");
        lane_push(
            lane,
            *seq,
            LaneTile::Whole(tile),
            &job.pending_subs,
            tx,
            shared,
        );
        return;
    }
    // finer tiles: every part goes to its own lane, in parallel when there is a pool
    let tile = Arc::new(tile);
    for sub in &job.subs {
        let (lane, seq) = sub.lane.clone().expect("fold part");
        let (tx, sh, cnt) = (tx.clone(), shared.clone(), job.pending_subs.clone());
        let part = LaneTile::Part(tile.clone(), sub.bx.clone());
        let task = move || lane_push(&lane, seq, part, &cnt, &tx, &sh);
        match shared.compute.get() {
            Some(pool) if job.subs.len() > 1 => pool.spawn(task),
            _ => task(),
        }
    }
}

/// Copies a finished output tile into the output chunks (on the calling thread) and hands the
/// chunks it completed to the writer, before the tile counts as done.
fn deliver(tile: Tile, tx: &Sender<Msg>, shared: &Shared) {
    if shared.cancel.load(Ordering::Relaxed) {
        return;
    }
    for (var, lin, buf) in shared.asm.add(tile) {
        let _ = tx.send(Msg::Chunk { var, lin, buf });
    }
}

/// Adds a computed tile to its lane. If no other thread is draining the lane, pushes every tile
/// that is next in fold order into the lane's state (outside the lock) and finishes the lane
/// after its last tile; otherwise returns at once, the drainer pushes it. A dispatched tile's
/// window permit is released when all its parts have been pushed.
fn lane_push(
    lane: &Lane,
    seq: usize,
    tile: LaneTile,
    cnt: &Arc<AtomicUsize>,
    tx: &Sender<Msg>,
    shared: &Shared,
) {
    let mut g = lane.inner.lock().expect("lane lock");
    g.pending.insert(seq, (tile, cnt.clone()));
    if g.busy {
        return;
    }
    g.busy = true;
    loop {
        let n = g.next;
        let Some((t, c)) = g.pending.remove(&n) else {
            g.busy = false;
            return;
        };
        g.next += 1;
        let done = g.next == g.total;
        let mut state = g.state.take();
        drop(g);
        if let Some(st) = state.as_mut() {
            // the state lives outside the lock while it folds: a panicking kernel must not leave
            // the lane busy (its queued tiles would never release their permits)
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let outs = push_lane_tile(st.as_mut(), t);
                if done {
                    outs.and_then(|mut o| {
                        o.extend(st.finish()?);
                        Ok(o)
                    })
                } else {
                    outs
                }
            }));
            let panicked = r.is_err();
            let outs = r.unwrap_or_else(|_| Err(Error::internal("a fold kernel panicked")));
            if done || panicked {
                state = None;
            }
            match outs {
                Ok(o) => {
                    for t in o {
                        deliver(t, tx, shared);
                    }
                }
                Err(e) => shared.fail(e),
            }
        }
        if c.fetch_sub(1, Ordering::AcqRel) == 1 {
            shared.window.release();
            let _ = tx.send(Msg::Consumed);
        }
        g = lane.inner.lock().expect("lane lock");
        g.state = state;
    }
}

fn fetch_loop(rx: Receiver<Fetch>, tx: Sender<Msg>, shared: Arc<Shared>) {
    while let Ok(f) = rx.recv() {
        let fail = |e: Error| {
            let mut g = f.tile.error.lock().expect("tile error");
            if g.is_none() {
                *g = Some(e);
            }
        };
        if shared.cancel.load(Ordering::Relaxed) {
            fail(Error::internal("cancelled"));
            chunk_done(&f.tile, &tx, &shared);
            continue;
        }
        let li = &f.tile.sv.leaves[f.leaf];
        match li.src.read_chunk(&li.leaf.var, &f.idx) {
            Err(e) => {
                fail(e);
                chunk_done(&f.tile, &tx, &shared);
            }
            Ok(raw) => {
                super::progress::chunk_fetched(&raw);
                let (tx, sh) = (tx.clone(), shared.clone());
                let task = move || {
                    let r = match raw {
                        RawChunk::Decoded(d) => Ok(d),
                        RawChunk::Encoded(e) => e.decode(),
                    };
                    match r {
                        Ok(d) => {
                            f.tile.chunks.lock().expect("chunk lock")[f.leaf][f.slot] = Some(d);
                        }
                        Err(e) => {
                            let mut g = f.tile.error.lock().expect("tile error");
                            if g.is_none() {
                                *g = Some(e);
                            }
                        }
                    }
                    chunk_done(&f.tile, &tx, &sh);
                };
                match shared.compute.get() {
                    Some(pool) => pool.spawn(task),
                    None => task(),
                }
            }
        }
    }
}

/// An output chunk being filled.
struct ChunkBuf {
    origin: Vec<usize>,
    shape: Vec<usize>,
    data: Values,
    filled: usize,
}

/// An output chunk being filled by several tiles.
type OpenChunk = Arc<Mutex<ChunkBuf>>;

/// Collects tiles into output chunks. Tiles are copied in by the threads that finish them, each
/// chunk under its own lock; NaN becomes the writer's missing value on the way.
struct Assembler {
    vars: Vec<OutVar>,
    /// Per variable: what NaN becomes ([`Writer::missing_as`]).
    missing: Vec<Option<f64>>,
    open: Mutex<HashMap<(usize, usize), OpenChunk>>,
}

fn new_values(t: DType, n: usize) -> Values {
    match t {
        DType::F32 => Values::F32(vec![f32::NAN; n]),
        _ => Values::F64(vec![f64::NAN; n]),
    }
}

/// Replaces NaN by `mv` (in the values' type).
fn replace_nan(v: &mut Values, mv: f64) {
    match v {
        Values::F32(x) => {
            let m = mv as f32;
            for y in x.iter_mut() {
                *y = if y.is_nan() { m } else { *y };
            }
        }
        Values::F64(x) => {
            for y in x.iter_mut() {
                *y = if y.is_nan() { mv } else { *y };
            }
        }
    }
}

/// The values of box `bx` (inside the box `sbx` of `src`) in type `t`, NaN replaced by `mv`.
fn cut(src: &Values, sbx: &TileBox, bx: &TileBox, t: DType, mv: Option<f64>) -> Values {
    let v = match (src, t) {
        (Values::F32(s), DType::F32) => Values::F32(extract(s, sbx, bx)),
        (Values::F64(s), DType::F32) => {
            Values::F32(extract(s, sbx, bx).into_iter().map(|x| x as f32).collect())
        }
        (Values::F32(s), _) => {
            Values::F64(extract(s, sbx, bx).into_iter().map(f64::from).collect())
        }
        (Values::F64(s), _) => Values::F64(extract(s, sbx, bx)),
    };
    own(v, t, mv)
}

/// `v` in type `t`, NaN replaced by `mv`.
fn own(v: Values, t: DType, mv: Option<f64>) -> Values {
    let mut v = match (v, t) {
        (Values::F64(s), DType::F32) => Values::F32(s.into_iter().map(|x| x as f32).collect()),
        (Values::F32(s), t) if t != DType::F32 => {
            Values::F64(s.into_iter().map(f64::from).collect())
        }
        (v, _) => v,
    };
    if let Some(m) = mv {
        replace_nan(&mut v, m);
    }
    v
}

/// Copies the part of `src` (a box with `src_origin`, `src_shape`) that lies inside `dst`
/// (`dst_origin`, `dst_shape`), NaN replaced by `mv`; returns the number of elements copied.
fn copy_box(
    src: &Values,
    src_origin: &[usize],
    src_shape: &[usize],
    dst: &mut Values,
    dst_origin: &[usize],
    dst_shape: &[usize],
    mv: Option<f64>,
) -> usize {
    let nd = src_shape.len();
    let lo: Vec<usize> = (0..nd).map(|d| src_origin[d].max(dst_origin[d])).collect();
    let hi: Vec<usize> = (0..nd)
        .map(|d| (src_origin[d] + src_shape[d]).min(dst_origin[d] + dst_shape[d]))
        .collect();
    if (0..nd).any(|d| lo[d] >= hi[d]) {
        return 0;
    }
    let strides = |shape: &[usize]| {
        let mut s = vec![1usize; nd];
        for d in (0..nd - 1).rev() {
            s[d] = s[d + 1] * shape[d + 1];
        }
        s
    };
    let (ss, ds) = (strides(src_shape), strides(dst_shape));
    let row = hi[nd - 1] - lo[nd - 1];
    let mut idx = lo.clone();
    let mut n = 0;
    loop {
        let so: usize = (0..nd).map(|d| (idx[d] - src_origin[d]) * ss[d]).sum();
        let dof: usize = (0..nd).map(|d| (idx[d] - dst_origin[d]) * ds[d]).sum();
        match (src, &mut *dst) {
            (Values::F32(s), Values::F32(t)) => t[dof..dof + row].copy_from_slice(&s[so..so + row]),
            (Values::F64(s), Values::F64(t)) => t[dof..dof + row].copy_from_slice(&s[so..so + row]),
            (Values::F64(s), Values::F32(t)) => {
                for (a, b) in t[dof..dof + row].iter_mut().zip(&s[so..so + row]) {
                    *a = *b as f32;
                }
            }
            (Values::F32(s), Values::F64(t)) => {
                for (a, b) in t[dof..dof + row].iter_mut().zip(&s[so..so + row]) {
                    *a = f64::from(*b);
                }
            }
        }
        if let Some(m) = mv {
            match &mut *dst {
                Values::F32(t) => {
                    let m = m as f32;
                    for y in &mut t[dof..dof + row] {
                        *y = if y.is_nan() { m } else { *y };
                    }
                }
                Values::F64(t) => {
                    for y in &mut t[dof..dof + row] {
                        *y = if y.is_nan() { m } else { *y };
                    }
                }
            }
        }
        n += row;
        let mut d = nd - 1;
        loop {
            if d == 0 {
                return n;
            }
            d -= 1;
            idx[d] += 1;
            if idx[d] < hi[d] {
                break;
            }
            idx[d] = lo[d];
        }
    }
}

impl Assembler {
    /// Adds a tile; returns the output chunks it completed as (var, linear chunk, buffer). An
    /// output chunk that lies inside the tile is cut out of it directly (no buffer, no prefill;
    /// the tile's values themselves when the tile is the chunk).
    fn add(&self, tile: Tile) -> Vec<(usize, usize, ChunkBuf)> {
        let ov = &self.vars[tile.var];
        let mv = self.missing.get(tile.var).copied().flatten();
        let nd = ov.shape.len();
        let origin: Vec<usize> = tile.bx.ranges.iter().map(|r| r.start).collect();
        let shape = tile.bx.shape();
        if shape.contains(&0) {
            return Vec::new();
        }
        // chunk index ranges touched by the tile
        let c0: Vec<usize> = (0..nd).map(|d| origin[d] / ov.chunks[d]).collect();
        let c1: Vec<usize> = (0..nd)
            .map(|d| (origin[d] + shape[d] - 1) / ov.chunks[d])
            .collect();
        let counts = ov.chunk_counts();
        let mut done = Vec::new();
        let mut ci = c0.clone();
        let mut values = Some(tile.values);
        let single = c0 == c1;
        loop {
            let lin = ci.iter().zip(&counts).fold(0, |a, (&i, &n)| a * n + i);
            let key = (tile.var, lin);
            let o: Vec<usize> = (0..nd).map(|d| ci[d] * ov.chunks[d]).collect();
            let s: Vec<usize> = (0..nd)
                .map(|d| ov.chunks[d].min(ov.shape[d] - o[d]))
                .collect();
            let n: usize = s.iter().product();
            let inside = (0..nd).all(|d| origin[d] <= o[d] && o[d] + s[d] <= origin[d] + shape[d]);
            if inside {
                let cb = TileBox {
                    ranges: (0..nd).map(|d| o[d]..o[d] + s[d]).collect(),
                };
                let data = if single && cb == tile.bx {
                    own(values.take().expect("tile values"), ov.dtype, mv)
                } else {
                    let src = values.as_ref().expect("tile values");
                    cut(src, &tile.bx, &cb, ov.dtype, mv)
                };
                done.push((
                    tile.var,
                    lin,
                    ChunkBuf {
                        origin: o,
                        shape: s,
                        data,
                        filled: n,
                    },
                ));
            } else {
                let slot = self
                    .open
                    .lock()
                    .expect("assembler lock")
                    .entry(key)
                    .or_insert_with(|| {
                        Arc::new(Mutex::new(ChunkBuf {
                            data: new_values(ov.dtype, n),
                            origin: o,
                            shape: s,
                            filled: 0,
                        }))
                    })
                    .clone();
                let mut g = slot.lock().expect("chunk lock");
                let ChunkBuf {
                    origin: bo,
                    shape: bs,
                    data,
                    filled,
                } = &mut *g;
                let src = values.as_ref().expect("tile values");
                *filled += copy_box(src, &origin, &shape, data, bo, bs, mv);
                if *filled == n {
                    let buf = ChunkBuf {
                        origin: bo.clone(),
                        shape: bs.clone(),
                        data: std::mem::replace(data, Values::F32(Vec::new())),
                        filled: n,
                    };
                    drop(g);
                    self.open.lock().expect("assembler lock").remove(&key);
                    done.push((tile.var, lin, buf));
                }
            }
            let mut d = nd;
            loop {
                if d == 0 {
                    return done;
                }
                d -= 1;
                ci[d] += 1;
                if ci[d] <= c1[d] {
                    break;
                }
                ci[d] = c0[d];
            }
        }
    }
}

/// Chunk reads (at least one per tile) of a stage, counted up to `cap`: the number of tasks
/// that can run at once, which bounds the useful size of the thread pools.
pub fn count_reads(stage: &Stage, cap: usize) -> usize {
    let mut n = 0usize;
    for sv in &stage.vars {
        for t in 0..sv.tiling.num_tiles() {
            let bx = sv.tiling.tile(t);
            let reads: usize = sv
                .leaves
                .iter()
                .map(|l| LeafRead::new(l, &bx).num_chunks())
                .sum();
            n += reads.max(1);
            if n >= cap {
                return n;
            }
        }
    }
    n.max(1)
}

/// Runs one stage and writes its output through `writer`. Without a kernel, the stage's tiles
/// are written; with a kernel, they are pushed lane by lane, in fold order, into the kernel's
/// states, whose output tiles (described by `out`) are written.
pub fn run(stage: &Stage, out: &[OutVar], writer: Arc<dyn Writer>, s: Settings) -> Result<()> {
    trace("pipeline start");
    let shared = Arc::new(Shared {
        error: Mutex::new(None),
        cancel: AtomicBool::new(false),
        window: Semaphore::new(s.window),
        compute: OnceLock::new(),
        write: OnceLock::new(),
        asm: Assembler {
            vars: out.to_vec(),
            missing: (0..out.len()).map(|v| writer.missing_as(v)).collect(),
            open: Mutex::new(HashMap::new()),
        },
    });
    let (fetch_tx, fetch_rx) = unbounded::<Fetch>();
    let (msg_tx, msg_rx) = unbounded::<Msg>();
    let svs: Vec<Arc<StageVar>> = stage.vars.iter().cloned().map(Arc::new).collect();

    std::thread::scope(|scope| -> Result<()> {
        // Threads start in order of need, so that under a thread limit every role gets one
        // before any gets more: the writer and one I/O thread (required), the compute pool and
        // the Zarr write pool (halved while refused; without them their work runs on the I/O
        // and writer threads), then the other I/O threads (as many as allowed).
        let writer_handle = super::threads::spawn_scoped(scope, "cdors-writer".into(), || {
            let (rx, w, sh) = (msg_rx.clone(), writer.clone(), shared.clone());
            move || write_loop(rx, out, w, sh, s.threads, s.any_order)
        })?;
        drop(msg_rx);
        let io = |_| {
            let (rx, tx, sh) = (fetch_rx.clone(), msg_tx.clone(), shared.clone());
            move || fetch_loop(rx, tx, sh)
        };
        if let Err(e) = super::threads::spawn_scoped(scope, "cdors-io-0".into(), || io(0)) {
            // stop the writer and fail
            let _ = msg_tx.send(Msg::Done(0));
            drop(msg_tx);
            let _ = writer_handle.join();
            return Err(e);
        }
        let instead = "decoding on the I/O thread(s) instead";
        if let Some(pool) = super::threads::build_optional_pool("compute", s.threads, instead) {
            let _ = shared.compute.set(pool);
        }
        if !writer.ordered() {
            // Zarr chunks are encoded and stored (blocking file writes) on their own pool, so
            // that slow storage does not stall decoding
            let nchunks: usize = out
                .iter()
                .map(|v| v.chunk_counts().iter().product::<usize>())
                .sum();
            let instead = "writing chunks one by one instead";
            let n = s.threads.min(nchunks);
            if let Some(pool) = super::threads::build_optional_pool("write", n, instead) {
                let _ = shared.write.set(pool);
            }
        }
        super::threads::spawn_more(scope, "io", 1, s.io_threads, io);
        drop(fetch_rx);

        // dispatch: map stages tile by tile in canonical order; fold stages wave by wave (one
        // wave unless the lane states need lane-major dispatch), within a wave in canonical
        // order, every tile cut into the parts of its lanes
        let mut n = 0usize;
        let dispatch = |sv: &Arc<StageVar>, bx: TileBox, subs: Vec<Sub>, n: &mut usize| -> bool {
            shared.window.acquire();
            if shared.cancel.load(Ordering::SeqCst) {
                shared.window.release();
                return false;
            }
            let reads: Vec<LeafRead> = sv.leaves.iter().map(|l| LeafRead::new(l, &bx)).collect();
            let lists: Vec<Vec<Vec<u64>>> = reads.iter().map(LeafRead::chunks).collect();
            let total: usize = lists.iter().map(Vec::len).sum();
            let job = Arc::new(TileJob {
                sv: sv.clone(),
                bx,
                chunks: Mutex::new(lists.iter().map(|l| vec![None; l.len()]).collect()),
                reads,
                remaining: AtomicUsize::new(total),
                error: Mutex::new(None),
                pending_subs: Arc::new(AtomicUsize::new(subs.len())),
                subs,
            });
            *n += 1;
            for (leaf, l) in lists.into_iter().enumerate() {
                for (slot, idx) in l.into_iter().enumerate() {
                    let _ = fetch_tx.send(Fetch {
                        tile: job.clone(),
                        leaf,
                        slot,
                        idx,
                    });
                }
            }
            true
        };
        'outer: for (vi, sv) in svs.iter().enumerate() {
            let (Some(k), Some(vs)) = (&stage.kernel, stage.sched.vars.get(vi)) else {
                for t in 0..sv.tiling.num_tiles() {
                    let bx = sv.tiling.tile(t);
                    let subs = vec![Sub {
                        bx: bx.clone(),
                        lane: None,
                    }];
                    if !dispatch(sv, bx, subs, &mut n) {
                        break 'outer;
                    }
                }
                continue;
            };
            let mut lanes: HashMap<usize, Arc<Lane>> = HashMap::new();
            for wave in &vs.waves {
                for t in 0..sv.tiling.num_tiles() {
                    let seg = sv.tiling.segment_indices(t);
                    let parts: Vec<(usize, TileBox)> = vs
                        .tile_lanes(&sv.tiling, &seg)
                        .into_iter()
                        .filter(|(l, _)| wave.contains(l))
                        .collect();
                    if parts.is_empty() {
                        continue;
                    }
                    // position of the tile in its lanes: C order over the folded dimensions
                    let mut seq = 0usize;
                    for (d, &sg) in seg.iter().enumerate() {
                        if vs.fold[d] {
                            seq = seq * sv.tiling.segments[d].len() + sg;
                        }
                    }
                    let mut bx = parts[0].1.clone();
                    let subs: Vec<Sub> = parts
                        .into_iter()
                        .map(|(l, pb)| {
                            for (r, p) in bx.ranges.iter_mut().zip(&pb.ranges) {
                                *r = r.start.min(p.start)..r.end.max(p.end);
                            }
                            let lane = lanes
                                .entry(l)
                                .or_insert_with(|| {
                                    Arc::new(Lane {
                                        inner: Mutex::new(LaneInner {
                                            next: 0,
                                            total: vs.lane_tiles,
                                            pending: BTreeMap::new(),
                                            state: Some(k.start(sv.var, &vs.lane_box(l))),
                                            busy: false,
                                        }),
                                    })
                                })
                                .clone();
                            Sub {
                                bx: pb,
                                lane: Some((lane, seq)),
                            }
                        })
                        .collect();
                    if !dispatch(sv, bx, subs, &mut n) {
                        break 'outer;
                    }
                }
                // the lanes of this wave are complete once their tiles are pushed
                lanes.retain(|l, _| !wave.contains(l));
            }
        }
        trace(&format!("dispatched {n} tiles"));
        let _ = msg_tx.send(Msg::Done(n));
        drop(fetch_tx);
        drop(msg_tx);
        let r = writer_handle
            .join()
            .unwrap_or_else(|_| Err(Error::internal("writer thread panicked")));
        if let Err(e) = r {
            shared.fail(e);
        }
        Ok(())
    })?;
    match shared.error.lock().expect("error lock").take() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn write_loop(
    rx: Receiver<Msg>,
    out: &[OutVar],
    writer: Arc<dyn Writer>,
    shared: Arc<Shared>,
    threads: usize,
    any_order: bool,
) -> Result<()> {
    // if this thread dies, dispatch must not block on window permits it would have released
    struct Gone(Arc<Shared>);
    impl Drop for Gone {
        fn drop(&mut self) {
            if std::thread::panicking() {
                self.0.fail(Error::internal("writer thread panicked"));
                self.0.window.close();
            }
        }
    }
    let _gone = Gone(shared.clone());
    // decided at the first chunk, when the pools are up: without a write pool, chunks are
    // written in canonical order on this thread
    let mut ordered: Option<bool> = None;
    // canonical order of chunks: (var, linear chunk index)
    let mut pending: BTreeMap<(usize, usize), ChunkBuf> = BTreeMap::new();
    let mut next = (0usize, 0usize);
    let nchunks: Vec<usize> = out
        .iter()
        .map(|v| v.chunk_counts().iter().product())
        .collect();
    let encode_slots = Arc::new(Semaphore::new(2 * threads.max(1)));
    let mut received = 0usize;
    let mut expected: Option<usize> = None;
    let mut emit = |var: usize, lin: usize, buf: ChunkBuf| {
        let wpool = shared.write.get();
        if *ordered.get_or_insert(!any_order && (writer.ordered() || wpool.is_none())) {
            pending.insert((var, lin), buf);
            while let Some(b) = pending.remove(&next) {
                if !shared.cancel.load(Ordering::Relaxed)
                    && let Err(e) = writer.write_ready(next.0, &b.origin, &b.shape, b.data)
                {
                    shared.fail(e);
                }
                next.1 += 1;
                while next.0 < nchunks.len() && next.1 >= nchunks[next.0] {
                    next = (next.0 + 1, 0);
                }
            }
        } else if let Some(pool) = wpool {
            encode_slots.acquire();
            let (w, sh, slots) = (writer.clone(), shared.clone(), encode_slots.clone());
            pool.spawn(move || {
                if !sh.cancel.load(Ordering::Relaxed)
                    && let Err(e) = w.write_ready(var, &buf.origin, &buf.shape, buf.data)
                {
                    sh.fail(e);
                }
                slots.release();
            });
        } else if !shared.cancel.load(Ordering::Relaxed)
            && let Err(e) = writer.write_ready(var, &buf.origin, &buf.shape, buf.data)
        {
            // any order, no write pool (NetCDF): written at once on this thread
            shared.fail(e);
        }
    };
    while expected != Some(received) {
        let Ok(msg) = rx.recv() else { break };
        let (tile, input) = match msg {
            Msg::Done(n) => {
                expected = Some(n);
                continue;
            }
            Msg::Consumed => {
                received += 1;
                continue;
            }
            Msg::Chunk { var, lin, buf } => {
                emit(var, lin, buf);
                continue;
            }
            Msg::Out { tile, input } => {
                if input {
                    received += 1;
                }
                (tile, input)
            }
        };
        let release = || {
            if input {
                shared.window.release();
            }
        };
        let tile = match tile {
            Ok(t) if !shared.cancel.load(Ordering::Relaxed) => t,
            Ok(_) => {
                release();
                continue;
            }
            Err(e) => {
                shared.fail(e);
                release();
                continue;
            }
        };
        let done = shared.asm.add(tile);
        release();
        for (var, lin, buf) in done {
            emit(var, lin, buf);
        }
    }
    drop(emit);
    trace("all tiles assembled");
    encode_slots.wait_all(2 * threads.max(1));
    trace("all chunks written");
    let open = shared.asm.open.lock().expect("assembler lock").len();
    if !shared.cancel.load(Ordering::SeqCst) && (open > 0 || !pending.is_empty()) {
        return Err(Error::internal(format!(
            "{} output chunks were not completed",
            open + pending.len()
        )));
    }
    Ok(())
}
