//! CF time: calendars, time units, and the decoded time axis.
//!
//! Each timestep is kept as a calendar date-time plus an integer count of seconds since the
//! reference date of the time units, counted in the variable's calendar. Grouping by year, month,
//! day or season (later tasks) works on the date-time; differences and durations work on the
//! seconds.
//!
//! Decoding follows CDI (`libcdi/src/taxis.c`, `rtimeval2datetime`): `days` values keep whole
//! days and round the fraction to whole seconds, `hours` are converted to days first, `minutes` to
//! seconds; `months`/`years` add whole calendar months and turn the fraction into days of the
//! month reached (in `360_day`, a month is 30 days). Sub-second parts are rounded to milliseconds
//! and then dropped.
//!
//! Calendars: `standard`/`gregorian` (Julian before 1582-10-15, ten days skipped),
//! `proleptic_gregorian`, `noleap`/`365_day`, `all_leap`/`366_day`, `360_day`, and `julian`.
//! Deviation: CDI treats `julian` like `proleptic_gregorian`; cdors uses the real Julian calendar.

use crate::error::{Error, Result};
use serde::Serialize;

/// CF calendars.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Calendar {
    /// Mixed Julian/Gregorian (CF `standard`, `gregorian`): Julian up to 1582-10-04, Gregorian
    /// from 1582-10-15.
    Standard,
    ProlepticGregorian,
    /// 365 days every year (CF `noleap`, `365_day`).
    NoLeap,
    /// 366 days every year (CF `all_leap`, `366_day`).
    AllLeap,
    /// Twelve 30-day months.
    Day360,
    /// Julian calendar: a leap year every 4 years.
    Julian,
}

const DAYS_365: [u32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
const DAYS_366: [u32; 12] = [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

/// Julian day number of 1582-10-15, the first Gregorian day of the `standard` calendar.
const GREGORIAN_START_JDN: i64 = 2_299_161;

impl Calendar {
    /// Parses a CF calendar name (case-insensitive). `None` for unknown names.
    pub fn from_cf(name: &str) -> Option<Self> {
        Some(match name.trim().to_ascii_lowercase().as_str() {
            "standard" | "gregorian" | "mixed" => Self::Standard,
            "proleptic_gregorian" => Self::ProlepticGregorian,
            "noleap" | "365_day" | "no_leap" => Self::NoLeap,
            "all_leap" | "366_day" => Self::AllLeap,
            "360_day" => Self::Day360,
            "julian" => Self::Julian,
            _ => return None,
        })
    }

    /// CF name as CDO writes it.
    pub fn cf_name(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::ProlepticGregorian => "proleptic_gregorian",
            Self::NoLeap => "365_day",
            Self::AllLeap => "366_day",
            Self::Day360 => "360_day",
            Self::Julian => "julian",
        }
    }

    fn fixed_days_per_year(self) -> Option<i64> {
        match self {
            Self::NoLeap => Some(365),
            Self::AllLeap => Some(366),
            Self::Day360 => Some(360),
            _ => None,
        }
    }

    /// Whether `year` has a 29 February (always false for 360_day).
    pub fn is_leap(self, year: i32) -> bool {
        let greg = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
        let jul = year.rem_euclid(4) == 0;
        match self {
            Self::ProlepticGregorian => greg,
            Self::Julian => jul,
            Self::Standard => {
                if year > 1582 {
                    greg
                } else {
                    jul
                }
            }
            Self::NoLeap | Self::Day360 => false,
            Self::AllLeap => true,
        }
    }

    pub fn days_in_month(self, year: i32, month: u32) -> u32 {
        if self == Self::Day360 {
            return 30;
        }
        let i = (month.clamp(1, 12) - 1) as usize;
        if self.is_leap(year) {
            DAYS_366[i]
        } else {
            DAYS_365[i]
        }
    }

    pub fn days_in_year(self, year: i32) -> u32 {
        match self {
            Self::Day360 => 360,
            Self::Standard if year == 1582 => 355,
            _ => {
                if self.is_leap(year) {
                    366
                } else {
                    365
                }
            }
        }
    }

    /// Consecutive day number of a date within this calendar (Julian day number for the
    /// Gregorian-type calendars, `year * days_per_year + day_of_year` for fixed-length ones).
    pub fn day_number(self, year: i32, month: u32, day: u32) -> i64 {
        if let Some(dpy) = self.fixed_days_per_year() {
            let months: &[u32; 12] = if dpy == 366 { &DAYS_366 } else { &DAYS_365 };
            let before: i64 = if dpy == 360 {
                30 * (month as i64 - 1)
            } else {
                months[..(month as usize - 1)]
                    .iter()
                    .map(|&d| d as i64)
                    .sum()
            };
            return year as i64 * dpy + before + day as i64 - 1;
        }
        match self {
            Self::ProlepticGregorian => gregorian_jdn(year, month, day),
            Self::Julian => julian_jdn(year, month, day),
            _ => {
                if (year, month, day) >= (1582, 10, 15) {
                    gregorian_jdn(year, month, day)
                } else {
                    julian_jdn(year, month, day)
                }
            }
        }
    }

    /// Inverse of [`Calendar::day_number`].
    pub fn from_day_number(self, n: i64) -> (i32, u32, u32) {
        if let Some(dpy) = self.fixed_days_per_year() {
            let year = n.div_euclid(dpy);
            let mut doy = n.rem_euclid(dpy) as u32;
            let mut month = 1;
            while month < 12 {
                let dim = self.days_in_month(year as i32, month);
                if doy < dim {
                    break;
                }
                doy -= dim;
                month += 1;
            }
            return (year as i32, month, doy + 1);
        }
        match self {
            Self::ProlepticGregorian => gregorian_from_jdn(n),
            Self::Julian => julian_from_jdn(n),
            _ => {
                if n >= GREGORIAN_START_JDN {
                    gregorian_from_jdn(n)
                } else {
                    julian_from_jdn(n)
                }
            }
        }
    }
}

/// Days from 1970-01-01 in the proleptic Gregorian calendar (H. Hinnant's algorithm).
fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = y as i64 - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    ((y + i64::from(m <= 2)) as i32, m, d)
}

fn gregorian_jdn(y: i32, m: u32, d: u32) -> i64 {
    days_from_civil(y, m, d) + 2_440_588
}

fn gregorian_from_jdn(n: i64) -> (i32, u32, u32) {
    civil_from_days(n - 2_440_588)
}

fn julian_jdn(y: i32, m: u32, d: u32) -> i64 {
    let a = (14 - m as i64) / 12;
    let yy = y as i64 + 4800 - a;
    let mm = m as i64 + 12 * a - 3;
    d as i64 + (153 * mm + 2) / 5 + 365 * yy + yy.div_euclid(4) - 32_083
}

fn julian_from_jdn(n: i64) -> (i32, u32, u32) {
    let c = n + 32_082;
    let d = (4 * c + 3).div_euclid(1461);
    let e = c - (1461 * d).div_euclid(4);
    let m = (5 * e + 2) / 153;
    let day = (e - (153 * m + 2) / 5 + 1) as u32;
    let month = (m + 3 - 12 * (m / 10)) as u32;
    let year = (d - 4800 + m / 10) as i32;
    (year, month, day)
}

/// A calendar date and time of day (whole seconds). Ordering is chronological.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct CalDateTime {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

impl CalDateTime {
    pub fn new(year: i32, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> Self {
        Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
        }
    }

    pub fn second_of_day(&self) -> i64 {
        (self.hour * 3600 + self.minute * 60 + self.second) as i64
    }

    /// Seconds since `reference`, counted in `cal`.
    pub fn seconds_since(&self, reference: &CalDateTime, cal: Calendar) -> i64 {
        let d1 = cal.day_number(self.year, self.month, self.day);
        let d0 = cal.day_number(reference.year, reference.month, reference.day);
        (d1 - d0) * 86_400 + self.second_of_day() - reference.second_of_day()
    }

    /// The date-time `seconds` after `reference` in `cal`.
    pub fn from_seconds(reference: &CalDateTime, cal: Calendar, seconds: i64) -> Self {
        let d0 = cal.day_number(reference.year, reference.month, reference.day);
        let total = reference.second_of_day() + seconds;
        let days = total.div_euclid(86_400);
        let sod = total.rem_euclid(86_400) as u32;
        let (y, m, d) = cal.from_day_number(d0 + days);
        Self::new(y, m, d, sod / 3600, (sod / 60) % 60, sod % 60)
    }

    /// CDO's date format `%5.4d-%02d-%02d` (year right-aligned in five columns).
    pub fn date_string(&self) -> String {
        let y = if self.year < 0 {
            format!("-{:04}", -(self.year as i64))
        } else {
            format!("{:04}", self.year)
        };
        format!("{:>5}-{:02}-{:02}", y, self.month, self.day)
    }

    /// CDO's time format `%02d:%02d:%02d`.
    pub fn time_string(&self) -> String {
        format!("{:02}:{:02}:{:02}", self.hour, self.minute, self.second)
    }

    /// CDO's `datetime_to_string`: `date_string() + "T" + time_string()`.
    pub fn cdo_string(&self) -> String {
        format!("{}T{}", self.date_string(), self.time_string())
    }

    /// ISO 8601 form without padding to five columns (for JSON).
    pub fn iso(&self) -> String {
        format!("{}T{}", self.date_string().trim_start(), self.time_string())
    }
}

/// Unit of a CF time axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeUnit {
    Second,
    Minute,
    Hour,
    Day,
    Month,
    Year,
}

impl TimeUnit {
    /// Parses a unit word as CDI does (prefix match: `sec`, `minute`, `hour`, `day`, `month`,
    /// `calendar_month`, `year`, plus a few common short forms).
    pub fn parse(word: &str) -> Option<Self> {
        let w = word.trim().to_ascii_lowercase();
        let starts = |p: &str| w.starts_with(p);
        Some(if w == "s" || starts("sec") {
            Self::Second
        } else if starts("minute") || w == "min" || w == "mins" {
            Self::Minute
        } else if starts("hour") || w == "h" || w == "hr" || w == "hrs" {
            Self::Hour
        } else if starts("day") || w == "d" {
            Self::Day
        } else if starts("month") || starts("calendar_month") {
            Self::Month
        } else if starts("year") || starts("calendar_year") {
            Self::Year
        } else {
            return None;
        })
    }

    pub fn cf_name(self) -> &'static str {
        match self {
            Self::Second => "seconds",
            Self::Minute => "minutes",
            Self::Hour => "hours",
            Self::Day => "days",
            Self::Month => "months",
            Self::Year => "years",
        }
    }
}

/// Parsed CF time units: relative (`<unit> since <date>`) or CDO's absolute `day as %Y%m%d.%f`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum TimeUnits {
    Relative {
        unit: TimeUnit,
        reference: CalDateTime,
    },
    /// `day as %Y%m%d.%f`: the value encodes the date; the fraction is the fraction of the day.
    AbsoluteDay,
}

impl TimeUnits {
    /// Parses `"<unit> since <date>[ |T<time>][ <zone>]"` or `"day as %Y%m%d.%f"`.
    pub fn parse(units: &str) -> Result<Self> {
        let s = units.trim();
        let lower = s.to_ascii_lowercase();
        if lower.starts_with("day as %y%m%d") {
            return Ok(Self::AbsoluteDay);
        }
        let Some(pos) = lower.find(" since ") else {
            return Err(Error::bad_data(format!("unsupported time units '{units}'")));
        };
        let unit = TimeUnit::parse(&s[..pos])
            .ok_or_else(|| Error::bad_data(format!("unsupported time unit in '{units}'")))?;
        let reference = parse_reference(&s[pos + 7..])
            .ok_or_else(|| Error::bad_data(format!("cannot parse reference date in '{units}'")))?;
        Ok(Self::Relative { unit, reference })
    }

    /// Whether `units` looks like CF time units.
    pub fn is_time_units(units: &str) -> bool {
        Self::parse(units).is_ok()
    }

    /// The reference date (for absolute units: none).
    pub fn reference(&self) -> Option<CalDateTime> {
        match self {
            Self::Relative { reference, .. } => Some(*reference),
            Self::AbsoluteDay => None,
        }
    }

    /// Encodes a date-time as a value in these units, the inverse of [`Self::decode`]. `None` for
    /// months and years, which CDI encodes with its own month-length rules (not reproduced).
    pub fn encode(&self, t: &CalDateTime, cal: Calendar) -> Option<f64> {
        match self {
            Self::AbsoluteDay => {
                let date = t.year as f64 * 10_000.0 + (t.month * 100 + t.day) as f64;
                Some(date + t.second_of_day() as f64 / 86_400.0)
            }
            Self::Relative { unit, reference } => {
                let secs = t.seconds_since(reference, cal) as f64;
                match unit {
                    TimeUnit::Second => Some(secs),
                    TimeUnit::Minute => Some(secs / 60.0),
                    TimeUnit::Hour => Some(secs / 3600.0),
                    TimeUnit::Day => Some(secs / 86_400.0),
                    TimeUnit::Month | TimeUnit::Year => None,
                }
            }
        }
    }

    /// Decodes one time value into a date-time (CDI's rules, see the module docs).
    pub fn decode(&self, value: f64, cal: Calendar) -> Result<CalDateTime> {
        if !value.is_finite() {
            return Err(Error::bad_data(format!("non-finite time value {value}")));
        }
        // about 100 million years either way: beyond that the value is a fill value or garbage,
        // and the date arithmetic below would overflow
        const MAX_SECONDS: f64 = 3e15;
        let seconds = match self {
            Self::AbsoluteDay => value / 10_000.0 * 365.0 * 86_400.0,
            Self::Relative { unit, .. } => {
                value
                    * match unit {
                        TimeUnit::Second => 1.0,
                        TimeUnit::Minute => 60.0,
                        TimeUnit::Hour => 3_600.0,
                        TimeUnit::Day => 86_400.0,
                        TimeUnit::Month => 31.0 * 86_400.0,
                        TimeUnit::Year => 366.0 * 86_400.0,
                    }
            }
        };
        if seconds.abs() > MAX_SECONDS {
            return Err(Error::bad_data(format!(
                "time value {value} is out of range (a fill value in the time axis?)"
            ))
            .with_hint("time values must be valid dates; check the time variable and its units"));
        }
        match self {
            Self::AbsoluteDay => {
                let date = value.trunc() as i64;
                let frac = value - value.trunc();
                let sod = (frac * 86_400.0).round() as i64;
                let (y, m, d) = (date / 10_000, (date / 100) % 100, date % 100);
                let base = CalDateTime::new(y as i32, m.max(1) as u32, d.max(1) as u32, 0, 0, 0);
                Ok(CalDateTime::from_seconds(&base, cal, sod))
            }
            Self::Relative { unit, reference } => {
                if value == 0.0 {
                    return Ok(*reference);
                }
                let mut unit = *unit;
                let mut value = value;
                let mut reference = *reference;
                if unit == TimeUnit::Month && cal == Calendar::Day360 {
                    unit = TimeUnit::Day;
                    value *= 30.0;
                }
                if matches!(unit, TimeUnit::Month | TimeUnit::Year) {
                    if unit == TimeUnit::Year {
                        value *= 12.0;
                    }
                    let fmon = value % 1.0;
                    let total = reference.year as i64 * 12 + reference.month as i64 - 1
                        + value.trunc() as i64;
                    reference.year = total.div_euclid(12) as i32;
                    reference.month = total.rem_euclid(12) as u32 + 1;
                    // As in CDI, a day beyond the end of the month reached overflows into the
                    // next month (day_number is linear in the day).
                    let dim = cal.days_in_month(reference.year, reference.month);
                    value = fmon * dim as f64;
                    unit = TimeUnit::Day;
                }
                let ms: i64 = match unit {
                    TimeUnit::Second | TimeUnit::Minute => {
                        let secs = if unit == TimeUnit::Minute {
                            value * 60.0
                        } else {
                            value
                        };
                        let days = (secs / 86_400.0).trunc() as i64;
                        let rest = ((secs % 86_400.0) * 1000.0).round() as i64;
                        days * 86_400_000 + rest
                    }
                    _ => {
                        let days_v = if unit == TimeUnit::Hour {
                            value / 24.0
                        } else {
                            value
                        };
                        let days = days_v.trunc() as i64;
                        let secs = ((days_v - days_v.trunc()) * 86_400.0).round() as i64;
                        days * 86_400_000 + secs * 1000
                    }
                };
                Ok(CalDateTime::from_seconds(
                    &reference,
                    cal,
                    ms.div_euclid(1000),
                ))
            }
        }
    }
}

/// Parses `YYYY-M-D[( |T)h[:m[:s[.f]]]][Z| UTC| +00:00]`.
fn parse_reference(s: &str) -> Option<CalDateTime> {
    let s = s.trim();
    let (neg, s) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s),
    };
    let (date, time) = match s.find(['T', ' ']) {
        Some(i) => (&s[..i], s[i + 1..].trim()),
        None => (s, ""),
    };
    let mut dp = date.split('-');
    let mut year: i32 = dp.next()?.trim().parse().ok()?;
    if neg {
        year = -year;
    }
    let month: u32 = dp.next().map_or(Some(1), |v| v.trim().parse().ok())?;
    let day: u32 = dp.next().map_or(Some(1), |v| v.trim().parse().ok())?;
    let time = time
        .trim_end_matches('Z')
        .split([' ', '+', 'Z'])
        .next()
        .unwrap_or("");
    let mut tp = time.split(':').filter(|t| !t.is_empty());
    let hour: u32 = tp.next().map_or(Some(0), |v| v.parse().ok())?;
    let minute: u32 = tp.next().map_or(Some(0), |v| v.parse().ok())?;
    let second: f64 = tp.next().map_or(Some(0.0), |v| v.parse().ok())?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    Some(CalDateTime::new(
        year,
        month,
        day,
        hour,
        minute,
        second.trunc() as u32,
    ))
}

/// One timestep: calendar date-time and whole seconds since the reference date.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct TimeStep {
    pub datetime: CalDateTime,
    pub seconds: i64,
}

/// The decoded time axis of a dataset.
#[derive(Debug, Clone, Serialize)]
pub struct TimeAxis {
    /// Name of the time coordinate variable.
    pub var: String,
    /// Name of the time dimension.
    pub dim: String,
    /// The `units` attribute as written in the file.
    pub units_attr: String,
    pub units: TimeUnits,
    pub calendar: Calendar,
    /// Reference date for `seconds` (for absolute units: the first timestep's date at 00:00).
    pub reference: CalDateTime,
    pub steps: Vec<TimeStep>,
    /// Name of the bounds variable, if any.
    pub bounds_var: Option<String>,
    /// Bounds per timestep, if a bounds variable exists.
    pub bounds: Option<Vec<[TimeStep; 2]>>,
}

impl TimeAxis {
    /// Decodes raw time values (and optional bounds, two per step) into a time axis.
    pub fn decode(
        var: &str,
        dim: &str,
        units_attr: &str,
        calendar_attr: Option<&str>,
        values: &[f64],
        bounds: Option<(&str, &[f64])>,
    ) -> Result<Self> {
        let units = TimeUnits::parse(units_attr)?;
        let calendar = match calendar_attr {
            None => Calendar::Standard,
            Some(c) => Calendar::from_cf(c).ok_or_else(|| {
                Error::bad_data(format!("unsupported calendar '{c}' of variable '{var}'"))
            })?,
        };
        let decode_all = |vals: &[f64]| -> Result<Vec<CalDateTime>> {
            vals.iter().map(|&v| units.decode(v, calendar)).collect()
        };
        let dts = decode_all(values)?;
        let reference = units.reference().unwrap_or_else(|| {
            dts.first()
                .map(|d| CalDateTime::new(d.year, d.month, d.day, 0, 0, 0))
                .unwrap_or(CalDateTime::new(1, 1, 1, 0, 0, 0))
        });
        let mk = |dt: CalDateTime| TimeStep {
            datetime: dt,
            seconds: dt.seconds_since(&reference, calendar),
        };
        let steps = dts.into_iter().map(mk).collect();
        let (bounds_var, bounds) = match bounds {
            Some((name, b)) if b.len() == 2 * values.len() => {
                let bd = decode_all(b)?;
                let pairs = bd.chunks(2).map(|p| [mk(p[0]), mk(p[1])]).collect();
                (Some(name.to_owned()), Some(pairs))
            }
            _ => (None, None),
        };
        Ok(Self {
            var: var.to_owned(),
            dim: dim.to_owned(),
            units_attr: units_attr.to_owned(),
            units,
            calendar,
            reference,
            steps,
            bounds_var,
            bounds,
        })
    }

    pub fn len(&self) -> usize {
        self.steps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
}
