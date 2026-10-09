//! Percentiles over time groups: `timpctl`, `hourpctl`, `daypctl`, `monpctl`, `seaspctl`,
//! `yearpctl` with the percentile `p` as argument and cdo's `--percentile <method>`.
//!
//! **cdo** (`src/operators/Timpctl.cc`, `src/percentiles_hist.cc`) takes three inputs: the data
//! and, per output step, the minimum and maximum of each group (usually `-timmin in -timmax in`),
//! which bound a 101-bin histogram used once a cell holds more than 50 values. cdors computes
//! every percentile exactly (`kernels::percentile`), so it never needs the bounds: the min/max
//! inputs are accepted but never opened, described or read (`ops::used_inputs`), and the
//! one-input form `timpctl,p in` is accepted too. Groups of up to 50 values match cdo exactly;
//! larger groups differ from cdo's histogram result by up to one bin (`docs/deviations.md`).
//!
//! **Groups and output time axis** as the period statistics (`model/timegroup.rs`,
//! `Timpctl.cc:150-185`: same comparison of each step with the first step of the open group, same
//! season rule, `middle` timestamp and `time_bnds` by default). cdo writes no `cell_methods` and no
//! `frequency` attribute for percentiles, and neither does cdors.
//!
//! **Missing values**: missing values are skipped (`histAddVarLevelValues`); a cell without a
//! valid value in a group is missing (`calcPercentile`).
//!
//! **Kernel**: a lane is a block of cells; its state keeps the values of the open group for its
//! cells (time-major, in the input's type) and emits the group's percentiles when the group
//! closes. The planner sizes lanes, waves of lanes and passes over the input from
//! `cells × longest group` values (`state_bytes`, `plan::schedule`).

use crate::chain::OpNode;
use crate::error::{Error, Result};
use crate::io::Values;
use crate::kernels::percentile::{PercentileMethod, Sample, percentile};
use crate::model::DimRole;
use crate::model::timegroup::Period;
use crate::plan::stage::{FoldKernel, FoldState, Tile};
use crate::plan::tiling::TileBox;
use crate::plan::{Desc, Fold, Sources};
use std::sync::Arc;

const PREFIXES: [(&str, &str); 6] = [
    ("tim", "over all timesteps"),
    ("hour", "per hour"),
    ("day", "per day"),
    ("mon", "per month"),
    ("seas", "per season"),
    ("year", "per year"),
];

/// Whether `name` is a percentile operator.
pub fn handles(name: &str) -> bool {
    name.strip_suffix("pctl")
        .is_some_and(|p| PREFIXES.iter().any(|(q, _)| *q == p))
}

/// Names and descriptions for the registry.
pub fn operators() -> Vec<(String, String)> {
    PREFIXES
        .iter()
        .map(|(p, d)| {
            (
                format!("{p}pctl"),
                format!(
                    "Percentile p {d}, exact (cdo's three-input form `{p}pctl,p in min max` \
                     is accepted; the min/max inputs are not used)"
                ),
            )
        })
        .collect()
}

/// Output description: grouped time axis and the pending fold.
pub fn describe(node: &OpNode, inputs: Vec<Desc>, srcs: &mut Sources) -> Result<Desc> {
    let op = node.name.as_str();
    let period = Period::from_operator(op)
        .ok_or_else(|| Error::internal(format!("'{op}' is not a percentile operator")))?;
    let p: f64 = node
        .args
        .first()
        .and_then(|a| a.trim().parse().ok())
        .ok_or_else(|| Error::bad_arguments(format!("'{op}' needs the percentile p")))?;
    if !(0.0..=100.0).contains(&p) {
        return Err(Error::bad_arguments(format!(
            "percentile {p} out of range: p must be in [0, 100]"
        ))
        .with("operator", op.to_owned()));
    }
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
    let (groups, closed) = super::timstat::group_steps(
        super::timstat::Grouping::Period(period),
        &members,
        t.axis.calendar,
        srcs.timestat_date,
    );
    let longest = closed.iter().map(|c| c.count).max().unwrap_or(0) as u64;
    let time = super::timstat::grouped_time(t, &closed);
    let ngroups = closed.len();

    let mut out = input.clone();
    let mut vars = Vec::with_capacity(out.vars.len());
    for v in &mut out.vars {
        let td = v.dim_of(DimRole::Time);
        if let Some(td) = td {
            v.dims[td].size = ngroups;
        }
        vars.push(td);
    }
    out.time = Some(time);
    out.fold = Some(Fold {
        input: Box::new(input),
        kernel: Arc::new(Kernel {
            p,
            method: srcs.percentile,
            groups: Arc::new(groups),
            longest: longest as usize,
            vars,
        }),
    });
    Ok(out)
}

struct Kernel {
    p: f64,
    method: PercentileMethod,
    /// Output step of every input step (consecutive groups).
    groups: Arc<Vec<usize>>,
    /// Steps of the longest group.
    longest: usize,
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
        // the values of the open group (in the tiles' type; f64 assumed) and the sort scratch
        let cells = lane.len() / lane.ranges[td].len().max(1);
        cells * (self.longest * 8 + 8)
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
        Box::new(LaneState {
            p: self.p,
            method: self.method,
            groups: self.groups.clone(),
            var,
            td,
            lane: lane.clone(),
            outer,
            inner,
            buf: None,
            open: None,
        })
    }
}

/// Values of the open group, time-major (`steps × cells`), in the type of the tiles.
enum Buf {
    F32(Vec<f32>),
    F64(Vec<f64>),
}

struct LaneState {
    p: f64,
    method: PercentileMethod,
    groups: Arc<Vec<usize>>,
    var: usize,
    td: Option<usize>,
    lane: TileBox,
    /// Cells of the lane = outer (dims before time) x inner (dims after time).
    outer: usize,
    inner: usize,
    buf: Option<Buf>,
    /// Output step of the open group.
    open: Option<usize>,
}

/// Percentile of every cell of a time-major block of `nt` steps.
fn cell_percentiles<T: Sample>(
    vals: &[T],
    ncell: usize,
    p: f64,
    method: PercentileMethod,
) -> Vec<f64> {
    const BLOCK: usize = 32;
    let nt = vals.len().checked_div(ncell).unwrap_or(0);
    let mut out = Vec::with_capacity(ncell);
    let mut scratch = vec![T::default(); BLOCK * nt];
    for c0 in (0..ncell).step_by(BLOCK) {
        let nb = BLOCK.min(ncell - c0);
        for k in 0..nt {
            for (i, &v) in vals[k * ncell + c0..][..nb].iter().enumerate() {
                scratch[i * nt + k] = v;
            }
        }
        for i in 0..nb {
            out.push(percentile(&mut scratch[i * nt..(i + 1) * nt], p, method));
        }
    }
    out
}

/// Time-major copy of `n` consecutive steps of a tile whose time dimension is not the first.
fn time_major<T: Copy + Default>(v: &[T], outer: usize, nt: usize, inner: usize) -> Vec<T> {
    let mut tm = vec![T::default(); nt * outer * inner];
    for o in 0..outer {
        for k in 0..nt {
            let src = &v[(o * nt + k) * inner..][..inner];
            tm[k * outer * inner + o * inner..][..inner].copy_from_slice(src);
        }
    }
    tm
}

impl LaneState {
    fn ncell(&self) -> usize {
        self.outer * self.inner
    }

    /// Emits the open group and clears the buffer.
    fn emit(&mut self, td: usize) -> Option<Tile> {
        let g = self.open.take()?;
        let n = self.ncell();
        let values = match &mut self.buf {
            Some(Buf::F32(v)) => {
                let r = cell_percentiles(v, n, self.p, self.method);
                v.clear();
                r
            }
            Some(Buf::F64(v)) => {
                let r = cell_percentiles(v, n, self.p, self.method);
                v.clear();
                r
            }
            None => vec![f64::NAN; n],
        };
        let mut bx = self.lane.clone();
        bx.ranges[td] = g..g + 1;
        Some(Tile {
            var: self.var,
            bx,
            values: Values::F64(values),
        })
    }
}

impl FoldState for LaneState {
    fn push(&mut self, tile: Tile) -> Result<Vec<Tile>> {
        let Some(td) = self.td else {
            return Ok(vec![tile]);
        };
        let t0 = tile.bx.ranges[td].start;
        let nt = tile.bx.ranges[td].len();
        let n = self.ncell();
        if tile.values.len() != nt * n {
            return Err(Error::internal(format!(
                "percentile: tile of {} values, expected {nt} x {n}",
                tile.values.len()
            )));
        }
        let (outer, inner) = (self.outer, self.inner);
        let vals = match tile.values {
            Values::F32(v) if outer > 1 => Values::F32(time_major(&v, outer, nt, inner)),
            Values::F64(v) if outer > 1 => Values::F64(time_major(&v, outer, nt, inner)),
            v => v,
        };
        let mut out = Vec::new();
        for k in 0..nt {
            let g = self.groups[t0 + k];
            if self.open != Some(g) {
                out.extend(self.emit(td));
                self.open = Some(g);
            }
            match (&mut self.buf, &vals) {
                (Some(Buf::F32(b)), Values::F32(v)) => b.extend_from_slice(&v[k * n..][..n]),
                (Some(Buf::F64(b)), Values::F64(v)) => b.extend_from_slice(&v[k * n..][..n]),
                (None, Values::F32(v)) => self.buf = Some(Buf::F32(v[k * n..][..n].to_vec())),
                (None, Values::F64(v)) => self.buf = Some(Buf::F64(v[k * n..][..n].to_vec())),
                _ => {
                    return Err(Error::internal(
                        "percentile: tiles of one variable change type",
                    ));
                }
            }
        }
        Ok(out)
    }

    fn finish(&mut self) -> Result<Vec<Tile>> {
        Ok(match self.td {
            Some(td) => self.emit(td).into_iter().collect(),
            None => Vec::new(),
        })
    }
}
