//! `shifttime` (cdo's Settime family): every timestep and its bounds shifted by a time interval;
//! the data are not touched.
//!
//! `shifttime,<n><unit>`: `n` a whole number (with sign), `unit` a prefix of `seconds`,
//! `minutes`, `hours`, `days`, `months` or `years`, as cdo reads it (`-1sec`, `3hours`, `1day`,
//! `-1mon`, `1year`):
//! - seconds to days move the date-time by that many seconds of the calendar (cdo's julian-date
//!   arithmetic, any calendar);
//! - months and years move the month and keep the day of the month, the month's last day where
//!   that day does not exist (cdo writes such dates as they are: 2000-01-31 + 1 month gives
//!   2000-02-31, which no calendar has);
//! - times are written in the input's units, in days since its reference where those are months
//!   or years ([`super::timstat::retime`]).
//!
//! A shift is metadata only, so a fold before it stays pending (`-shifttime,-1day -yearmean` is
//! one stage).

use crate::chain::OpNode;
use crate::error::{Error, Result};
use crate::model::{CalDateTime, Calendar};
use crate::plan::Desc;

pub fn handles(name: &str) -> bool {
    name == "shifttime"
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Shift {
    Seconds(i64),
    Months(i64),
}

fn bad(msg: String) -> Error {
    Error::bad_arguments(msg)
        .with("operator", "shifttime".to_owned())
        .with_hint(
            "shifttime,<n><unit>: a whole number and a unit, seconds, minutes, hours, days, \
             months or years, or an unambiguous prefix (-1sec, 3hours, 1day, -1mon, 1year)",
        )
}

fn parse(args: &[String]) -> Result<Shift> {
    let [a] = args else {
        return Err(bad(format!("shifttime takes one argument, got {}", args.len())));
    };
    let a = a.trim();
    let at = a.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(a.len());
    let (num, unit) = (a[..at].trim(), a[at..].trim().to_ascii_lowercase());
    let n: i64 = num.parse().map_err(|_| {
        bad(if num.parse::<f64>().is_ok() {
            format!("shifttime,{a}: the number of units must be whole (cdo rounds it)")
        } else {
            format!("shifttime,{a}: no number of units")
        })
    })?;
    const UNITS: [(&str, i64, bool); 6] = [
        ("seconds", 1, false),
        ("minutes", 60, false),
        ("hours", 3600, false),
        ("days", 86_400, false),
        ("months", 1, true),
        ("years", 12, true),
    ];
    let hits: Vec<_> = UNITS
        .iter()
        .filter(|(u, _, _)| !unit.is_empty() && u.starts_with(unit.as_str()))
        .collect();
    match hits.as_slice() {
        [(_, k, false)] => Ok(Shift::Seconds(n * k)),
        [(_, k, true)] => Ok(Shift::Months(n * k)),
        [] if unit.is_empty() => Err(bad(format!(
            "shifttime,{a}: no unit (cdo then takes whichever unit its table lists first)"
        ))),
        [] => Err(bad(format!("shifttime,{a}: unknown unit '{unit}'"))),
        _ => Err(bad(format!(
            "shifttime,{a}: '{unit}' is ambiguous ({})",
            hits.iter().map(|h| h.0).collect::<Vec<_>>().join(", ")
        ))),
    }
}

/// `d` shifted by `s`, and whether its day of the month was moved to the month's last day.
fn shift(d: CalDateTime, s: Shift, cal: Calendar) -> (CalDateTime, bool) {
    match s {
        Shift::Seconds(n) => (CalDateTime::from_seconds(&d, cal, n), false),
        Shift::Months(n) => {
            let m = d.year as i64 * 12 + d.month as i64 - 1 + n;
            let (year, month) = (m.div_euclid(12) as i32, m.rem_euclid(12) as u32 + 1);
            let day = d.day.min(cal.days_in_month(year, month));
            (CalDateTime { year, month, day, ..d }, day != d.day)
        }
    }
}

/// Output description of `shifttime`: the input with its time axis shifted.
pub fn describe(node: &OpNode, inputs: Vec<Desc>) -> Result<Desc> {
    let s = parse(&node.args)?;
    let mut d = inputs
        .into_iter()
        .next()
        .ok_or_else(|| Error::internal("shifttime without input"))?;
    let Some(t) = d.time.as_ref() else {
        return Ok(d);
    };
    let cal = t.axis.calendar;
    let mut clamped = 0;
    let mut mv = |x: CalDateTime| {
        let (y, c) = shift(x, s, cal);
        clamped += c as usize;
        y
    };
    let stamps: Vec<CalDateTime> = t.axis.steps.iter().map(|x| mv(x.datetime)).collect();
    let bounds = t
        .axis
        .bounds
        .as_ref()
        .map(|b| b.iter().map(|p| [mv(p[0].datetime), mv(p[1].datetime)]).collect());
    let time = super::timstat::retime(t, &stamps, bounds);
    d.time = Some(time);
    if clamped > 0 {
        d.method.push(serde_json::json!({
            "operator": crate::plan::explain::op_token(node),
            "days": format!(
                "{clamped} date(s) on a day their new month does not have moved to its last day \
                 (cdo keeps the day: 2000-01-31 + 1 month gives 2000-02-31)"
            ),
        }));
    }
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_and_shifts() {
        let p = |s: &str| parse(&[s.to_owned()]);
        assert_eq!(p("-1sec").unwrap(), Shift::Seconds(-1));
        assert_eq!(p("3hours").unwrap(), Shift::Seconds(10_800));
        assert_eq!(p("+2d").unwrap(), Shift::Seconds(172_800));
        assert_eq!(p("-1mon").unwrap(), Shift::Months(-1));
        assert_eq!(p("1year").unwrap(), Shift::Months(12));
        for s in ["1", "1m", "2.6day", "day", "1fortnight"] {
            assert!(p(s).is_err(), "{s}");
        }
        let cal = Calendar::Standard;
        let d = CalDateTime::new(2000, 1, 31, 12, 0, 0);
        assert_eq!(shift(d, Shift::Months(1), cal), (CalDateTime::new(2000, 2, 29, 12, 0, 0), true));
        assert_eq!(shift(d, Shift::Months(-13), cal), (CalDateTime::new(1998, 12, 31, 12, 0, 0), false));
        let y = CalDateTime::new(1959, 1, 1, 0, 0, 0);
        assert_eq!(shift(y, Shift::Seconds(-1), cal).0, CalDateTime::new(1958, 12, 31, 23, 59, 59));
        assert_eq!(
            shift(CalDateTime::new(2000, 2, 30, 0, 0, 0), Shift::Seconds(86_400), Calendar::Day360).0,
            CalDateTime::new(2000, 3, 1, 0, 0, 0)
        );
    }
}
