//! Check `cdors_core::model::area` against cdo: cell areas (`cdo gridarea`), cell weights
//! (`cdo gridweights`), and statistics of `tas` at the first timestep computed with our weights
//! and rows: `fldmean`, `fldstd`, `zonmean`, `mermean`, `vertmean` (`cdo -s outputf`).
//!
//!   source env.sh; cargo run --release --example area_check
//!
//! Inputs: the fixtures in $CDORS_FIXTURES plus extra grids in $CDORS_SCRATCH (default
//! `$CARGO_TARGET_DIR/scratch`), which this program makes with cdo when missing (Gaussian F16,
//! curvilinear with and without bounds, unstructured without bounds, a `cell_measures` area
//! variable, zonal and meridional means). cdo outputs go to `$CDORS_SCRATCH/area_ref/` and are
//! reused when present. Nothing is deleted. Exit status 1 if a comparison exceeds its tolerance
//! (areas and weights: relative 1e-12; statistics: 2 float32 ulp).

use std::path::{Path, PathBuf};
use std::process::Command;

use cdors_core::model::area::{self, WeightedSums};
use cdors_core::model::{Dataset, Grid, Variable};

type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn cdo() -> PathBuf {
    std::env::var_os("CDO").map_or_else(|| PathBuf::from("cdo"), PathBuf::from)
}

/// Runs cdo; returns (success, stdout, stderr).
fn run_cdo(args: &[&str], env: &[(&str, &str)]) -> Res<(bool, String, String)> {
    let mut c = Command::new(cdo());
    c.args(args);
    for (k, v) in env {
        c.env(k, v);
    }
    let out = c.output()?;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

/// Makes `out` with cdo unless it exists (written under a temporary name, then renamed).
fn make(out: &Path, args: &[&str], env: &[(&str, &str)]) -> Res<()> {
    if out.exists() {
        return Ok(());
    }
    let tmp = out.with_extension(format!("tmp{}.nc", std::process::id()));
    let mut a: Vec<&str> = vec!["-s", "-f", "nc4"];
    a.extend_from_slice(args);
    let t = tmp.to_string_lossy().into_owned();
    a.push(&t);
    let (ok, _, err) = run_cdo(&a, env)?;
    if !ok {
        return Err(format!("cdo {} failed: {err}", a.join(" ")).into());
    }
    std::fs::rename(&tmp, out)?;
    Ok(())
}

/// Reads a variable as f64 with `_FillValue`/`missing_value` as NaN.
fn read_nc(path: &Path, name: &str) -> Res<Vec<f64>> {
    let file = netcdf::open(path)?;
    let v = file
        .variable(name)
        .ok_or(format!("{name} not in {}", path.display()))?;
    let mut vals: Vec<f64> = v.get_values(..)?;
    for att in ["_FillValue", "missing_value"] {
        let fill = match v.attribute(att).map(|a| a.value()) {
            Some(Ok(netcdf::AttributeValue::Float(f))) => f64::from(f),
            Some(Ok(netcdf::AttributeValue::Double(f))) => f,
            _ => continue,
        };
        // compare in f32 for float variables, as cdo does
        for x in vals.iter_mut().filter(|x| **x as f32 == fill as f32) {
            *x = f64::NAN;
        }
    }
    Ok(vals)
}

/// Distance in float32 ulp (NaN = NaN is 0, NaN vs number is u64::MAX).
fn ulp(a: f64, b: f64) -> u64 {
    let (a, b) = (a as f32, b as f32);
    if a.is_nan() || b.is_nan() {
        return if a.is_nan() && b.is_nan() {
            0
        } else {
            u64::MAX
        };
    }
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

fn max_rel(a: &[f64], b: &[f64]) -> f64 {
    if a.len() != b.len() {
        return f64::INFINITY;
    }
    a.iter()
        .zip(b)
        .map(|(x, y)| {
            if *y == 0.0 {
                (x - y).abs()
            } else {
                ((x - y) / y).abs()
            }
        })
        .fold(0.0, f64::max)
}

/// cdo statistic of tas at timestep 1: values printed by `outputf,%.17g` (missing → NaN).
fn cdo_stat(
    path: &Path,
    op: &str,
    has_time: bool,
    refdir: &Path,
    label: &str,
) -> Res<Option<Vec<f64>>> {
    let cache = refdir.join(format!(
        "{label}.{}.txt",
        op.replace(',', "_").replace('=', "-")
    ));
    let text = if cache.exists() {
        std::fs::read_to_string(&cache)?
    } else {
        let p = path.to_string_lossy();
        let mut a = vec!["-s", "outputf,%.17g,1", op];
        if has_time {
            a.push("-seltimestep,1");
        }
        a.extend(["-selname,tas", &p]);
        let (ok, out, err) = run_cdo(&a, &[])?;
        if !ok {
            eprintln!(
                "  note: cdo {op} failed on {label}: {}",
                err.lines().last().unwrap_or("")
            );
            return Ok(None);
        }
        std::fs::write(&cache, &out)?;
        out
    };
    Ok(Some(
        text.split_whitespace()
            .map(|t| {
                t.parse::<f64>()
                    .map(|v| if (v as f32) == -9e33f32 { f64::NAN } else { v })
            })
            .collect::<Result<_, _>>()?,
    ))
}

fn max_ulp(ours: &[f64], cdo: &[f64]) -> u64 {
    if ours.len() != cdo.len() {
        return u64::MAX;
    }
    ours.iter()
        .zip(cdo)
        .map(|(a, b)| ulp(*a, *b))
        .max()
        .unwrap_or(0)
}

struct Case<'a> {
    label: &'a str,
    path: PathBuf,
}

#[derive(Default)]
struct Row {
    label: String,
    kind: String,
    area: String,
    weights: String,
    stats: Vec<(String, u64)>,
    ok: bool,
}

fn fmt_ulp(u: u64) -> String {
    if u == u64::MAX {
        "len/NaN mismatch".into()
    } else {
        u.to_string()
    }
}

/// Our statistic for each field (level) of tas at timestep 1.
fn our_stat(
    op: &str,
    fields: &[Vec<f64>],
    weights: &[f64],
    grid: &Grid,
    lw: Option<&area::LayerWeights>,
) -> cdors_core::error::Result<Vec<f64>> {
    let rows_stat = |rows: &area::Rows, w: &dyn Fn(usize) -> f64, f: &[f64]| -> Vec<f64> {
        (0..rows.len())
            .map(|r| {
                let mut s = WeightedSums::default();
                for &i in rows.row(r) {
                    s.add(w(i), f[i]);
                }
                s.mean()
            })
            .collect()
    };
    Ok(match op {
        "-fldmean" | "-fldstd" => fields
            .iter()
            .map(|f| {
                let mut s = WeightedSums::default();
                for (w, x) in weights.iter().zip(f) {
                    s.add(*w, *x);
                }
                if op == "-fldmean" { s.mean() } else { s.std() }
            })
            .collect(),
        "-zonmean" => {
            let rows = area::zonal_rows(grid)?;
            fields
                .iter()
                .flat_map(|f| rows_stat(&rows, &|_| 1.0, f))
                .collect()
        }
        "-mermean" => {
            let rows = area::meridional_columns(grid)?;
            fields
                .iter()
                .flat_map(|f| rows_stat(&rows, &|i| weights[i], f))
                .collect()
        }
        _ => {
            // vertmean: Σ w·x / Σ w over levels, per cell (Vertstat.cc:270-316)
            let lw = lw.expect("layer weights");
            let n = grid.size;
            (0..n)
                .map(|i| {
                    let mut s = WeightedSums::default();
                    for (l, f) in fields.iter().enumerate() {
                        s.add(lw.weights[l], f[i]);
                    }
                    s.mean()
                })
                .collect()
        }
    })
}

fn check(case: &Case, refdir: &Path) -> Res<Row> {
    let mut row = Row {
        label: case.label.to_owned(),
        ok: true,
        ..Default::default()
    };
    let p = case.path.to_string_lossy().into_owned();
    let src = cdors_core::io::open(&p)?;
    let ds: &Dataset = src.dataset();
    let var: &Variable = ds.var("tas").ok_or("no tas")?;
    let grid: &Grid = &ds.grids[var.grid.ok_or("tas has no grid")?];
    row.kind = format!("{} {}", grid.kind.name(), grid.size);
    let read = |n: &str| src.read_var(n);

    // areas
    let ga = refdir.join(format!("{}.gridarea.nc", case.label));
    let cdo_area = if ga.exists() {
        Some(read_nc(&ga, "cell_area")?)
    } else {
        let (ok, _, _) = run_cdo(&["-s", "gridarea", &p, &ga.to_string_lossy()], &[])?;
        if ok {
            Some(read_nc(&ga, "cell_area")?)
        } else {
            None
        }
    };
    let ours = area::cell_areas(ds, var, grid, &read);
    row.area = match (&ours, &cdo_area) {
        (Ok(a), Some(c)) => {
            // cdo 2.6.0 writes an area taken from the file as float32, although the operator
            // asks for float64 (Gridcell.cc:268): compare at that precision
            let file = matches!(a.source, area::AreaSource::File(_));
            let ours: Vec<f64> = if file {
                a.values.iter().map(|&x| f64::from(x as f32)).collect()
            } else {
                a.values.clone()
            };
            let r = max_rel(&ours, c);
            row.ok &= r <= 1e-12;
            format!(
                "{r:.1e} ({})",
                if file { "file, as f32" } else { "computed" }
            )
        }
        (Err(e), None) => format!("both fail ({})", e.code.as_str()),
        (Ok(_), None) => {
            row.ok = false;
            "cdo fails, we don't".into()
        }
        (Err(e), Some(_)) => {
            row.ok = false;
            format!("we fail: {}", e.message)
        }
    };

    // weights
    let gw = refdir.join(format!("{}.gridweights.nc", case.label));
    let gw_err = refdir.join(format!("{}.gridweights.stderr", case.label));
    if !gw.exists() {
        let (ok, _, err) = run_cdo(&["gridweights", &p, &gw.to_string_lossy()], &[])?;
        if !ok {
            return Err(format!("cdo gridweights {p}: {err}").into());
        }
        std::fs::write(&gw_err, err)?;
    }
    let cdo_w = read_nc(&gw, "cell_weights")?;
    let cdo_const = std::fs::read_to_string(&gw_err)
        .unwrap_or_default()
        .contains("constant grid cell area weights");
    let w = area::fld_weights(ds, var, grid, &read)?;
    let r = max_rel(&w.values, &cdo_w);
    let same_warn = w.constant() == cdo_const;
    row.ok &= r <= 1e-12 && same_warn;
    row.weights = format!(
        "{r:.1e}{}{}",
        if w.constant() { " const" } else { "" },
        if same_warn { "" } else { " WARNING MISMATCH" }
    );

    // statistics of tas at timestep 1
    let has_time = var.has_time();
    let vals = read_nc(&case.path, "tas")?;
    let n = grid.size;
    let zaxis = var.zaxis.map(|z| &ds.zaxes[z]);
    let nlev = zaxis.map_or(1, |z| z.len());
    let fields: Vec<Vec<f64>> = (0..nlev)
        .map(|l| vals[l * n..(l + 1) * n].to_vec())
        .collect();
    let lw = area::layer_weights(zaxis, nlev, true, false);
    let mut ops = vec!["-fldmean", "-fldstd"];
    if area::zonal_rows(grid).is_ok() && grid.xsize > 1 {
        ops.push("-zonmean");
    }
    if area::meridional_columns(grid).is_ok() && grid.ysize > 1 {
        ops.push("-mermean");
    }
    if nlev > 1 {
        ops.push("-vertmean");
    }
    for op in ops {
        let Some(c) = cdo_stat(&case.path, op, has_time, refdir, case.label)? else {
            continue;
        };
        let ours = our_stat(op, &fields, &w.values, grid, Some(&lw))?;
        let u = max_ulp(&ours, &c);
        row.ok &= u <= 2;
        row.stats.push((op.trim_start_matches('-').to_owned(), u));
    }
    if nlev > 1 {
        // generated layer bounds (vertmean,genbounds=true)
        let lwg = area::layer_weights(zaxis, nlev, true, true);
        if let Some(c) = cdo_stat(
            &case.path,
            "-vertmean,genbounds=true",
            has_time,
            refdir,
            case.label,
        )? {
            let ours = our_stat("-vertmean", &fields, &w.values, grid, Some(&lwg))?;
            let u = max_ulp(&ours, &c);
            row.ok &= u <= 2;
            row.stats.push(("vertmean,genbounds".into(), u));
        }
    }
    Ok(row)
}

fn main() -> Res<()> {
    let fix = PathBuf::from(std::env::var("CDORS_FIXTURES")?);
    let scratch = std::env::var_os("CDORS_SCRATCH").map_or_else(
        || {
            PathBuf::from(std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".into()))
                .join("scratch")
        },
        PathBuf::from,
    );
    let refdir = scratch.join("area_ref");
    std::fs::create_dir_all(&refdir)?;
    let s = |n: &str| scratch.join(n);
    let f = |n: &str| fix.join(n);
    let fs = |n: &str| f(n).to_string_lossy().into_owned();

    // extra grids
    let r36 = fs("r36x18_std.nc");
    make(
        &s("gauss_f16.nc"),
        &["-b", "F32", "-setname,tas", "-random,F16"],
        &[],
    )?;
    make(
        &s("curv_r36x18.nc"),
        &[
            "-b",
            "F32",
            "-setgridtype,curvilinear",
            "-seltimestep,1",
            &r36,
        ],
        &[],
    )?;
    make(
        &s("curv_r40x25.nc"),
        &[
            "-b",
            "F32",
            "-setgridtype,curvilinear",
            "-setname,tas",
            "-random,r40x25",
        ],
        &[],
    )?;
    let c40 = s("curv_r40x25.nc").to_string_lossy().into_owned();
    let nobnds = [("CDI_READ_CELL_CORNERS", "0")];
    make(&s("curv_nobnds.nc"), &["copy", &c40], &nobnds)?;
    make(
        &s("unst_nobnds.nc"),
        &["-seltimestep,1", &fs("unst_360.nc")],
        &nobnds,
    )?;
    make(
        &s("area2_r40x25.nc"),
        &[
            "-setname,cell_area",
            "-mul",
            "-gridarea",
            &c40,
            "-addc,1",
            "-random,r40x25,7",
        ],
        &[],
    )?;
    let a2 = s("area2_r40x25.nc").to_string_lossy().into_owned();
    make(
        &s("cellm2_r40x25.nc"),
        &[
            "-setattribute,tas@cell_measures=area: cell_area",
            "-merge",
            &c40,
            &a2,
        ],
        &[],
    )?;
    make(
        &s("zon_r36x18.nc"),
        &["-zonmean", "-seltimestep,1", &r36],
        &[],
    )?;
    make(
        &s("mer_r36x18.nc"),
        &["-mermean", "-seltimestep,1", &r36],
        &[],
    )?;
    // two pressure levels with unequal layer bounds (thickness 12500 and 42500 Pa)
    let zfile = s("zaxis_bnds.txt");
    if !zfile.exists() {
        std::fs::write(
            &zfile,
            "zaxistype = pressure\nsize = 2\nlevels = 100000 85000\nlbounds = 105000 92500\nubounds = 92500 50000\n",
        )?;
    }
    let setz = format!("-setzaxis,{}", zfile.to_string_lossy());
    make(
        &s("lev_bnds.nc"),
        &[&setz, "-seltimestep,1", &fs("r36x18_2lev.nc")],
        &[],
    )?;

    let cases = [
        Case {
            label: "r36x18_std",
            path: f("r36x18_std.nc"),
        },
        Case {
            label: "hpz2_noleap",
            path: f("hpz2_noleap.nc"),
        },
        Case {
            label: "unst_360",
            path: f("unst_360.nc"),
        },
        Case {
            label: "r36x18_2lev",
            path: f("r36x18_2lev.nc"),
        },
        Case {
            label: "gauss_f16",
            path: s("gauss_f16.nc"),
        },
        Case {
            label: "curv_r36x18",
            path: s("curv_r36x18.nc"),
        },
        Case {
            label: "curv_r40x25",
            path: s("curv_r40x25.nc"),
        },
        Case {
            label: "curv_nobnds",
            path: s("curv_nobnds.nc"),
        },
        Case {
            label: "unst_nobnds",
            path: s("unst_nobnds.nc"),
        },
        Case {
            label: "cellm2_r40x25",
            path: s("cellm2_r40x25.nc"),
        },
        Case {
            label: "zon_r36x18",
            path: s("zon_r36x18.nc"),
        },
        Case {
            label: "mer_r36x18",
            path: s("mer_r36x18.nc"),
        },
        Case {
            label: "lev_bnds",
            path: s("lev_bnds.nc"),
        },
    ];
    println!(
        "{:<14} {:<17} {:<26} {:<18} statistics (max float32 ulp)",
        "case", "grid", "gridarea (max rel)", "gridweights"
    );
    let mut all_ok = true;
    for c in &cases {
        match check(c, &refdir) {
            Ok(r) => {
                all_ok &= r.ok;
                let stats: Vec<String> = r
                    .stats
                    .iter()
                    .map(|(o, u)| format!("{o} {}", fmt_ulp(*u)))
                    .collect();
                println!(
                    "{:<14} {:<17} {:<26} {:<18} {}{}",
                    r.label,
                    r.kind,
                    r.area,
                    r.weights,
                    stats.join(", "),
                    if r.ok { "" } else { "   FAIL" }
                );
            }
            Err(e) => {
                all_ok = false;
                println!("{:<14} ERROR {e}", c.label);
            }
        }
    }
    println!(
        "{}",
        if all_ok {
            "all within tolerance"
        } else {
            "FAILURES"
        }
    );
    if !all_ok {
        std::process::exit(1);
    }
    Ok(())
}
