//! Running statistics over `n` consecutive timesteps: `runmean`, `runavg`, `runmin`, `runmax`,
//! `runsum`, `runrange`, `runstd`, `runstd1`, `runvar`, `runvar1` (`src/operators/Runstat.cc`).
//!
//! **Windows**: output step `k` is the statistic of input steps `k .. k+n-1`; the output has
//! `len - n + 1` steps, and fewer than `n` input steps is an error. Timestamps and `time_bnds`
//! as a group of the window's `n` steps (`timegroup::run_axis`; default `middle`,
//! `--timestat_date` honoured). Like cdo, no `cell_methods` is written.
//!
//! **Values**: cdo folds each window separately, adding its steps in time order (the first step is
//! copied, the others added; `Runstat.cc:200-240`), and divides by the number of valid values
//! (`samp`); cdors folds each window with the same per-cell accumulators as the period statistics
//! (`timstat::Acc`), so the missing-value rules are those of `timstat.rs`: `mean`/`sum` over the
//! valid values (missing if none), `avg` missing as soon as one value is missing, `min`/`max`
//! of the valid values. Variance and standard deviation use Welford's update instead of cdo's
//! one-pass sum of squares (`docs/deviations.md`).
//!
//! **Kernel**: a lane is a block of cells; its state keeps the last `n` steps of its cells (a ring
//! buffer, `n × cells × 8` bytes; the planner sizes lanes and waves from it, `plan::schedule`) and emits
//! output step `k` when step `k+n-1` arrives.

use super::timstat::{Acc, Stat};
use crate::chain::OpNode;
use crate::error::{Error, Result};
use crate::io::Values;
use crate::model::DimRole;
use crate::model::timegroup::{Period, TimestatDate, run_axis};
use crate::plan::stage::{FoldKernel, FoldState, Tile};
use crate::plan::tiling::TileBox;
use crate::plan::{Desc, Fold, Sources};
use std::sync::Arc;

const STATS: [(&str, &str); 10] = [
    ("mean", "Running mean"),
    ("avg", "Running average (missing if any value is missing)"),
    ("min", "Running minimum"),
    ("max", "Running maximum"),
    ("sum", "Running sum"),
    ("range", "Running range (maximum - minimum)"),
    ("std", "Running standard deviation (n)"),
    ("std1", "Running standard deviation (n-1)"),
    ("var", "Running variance (n)"),
    ("var1", "Running variance (n-1)"),
];

/// The statistic of a running-statistic operator name.
pub fn parse(name: &str) -> Option<Stat> {
    name.strip_prefix("run").and_then(Stat::parse)
}

/// Whether `name` is a running statistic.
pub fn handles(name: &str) -> bool {
    parse(name).is_some()
}

/// Names and descriptions for the registry.
pub fn operators() -> Vec<(String, String)> {
    STATS
        .iter()
        .map(|(s, d)| (format!("run{s}"), format!("{d} over nts timesteps")))
        .collect()
}

/// Output description: `len - n + 1` steps and the pending fold.
pub fn describe(node: &OpNode, inputs: Vec<Desc>, srcs: &mut Sources) -> Result<Desc> {
    let op = node.name.as_str();
    let stat =
        parse(op).ok_or_else(|| Error::internal(format!("'{op}' is not a run statistic")))?;
    let n: usize = node
        .args
        .first()
        .and_then(|a| a.trim().parse().ok())
        .filter(|&n| n >= 1)
        .ok_or_else(|| {
            Error::bad_arguments(format!("'{op}' needs the number of timesteps (>= 1)"))
                .with("operator", op.to_owned())
        })?;
    let input = inputs
        .into_iter()
        .next()
        .ok_or_else(|| Error::bad_arguments(format!("'{op}' needs one input")))?;
    let t = input
        .time
        .as_ref()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            Error::bad_data(format!("'{op}' needs an input with timesteps"))
                .with("operator", op.to_owned())
        })?;
    let members = super::timstat::members(t);
    let stamp = TimestatDate::resolve(srcs.timestat_date, Period::DEFAULT_TIMESTAT);
    let closed = run_axis(n, &members, stamp, t.axis.calendar)
        .map_err(|e| e.with("operator", op.to_owned()))?;
    let nout = closed.len();
    let time = super::timstat::grouped_time(t, &closed);

    let mut out = input.clone();
    let mut vars = Vec::with_capacity(out.vars.len());
    for v in &mut out.vars {
        let td = v.dim_of(DimRole::Time);
        if let Some(td) = td {
            v.dims[td].size = nout;
        }
        vars.push(td);
    }
    out.time = Some(time);
    out.fold = Some(Fold {
        input: Box::new(input),
        kernel: Arc::new(Kernel { stat, n, vars }),
    });
    Ok(out)
}

struct Kernel {
    stat: Stat,
    n: usize,
    /// Time dimension of every variable (`None`: no time axis, passed through).
    vars: Vec<Option<usize>>,
}

impl FoldKernel for Kernel {
    fn fold_dim(&self) -> DimRole {
        DimRole::Time
    }

    fn state_bytes(&self, var: usize, lane: &TileBox) -> usize {
        let Some(td) = self.vars.get(var).copied().flatten() else {
            return lane.len() * 8;
        };
        // ring buffer of n steps, accumulators and one result
        let cells = lane.len() / lane.ranges[td].len().max(1);
        cells * (self.n * 8 + 32)
    }

    fn start(&self, var: usize, lane: &TileBox) -> Box<dyn FoldState> {
        let td = self.vars.get(var).copied().flatten();
        let (outer, inner) = match td {
            Some(td) => {
                let shape = lane.shape();
                (
                    shape[..td].iter().product(),
                    shape[td + 1..].iter().product(),
                )
            }
            None => (1, lane.len()),
        };
        let ncell = outer * inner;
        Box::new(LaneState {
            stat: self.stat,
            n: self.n,
            var,
            td,
            lane: lane.clone(),
            outer,
            inner,
            ring: Vec::new(),
            acc: Acc::new(self.stat, ncell),
        })
    }
}

struct LaneState {
    stat: Stat,
    n: usize,
    var: usize,
    td: Option<usize>,
    lane: TileBox,
    outer: usize,
    inner: usize,
    /// The last `n` steps: step `t` in slot `t % n` (`n × cells`, allocated on the first tile).
    ring: Vec<f64>,
    acc: Acc,
}

impl FoldState for LaneState {
    fn push(&mut self, tile: Tile) -> Result<Vec<Tile>> {
        let Some(td) = self.td else {
            return Ok(vec![tile]);
        };
        let t0 = tile.bx.ranges[td].start;
        let nt = tile.bx.ranges[td].len();
        let (outer, inner) = (self.outer, self.inner);
        let nc = outer * inner;
        if tile.values.len() != nt * nc {
            return Err(Error::internal(format!(
                "running statistic: tile of {} values, expected {nt} x {nc}",
                tile.values.len()
            )));
        }
        if self.ring.is_empty() {
            self.ring = vec![f64::NAN; self.n * nc];
        }
        let get = |i: usize| match &tile.values {
            Values::F32(v) => f64::from(v[i]),
            Values::F64(v) => v[i],
        };
        let n = self.n;
        let mut out = Vec::new();
        for k in 0..nt {
            let t = t0 + k;
            let row = &mut self.ring[(t % n) * nc..][..nc];
            for o in 0..outer {
                for i in 0..inner {
                    row[o * inner + i] = get((o * nt + k) * inner + i);
                }
            }
            if t + 1 < n {
                continue;
            }
            // window t-n+1 ..= t, folded in time order
            let w0 = t + 1 - n;
            self.acc.reset(self.stat);
            for s in w0..=t {
                self.acc.update(self.stat, &self.ring[(s % n) * nc..][..nc]);
            }
            let mut bx = self.lane.clone();
            bx.ranges[td] = w0..w0 + 1;
            out.push(Tile {
                var: self.var,
                bx,
                values: Values::F64(self.acc.result(self.stat)),
            });
        }
        Ok(out)
    }

    fn finish(&mut self) -> Result<Vec<Tile>> {
        Ok(Vec::new())
    }
}
