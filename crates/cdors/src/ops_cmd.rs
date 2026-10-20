//! `cdors ops [--json]` and `cdors help <operator>` (also `cdors -h <operator>`).
//!
//! - `ops` lists the implemented operators with arity, arguments (name, type, default), access
//!   class and a one-line description, then every other operator of cdo's catalog by name, with
//!   its section and description, marked `"implemented": false`.
//! - `help <op>` prints cdo's own help text for an implemented cdo operator, or the cdors text
//!   of an operator cdo does not have (`cdors_help.txt`, origin `cdors`), followed by the cdors
//!   usage line, the cdors extensions of a cdo operator and short notes where cdors deliberately
//!   deviates (`docs/deviations.md`). For a cdo operator that cdors does not implement it fails
//!   with `not_implemented` (exit 1); for an unknown name with `unknown_operator` and
//!   did-you-mean candidates.

use cdors_core::error::{Error, ErrorCode, Result};
use cdors_core::ops::{self, AccessClass, Inputs, OpSpec, catalog};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::sync::LazyLock;

/// Help texts of the cdors operators, each after a line `%%% <op> [<op>...]`.
const CDORS_HELP_TXT: &str = include_str!("cdors_help.txt");

static CDORS_HELP: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    for part in CDORS_HELP_TXT.split("\n%%% ").skip(1) {
        let (names, body) = part.split_once('\n').expect("cdors_help.txt: names line");
        for n in names.split_whitespace() {
            m.insert(n, body);
        }
    }
    m
});

/// The help text of a cdors operator (origin `cdors`).
fn cdors_help(name: &str) -> Option<&'static str> {
    CDORS_HELP.get(name).copied()
}

/// The SYNOPSIS lines of operator `name` in a cdors help text, without the leading `cdors `
/// (`<operator>` replaced by the name; a text for several operators keeps the lines of `name`).
fn synopsis(name: &str, help: &str) -> Vec<String> {
    help.lines()
        .skip_while(|l| *l != "SYNOPSIS")
        .skip(1)
        .take_while(|l| l.starts_with("    cdors "))
        .map(|l| {
            l.trim()
                .trim_start_matches("cdors ")
                .replace("<operator>", name)
        })
        .filter(|l| {
            l.strip_prefix('-')
                .and_then(|r| r.strip_prefix(name))
                .is_some_and(|r| r.starts_with([',', ' ', '[']))
        })
        .collect()
}

/// What cdors adds to a cdo operator (it keeps cdo's name and semantics otherwise).
pub fn extensions(name: &str) -> Vec<&'static str> {
    let mut e = Vec::new();
    let is = |p: &str| name.starts_with(p) && ops::lookup(name).is_some_and(|s| s.origin == "cdo");
    if name == "setgrid" {
        e.push(
            "setgrid,fesom:<dir>[,units=deg|rad]: a FESOM2 mesh (fesom.mesh.diag.nc in <dir>): \
             nodes and elements with their areas per level and wet levels; dry levels become \
             missing; `cdors mesh info fesom:<dir>` describes it",
        );
    }
    if name == "maskregion" {
        e.push(
            "maskregion,regions=<set>[,select=<abbrev,...>][,fill=nearest]: the union of \
             catalog regions (regmask's sets and rules) as the mask",
        );
    }
    if matches!(name, "zonmean" | "zonsum" | "zonint") {
        e.push(
            "zonal_<DY> on a model mesh: each cell in the band of its centre, with its (per-level) \
             area",
        );
    }
    if is("fld") {
        e.push(
            "on a model mesh (-setgrid,fesom:): the mesh's cell areas, per level for variables \
             with the mesh's levels; dry levels left out",
        );
    }
    if is("vert") {
        e.push("on a model mesh: the mesh's layer thicknesses over each cell's wet levels");
    }
    if name == "intlevel" {
        e.push("on a model mesh: missing above each cell's top and below its bottom");
    }
    if name == "bottomvalue" {
        e.push("on a model mesh: the value at each cell's own deepest wet level");
    }
    e
}

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
            (Some(_), true) => format!("[{}...]", a.name),
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
                let mut v = json!({
                    "name": o.name,
                    "implemented": true,
                    "origin": o.origin,
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
                    "section": catalog::cdo_operator(&o.name).map(|c| c.section),
                    "notes": notes(&o.name),
                });
                if let Some(h) = cdors_help(&o.name) {
                    v["synopsis"] = json!(synopsis(&o.name, h));
                }
                let ext = extensions(&o.name);
                if !ext.is_empty() {
                    v["extensions"] = json!(ext);
                }
                v
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
            // a cdors operator: the first line of its synopsis, without the files
            let u = cdors_help(&o.name)
                .and_then(|h| synopsis(&o.name, h).into_iter().next())
                .map_or_else(
                    || usage(o),
                    |l| l[1..].split(' ').next().unwrap_or_default().to_owned(),
                );
            let _ = writeln!(s, "  {u:<28} {}{inputs}", o.description);
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
    // not "mer": mer* is not implemented, and the prefix would match mergetime
    if is(&["fld", "zon", "vert"]) {
        n.push(
            "cell_methods (area: ..., longitude: ..., vertical) is added to the output \
             [Space statistics: cell_methods]",
        );
    }
    if cdors_core::model::area::fldstat_weighting(name)
        == Some(cdors_core::model::area::SpaceWeighting::Weights)
    {
        n.push(
            "weighted by cell area; a grid without cell areas or bounds gets equal weights with a \
             framed warning (cdo: one line) and a `warnings` entry in --plan \
             [Space statistics: Equal area weights are announced loudly]; reduced Gaussian grids \
             are weighted by cell area, where cdo weights them equally \
             [Space statistics: Reduced Gaussian grids are weighted by cell area]",
        );
    }
    if is(&["zon"]) {
        n.push(
            "on HEALPix grids each ring is summed in nested order [Space statistics: HEALPix zon*]",
        );
    }
    if cdors_core::ops::fldstat::takes_zonaldes(name) {
        n.push(
            "zonal_<DY> latitude bands as cdo (also zonaldes=zonal_<DY>; zonsum and zonint as cdo \
             2.6.5) [Space statistics: Latitude bands zonal_<DY>]; on a model mesh a cell is in \
             the band of its centre, with its (per-level) area [Space statistics: Latitude bands \
             on model meshes]",
        );
    }
    if is(&["vert"]) {
        n.push("accumulates in double precision [Space statistics: vert*]");
    }
    // `remap,<grid>,<weights>` uses the given weight file: nothing is generated
    if is(&["remap"]) && name != "remap" {
        n.push(
            "weights are generated once by cdo for the unmasked grid and cached in \
             $CDORS_CACHE/weights; missing values are renormalised per method \
             [Remapping: weights for the unmasked grid]",
        );
    }
    if is(&["remap"]) {
        n.push("--force is accepted and ignored [Remapping: --force]");
    }
    if name == "selregion" {
        n.push(
            "each selected cell is written once, in ascending order (cdo 2.6.0 repeats the cells \
             of earlier polygons for every further polygon) [Regions: selregion with several \
             polygons]",
        );
    }
    if matches!(name, "selregion" | "maskregion") {
        n.push(
            "dcw:<code> is not implemented: use regions=ne.countries_50,select=<country name> \
             [Regions: dcw]",
        );
    }
    if name == "maskregion" {
        n.push(
            "regions=<set>[,select=<abbrev,...>][,fill=nearest] masks with catalog regions \
             (regionmask's rules), a cdors extension [Regions: maskregion,regions=]",
        );
    }
    if name == "hpdegrade" {
        n.push("zoom=0 degrades to nside 1, as cdo 2.6.5 [Remapping: hpdegrade,zoom=0]");
    }
    if is(&["seltimestep", "seldate", "selyear", "selmon", "selseason"]) {
        n.push(
            "a selection that leaves no timestep fails before reading (bad_arguments), as cdo \
             fails; inside a chain cdo 2.6.0 crashes instead [cdo bugs observed: empty time \
             selection in a chain]",
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
    let mut n: Vec<String> = n.into_iter().map(str::to_owned).collect();
    if name == "maskregion" {
        n.push(
            "regions=: membership by regmask's rules (overlapping sets list a cell under every \
             region; others under the last one), fill=nearest reported as regions_filled \
             [Regions: regmask]"
                .to_owned(),
        );
    }
    if matches!(name, "regmask" | "maskregion" | "moc") || ops::regstat::handles(name) {
        n.extend(
            ops::regions::catalog_text()
                .into_iter()
                .map(|t| format!("region set {t}")),
        );
    }
    n
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
            .with("section", c.section)
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
    let own = cdors_help(&spec.name);
    let notes = notes(&spec.name);
    let ext = extensions(&spec.name);
    if json {
        return Ok(format!(
            "{}\n",
            json!({
                "operator": spec.name,
                "origin": spec.origin,
                "usage": usage(spec),
                "synopsis": own.map(|h| synopsis(&spec.name, h)),
                "help": own,
                "extensions": ext,
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
    match own.or(cdo_help) {
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
    if own.is_some() {
        // the synopsis above has the usage
        let _ = writeln!(
            s,
            "    {inputs} input(s), access class: {}",
            class_name(spec.class)
        );
    } else {
        let _ = writeln!(
            s,
            "    usage: cdors -{} <{} input(s)>{}   (access class: {})",
            usage(spec),
            inputs,
            if spec.outputs == 1 { " <output>" } else { "" },
            class_name(spec.class)
        );
    }
    if !ext.is_empty() {
        let _ = writeln!(s, "    cdors extensions:");
        for e in &ext {
            let _ = writeln!(s, "    - {e}");
        }
    }
    if !notes.is_empty() {
        let _ = writeln!(s, "    notes (docs/deviations.md, [section: entry]):");
        for n in &notes {
            let _ = writeln!(s, "    - {n}");
        }
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every operator cdo does not have prints its own help text, short, with a synopsis.
    #[test]
    fn cdors_operators_have_help() {
        for o in implemented().iter().filter(|o| o.origin == "cdors") {
            let h = cdors_help(&o.name).unwrap_or_else(|| panic!("no help text for {}", o.name));
            assert!(h.lines().count() <= 25, "help of {} is too long", o.name);
            assert!(
                h.contains("EXAMPLE") && !synopsis(&o.name, h).is_empty(),
                "{}",
                o.name
            );
        }
        for name in CDORS_HELP.keys() {
            assert!(
                ops::lookup(name).is_some_and(|o| o.origin == "cdors"),
                "{name}"
            );
        }
    }
}
