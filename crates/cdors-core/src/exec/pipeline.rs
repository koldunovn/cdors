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
//! - **assemble + write** (one writer thread): copies finished tiles into output-chunk buffers
//!   and hands each complete output chunk to the writer — in canonical order for NetCDF (one
//!   netCDF-C call at a time anyway), as parallel encode tasks on the rayon pool for Zarr.
//!   Releasing a tile's permit happens here, after its values have been copied.

use super::{OutVar, Writer};
use crate::error::{Error, Result};
use crate::io::{DecodedChunk, RawChunk, Values};
use crate::model::DType;
use crate::plan::stage::{FoldState, Stage, StageVar, Tile, eval};
use crate::plan::tiling::{LeafRead, TileBox};
use crossbeam_channel::{Receiver, Sender, unbounded};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

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
}

struct Shared {
    error: Mutex<Option<Error>>,
    cancel: AtomicBool,
    window: Semaphore,
}

impl Shared {
    fn fail(&self, e: Error) {
        let mut g = self.error.lock().expect("error lock");
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
    /// Fold stages: the lane of the tile and its position along the folded dimension(s).
    lane: Option<(Arc<Lane>, usize)>,
}

/// A lane of a fold stage: tiles that differ only along the folded dimension(s), pushed into
/// one running state strictly in fold order.
struct Lane {
    inner: Mutex<LaneInner>,
}

struct LaneInner {
    next: usize,
    total: usize,
    pending: BTreeMap<usize, Tile>,
    state: Option<Box<dyn FoldState>>,
}

struct Fetch {
    tile: Arc<TileJob>,
    leaf: usize,
    slot: usize,
    idx: Vec<u64>,
}

enum Msg {
    /// A tile for the writer. `input`: it also completes one dispatched tile (map stages, or a
    /// failed tile), whose window permit the writer releases after copying it.
    Out { tile: Result<Tile>, input: bool },
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

/// Called after one chunk of `job` finished (decoded or failed).
fn chunk_done(job: &Arc<TileJob>, tx: &Sender<Msg>, shared: &Shared) {
    if job.remaining.fetch_sub(1, Ordering::AcqRel) != 1 {
        return;
    }
    let err = job.error.lock().expect("tile error").take();
    let r = match err {
        Some(e) => Err(e),
        None => compute(job),
    };
    match (&job.lane, r) {
        (Some((lane, seq)), Ok(tile)) => lane_push(lane, *seq, tile, tx, shared),
        (_, r) => {
            let _ = tx.send(Msg::Out {
                tile: r,
                input: true,
            });
        }
    }
}

/// Adds a computed tile to its lane and pushes every tile that is next in fold order into the
/// lane's state; finishes the lane after its last tile.
fn lane_push(lane: &Lane, seq: usize, tile: Tile, tx: &Sender<Msg>, shared: &Shared) {
    let mut g = lane.inner.lock().expect("lane lock");
    g.pending.insert(seq, tile);
    let mut consumed = 0;
    loop {
        let n = g.next;
        let Some(t) = g.pending.remove(&n) else { break };
        g.next += 1;
        consumed += 1;
        let done = g.next == g.total;
        let Some(state) = g.state.as_mut() else { break };
        let mut outs = state.push(t);
        if done {
            outs = outs.and_then(|mut o| {
                o.extend(state.finish()?);
                Ok(o)
            });
            g.state = None;
        }
        match outs {
            Ok(o) => {
                for t in o {
                    let _ = tx.send(Msg::Out {
                        tile: Ok(t),
                        input: false,
                    });
                }
            }
            Err(e) => shared.fail(e),
        }
    }
    drop(g);
    for _ in 0..consumed {
        shared.window.release();
        let _ = tx.send(Msg::Consumed);
    }
}

fn fetch_loop(
    rx: Receiver<Fetch>,
    tx: Sender<Msg>,
    pool: Arc<rayon::ThreadPool>,
    shared: Arc<Shared>,
) {
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
                let tx = tx.clone();
                let shared = shared.clone();
                pool.spawn(move || {
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
                    chunk_done(&f.tile, &tx, &shared);
                });
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

/// Collects tiles into output chunks.
struct Assembler {
    vars: Vec<OutVar>,
    open: HashMap<(usize, usize), ChunkBuf>,
}

fn new_values(t: DType, n: usize) -> Values {
    match t {
        DType::F32 => Values::F32(vec![f32::NAN; n]),
        _ => Values::F64(vec![f64::NAN; n]),
    }
}

/// Copies the part of `src` (a box with `src_origin`, `src_shape`) that lies inside `dst`
/// (`dst_origin`, `dst_shape`); returns the number of elements copied.
fn copy_box(
    src: &Values,
    src_origin: &[usize],
    src_shape: &[usize],
    dst: &mut Values,
    dst_origin: &[usize],
    dst_shape: &[usize],
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
    /// Adds a tile; returns the output chunks it completed as (var, linear chunk, buffer).
    fn add(&mut self, tile: Tile) -> Vec<(usize, usize, ChunkBuf)> {
        let ov = &self.vars[tile.var];
        let nd = ov.shape.len();
        let origin: Vec<usize> = tile.bx.ranges.iter().map(|r| r.start).collect();
        let shape = tile.bx.shape();
        // chunk index ranges touched by the tile
        let c0: Vec<usize> = (0..nd).map(|d| origin[d] / ov.chunks[d]).collect();
        let c1: Vec<usize> = (0..nd)
            .map(|d| (origin[d] + shape[d] - 1) / ov.chunks[d])
            .collect();
        let counts = ov.chunk_counts();
        let mut done = Vec::new();
        let mut ci = c0.clone();
        loop {
            let lin = ci.iter().zip(&counts).fold(0, |a, (&i, &n)| a * n + i);
            let key = (tile.var, lin);
            let buf = self.open.entry(key).or_insert_with(|| {
                let o: Vec<usize> = (0..nd).map(|d| ci[d] * ov.chunks[d]).collect();
                let s: Vec<usize> = (0..nd)
                    .map(|d| ov.chunks[d].min(ov.shape[d] - o[d]))
                    .collect();
                ChunkBuf {
                    data: new_values(ov.dtype, s.iter().product()),
                    origin: o,
                    shape: s,
                    filled: 0,
                }
            });
            buf.filled += copy_box(
                &tile.values,
                &origin,
                &shape,
                &mut buf.data,
                &buf.origin,
                &buf.shape,
            );
            if buf.filled == buf.shape.iter().product::<usize>() {
                let b = self.open.remove(&key).expect("open chunk");
                done.push((tile.var, lin, b));
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

/// Runs one stage and writes its output through `writer`. Without a kernel, the stage's tiles
/// are written; with a kernel, they are pushed lane by lane, in fold order, into the kernel's
/// states, whose output tiles (described by `out`) are written.
pub fn run(stage: &Stage, out: &[OutVar], writer: Arc<dyn Writer>, s: Settings) -> Result<()> {
    trace("pipeline start");
    let pool = Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(s.threads)
            .thread_name(|i| format!("cdors-compute-{i}"))
            .build()
            .map_err(|e| Error::internal(format!("thread pool: {e}")))?,
    );
    let shared = Arc::new(Shared {
        error: Mutex::new(None),
        cancel: AtomicBool::new(false),
        window: Semaphore::new(s.window),
    });
    let (fetch_tx, fetch_rx) = unbounded::<Fetch>();
    let (msg_tx, msg_rx) = unbounded::<Msg>();
    let svs: Vec<Arc<StageVar>> = stage.vars.iter().cloned().map(Arc::new).collect();

    std::thread::scope(|scope| {
        for i in 0..s.io_threads {
            let (rx, tx, pool, sh) = (
                fetch_rx.clone(),
                msg_tx.clone(),
                pool.clone(),
                shared.clone(),
            );
            std::thread::Builder::new()
                .name(format!("cdors-io-{i}"))
                .spawn_scoped(scope, move || fetch_loop(rx, tx, pool, sh))
                .expect("spawn I/O thread");
        }
        drop(fetch_rx);

        // writer thread
        let wshared = shared.clone();
        // Zarr chunks are encoded and stored (blocking file writes) on their own pool, so that
        // slow storage does not stall decoding
        let wpool = match rayon::ThreadPoolBuilder::new()
            .num_threads(s.threads)
            .thread_name(|i| format!("cdors-write-{i}"))
            .build()
        {
            Ok(p) => Arc::new(p),
            Err(e) => {
                shared.fail(Error::internal(format!("thread pool: {e}")));
                pool.clone()
            }
        };
        let writer_handle = std::thread::Builder::new()
            .name("cdors-writer".into())
            .spawn_scoped(scope, move || {
                write_loop(msg_rx, out, writer, wshared, wpool, s.threads)
            })
            .expect("spawn writer thread");

        // dispatch
        let mut n = 0usize;
        let mut lanes: HashMap<(usize, Vec<usize>), Arc<Lane>> = HashMap::new();
        'outer: for sv in &svs {
            let fold: Vec<bool> = match &stage.kernel {
                Some(k) => sv.dims.iter().map(|d| d.role == k.fold_dim()).collect(),
                None => vec![false; sv.dims.len()],
            };
            for t in 0..sv.tiling.num_tiles() {
                shared.window.acquire();
                if shared.cancel.load(Ordering::SeqCst) {
                    shared.window.release();
                    break 'outer;
                }
                let bx = sv.tiling.tile(t);
                let lane = stage.kernel.as_ref().map(|k| {
                    let seg = sv.tiling.segment_indices(t);
                    let (mut key, mut seq, mut total) = (seg.clone(), 0usize, 1usize);
                    for d in 0..seg.len() {
                        if fold[d] {
                            let m = sv.tiling.segments[d].len();
                            seq = seq * m + seg[d];
                            total *= m;
                            key[d] = 0;
                        }
                    }
                    let l = lanes
                        .entry((sv.var, key))
                        .or_insert_with(|| {
                            let mut lb = bx.clone();
                            for (d, f) in fold.iter().enumerate() {
                                if *f {
                                    lb.ranges[d] = 0..sv.dims[d].size;
                                }
                            }
                            Arc::new(Lane {
                                inner: Mutex::new(LaneInner {
                                    next: 0,
                                    total,
                                    pending: BTreeMap::new(),
                                    state: Some(k.start(sv.var, &lb)),
                                }),
                            })
                        })
                        .clone();
                    (l, seq)
                });
                let reads: Vec<LeafRead> =
                    sv.leaves.iter().map(|l| LeafRead::new(l, &bx)).collect();
                let lists: Vec<Vec<Vec<u64>>> = reads.iter().map(LeafRead::chunks).collect();
                let total: usize = lists.iter().map(Vec::len).sum();
                let job = Arc::new(TileJob {
                    sv: sv.clone(),
                    bx,
                    chunks: Mutex::new(lists.iter().map(|l| vec![None; l.len()]).collect()),
                    reads,
                    remaining: AtomicUsize::new(total),
                    error: Mutex::new(None),
                    lane,
                });
                n += 1;
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
    });
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
    pool: Arc<rayon::ThreadPool>,
    threads: usize,
) -> Result<()> {
    let mut asm = Assembler {
        vars: out.to_vec(),
        open: HashMap::new(),
    };
    let ordered = writer.ordered();
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
        let done = asm.add(tile);
        release();
        for (var, lin, buf) in done {
            if ordered {
                pending.insert((var, lin), buf);
                while let Some(b) = pending.remove(&next) {
                    if !shared.cancel.load(Ordering::Relaxed)
                        && let Err(e) = writer.write(next.0, &b.origin, &b.shape, b.data)
                    {
                        shared.fail(e);
                    }
                    next.1 += 1;
                    while next.0 < nchunks.len() && next.1 >= nchunks[next.0] {
                        next = (next.0 + 1, 0);
                    }
                }
            } else {
                encode_slots.acquire();
                let (w, sh, slots) = (writer.clone(), shared.clone(), encode_slots.clone());
                pool.spawn(move || {
                    if !sh.cancel.load(Ordering::Relaxed)
                        && let Err(e) = w.write(var, &buf.origin, &buf.shape, buf.data)
                    {
                        sh.fail(e);
                    }
                    slots.release();
                });
            }
        }
    }
    trace("all tiles assembled");
    encode_slots.wait_all(2 * threads.max(1));
    trace("all chunks written");
    if !shared.cancel.load(Ordering::SeqCst) && (!asm.open.is_empty() || !pending.is_empty()) {
        return Err(Error::internal(format!(
            "{} output chunks were not completed",
            asm.open.len() + pending.len()
        )));
    }
    Ok(())
}
