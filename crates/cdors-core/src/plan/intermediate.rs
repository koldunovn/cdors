//! Multi-stage chains: the result of a fold (statistic, remapping, ...) used as the input of
//! another operator becomes an **intermediate**.
//!
//! When an operator's input description still carries a pending fold, the inner stage ends
//! there ([`materialize`]): its output is written by the ordinary Zarr writer into an in-memory
//! store (`zarrs` `MemoryStore`, uncompressed, so its size is known exactly) and read back by the
//! ordinary Zarr reader as a [`ChunkSource`]. Only metadata and coordinates are written while
//! planning; the data is written when the stage runs. The outer chain is then described and
//! planned against that source like against a file: every output variable of the inner stage
//! becomes a leaf with identity maps, while grids, vertical axes, time axis and attributes are
//! taken over from the inner description unchanged (so nothing is lost in the round trip).
//!
//! Values are stored in double precision, as cdo passes double fields between chained operators
//! (the climatology arithmetic `ymon*`/`yday*`/`yseas*` keeps its climatology input in the
//! data's precision, as cdo does).
//!
//! Chunks are chosen for the access pattern of the consuming operator: all timesteps of a block
//! of cells for time folds (statistics, percentiles, running statistics), one complete field
//! per timestep otherwise (field statistics, remapping, pointwise operators).
//!
//! Stages run one after another, inner first. All intermediates stay in memory until the run
//! ends; together they may take at most half of the memory budget, otherwise planning fails with
//! `intermediate_too_large` (no spilling to disk in the prototype).

use super::stage::Stage;
use super::{Desc, Expr, IndexMap, Leaf, OutKind, Plan, Sources};
use crate::error::{Error, ErrorCode, Result};
use crate::exec::{OutVar, Writer};
use crate::io::ChunkSource;
use crate::model::{DType, DimRole};
use std::sync::Arc;
use zarrs::storage::store::MemoryStore;

/// An inner stage whose output is kept in memory.
pub struct Intermediate {
    /// Name of the in-memory store (`intermediate:<n>`), the source name in `--plan`.
    pub name: String,
    /// Index of its reader in the plan's sources.
    pub src: usize,
    /// The stage that computes it.
    pub stage: Stage,
    /// Description of its output (with the pending fold).
    pub desc: Desc,
    /// Layout of the written arrays.
    pub lay: Vec<OutVar>,
    pub writer: Arc<dyn Writer>,
    /// Size of the stored values.
    pub bytes: u64,
    /// Operator that consumes it (chunking).
    pub consumer: String,
}

/// How the consuming operator walks through its input.
fn time_complete(consumer: &str) -> bool {
    crate::ops::timstat::parse(consumer).is_some()
        || crate::ops::pctl::handles(consumer)
        || crate::ops::runstat::handles(consumer)
}

/// Elements per chunk of an intermediate (4 MiB of f32).
const CHUNK_ELEMS: usize = 1 << 20;

fn chunks_for(dims: &[crate::model::VarDim], time_major: bool) -> Vec<usize> {
    let mut c: Vec<usize> = dims.iter().map(|d| d.size.max(1)).collect();
    if time_major {
        // all timesteps, cells from the last dimension until the chunk is full
        let nt: usize = dims
            .iter()
            .filter(|d| d.role == DimRole::Time)
            .map(|d| d.size.max(1))
            .product();
        let mut room = (CHUNK_ELEMS / nt).max(1);
        for d in (0..dims.len()).rev() {
            if dims[d].role == DimRole::Time {
                continue;
            }
            c[d] = dims[d].size.clamp(1, room);
            room = (room / c[d]).max(1);
        }
    } else {
        for (cd, d) in c.iter_mut().zip(dims) {
            if d.role == DimRole::Time {
                *cd = 1;
            }
        }
    }
    c
}

/// Ends the stage of `d` (which carries a pending fold) and returns a description of its result
/// read back from memory, for `consumer`.
pub fn materialize(d: Desc, consumer: &str, srcs: &mut Sources) -> Result<Desc> {
    let fold = d
        .fold
        .clone()
        .ok_or_else(|| Error::internal("materialize without a fold"))?;
    let stage = Stage {
        kernel: Some(fold.kernel.clone()),
        ..Stage::map(&fold.input, &srcs.srcs)?
    };
    let time_major = time_complete(consumer);
    // cdo passes double fields from one chained operator to the next, so intermediates keep
    // f64; the climatology arithmetic stores its climatology input in the data's precision
    // (checked against cdo 2.6.0: `-sub -yearmean in -timmean in` and `-ymonsub in -ymonmean in`
    // are bit-identical only this way)
    let float_input = crate::ops::ymonarith::handles(consumer);
    let lay: Vec<OutVar> = d
        .vars
        .iter()
        .map(|v| OutVar {
            name: v.name.clone(),
            dims: v.dims.iter().map(|x| x.name.clone()).collect(),
            shape: v.shape(),
            chunks: chunks_for(&v.dims, time_major),
            dtype: if v.dtype == DType::F32 && float_input {
                DType::F32
            } else {
                DType::F64
            },
            missval: v.missval,
        })
        .collect();
    let bytes: u64 = lay
        .iter()
        .map(|o| o.shape.iter().product::<usize>() as u64 * o.dtype.size() as u64)
        .sum();
    let name = format!("intermediate:{}", srcs.intermediates.len() + 1);
    let store = Arc::new(MemoryStore::new());
    let tmp = Plan::new(
        srcs.srcs.clone(),
        Desc {
            fold: None,
            ..d.clone()
        },
        name.clone(),
        OutKind::Zarr3,
    );
    let writer = crate::io::write_zarr::ZarrWriter::create_in(
        store.clone(),
        &tmp,
        &lay,
        false,
        None,
        false,
    )?;
    let reader = crate::io::zarr::ZarrSource::open_store(&name, store, &Vec::new)?;
    let reader: Arc<dyn ChunkSource> = Arc::new(reader);
    srcs.paths.push(name.clone());
    srcs.srcs.push(reader);
    let si = srcs.srcs.len() - 1;
    let mut out = Desc {
        fold: None,
        ..d.clone()
    };
    for v in &mut out.vars {
        v.expr = Expr::Leaf(Leaf {
            src: si,
            var: v.name.clone(),
            maps: v
                .dims
                .iter()
                .enumerate()
                .map(|(i, x)| (i, IndexMap::identity(x.size)))
                .collect(),
        });
    }
    srcs.intermediates.push(Intermediate {
        name,
        src: si,
        stage,
        desc: d,
        lay,
        writer: Arc::new(writer),
        bytes,
        consumer: consumer.to_owned(),
    });
    Ok(out)
}

/// `intermediate_too_large` if the intermediates take more than half of `budget`.
pub fn check_budget(ims: &[Intermediate], budget: u64) -> Result<()> {
    let total: u64 = ims.iter().map(|i| i.bytes).sum();
    if total <= budget / 2 {
        return Ok(());
    }
    let largest = ims
        .iter()
        .max_by_key(|i| i.bytes)
        .expect("one intermediate");
    Err(Error::new(
        ErrorCode::IntermediateTooLarge,
        format!(
            "the intermediate results of this chain take {} in memory (largest: the input of \
             '{}', {}), more than half of the memory budget of {}",
            super::schedule::human(total),
            largest.consumer,
            super::schedule::human(largest.bytes),
            super::schedule::human(budget)
        ),
    )
    .with("bytes", total)
    .with("limit", budget / 2)
    .with_hint(format!(
        "raise --mem to at least {}, reduce the inner result first (selections inside the inner \
         operator), or run the chain in two commands with the inner result written to a file",
        super::schedule::human(2 * total)
    )))
}
