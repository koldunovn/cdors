//! Check program for `model::timegroup`: compares the output time axis (timestamps and
//! `time_bnds`) computed by cdors with that of cdo's actual output.
//!
//! ```text
//! timegroup_check [SCRATCH_DIR]
//! ```
//!
//! Inputs: the fixtures in `$CDORS_FIXTURES` (standard, 365_day, 360_day calendars) plus small
//! time-axis files made with `$CDO` in `SCRATCH_DIR/inputs` (default
//! `/work/ab0995/a270088/cdors-target-tg/scratch/timegroup`): half-hourly data across a month
//! boundary, monthly data starting in January, 12-hourly data over 29 February in the standard
//! and proleptic_gregorian calendars, 6-hourly 360_day data, and daily data with time bounds.
//! For every input, operator and `--timestat_date` value (and the operator default) cdo writes
//! its result to `SCRATCH_DIR/out`; its timestamps (`cdo -s showtimestamp`) and its `time_bnds`
//! (read through netCDF) must equal what `timegroup` computes from the input time axis.
//! Existing files are reused, never removed.

use std::path::{Path, PathBuf};
use std::process::Command;

use cdors_core::model::timegroup::{
    ClimTracker, Climatology, GroupTracker, Member, Period, SeasonStart, TimestatDate, period_axis,
    run_axis,
};
use cdors_core::model::{CalDateTime, Calendar, TimeAxis, TimeUnit, TimeUnits};
use rayon::prelude::*;

type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone, Copy)]
enum Kind {
    Period(Period),
    Clim(Climatology),
    Run(usize),
}

#[derive(Clone)]
struct Case {
    /// Operator as given to cdo (with arguments).
    op: &'static str,
    kind: Kind,
    /// Extra environment for cdo, mirrored in the cdors computation.
    env: &'static [(&'static str, &'static str)],
    /// Extra cdo options (before the operator).
    opts: &'static [&'static str],
    /// Percentile form: `op in -yearmin in -yearmax in`.
    pctl: Option<(&'static str, &'static str)>,
    label: &'static str,
}

const fn case(op: &'static str, kind: Kind) -> Case {
    Case {
        op,
        kind,
        env: &[],
        opts: &[],
        pctl: None,
        label: "",
    }
}

fn cases() -> Vec<Case> {
    use Climatology as C;
    use Period as P;
    vec![
        case("hourmean", Kind::Period(P::Hour)),
        case("daymean", Kind::Period(P::Day)),
        case("monmean", Kind::Period(P::Month)),
        case("seasmean", Kind::Period(P::Season)),
        Case {
            env: &[("CDO_SEASON_START", "JAN")],
            label: "seasmean JAN",
            ..case("seasmean", Kind::Period(P::Season))
        },
        case("yearmean", Kind::Period(P::Year)),
        case("timmean", Kind::Period(P::All)),
        case("ymonmean", Kind::Clim(C::Month)),
        case("ydaymean", Kind::Clim(C::Day)),
        case("yseasmean", Kind::Clim(C::Season)),
        Case {
            env: &[("CDO_SEASON_START", "JAN")],
            label: "yseasmean JAN",
            ..case("yseasmean", Kind::Clim(C::Season))
        },
        case("runmean,5", Kind::Run(5)),
        Case {
            pctl: Some(("-yearmin", "-yearmax")),
            ..case("yearpctl,90", Kind::Period(P::Year))
        },
        Case {
            pctl: Some(("-monmin", "-monmax")),
            ..case("monpctl,90", Kind::Period(P::Month))
        },
        Case {
            pctl: Some(("-timmin", "-timmax")),
            ..case("timpctl,90", Kind::Period(P::All))
        },
        Case {
            env: &[("CDO_TIMESTAT_DATE", "first")],
            label: "monmean env=first",
            ..case("monmean", Kind::Period(P::Month))
        },
        Case {
            opts: &["--use_time_bounds"],
            label: "daymean use_tb",
            ..case("daymean", Kind::Period(P::Day))
        },
        Case {
            opts: &["--use_time_bounds"],
            label: "monmean use_tb",
            ..case("monmean", Kind::Period(P::Month))
        },
    ]
}

const STATS: [Option<&str>; 5] = [
    None,
    Some("first"),
    Some("middle"),
    Some("midhigh"),
    Some("last"),
];

fn cdo() -> String {
    std::env::var("CDO").unwrap_or_else(|_| "cdo".into())
}

fn run_cdo(args: &[&str], env: &[(&str, &str)]) -> Res<String> {
    let out = Command::new(cdo())
        .args(args)
        .envs(env.iter().copied())
        .output()?;
    if !out.status.success() {
        return Err(format!(
            "cdo {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(out.stdout)?)
}

/// Makes the extra time-axis inputs (each only if missing; written to a temporary name, then renamed).
fn make_inputs(dir: &Path) -> Res<Vec<PathBuf>> {
    std::fs::create_dir_all(dir)?;
    let specs: &[(&str, &[&str])] = &[
        (
            "hourly_std",
            &[
                "-settaxis,2001-01-30,00:30:00,30min",
                "-setcalendar,standard",
                "-for,1,240",
            ],
        ),
        (
            "mon_jan_std",
            &[
                "-settaxis,2001-01-16,00:00:00,1month",
                "-setcalendar,standard",
                "-for,1,40",
            ],
        ),
        (
            "leap_std",
            &[
                "-settaxis,1999-12-01,06:00:00,12hour",
                "-setcalendar,standard",
                "-for,1,2200",
            ],
        ),
        (
            "leap_prolep",
            &[
                "-settaxis,1999-12-01,06:00:00,12hour",
                "-setcalendar,proleptic_gregorian",
                "-for,1,2200",
            ],
        ),
        (
            "d360_6h",
            &[
                "-settaxis,2000-12-16,00:00:00,6hour",
                "-setcalendar,360_day",
                "-for,1,1500",
            ],
        ),
        (
            "bnds_std",
            &[
                "-settbounds,day",
                "-settaxis,2000-01-01,12:00:00,1day",
                "-setcalendar,standard",
                "-for,1,800",
            ],
        ),
        // daily values stamped at 00:00 with bounds [d, d+1]
        (
            "bnds_end_std",
            &[
                "-shifttime,1day",
                "-settbounds,day",
                "-settaxis,2000-01-01,00:00:00,1day",
                "-for,1,800",
            ],
        ),
    ];
    let mut paths = Vec::new();
    for (name, chain) in specs {
        let path = dir.join(format!("{name}.nc"));
        if !path.exists() {
            let tmp = dir.join(format!("{name}.nc.tmp{}", std::process::id()));
            let mut args = vec!["-s", "-f", "nc4"];
            args.extend_from_slice(chain);
            let t = tmp.to_string_lossy().into_owned();
            args.push(&t);
            run_cdo(&args, &[])?;
            std::fs::rename(&tmp, &path)?;
        }
        paths.push(path);
    }
    paths.push(make_end_stamped(dir)?);
    Ok(paths)
}

/// Daily values stamped at the end of their interval: time `d+1 00:00`, bounds `[d, d+1]`
/// (what `--use_time_bounds` is for). cdo cannot make this, so it is written through netCDF.
fn make_end_stamped(dir: &Path) -> Res<PathBuf> {
    let path = dir.join("tb_end_std.nc");
    if path.exists() {
        return Ok(path);
    }
    let tmp = dir.join(format!("tb_end_std.nc.tmp{}", std::process::id()));
    let n = 800;
    {
        let mut f = netcdf::create(&tmp)?;
        f.add_unlimited_dimension("time")?;
        f.add_dimension("bnds", 2)?;
        let t: Vec<f64> = (1..=n).map(f64::from).collect();
        let b: Vec<f64> = (1..=n)
            .flat_map(|i| [f64::from(i - 1), f64::from(i)])
            .collect();
        let mut v = f.add_variable::<f64>("time", &["time"])?;
        v.put_attribute("units", "days since 2000-01-01 00:00:00")?;
        v.put_attribute("calendar", "standard")?;
        v.put_attribute("bounds", "time_bnds")?;
        v.put_values(&t, ..)?;
        let mut vb = f.add_variable::<f64>("time_bnds", &["time", "bnds"])?;
        vb.put_values(&b, ..)?;
        let mut x = f.add_variable::<f32>("x", &["time"])?;
        x.put_values(&t.iter().map(|&v| v as f32).collect::<Vec<_>>(), ..)?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Reads the time axis (and bounds) of a NetCDF file.
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
    let bname = att("bounds");
    let bvals: Option<Vec<f64>> = match &bname {
        Some(b) => match file.variable(b) {
            Some(bv) => Some(bv.get_values::<f64, _>(..)?),
            None => None,
        },
        None => None,
    };
    let bounds = match (&bname, &bvals) {
        (Some(n), Some(v)) => Some((n.as_str(), v.as_slice())),
        _ => None,
    };
    Ok(TimeAxis::decode(
        "time",
        "time",
        &units,
        calendar.as_deref(),
        &values,
        bounds,
    )?)
}

type Axis = Vec<(CalDateTime, Option<[CalDateTime; 2]>)>;

/// What a timestamp reads back as after CDI wrote it in `months`/`years since` units
/// (`libcdi/src/taxis.c:datetime2rtimeval`, then `rtimeval2datetime`). This is the writer's job,
/// not the grouping's; the check applies it so that the grouping can be compared exactly.
fn cdi_units_roundtrip(t: CalDateTime, units: &TimeUnits, cal: Calendar) -> CalDateTime {
    let TimeUnits::Relative { unit, reference: r } = units else {
        return t;
    };
    if !matches!(unit, TimeUnit::Month | TimeUnit::Year) || cal == Calendar::Day360 {
        return t;
    }
    let mut value = ((t.year - r.year) * 12 - r.month as i32 + t.month as i32) as f64;
    let total = t.year as i64 * 12 + t.month as i64 - 1 - value as i64;
    let mut t2 = t;
    t2.year = total.div_euclid(12) as i32;
    t2.month = total.rem_euclid(12) as u32 + 1;
    let dpm = cal.days_in_month(t2.year, t2.month) as f64;
    let secs = t2.seconds_since(r, cal);
    value += (secs.div_euclid(86_400) as f64 + secs.rem_euclid(86_400) as f64 / 86_400.0) / dpm;
    if *unit == TimeUnit::Year {
        value /= 12.0;
    }
    units.decode(value, cal).expect("decode")
}

fn expected(input: &TimeAxis, c: &Case, stat: Option<&str>) -> Res<Axis> {
    let env = |k: &str| c.env.iter().find(|(n, _)| *n == k).map(|(_, v)| *v);
    let season = if env("CDO_SEASON_START") == Some("JAN") {
        SeasonStart::Jan
    } else {
        SeasonStart::Dec
    };
    let cli = stat.map(|s| TimestatDate::parse(s).expect("stat"));
    let env_stat = env("CDO_TIMESTAT_DATE").and_then(TimestatDate::parse);
    let pick = |default| cli.or(env_stat).unwrap_or(default);
    let cal = input.calendar;
    let members: Vec<Member> = input
        .steps
        .iter()
        .enumerate()
        .map(|(i, s)| Member::new(s, input.bounds.as_ref().map(|b| &b[i])))
        .collect();
    let groups: Vec<_> = match c.kind {
        Kind::Period(p) => {
            let tr = GroupTracker::new(p, cal, pick(Period::DEFAULT_TIMESTAT))
                .with_season_start(season)
                .with_use_time_bounds(c.opts.contains(&"--use_time_bounds"));
            period_axis(tr, &members)
        }
        Kind::Clim(k) => {
            let mut tr = ClimTracker::new(k, cal, pick(Climatology::DEFAULT_TIMESTAT))
                .with_season_start(season);
            for m in &members {
                tr.push_member(*m);
            }
            tr.finish().into_iter().map(|(_, g)| g).collect()
        }
        Kind::Run(n) => run_axis(n, &members, pick(Period::DEFAULT_TIMESTAT), cal)?,
    };
    Ok(groups
        .into_iter()
        .map(|g| (g.timestamp, g.bounds))
        .collect())
}

struct Job {
    input: usize,
    case: usize,
    stat: Option<&'static str>,
    out: PathBuf,
}

fn main() -> Res<()> {
    let scratch = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/work/ab0995/a270088/cdors-target-tg/scratch/timegroup"));
    let fixtures = PathBuf::from(std::env::var("CDORS_FIXTURES")?);
    let mut inputs: Vec<PathBuf> = ["r36x18_std", "hpz2_noleap", "unst_360"]
        .iter()
        .map(|n| fixtures.join(format!("{n}.nc")))
        .collect();
    for p in &inputs {
        if !p.exists() {
            return Err(format!("missing {} (run tests/make_fixtures.sh)", p.display()).into());
        }
    }
    inputs.extend(make_inputs(&scratch.join("inputs"))?);
    let outdir = scratch.join("out");
    std::fs::create_dir_all(&outdir)?;
    let cases = cases();

    let mut jobs = Vec::new();
    for (i, inp) in inputs.iter().enumerate() {
        let stem = inp.file_stem().unwrap().to_string_lossy().into_owned();
        for k in 0..cases.len() {
            for stat in STATS {
                let tag: String = format!("{}_{}_{}", stem, k, stat.unwrap_or("default"));
                jobs.push(Job {
                    input: i,
                    case: k,
                    stat,
                    out: outdir.join(format!("{tag}.nc")),
                });
            }
        }
    }

    // Run cdo in parallel (netCDF reading below stays serial: netCDF-C is not thread-safe).
    let pool = rayon::ThreadPoolBuilder::new().num_threads(8).build()?;
    let results: Vec<Res<String>> = pool.install(|| {
        jobs.par_iter()
            .map(|j| {
                let c = &cases[j.case];
                let inp = inputs[j.input].to_string_lossy().into_owned();
                let out = j.out.to_string_lossy().into_owned();
                let mut args: Vec<String> = vec!["-s".into()];
                args.extend(c.opts.iter().map(|s| s.to_string()));
                if let Some(s) = j.stat {
                    args.extend(["--timestat_date".into(), s.into()]);
                }
                args.push(c.op.into());
                args.push(inp.clone());
                if let Some((lo, hi)) = c.pctl {
                    args.extend([lo.into(), inp.clone(), hi.into(), inp.clone()]);
                }
                args.push(out.clone());
                let a: Vec<&str> = args.iter().map(String::as_str).collect();
                run_cdo(&a, c.env)?;
                run_cdo(&["-s", "showtimestamp", &out], &[])
            })
            .collect()
    });

    let axes: Vec<TimeAxis> = inputs.iter().map(|p| read_axis(p)).collect::<Res<_>>()?;
    let mut table: Vec<(String, String, Vec<&str>)> = Vec::new();
    let (mut pass, mut fail) = (0, 0);
    let mut details = Vec::new();
    for (j, res) in jobs.iter().zip(results) {
        let c = &cases[j.case];
        let verdict = (|| -> Res<()> {
            let shown = res?;
            let got_ts: Vec<&str> = shown.split_whitespace().collect();
            let got = read_axis(&j.out)?;
            let rt = |t| cdi_units_roundtrip(t, &got.units, got.calendar);
            let exp: Axis = expected(&axes[j.input], c, j.stat)?
                .into_iter()
                .map(|(t, b)| (rt(t), b.map(|[b0, b1]| [rt(b0), rt(b1)])))
                .collect();
            let exp_ts: Vec<String> = exp
                .iter()
                .map(|(t, _)| t.cdo_string().trim().to_owned())
                .collect();
            if got_ts != exp_ts {
                let k = got_ts
                    .iter()
                    .zip(&exp_ts)
                    .position(|(a, b)| a != b)
                    .unwrap_or(got_ts.len().min(exp_ts.len()));
                return Err(format!(
                    "timestamps: {} vs {} steps; first difference at {k}: cdo {:?} cdors {:?}",
                    got_ts.len(),
                    exp_ts.len(),
                    got_ts.get(k),
                    exp_ts.get(k)
                )
                .into());
            }
            let got_b: Vec<Option<[CalDateTime; 2]>> = match &got.bounds {
                Some(b) => b
                    .iter()
                    .map(|p| Some([p[0].datetime, p[1].datetime]))
                    .collect(),
                None => vec![None; got.len()],
            };
            for (k, ((_, eb), gb)) in exp.iter().zip(&got_b).enumerate() {
                if eb != gb {
                    let f = |b: &Option<[CalDateTime; 2]>| {
                        b.map_or("none".to_owned(), |b| {
                            format!("{}..{}", b[0].iso(), b[1].iso())
                        })
                    };
                    return Err(format!("bounds at {k}: cdo {} cdors {}", f(gb), f(eb)).into());
                }
            }
            Ok(())
        })();
        let stem = inputs[j.input]
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let label = if c.label.is_empty() { c.op } else { c.label };
        if table.last().map(|(s, l, _)| (s.as_str(), l.as_str())) != Some((stem.as_str(), label)) {
            table.push((stem.clone(), label.to_owned(), Vec::new()));
        }
        let cell = if verdict.is_ok() { "ok" } else { "FAIL" };
        table.last_mut().unwrap().2.push(cell);
        match verdict {
            Ok(()) => pass += 1,
            Err(e) => {
                fail += 1;
                details.push(format!(
                    "{stem} {label} {}: {}",
                    j.stat.unwrap_or("default"),
                    e.to_string().lines().collect::<Vec<_>>().join(" | ")
                ));
            }
        }
    }

    println!(
        "{:<14} {:<20} {:>8} {:>6} {:>7} {:>8} {:>5}",
        "input", "operator", "default", "first", "middle", "midhigh", "last"
    );
    for (stem, label, cells) in &table {
        print!("{stem:<14} {label:<20}");
        for (w, c) in [8, 6, 7, 8, 5].iter().zip(cells) {
            print!(" {c:>w$}");
        }
        println!();
    }
    for d in &details {
        println!("FAIL {d}");
    }
    println!("{pass} passed, {fail} failed");
    if fail > 0 {
        std::process::exit(1);
    }
    Ok(())
}
