//! Check `cdors_core::remap` against cdo: for each fixture × method × target, generate weights
//! through the cache, apply them to all timesteps of `tas` and `pr`, and compare with
//! `cdo remap,<target>,<weights.nc>`. Then time one larger case single- vs multi-threaded.
//!
//!   source env.sh; cargo run --release --example remap_check [-- --skip-timing]
//!
//! Needs $CDO, $CDORS_CACHE and $CDORS_FIXTURES. cdo reference outputs go to $CDORS_SCRATCH
//! (default `$CARGO_TARGET_DIR/scratch`) and are reused when present. Nothing is deleted.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use cdors_core::remap::{GenMethod, RemapWeights, WeightCache, WeightRequest};

type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Values of a variable as f32 with `_FillValue`/`missing_value` turned into NaN.
fn read_var(path: &Path, name: &str) -> Res<Option<Vec<f32>>> {
    let file = netcdf::open(path)?;
    let Some(v) = file.variable(name) else {
        return Ok(None);
    };
    let mut vals: Vec<f32> = v.get_values(..)?;
    for att in ["_FillValue", "missing_value"] {
        let fill = match v.attribute(att).map(|a| a.value()) {
            Some(Ok(netcdf::AttributeValue::Float(f))) => f,
            Some(Ok(netcdf::AttributeValue::Double(f))) => f as f32,
            _ => continue,
        };
        for x in vals.iter_mut().filter(|x| **x == fill) {
            *x = f32::NAN;
        }
    }
    Ok(Some(vals))
}

/// Distance in units of float32 ulp (via the ordered integer representation).
fn ulp_diff(a: f32, b: f32) -> u64 {
    let ord = |x: f32| {
        let i = x.to_bits() as i32;
        if i < 0 {
            i64::from(i32::MIN) - i64::from(i)
        } else {
            i64::from(i)
        }
    };
    (ord(a) - ord(b)).unsigned_abs()
}

fn cdo() -> PathBuf {
    std::env::var_os("CDO").map_or_else(|| PathBuf::from("cdo"), PathBuf::from)
}

fn run_cdo(args: &[&str]) -> Res<f64> {
    let t = Instant::now();
    let out = Command::new(cdo()).args(args).output()?;
    if !out.status.success() {
        return Err(format!(
            "cdo {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    Ok(t.elapsed().as_secs_f64())
}

struct Row {
    fixture: &'static str,
    method: &'static str,
    target: &'static str,
    max_abs: f64,
    max_ulp: u64,
    nan_mismatch: usize,
    nan_count: usize,
    values: usize,
}

fn check_case(
    cache: &WeightCache,
    scratch: &Path,
    fixture: &'static str,
    method: &'static str,
    target: &'static str,
) -> Res<Row> {
    let src = fixture_path(scratch, fixture)?;
    let m = GenMethod::parse(method).ok_or("bad method")?;
    let req = WeightRequest::new(m, target, &src);
    let wpath = cache.weights_for(&req)?;
    let w = RemapWeights::read(&wpath)?;

    let stem = wpath.file_stem().unwrap().to_string_lossy();
    let refp = scratch.join(format!("ref_{fixture}_{stem}.nc"));
    if !refp.is_file() {
        let tmp = scratch.join(format!("ref_{fixture}_{stem}.tmp{}.nc", std::process::id()));
        let op = format!("remap,{target},{}", wpath.display());
        run_cdo(&[
            "-s",
            "--force",
            "--no_history",
            &op,
            src.to_str().unwrap(),
            tmp.to_str().unwrap(),
        ])?;
        std::fs::rename(&tmp, &refp)?;
    }

    let mut row = Row {
        fixture,
        method,
        target,
        max_abs: 0.0,
        max_ulp: 0,
        nan_mismatch: 0,
        nan_count: 0,
        values: 0,
    };
    for var in ["tas", "pr"] {
        let Some(input) = read_var(&src, var)? else {
            continue;
        };
        let reference = read_var(&refp, var)?.ok_or("variable missing in reference")?;
        let mut out = vec![0f32; input.len() / w.src_size() * w.dst_size()];
        w.apply_batch(&input, &mut out)?;
        assert_eq!(out.len(), reference.len(), "{fixture} {var}: output size");
        for (&a, &b) in out.iter().zip(&reference) {
            row.values += 1;
            match (a.is_nan(), b.is_nan()) {
                (true, true) => row.nan_count += 1,
                (false, false) => {
                    row.max_abs = row.max_abs.max((f64::from(a) - f64::from(b)).abs());
                    row.max_ulp = row.max_ulp.max(ulp_diff(a, b));
                }
                _ => row.nan_mismatch += 1,
            }
        }
    }
    Ok(row)
}

/// Fixture file; `r36x18_miss` (regular grid with time-varying missing values, also near the
/// poles where cdo's bilinear fallback applies) is derived from `r36x18_std` into the scratch dir.
fn fixture_path(scratch: &Path, fixture: &str) -> Res<PathBuf> {
    let fixtures = PathBuf::from(std::env::var("CDORS_FIXTURES")?);
    if fixture != "r36x18_miss" {
        return Ok(fixtures.join(format!("{fixture}.nc")));
    }
    let path = scratch.join("r36x18_miss.nc");
    if !path.is_file() {
        let tmp = scratch.join(format!("r36x18_miss.tmp{}.nc", std::process::id()));
        let base = fixtures.join("r36x18_std.nc");
        run_cdo(&[
            "-s",
            "--no_history",
            "-setrtomiss,281,283",
            "-setrtomiss,5.0,5.4",
            base.to_str().unwrap(),
            tmp.to_str().unwrap(),
        ])?;
        std::fs::rename(&tmp, &path)?;
    }
    Ok(path)
}

fn timing(cache: &WeightCache, scratch: &Path) -> Res<()> {
    let fixtures = PathBuf::from(std::env::var("CDORS_FIXTURES")?);
    let src = fixtures.join("r36x18_std.nc");
    let target = "hpz5";
    let req = WeightRequest::new(GenMethod::Con, target, &src);
    let t = Instant::now();
    let wpath = cache.weights_for(&req)?;
    let t_gen = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let w = RemapWeights::read(&wpath)?;
    let t_read = t.elapsed().as_secs_f64();
    let input = read_var(&src, "tas")?.ok_or("tas")?;
    let nsteps = input.len() / w.src_size();
    let mut out = vec![0f32; nsteps * w.dst_size()];
    println!(
        "\ntiming: remapcon r36x18 -> {target} ({} -> {} cells, {} links), tas, {nsteps} steps",
        w.src_size(),
        w.dst_size(),
        w.num_links()
    );
    println!("  weights (cache lookup or cdo gencon): {t_gen:.3} s, read: {t_read:.3} s");
    for threads in [1usize, 4, 16] {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()?;
        let mut best = f64::INFINITY;
        for _ in 0..3 {
            let t = Instant::now();
            pool.install(|| w.apply_batch(&input, &mut out))?;
            best = best.min(t.elapsed().as_secs_f64());
        }
        let ops = (w.num_links() * nsteps) as f64;
        println!(
            "  apply_batch, {threads:>2} thread(s): {:.4} s  ({:.2} Glinks/s)",
            best,
            ops / best / 1e9
        );
        // Per-field path (parallel over destination rows within each field).
        let t = Instant::now();
        pool.install(|| -> Res<()> {
            for (s, d) in input.chunks(w.src_size()).zip(out.chunks_mut(w.dst_size())) {
                w.apply(s, d)?;
            }
            Ok(())
        })?;
        println!(
            "  apply per field, {threads:>2} thread(s): {:.4} s",
            t.elapsed().as_secs_f64()
        );
    }
    // The row-parallel path of `apply` (used for fields with many links) must give the same bits
    // as the field-parallel batch path; hpz5 is below its threshold, hpz7 is above it.
    let req7 = WeightRequest::new(GenMethod::Con, "hpz7", &src);
    let w7 = RemapWeights::read(&cache.weights_for(&req7)?)?;
    let n7 = 40;
    let input7 = &input[..n7 * w7.src_size()];
    let mut batch = vec![0f32; n7 * w7.dst_size()];
    let mut per_field = vec![0f32; n7 * w7.dst_size()];
    w7.apply_batch(input7, &mut batch)?;
    let t = Instant::now();
    for (s, d) in input7
        .chunks(w7.src_size())
        .zip(per_field.chunks_mut(w7.dst_size()))
    {
        w7.apply(s, d)?;
    }
    let t7 = t.elapsed().as_secs_f64();
    let same = batch
        .iter()
        .zip(&per_field)
        .all(|(a, b)| a.to_bits() == b.to_bits());
    println!(
        "  hpz7 ({} links): row-parallel apply of {n7} fields {t7:.4} s, bit-identical to apply_batch: {same}",
        w7.num_links()
    );
    if !same {
        return Err("row-parallel apply differs from apply_batch".into());
    }

    // cdo output for the timing run (overwritten on every run).
    let refp = scratch.join("timing_cdo_remap_hpz5.nc");
    let op = format!("remap,{target},{}", wpath.display());
    let t_cdo = run_cdo(&[
        "-s",
        "--force",
        "--no_history",
        "-P",
        "1",
        &op,
        "-selname,tas",
        src.to_str().unwrap(),
        refp.to_str().unwrap(),
    ])?;
    println!("  cdo -P 1 remap (whole process incl. I/O): {t_cdo:.3} s");
    Ok(())
}

fn main() -> Res<()> {
    let cache = WeightCache::from_env()?;
    let scratch = std::env::var_os("CDORS_SCRATCH").map_or_else(
        || PathBuf::from(std::env::var_os("CARGO_TARGET_DIR").unwrap_or_default()).join("scratch"),
        PathBuf::from,
    );
    std::fs::create_dir_all(&scratch)?;
    println!("weight cache: {}", cache.dir().display());

    // bil only where cdo supports it (regular and HEALPix sources).
    let cases: &[(&str, &[&str])] = &[
        ("r36x18_std", &["nn", "dis", "bil", "con"]),
        ("r36x18_2lev", &["nn", "dis", "bil", "con"]),
        ("r36x18_miss", &["nn", "dis", "bil", "con"]),
        ("hpz2_noleap", &["nn", "dis", "bil", "con"]),
        ("unst_360", &["nn", "dis", "con"]),
    ];
    let targets = ["r18x9", "hpz1", "r72x36"];
    println!(
        "\n{:<12} {:<4} {:<7} {:>10} {:>5} {:>8} {:>8} {:>8}",
        "fixture", "meth", "target", "max_abs", "ulp", "nan_ok", "nan_bad", "values"
    );
    let mut fail = false;
    for (fixture, methods) in cases {
        for method in *methods {
            for target in targets {
                match check_case(&cache, &scratch, fixture, method, target) {
                    Ok(r) => {
                        let bad = r.max_ulp > 2 || r.nan_mismatch > 0;
                        fail |= bad;
                        println!(
                            "{:<12} {:<4} {:<7} {:>10.3e} {:>5} {:>8} {:>8} {:>8}{}",
                            r.fixture,
                            r.method,
                            r.target,
                            r.max_abs,
                            r.max_ulp,
                            r.nan_count,
                            r.nan_mismatch,
                            r.values,
                            if bad { "  FAIL" } else { "" }
                        );
                    }
                    Err(e) => {
                        fail = true;
                        println!("{fixture:<12} {method:<4} {target:<7} ERROR {e}");
                    }
                }
            }
        }
    }
    // Early, clear error for bil on an unstructured source.
    let fixtures = PathBuf::from(std::env::var("CDORS_FIXTURES")?);
    let unst = fixtures.join("unst_360.nc");
    match cache.weights_for(&WeightRequest::new(GenMethod::Bil, "r18x9", &unst)) {
        Err(e) => println!("\nbil on unst_360 refused as expected: {e}"),
        Ok(_) => {
            fail = true;
            println!("\nbil on unst_360 unexpectedly produced weights");
        }
    }

    if !std::env::args().any(|a| a == "--skip-timing") {
        timing(&cache, &scratch)?;
    }
    if fail {
        return Err("some cases failed".into());
    }
    println!("\nall cases within 2 float32 ulp, missing-value patterns identical");
    Ok(())
}
