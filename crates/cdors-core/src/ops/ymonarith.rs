//! Arithmetic with a climatology: `ymonadd/sub/mul/div`, `ydayadd/sub/mul/div`,
//! `yseasadd/sub/mul/div` (`src/operators/Ytimarith.cc`).
//!
//! The second input holds one timestep per month, day of year or season (typically the output
//! of `ymonmean`, `ydaymean`, `yseasmean`). Each timestep of the first input is combined with the
//! step of the second input in the same bucket: month 1..12, day index `(month-1)*31 + day`
//! (`decode_day_of_year`, so 29 February is its own bucket), or season (`month_to_season`, with
//! `CDO_SEASON_START`). As in cdo, a bucket that occurs twice in the second input or is missing
//! from it is an error. The output keeps the time axis of the first input.
//!
//! This is pointwise arithmetic: the second input's time map becomes an index list, so the
//! operator fuses into the surrounding stage like `sub`, with cdo's missing-value rules and
//! float32 rounding (`arith.rs`, `plan/stage.rs:binop`).

use crate::chain::OpNode;
use crate::error::{Error, ErrorCode, Result};
use crate::model::CalDateTime;
use crate::model::timegroup::SeasonStart;
use crate::plan::{BinOp, Desc, Expr, IndexMap};

const KINDS: [(&str, &str); 3] = [
    ("ymon", "multi-year monthly"),
    ("yday", "multi-year daily"),
    ("yseas", "multi-year seasonal"),
];
const OPS: [(&str, BinOp, &str); 4] = [
    ("add", BinOp::Add, "Add"),
    ("sub", BinOp::Sub, "Subtract"),
    ("mul", BinOp::Mul, "Multiply by"),
    ("div", BinOp::Div, "Divide by"),
];

fn parse(name: &str) -> Option<(&'static str, BinOp)> {
    KINDS.iter().find_map(|(k, _)| {
        let rest = name.strip_prefix(k)?;
        OPS.iter()
            .find(|(o, _, _)| *o == rest)
            .map(|(_, b, _)| (*k, *b))
    })
}

/// Whether `name` is a climatology arithmetic operator.
pub fn handles(name: &str) -> bool {
    parse(name).is_some()
}

/// Names and descriptions for the registry.
pub fn operators() -> Vec<(String, String)> {
    let mut v = Vec::new();
    for (k, kd) in KINDS {
        for (o, _, od) in OPS {
            v.push((
                format!("{k}{o}"),
                format!("{od} a {kd} climatology (second input), matched per timestep"),
            ));
        }
    }
    v
}

/// Bucket of a timestamp (`Ytimarith.cc:get_month_index`, `get_doy_index`, `get_season_index`).
fn bucket(kind: &str, t: &CalDateTime, season_start: SeasonStart) -> usize {
    match kind {
        "ymon" => t.month as usize,
        "yday" => ((t.month.max(1) - 1) * 31 + t.day) as usize,
        _ => season_start.season(t.month),
    }
}

fn bucket_name(kind: &str, b: usize, season_start: SeasonStart) -> String {
    match kind {
        "ymon" => format!("month {b}"),
        "yday" => format!(
            "day of year {b} (month {}, day {})",
            (b - 1) / 31 + 1,
            (b - 1) % 31 + 1
        ),
        _ => format!("season {}", season_start.names()[b.min(3)]),
    }
}

fn out_names_match(a: &Desc, b: &Desc) -> bool {
    fn names(d: &Desc) -> Vec<&str> {
        let mut n: Vec<&str> = d.vars.iter().map(|v| v.name.as_str()).collect();
        n.sort_unstable();
        n
    }
    let na = names(a);
    na.windows(2).all(|w| w[0] != w[1]) && na == names(b)
}

/// Output description: the first input with the climatology subtracted (added, ...).
pub fn describe(node: &OpNode, mut inputs: Vec<Desc>) -> Result<Desc> {
    let op = node.name.as_str();
    let (kind, bop) = parse(op)
        .ok_or_else(|| Error::internal(format!("'{op}' is not a climatology operator")))?;
    if inputs.len() != 2 {
        return Err(Error::bad_arguments(format!("'{op}' needs two inputs")));
    }
    let clim = inputs.pop().expect("two inputs");
    let data = inputs.pop().expect("two inputs");
    let err = |msg: String| {
        Error::new(ErrorCode::BadData, msg)
            .with("operator", op.to_owned())
            .with_hint(format!(
                "the second input must hold one timestep per {}, e.g. from -{}mean",
                match kind {
                    "ymon" => "month",
                    "yday" => "day of year",
                    _ => "season",
                },
                kind
            ))
    };
    let (Some(t1), Some(t2)) = (data.time.as_ref(), clim.time.as_ref()) else {
        return Err(err("both inputs need a time axis".into()));
    };
    let season_start = SeasonStart::from_env();
    let mut slot = std::collections::HashMap::new();
    for (i, s) in t2.axis.steps.iter().enumerate() {
        let b = bucket(kind, &s.datetime, season_start);
        if slot.insert(b, i).is_some() {
            return Err(err(format!(
                "{} occurs more than once in the second input",
                bucket_name(kind, b, season_start)
            )));
        }
    }
    let map = t1
        .axis
        .steps
        .iter()
        .map(|s| {
            let b = bucket(kind, &s.datetime, season_start);
            slot.get(&b).copied().ok_or_else(|| {
                err(format!(
                    "{} (timestep {}) not found in the second input",
                    bucket_name(kind, b, season_start),
                    s.datetime.date_string().trim()
                ))
            })
        })
        .collect::<Result<Vec<usize>>>()?;
    let map = IndexMap::from_list(map);
    if data.vars.len() != clim.vars.len() {
        return Err(err(format!(
            "inputs have {} and {} variables",
            data.vars.len(),
            clim.vars.len()
        )));
    }
    // pair by name when both inputs have the same names (Zarr stores have no variable order),
    // else by position as cdo does
    let by_name = out_names_match(&data, &clim);
    let mut out = data;
    for (i, v) in out.vars.iter_mut().enumerate() {
        let c = if by_name {
            clim.vars
                .iter()
                .find(|c| c.name == v.name)
                .expect("same names")
        } else {
            &clim.vars[i]
        };
        let ec = super::arith::align_time(op, v, c, false, Some(&map))?;
        let ev = v.expr.clone();
        v.expr = Expr::Binary(bop, v.dtype, Box::new(ev), Box::new(ec));
    }
    Ok(out)
}
