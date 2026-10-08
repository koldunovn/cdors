//! Time grouping for time statistics: which input timesteps form one output step, and the output
//! time axis (timestamp and `time_bnds`) CDO writes for it.
//!
//! References are to the CDO 2.6.5 source (`src/`), checked against cdo 2.6.0 output by
//! `examples/timegroup_check.rs`.
//!
//! **Period groups** (`hour*`, `day*`, `mon*`, `year*`, `tim*`; `Timstat.cc:run_impl`, also
//! `Timpctl.cc` for `hourpctl` .. `yearpctl`, `timpctl`). CDO never sorts: it compares each step with
//! the *first* step of the open group and starts a new group when they differ
//! (`date_is_neq`, `util_date.h:21`). The comparison is on the leading characters of
//! `YYYYMMDDhhmmss`: year for `year*`, year+month for `mon*`, the date for `day*`, date+hour for
//! `hour*`, nothing for `tim*` (one group). Steps that go back in time therefore open new groups.
//!
//! **Seasons** (`seas*`, `Seasstat.cc:131`; `seaspctl`, `Timpctl.cc:170`). Season of a month
//! (`cdo_season.cc:month_to_season`): with the default `CDO_SEASON_START=DEC`, DJF=0, MAM=1, JJA=2,
//! SON=3 (`(month % 12) / 3`); with `CDO_SEASON_START=JAN`, JFM, AMJ, JAS, OND (`(month-1) / 3`).
//! A new season group starts when the season index changes, or when the month goes backwards
//! within the same season, where December counts as month 0 with `DEC` (so December 2000 groups
//! with January and February 2001, and the group is not tied to a year at all: a series holding
//! only Januaries stays one group, as in CDO).
//!
//! **Multi-year groups** (`Ymonstat.cc`, `Ydaystat.cc`). Each step goes to a bucket and the
//! output lists the non-empty buckets in index order, not in order of first appearance:
//! `ymon*` month 1..12, `yseas*` season 0..3 (as above), `yday*` the index
//! `(month - 1) * 31 + day` (`datetime.cc:decode_day_of_year`). The day index is the calendar
//! month and day, not the day of the year: 29 February is its own group (index 60, between 28
//! February and 1 March) in every calendar that has it, and 360_day's 29 and 30 February are
//! groups 60 and 61. `ydaystat,year=Y` sets the year of every output timestamp (not its bounds);
//! `yearMode=true` uses the smallest year among the groups' last members.
//!
//! **Output timestamp** (`datetime.cc:DateTimeList::stat_taxis_def_timestep`) for a group of `n`
//! members `v[0..n]`: `first` = `v[0]`; `last` = `v[n-1]`; `midhigh` = `v[n/2]`; `middle` = `v[n/2]`
//! for odd `n`, and for even `n` the point half-way between `v[n/2-1]` and `v[n/2]`, with the
//! half-difference in seconds rounded half away from zero (`DateTimeList::mean`, `lround`).
//! Default when `--timestat_date` is not given: the environment `CDO_TIMESTAT_DATE`, then
//! `RUNSTAT_DATE` (`datetime.cc:get_timestat_date`), then the operator's own default: `middle` for
//! period and season statistics, percentiles and `run*` (`Timstat.cc:434`, `Seasstat.cc:65`,
//! `Timpctl.cc:124`, `Runstat.cc:132`), `last` for `ymon*` and `yday*` (`Ymonstat.cc:167`,
//! `Ydaystat.cc:148`).
//!
//! **`yseas*` timestamps follow cdo 2.6.0, the reference build**, whose separate `Yseasstat`
//! ignores `--timestat_date`: the member with the latest date, counting a December as December of
//! the previous year and writing that decremented year (`datetime.cc:set_date_time`, still present
//! but unused in 2.6.5); on equal dates the earliest member wins. So DJF of 2000-01..2002-12 data
//! is stamped 2002-02-28, and December-only data from 2000 and 2001 is stamped 2000-12-xx. In 2.6.5
//! `yseas*` moved into `Ymonstat.cc` and uses the `last` rule like `ymon*`.
//!
//! **Time units of months or years**: the timestamps here are exact; CDI then encodes them into
//! the output units with `taxis.c:datetime2rtimeval`, which for `months since` divides the day
//! remainder by the length of the *reference* month while decoding multiplies by the length of the
//! month reached, so e.g. 2001-07-01 00:00 in `months since 2001-01-16` reads back as
//! 2001-07-01 11:36:46. A writer must reproduce that encoding to match `showtimestamp`.
//!
//! **Bounds** (same function): if the input has time bounds, `[b0 of v[0], b1 of v[n-1]]`,
//! otherwise `[v[0], v[n-1]]`. Whether the input has bounds is decided on its first step. All
//! statistics write `time_bnds` except `yseas*`, which deletes them (`Ymonstat.cc:142`).
//!
//! **Running statistics** (`Runstat.cc`): window `k` holds input steps `k .. k+n`; the output has
//! `len - n + 1` steps stamped like a group of `n` members; fewer than `n` input steps is an error.
//!
//! **`--use_time_bounds`** (`datetime.cc:DateTimeList::taxis_inq_timestep`): for period and
//! season groups only, a step whose time of day is 00:00:00 and equals its upper bound (and whose
//! bounds are increasing) is grouped as if it were one second earlier; the output timestamp still
//! uses the original time. Multi-year groups ignore it (`Ymonstat.cc` reads the time directly).

use super::time::{CalDateTime, Calendar, TimeStep};
use crate::error::{Error, Result};
use std::collections::BTreeMap;

/// Which member's time stamps a group (`--timestat_date`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TimestatDate {
    First,
    /// `middle` (CDO's `TimeStat::MEAN`).
    Middle,
    MidHigh,
    Last,
}

impl TimestatDate {
    /// Parses a `--timestat_date` argument (`datetime.cc:set_timestat_date`, case-sensitive).
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "first" => Self::First,
            "middle" => Self::Middle,
            "midhigh" => Self::MidHigh,
            "last" => Self::Last,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::First => "first",
            Self::Middle => "middle",
            Self::MidHigh => "midhigh",
            Self::Last => "last",
        }
    }

    /// `CDO_TIMESTAT_DATE`, else `RUNSTAT_DATE` (case-insensitive); an unknown value counts as unset.
    pub fn from_env() -> Option<Self> {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let v = get("CDO_TIMESTAT_DATE").or_else(|| get("RUNSTAT_DATE"))?;
        Self::parse(&v.to_ascii_lowercase())
    }

    /// The effective choice: command-line option, else environment, else the operator default.
    pub fn resolve(cli: Option<Self>, operator_default: Self) -> Self {
        cli.or_else(Self::from_env).unwrap_or(operator_default)
    }
}

/// First month of the first season (`CDO_SEASON_START`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SeasonStart {
    /// DJF, MAM, JJA, SON (default).
    #[default]
    Dec,
    /// JFM, AMJ, JAS, OND.
    Jan,
}

impl SeasonStart {
    /// `CDO_SEASON_START`: exactly `DEC` or `JAN`; anything else keeps the default (`cdo_season.cc:22`).
    pub fn from_env() -> Self {
        match std::env::var("CDO_SEASON_START").as_deref() {
            Ok("JAN") => Self::Jan,
            _ => Self::Dec,
        }
    }

    /// Season index 0..=3 of a month 1..=12 (`cdo_season.cc:month_to_season`).
    pub fn season(self, month: u32) -> usize {
        match self {
            Self::Dec => (month % 12 / 3) as usize,
            Self::Jan => ((month.max(1) - 1) / 3) as usize,
        }
    }

    pub fn names(self) -> [&'static str; 4] {
        match self {
            Self::Dec => ["DJF", "MAM", "JJA", "SON"],
            Self::Jan => ["JFM", "AMJ", "JAS", "OND"],
        }
    }

    /// Month order within a season: December is month 0 with `Dec` (`Seasstat.cc:136`).
    fn month_rank(self, month: u32) -> u32 {
        if self == Self::Dec && month == 12 {
            0
        } else {
            month
        }
    }
}

/// Consecutive-step groups: `hour*`, `day*`, `mon*`, `seas*`, `year*`, `tim*` (and the
/// percentile variants `hourpctl` .. `yearpctl`, `seaspctl`, `timpctl`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Period {
    Hour,
    Day,
    Month,
    Season,
    Year,
    /// `tim*`: all steps form one group.
    All,
}

impl Period {
    /// CDO's default `--timestat_date` for period statistics.
    pub const DEFAULT_TIMESTAT: TimestatDate = TimestatDate::Middle;

    /// The period of an operator name such as `monmean` or `seaspctl` (by prefix).
    pub fn from_operator(op: &str) -> Option<Self> {
        [
            ("hour", Self::Hour),
            ("day", Self::Day),
            ("mon", Self::Month),
            ("seas", Self::Season),
            ("year", Self::Year),
            ("tim", Self::All),
        ]
        .into_iter()
        .find(|(p, _)| op.starts_with(p))
        .map(|(_, k)| k)
    }

    /// Comparison key for non-season periods (`util_date.h:date_is_neq`).
    fn key(self, t: &CalDateTime) -> (i32, u32, u32, u32) {
        match self {
            Self::Hour => (t.year, t.month, t.day, t.hour),
            Self::Day => (t.year, t.month, t.day, 0),
            Self::Month => (t.year, t.month, 0, 0),
            Self::Year => (t.year, 0, 0, 0),
            Self::All | Self::Season => (0, 0, 0, 0),
        }
    }
}

/// Multi-year groups: `ymon*`, `yday*`, `yseas*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Climatology {
    Month,
    Day,
    Season,
}

impl Climatology {
    /// CDO's default `--timestat_date` for multi-year statistics.
    pub const DEFAULT_TIMESTAT: TimestatDate = TimestatDate::Last;

    /// Number of bucket slots (indices are below this): 17 months, 373 days, 4 seasons.
    pub fn slots(self) -> usize {
        match self {
            Self::Month => 17,
            Self::Day => 373,
            Self::Season => 4,
        }
    }

    /// Bucket of a date: month 1..=12, `(month-1)*31 + day` (0 for an invalid date), or season 0..=3.
    pub fn index(self, t: &CalDateTime, season_start: SeasonStart) -> usize {
        match self {
            Self::Month => t.month as usize,
            Self::Season => season_start.season(t.month),
            Self::Day => {
                if !(1..=31).contains(&t.day) || !(1..=12).contains(&t.month) {
                    0
                } else {
                    ((t.month - 1) * 31 + t.day) as usize
                }
            }
        }
    }

    /// Whether the output has `time_bnds` (`yseas*` drops them).
    pub fn has_bounds(self) -> bool {
        self != Self::Season
    }
}

/// One input timestep as the grouping sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Member {
    pub datetime: CalDateTime,
    /// The step's input time bounds, if the input has a bounds variable.
    pub bounds: Option<[CalDateTime; 2]>,
}

impl Member {
    pub fn new(step: &TimeStep, bounds: Option<&[TimeStep; 2]>) -> Self {
        Self {
            datetime: step.datetime,
            bounds: bounds.map(|b| [b[0].datetime, b[1].datetime]),
        }
    }
}

/// One output timestep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClosedGroup {
    /// Output timestamp.
    pub timestamp: CalDateTime,
    /// Output `time_bnds` (`None` for `yseas*`).
    pub bounds: Option<[CalDateTime; 2]>,
    /// Number of input steps in the group.
    pub count: usize,
    /// Position of the group's first member in the input.
    pub first: usize,
}

/// Output timestamp of a group (`DateTimeList::stat_taxis_def_timestep`, `::mean`, `::midhigh`).
/// `members` must not be empty.
pub fn group_timestamp(members: &[Member], stat: TimestatDate, cal: Calendar) -> CalDateTime {
    let n = members.len();
    match stat {
        TimestatDate::First => members[0].datetime,
        TimestatDate::Last => members[n - 1].datetime,
        TimestatDate::MidHigh => members[n / 2].datetime,
        TimestatDate::Middle if n % 2 == 1 => members[n / 2].datetime,
        TimestatDate::Middle => {
            let a = members[n / 2 - 1].datetime;
            let b = members[n / 2].datetime;
            let half = (b.seconds_since(&a, cal) as f64 / 2.0).round() as i64;
            CalDateTime::from_seconds(&a, cal, half)
        }
    }
}

/// Output bounds of a group: the outer input bounds if the input has bounds (decided by the first
/// member, as CDO decides on the first step), else the first and last member times.
pub fn group_bounds(members: &[Member]) -> [CalDateTime; 2] {
    let (first, last) = (&members[0], &members[members.len() - 1]);
    match (first.bounds, last.bounds) {
        (Some(b0), Some(b1)) => [b0[0], b1[1]],
        _ => [first.datetime, last.datetime],
    }
}

fn close(members: &[Member], first: usize, stat: TimestatDate, cal: Calendar) -> ClosedGroup {
    ClosedGroup {
        timestamp: group_timestamp(members, stat, cal),
        bounds: Some(group_bounds(members)),
        count: members.len(),
        first,
    }
}

/// Streams timesteps in input order and reports each period group when it closes.
///
/// `push` returns the group that the step *closes* (the step itself then opens the next group);
/// `finish` returns the last open group. A kernel therefore finalises its state on `Some`, resets
/// it, and then folds the pushed step.
#[derive(Debug, Clone)]
pub struct GroupTracker {
    period: Period,
    calendar: Calendar,
    stat: TimestatDate,
    season_start: SeasonStart,
    use_time_bounds: bool,
    members: Vec<Member>,
    first: usize,
    pushed: usize,
    key0: (i32, u32, u32, u32),
    season0: usize,
    old_month: u32,
}

impl GroupTracker {
    /// A tracker with `CDO_SEASON_START=DEC` and `--use_time_bounds` off; see the builder methods.
    pub fn new(period: Period, calendar: Calendar, stat: TimestatDate) -> Self {
        Self {
            period,
            calendar,
            stat,
            season_start: SeasonStart::Dec,
            use_time_bounds: false,
            members: Vec::new(),
            first: 0,
            pushed: 0,
            key0: (0, 0, 0, 0),
            season0: 0,
            old_month: 0,
        }
    }

    pub fn with_season_start(mut self, s: SeasonStart) -> Self {
        self.season_start = s;
        self
    }

    /// CDO's `--use_time_bounds` (see the module docs).
    pub fn with_use_time_bounds(mut self, on: bool) -> Self {
        self.use_time_bounds = on;
        self
    }

    /// The time used for grouping (CDO's "corrected" time `c`).
    fn group_time(&self, m: &Member) -> CalDateTime {
        let v = m.datetime;
        match (self.use_time_bounds, m.bounds) {
            (true, Some([b0, b1]))
                if v.second_of_day() == 0
                    && v == b1
                    && b0.seconds_since(&b1, self.calendar) < 0 =>
            {
                CalDateTime::from_seconds(&b1, self.calendar, -1)
            }
            _ => v,
        }
    }

    /// Adds the next input step; returns the group it closes, if any.
    pub fn push(&mut self, step: &TimeStep, bounds: Option<&[TimeStep; 2]>) -> Option<ClosedGroup> {
        self.push_member(Member::new(step, bounds))
    }

    pub fn push_member(&mut self, m: Member) -> Option<ClosedGroup> {
        let t = self.group_time(&m);
        let index = self.pushed;
        self.pushed += 1;
        let mut closed = None;
        if !self.members.is_empty() {
            let new_group = if self.period == Period::Season {
                let rank = self.season_start.month_rank(t.month);
                self.season_start.season(t.month) != self.season0 || rank < self.old_month
            } else {
                self.period.key(&t) != self.key0
            };
            if new_group {
                closed = Some(close(&self.members, self.first, self.stat, self.calendar));
                self.members.clear();
            }
        }
        if self.members.is_empty() {
            self.first = index;
            self.key0 = self.period.key(&t);
            self.season0 = self.season_start.season(t.month);
        }
        self.old_month = self.season_start.month_rank(t.month);
        self.members.push(m);
        closed
    }

    /// Number of steps in the open group.
    pub fn open_count(&self) -> usize {
        self.members.len()
    }

    /// Closes the last group (`None` if no step was pushed).
    pub fn finish(self) -> Option<ClosedGroup> {
        (!self.members.is_empty())
            .then(|| close(&self.members, self.first, self.stat, self.calendar))
    }
}

/// The whole output axis of a period statistic (convenience over [`GroupTracker`]).
pub fn period_axis(tracker: GroupTracker, members: &[Member]) -> Vec<ClosedGroup> {
    let mut tracker = tracker;
    let mut out: Vec<ClosedGroup> = members
        .iter()
        .filter_map(|m| tracker.push_member(*m))
        .collect();
    out.extend(tracker.finish());
    out
}

/// Collects multi-year groups; the output axis is produced after the last step.
#[derive(Debug, Clone)]
pub struct ClimTracker {
    kind: Climatology,
    calendar: Calendar,
    stat: TimestatDate,
    season_start: SeasonStart,
    /// `ydaystat,year=Y`: overrides the year of the output timestamps.
    pub year: Option<i32>,
    /// `ydaystat,yearMode=true`: `year` = smallest year of the groups' last members.
    pub year_mode: bool,
    buckets: BTreeMap<usize, (usize, Vec<Member>)>,
    pushed: usize,
}

impl ClimTracker {
    pub fn new(kind: Climatology, calendar: Calendar, stat: TimestatDate) -> Self {
        Self {
            kind,
            calendar,
            stat,
            season_start: SeasonStart::Dec,
            year: None,
            year_mode: false,
            buckets: BTreeMap::new(),
            pushed: 0,
        }
    }

    pub fn with_season_start(mut self, s: SeasonStart) -> Self {
        self.season_start = s;
        self
    }

    /// Group index of a step (see [`Climatology::index`]).
    pub fn index(&self, t: &CalDateTime) -> usize {
        self.kind.index(t, self.season_start)
    }

    /// Adds the next input step; returns its group index.
    pub fn push(&mut self, step: &TimeStep, bounds: Option<&[TimeStep; 2]>) -> usize {
        self.push_member(Member::new(step, bounds))
    }

    pub fn push_member(&mut self, m: Member) -> usize {
        let idx = self.index(&m.datetime);
        let first = self.pushed;
        self.buckets
            .entry(idx)
            .or_insert((first, Vec::new()))
            .1
            .push(m);
        self.pushed += 1;
        idx
    }

    /// The output axis: one entry per non-empty group, in group-index order.
    pub fn finish(self) -> Vec<(usize, ClosedGroup)> {
        let year = if self.year_mode {
            self.buckets
                .values()
                .map(|(_, m)| m[m.len() - 1].datetime.year)
                .min()
        } else {
            self.year
        };
        self.buckets
            .iter()
            .map(|(&idx, (first, members))| {
                let mut g = close(members, *first, self.stat, self.calendar);
                if let Some(y) = year.filter(|&y| y != 0) {
                    g.timestamp.year = y;
                }
                if self.kind == Climatology::Season {
                    g.timestamp = yseas_timestamp(members);
                    g.bounds = None;
                }
                (idx, g)
            })
            .collect()
    }
}

/// cdo 2.6.0 `yseas*` timestamp (old `Yseasstat.cc`, `datetime.cc:set_date_time`): the member
/// with the latest date, where a December counts as December of the previous year and that
/// decremented year is what gets written; on equal dates the earliest member wins.
/// `--timestat_date` is ignored.
fn yseas_timestamp(members: &[Member]) -> CalDateTime {
    let shifted = |m: &Member| {
        let mut t = m.datetime;
        if t.month == 12 {
            t.year -= 1;
        }
        t
    };
    let date = |t: &CalDateTime| (t.year, t.month, t.day);
    let mut best = shifted(&members[0]);
    for m in &members[1..] {
        let t = shifted(m);
        if date(&t) > date(&best) {
            best = t;
        }
    }
    best
}

/// Output axis of a running statistic over windows of `n` steps (`Runstat.cc`).
pub fn run_axis(
    n: usize,
    members: &[Member],
    stat: TimestatDate,
    cal: Calendar,
) -> Result<Vec<ClosedGroup>> {
    if n == 0 || members.len() < n {
        return Err(Error::bad_data(format!(
            "input has fewer than {n} timesteps"
        )));
    }
    Ok(members
        .windows(n)
        .enumerate()
        .map(|(k, w)| close(w, k, stat, cal))
        .collect())
}
