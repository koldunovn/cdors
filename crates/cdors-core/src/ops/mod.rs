//! The operator registry: names, arity, arguments, access class and descriptions.
//!
//! Every operator cdors knows is listed here, also those that later tasks implement
//! (`implemented: false`), so that whole chains parse and `--plan` can describe them. The access
//! class tells the planner how an operator runs:
//!
//! | class | how it runs |
//! |---|---|
//! | selection | index sets; decides which chunks are read |
//! | pointwise | merged into the surrounding stage |
//! | reduction | state folded in a fixed order, carried across chunk boundaries |
//! | whole_extent | tiles complete along one dimension (percentiles, running means, remapping) |
//! | info | reads metadata (and possibly data) and prints to stdout; no output file |
//!
//! The value-printing operators (`info`, `infon`, `output`, `outputf`, `outputtab`) are of class
//! info but take an operator chain as input (`ops::output`).

pub mod arith;
pub mod catalog;
pub mod files;
pub mod fldhist;
pub mod fldstat;
pub mod healpix;
pub mod info;
pub mod moc;
pub mod transport;
pub mod output;
pub mod pctl;
pub mod places;
pub mod regions;
pub mod regstat;
pub mod remap;
pub mod runstat;
pub mod sample;
pub mod select;
pub mod settime;
pub mod timstat;
pub mod vertical;
pub mod ymonarith;

use crate::chain::OpNode;
use crate::error::{Error, ErrorCode, Result};
use crate::plan::{Desc, Sources};
use serde::Serialize;
use std::sync::LazyLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessClass {
    Selection,
    Pointwise,
    Reduction,
    WholeExtent,
    Info,
}

/// Type of an operator argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArgType {
    Int,
    Float,
    /// Any string (names, dates, ranges like `1/12`, grid names, `key=value`).
    Str,
}

/// One argument of an operator.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ArgSpec {
    pub name: &'static str,
    pub ty: ArgType,
    /// Default value; arguments with a default are optional.
    pub default: Option<&'static str>,
    /// The argument may be repeated (`selname,a,b,c`); at least one value is required unless it
    /// has a default.
    pub repeated: bool,
}

const fn arg(name: &'static str, ty: ArgType) -> ArgSpec {
    ArgSpec {
        name,
        ty,
        default: None,
        repeated: false,
    }
}

const fn args_of(name: &'static str, ty: ArgType) -> ArgSpec {
    ArgSpec {
        name,
        ty,
        default: None,
        repeated: true,
    }
}

const fn opt(name: &'static str, ty: ArgType, default: &'static str) -> ArgSpec {
    ArgSpec {
        name,
        ty,
        default: Some(default),
        repeated: false,
    }
}

/// Number of inputs of an operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Inputs {
    Fixed(usize),
    /// One or more: all remaining inputs (only as the outermost operator, as in cdo).
    Variadic,
    /// One input, or cdo's three inputs `data min max` of the percentile operators, whose last
    /// two are accepted but never evaluated (see [`used_inputs`]).
    OneOrThree,
}

/// Registry entry of one operator.
#[derive(Debug, Clone, Serialize)]
pub struct OpSpec {
    pub name: String,
    pub aliases: Vec<&'static str>,
    pub inputs: Inputs,
    /// Number of output files: 1 for data operators, 0 for information operators.
    pub outputs: usize,
    pub args: Vec<ArgSpec>,
    pub class: AccessClass,
    pub description: String,
    pub implemented: bool,
    /// `cdo` for operators of cdo's catalogue (cdo's name and semantics, possibly with cdors
    /// extensions), `cdors` for operators cdo does not have; set in [`build_registry`] only.
    pub origin: &'static str,
}

impl OpSpec {
    /// Checks the number and types of raw arguments.
    pub fn check_args(&self, args: &[String]) -> Result<()> {
        let required = self.args.iter().filter(|a| a.default.is_none()).count();
        let variadic = self.args.iter().any(|a| a.repeated);
        let usage = || {
            let list: Vec<String> = self
                .args
                .iter()
                .map(|a| match (a.default, a.repeated) {
                    (Some(_), true) => format!("[{}...]", a.name),
                    (Some(d), _) => format!("[{}={d}]", a.name),
                    (None, true) => format!("{}...", a.name),
                    (None, false) => a.name.to_owned(),
                })
                .collect();
            if list.is_empty() {
                self.name.clone()
            } else {
                format!("{},{}", self.name, list.join(","))
            }
        };
        if args.len() < required || (!variadic && args.len() > self.args.len()) {
            return Err(Error::bad_arguments(format!(
                "operator '{}' takes {} argument(s), got {}",
                self.name,
                if variadic {
                    format!("at least {required}")
                } else if required == self.args.len() {
                    required.to_string()
                } else {
                    format!("{required} to {}", self.args.len())
                },
                args.len()
            ))
            .with("operator", self.name.clone())
            .with_hint(format!("usage: {}", usage())));
        }
        for (i, a) in args.iter().enumerate() {
            let spec = self
                .args
                .get(i)
                .or_else(|| self.args.last())
                .expect("checked above");
            let ok = match spec.ty {
                ArgType::Int => a.trim().parse::<i64>().is_ok(),
                ArgType::Float => a.trim().parse::<f64>().is_ok(),
                ArgType::Str => true,
            };
            if !ok {
                return Err(Error::bad_arguments(format!(
                    "argument '{}' of operator '{}' must be {}, got '{a}'",
                    spec.name,
                    self.name,
                    match spec.ty {
                        ArgType::Int => "an integer",
                        _ => "a number",
                    }
                ))
                .with("operator", self.name.clone())
                .with_hint(format!("usage: {}", usage())));
            }
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn op(
    name: &str,
    inputs: Inputs,
    outputs: usize,
    args: Vec<ArgSpec>,
    class: AccessClass,
    description: &str,
    implemented: bool,
) -> OpSpec {
    OpSpec {
        name: name.to_owned(),
        aliases: Vec::new(),
        inputs,
        outputs,
        args,
        class,
        description: description.to_owned(),
        implemented,
        origin: "cdo",
    }
}

/// Data operators implemented so far (information operators are marked in the table).
const IMPLEMENTED: &[&str] = &[
    "selname",
    "sellevel",
    "seltimestep",
    "seldate",
    "selyear",
    "selmon",
    "selseason",
    "sellonlatbox",
    "shifttime",
    "add",
    "sub",
    "mul",
    "div",
    "addc",
    "subc",
    "mulc",
    "divc",
    "ifthen",
    "copy",
    "setgrid",
    "remap",
    "remapnn",
    "remapdis",
    "remapbil",
    "remapcon",
    "remapycon",
    "hpdegrade",
    "hpupgrade",
    "toelems",
    "tonodes",
    "selpoints",
    "transect",
    "moc",
    "transport",
    "mergetime",
    "cat",
    "selregion",
    "maskregion",
    "regmask",
];

fn build_registry() -> Vec<OpSpec> {
    use AccessClass::*;
    use ArgType::*;
    use Inputs::*;
    let one = Fixed(1);
    let two = Fixed(2);
    let mut r = vec![
        // information operators (implemented)
        op(
            "sinfo",
            one,
            0,
            vec![],
            Info,
            "Short information about the dataset (--json: machine-readable)",
            true,
        ),
        op(
            "showname",
            one,
            0,
            vec![],
            Info,
            "Show variable names",
            true,
        ),
        op(
            "showtimestamp",
            one,
            0,
            vec![],
            Info,
            "Show timestamps",
            true,
        ),
        op(
            "griddes",
            one,
            0,
            vec![],
            Info,
            "Grid description (cdo's text format)",
            true,
        ),
        // value-printing operators: the input may be an operator chain (`ops::output`)
        op(
            "info",
            one,
            0,
            vec![],
            Info,
            "Per field: date, level, size, missing values, minimum, mean, maximum, parameter ID",
            true,
        ),
        op(
            "infon",
            one,
            0,
            vec![],
            Info,
            "Per field: date, level, size, missing values, minimum, mean, maximum, name",
            true,
        ),
        op(
            "output",
            one,
            0,
            vec![],
            Info,
            "Print all values (six per line, %12.6g)",
            true,
        ),
        op(
            "outputf",
            one,
            0,
            vec![arg("format", Str), opt("nelem", Int, "1")],
            Info,
            "Print all values with a C format (one floating-point conversion), nelem per line",
            true,
        ),
        op(
            "outputtab",
            one,
            0,
            vec![args_of("keys", Str)],
            Info,
            "Print a table, one line per value: keys value, name, param, code, lon, lat, x, y, \
             xind, yind, lev, timestep, date, time, year, month, day, nohead (key:width sets \
             the column width)",
            true,
        ),
        // selections
        op(
            "selname",
            one,
            1,
            vec![args_of("names", Str)],
            Selection,
            "Select variables by name",
            false,
        ),
        op(
            "sellevel",
            one,
            1,
            vec![args_of("levels", Float)],
            Selection,
            "Select levels",
            false,
        ),
        op(
            "seltimestep",
            one,
            1,
            vec![args_of("timesteps", Str)],
            Selection,
            "Select timesteps (1-based, ranges a/b)",
            false,
        ),
        op(
            "seldate",
            one,
            1,
            vec![arg("startdate", Str), opt("enddate", Str, "startdate")],
            Selection,
            "Select a date range",
            false,
        ),
        op(
            "selyear",
            one,
            1,
            vec![args_of("years", Str)],
            Selection,
            "Select years",
            false,
        ),
        op(
            "selmon",
            one,
            1,
            vec![args_of("months", Str)],
            Selection,
            "Select months",
            false,
        ),
        op(
            "selseason",
            one,
            1,
            vec![args_of("seasons", Str)],
            Selection,
            "Select seasons (DJF, MAM, JJA, SON)",
            false,
        ),
        op(
            "sellonlatbox",
            one,
            1,
            vec![
                arg("lon1", Float),
                arg("lon2", Float),
                arg("lat1", Float),
                arg("lat2", Float),
            ],
            Selection,
            "Select a longitude/latitude box",
            false,
        ),
        op(
            "shifttime",
            one,
            1,
            vec![arg("shiftValue", Str)],
            Pointwise,
            "Shift the timesteps and their bounds by a time interval (e.g. -1sec, 3hours, -1mon)",
            false,
        ),
        op(
            "selregion",
            one,
            1,
            vec![args_of("regions", Str)],
            Selection,
            "Select the cells whose centre lies in the polygons of cdo region files (lon lat \
             per line, & between polygons); the result is unstructured",
            false,
        ),
        // pointwise
        op(
            "maskregion",
            one,
            1,
            vec![args_of("regions", Str)],
            Pointwise,
            "Set the cells outside the polygons of cdo region files to missing; cdors also takes \
             regions=<set>[,select=<abbrev,...>][,fill=nearest] (a catalog set, see regmask)",
            false,
        ),
        op(
            "regmask",
            one,
            1,
            vec![args_of("params", Str)],
            Pointwise,
            "Region membership mask(region, <cells>) (int8) of the input's grid: \
             regions=<set>[,select=<abbrev,...>][,fill=nearest]; sets ar6.all, ar6.land, \
             ar6.ocean, srex, giorgi, prudence, nino, basins, moc, ne.<set> or a GeoJSON file",
            false,
        ),
        // pointwise
        op("add", two, 1, vec![], Pointwise, "Add two fields", false),
        op(
            "sub",
            two,
            1,
            vec![],
            Pointwise,
            "Subtract two fields",
            false,
        ),
        op(
            "mul",
            two,
            1,
            vec![],
            Pointwise,
            "Multiply two fields",
            false,
        ),
        op("div", two, 1, vec![], Pointwise, "Divide two fields", false),
        op(
            "addc",
            one,
            1,
            vec![arg("c", Float)],
            Pointwise,
            "Add a constant",
            false,
        ),
        op(
            "subc",
            one,
            1,
            vec![arg("c", Float)],
            Pointwise,
            "Subtract a constant",
            false,
        ),
        op(
            "mulc",
            one,
            1,
            vec![arg("c", Float)],
            Pointwise,
            "Multiply by a constant",
            false,
        ),
        op(
            "divc",
            one,
            1,
            vec![arg("c", Float)],
            Pointwise,
            "Divide by a constant",
            false,
        ),
        op(
            "ifthen",
            two,
            1,
            vec![],
            Pointwise,
            "If the first field is non-zero, take the second, else missing",
            false,
        ),
        op(
            "copy",
            Variadic,
            1,
            vec![],
            Pointwise,
            "Copy (and concatenate) datasets",
            false,
        ),
        op(
            "setgrid",
            one,
            1,
            // a mesh spec may carry options: setgrid,fesom:<dir>,units=deg
            vec![
                arg("grid", Str),
                ArgSpec {
                    name: "mesh_option",
                    ty: Str,
                    default: Some(""),
                    repeated: true,
                },
            ],
            Pointwise,
            "Set the horizontal grid (grid file or mesh)",
            false,
        ),
        // remapping
        op(
            "remap",
            one,
            1,
            vec![arg("grid", Str), arg("weights", Str)],
            WholeExtent,
            "Remap with a SCRIP weight file",
            false,
        ),
        op(
            "hpdegrade",
            one,
            1,
            vec![args_of("params", Str)],
            WholeExtent,
            "Degrade a HEALPix grid: mean over nested children \
             (nside=<n>|zoom=<z>|fact=<f>[,order=nested|ring][,stat=mean|avg][,power=<p>])",
            false,
        ),
        op(
            "toelems",
            one,
            1,
            vec![],
            WholeExtent,
            "Map a variable on the nodes of a model mesh to its elements: the mean of each \
             element's nodes (cdors operator, needs -setgrid,<mesh>)",
            false,
        ),
        op(
            "tonodes",
            one,
            1,
            vec![],
            WholeExtent,
            "Map a variable on the elements of a model mesh to its nodes: the area-weighted mean \
             of the elements around each node (cdors operator, needs -setgrid,<mesh>)",
            false,
        ),
        op(
            "selpoints",
            one,
            1,
            vec![args_of("params", Str)],
            WholeExtent,
            "Values at stations (cdors operator): file=<csv: name,lon,lat>|lon=<x>_lat=<y>[,...]\
             [,method=nn|linear]; dimension station with station_name",
            false,
        ),
        op(
            "transect",
            one,
            1,
            vec![args_of("params", Str)],
            WholeExtent,
            "Values along a great-circle path (cdors operator): <lon0>,<lat0>,<lon1>,<lat1>[,...] \
             or file=<GeoJSON LineString>[,step=<km>][,method=nn|linear]; dimension distance \
             (km) with lon, lat and, on a model mesh, bottom_depth; missing below the bottom",
            false,
        ),
        op(
            "moc",
            one,
            1,
            vec![ArgSpec {
                default: Some("dlat=1"),
                ..args_of("params", Str)
            }],
            Reduction,
            "Meridional overturning streamfunction from the vertical velocity on a model mesh \
             (cdors operator): [dlat=<deg>|lat=<lat>[/<lat>...]][,regions=<set>,select=<abbrev>]; \
             -sum of element-mean w times elem_area north of each latitude line, Sv; \
             moc(time, nz, lat, lon)",
            true,
        ),
        op(
            "transport",
            two,
            1,
            vec![args_of("params", Str)],
            Reduction,
            "Transport across a section of a model mesh (cdors operator): \
             <lon0>,<lat0>,<lon1>,<lat1>[,...] or file=<GeoJSON LineString>; two inputs, the \
             eastward and the northward velocity on the elements and layers; the flux across \
             the broken lines through the crossed elements (FESOM2's dual-cell faces) times the \
             layer thickness, Sv, positive from the right of the path to its left; \
             transport(time, nz1, lat, lon)",
            true,
        ),
        op(
            "hpupgrade",
            one,
            1,
            vec![args_of("params", Str)],
            WholeExtent,
            "Upgrade a HEALPix grid: copy to nested children \
             (nside=<n>|zoom=<z>|fact=<f>[,order=nested|ring][,power=<p>])",
            false,
        ),
        // multi-file
        op(
            "mergetime",
            Variadic,
            1,
            vec![],
            Pointwise,
            "Merge datasets sorted by time",
            false,
        ),
        op(
            "cat",
            Variadic,
            1,
            vec![],
            Pointwise,
            "Concatenate datasets",
            false,
        ),
    ];
    // percentiles, running statistics, climatology arithmetic
    for (name, d) in pctl::operators() {
        r.push(op(
            &name,
            OneOrThree,
            1,
            vec![arg("p", Float)],
            WholeExtent,
            &d,
            true,
        ));
    }
    for (name, d) in runstat::operators() {
        r.push(op(
            &name,
            one,
            1,
            vec![arg("nts", Int)],
            WholeExtent,
            &d,
            true,
        ));
    }
    for (name, d) in ymonarith::operators() {
        r.push(op(&name, two, 1, vec![], Pointwise, &d, true));
    }
    // space statistics
    for (name, d) in regstat::operators() {
        r.push(op(
            &name,
            one,
            1,
            vec![args_of("params", Str)],
            Reduction,
            &d,
            true,
        ));
    }
    for (name, d) in fldstat::operators() {
        let args = if fldstat::takes_zonaldes(&name) {
            vec![opt("zonaldes", Str, "none")]
        } else if name == "vertint" {
            vec![opt("z0/z1", Str, "none")]
        } else {
            vec![]
        };
        r.push(op(&name, one, 1, args, Reduction, &d, true));
    }
    // vertical geometry
    for (name, d) in vertical::operators() {
        let (args, class) = if name == "intlevel" {
            (vec![args_of("levels", Str)], WholeExtent)
        } else {
            (vec![opt("z0/z1", Str, "none")], Reduction)
        };
        r.push(op(name, one, 1, args, class, d, true));
    }
    for (name, d) in fldhist::operators() {
        r.push(op(
            &name,
            one,
            1,
            vec![arg("bins", Str), opt("out", Str, "area")],
            Reduction,
            &d,
            true,
        ));
    }
    for m in ["nn", "dis", "bil", "con", "ycon"] {
        r.push(op(
            &format!("remap{m}"),
            one,
            1,
            vec![arg("grid", Str)],
            WholeExtent,
            &format!("Remap ({m}) to a target grid"),
            false,
        ));
    }
    let periods = [
        ("tim", "over all timesteps"),
        ("hour", "per hour"),
        ("day", "per day"),
        ("mon", "per month"),
        ("seas", "per season"),
        ("year", "per year"),
        ("ymon", "per calendar month over all years"),
        ("yday", "per day of year over all years"),
        ("yseas", "per season over all years"),
    ];
    for (p, pd) in periods {
        for (s, sd) in [
            ("mean", "Mean"),
            ("avg", "Average (missing if any value is missing)"),
            ("min", "Minimum"),
            ("max", "Maximum"),
            ("sum", "Sum"),
            ("range", "Range (maximum - minimum)"),
            ("std", "Standard deviation (n)"),
            ("std1", "Standard deviation (n-1)"),
            ("var", "Variance (n)"),
            ("var1", "Variance (n-1)"),
        ] {
            r.push(op(
                &format!("{p}{s}"),
                one,
                1,
                vec![],
                Reduction,
                &format!("{sd} {pd}"),
                false,
            ));
        }
    }
    for o in &mut r {
        if IMPLEMENTED.contains(&o.name.as_str()) || timstat::parse(&o.name).is_some() {
            o.implemented = true;
        }
        if o.name == "showname" {
            o.aliases.push("showvar");
        }
        if o.name == "outputtab" {
            o.aliases.push("outputkey");
        }
        // the one place that decides the origin: cdo 2.6.5's catalogue (names and aliases)
        if catalog::cdo_operator(&o.name).is_none() {
            o.origin = "cdors";
        }
    }
    r
}

static REGISTRY: LazyLock<Vec<OpSpec>> = LazyLock::new(build_registry);

/// All registered operators.
pub fn registry() -> &'static [OpSpec] {
    &REGISTRY
}

/// Looks up an operator by name or alias.
pub fn lookup(name: &str) -> Option<&'static OpSpec> {
    REGISTRY
        .iter()
        .find(|o| o.name == name || o.aliases.contains(&name))
}

fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, &cb) in b.iter().enumerate() {
            cur[j + 1] = (prev[j + 1] + 1)
                .min(cur[j] + 1)
                .min(prev[j] + usize::from(ca != cb));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Registered names closest to `name` (edit distance at most 2, or a prefix match).
pub fn suggestions(name: &str) -> Vec<&'static str> {
    let mut scored: Vec<(usize, &'static str)> = REGISTRY
        .iter()
        .flat_map(|o| std::iter::once(o.name.as_str()).chain(o.aliases.iter().copied()))
        .map(|n| (levenshtein(name, n), n))
        .filter(|&(d, n)| d <= 2 || (name.len() >= 4 && n.starts_with(name)))
        .collect();
    scored.sort();
    scored.into_iter().take(3).map(|(_, n)| n).collect()
}

/// Names of the implemented operators (and their aliases).
pub fn implemented_names() -> Vec<&'static str> {
    REGISTRY
        .iter()
        .filter(|o| o.implemented)
        .flat_map(|o| std::iter::once(o.name.as_str()).chain(o.aliases.iter().copied()))
        .collect()
}

/// The error for an operator name cdors does not register: `not_implemented` for an operator
/// of cdo's catalog, otherwise `unknown_operator` with did-you-mean candidates from cdo's
/// catalog, implemented operators first.
pub fn unknown_operator(name: &str) -> Error {
    if let Some(c) = catalog::cdo_operator(name) {
        return Error::new(
            ErrorCode::NotImplemented,
            format!(
                "cdo operator '{name}' ({}) is not implemented in cdors",
                c.description
            ),
        )
        .with("operator", name)
        .with("section", c.section)
        .with_hint("list the implemented operators with `cdors ops`; run this step with cdo");
    }
    let implemented = implemented_names();
    let sugg = catalog::suggest(name, &implemented);
    let e = Error::new(
        ErrorCode::UnknownOperator,
        format!("unknown operator '{name}'"),
    )
    .with("operator", name);
    if sugg.is_empty() {
        return e.with_hint("see the operator list with `cdors ops`");
    }
    let marked: Vec<String> = sugg
        .iter()
        .map(|s| {
            if implemented.contains(s) {
                (*s).to_owned()
            } else {
                format!("{s} (cdo only, not in cdors)")
            }
        })
        .collect();
    e.with("suggestions", sugg.clone())
        .with_hint(format!("did you mean {}?", marked.join(" or ")))
}

/// Returns the spec of an implemented operator, or `not_implemented`.
pub fn require_implemented(node: &OpNode) -> Result<&'static OpSpec> {
    let spec = lookup(&node.name).ok_or_else(|| unknown_operator(&node.name))?;
    if !spec.implemented {
        return Err(Error::new(
            ErrorCode::NotImplemented,
            format!("operator '{}' is not implemented yet", node.name),
        )
        .with("operator", node.name.clone())
        .with_hint("list the implemented operators with `cdors ops`; run this step with cdo"));
    }
    Ok(spec)
}

/// Number of leading inputs of `node` that are evaluated: the percentile operators ignore the
/// min/max inputs of cdo's three-input form, which are therefore never opened or read.
pub fn used_inputs(node: &OpNode) -> usize {
    if pctl::handles(&node.name) {
        node.inputs.len().min(1)
    } else {
        node.inputs.len()
    }
}

/// Output description of a data operator from the descriptions of its inputs (no data read).
pub fn describe(node: &OpNode, inputs: Vec<Desc>, srcs: &mut Sources) -> Result<Desc> {
    // method notes: the inputs' first, then what this operator adds (its own notes, and for a
    // reduction what its kernel weighs with), each note once
    let mut method: Vec<serde_json::Value> = Vec::new();
    let add = |n: serde_json::Value, m: &mut Vec<serde_json::Value>| {
        if !m.contains(&n) {
            m.push(n);
        }
    };
    for n in inputs.iter().flat_map(|d| d.method.iter()) {
        add(n.clone(), &mut method);
    }
    let mut out = describe_op(node, inputs, srcs)?;
    let own = std::mem::take(&mut out.method);
    // an operator that wrote its own note of the method (moc) needs no note of its kernel
    let token = serde_json::Value::from(crate::plan::explain::op_token(node));
    let noted = own.iter().any(|n| n.get("operator") == Some(&token));
    for n in own {
        add(n, &mut method);
    }
    if let Some(n) = out
        .fold
        .as_ref()
        .filter(|_| !noted)
        .and_then(|f| fold_note(node, f.kernel.plan_info()))
    {
        add(n, &mut method);
    }
    out.method = method;
    Ok(out)
}

/// The method note of a reduction from its kernel's `--plan` facts: the area weights (with the
/// layers of vertical ranges) and warnings; mesh weights only where there are no area weights
/// (region membership, sampling), without their byte counts.
pub(crate) fn fold_note(
    node: &OpNode,
    info: Option<serde_json::Value>,
) -> Option<serde_json::Value> {
    let info = info?;
    let mut n = serde_json::Map::new();
    if let Some(a) = info.get("area_weights").filter(|a| a.as_array().is_some_and(|a| !a.is_empty())) {
        n.insert("area_weights".into(), a.clone());
    } else if let Some(m) = info.get("mesh_weights").and_then(|m| m.as_array()) {
        let m: Vec<serde_json::Value> = m
            .iter()
            .map(|e| {
                let mut e = e.clone();
                if let Some(o) = e.as_object_mut() {
                    o.retain(|k, _| !matches!(k.as_str(), "bytes" | "loaded" | "loaded_bytes"));
                }
                e
            })
            .collect();
        if !m.is_empty() {
            n.insert("mesh_weights".into(), m.into());
        }
    }
    if let Some(w) = info.get("warnings").filter(|w| w.as_array().is_some_and(|w| !w.is_empty())) {
        n.insert("warnings".into(), w.clone());
    }
    if n.is_empty() {
        return None;
    }
    let mut o = serde_json::Map::new();
    o.insert("operator".into(), crate::plan::explain::op_token(node).into());
    o.extend(n);
    Some(o.into())
}

fn describe_op(node: &OpNode, inputs: Vec<Desc>, srcs: &mut Sources) -> Result<Desc> {
    let spec = require_implemented(node)?;
    // time metadata only: a fold before it stays pending
    if settime::handles(&node.name) {
        return settime::describe(node, inputs);
    }
    // the result of a fold used as input ends the inner stage: it is kept in memory as an
    // intermediate and read back like a stored dataset (`plan::intermediate`)
    let inputs = inputs
        .into_iter()
        .map(|d| {
            if d.fold.is_some() {
                crate::plan::intermediate::materialize(d, &node.name, srcs)
            } else {
                Ok(d)
            }
        })
        .collect::<Result<Vec<_>>>()?;
    match node.name.as_str() {
        n if remap::handles(n) => return remap::describe(node, inputs, srcs),
        "hpdegrade" | "hpupgrade" => return healpix::describe(node, inputs),
        n if places::handles(n) => return places::describe(node, inputs),
        n if sample::handles(n) => return sample::describe(node, inputs, srcs),
        n if pctl::handles(n) => return pctl::describe(node, inputs, srcs),
        n if runstat::handles(n) => return runstat::describe(node, inputs, srcs),
        n if ymonarith::handles(n) => return ymonarith::describe(node, inputs),
        n if regions::handles(n) => return regions::describe(node, inputs, srcs),
        n if regstat::handles(n) => return regstat::describe(node, inputs, srcs),
        n if moc::handles(n) => return moc::describe(node, inputs),
        n if transport::handles(n) => return transport::describe(node, inputs, srcs),
        n if vertical::handles(n) => return vertical::describe(node, inputs, srcs),
        _ => {}
    }
    match spec.class {
        AccessClass::Selection => select::describe(node, inputs),
        AccessClass::Pointwise => match node.name.as_str() {
            "copy" | "setgrid" => files::describe(node, inputs, srcs),
            _ => arith::describe(node, inputs),
        },
        AccessClass::Info => Err(Error::bad_arguments(format!(
            "'{}' prints information and cannot be the input of another operator",
            node.name
        ))),
        AccessClass::Reduction if timstat::parse(&node.name).is_some() => {
            timstat::describe(node, inputs, srcs)
        }
        AccessClass::Reduction if fldstat::handles(&node.name) => {
            fldstat::describe(node, inputs, srcs)
        }
        AccessClass::Reduction if node.name == "fldhist" => fldhist::describe(node, inputs, srcs),
        AccessClass::Reduction | AccessClass::WholeExtent => Err(Error::new(
            ErrorCode::NotImplemented,
            format!("operator '{}' is not implemented yet", node.name),
        )),
    }
}
