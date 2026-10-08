//! Time statistics by period (`tim*`, `hour*`, `day*`, `mon*`, `seas*`, `year*`) and over
//! multi-year groups (`ymon*`, `yday*`, `yseas*`), for `mean avg min max sum range std std1 var
//! var1`.
//!
//! **Grouping and output time axis** come from `model/timegroup.rs` (cdo 2.6.0's rules): at
//! describe time every input step gets the index of the output step it contributes to, and the
//! output description gets the group timestamps and `time_bnds` (`--timestat_date`,
//! `CDO_TIMESTAT_DATE`, `CDO_SEASON_START`).
//!
//! **Kernel.** A [`FoldKernel`] over the time dimension. A lane is a block of cells (all
//! non-time indices of one tile column); its state holds per-cell accumulators for the open group
//! (period statistics: one group at a time, emitted as soon as the next group starts) or for every
//! group (multi-year statistics: emitted at the end). Steps are folded one at a time in time
//! order, so the result does not depend on chunking or threads. Accumulation is in f64; output
//! values are rounded to the written type by the writer, as cdo does when it writes its double
//! accumulators to a float variable.
//!
//! **Semantics** (cdo `cdo_stepstat.h`, `field2.cc`, `fieldc.cc`), NaN = missing:
//! - `mean`: sum of the valid values / their number; missing where no value is valid;
//! - `avg`: sum / number, but missing as soon as one value of the group is missing (cdo adds
//!   with `addm`, which propagates missing values);
//! - `sum`: sum of the valid values, missing where none is valid; `min`, `max` of the valid
//!   values; `range` = max - min;
//! - `var`, `std` divide by n, `var1`, `std1` by n - 1 (n = valid values); missing where
//!   n - divisor is 0. cdo computes the variance with the one-pass formula
//!   `(Σx² - (Σx)²/n) / (n - d)`, which cancels catastrophically; cdors uses Welford's update
//!   (sequential, so no merge formula is needed) — recorded in `docs/deviations.md`.

use crate::chain::OpNode;
use crate::error::{Error, Result};
use crate::io::Values;
use crate::model::timegroup::{
    ClimTracker, Climatology, ClosedGroup, GroupTracker, Member, Period, SeasonStart, TimestatDate,
};
use crate::model::{AttrValue, Attrs, DimRole, TimeStep, TimeUnit, TimeUnits};
use crate::plan::stage::{FoldKernel, FoldState, Tile};
use crate::plan::tiling::TileBox;
use crate::plan::{Desc, Fold, Sources, TimeDesc};
use std::sync::Arc;

/// The statistic computed per group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stat {
    Mean,
    Avg,
    Min,
    Max,
    Sum,
    Range,
    Std,
    Std1,
    Var,
    Var1,
}

impl Stat {
    pub(crate) fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "mean" => Self::Mean,
            "avg" => Self::Avg,
            "min" => Self::Min,
            "max" => Self::Max,
            "sum" => Self::Sum,
            "range" => Self::Range,
            "std" => Self::Std,
            "std1" => Self::Std1,
            "var" => Self::Var,
            "var1" => Self::Var1,
            _ => return None,
        })
    }

    /// CF `cell_methods` method name.
    fn cell_method(self) -> &'static str {
        match self {
            Self::Mean | Self::Avg => "mean",
            Self::Min => "minimum",
            Self::Max => "maximum",
            Self::Sum => "sum",
            Self::Range => "range",
            Self::Std | Self::Std1 => "standard_deviation",
            Self::Var | Self::Var1 => "variance",
        }
    }

    fn is_var(self) -> bool {
        matches!(self, Self::Std | Self::Std1 | Self::Var | Self::Var1)
    }

    /// Normalisation n - divisor (cdo's `divisor`).
    fn divisor(self) -> u32 {
        u32::from(matches!(self, Self::Std1 | Self::Var1))
    }
}

/// How input steps are grouped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grouping {
    Period(Period),
    Clim(Climatology),
}

/// Grouping and statistic of an operator name (`yearmean`, `ydaystd1`, ...).
pub fn parse(name: &str) -> Option<(Grouping, Stat)> {
    const PREFIXES: [(&str, Grouping); 9] = [
        ("ymon", Grouping::Clim(Climatology::Month)),
        ("yday", Grouping::Clim(Climatology::Day)),
        ("yseas", Grouping::Clim(Climatology::Season)),
        ("tim", Grouping::Period(Period::All)),
        ("hour", Grouping::Period(Period::Hour)),
        ("day", Grouping::Period(Period::Day)),
        ("mon", Grouping::Period(Period::Month)),
        ("seas", Grouping::Period(Period::Season)),
        ("year", Grouping::Period(Period::Year)),
    ];
    PREFIXES
        .iter()
        .find_map(|(p, g)| name.strip_prefix(p).and_then(Stat::parse).map(|s| (*g, s)))
}

pub(crate) fn set_attr(a: &mut Attrs, k: &str, v: AttrValue) {
    match a.0.iter_mut().find(|(n, _)| n == k) {
        Some(e) => e.1 = v,
        None => a.0.push((k.to_owned(), v)),
    }
}

/// Output index of every input step and the closed groups in output order.
pub(crate) fn group_steps(
    grouping: Grouping,
    members: &[Member],
    cal: crate::model::Calendar,
    cli: Option<TimestatDate>,
) -> (Vec<usize>, Vec<ClosedGroup>) {
    let season_start = SeasonStart::from_env();
    match grouping {
        Grouping::Period(p) => {
            let stat = TimestatDate::resolve(cli, Period::DEFAULT_TIMESTAT);
            let mut tr = GroupTracker::new(p, cal, stat).with_season_start(season_start);
            let mut groups = Vec::with_capacity(members.len());
            let mut closed = Vec::new();
            for m in members {
                if let Some(c) = tr.push_member(*m) {
                    closed.push(c);
                }
                groups.push(closed.len());
            }
            closed.extend(tr.finish());
            (groups, closed)
        }
        Grouping::Clim(c) => {
            let stat = TimestatDate::resolve(cli, Climatology::DEFAULT_TIMESTAT);
            let mut tr = ClimTracker::new(c, cal, stat).with_season_start(season_start);
            let idx: Vec<usize> = members.iter().map(|m| tr.push_member(*m)).collect();
            let fin = tr.finish();
            let mut pos = vec![usize::MAX; c.slots()];
            for (k, (i, _)) in fin.iter().enumerate() {
                pos[*i] = k;
            }
            let groups = idx.iter().map(|&i| pos[i]).collect();
            (groups, fin.into_iter().map(|(_, g)| g).collect())
        }
    }
}

/// The input steps as the grouping sees them.
pub(crate) fn members(t: &TimeDesc) -> Vec<Member> {
    (0..t.len())
        .map(|i| Member::new(&t.axis.steps[i], t.axis.bounds.as_ref().map(|b| &b[i])))
        .collect()
}

/// The output time axis of closed groups (timestamps and `time_bnds`), in the input's units where
/// they can be encoded exactly, otherwise in days since the same reference
/// (`docs/deviations.md`).
pub(crate) fn grouped_time(t: &TimeDesc, closed: &[ClosedGroup]) -> TimeDesc {
    let cal = t.axis.calendar;
    let mut time = t.clone();
    let reference = time.axis.reference;
    if closed
        .iter()
        .any(|c| time.axis.units.encode(&c.timestamp, cal).is_none())
    {
        time.axis.units = TimeUnits::Relative {
            unit: TimeUnit::Day,
            reference,
        };
        time.axis.units_attr = format!(
            "days since {} {}",
            reference.date_string().trim_start(),
            reference.time_string()
        );
        set_attr(
            &mut time.attrs,
            "units",
            AttrValue::Text(time.axis.units_attr.clone()),
        );
    }
    let units = time.axis.units.clone();
    let enc = |d: &crate::model::CalDateTime| units.encode(d, cal).unwrap_or(f64::NAN);
    let step = |d: crate::model::CalDateTime| TimeStep {
        datetime: d,
        seconds: d.seconds_since(&reference, cal),
    };
    time.raw = closed.iter().map(|c| enc(&c.timestamp)).collect();
    time.axis.steps = closed.iter().map(|c| step(c.timestamp)).collect();
    let bounds: Option<Vec<[crate::model::CalDateTime; 2]>> =
        closed.iter().map(|c| c.bounds).collect();
    time.raw_bounds = bounds
        .as_ref()
        .map(|b| b.iter().map(|p| [enc(&p[0]), enc(&p[1])]).collect());
    time.axis.bounds = bounds.map(|b| b.iter().map(|p| [step(p[0]), step(p[1])]).collect());
    time
}

/// The global `frequency` attribute of period statistics.
fn set_frequency(out: &mut Desc, p: Period) {
    let freq = match p {
        Period::Day => Some("day"),
        Period::Month => Some("mon"),
        Period::Year => Some("year"),
        _ => None,
    };
    if let Some(f) = freq {
        set_attr(&mut out.attrs, "frequency", AttrValue::Text(f.into()));
    }
}

/// Output description: grouped time axis, `cell_methods`, and the pending fold.
pub fn describe(node: &OpNode, inputs: Vec<Desc>, srcs: &mut Sources) -> Result<Desc> {
    let (grouping, stat) = parse(&node.name)
        .ok_or_else(|| Error::internal(format!("'{}' is not a time statistic", node.name)))?;
    let input = inputs
        .into_iter()
        .next()
        .ok_or_else(|| Error::bad_arguments(format!("'{}' needs one input", node.name)))?;
    let t = input
        .time
        .as_ref()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            Error::bad_data(format!("'{}' needs an input with timesteps", node.name))
                .with("operator", node.name.clone())
        })?;
    let members = members(t);
    let (groups, closed) = group_steps(grouping, &members, t.axis.calendar, srcs.timestat_date);
    let ngroups = closed.len();
    let time = grouped_time(t, &closed);

    let mut out = input.clone();
    let method = format!("{}: {}", time.axis.var, stat.cell_method());
    let mut vars = Vec::with_capacity(out.vars.len());
    for v in &mut out.vars {
        let td = v.dim_of(DimRole::Time);
        if let Some(td) = td {
            v.dims[td].size = ngroups;
            set_attr(
                &mut v.attrs,
                "cell_methods",
                AttrValue::Text(method.clone()),
            );
        }
        vars.push(td);
    }
    out.time = Some(time);
    if let Grouping::Period(p) = grouping {
        set_frequency(&mut out, p);
    }
    let kernel = Kernel {
        stat,
        period: matches!(grouping, Grouping::Period(_)),
        groups: Arc::new(groups),
        ngroups,
        vars,
    };
    out.fold = Some(Fold {
        input: Box::new(input),
        kernel: Arc::new(kernel),
    });
    Ok(out)
}

struct Kernel {
    stat: Stat,
    /// Period groups (one open group per lane) or multi-year groups (all open).
    period: bool,
    /// Output step of every input step.
    groups: Arc<Vec<usize>>,
    ngroups: usize,
    /// Time dimension of every variable (`None`: no time axis, passed through).
    vars: Vec<Option<usize>>,
}

impl FoldKernel for Kernel {
    fn fold_dim(&self) -> DimRole {
        DimRole::Time
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
            stat: self.stat,
            period: self.period,
            groups: self.groups.clone(),
            var,
            td,
            lane: lane.clone(),
            outer,
            inner,
            ncell: outer * inner,
            slots: (0..if self.period { 1 } else { self.ngroups })
                .map(|_| None)
                .collect(),
            open: None,
        })
    }
}

/// Per-cell accumulators of one group: `a` = sum, minimum, maximum or Welford mean; `b` =
/// minimum (range) or Welford M2; `n` = number of valid values.
pub(crate) struct Acc {
    a: Vec<f64>,
    b: Vec<f64>,
    n: Vec<u32>,
}

impl Acc {
    pub(crate) fn new(stat: Stat, ncell: usize) -> Self {
        let mut acc = Self {
            a: vec![0.0; ncell],
            b: if stat.is_var() || stat == Stat::Range {
                vec![0.0; ncell]
            } else {
                Vec::new()
            },
            n: if matches!(stat, Stat::Min | Stat::Max | Stat::Range) {
                Vec::new()
            } else {
                vec![0; ncell]
            },
        };
        acc.reset(stat);
        acc
    }

    pub(crate) fn reset(&mut self, stat: Stat) {
        let a0 = if matches!(stat, Stat::Min | Stat::Max | Stat::Range) {
            f64::NAN
        } else {
            0.0
        };
        self.a.fill(a0);
        self.b
            .fill(if stat == Stat::Range { f64::NAN } else { 0.0 });
        self.n.fill(0);
    }

    /// Folds one timestep (`row`: one value per lane cell).
    pub(crate) fn update<T: Copy + Into<f64>>(&mut self, stat: Stat, row: &[T]) {
        let x = row.iter().map(|&v| -> f64 { v.into() });
        match stat {
            Stat::Mean | Stat::Sum => {
                for ((a, n), v) in self.a.iter_mut().zip(&mut self.n).zip(x) {
                    if !v.is_nan() {
                        *a += v;
                        *n += 1;
                    }
                }
            }
            Stat::Avg => {
                for ((a, n), v) in self.a.iter_mut().zip(&mut self.n).zip(x) {
                    *a += v;
                    *n += u32::from(!v.is_nan());
                }
            }
            Stat::Min => {
                for (a, v) in self.a.iter_mut().zip(x) {
                    if v < *a || a.is_nan() {
                        *a = v;
                    }
                }
            }
            Stat::Max => {
                for (a, v) in self.a.iter_mut().zip(x) {
                    if v > *a || a.is_nan() {
                        *a = v;
                    }
                }
            }
            Stat::Range => {
                for ((a, b), v) in self.a.iter_mut().zip(&mut self.b).zip(x) {
                    if v > *a || a.is_nan() {
                        *a = v;
                    }
                    if v < *b || b.is_nan() {
                        *b = v;
                    }
                }
            }
            Stat::Std | Stat::Std1 | Stat::Var | Stat::Var1 => {
                for (((m, m2), n), v) in self.a.iter_mut().zip(&mut self.b).zip(&mut self.n).zip(x)
                {
                    if !v.is_nan() {
                        *n += 1;
                        let d = v - *m;
                        *m += d / f64::from(*n);
                        *m2 += d * (v - *m);
                    }
                }
            }
        }
    }

    /// The statistic per cell.
    pub(crate) fn result(&self, stat: Stat) -> Vec<f64> {
        let nan = f64::NAN;
        match stat {
            Stat::Mean | Stat::Avg => self
                .a
                .iter()
                .zip(&self.n)
                .map(|(&a, &n)| if n == 0 { nan } else { a / f64::from(n) })
                .collect(),
            Stat::Sum => self
                .a
                .iter()
                .zip(&self.n)
                .map(|(&a, &n)| if n == 0 { nan } else { a })
                .collect(),
            Stat::Min | Stat::Max => self.a.clone(),
            Stat::Range => self.a.iter().zip(&self.b).map(|(&a, &b)| a - b).collect(),
            Stat::Std | Stat::Std1 | Stat::Var | Stat::Var1 => {
                let d = stat.divisor();
                let sqrt = matches!(stat, Stat::Std | Stat::Std1);
                self.b
                    .iter()
                    .zip(&self.n)
                    .map(|(&m2, &n)| {
                        if n <= d {
                            return nan;
                        }
                        let var = m2 / f64::from(n - d);
                        if sqrt { var.sqrt() } else { var }
                    })
                    .collect()
            }
        }
    }
}

struct LaneState {
    stat: Stat,
    period: bool,
    groups: Arc<Vec<usize>>,
    var: usize,
    td: Option<usize>,
    lane: TileBox,
    /// Cells of the lane = outer (dims before time) x inner (dims after time).
    outer: usize,
    inner: usize,
    ncell: usize,
    /// Accumulators: one slot for period statistics, one per output step otherwise.
    slots: Vec<Option<Acc>>,
    /// Period statistics: output step of the open group.
    open: Option<usize>,
}

impl LaneState {
    fn emit(&self, g: usize, acc: Option<&Acc>, td: usize) -> Tile {
        let mut bx = self.lane.clone();
        bx.ranges[td] = g..g + 1;
        let values = match acc {
            Some(a) => a.result(self.stat),
            None => vec![f64::NAN; self.ncell],
        };
        Tile {
            var: self.var,
            bx,
            values: Values::F64(values),
        }
    }

    fn fold<T: Copy + Into<f64>>(
        &mut self,
        vals: &[T],
        t0: usize,
        nt: usize,
        td: usize,
    ) -> Result<Vec<Tile>> {
        let n = self.ncell;
        if vals.len() != nt * n {
            return Err(Error::internal(format!(
                "time statistic: tile of {} values, expected {nt} x {n}",
                vals.len()
            )));
        }
        let mut out = Vec::new();
        let mut j = 0;
        while j < nt {
            let g = self.groups[t0 + j];
            let mut j1 = j + 1;
            while j1 < nt && self.groups[t0 + j1] == g {
                j1 += 1;
            }
            let slot = if self.period {
                if self.open != Some(g) {
                    if let Some(og) = self.open {
                        out.push(self.emit(og, self.slots[0].as_ref(), td));
                        if let Some(a) = self.slots[0].as_mut() {
                            a.reset(self.stat);
                        }
                    }
                    self.open = Some(g);
                }
                0
            } else {
                g
            };
            let (stat, ncell) = (self.stat, self.ncell);
            let acc = self.slots[slot].get_or_insert_with(|| Acc::new(stat, ncell));
            for k in j..j1 {
                acc.update(stat, &vals[k * n..(k + 1) * n]);
            }
            j = j1;
        }
        Ok(out)
    }
}

impl FoldState for LaneState {
    fn push(&mut self, tile: Tile) -> Result<Vec<Tile>> {
        let Some(td) = self.td else {
            return Ok(vec![tile]);
        };
        let t0 = tile.bx.ranges[td].start;
        let nt = tile.bx.ranges[td].len();
        if self.outer == 1 {
            match &tile.values {
                Values::F32(v) => self.fold(v, t0, nt, td),
                Values::F64(v) => self.fold(v, t0, nt, td),
            }
        } else {
            // time is not the first dimension: reorder the tile to time-major
            let (outer, inner) = (self.outer, self.inner);
            let get = |i: usize| match &tile.values {
                Values::F32(v) => f64::from(v[i]),
                Values::F64(v) => v[i],
            };
            let mut tm = vec![0.0f64; nt * outer * inner];
            for o in 0..outer {
                for k in 0..nt {
                    for i in 0..inner {
                        tm[k * outer * inner + o * inner + i] = get((o * nt + k) * inner + i);
                    }
                }
            }
            self.fold(&tm, t0, nt, td)
        }
    }

    fn finish(&mut self) -> Result<Vec<Tile>> {
        let Some(td) = self.td else {
            return Ok(Vec::new());
        };
        if self.period {
            return Ok(match self.open.take() {
                Some(g) => vec![self.emit(g, self.slots[0].as_ref(), td)],
                None => Vec::new(),
            });
        }
        let mut out = Vec::with_capacity(self.slots.len());
        for g in 0..self.slots.len() {
            let acc = self.slots[g].take();
            out.push(self.emit(g, acc.as_ref(), td));
        }
        Ok(out)
    }
}
