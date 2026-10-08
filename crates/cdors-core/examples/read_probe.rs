//! Raw read-and-decode throughput probe for one Zarr array (Task 2 of the cdors prototype).
//!
//! Reads the *encoded* bytes of a set of chunks and decodes them in parallel, then prints one
//! JSON line with encoded and decoded throughput and the split between fetching and decoding.
//!
//! ```text
//! read_probe <store path | https URL> <array path> [--threads N] [--concurrency M]
//!            [--max-chunks K] [--dim0-range a:b] [--order index|spread] [--http-sync] [--http2]
//! ```
//!
//! - Local stores: N rayon threads each fetch a chunk and decode it right away ("fused"). With
//!   `--concurrency M`, M plain OS threads read files instead and pass the bytes to the N decoders.
//! - HTTPS stores: M async fetches in flight (tokio) feeding N rayon decoders. `--http-sync`
//!   instead lets each of the N rayon threads block on its own fetch (concurrency = N).
//! - `--dim0-range a:b` restricts the first chunk index to `a..b` (chunk indices, not elements);
//!   `--max-chunks K` keeps the first K chunks in index order; `--order spread` then visits them in
//!   a fixed pseudo-random order.
//! - Missing chunks count as fill (`missing_chunks`, `fill_bytes`), not as errors, and are not
//!   included in `decoded_bytes`.
//!
//! The probe writes nothing but the JSON line on stdout (and errors on stderr).

use std::borrow::Cow;
use std::num::NonZeroU64;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use object_store::ClientOptions;
use object_store::http::HttpBuilder;
use rayon::prelude::*;
use zarrs::array::{Array, ArrayBytes, ArrayToBytesCodecTraits, CodecOptions, DataType, FillValue};
use zarrs::filesystem::FilesystemStore;
use zarrs_object_store::AsyncObjectStore;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

struct Args {
    store: String,
    array: String,
    threads: usize,
    concurrency: usize,
    max_chunks: Option<usize>,
    dim0_range: Option<(u64, u64)>,
    spread: bool,
    http_sync: bool,
    http2: bool,
}

fn usage() -> ! {
    eprintln!(
        "usage: read_probe <store path or https URL> <array path> [--threads N] [--concurrency M] \
         [--max-chunks K] [--dim0-range a:b] [--order index|spread] [--http-sync] [--http2]"
    );
    std::process::exit(1);
}

fn parse_args() -> Args {
    let mut it = std::env::args().skip(1);
    let mut pos = Vec::new();
    let mut a = Args {
        store: String::new(),
        array: String::new(),
        threads: 8,
        concurrency: 0,
        max_chunks: None,
        dim0_range: None,
        spread: false,
        http_sync: false,
        http2: false,
    };
    while let Some(arg) = it.next() {
        let mut val = |name: &str| it.next().unwrap_or_else(|| panic!("{name} needs a value"));
        match arg.as_str() {
            "--threads" => a.threads = val("--threads").parse().unwrap_or_else(|_| usage()),
            "--concurrency" => {
                a.concurrency = val("--concurrency").parse().unwrap_or_else(|_| usage());
            }
            "--max-chunks" => {
                a.max_chunks = Some(val("--max-chunks").parse().unwrap_or_else(|_| usage()));
            }
            "--dim0-range" => {
                let v = val("--dim0-range");
                let (lo, hi) = v.split_once(':').unwrap_or_else(|| usage());
                let lo = if lo.is_empty() {
                    0
                } else {
                    lo.parse().unwrap_or_else(|_| usage())
                };
                let hi = if hi.is_empty() {
                    u64::MAX
                } else {
                    hi.parse().unwrap_or_else(|_| usage())
                };
                a.dim0_range = Some((lo, hi));
            }
            "--order" => match val("--order").as_str() {
                "index" => a.spread = false,
                "spread" => a.spread = true,
                _ => usage(),
            },
            "--http-sync" => a.http_sync = true,
            "--http2" => a.http2 = true,
            "-h" | "--help" => usage(),
            s if s.starts_with("--") => usage(),
            _ => pos.push(arg),
        }
    }
    if pos.len() != 2 || a.threads == 0 {
        usage();
    }
    a.store = pos[0].clone();
    a.array = if pos[1].starts_with('/') {
        pos[1].clone()
    } else {
        format!("/{}", pos[1])
    };
    a
}

/// Chunk indices of the grid in C order, first index restricted to `dim0`, at most `max` of them.
fn enumerate_chunks(grid: &[u64], dim0: Option<(u64, u64)>, max: Option<usize>) -> Vec<Vec<u64>> {
    let (lo, hi) = dim0.unwrap_or((0, u64::MAX));
    let hi = hi.min(grid[0]);
    let max = max.unwrap_or(usize::MAX);
    let mut out = Vec::new();
    if lo >= hi || grid.contains(&0) {
        return out;
    }
    let mut idx: Vec<u64> = grid.iter().map(|_| 0).collect();
    idx[0] = lo;
    'outer: while out.len() < max {
        out.push(idx.clone());
        for d in (0..idx.len()).rev() {
            idx[d] += 1;
            let end = if d == 0 { hi } else { grid[d] };
            if idx[d] < end {
                continue 'outer;
            }
            if d == 0 {
                break 'outer;
            }
            idx[d] = 0;
        }
    }
    out
}

/// Deterministic Fisher-Yates shuffle (xorshift64), so that "spread" runs are repeatable.
fn spread(chunks: &mut [Vec<u64>]) {
    let mut s: u64 = 0x9E37_79B9_7F4A_7C15;
    for i in (1..chunks.len()).rev() {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        let j = (s % (i as u64 + 1)) as usize;
        chunks.swap(i, j);
    }
}

/// Shared counters; durations are summed over threads / requests in nanoseconds.
#[derive(Default)]
struct Stats {
    chunks: AtomicU64,
    missing: AtomicU64,
    encoded: AtomicU64,
    decoded: AtomicU64,
    fill: AtomicU64,
    fetch_ns: AtomicU64,
    decode_ns: AtomicU64,
    /// Sum of sampled values in thousandths (wrapping), so the decoded data must really exist.
    checksum_milli: AtomicU64,
    nan_samples: AtomicU64,
}

fn add_dur(a: &AtomicU64, d: Duration) {
    a.fetch_add(d.as_nanos() as u64, Ordering::Relaxed);
}

/// What the decoder needs from the array, independent of the storage type.
struct Decoder {
    codecs: Arc<zarrs::array::CodecChain>,
    data_type: DataType,
    fill_value: FillValue,
    elem_size: usize,
    is_f32: bool,
    opts: CodecOptions,
}

impl Decoder {
    fn new<S: ?Sized>(array: &Array<S>) -> Self {
        let elem_size = array.data_type().fixed_size().unwrap_or(1);
        let is_f32 = elem_size == 4 && format!("{:?}", array.data_type()).contains("Float32");
        Self {
            codecs: array.codecs(),
            data_type: array.data_type().clone(),
            fill_value: array.fill_value().clone(),
            elem_size,
            is_f32,
            // one thread per chunk: parallelism comes from decoding many chunks at once
            opts: CodecOptions::default().with_concurrent_target(1),
        }
    }

    /// Decode one chunk (or account for it as fill if missing) and update `stats`.
    fn decode(&self, encoded: Option<&[u8]>, shape: &[NonZeroU64], stats: &Stats) {
        stats.chunks.fetch_add(1, Ordering::Relaxed);
        let n_elem: u64 = shape.iter().map(|s| s.get()).product();
        let Some(enc) = encoded else {
            stats.missing.fetch_add(1, Ordering::Relaxed);
            stats
                .fill
                .fetch_add(n_elem * self.elem_size as u64, Ordering::Relaxed);
            return;
        };
        let t = Instant::now();
        let bytes = self
            .codecs
            .decode(
                Cow::Borrowed(enc),
                shape,
                &self.data_type,
                &self.fill_value,
                &self.opts,
            )
            .unwrap_or_else(|e| panic!("decode failed: {e}"));
        let raw = match &bytes {
            ArrayBytes::Fixed(raw) => raw.as_ref(),
            _ => panic!("variable-length data types are not supported by the probe"),
        };
        // touch one value every 4 KiB so the decoded buffer is really read
        let mut sum = 0f64;
        let mut nan = 0u64;
        if self.is_f32 {
            for c in raw.as_chunks::<4>().0.iter().step_by(1024) {
                let v = f32::from_le_bytes(*c);
                if v.is_nan() {
                    nan += 1
                } else {
                    sum += f64::from(v)
                }
            }
        } else {
            sum = raw.iter().step_by(4096).map(|&b| f64::from(b)).sum();
        }
        add_dur(&stats.decode_ns, t.elapsed());
        stats.encoded.fetch_add(enc.len() as u64, Ordering::Relaxed);
        stats.decoded.fetch_add(raw.len() as u64, Ordering::Relaxed);
        stats
            .checksum_milli
            .fetch_add((sum * 1e3) as i64 as u64, Ordering::Relaxed);
        stats.nan_samples.fetch_add(nan, Ordering::Relaxed);
    }
}

fn chunk_shapes<S: ?Sized>(
    array: &Array<S>,
    chunks: &[Vec<u64>],
) -> Result<Vec<Vec<NonZeroU64>>, BoxError> {
    chunks
        .iter()
        .map(|c| Ok(array.chunk_shape(c)?.to_vec()))
        .collect()
}

/// Local filesystem store, fused: each rayon thread reads a chunk file and decodes it.
fn run_local_fused(
    array: &Array<FilesystemStore>,
    chunks: &[Vec<u64>],
    stats: &Stats,
) -> Result<(), BoxError> {
    let dec = Decoder::new(array);
    let shapes = chunk_shapes(array, chunks)?;
    chunks
        .par_iter()
        .zip(shapes.par_iter())
        .try_for_each(|(c, shape)| -> Result<(), BoxError> {
            let t = Instant::now();
            let enc = array.retrieve_encoded_chunk(c)?;
            add_dur(&stats.fetch_ns, t.elapsed());
            dec.decode(enc.as_deref(), shape, stats);
            Ok(())
        })
}

/// Local filesystem store, pipelined: `conc` OS threads read files, the rayon pool decodes.
fn run_local_pipelined(
    array: &Array<FilesystemStore>,
    chunks: &[Vec<u64>],
    conc: usize,
    threads: usize,
    stats: &Stats,
) -> Result<(), BoxError> {
    let dec = Decoder::new(array);
    let shapes = chunk_shapes(array, chunks)?;
    let next = AtomicUsize::new(0);
    let (tx, rx) = std::sync::mpsc::sync_channel::<(usize, Option<Vec<u8>>)>(2 * threads + conc);
    std::thread::scope(|s| -> Result<(), BoxError> {
        for _ in 0..conc {
            let tx = tx.clone();
            let next = &next;
            s.spawn(move || {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= chunks.len() {
                        break;
                    }
                    let t = Instant::now();
                    let enc = array
                        .retrieve_encoded_chunk(&chunks[i])
                        .unwrap_or_else(|e| panic!("read failed: {e}"));
                    add_dur(&stats.fetch_ns, t.elapsed());
                    if tx.send((i, enc)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        rx.into_iter()
            .par_bridge()
            .for_each(|(i, enc)| dec.decode(enc.as_deref(), &shapes[i], stats));
        Ok(())
    })
}

fn http_store(
    url: &str,
    http2: bool,
) -> Result<AsyncObjectStore<object_store::http::HttpStore>, BoxError> {
    let mut opts = ClientOptions::new()
        .with_timeout(Duration::from_secs(120))
        .with_pool_max_idle_per_host(256);
    if http2 {
        opts = opts.with_allow_http2();
    }
    let store = HttpBuilder::new()
        .with_url(url)
        .with_client_options(opts)
        .build()?;
    Ok(AsyncObjectStore::new(store))
}

type HttpArray = Array<AsyncObjectStore<object_store::http::HttpStore>>;

/// Open an array over HTTP. Kerchunk stores built from HDF5 put the compressor into `filters`
/// (`"compressor": null, "filters": [{"id": "blosc", ...}]`), which zarrs refuses for blosc
/// because the v2 -> v3 conversion needs the element size. A trailing bytes-to-bytes filter with
/// no compressor is the same pipeline as that codec used as the compressor, so move it there.
async fn open_http_array(
    store: Arc<AsyncObjectStore<object_store::http::HttpStore>>,
    path: &str,
) -> Result<HttpArray, BoxError> {
    match Array::async_open(store.clone(), path).await {
        Ok(a) => return Ok(a),
        Err(zarrs::array::ArrayCreateError::UnsupportedZarrV2Array(msg)) => {
            eprintln!("note: {msg}; retrying with the trailing filter moved to the compressor");
        }
        Err(e) => return Err(e.into()),
    }
    use zarrs::storage::AsyncReadableStorageTraits;
    let key = zarrs::storage::StoreKey::new(format!("{}/.zarray", path.trim_start_matches('/')))?;
    let raw = store.get(&key).await?.ok_or("no .zarray")?;
    let mut meta: serde_json::Value = serde_json::from_slice(&raw)?;
    if meta["compressor"].is_null()
        && let Some(filters) = meta["filters"].as_array_mut()
        && let Some(last) = filters.pop()
    {
        if filters.is_empty() {
            meta["filters"] = serde_json::Value::Null;
        }
        meta["compressor"] = last;
    }
    let v2: zarrs::array::ArrayMetadataV2 = serde_json::from_value(meta)?;
    Ok(Array::new_with_metadata(
        store,
        path,
        zarrs::array::ArrayMetadata::V2(v2),
    )?)
}

/// HTTP store: `conc` async fetch workers on tokio, decoded on the rayon pool.
fn run_http_async(
    rt: &tokio::runtime::Runtime,
    array: Arc<HttpArray>,
    chunks: Arc<Vec<Vec<u64>>>,
    conc: usize,
    threads: usize,
    stats: Arc<Stats>,
) -> Result<(), BoxError> {
    let dec = Decoder::new(&array);
    let shapes = chunk_shapes(&array, &chunks)?;
    let (tx, mut rx) =
        tokio::sync::mpsc::channel::<(usize, Option<zarrs::storage::Bytes>)>(2 * threads + conc);
    let next = Arc::new(AtomicUsize::new(0));
    let mut workers = Vec::new();
    for _ in 0..conc {
        let (tx, next, array, chunks, stats) = (
            tx.clone(),
            next.clone(),
            array.clone(),
            chunks.clone(),
            stats.clone(),
        );
        workers.push(rt.spawn(async move {
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= chunks.len() {
                    return Ok::<(), String>(());
                }
                let t = Instant::now();
                let enc = array
                    .async_retrieve_encoded_chunk(&chunks[i])
                    .await
                    .map_err(|e| format!("fetch of chunk {:?} failed: {e}", chunks[i]))?;
                add_dur(&stats.fetch_ns, t.elapsed());
                if tx.send((i, enc)).await.is_err() {
                    return Ok(());
                }
            }
        }));
    }
    drop(tx);
    std::iter::from_fn(|| rx.blocking_recv())
        .par_bridge()
        .for_each(|(i, enc)| dec.decode(enc.as_deref(), &shapes[i], &stats));
    for w in workers {
        rt.block_on(w)??;
    }
    Ok(())
}

/// HTTP store, sync style: each rayon thread blocks on its own fetch, then decodes.
fn run_http_sync(
    rt: &tokio::runtime::Runtime,
    array: &HttpArray,
    chunks: &[Vec<u64>],
    stats: &Stats,
) -> Result<(), BoxError> {
    let dec = Decoder::new(array);
    let shapes = chunk_shapes(array, chunks)?;
    let handle = rt.handle().clone();
    chunks
        .par_iter()
        .zip(shapes.par_iter())
        .try_for_each(|(c, shape)| -> Result<(), BoxError> {
            let t = Instant::now();
            let enc = handle.block_on(array.async_retrieve_encoded_chunk(c))?;
            add_dur(&stats.fetch_ns, t.elapsed());
            dec.decode(enc.as_deref(), shape, stats);
            Ok(())
        })
}

fn main() -> Result<(), BoxError> {
    let args = parse_args();
    rayon::ThreadPoolBuilder::new()
        .num_threads(args.threads)
        .build_global()?;
    let is_http = args.store.starts_with("http://") || args.store.starts_with("https://");
    let stats = Arc::new(Stats::default());

    let (mode, conc, n_chunks, wall) = if is_http {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()?;
        let store = rt.block_on(async { http_store(&args.store, args.http2) })?;
        let array = rt.block_on(open_http_array(Arc::new(store), &args.array))?;
        let mut chunks =
            enumerate_chunks(array.chunk_grid_shape(), args.dim0_range, args.max_chunks);
        if args.spread {
            spread(&mut chunks);
        }
        let n = chunks.len();
        let t0 = Instant::now();
        if args.http_sync {
            run_http_sync(&rt, &array, &chunks, &stats)?;
            ("http-sync", args.threads, n, t0.elapsed())
        } else {
            let conc = if args.concurrency == 0 {
                16
            } else {
                args.concurrency
            };
            run_http_async(
                &rt,
                Arc::new(array),
                Arc::new(chunks),
                conc,
                args.threads,
                stats.clone(),
            )?;
            ("http-async", conc, n, t0.elapsed())
        }
    } else {
        let store = FilesystemStore::new(&args.store)?;
        let array = Array::open(Arc::new(store), &args.array)?;
        let mut chunks =
            enumerate_chunks(array.chunk_grid_shape(), args.dim0_range, args.max_chunks);
        if args.spread {
            spread(&mut chunks);
        }
        let n = chunks.len();
        let t0 = Instant::now();
        if args.concurrency == 0 {
            run_local_fused(&array, &chunks, &stats)?;
            ("local-fused", args.threads, n, t0.elapsed())
        } else {
            run_local_pipelined(&array, &chunks, args.concurrency, args.threads, &stats)?;
            ("local-pipelined", args.concurrency, n, t0.elapsed())
        }
    };

    let secs = wall.as_secs_f64();
    let ld = |a: &AtomicU64| a.load(Ordering::Relaxed);
    let (enc, decd) = (ld(&stats.encoded), ld(&stats.decoded));
    let fetch_s = ld(&stats.fetch_ns) as f64 * 1e-9;
    let decode_s = ld(&stats.decode_ns) as f64 * 1e-9;
    assert_eq!(
        ld(&stats.chunks) as usize,
        n_chunks,
        "not every chunk was processed"
    );
    let r3 = |x: f64| (x * 1000.0).round() / 1000.0;
    let out = serde_json::json!({
        "url": args.store,
        "array": args.array,
        "mode": mode,
        "order": if args.spread { "spread" } else { "index" },
        "dim0_range": args.dim0_range.map(|(a, b)| format!("{a}:{}", if b == u64::MAX { String::new() } else { b.to_string() })),
        "chunks": n_chunks,
        "missing_chunks": ld(&stats.missing),
        "encoded_bytes": enc,
        "decoded_bytes": decd,
        "fill_bytes": ld(&stats.fill),
        "seconds": r3(secs),
        "encoded_GBps": r3(enc as f64 / secs / 1e9),
        "decoded_GBps": r3(decd as f64 / secs / 1e9),
        "threads": args.threads,
        "concurrency": conc,
        // summed over threads (fused: fetch + decode = busy time of the N threads)
        "fetch_thread_s": r3(fetch_s),
        "decode_thread_s": r3(decode_s),
        "fetch_share": r3(fetch_s / (fetch_s + decode_s).max(1e-12)),
        // mean number of fetches in flight, and fraction of the N decode threads kept busy
        "fetch_inflight_mean": r3(fetch_s / secs),
        "decode_busy": r3(decode_s / (secs * args.threads as f64)),
        "checksum": ld(&stats.checksum_milli) as i64 as f64 * 1e-3,
        "nan_samples": ld(&stats.nan_samples),
    });
    println!("{out}");
    Ok(())
}
