//! Check program for remote reads (`io::remote`). Every read is capped by its arguments.
//!
//! ```text
//! remote_check --compare A B VAR FIRST COUNT THREADS
//!     Open A and B through `io::open` (URL, kerchunk references, Zarr, NetCDF), read chunks
//!     FIRST..FIRST+COUNT along the first dimension of VAR (all other chunk indices 0) on THREADS
//!     threads, decode them and compare: stored bytes equal, values bitwise equal (NaN = NaN).
//!
//! remote_check --rate SRC VAR FIRST COUNT THREADS
//!     Fetch the stored bytes of the same chunks on THREADS threads, each blocking on one
//!     request at a time (as the executor's I/O pool does), and print the fetch rate.
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use cdors_core::io::{ChunkSource, EncodedChunk, RawChunk, Values};

type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
/// Stored bytes and value bits of one chunk.
type Chunk = (Vec<u8>, Vec<u64>);

/// Runs `f(i)` for i in 0..n on `threads` threads (a shared counter, like an I/O pool).
fn pool<T: Send>(n: usize, threads: usize, f: impl Fn(usize) -> Res<T> + Sync) -> Res<Vec<T>> {
    let next = AtomicUsize::new(0);
    let mut out: Vec<(usize, T)> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| -> Res<Vec<(usize, T)>> {
                    let mut v = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= n {
                            return Ok(v);
                        }
                        v.push((i, f(i)?));
                    }
                })
            })
            .collect();
        let mut all = Vec::new();
        for h in hs {
            all.extend(h.join().map_err(|_| "thread panicked")??);
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(all)
    })?;
    out.sort_by_key(|(i, _)| *i);
    Ok(out.into_iter().map(|(_, t)| t).collect())
}

fn indices(src: &dyn ChunkSource, var: &str, first: u64, count: u64) -> Res<Vec<Vec<u64>>> {
    let grid = src.chunk_grid(var)?;
    let n0 = grid.counts()[0];
    let last = (first + count).min(n0);
    Ok((first..last)
        .map(|t| {
            let mut ix = vec![0u64; grid.shape.len()];
            ix[0] = t;
            ix
        })
        .collect())
}

fn encoded(raw: RawChunk) -> Res<EncodedChunk> {
    match raw {
        RawChunk::Encoded(e) => Ok(e),
        RawChunk::Decoded(_) => Err("source returned decoded chunks".into()),
    }
}

fn bits(v: &Values) -> Vec<u64> {
    match v {
        Values::F32(x) => x
            .iter()
            .map(|f| {
                if f.is_nan() {
                    u64::MAX
                } else {
                    f.to_bits() as u64
                }
            })
            .collect(),
        Values::F64(x) => x
            .iter()
            .map(|f| if f.is_nan() { u64::MAX } else { f.to_bits() })
            .collect(),
    }
}

fn main() -> Res<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--compare") => {
            let [_, a, b, var, first, count, threads] = args.as_slice() else {
                return Err("usage: --compare A B VAR FIRST COUNT THREADS".into());
            };
            let (first, count, threads) = (first.parse()?, count.parse()?, threads.parse()?);
            let t = Instant::now();
            let sa = cdors_core::io::open(a)?;
            let sb = cdors_core::io::open(b)?;
            eprintln!("opened both in {:.2} s", t.elapsed().as_secs_f64());
            let ix = indices(sa.as_ref(), var, first, count)?;
            if ix != indices(sb.as_ref(), var, first, count)? {
                return Err("chunk grids differ".into());
            }
            let read = |src: &Arc<dyn ChunkSource>| -> Res<(Vec<Chunk>, f64)> {
                let t = Instant::now();
                let v = pool(ix.len(), threads, |i| {
                    let e = encoded(src.read_chunk(var, &ix[i])?)?;
                    let d = e.decode()?;
                    Ok((e.bytes.clone().unwrap_or_default(), bits(&d.values)))
                })?;
                Ok((v, t.elapsed().as_secs_f64()))
            };
            let (va, ta) = read(&sa)?;
            let (vb, tb) = read(&sb)?;
            let enc: usize = va.iter().map(|x| x.0.len()).sum();
            let (mut same_bytes, mut same_values, mut values) = (0, 0, 0usize);
            for ((ea, xa), (eb, xb)) in va.iter().zip(&vb) {
                same_bytes += usize::from(ea == eb);
                same_values += usize::from(xa == xb);
                values += xa.len();
            }
            println!(
                "{{\"chunks\":{},\"values\":{values},\"encoded_bytes\":{enc},\
                 \"same_bytes\":{same_bytes},\"same_values\":{same_values},\
                 \"a_s\":{ta:.2},\"b_s\":{tb:.2}}}",
                ix.len()
            );
            if same_values != ix.len() {
                return Err("values differ".into());
            }
        }
        Some("--rate") => {
            let [_, src, var, first, count, threads] = args.as_slice() else {
                return Err("usage: --rate SRC VAR FIRST COUNT THREADS".into());
            };
            let (first, count, threads) = (first.parse()?, count.parse()?, threads.parse()?);
            let s = cdors_core::io::open(src)?;
            let ix = indices(s.as_ref(), var, first, count)?;
            let t = Instant::now();
            let sizes = pool(ix.len(), threads, |i| {
                Ok(encoded(s.read_chunk(var, &ix[i])?)?.encoded_len())
            })?;
            let secs = t.elapsed().as_secs_f64();
            let bytes: usize = sizes.iter().sum();
            println!(
                "{{\"chunks\":{},\"in_flight\":{threads},\"encoded_gb\":{:.3},\"s\":{secs:.2},\
                 \"gb_per_s\":{:.3}}}",
                ix.len(),
                bytes as f64 / 1e9,
                bytes as f64 / 1e9 / secs
            );
        }
        _ => return Err("usage: remote_check --compare A B VAR FIRST COUNT THREADS | --rate SRC VAR FIRST COUNT THREADS".into()),
    }
    Ok(())
}
