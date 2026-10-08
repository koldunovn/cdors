//! Run progress: counters the pipeline bumps per fetched chunk, `--progress json` lines on
//! stderr, and how far a failed run got.
//!
//! The counters are process-wide atomics (one run per process). With `--progress json` a
//! reporter thread prints one line about once per second and a final summary line:
//!
//! ```json
//! {"event":"start","stages":1,"chunks_total":3288,"bytes_decoded_total":9469440}
//! {"event":"progress","stage":0,"stages":1,"chunks_done":1200,"chunks_total":3288,"bytes_read":1234567,"elapsed_s":1.0}
//! {"event":"done","status":"ok","wall_s":2.31,"chunks_read":3288,"bytes_read":3456789,"bytes_decoded_planned":9469440,"peak_rss_bytes":123456789,"output":"out.nc"}
//! ```
//!
//! `bytes_read` counts stored (compressed) bytes as fetched; `bytes_decoded` is the planned
//! decoded total (`bytes_decoded_planned`).

use crate::io::{RawChunk, Values};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

static CHUNKS_DONE: AtomicU64 = AtomicU64::new(0);
static BYTES_READ: AtomicU64 = AtomicU64::new(0);
static STAGE: AtomicUsize = AtomicUsize::new(0);
static STAGES: AtomicUsize = AtomicUsize::new(0);
static CHUNKS_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Called by the pipeline for every fetched chunk.
pub fn chunk_fetched(raw: &RawChunk) {
    let n = match raw {
        RawChunk::Encoded(e) => e.encoded_len(),
        RawChunk::Decoded(d) => match &d.values {
            Values::F32(v) => v.len() * 4,
            Values::F64(v) => v.len() * 8,
        },
    };
    CHUNKS_DONE.fetch_add(1, Ordering::Relaxed);
    BYTES_READ.fetch_add(n as u64, Ordering::Relaxed);
}

/// Sets the totals before the first stage runs.
pub fn start(stages: usize, chunks_total: u64) {
    STAGES.store(stages, Ordering::Relaxed);
    CHUNKS_TOTAL.store(chunks_total, Ordering::Relaxed);
}

/// Marks the stage now running (0-based).
pub fn set_stage(i: usize) {
    STAGE.store(i, Ordering::Relaxed);
}

/// Where the run is: `(stage, chunks done, chunks total, bytes read)`.
pub fn snapshot() -> (usize, u64, u64, u64) {
    (
        STAGE.load(Ordering::Relaxed),
        CHUNKS_DONE.load(Ordering::Relaxed),
        CHUNKS_TOTAL.load(Ordering::Relaxed),
        BYTES_READ.load(Ordering::Relaxed),
    )
}

/// Peak resident memory of this process (`VmHWM` in /proc/self/status), if available.
pub fn peak_rss() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = s.lines().find(|l| l.starts_with("VmHWM:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

fn line(v: Value) {
    eprintln!("{v}");
}

fn progress_line(t0: Instant) {
    let (stage, done, total, bytes) = snapshot();
    line(json!({
        "event": "progress",
        "stage": stage,
        "stages": STAGES.load(Ordering::Relaxed),
        "chunks_done": done,
        "chunks_total": total,
        "bytes_read": bytes,
        "elapsed_s": (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0,
    }));
}

/// The `--progress json` reporter: prints a line about once per second until dropped.
pub struct Reporter {
    t0: Instant,
    stop: Option<mpsc::Sender<()>>,
    handle: Option<std::thread::JoinHandle<()>>,
    bytes_decoded: u64,
}

impl Reporter {
    /// Starts reporting (after [`start`]); `bytes_decoded` is the planned decoded total.
    pub fn spawn(bytes_decoded: u64) -> Self {
        let t0 = Instant::now();
        line(json!({
            "event": "start",
            "stages": STAGES.load(Ordering::Relaxed),
            "chunks_total": CHUNKS_TOTAL.load(Ordering::Relaxed),
            "bytes_decoded_total": bytes_decoded,
        }));
        let (tx, rx) = mpsc::channel::<()>();
        let handle = std::thread::Builder::new()
            .name("cdors-progress".into())
            .spawn(move || {
                while let Err(mpsc::RecvTimeoutError::Timeout) =
                    rx.recv_timeout(Duration::from_secs(1))
                {
                    progress_line(t0);
                }
            })
            .ok();
        Self {
            t0,
            stop: Some(tx),
            handle,
            bytes_decoded,
        }
    }

    /// Stops the reporter and prints the summary line.
    pub fn finish(mut self, status: &str, error: Option<&str>, output: &str) {
        self.stop.take();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let (_, done, _, bytes) = snapshot();
        let mut v = json!({
            "event": "done",
            "status": status,
            "wall_s": (self.t0.elapsed().as_secs_f64() * 1000.0).round() / 1000.0,
            "chunks_read": done,
            "bytes_read": bytes,
            "bytes_decoded_planned": self.bytes_decoded,
            "peak_rss_bytes": peak_rss(),
            "output": output,
        });
        if let Some(e) = error {
            v["error"] = Value::from(e);
        }
        line(v);
    }
}
