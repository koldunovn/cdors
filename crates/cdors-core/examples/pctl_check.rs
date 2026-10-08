//! Check program for `kernels::percentile`: compares the exact percentiles of cdors with cdo's
//! `monpctl` and `timpctl` for every `--percentile` method, then times the batched kernel.
//!
//! ```text
//! pctl_check [SCRATCH_DIR] [--no-bench | --bench-only]
//! ```
//!
//! For each fixture in `$CDORS_FIXTURES` (`r36x18_std`, `hpz2_noleap`, `unst_360`), each method and
//! each `p` in {1, 10, 50, 90, 95, 99, 100}, cdo writes
//! `cdo -s --percentile <m> {mon,tim}pctl,<p> in -{mon,tim}min in -{mon,tim}max in` to
//! `SCRATCH_DIR/ref` (default `/work/ab0995/a270088/cdors-target-pctl/scratch/pctl`; existing
//! files are reused, never removed). For `tas` and `pr`:
//! - `monpctl` (28-31 values per group, so cdo is exact): every value must equal ours to float32
//!   rounding (at most 1 ulp) with identical missing values. Where cdo reads one past the end of
//!   its buffer (`cdo_reads_past_end`; continuous NumPy variants near p = 100) its value is no
//!   percentile; those points are counted separately ("oob") and not required to match.
//! - `timpctl` (about 1095 values, cdo uses a 101-bin histogram): cells with at most 50 valid values
//!   must match as above; elsewhere the difference must not exceed the bin width
//!   `(timmax - timmin) / 101` of the cell (plus 2 ulp of rounding) for `nrank`, cdo's default. For
//!   the other methods the histogram ignores the method, so the ratio is only reported.
//!
//! The benchmark computes percentiles of a synthetic 65536 cells × 1096 steps float32 tile (1 %
//! missing) on 1 and 16 threads, in both layouts.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use cdors_core::kernels::percentile::{
    Layout, PercentileMethod, cdo_reads_past_end, percentiles_tile,
};
use cdors_core::model::TimeAxis;
use cdors_core::model::timegroup::{GroupTracker, Member, Period, TimestatDate, period_axis};
use rayon::prelude::*;

type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const FIXTURES: [&str; 3] = ["r36x18_std", "hpz2_noleap", "unst_360"];
const VARS: [&str; 2] = ["tas", "pr"];
const PS: [f64; 7] = [1.0, 10.0, 50.0, 90.0, 95.0, 99.0, 100.0];
/// cdo keeps raw values (exact percentiles) up to this many per cell.
const CDO_EXACT_MAX: usize = 50;
const CDO_NBINS: f32 = 101.0;

fn cdo() -> String {
    std::env::var("CDO").unwrap_or_else(|_| "cdo".into())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Mon,
    Tim,
}

impl Op {
    fn name(self) -> &'static str {
        match self {
            Op::Mon => "mon",
            Op::Tim => "tim",
        }
    }
}

fn ref_path(dir: &Path, fx: &str, op: Op, m: PercentileMethod, p: f64) -> PathBuf {
    dir.join(format!("{fx}_{}pctl_{}_{p}.nc", op.name(), m.name()))
}

/// Runs cdo for one reference file unless it exists (written to a temporary name, then renamed).
fn make_ref(input: &Path, out: &Path, op: Op, m: PercentileMethod, p: f64) -> Res<()> {
    if out.exists() {
        return Ok(());
    }
    let tmp = out.with_extension("nc.tmp");
    let i = input.to_str().ok_or("path")?;
    let o = op.name();
    let res = Command::new(cdo())
        .args(["-s", "-O", "--percentile", m.name()])
        .arg(format!("{o}pctl,{p}"))
        .args([i, &format!("-{o}min"), i, &format!("-{o}max"), i])
        .arg(&tmp)
        .output()?;
    if !res.status.success() {
        return Err(format!(
            "cdo {o}pctl,{p} --percentile {}: {}",
            m.name(),
            String::from_utf8_lossy(&res.stderr)
        )
        .into());
    }
    std::fs::rename(&tmp, out)?;
    Ok(())
}

/// Reads the time axis of a NetCDF file.
fn read_axis(path: &Path) -> Res<TimeAxis> {
    let file = netcdf::open(path)?;
    let var = file.variable("time").ok_or("no variable 'time'")?;
    let att = |name: &str| -> Option<String> {
        match var.attribute_value(name)?.ok()? {
            netcdf::AttributeValue::Str(s) => Some(s),
            _ => None,
        }
    };
    let units = att("units").ok_or("time has no units")?;
    let calendar = att("calendar");
    let values: Vec<f64> = var.get_values::<f64, _>(..)?;
    Ok(TimeAxis::decode(
        "time",
        "time",
        &units,
        calendar.as_deref(),
        &values,
        None,
    )?)
}

/// A float variable as `(nsteps, ncells, values)` in time-major order; missing values become NaN.
fn read_var(path: &Path, name: &str) -> Res<(usize, usize, Vec<f32>)> {
    let file = netcdf::open(path)?;
    let var = file
        .variable(name)
        .ok_or_else(|| format!("no variable '{name}' in {}", path.display()))?;
    let dims: Vec<usize> = var.dimensions().iter().map(|d| d.len()).collect();
    let nsteps = dims[0];
    let ncells: usize = dims[1..].iter().product();
    let mut v: Vec<f32> = var.get_values::<f32, _>(..)?;
    let mut missing = Vec::new();
    for a in ["missing_value", "_FillValue"] {
        match var.attribute_value(a).and_then(|r| r.ok()) {
            Some(netcdf::AttributeValue::Float(x)) => missing.push(x),
            Some(netcdf::AttributeValue::Double(x)) => missing.push(x as f32),
            _ => {}
        }
    }
    for x in &mut v {
        if missing.contains(x) {
            *x = f32::NAN;
        }
    }
    Ok((nsteps, ncells, v))
}

/// Distance in float32 units in the last place.
fn ulps(a: f32, b: f32) -> i64 {
    let key = |x: f32| {
        let i = x.to_bits() as i32;
        i64::from(if i < 0 { i32::MIN - i } else { i })
    };
    (key(a) - key(b)).abs()
}

#[derive(Default)]
struct MonStats {
    n: usize,
    exact: usize,
    ulp1: usize,
    oob: usize,
    bad: usize,
    max_bad: f64,
}

#[derive(Default)]
struct TimStats {
    exact_cells: usize,
    exact_bad: usize,
    max_diff: f64,
    max_ratio: f64,
    over_bin: usize,
}

struct Input {
    nsteps: usize,
    ncells: usize,
    data: Vec<f32>,
}

impl Input {
    fn valid_count(&self, first: usize, count: usize, cell: usize) -> usize {
        (first..first + count)
            .filter(|t| !self.data[t * self.ncells + cell].is_nan())
            .count()
    }
}

fn check_mon(
    dir: &Path,
    fx: &str,
    var: &str,
    inp: &Input,
    groups: &[(usize, usize)],
    m: PercentileMethod,
) -> Res<MonStats> {
    let nc = inp.ncells;
    let np = PS.len();
    // ours[g][cell * np + k]
    let ours: Vec<Vec<f64>> = groups
        .iter()
        .map(|&(first, count)| {
            let mut out = vec![0.0; nc * np];
            let tile = &inp.data[first * nc..(first + count) * nc];
            percentiles_tile(tile, nc, count, Layout::TimeMajor, &PS, m, &mut out);
            out
        })
        .collect();
    let mut st = MonStats::default();
    for (k, &p) in PS.iter().enumerate() {
        let (ng, nc2, refv) = read_var(&ref_path(dir, fx, Op::Mon, m, p), var)?;
        if ng != groups.len() || nc2 != nc {
            return Err(format!("{fx} {var}: shape {ng}x{nc2} != {}x{nc}", groups.len()).into());
        }
        for (g, &(first, count)) in groups.iter().enumerate() {
            for c in 0..nc {
                let r = refv[g * nc + c];
                let o = ours[g][c * np + k] as f32;
                st.n += 1;
                if r.is_nan() || o.is_nan() {
                    if r.is_nan() && o.is_nan() {
                        st.exact += 1;
                    } else {
                        st.bad += 1;
                        st.max_bad = f64::INFINITY;
                    }
                    continue;
                }
                match ulps(r, o) {
                    0 => st.exact += 1,
                    1 => st.ulp1 += 1,
                    _ if cdo_reads_past_end(inp.valid_count(first, count, c), p, m) => st.oob += 1,
                    _ => {
                        st.bad += 1;
                        st.max_bad = st.max_bad.max((f64::from(r) - f64::from(o)).abs());
                    }
                }
            }
        }
    }
    Ok(st)
}

fn check_tim(dir: &Path, fx: &str, var: &str, inp: &Input, m: PercentileMethod) -> Res<TimStats> {
    let (nt, nc) = (inp.nsteps, inp.ncells);
    let np = PS.len();
    let mut ours = vec![0.0; nc * np];
    percentiles_tile(&inp.data, nc, nt, Layout::TimeMajor, &PS, m, &mut ours);
    // Per cell: valid count, min, max.
    let mut cnt = vec![0usize; nc];
    let mut lo = vec![f32::INFINITY; nc];
    let mut hi = vec![f32::NEG_INFINITY; nc];
    for t in 0..nt {
        for c in 0..nc {
            let v = inp.data[t * nc + c];
            if !v.is_nan() {
                cnt[c] += 1;
                lo[c] = lo[c].min(v);
                hi[c] = hi[c].max(v);
            }
        }
    }
    let mut st = TimStats::default();
    for (k, &p) in PS.iter().enumerate() {
        let (_, _, refv) = read_var(&ref_path(dir, fx, Op::Tim, m, p), var)?;
        for c in 0..nc {
            let r = refv[c];
            let o = ours[c * np + k] as f32;
            if r.is_nan() || o.is_nan() || cnt[c] <= CDO_EXACT_MAX {
                st.exact_cells += 1;
                let same = (r.is_nan() && o.is_nan())
                    || (!r.is_nan() && !o.is_nan() && ulps(r, o) <= 1)
                    || (!r.is_nan() && cdo_reads_past_end(cnt[c], p, m));
                if !same {
                    st.exact_bad += 1;
                }
                continue;
            }
            // cdo's bin width, computed in float as in histDefBounds.
            let step = (hi[c] - lo[c]) / CDO_NBINS;
            let diff = (f64::from(r) - f64::from(o)).abs();
            let tol =
                f64::from(step) + 2.0 * f64::from(f32::EPSILON) * f64::from(r.abs().max(o.abs()));
            st.max_diff = st.max_diff.max(diff);
            if step > 0.0 {
                st.max_ratio = st.max_ratio.max(diff / f64::from(step));
            }
            if diff > tol {
                st.over_bin += 1;
            }
        }
    }
    Ok(st)
}

fn bench() {
    const NC: usize = 65536;
    const NT: usize = 1096;
    println!("\nbenchmark: synthetic tile {NC} cells x {NT} steps, float32, 1 % missing");
    let mut s: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    let time_major: Vec<f32> = (0..NC * NT)
        .map(|_| {
            let r = next();
            if r % 100 == 0 {
                f32::NAN
            } else {
                250.0 + (r >> 40) as f32 / (1u64 << 24) as f32 * 60.0
            }
        })
        .collect();
    let mut cell_major = vec![0.0f32; NC * NT];
    for t in 0..NT {
        for c in 0..NC {
            cell_major[c * NT + t] = time_major[t * NC + c];
        }
    }
    println!(
        "{:<11} {:<8} {:>8} {:>12} {:>12} {:>8}",
        "layout", "method", "pcts", "1 thread s", "16 threads s", "speedup"
    );
    for (layout, data) in [
        (Layout::TimeMajor, &time_major),
        (Layout::CellMajor, &cell_major),
    ] {
        for (m, ps) in [
            (PercentileMethod::Nrank, &PS[3..4]),
            (PercentileMethod::Nrank, &PS[..]),
            (PercentileMethod::Nist, &PS[..]),
        ] {
            let mut times = [0.0; 2];
            let mut results: Vec<Vec<f64>> = Vec::new();
            for (i, threads) in [1usize, 16].into_iter().enumerate() {
                let pool = rayon::ThreadPoolBuilder::new()
                    .num_threads(threads)
                    .build()
                    .expect("thread pool");
                let mut out = vec![0.0; NC * ps.len()];
                let t0 = Instant::now();
                pool.install(|| percentiles_tile(data, NC, NT, layout, ps, m, &mut out));
                times[i] = t0.elapsed().as_secs_f64();
                results.push(out);
            }
            assert!(
                results[0]
                    .iter()
                    .zip(&results[1])
                    .all(|(a, b)| a.to_bits() == b.to_bits()),
                "results depend on the thread count"
            );
            println!(
                "{:<11} {:<8} {:>8} {:>12.3} {:>12.3} {:>8.1}",
                format!("{layout:?}"),
                m.name(),
                ps.len(),
                times[0],
                times[1],
                times[0] / times[1]
            );
        }
    }
}

fn main() -> Res<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |f: &str| args.iter().any(|a| a == f);
    if !flag("--bench-only") {
        let dir = args
            .iter()
            .find(|a| !a.starts_with("--"))
            .map(PathBuf::from)
            .unwrap_or_else(|| "/work/ab0995/a270088/cdors-target-pctl/scratch/pctl".into())
            .join("ref");
        std::fs::create_dir_all(&dir)?;
        let fixtures = PathBuf::from(std::env::var("CDORS_FIXTURES")?);
        check(&fixtures, &dir)?;
    }
    if !flag("--no-bench") {
        bench();
    }
    Ok(())
}

fn check(fixtures: &Path, dir: &Path) -> Res<()> {
    // cdo references, 5 at a time (each cdo chain runs a few threads).
    let mut jobs = Vec::new();
    for fx in FIXTURES {
        for op in [Op::Mon, Op::Tim] {
            for m in PercentileMethod::ALL {
                for p in PS {
                    jobs.push((fx, op, m, p));
                }
            }
        }
    }
    let pool = rayon::ThreadPoolBuilder::new().num_threads(5).build()?;
    let t0 = Instant::now();
    pool.install(|| {
        jobs.par_iter().try_for_each(|&(fx, op, m, p)| {
            let input = fixtures.join(format!("{fx}.nc"));
            make_ref(&input, &ref_path(dir, fx, op, m, p), op, m, p)
        })
    })?;
    eprintln!(
        "cdo references ready ({} files, {:.1} s)",
        jobs.len(),
        t0.elapsed().as_secs_f64()
    );

    println!(
        "{:<12} {:<4} {:<26} | {:>6} {:>6} {:>5} {:>4} {:>4} | {:>5} {:>4} {:>10} {:>6} {:>4}",
        "fixture",
        "var",
        "method",
        "mon:n",
        "exact",
        "1ulp",
        "oob",
        "BAD",
        "tim:x",
        "BAD",
        "max|diff|",
        "/bin",
        ">bin"
    );
    let mut failures = 0usize;
    for fx in FIXTURES {
        let path = fixtures.join(format!("{fx}.nc"));
        let axis = read_axis(&path)?;
        let members: Vec<Member> = axis.steps.iter().map(|s| Member::new(s, None)).collect();
        let tracker = GroupTracker::new(Period::Month, axis.calendar, TimestatDate::Middle);
        let groups: Vec<(usize, usize)> = period_axis(tracker, &members)
            .iter()
            .map(|g| (g.first, g.count))
            .collect();
        for var in VARS {
            let (nsteps, ncells, data) = read_var(&path, var)?;
            let inp = Input {
                nsteps,
                ncells,
                data,
            };
            for m in PercentileMethod::ALL {
                let ms = check_mon(dir, fx, var, &inp, &groups, m)?;
                let ts = check_tim(dir, fx, var, &inp, m)?;
                let tim_must = m == PercentileMethod::Nrank;
                let fail = ms.bad > 0 || ts.exact_bad > 0 || (tim_must && ts.over_bin > 0);
                failures += usize::from(fail);
                println!(
                    "{:<12} {:<4} {:<26} | {:>6} {:>6} {:>5} {:>4} {:>4} | {:>5} {:>4} {:>10.3e} {:>6.3} {:>4}{}",
                    fx,
                    var,
                    m.name(),
                    ms.n,
                    ms.exact,
                    ms.ulp1,
                    ms.oob,
                    ms.bad,
                    ts.exact_cells,
                    ts.exact_bad,
                    ts.max_diff,
                    ts.max_ratio,
                    ts.over_bin,
                    if fail { "  FAIL" } else { "" }
                );
                if ms.bad > 0 && ms.max_bad.is_finite() {
                    println!("    monpctl max |diff| of mismatches: {:.3e}", ms.max_bad);
                }
            }
        }
    }
    println!(
        "\nmon: values compared, exact, 1 ulp, cdo out-of-bounds read (not compared), mismatches;\n\
         tim: values with <= {CDO_EXACT_MAX} valid inputs or missing (must match), their mismatches, \
         max |diff| and max diff / bin width elsewhere, values beyond one bin (must be 0 for nrank)"
    );
    if failures > 0 {
        return Err(format!("{failures} rows failed").into());
    }
    println!("all rows pass");
    Ok(())
}
