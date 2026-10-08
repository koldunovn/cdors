//! Stages and the kernel interface.
//!
//! A stage computes the tiles of its output variables from stored chunks: every tile is
//! gathered from the chunks of the expression's leaves and the pointwise operators are applied
//! (`eval`). A stage without a kernel writes these tiles; a stage with a [`FoldKernel`] feeds
//! them to the kernel in a fixed order:
//!
//! - tiles are grouped into **lanes**: tiles that differ only along the folded dimension
//!   ([`FoldKernel::fold_dim`]) belong to the same lane;
//! - within a lane, tiles arrive strictly in ascending order along the folded dimension
//!   (time-ascending for time statistics, cell-index-ascending for field statistics), whatever
//!   order the chunks were read and decoded in; lanes run in parallel;
//! - the lane's [`FoldState`] carries running state across tile (= chunk) boundaries and returns
//!   output tiles when a group closes (`push`) or at the end (`finish`).
//!
//! Partial results of different tiles are never merged, so the result is bit-identical for any
//! chunk layout, tiling and thread count (plan, Technical Details > Determinism).

use super::tiling::{LeafInfo, TileBox, VarTiling};
use super::{BinOp, Desc, Expr, UnOp};
use crate::error::Result;
use crate::io::{ChunkSource, Values};
use crate::model::{DType, DimRole};
use serde_json::{Value, json};
use std::sync::Arc;

/// Values of one tile of one output variable.
#[derive(Debug, Clone)]
pub struct Tile {
    /// Index of the variable in the stage's output description.
    pub var: usize,
    pub bx: TileBox,
    pub values: Values,
}

/// A reduction or whole-extent operator run on the tiles of a stage (Tasks 6-8).
pub trait FoldKernel: Send + Sync {
    /// Dimension folded in order: [`DimRole::Time`] (time statistics: cells in parallel) or
    /// [`DimRole::Horizontal`] (field statistics: timesteps in parallel).
    fn fold_dim(&self) -> DimRole;
    /// Starts the running state of one lane of variable `var`; `lane` is the lane's box with the
    /// folded dimension spanning the whole extent.
    fn start(&self, var: usize, lane: &TileBox) -> Box<dyn FoldState>;
}

/// Running state of one lane.
pub trait FoldState: Send {
    /// The next tile of the lane in fold order; returns output tiles of groups that closed.
    fn push(&mut self, tile: Tile) -> Result<Vec<Tile>>;
    /// No more tiles: returns the remaining output tiles.
    fn finish(&mut self) -> Result<Vec<Tile>>;
}

/// One output variable of a stage.
#[derive(Clone)]
pub struct StageVar {
    /// Index into the stage's input description (`Desc::vars`).
    pub var: usize,
    /// Dimensions of the variable (roles tell the executor which dimension a kernel folds).
    pub dims: Vec<crate::model::VarDim>,
    pub expr: Expr,
    pub dtype: DType,
    pub leaves: Vec<LeafInfo>,
    pub tiling: VarTiling,
}

/// A stage: fused selections and pointwise operators, optionally folded by a kernel.
#[derive(Clone)]
pub struct Stage {
    pub vars: Vec<StageVar>,
    pub kernel: Option<Arc<dyn FoldKernel>>,
}

impl Stage {
    /// A stage that computes the variables of `desc` (no kernel).
    pub fn map(desc: &Desc, sources: &[Arc<dyn ChunkSource>]) -> Result<Self> {
        let mut vars = Vec::with_capacity(desc.vars.len());
        for (i, v) in desc.vars.iter().enumerate() {
            let leaves = LeafInfo::of(&v.expr, sources)?;
            let tiling = VarTiling::new(v, &leaves);
            vars.push(StageVar {
                var: i,
                dims: v.dims.clone(),
                expr: v.expr.clone(),
                dtype: v.dtype,
                leaves,
                tiling,
            });
        }
        Ok(Self { vars, kernel: None })
    }

    pub fn num_tiles(&self) -> usize {
        self.vars.iter().map(|v| v.tiling.num_tiles()).sum()
    }

    pub fn to_json(&self, sources: &[Arc<dyn ChunkSource>], desc: &Desc) -> Value {
        let vars: Vec<Value> = self
            .vars
            .iter()
            .map(|sv| {
                let vd = &desc.vars[sv.var];
                let ntiles = sv.tiling.num_tiles();
                // chunks read per leaf (summed over tiles) and decoded bytes
                let leaves: Vec<Value> = sv
                    .leaves
                    .iter()
                    .map(|li| {
                        let mut chunks = 0usize;
                        let mut distinct = std::collections::HashSet::new();
                        for t in 0..ntiles {
                            let lr = super::tiling::LeafRead::new(li, &sv.tiling.tile(t));
                            chunks += lr.num_chunks();
                            if distinct.len() < 1_000_000 {
                                distinct.extend(lr.chunks());
                            }
                        }
                        let csize: usize = li.grid.chunk_shape.iter().product();
                        let esize = match super::leaf_dtype(li.src.as_ref(), &li.leaf.var) {
                            DType::F32 => 4,
                            _ => 8,
                        };
                        json!({
                            "source": li.src.dataset().source,
                            "variable": li.leaf.var,
                            "chunk_shape": li.grid.chunk_shape,
                            "chunks_total": li.grid.num_chunks(),
                            "chunks_read": chunks,
                            "chunks_distinct": distinct.len(),
                            "bytes_decoded": chunks * csize * esize,
                        })
                    })
                    .collect();
                json!({
                    "name": vd.name,
                    "dims": vd.dims.iter().map(|d| d.name.clone()).collect::<Vec<_>>(),
                    "shape": vd.shape(),
                    "expr": sv.expr.describe(sources),
                    "tiles": ntiles,
                    "leaves": leaves,
                })
            })
            .collect();
        json!({
            "kind": if self.kernel.is_some() { "fold" } else { "map" },
            "variables": vars,
        })
    }
}

#[inline]
fn round(v: f64, t: DType) -> f64 {
    if t == DType::F32 { v as f32 as f64 } else { v }
}

fn unop(op: UnOp, t: DType, x: f64) -> f64 {
    // cdo's rules for missing values (math_operators.h): missing stays missing, except that
    // multiplying by zero gives zero and dividing by zero gives missing.
    let r = match op {
        UnOp::AddC(c) => x + c,
        UnOp::SubC(c) => x - c,
        UnOp::MulC(c) => {
            if c == 0.0 || x == 0.0 {
                0.0
            } else {
                x * c
            }
        }
        UnOp::DivC(c) => {
            if c == 0.0 {
                f64::NAN
            } else {
                x / c
            }
        }
    };
    round(r, t)
}

fn binop(op: BinOp, t: DType, x: f64, y: f64) -> f64 {
    let r = match op {
        BinOp::Add => x + y,
        BinOp::Sub => x - y,
        BinOp::Mul => {
            if x == 0.0 || y == 0.0 {
                0.0
            } else {
                x * y
            }
        }
        BinOp::Div => {
            if y == 0.0 {
                f64::NAN
            } else {
                x / y
            }
        }
        BinOp::IfThen => {
            if x.is_nan() || x == 0.0 {
                f64::NAN
            } else {
                y
            }
        }
    };
    round(r, t)
}

fn to_f64(v: Values) -> Vec<f64> {
    match v {
        Values::F64(x) => x,
        Values::F32(x) => x.into_iter().map(f64::from).collect(),
    }
}

/// Evaluates an expression on a tile, given the gathered values of its leaves (depth-first
/// order, as [`Expr::leaves`]). A bare leaf is returned unchanged; otherwise the result is f64
/// (already rounded to the expression's type).
pub fn eval(expr: &Expr, leaves: &mut std::vec::IntoIter<Values>) -> Values {
    match expr {
        Expr::Leaf(_) => leaves.next().expect("one value set per leaf"),
        Expr::Unary(op, t, e) => {
            let mut v = to_f64(eval(e, leaves));
            for x in &mut v {
                *x = unop(*op, *t, *x);
            }
            Values::F64(v)
        }
        Expr::Binary(op, t, a, b) => {
            let mut va = to_f64(eval(a, leaves));
            let vb = to_f64(eval(b, leaves));
            for (x, &y) in va.iter_mut().zip(&vb) {
                *x = binop(*op, *t, *x, y);
            }
            Values::F64(va)
        }
    }
}
