//! Value-printing operators: `info`, `infon`, `output`, `outputf`, `outputtab`.
//!
//! Unlike the other information operators, these take an operator chain as input
//! (`cdors outputtab,date,lon,lat,value -fldmean -sellonlatbox,-30,40,30,75 in.zarr`). The chain
//! is planned like any other command (a plain input is planned as `-copy <input>`), its stages
//! run through the ordinary pipeline, and the output stage hands its result, one complete field
//! (one timestep and level of one variable) per chunk, to an in-memory collector instead of a
//! file writer. `info`/`infon` keep only the statistics of each field; the `output*` operators
//! keep the values, which `--max-values` bounds.
//!
//! The text forms reproduce cdo 2.6.0 (`src/operators/Info.cc`, `src/operators/Output.cc`);
//! numbers are formatted by the C library's `snprintf` with cdo's formats, so they match cdo's
//! byte for byte where the values do. With `--json` one JSON object is printed (schema
//! `"cdors_values": 1`): `operator`, `variables` (name, units, long_name, param, gridsize,
//! levels) and `records`, with dates as ISO strings, numbers as numbers and missing values as
//! null.
//!
//! **Flood guard.** Before anything is read, the number of values to print (for `info`/`infon`:
//! the number of fields, one line each) is compared with `--max-values` (default 1,000,000;
//! `none` removes the limit); above it the run is refused (`too_many_values`, exit code 4).

use crate::chain::{Command, Input, OpNode};
use crate::error::{Error, ErrorCode, Result};
use crate::exec::{OutVar, Writer};
use crate::io::Values;
use crate::model::{DType, DimRole, GridKind};
use crate::plan::{self, Desc, GridDesc, Plan};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int};
use std::sync::{Arc, Mutex};

/// `--max-values` default.
pub const DEFAULT_MAX_VALUES: u64 = 1_000_000;
/// Schema version of the `--json` output.
pub const SCHEMA: u32 = 1;

/// Whether `name` is one of the value-printing operators.
pub fn handles(name: &str) -> bool {
    matches!(name, "info" | "infon" | "output" | "outputf" | "outputtab")
}

// ------------------------------------------------------------------ C formatting

unsafe extern "C" {
    fn snprintf(buf: *mut c_char, n: usize, fmt: *const c_char, ...) -> c_int;
}

/// A validated C format with exactly one floating-point conversion.
#[derive(Debug, Clone)]
struct CFormat(CString);

impl CFormat {
    /// Accepts literal text, `%%` and exactly one conversion
    /// `%[-+ #0]*[width][.precision][l](e|E|f|F|g|G|a|A)`; width and precision have at most
    /// three digits.
    fn parse(f: &str) -> Result<Self> {
        let bad = || {
            Error::bad_arguments(format!("invalid format '{f}'")).with_hint(
                "give a C format with one floating-point conversion, e.g. outputf,%8.3f or \
                 outputf,%g,6 (flags -+ #0, width, precision; conversions e f g a)",
            )
        };
        let b = f.as_bytes();
        let (mut i, mut convs) = (0, 0);
        while i < b.len() {
            if b[i] != b'%' {
                i += 1;
                continue;
            }
            if b.get(i + 1) == Some(&b'%') {
                i += 2;
                continue;
            }
            i += 1;
            while i < b.len() && b"-+ #0".contains(&b[i]) {
                i += 1;
            }
            let digits = |i: &mut usize| {
                let s = *i;
                while *i < b.len() && b[*i].is_ascii_digit() {
                    *i += 1;
                }
                *i - s
            };
            if digits(&mut i) > 3 {
                return Err(bad());
            }
            if i < b.len() && b[i] == b'.' {
                i += 1;
                if digits(&mut i) > 3 {
                    return Err(bad());
                }
            }
            if i < b.len() && b[i] == b'l' {
                i += 1;
            }
            if i < b.len() && b"eEfFgGaA".contains(&b[i]) {
                convs += 1;
                i += 1;
            } else {
                return Err(bad());
            }
        }
        if convs != 1 {
            return Err(bad());
        }
        Ok(Self(CString::new(f).map_err(|_| bad())?))
    }

    /// `%<width>.<prec>g` (a negative width aligns left, as in C).
    fn g(width: i32, prec: usize) -> Self {
        Self(CString::new(format!("%{width}.{prec}g")).expect("no NUL"))
    }

    fn fixed(f: &str) -> Self {
        Self(CString::new(f).expect("no NUL"))
    }

    fn fmt(&self, v: f64) -> String {
        c_fmt(&self.0, v)
    }
}

fn c_fmt(fmt: &CStr, v: f64) -> String {
    let mut buf = vec![0u8; 64];
    loop {
        // SAFETY: `fmt` holds exactly one floating-point conversion (validated or built here),
        // which consumes the one double passed; `buf` has `buf.len()` writable bytes.
        let n = unsafe {
            snprintf(
                buf.as_mut_ptr().cast::<c_char>(),
                buf.len(),
                fmt.as_ptr(),
                v,
            )
        };
        let Ok(n) = usize::try_from(n) else {
            return String::new();
        };
        if n < buf.len() {
            buf.truncate(n);
            return String::from_utf8_lossy(&buf).into_owned();
        }
        buf.resize(n + 1, 0);
    }
}

/// C's `%*s` / `%*d`: right-aligned to `width`, left-aligned for a negative width.
fn pad(s: &str, width: i32) -> String {
    let w = width.unsigned_abs() as usize;
    if width < 0 {
        format!("{s:<w$}")
    } else {
        format!("{s:>w$}")
    }
}

// ------------------------------------------------------------------ operators and keys

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    Value,
    Param,
    Code,
    Name,
    X,
    Y,
    Lon,
    Lat,
    Lev,
    Bin,
    Xind,
    Yind,
    Timestep,
    Date,
    Time,
    Year,
    Month,
    Day,
}

/// cdo's keys of `outputtab` with their default column widths (`Output.cc`, `keyMap`).
const KEYS: &[(&str, Option<Key>, i32)] = &[
    ("nohead", None, 0),
    ("value", Some(Key::Value), 8),
    ("param", Some(Key::Param), 11),
    ("code", Some(Key::Code), 4),
    ("name", Some(Key::Name), 8),
    ("x", Some(Key::X), 6),
    ("y", Some(Key::Y), 6),
    ("lon", Some(Key::Lon), 6),
    ("lat", Some(Key::Lat), 6),
    ("lev", Some(Key::Lev), 6),
    ("bin", Some(Key::Bin), 6),
    ("xind", Some(Key::Xind), 4),
    ("yind", Some(Key::Yind), 4),
    ("timestep", Some(Key::Timestep), 6),
    ("date", Some(Key::Date), 10),
    ("time", Some(Key::Time), 8),
    ("year", Some(Key::Year), 5),
    ("month", Some(Key::Month), 2),
    ("day", Some(Key::Day), 2),
];

enum Kind {
    Info {
        names: bool,
    },
    Output,
    Outputf {
        fmt: CFormat,
        nelem: usize,
    },
    Tab {
        keys: Vec<(&'static str, Key, i32)>,
        head: bool,
    },
}

impl Kind {
    fn parse(node: &OpNode) -> Result<Self> {
        Ok(match node.name.as_str() {
            "info" => Self::Info { names: false },
            "infon" => Self::Info { names: true },
            "output" => Self::Output,
            "outputf" => {
                let nelem = match node.args.get(1) {
                    Some(a) => a
                        .trim()
                        .parse::<usize>()
                        .ok()
                        .filter(|&n| n >= 1)
                        .ok_or_else(|| {
                            Error::bad_arguments(format!(
                                "outputf: number of values per line must be >= 1, got '{a}'"
                            ))
                        })?,
                    None => 1,
                };
                Self::Outputf {
                    fmt: CFormat::parse(&node.args[0])?,
                    nelem,
                }
            }
            _ => {
                let mut keys = Vec::new();
                let mut head = true;
                for a in &node.args {
                    let (name, width) = match a.split_once(':') {
                        Some((n, w)) => (
                            n,
                            Some(
                                w.trim()
                                    .parse::<i32>()
                                    .ok()
                                    .filter(|w| w.abs() <= 999)
                                    .ok_or_else(|| {
                                        Error::bad_arguments(format!(
                                            "outputtab: invalid column width in '{a}'"
                                        ))
                                        .with_hint("key:width, e.g. value:12")
                                    })?,
                            ),
                        ),
                        None => (a.as_str(), None),
                    };
                    let Some(&(kname, key, w0)) = KEYS.iter().find(|k| k.0 == name) else {
                        let all: Vec<&str> = KEYS.iter().map(|k| k.0).collect();
                        return Err(Error::bad_arguments(format!(
                            "outputtab: unsupported key '{name}'"
                        ))
                        .with("key", name)
                        .with_hint(format!("keys: {}", all.join(", "))));
                    };
                    match key {
                        None => head = false,
                        Some(k) => keys.push((kname, k, width.unwrap_or(w0))),
                    }
                }
                Self::Tab { keys, head }
            }
        })
    }

    fn is_info(&self) -> bool {
        matches!(self, Self::Info { .. })
    }
}

// ------------------------------------------------------------------ collecting the fields

/// Statistics of one field (cdo's `info`).
#[derive(Debug, Clone, Copy)]
struct Stats {
    min: f64,
    max: f64,
    sum: f64,
    n: usize,
    nmiss: usize,
}

impl Stats {
    fn of(v: &[f64]) -> Self {
        let mut s = Self {
            min: f64::MAX,
            max: -f64::MAX,
            sum: 0.0,
            n: 0,
            nmiss: 0,
        };
        for &x in v {
            if x.is_nan() {
                s.nmiss += 1;
            } else {
                s.min = s.min.min(x);
                s.max = s.max.max(x);
                s.sum += x;
                s.n += 1;
            }
        }
        s
    }
}

enum Field {
    Stats(Stats),
    Values(Vec<f64>),
}

/// The "writer" of the output stage: one complete field per chunk, kept in memory.
struct Collector {
    stats_only: bool,
    /// Per variable: positions of the time and vertical dimensions.
    dims: Vec<(Option<usize>, Option<usize>)>,
    fields: Mutex<HashMap<(usize, usize, usize), Field>>,
}

impl Writer for Collector {
    fn ordered(&self) -> bool {
        true
    }

    fn write(&self, var: usize, origin: &[usize], _shape: &[usize], data: Values) -> Result<()> {
        let (td, zd) = self.dims[var];
        let key = (
            var,
            td.map_or(0, |d| origin[d]),
            zd.map_or(0, |d| origin[d]),
        );
        let vals: Vec<f64> = match data {
            Values::F64(v) => v,
            Values::F32(v) => v.into_iter().map(f64::from).collect(),
        };
        let f = if self.stats_only {
            Field::Stats(Stats::of(&vals))
        } else {
            Field::Values(vals)
        };
        self.fields.lock().expect("collector lock").insert(key, f);
        Ok(())
    }

    fn finish(&self) -> Result<()> {
        Ok(())
    }
}

// ------------------------------------------------------------------ grid points

/// Coordinates of every point of a grid, in output order: longitudes and latitudes in degrees,
/// the stored x/y values, and cdo's x/y indices (0-based).
struct Points {
    lon: Vec<f64>,
    lat: Vec<f64>,
    x: Vec<f64>,
    y: Vec<f64>,
    xind: Vec<usize>,
    yind: Vec<usize>,
}

fn to_deg(v: Vec<f64>, units: &str) -> Vec<f64> {
    if units.starts_with("rad") {
        v.into_iter().map(f64::to_degrees).collect()
    } else {
        v
    }
}

fn grid_points(g: &GridDesc) -> Result<Option<Points>> {
    let n = g.size();
    let two_d = g.sel.len() == 2;
    if let Some(c) = g.coords()? {
        if two_d && matches!(g.kind, GridKind::Regular | GridKind::Gaussian) {
            let nx = c.xvals.len().max(1);
            let lon: Vec<f64> = (0..n).map(|i| c.xvals[i % nx]).collect();
            let lat: Vec<f64> = (0..n).map(|i| c.yvals[i / nx]).collect();
            // cdo treats only lon-lat and curvilinear grids as 2-D for xind/yind
            let regular = g.kind == GridKind::Regular;
            return Ok(Some(Points {
                x: lon.clone(),
                y: lat.clone(),
                lon: to_deg(lon, &c.xunits),
                lat: to_deg(lat, &c.yunits),
                xind: (0..n).map(|i| if regular { i % nx } else { i }).collect(),
                yind: (0..n).map(|i| if regular { i / nx } else { i }).collect(),
            }));
        }
        let nx = if two_d {
            g.sel[1].len().max(1)
        } else {
            n.max(1)
        };
        let curvi = two_d && g.kind == GridKind::Curvilinear;
        return Ok(Some(Points {
            x: c.xvals.clone(),
            y: c.yvals.clone(),
            lon: to_deg(c.xvals, &c.xunits),
            lat: to_deg(c.yvals, &c.yunits),
            xind: (0..n).map(|i| if curvi { i % nx } else { i }).collect(),
            yind: (0..n).map(|i| if curvi { i / nx } else { i }).collect(),
        }));
    }
    // a complete HEALPix grid: analytic cell centres
    let Some(hp) = g.base.healpix.as_ref() else {
        return Ok(None);
    };
    let cells: Vec<u64> = match &hp.index_var {
        Some(iv) => {
            let idx = g.src.read_var(iv)?;
            g.sel[0].to_vec().iter().map(|&i| idx[i] as u64).collect()
        }
        None => g.sel[0].to_vec().iter().map(|&i| i as u64).collect(),
    };
    let (xs, ys) = plan::healpix_centers(hp.nside, hp.order, &cells);
    let lon = to_deg(xs, "radian");
    let lat = to_deg(ys, "radian");
    Ok(Some(Points {
        x: lon.clone(),
        y: lat.clone(),
        lon,
        lat,
        xind: (0..n).collect(),
        yind: vec![0; n],
    }))
}

// ------------------------------------------------------------------ running

/// The fields of the output in cdo's order: timestep, then variable, then level. Variables
/// without a time dimension appear at the first timestep only.
fn field_order(desc: &Desc) -> Vec<(usize, usize, usize)> {
    let has_time = desc.vars.iter().any(|v| v.dim_of(DimRole::Time).is_some());
    let nt = if has_time { desc.ntime().max(1) } else { 1 };
    let mut out = Vec::new();
    for t in 0..nt {
        for (vi, v) in desc.vars.iter().enumerate() {
            if t > 0 && v.dim_of(DimRole::Time).is_none() {
                continue;
            }
            let nz = v.dim_of(DimRole::Vertical).map_or(1, |d| v.dims[d].size);
            for z in 0..nz {
                out.push((vi, t, z));
            }
        }
    }
    out
}

fn gridsize(v: &plan::VarDesc) -> usize {
    v.hdims().iter().map(|&d| v.dims[d].size).product()
}

/// cdo's parameter ID of a NetCDF variable: `-(position among the data variables + 1)` of the
/// first input; position in the output otherwise.
fn param_ids(plan: &Plan) -> Vec<i64> {
    let ims: Vec<usize> = plan.intermediates.iter().map(|i| i.src).collect();
    let names: Vec<String> = (0..plan.sources.len())
        .find(|i| !ims.contains(i))
        .map(|i| {
            let ds = plan.sources[i].dataset();
            ds.data_vars()
                .filter(|v| v.dtype.is_numeric() && !v.dims.is_empty())
                .map(|v| v.name.clone())
                .collect()
        })
        .unwrap_or_default();
    plan.desc
        .vars
        .iter()
        .enumerate()
        .map(|(k, v)| -(names.iter().position(|n| *n == v.name).unwrap_or(k) as i64 + 1))
        .collect()
}

/// Number of values (for `info`/`infon`: fields) the operator would print.
fn count(desc: &Desc, kind: &Kind) -> u64 {
    if kind.is_info() {
        return field_order(desc).len() as u64;
    }
    let order = field_order(desc);
    order
        .iter()
        .map(|&(v, _, _)| gridsize(&desc.vars[v]) as u64)
        .sum()
}

/// Plans the input chain, prints its values (or, with `--plan`, the plan).
pub fn run(cmd: &Command) -> Result<String> {
    let root = &cmd.root;
    let kind = Kind::parse(root)?;
    let inner = match &root.inputs[0] {
        Input::Op(o) => o.clone(),
        Input::Path(p) => OpNode {
            name: "copy".into(),
            args: Vec::new(),
            inputs: vec![Input::Path(p.clone())],
        },
    };
    let sub = Command {
        options: cmd.options.clone(),
        root: inner,
        // the plan needs an output name; nothing is written
        outputs: if cmd.options.plan {
            Vec::new()
        } else {
            vec!["-".into()]
        },
    };
    // without cdo first: --max-values and --max-read are evaluated before a remapping in the
    // chain runs cdo for its target grid or weights
    crate::io::netcdf4_index::hold_cache_writes();
    let plan = plan::build(
        &sub,
        if cmd.options.plan {
            plan::CdoUse::Never
        } else {
            plan::CdoUse::Defer
        },
    )?;
    let n = count(&plan.desc, &kind);
    let limit = cmd.options.max_values.unwrap_or(DEFAULT_MAX_VALUES);
    let what = if kind.is_info() { "fields" } else { "values" };
    if cmd.options.plan {
        let mut p = plan::explain::to_json(&plan, &sub, plan.threads, plan.io_threads);
        p["print"] = json!({
            "operator": root.name,
            what: n,
            "max_values": (limit != u64::MAX).then_some(limit),
            "exceeded": n > limit,
        });
        return Ok(if cmd.options.json {
            format!("{p}\n")
        } else {
            format!(
                "{}print: {} {n} {what} (--max-values {})\n",
                plan::explain::to_text(&p),
                root.name,
                if limit == u64::MAX {
                    "none".to_owned()
                } else {
                    limit.to_string()
                }
            )
        });
    }
    if n > limit {
        return Err(Error::new(
            ErrorCode::TooManyValues,
            format!(
                "'{}' would print {n} {what}, more than --max-values {limit}",
                root.name
            ),
        )
        .with("operator", root.name.clone())
        .with(what, n)
        .with("limit", limit)
        .with_hint(
            "reduce with fldmean/sellonlatbox/seltimestep or write to a file \
             (cdors <chain> out.nc); --max-values none removes the limit",
        ));
    }
    crate::exec::check_read_limit(&plan, cmd)?;
    crate::io::netcdf4_index::release_cache_writes(true);
    let plan = if plan.deferred {
        plan::build(&sub, plan::CdoUse::Now)?
    } else {
        plan
    };
    let stats = plan::explain::read_stats(&plan);
    // coordinates are read before the stages run (they may come from an intermediate)
    let want_points = matches!(&kind, Kind::Tab { keys, .. }
        if keys.iter().any(|k| matches!(k.1, Key::Lon | Key::Lat | Key::X | Key::Y | Key::Xind | Key::Yind)));
    let mut points: HashMap<usize, Points> = HashMap::new();
    if want_points {
        for v in &plan.desc.vars {
            let Some(gi) = v.grid else { continue };
            if points.contains_key(&gi) {
                continue;
            }
            let p = grid_points(&plan.desc.grids[gi])?.ok_or_else(|| {
                Error::new(
                    ErrorCode::NoCoordinates,
                    format!("variable '{}' has no cell-centre coordinates", v.name),
                )
                .with("variable", v.name.clone())
                .with_hint("leave out the keys lon, lat, x, y, xind and yind")
            })?;
            points.insert(gi, p);
        }
    }
    let lay: Vec<OutVar> = plan
        .desc
        .vars
        .iter()
        .map(|v| OutVar {
            name: v.name.clone(),
            dims: v.dims.iter().map(|d| d.name.clone()).collect(),
            shape: v.shape(),
            chunks: v
                .dims
                .iter()
                .map(|d| {
                    if d.role == DimRole::Horizontal {
                        d.size.max(1)
                    } else {
                        1
                    }
                })
                .collect(),
            dtype: DType::F64,
            missval: v.missval,
        })
        .collect();
    let coll = Arc::new(Collector {
        stats_only: kind.is_info(),
        dims: plan
            .desc
            .vars
            .iter()
            .map(|v| (v.dim_of(DimRole::Time), v.dim_of(DimRole::Vertical)))
            .collect(),
        fields: Mutex::new(HashMap::new()),
    });
    crate::exec::progress::start(plan.stages.len(), stats.iter().map(|s| s.chunks_read).sum());
    crate::exec::run_stages(&plan, &lay, coll.clone())?;
    let fields = std::mem::take(&mut *coll.fields.lock().expect("collector lock"));
    let ctx = Ctx {
        plan: &plan,
        params: param_ids(&plan),
        fields,
        points,
        name: &root.name,
    };
    if cmd.options.json {
        Ok(format!("{}\n", ctx.json(&kind)?))
    } else {
        ctx.text(&kind)
    }
}

struct Ctx<'a> {
    plan: &'a Plan,
    params: Vec<i64>,
    fields: HashMap<(usize, usize, usize), Field>,
    points: HashMap<usize, Points>,
    name: &'a str,
}

/// Date and time of timestep `t` as cdo prints them, and as ISO strings.
struct When {
    date: String,
    time: String,
    iso_date: Option<String>,
    iso: Option<String>,
    ymd: (i32, u32, u32),
}

impl Ctx<'_> {
    fn when(&self, t: usize) -> When {
        match self
            .plan
            .desc
            .time
            .as_ref()
            .and_then(|tm| tm.axis.steps.get(t))
        {
            Some(s) => {
                let d = &s.datetime;
                let iso = d.iso();
                When {
                    date: d.date_string(),
                    time: d.time_string(),
                    iso_date: iso.split('T').next().map(str::to_owned),
                    iso: Some(iso),
                    ymd: (d.year, d.month, d.day),
                }
            }
            None => When {
                date: " 0000-00-00".into(),
                time: "00:00:00".into(),
                iso_date: None,
                iso: None,
                ymd: (0, 0, 0),
            },
        }
    }

    fn level(&self, v: usize, z: usize) -> f64 {
        let var = &self.plan.desc.vars[v];
        var.zaxis
            .and_then(|zi| self.plan.desc.zaxes.get(zi))
            .and_then(|za| za.axis.values.get(z).copied())
            .unwrap_or(0.0)
    }

    /// cdo's digits: 7 for float, 15 for double variables.
    fn digits(&self, v: usize) -> usize {
        if self.plan.desc.vars[v].dtype == DType::F32 {
            7
        } else {
            15
        }
    }

    /// A value for JSON: null if missing; values (and means) of float32 variables in their
    /// shortest float32 form, so that `299.59595` is not printed as `299.59595774884014`.
    fn num(&self, v: usize, x: f64) -> Value {
        if x.is_nan() {
            return Value::Null;
        }
        let x = if self.plan.desc.vars[v].dtype == DType::F32 {
            format!("{}", x as f32).parse::<f64>().unwrap_or(x)
        } else {
            x
        };
        json!(x)
    }

    fn field(&self, key: (usize, usize, usize)) -> Result<&Field> {
        self.fields.get(&key).ok_or_else(|| {
            Error::internal(format!(
                "field {} of variable '{}' (timestep {}, level {}) was not computed",
                key.0, self.plan.desc.vars[key.0].name, key.1, key.2
            ))
        })
    }

    fn values(&self, key: (usize, usize, usize)) -> Result<&[f64]> {
        match self.field(key)? {
            Field::Values(v) => Ok(v),
            Field::Stats(_) => Err(Error::internal("values of a statistics-only field")),
        }
    }

    fn text(&self, kind: &Kind) -> Result<String> {
        let desc = &self.plan.desc;
        let order = field_order(desc);
        let mut out = String::new();
        match kind {
            Kind::Info { names } => {
                let label = if *names {
                    "Parameter name"
                } else {
                    "Parameter ID"
                };
                let header = |idx: &str| {
                    format!(
                        "{idx} :       Date     Time   Level Gridsize    Miss :     Minimum        \
                         Mean     Maximum : {label}\n"
                    )
                };
                out.push_str(&header("    -1"));
                let g5 = CFormat::fixed("%#12.5g");
                let g7 = CFormat::fixed("%7g");
                for (k, &(v, t, z)) in order.iter().enumerate() {
                    let Field::Stats(s) = self.field((v, t, z))? else {
                        return Err(Error::internal("values instead of statistics"));
                    };
                    let w = self.when(t);
                    let stats = match s.n {
                        0 => "                     nan            ".to_owned(),
                        1 => format!("            {}            ", g5.fmt(s.sum)),
                        n => format!(
                            "{}{}{}",
                            g5.fmt(s.min),
                            g5.fmt(s.sum / n as f64),
                            g5.fmt(s.max)
                        ),
                    };
                    let var = &desc.vars[v];
                    let tag = if *names {
                        var.name.clone()
                    } else {
                        self.params[v].to_string()
                    };
                    out.push_str(&format!(
                        "{:6} :{} {} {} {:8} {:7} :{} : {:<14}\n",
                        k + 1,
                        w.date,
                        w.time,
                        g7.fmt(self.level(v, z)),
                        gridsize(var),
                        s.nmiss,
                        stats,
                        tag
                    ));
                }
                if order.len() > 36 {
                    out.push_str(&header("      "));
                }
            }
            Kind::Output | Kind::Outputf { .. } => {
                let (fmt, nelem) = match kind {
                    Kind::Outputf { fmt, nelem } => (fmt.clone(), *nelem),
                    _ => (CFormat::fixed(" %12.6g"), 6),
                };
                for &key in &order {
                    let missval = desc.vars[key.0].missval;
                    for (i, &x) in self.values(key)?.iter().enumerate() {
                        if i > 0 && i % nelem == 0 {
                            out.push('\n');
                        }
                        out.push_str(&fmt.fmt(if x.is_nan() { missval } else { x }));
                    }
                    out.push('\n');
                }
            }
            Kind::Tab { keys, head } => {
                if *head {
                    out.push('#');
                    for (name, _, w) in keys {
                        out.push_str(&pad(name, *w));
                        out.push(' ');
                    }
                    out.push('\n');
                }
                for &(v, t, z) in &order {
                    let var = &desc.vars[v];
                    let dig = self.digits(v);
                    let fmts: Vec<CFormat> = keys.iter().map(|k| CFormat::g(k.2, dig)).collect();
                    let w = self.when(t);
                    let level = self.level(v, z);
                    let pts = var.grid.and_then(|g| self.points.get(&g));
                    let param = self.params[v].to_string();
                    for (i, &x) in self.values((v, t, z))?.iter().enumerate() {
                        let x = if x.is_nan() { var.missval } else { x };
                        for ((_, key, wd), f) in keys.iter().zip(&fmts) {
                            let p = |sel: fn(&Points) -> &Vec<f64>| {
                                pts.map_or(f64::NAN, |p| sel(p).get(i).copied().unwrap_or(f64::NAN))
                            };
                            let s = match key {
                                Key::Value => f.fmt(x),
                                Key::X => f.fmt(p(|p| &p.x)),
                                Key::Y => f.fmt(p(|p| &p.y)),
                                Key::Lon => f.fmt(p(|p| &p.lon)),
                                Key::Lat => f.fmt(p(|p| &p.lat)),
                                Key::Lev | Key::Bin => f.fmt(level),
                                Key::Param => pad(&param, *wd),
                                Key::Code => pad(&self.params[v].to_string(), *wd),
                                Key::Name => pad(&var.name, *wd),
                                Key::Xind => pad(
                                    &(pts.map_or(i, |p| p.xind.get(i).copied().unwrap_or(i)) + 1)
                                        .to_string(),
                                    *wd,
                                ),
                                Key::Yind => pad(
                                    &(pts.map_or(i, |p| p.yind.get(i).copied().unwrap_or(i)) + 1)
                                        .to_string(),
                                    *wd,
                                ),
                                Key::Timestep => pad(&(t + 1).to_string(), *wd),
                                Key::Date => pad(&w.date, *wd),
                                Key::Time => pad(&w.time, *wd),
                                Key::Year => pad(&w.ymd.0.to_string(), *wd),
                                Key::Month => pad(&w.ymd.1.to_string(), *wd),
                                Key::Day => pad(&w.ymd.2.to_string(), *wd),
                            };
                            out.push_str(&s);
                            out.push(' ');
                        }
                        out.push('\n');
                    }
                }
            }
        }
        Ok(out)
    }

    fn json(&self, kind: &Kind) -> Result<Value> {
        let desc = &self.plan.desc;
        let order = field_order(desc);
        let variables: Vec<Value> = desc
            .vars
            .iter()
            .enumerate()
            .map(|(k, v)| {
                json!({
                    "name": v.name,
                    "units": v.attrs.get_str("units"),
                    "long_name": v.attrs.get_str("long_name"),
                    "param": self.params[k].to_string(),
                    "dtype": if v.dtype == DType::F32 { "float32" } else { "float64" },
                    "gridsize": gridsize(v),
                    "levels": v.dim_of(DimRole::Vertical).map_or(1, |d| v.dims[d].size),
                })
            })
            .collect();
        let mut records: Vec<Value> = Vec::new();
        match kind {
            Kind::Info { .. } => {
                for (k, &(v, t, z)) in order.iter().enumerate() {
                    let Field::Stats(s) = self.field((v, t, z))? else {
                        return Err(Error::internal("values instead of statistics"));
                    };
                    let (min, mean, max) = if s.n == 0 {
                        (Value::Null, Value::Null, Value::Null)
                    } else {
                        (
                            self.num(v, s.min),
                            self.num(v, s.sum / s.n as f64),
                            self.num(v, s.max),
                        )
                    };
                    let var = &desc.vars[v];
                    records.push(json!({
                        "index": k + 1,
                        "timestep": t + 1,
                        "date": self.when(t).iso,
                        "name": var.name,
                        "param": self.params[v].to_string(),
                        "level": self.level(v, z),
                        "gridsize": gridsize(var),
                        "missing": s.nmiss,
                        "valid": s.n,
                        "min": min,
                        "mean": mean,
                        "max": max,
                    }));
                }
            }
            Kind::Output | Kind::Outputf { .. } => {
                for &(v, t, z) in &order {
                    let vals: Vec<Value> = self
                        .values((v, t, z))?
                        .iter()
                        .map(|&x| self.num(v, x))
                        .collect();
                    records.push(json!({
                        "name": desc.vars[v].name,
                        "timestep": t + 1,
                        "date": self.when(t).iso,
                        "level": self.level(v, z),
                        "values": vals,
                    }));
                }
            }
            Kind::Tab { keys, .. } => {
                for &(v, t, z) in &order {
                    let var = &desc.vars[v];
                    let w = self.when(t);
                    let level = self.level(v, z);
                    let pts = var.grid.and_then(|g| self.points.get(&g));
                    let coord = |sel: fn(&Points) -> &Vec<f64>, i: usize| -> Value {
                        pts.and_then(|p| sel(p).get(i).copied())
                            .filter(|x| x.is_finite())
                            .map_or(Value::Null, |x| json!(x))
                    };
                    for (i, &x) in self.values((v, t, z))?.iter().enumerate() {
                        let mut r = Map::new();
                        for (name, key, _) in keys {
                            let val = match key {
                                Key::Value => self.num(v, x),
                                Key::X => coord(|p| &p.x, i),
                                Key::Y => coord(|p| &p.y, i),
                                Key::Lon => coord(|p| &p.lon, i),
                                Key::Lat => coord(|p| &p.lat, i),
                                Key::Lev | Key::Bin => json!(level),
                                Key::Param => json!(self.params[v].to_string()),
                                Key::Code => json!(self.params[v]),
                                Key::Name => json!(var.name),
                                Key::Xind => json!(
                                    pts.map_or(i, |p| p.xind.get(i).copied().unwrap_or(i)) + 1
                                ),
                                Key::Yind => json!(
                                    pts.map_or(i, |p| p.yind.get(i).copied().unwrap_or(i)) + 1
                                ),
                                Key::Timestep => json!(t + 1),
                                Key::Date => json!(w.iso_date),
                                Key::Time => json!(w.iso.as_ref().map(|_| w.time.clone())),
                                Key::Year => json!(w.iso.as_ref().map(|_| w.ymd.0)),
                                Key::Month => json!(w.iso.as_ref().map(|_| w.ymd.1)),
                                Key::Day => json!(w.iso.as_ref().map(|_| w.ymd.2)),
                            };
                            r.insert((*name).to_owned(), val);
                        }
                        records.push(Value::Object(r));
                    }
                }
            }
        }
        let mut o = json!({
            "cdors_values": SCHEMA,
            "operator": self.name,
            "variables": variables,
            "records": records,
        });
        if let Kind::Tab { keys, .. } = kind {
            o["keys"] = json!(keys.iter().map(|k| k.0).collect::<Vec<_>>());
        }
        Ok(o)
    }
}
