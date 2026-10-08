//! Pointwise arithmetic: `add`, `sub`, `mul`, `div` (two inputs), `addc`, `subc`, `mulc`,
//! `divc` (a constant) and `ifthen` (mask, data).
//!
//! Broadcasting follows cdo (`src/operators/Arith.cc`, `Cond.cc`): variables are paired by
//! position, a second input with a single variable is used for every variable of the first, a
//! second input with one timestep is used for every timestep (and, the other way round, a first
//! input with one timestep is used for every timestep of the second), and an input with one
//! level is used for every level. Missing values follow cdo's `math_operators.h`: missing in
//! either operand gives missing, except that a product with zero is zero and a division by zero
//! is missing. Each result is rounded to the type of the operand that cdo keeps (the first one;
//! the data operand for `ifthen`), as cdo computes float32 fields in float32.

use crate::chain::OpNode;
use crate::error::{Error, ErrorCode, Result};
use crate::model::DimRole;
use crate::plan::{BinOp, Desc, Expr, IndexMap, UnOp, VarDesc};

fn shape_err(op: &str, msg: String) -> Error {
    Error::new(ErrorCode::BadData, msg)
        .with("operator", op.to_owned())
        .with_hint("both inputs need the same grid and levels; a second input may have one variable, one timestep or one level")
}

/// Re-expresses `b` (a variable of the other input) over the dimensions of `a`.
/// `bcast_time`: `b` has one timestep that is used for every timestep of `a`.
fn align(op: &str, a: &VarDesc, b: &VarDesc, bcast_time: bool) -> Result<Expr> {
    let mut map: Vec<(usize, Option<IndexMap>)> = Vec::with_capacity(b.dims.len());
    let ah = a.hdims();
    let bh = b.hdims();
    let mut hk = 0;
    for bd in &b.dims {
        let target = match bd.role {
            DimRole::Time => {
                let Some(t) = a.dim_of(DimRole::Time) else {
                    return Err(shape_err(
                        op,
                        format!(
                            "variable '{}' has a time axis but '{}' has none",
                            b.name, a.name
                        ),
                    ));
                };
                let n = a.dims[t].size;
                if bd.size == n && !bcast_time {
                    (t, None)
                } else if bd.size == 1 {
                    (t, Some(IndexMap::Const { idx: 0, len: n }))
                } else {
                    return Err(shape_err(
                        op,
                        format!("inputs have {} and {} timesteps", n, bd.size),
                    ));
                }
            }
            DimRole::Vertical => {
                let Some(z) = a.dim_of(DimRole::Vertical) else {
                    if bd.size == 1 {
                        return Err(shape_err(
                            op,
                            format!("variable '{}' has levels but '{}' has none", b.name, a.name),
                        ));
                    }
                    return Err(shape_err(
                        op,
                        format!(
                            "variable '{}' has {} levels but '{}' has none",
                            b.name, bd.size, a.name
                        ),
                    ));
                };
                let n = a.dims[z].size;
                if bd.size == n {
                    (z, None)
                } else if bd.size == 1 {
                    (z, Some(IndexMap::Const { idx: 0, len: n }))
                } else {
                    return Err(shape_err(
                        op,
                        format!("inputs have {} and {} levels", n, bd.size),
                    ));
                }
            }
            DimRole::Horizontal => {
                let Some(&ad) = ah.get(hk) else {
                    return Err(shape_err(op, "grids differ".into()));
                };
                hk += 1;
                if a.dims[ad].size != bd.size || ah.len() != bh.len() {
                    return Err(shape_err(
                        op,
                        format!(
                            "grids differ: '{}' has {} points, '{}' has {}",
                            a.name,
                            ah.iter().map(|&i| a.dims[i].size).product::<usize>(),
                            b.name,
                            bh.iter().map(|&i| b.dims[i].size).product::<usize>()
                        ),
                    ));
                }
                (ad, None)
            }
            DimRole::Other => {
                return Err(shape_err(
                    op,
                    format!("dimension '{}' is not supported", bd.name),
                ));
            }
        };
        map.push(target);
    }
    let mut e = b.expr.clone();
    e.retarget(&|d| map[d].clone());
    Ok(e)
}

fn two_inputs(node: &OpNode, mut inputs: Vec<Desc>) -> Result<Desc> {
    let op = node.name.as_str();
    let d2 = inputs.pop().expect("two inputs");
    let d1 = inputs.pop().expect("two inputs");
    let bop = match op {
        "add" => BinOp::Add,
        "sub" => BinOp::Sub,
        "mul" => BinOp::Mul,
        "div" => BinOp::Div,
        "ifthen" => BinOp::IfThen,
        _ => unreachable!("binary operator"),
    };
    // ifthen: the output is the data (second) input, the mask is broadcast onto it
    let (base, other, other_first) = if bop == BinOp::IfThen {
        (d2, d1, true)
    } else {
        let (n1, n2) = (d1.ntime(), d2.ntime());
        if n1 == 1 && n2 > 1 {
            // cdo fills up the first input: the output follows the second
            let mut out = d2.clone();
            let n = d1.vars.len().max(d2.vars.len());
            if d1.vars.len() != d2.vars.len() && d1.vars.len() != 1 && d2.vars.len() != 1 {
                return Err(shape_err(
                    op,
                    format!(
                        "inputs have {} and {} variables",
                        d1.vars.len(),
                        d2.vars.len()
                    ),
                ));
            }
            out.vars = (0..n)
                .map(|i| {
                    let a = &d1.vars[i.min(d1.vars.len() - 1)];
                    let b = &d2.vars[i.min(d2.vars.len() - 1)];
                    let ea = align(op, b, a, true)?;
                    let mut v = if d1.vars.len() >= d2.vars.len() {
                        a.clone()
                    } else {
                        b.clone()
                    };
                    v.dims = b.dims.clone();
                    v.grid = b.grid;
                    v.zaxis = b.zaxis;
                    v.expr = Expr::Binary(bop, a.dtype, Box::new(ea), Box::new(b.expr.clone()));
                    v.dtype = a.dtype;
                    Ok(v)
                })
                .collect::<Result<_>>()?;
            return Ok(out);
        }
        (d1, d2, false)
    };
    let mut out = base.clone();
    let nb = base.vars.len();
    let no = other.vars.len();
    if no != nb && no != 1 {
        return Err(shape_err(
            op,
            format!(
                "inputs have {} and {} variables",
                if other_first { no } else { nb },
                if other_first { nb } else { no }
            ),
        ));
    }
    let bcast_time = base.ntime() > 1 && other.ntime() == 1;
    for (i, v) in out.vars.iter_mut().enumerate() {
        let o = &other.vars[if no == 1 { 0 } else { i }];
        let eo = align(op, v, o, bcast_time)?;
        let ev = v.expr.clone();
        v.expr = if other_first {
            Expr::Binary(bop, v.dtype, Box::new(eo), Box::new(ev))
        } else {
            Expr::Binary(bop, v.dtype, Box::new(ev), Box::new(eo))
        };
    }
    Ok(out)
}

/// Output description of an arithmetic operator.
pub fn describe(node: &OpNode, mut inputs: Vec<Desc>) -> Result<Desc> {
    let op = node.name.as_str();
    let c = || -> Result<f64> {
        node.args[0]
            .trim()
            .parse::<f64>()
            .map_err(|_| Error::bad_arguments(format!("'{}' is not a number", node.args[0])))
    };
    let uop = match op {
        "addc" => UnOp::AddC(c()?),
        "subc" => UnOp::SubC(c()?),
        "mulc" => UnOp::MulC(c()?),
        "divc" => UnOp::DivC(c()?),
        _ => return two_inputs(node, inputs),
    };
    let mut d = inputs.remove(0);
    for v in &mut d.vars {
        let e = std::mem::replace(
            &mut v.expr,
            Expr::Leaf(crate::plan::Leaf {
                src: 0,
                var: String::new(),
                maps: vec![],
            }),
        );
        v.expr = Expr::Unary(uop, v.dtype, Box::new(e));
    }
    Ok(d)
}
