//! Check program for the Task 10 readers (`io::netcdf4_index`, `io::kerchunk`).
//!
//! ```text
//! nc4_index_check [--threads N] FILE.nc...
//!     For every variable of each NetCDF-4 file: decode every chunk through the chunk index and
//!     compare it byte for byte with the same hyperslab read through netCDF-C; for variables
//!     expressible as Zarr v2, also compare with the chunk read through zarrs from the index's
//!     kerchunk references. Then time a serial netCDF-C read of all variables against the
//!     direct decode with 1 and N threads, and zarrs with N threads.
//!
//! nc4_index_check --kerchunk REFS ARRAY NCHUNKS OUTDIR
//!     Open a kerchunk reference set (JSON file or Parquet directory) with zarrs, read NCHUNKS
//!     chunks of ARRAY spread over the chunk grid and write each as raw native-order bytes to
//!     OUTDIR/<ARRAY>_<i.j.k>.bin for comparison with xarray.
//!
//! nc4_index_check --sources [--threads N] [--var V] A B
//!     Open A and B through `io::open` (a `netcdf:` prefix forces the serial netCDF-C reader;
//!     globs give a multi-file source), read and decode every chunk of every numeric variable (or
//!     only V) on N threads, compare the values (bitwise, NaN = NaN) and print the times.
//!     Chunkwise when the chunk grids agree, otherwise variable by variable.
//! ```

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use cdors_core::io::kerchunk::KerchunkStore;
use cdors_core::io::netcdf_fallback::NetcdfSource;
use cdors_core::io::netcdf4_index::{Nc4File, Nc4Index, Nc4Variable};
use cdors_core::io::{ChunkSource, DecodedChunk, Values};
use rayon::prelude::*;
use zarrs::array::{Array, ArrayBytes};

type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn main() -> Res<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--kerchunk") {
        let [_, refs, array, n, out] = args.as_slice() else {
            return Err("usage: --kerchunk REFS ARRAY NCHUNKS OUTDIR".into());
        };
        return kerchunk_check(Path::new(refs), array, n.parse()?, Path::new(out));
    }
    let mut threads = 8;
    let mut files = Vec::new();
    let mut var = None;
    let mut sources = false;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        if a == "--threads" {
            threads = it.next().ok_or("--threads N")?.parse()?;
        } else if a == "--var" {
            var = Some(it.next().ok_or("--var V")?);
        } else if a == "--sources" {
            sources = true;
        } else {
            files.push(a);
        }
    }
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()?;
    if sources {
        let [a, b] = files.as_slice() else {
            return Err("usage: --sources [--threads N] [--var V] A B".into());
        };
        return pool.install(|| sources_check(a, b, var.as_deref()));
    }
    let mut failed = false;
    for f in &files {
        failed |= !check_file(Path::new(f), &pool, threads)?;
    }
    if failed {
        return Err("mismatches found".into());
    }
    Ok(())
}

/// All chunk grid coordinates of a variable, C order.
fn grid_coords(grid: &[u64]) -> Vec<Vec<u64>> {
    let n: u64 = grid.iter().product();
    (0..n)
        .map(|mut lin| {
            let mut c = vec![0; grid.len()];
            for d in (0..grid.len()).rev() {
                c[d] = lin % grid[d];
                lin /= grid[d];
            }
            c
        })
        .collect()
}

/// Copy the valid (in-extent) part of a full chunk into a compact C-order buffer.
fn crop(data: &[u8], chunk: &[u64], valid: &[u64], esize: usize) -> Vec<u8> {
    if chunk == valid {
        return data.to_vec();
    }
    let nd = chunk.len();
    let inner = valid[nd - 1] as usize * esize;
    let mut out = Vec::with_capacity(valid.iter().product::<u64>() as usize * esize);
    let outer: Vec<u64> = valid[..nd - 1].to_vec();
    for idx in grid_coords(&outer) {
        let mut off = 0u64;
        for d in 0..nd - 1 {
            off = off * chunk[d] + idx[d];
        }
        let start = (off * chunk[nd - 1]) as usize * esize;
        out.extend_from_slice(&data[start..start + inner]);
    }
    out
}

/// Root-group variables only; variables in groups are reported as skipped.
fn nc_variable<'f>(file: &'f netcdf::File, name: &str) -> Option<netcdf::Variable<'f>> {
    if name.contains('/') {
        return None;
    }
    file.variable(name)
}

fn check_file(path: &Path, pool: &rayon::ThreadPool, threads: usize) -> Res<bool> {
    println!("== {}", path.display());
    let t = Instant::now();
    let index = Nc4Index::build(path)?;
    let build_s = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let _ = Nc4Index::open(path)?; // writes the cache entry
    let t2 = Instant::now();
    let index_cached = Nc4Index::open(path)?;
    let cached_s = t2.elapsed().as_secs_f64();
    let _ = t;
    let nchunks: usize = index.variables.iter().map(|v| v.chunks.len()).sum();
    println!(
        "index: {} variables, {nchunks} stored chunks, HDF5 build {:.3} s, cached load {:.4} s",
        index.variables.len(),
        build_s,
        cached_s
    );
    assert_eq!(
        serde_json::to_string(&index.variables)?,
        serde_json::to_string(&index_cached.variables)?,
        "cached index differs"
    );

    let nc = netcdf::open(path)?;
    // HDF5 open of a file that netCDF-C holds open must work for lazy indexing in the engine.
    match Nc4Index::build(path) {
        Ok(_) => println!("index build while netCDF-C holds the file open: ok"),
        Err(e) => println!("index build while netCDF-C holds the file open: FAILED: {e}"),
    }
    let reader = Nc4File::with_index(index)?;
    let store = Arc::new(reader.index().to_kerchunk());
    let mut ok = true;
    let mut timed: Vec<&Nc4Variable> = Vec::new();
    for var in &reader.index().variables {
        if let Some(r) = &var.unsupported {
            println!("  {:<24} unsupported ({r}) -> netCDF-C fallback", var.name);
            continue;
        }
        let Some(ncvar) = nc_variable(&nc, &var.name) else {
            println!("  {:<24} not a netCDF variable, skipped", var.name);
            continue;
        };
        let esize = var.dtype.size();
        let zarr = if var.zarr_compatible() {
            Some(Array::open(store.clone(), &format!("/{}", var.name))?)
        } else {
            None
        };
        let (mut bad, mut zbad, mut n) = (0usize, 0usize, 0usize);
        for coords in grid_coords(&var.chunk_grid()) {
            let dc = reader.read_chunk(&var.name, &coords)?;
            let valid: Vec<u64> = (0..coords.len())
                .map(|d| (var.shape[d] - dc.origin[d]).min(dc.shape[d]))
                .collect();
            let extents: Vec<std::ops::Range<usize>> = (0..coords.len())
                .map(|d| dc.origin[d] as usize..(dc.origin[d] + valid[d]) as usize)
                .collect();
            let reference = ncvar.get_raw_values(extents)?;
            if crop(&dc.data, &dc.shape, &valid, esize) != reference {
                bad += 1;
                if bad <= 3 {
                    println!("    MISMATCH {} chunk {coords:?}", var.name);
                }
            }
            if let Some(a) = &zarr {
                let zb = a
                    .retrieve_chunk::<ArrayBytes<'static>>(&coords)?
                    .into_fixed()?;
                if crop(&zb, &dc.shape, &valid, esize) != reference {
                    zbad += 1;
                    if zbad <= 3 {
                        println!("    ZARRS MISMATCH {} chunk {coords:?}", var.name);
                    }
                }
            }
            n += 1;
        }
        let filters: Vec<String> = var.filters.iter().map(|f| format!("{f:?}")).collect();
        println!(
            "  {:<24} {:?} {:?} chunk {:?} [{}]: {n} chunks, direct {} mismatches, zarrs {}",
            var.name,
            var.dtype,
            var.shape,
            var.chunk_shape,
            filters.join(","),
            bad,
            if zarr.is_some() {
                format!("{zbad} mismatches")
            } else {
                "n/a".into()
            }
        );
        ok &= bad == 0 && zbad == 0;
        timed.push(var);
    }

    // Timings (page cache is warm from the comparison pass).
    let total_bytes: usize = timed
        .iter()
        .map(|v| v.shape.iter().product::<u64>() as usize * v.dtype.size())
        .sum();
    let mb = total_bytes as f64 / 1e6;
    let t = Instant::now();
    for var in &timed {
        let v = nc_variable(&nc, &var.name).ok_or("variable vanished")?;
        std::hint::black_box(v.get_raw_values(..)?);
    }
    let nc_s = t.elapsed().as_secs_f64();
    let jobs: Vec<(&str, Vec<u64>)> = timed
        .iter()
        .flat_map(|v| {
            grid_coords(&v.chunk_grid())
                .into_iter()
                .map(|c| (v.name.as_str(), c))
        })
        .collect();
    let direct = |jobs: &[(&str, Vec<u64>)]| -> Res<f64> {
        let t = Instant::now();
        jobs.par_iter().try_for_each(|(name, c)| -> Res<()> {
            std::hint::black_box(reader.read_chunk(name, c)?);
            Ok(())
        })?;
        Ok(t.elapsed().as_secs_f64())
    };
    let one = rayon::ThreadPoolBuilder::new().num_threads(1).build()?;
    let d1 = one.install(|| direct(&jobs))?;
    let dn = pool.install(|| direct(&jobs))?;
    let zjobs: Vec<(Arc<Array<KerchunkStore>>, Vec<u64>)> = timed
        .iter()
        .filter(|v| v.zarr_compatible())
        .flat_map(|v| {
            let a = Arc::new(
                Array::open(store.clone(), &format!("/{}", v.name)).expect("opened above"),
            );
            grid_coords(&v.chunk_grid())
                .into_iter()
                .map(move |c| (a.clone(), c))
        })
        .collect();
    let t = Instant::now();
    pool.install(|| {
        zjobs.par_iter().try_for_each(|(a, c)| -> Res<()> {
            std::hint::black_box(a.retrieve_chunk::<ArrayBytes<'static>>(c)?);
            Ok(())
        })
    })?;
    let zn = t.elapsed().as_secs_f64();
    println!(
        "timing ({mb:.1} MB decoded, warm page cache): netCDF-C serial {nc_s:.3} s ({:.0} MB/s); \
         direct 1 thread {d1:.3} s ({:.0} MB/s); direct {threads} threads {dn:.3} s ({:.0} MB/s); \
         zarrs {threads} threads {zn:.3} s",
        mb / nc_s,
        mb / d1,
        mb / dn
    );
    println!("result: {}", if ok { "OK" } else { "MISMATCH" });
    Ok(ok)
}

fn kerchunk_check(refs: &Path, array: &str, n: u64, out: &Path) -> Res<()> {
    let t = Instant::now();
    let store = Arc::new(KerchunkStore::open(refs)?);
    println!(
        "opened {} in {:.3} s; arrays: {:?}",
        refs.display(),
        t.elapsed().as_secs_f64(),
        store.arrays()
    );
    let a = Array::open(store.clone(), &format!("/{array}"))?;
    let grid = a.chunk_grid_shape().to_vec();
    let total: u64 = grid.iter().product();
    println!(
        "{array}: shape {:?}, chunk grid {grid:?}, data type {:?}",
        a.shape(),
        a.data_type()
    );
    std::fs::create_dir_all(out)?;
    for k in 0..n.min(total) {
        let mut lin = if n <= 1 { 0 } else { k * (total - 1) / (n - 1) };
        let mut c = vec![0; grid.len()];
        for d in (0..grid.len()).rev() {
            c[d] = lin % grid[d];
            lin /= grid[d];
        }
        let key = c.iter().map(u64::to_string).collect::<Vec<_>>().join(".");
        let r = store.reference(&format!("{array}/{key}"))?;
        let t = Instant::now();
        let bytes = a.retrieve_chunk::<ArrayBytes<'static>>(&c)?.into_fixed()?;
        let dt = t.elapsed().as_secs_f64();
        let file = out.join(format!("{array}_{key}.bin"));
        std::fs::write(&file, &bytes)?;
        let r = match r {
            Some(cdors_core::io::kerchunk::Ref::Range {
                url,
                offset,
                length,
            }) => {
                format!("{url} @{offset}+{length:?}")
            }
            Some(_) => "inline".into(),
            None => "missing (fill)".into(),
        };
        println!("chunk {key}: {} bytes in {dt:.4} s from {r}", bytes.len());
    }
    Ok(())
}

fn open_input(a: &str) -> Res<Arc<dyn ChunkSource>> {
    Ok(match a.strip_prefix("netcdf:") {
        Some(p) => Arc::new(NetcdfSource::open(p)?),
        None => cdors_core::io::open(a)?,
    })
}

/// Values as bits, all NaNs equal.
fn value_bits(v: &Values) -> Vec<u64> {
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

/// Reads and decodes all chunks of `var` on the current rayon pool; returns them and the seconds.
fn decode_all(src: &dyn ChunkSource, var: &str) -> Res<(Vec<DecodedChunk>, f64)> {
    let grid = src.chunk_grid(var)?;
    let coords = grid_coords(&grid.counts());
    let t = Instant::now();
    let chunks = coords
        .par_iter()
        .map(|c| src.read_chunk(var, c).and_then(|r| r.decode()))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((chunks, t.elapsed().as_secs_f64()))
}

/// Scatters decoded chunks into the whole variable (C order), as bits.
fn assemble(chunks: &[DecodedChunk], shape: &[usize]) -> Vec<u64> {
    let mut out = vec![0u64; shape.iter().product()];
    for c in chunks {
        let bits = value_bits(&c.values);
        let ext: Vec<u64> = c.shape.iter().map(|&s| s as u64).collect();
        for (k, local) in grid_coords(&ext).into_iter().enumerate() {
            let mut lin = 0usize;
            for d in 0..shape.len() {
                lin = lin * shape[d] + (c.origin[d] + local[d]) as usize;
            }
            out[lin] = bits[k];
        }
    }
    out
}

fn sources_check(a: &str, b: &str, only: Option<&str>) -> Res<()> {
    let t = Instant::now();
    let sa = open_input(a)?;
    let ta = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let sb = open_input(b)?;
    let tb = t.elapsed().as_secs_f64();
    println!("open: A {ta:.3} s, B {tb:.3} s");
    let names: Vec<String> = match only {
        Some(v) => vec![v.to_owned()],
        None => sa
            .dataset()
            .vars
            .iter()
            .filter(|v| v.dtype.is_numeric())
            .map(|v| v.name.clone())
            .collect(),
    };
    let (mut sum_a, mut sum_b, mut mb, mut bad) = (0.0, 0.0, 0.0, 0usize);
    for name in &names {
        let (ga, gb) = (sa.chunk_grid(name)?, sb.chunk_grid(name)?);
        let (ca, da) = decode_all(sa.as_ref(), name)?;
        let (cb, db) = decode_all(sb.as_ref(), name)?;
        let same = if ga == gb {
            ca.iter().zip(&cb).all(|(x, y)| {
                x.origin == y.origin
                    && x.shape == y.shape
                    && value_bits(&x.values) == value_bits(&y.values)
            })
        } else {
            ga.shape == gb.shape && assemble(&ca, &ga.shape) == assemble(&cb, &gb.shape)
        };
        let vmb = ga.shape.iter().product::<usize>() as f64
            * sa.dataset().var(name).map_or(4, |v| v.dtype.size()) as f64
            / 1e6;
        println!(
            "  {name:<16} shape {:?} chunks A {:?} B {:?}: {}  A {da:.3} s  B {db:.3} s  ({:?} | {:?})",
            ga.shape,
            ga.chunk_shape,
            gb.chunk_shape,
            if same { "equal" } else { "DIFFERENT" },
            sa.codecs(name),
            sb.codecs(name),
        );
        bad += usize::from(!same);
        sum_a += da;
        sum_b += db;
        mb += vmb;
    }
    println!(
        "total {mb:.1} MB decoded: A {sum_a:.3} s ({:.0} MB/s), B {sum_b:.3} s ({:.0} MB/s); {} of {} variables differ",
        mb / sum_a,
        mb / sum_b,
        bad,
        names.len()
    );
    if bad > 0 {
        return Err("values differ".into());
    }
    Ok(())
}
