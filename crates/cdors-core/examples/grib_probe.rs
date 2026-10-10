//! Decode speed of GRIB messages: `cargo run --release --example grib_probe -- file.grb [threads]`.
//! Splits the file into messages, decodes all of them on one thread, then on a rayon pool.
use rayon::prelude::*;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&args[1]).expect("read");
    let threads: usize = args.get(2).map_or(16, |s| s.parse().unwrap());
    let mut msgs = Vec::new();
    let mut pos = 0;
    while pos + 16 <= data.len() {
        assert_eq!(&data[pos..pos + 4], b"GRIB");
        let len = u64::from_be_bytes(data[pos + 8..pos + 16].try_into().unwrap()) as usize;
        msgs.push(&data[pos..pos + len]);
        pos += len;
    }
    let t = Instant::now();
    let mut n = 0;
    for m in &msgs {
        n += cdors_core::io::grib::decode(m).unwrap().len();
    }
    let dt = t.elapsed().as_secs_f64();
    println!(
        "{} messages, {n} values: 1 thread {:.2} s ({:.1} ms/message, {:.0} MB/s as f64)",
        msgs.len(),
        dt,
        dt * 1e3 / msgs.len() as f64,
        n as f64 * 8.0 / 1e6 / dt
    );
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .unwrap();
    let t = Instant::now();
    let n: usize = pool.install(|| {
        msgs.par_iter()
            .map(|m| cdors_core::io::grib::decode(m).unwrap().len())
            .sum()
    });
    let dt = t.elapsed().as_secs_f64();
    println!(
        "{threads} threads {:.2} s ({:.0} MB/s as f64)",
        dt,
        n as f64 * 8.0 / 1e6 / dt
    );
}
