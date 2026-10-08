//! Check the embedded cdo operator catalog (`ops/catalog.rs`): counts, a few lookups and
//! did-you-mean hints, and `cdo_help(op)` against `$CDO -h op` for every operator.
//!
//!   source env.sh; cargo run --example catalog_check
//!
//! The data comes from the cdo 2.6.5 source while `$CDO` may be an older cdo, so help texts that
//! cdo changed in between differ; the program reports how many are byte-identical.

#[path = "../src/ops/catalog.rs"]
#[allow(dead_code)]
mod catalog;

use std::process::Command;

fn main() {
    let ops = catalog::cdo_operators();
    let with_help = ops.iter().filter(|o| o.help().is_some()).count();
    let variadic = ops.iter().filter(|o| o.n_inputs < 0).count();
    println!(
        "{} operators, {} with help, {} with any number of inputs, {} aliases",
        ops.len(),
        with_help,
        variadic,
        catalog::cdo_aliases().count()
    );
    for name in ["yearmean", "chvar", "ensmean", "splityear", "nosuchop"] {
        println!("{name}: {:?}", catalog::cdo_operator(name));
    }
    let implemented = ["yearmean", "ymonmean", "fldmean", "remapbil", "selname"];
    for name in [
        "yearmeans",
        "ymonmen",
        "fldmaen",
        "remapbi",
        "SELNAME",
        "timcumsm",
    ] {
        println!(
            "suggest({name}) = {:?}",
            catalog::suggest(name, &implemented)
        );
    }

    let Ok(cdo) = std::env::var("CDO") else {
        println!("$CDO not set: help texts not compared");
        return;
    };
    let mut same = 0;
    let mut differ = Vec::new();
    for op in ops {
        let out = Command::new(&cdo)
            .args(["-h", op.name])
            .output()
            .expect("run cdo");
        let expected = catalog::cdo_help(op.name).map(|h| format!("{h}\n"));
        if expected.as_deref().map(str::as_bytes) == Some(&out.stdout[..]) {
            same += 1;
        } else {
            differ.push(op.name);
        }
    }
    println!(
        "cdo -h: {same} identical, {} different: {}",
        differ.len(),
        differ.join(" ")
    );
}
