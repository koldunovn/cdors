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

pub mod arith;
pub mod catalog;
pub mod files;
pub mod fldstat;
pub mod healpix;
pub mod info;
pub mod remap;
pub mod select;
pub mod timstat;

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
    "mergetime",
    "cat",
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
            vec![arg("grid", Str)],
            Pointwise,
            "Set the horizontal grid (grid file or mesh)",
            false,
        ),
        // whole extent
        op(
            "timpctl",
            Fixed(3),
            1,
            vec![arg("p", Float)],
            WholeExtent,
            "Percentile over all timesteps (min/max inputs accepted, not used)",
            false,
        ),
        op(
            "monpctl",
            Fixed(3),
            1,
            vec![arg("p", Float)],
            WholeExtent,
            "Monthly percentile",
            false,
        ),
        op(
            "yearpctl",
            Fixed(3),
            1,
            vec![arg("p", Float)],
            WholeExtent,
            "Yearly percentile",
            false,
        ),
        op(
            "runmean",
            one,
            1,
            vec![arg("nts", Int)],
            WholeExtent,
            "Running mean over nts timesteps",
            false,
        ),
        op(
            "ymonsub",
            two,
            1,
            vec![],
            Pointwise,
            "Subtract the multi-year monthly climatology",
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
    // space statistics
    for (name, d) in fldstat::operators() {
        r.push(op(&name, one, 1, vec![], Reduction, &d, true));
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

/// The `unknown_operator` error with a did-you-mean hint.
pub fn unknown_operator(name: &str) -> Error {
    let sugg = suggestions(name);
    let mut e = Error::new(
        ErrorCode::UnknownOperator,
        format!("unknown operator '{name}'"),
    )
    .with("operator", name);
    e = if sugg.is_empty() {
        e.with_hint(
            "see the operator list with `cdors ops` (cdo operators not listed are not implemented)",
        )
    } else {
        e.with("suggestions", sugg.clone())
            .with_hint(format!("did you mean {}?", sugg.join(" or ")))
    };
    e
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
        .with_hint(format!(
            "implemented now: {}",
            REGISTRY
                .iter()
                .filter(|o| o.implemented)
                .map(|o| o.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    Ok(spec)
}

/// Output description of a data operator from the descriptions of its inputs (no data read).
pub fn describe(node: &OpNode, inputs: Vec<Desc>, srcs: &mut Sources) -> Result<Desc> {
    let spec = require_implemented(node)?;
    if inputs.iter().any(|d| d.fold.is_some()) {
        return Err(Error::new(
            ErrorCode::NotImplemented,
            format!(
                "operator '{}' cannot take the output of a statistic as input yet",
                node.name
            ),
        )
        .with("operator", node.name.clone())
        .with_hint("put selections inside the statistic (-yearmean -selname,tas in.nc), or run two commands"));
    }
    match node.name.as_str() {
        n if remap::handles(n) => return remap::describe(node, inputs),
        "hpdegrade" | "hpupgrade" => return healpix::describe(node, inputs),
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
        AccessClass::Reduction | AccessClass::WholeExtent => Err(Error::new(
            ErrorCode::NotImplemented,
            format!("operator '{}' is not implemented yet", node.name),
        )),
    }
}
