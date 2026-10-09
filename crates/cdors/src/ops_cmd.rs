//! `cdors ops [--json]` and `cdors help <operator>` (also `cdors -h <operator>`).
//!
//! - `ops` lists the implemented operators with arity, arguments (name, type, default), access
//!   class and a one-line description, then every other operator of cdo's catalog by name, with
//!   its section and description, marked `"implemented": false`.
//! - `help <op>` prints cdo's own help text for an implemented operator, followed by the cdors
//!   usage line and short notes where cdors deliberately deviates (`docs/deviations.md`). For a
//!   cdo operator that cdors does not implement it fails with `not_implemented` (exit 1); for an
//!   unknown name with `unknown_operator` and did-you-mean candidates.

use cdors_core::error::{Error, ErrorCode, Result};
use cdors_core::ops::{self, AccessClass, Inputs, OpSpec, catalog};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::fmt::Write;

fn class_name(c: AccessClass) -> &'static str {
    match c {
        AccessClass::Selection => "selection",
        AccessClass::Pointwise => "pointwise",
        AccessClass::Reduction => "reduction",
        AccessClass::WholeExtent => "whole_extent",
        AccessClass::Info => "info",
    }
}

fn n_inputs(s: &OpSpec) -> i64 {
    match s.inputs {
        Inputs::Fixed(n) => n as i64,
        Inputs::Variadic => -1,
        // cdo's catalog lists the percentile operators with 3 inputs (data, min, max)
        Inputs::OneOrThree => 3,
    }
}

/// `name,arg1,[arg2=default],names...` as in cdo's synopsis.
fn usage(s: &OpSpec) -> String {
    let args: Vec<String> = s
        .args
        .iter()
        .map(|a| match (a.default, a.repeated) {
            (Some(d), _) => format!("[{}={d}]", a.name),
            (None, true) => format!("{}...", a.name),
            (None, false) => a.name.to_owned(),
        })
        .collect();
    if args.is_empty() {
        s.name.clone()
    } else {
        format!("{},{}", s.name, args.join(","))
    }
}

fn implemented() -> Vec<&'static OpSpec> {
    ops::registry().iter().filter(|o| o.implemented).collect()
}

/// `cdors ops [--json]`.
pub fn ops(json: bool) -> String {
    let imp = implemented();
    let names: HashSet<&str> = imp
        .iter()
        .flat_map(|o| std::iter::once(o.name.as_str()).chain(o.aliases.iter().copied()))
        .collect();
    let others: Vec<&catalog::CdoOperator> = catalog::cdo_operators()
        .iter()
        .filter(|c| !names.contains(c.name))
        .collect();
    if json {
        let mut list: Vec<Value> = imp
            .iter()
            .map(|o| {
                json!({
                    "name": o.name,
                    "implemented": true,
                    "aliases": o.aliases,
                    "usage": usage(o),
                    "inputs": n_inputs(o),
                    "outputs": o.outputs,
                    "args": o.args.iter().map(|a| json!({
                        "name": a.name,
                        "type": a.ty,
                        "default": a.default,
                        "repeated": a.repeated,
                    })).collect::<Vec<_>>(),
                    "class": class_name(o.class),
                    "description": o.description,
                    "cdo_section": catalog::cdo_operator(&o.name).map(|c| c.section),
                    "notes": notes(&o.name),
                })
            })
            .collect();
        list.extend(others.iter().map(|c| {
            json!({
                "name": c.name,
                "implemented": false,
                "section": c.section,
                "description": c.description,
                "inputs": c.n_inputs,
                "outputs": c.n_outputs,
            })
        }));
        return format!(
            "{}\n",
            json!({
                "implemented_count": imp.len(),
                "not_implemented_count": others.len(),
                "operators": list,
            })
        );
    }
    let mut s = String::new();
    let _ = writeln!(
        s,
        "Operators implemented in cdors ({}); `cdors help <op>` shows cdo's help and cdors notes.",
        imp.len()
    );
    for class in [
        AccessClass::Info,
        AccessClass::Selection,
        AccessClass::Pointwise,
        AccessClass::Reduction,
        AccessClass::WholeExtent,
    ] {
        let _ = writeln!(s, "\n{}:", class_name(class));
        for o in imp.iter().filter(|o| o.class == class) {
            let inputs = match o.inputs {
                Inputs::Fixed(1) => String::new(),
                Inputs::Fixed(n) => format!(" [{n} inputs]"),
                Inputs::Variadic => " [1 or more inputs]".to_owned(),
                Inputs::OneOrThree => " [1 input, or cdo's 3: data min max]".to_owned(),
            };
            let _ = writeln!(s, "  {:<28} {}{inputs}", usage(o), o.description);
        }
    }
    let _ = writeln!(
        s,
        "\ncdo operators not implemented in cdors ({}), by section (descriptions: cdors ops --json):",
        others.len()
    );
    let mut sections: Vec<&str> = Vec::new();
    for c in &others {
        if !sections.contains(&c.section) {
            sections.push(c.section);
        }
    }
    for sec in sections {
        let names: Vec<&str> = others
            .iter()
            .filter(|c| c.section == sec)
            .map(|c| c.name)
            .collect();
        let _ = writeln!(s, "  {sec}:");
        let mut line = String::from("   ");
        for n in names {
            if line.len() + n.len() + 1 > 100 {
                let _ = writeln!(s, "{line}");
                line = String::from("   ");
            }
            line.push(' ');
            line.push_str(n);
        }
        let _ = writeln!(s, "{line}");
    }
    s
}

/// cdors notes for an operator: deliberate deviations from cdo, each pointing at its entry in
/// `docs/deviations.md` (section: entry).
pub fn notes(name: &str) -> Vec<String> {
    let timstat = cdors_core::ops::timstat::parse(name).is_some();
    let is = |p: &[&str]| p.iter().any(|x| name.starts_with(x));
    let mut n: Vec<&str> = Vec::new();
    if timstat {
        n.push(
            "cell_methods is written for every time statistic (cdo: only for some, and only \
             with time bounds) [Statistics: cell_methods]",
        );
        n.push(
            "inputs with time units `months since`/`years since` get an output time axis in \
             `days since` the same reference [Time axis: months or years since]",
        );
        if name.contains("std") || name.contains("var") {
            n.push(
                "variance uses Welford's update instead of cdo's one-pass sum of squares; \
                 results agree to about 1 float32 ulp [Statistics: Variance and standard deviation]",
            );
        }
        if name.starts_with("yseas") {
            n.push("timestamps follow cdo 2.6.0, not 2.6.5 [Time axis: yseas* timestamps]");
        }
    }
    if name.contains("pctl") {
        n.push(
            "percentiles are exact for any group size (cdo: histogram above 50 values); the \
             min/max inputs of cdo's three-input form are accepted and ignored \
             [Statistics: Exact percentiles]",
        );
    }
    if is(&["fld", "zon", "mer", "vert"]) {
        n.push(
            "cell_methods (area: ..., longitude: ..., vertical) is added to the output \
             [Space statistics: cell_methods]",
        );
    }
    if is(&["zon"]) {
        n.push(
            "on HEALPix grids each ring is summed in nested order [Space statistics: HEALPix zon*]",
        );
    }
    if is(&["vert"]) {
        n.push("accumulates in double precision [Space statistics: vert*]");
    }
    if is(&["remap"]) {
        n.push(
            "weights are generated once by cdo for the unmasked grid and cached in \
             $CDORS_CACHE/weights; missing values are renormalised per method \
             [Remapping: weights for the unmasked grid]",
        );
        n.push("--force is accepted and ignored [Remapping: --force]");
    }
    if name == "hpdegrade" {
        n.push("zoom=0 degrades to nside 1, as cdo 2.6.5 [Remapping: hpdegrade,zoom=0]");
    }
    if is(&["seltimestep", "seldate", "selyear", "selmon", "selseason"]) {
        n.push(
            "a selection that leaves no timestep is an error (cdo warns and writes an empty \
             file) [Time axis: empty time selection]",
        );
    }
    if ops::output::handles(name) {
        n.push(
            "takes any operator chain as input; refuses to print more than --max-values values \
             (default 1000000; info/infon: fields) [Printing values: Flood guard]",
        );
        n.push(
            "--json prints one JSON object: records with ISO dates, numbers, null for missing \
             values [Printing values: --json]",
        );
    }
    if matches!(name, "cat" | "mergetime" | "copy") {
        n.push("never appends to an existing output [Files and outputs: no append]");
    }
    if ops::lookup(name).is_some_and(|s| s.outputs == 1) {
        n.push(
            "an existing output is refused unless -O is given [Files and outputs: no silent \
             overwrite]",
        );
    }
    n.into_iter().map(str::to_owned).collect()
}

/// `cdors help <op>`.
pub fn help(name: &str, json: bool) -> Result<String> {
    let spec = ops::lookup(name).filter(|s| s.implemented);
    let Some(spec) = spec else {
        // not implemented (cdo operator) or unknown
        let e = ops::unknown_operator(name);
        if e.code == ErrorCode::NotImplemented {
            let c = catalog::cdo_operator(name).expect("cdo operator");
            return Err(Error::new(
                ErrorCode::NotImplemented,
                format!(
                    "{} - {} (cdo section {}): not implemented in cdors",
                    c.name, c.description, c.section
                ),
            )
            .with("operator", name)
            .with("cdo_section", c.section)
            .with("description", c.description)
            .with_hint("list the implemented operators with `cdors ops`; run this step with cdo"));
        }
        // registered but not implemented yet (and not in cdo's catalog)
        if let Some(s) = ops::lookup(name) {
            return Err(Error::new(
                ErrorCode::NotImplemented,
                format!(
                    "{} - {}: not implemented in cdors yet",
                    s.name, s.description
                ),
            )
            .with("operator", name));
        }
        return Err(e);
    };
    let cdo_help = catalog::cdo_help(&spec.name);
    let notes = notes(&spec.name);
    if json {
        return Ok(format!(
            "{}\n",
            json!({
                "operator": spec.name,
                "usage": usage(spec),
                "inputs": n_inputs(spec),
                "outputs": spec.outputs,
                "class": class_name(spec.class),
                "description": spec.description,
                "cdo_help": cdo_help,
                "notes": notes,
            })
        ));
    }
    let mut s = String::new();
    match cdo_help {
        Some(h) => {
            s.push_str(h.trim_end());
            s.push('\n');
        }
        None => {
            let _ = writeln!(s, "{} - {}", spec.name, spec.description);
        }
    }
    let _ = writeln!(s, "\ncdors:");
    let inputs = match spec.inputs {
        Inputs::Fixed(n) => n.to_string(),
        Inputs::Variadic => "1 or more".to_owned(),
        Inputs::OneOrThree => "1 or 3 (data min max; min and max are not read)".to_owned(),
    };
    let _ = writeln!(
        s,
        "    usage: cdors -{} <{} input(s)>{}   (access class: {})",
        usage(spec),
        inputs,
        if spec.outputs == 1 { " <output>" } else { "" },
        class_name(spec.class)
    );
    if !notes.is_empty() {
        let _ = writeln!(s, "    notes (docs/deviations.md, [section: entry]):");
        for n in &notes {
            let _ = writeln!(s, "    - {n}");
        }
    }
    Ok(s)
}
