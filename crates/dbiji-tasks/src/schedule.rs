//! Task schedules.
//!
//! * [`Schedule::Cron`] accepts standard 5-field Unix cron expressions
//!   (`minute hour day-of-month month day-of-week`, day-of-week `0`/`7` =
//!   Sunday, and the Unix rule that a job runs when *either* restricted
//!   day field matches), the macros `@hourly`, `@daily`/`@midnight`,
//!   `@weekly`, `@monthly`, `@yearly`/`@annually`, and 6/7-field expressions
//!   with seconds (and years) in the Quartz convention of the `cron` crate
//!   (day-of-week `1` = Sunday; prefer names such as `MON-FRI`).
//! * [`Schedule::Interval`] runs every N seconds after the previous run.
//! * [`Schedule::Daily`] runs at a local wall clock time, optionally on some
//!   weekdays only.
//! * [`Schedule::Once`] runs once.
//!
//! Daylight saving time: wall clock times that do not exist (spring forward)
//! run at the first valid minute after the gap; times that occur twice (fall
//! back) run once, at the first occurrence.

use chrono::{DateTime, Datelike, Duration, Local, NaiveDateTime, NaiveTime, TimeZone, Weekday};
use dbiji_core::{tr, trf};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

/// When a task runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Schedule {
    /// Cron expression (see the module documentation).
    Cron(String),
    /// Every `every_secs` seconds after the previous run.
    Interval {
        /// Interval in seconds.
        every_secs: u64,
    },
    /// Every day at a local wall clock time, optionally on some weekdays only.
    Daily {
        /// `HH:MM` (local time).
        at: String,
        /// 1 = Monday ... 7 = Sunday (0 is accepted for Sunday); empty = every day.
        #[serde(default)]
        weekdays: Vec<u8>,
    },
    /// Once at the given time.
    Once {
        /// When to run.
        at: DateTime<Local>,
    },
}

/// Invalid schedule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleError(pub String);

impl fmt::Display for ScheduleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ScheduleError {}

impl From<ScheduleError> for dbiji_core::DbError {
    fn from(e: ScheduleError) -> Self {
        dbiji_core::DbError::config(e.0)
    }
}

/// A cron expression compiled for the `cron` crate. Unix expressions that
/// restrict both day fields become two schedules (either may match).
#[derive(Debug, Clone)]
pub(crate) struct CompiledCron {
    pub primary: cron::Schedule,
    pub alternate: Option<cron::Schedule>,
    /// Original expression was a 5-field Unix expression (or a macro).
    pub unix: bool,
}

const DOW_NAMES: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];

fn dow_value(s: &str) -> Result<u32, ScheduleError> {
    let t = s.trim().to_ascii_lowercase();
    if let Ok(n) = t.parse::<u32>() {
        return if n <= 7 { Ok(n) } else { Err(ScheduleError(trf("Invalid day of week: {}", &[&s]))) };
    }
    let names = [
        ("sun", 0),
        ("sunday", 0),
        ("mon", 1),
        ("monday", 1),
        ("tue", 2),
        ("tues", 2),
        ("tuesday", 2),
        ("wed", 3),
        ("wednesday", 3),
        ("thu", 4),
        ("thurs", 4),
        ("thursday", 4),
        ("fri", 5),
        ("friday", 5),
        ("sat", 6),
        ("saturday", 6),
    ];
    names.iter().find(|(n, _)| *n == t).map(|(_, v)| *v).ok_or_else(|| ScheduleError(trf("Invalid day of week: {}", &[&s])))
}

/// Unix day-of-week field (0/7 = Sunday) as a list of day names understood
/// by the `cron` crate.
fn unix_dow_field(field: &str) -> Result<String, ScheduleError> {
    if field == "*" || field == "?" {
        return Ok(field.to_string());
    }
    let mut days = BTreeSet::new();
    for item in field.split(',') {
        let (base, step) = match item.split_once('/') {
            Some((b, s)) => (b, Some(s.parse::<u32>().map_err(|_| ScheduleError(trf("Invalid step in day of week: {}", &[&item])))?)),
            None => (item, None),
        };
        let (lo, hi) = if base == "*" || base == "?" {
            (0, 6)
        } else if let Some((a, b)) = base.split_once('-') {
            (dow_value(a)?, dow_value(b)?)
        } else {
            let v = dow_value(base)?;
            (v, if step.is_some() { 6 } else { v })
        };
        if lo > hi {
            return Err(ScheduleError(trf("Invalid day of week range: {}", &[&item])));
        }
        let step = step.unwrap_or(1);
        if step == 0 {
            return Err(ScheduleError(trf("Invalid step in day of week: {}", &[&item])));
        }
        let mut d = lo;
        while d <= hi {
            days.insert(d % 7);
            d += step;
        }
    }
    Ok(days.iter().map(|&d| DOW_NAMES[d as usize]).collect::<Vec<_>>().join(","))
}

fn parse_cron(expr: &str) -> Result<cron::Schedule, ScheduleError> {
    cron::Schedule::from_str(expr).map_err(|e| ScheduleError(trf("Invalid cron expression '{}': {}", &[&expr, &e])))
}

/// Compile a cron expression (5, 6 or 7 fields, or a macro).
pub(crate) fn compile_cron(expr: &str) -> Result<CompiledCron, ScheduleError> {
    let e = expr.split_whitespace().collect::<Vec<_>>().join(" ");
    if e.is_empty() {
        return Err(ScheduleError(tr("The cron expression is empty").to_string()));
    }
    if let Some(m) = e.strip_prefix('@') {
        let mapped = match m.to_ascii_lowercase().as_str() {
            "yearly" | "annually" => "@yearly",
            "monthly" => "@monthly",
            "weekly" => "@weekly",
            "daily" | "midnight" => "@daily",
            "hourly" => "@hourly",
            _ => return Err(ScheduleError(trf("Unsupported cron macro: {}", &[&e]))),
        };
        return Ok(CompiledCron { primary: parse_cron(mapped)?, alternate: None, unix: true });
    }
    let f: Vec<&str> = e.split(' ').collect();
    match f.len() {
        5 => {
            let dow = unix_dow_field(f[4])?;
            let restricted = |x: &str| !x.starts_with('*') && x != "?";
            if restricted(f[2]) && restricted(f[4]) {
                Ok(CompiledCron {
                    primary: parse_cron(&format!("0 {} {} {} {} *", f[0], f[1], f[2], f[3]))?,
                    alternate: Some(parse_cron(&format!("0 {} {} * {} {}", f[0], f[1], f[3], dow))?),
                    unix: true,
                })
            } else {
                Ok(CompiledCron { primary: parse_cron(&format!("0 {} {} {} {} {}", f[0], f[1], f[2], f[3], dow))?, alternate: None, unix: true })
            }
        }
        6 | 7 => Ok(CompiledCron { primary: parse_cron(&e)?, alternate: None, unix: false }),
        n => Err(ScheduleError(trf("A cron expression needs 5, 6 or 7 fields, found {}", &[&n]))),
    }
}

fn cron_next<Tz: TimeZone>(s: &cron::Schedule, after: &DateTime<Tz>) -> Option<DateTime<Tz>> {
    // ambiguous local times may yield an instant before `after`; skip those
    s.after(after).take(8).find(|t| t > after)
}

/// Parse `HH:MM` (or `HH:MM:SS`).
pub(crate) fn parse_time_of_day(at: &str) -> Result<NaiveTime, ScheduleError> {
    let t = at.trim();
    NaiveTime::parse_from_str(t, "%H:%M").or_else(|_| NaiveTime::parse_from_str(t, "%H:%M:%S")).map_err(|_| ScheduleError(trf("Invalid time '{}', expected HH:MM", &[&at])))
}

fn weekday_of(n: u8) -> Option<Weekday> {
    Some(match n {
        1 => Weekday::Mon,
        2 => Weekday::Tue,
        3 => Weekday::Wed,
        4 => Weekday::Thu,
        5 => Weekday::Fri,
        6 => Weekday::Sat,
        0 | 7 => Weekday::Sun,
        _ => return None,
    })
}

/// Earliest instant for a local wall time; times inside a DST gap move to
/// the first valid minute after the gap.
fn resolve_local<Tz: TimeZone>(tz: &Tz, naive: NaiveDateTime) -> Option<DateTime<Tz>> {
    if let Some(t) = tz.from_local_datetime(&naive).earliest() {
        return Some(t);
    }
    (1..=24 * 60).find_map(|m| tz.from_local_datetime(&(naive + Duration::minutes(m))).earliest())
}

fn short_day(w: Weekday) -> &'static str {
    match w {
        Weekday::Mon => "Mon",
        Weekday::Tue => "Tue",
        Weekday::Wed => "Wed",
        Weekday::Thu => "Thu",
        Weekday::Fri => "Fri",
        Weekday::Sat => "Sat",
        Weekday::Sun => "Sun",
    }
}

impl Schedule {
    /// Check the schedule for errors.
    pub fn validate(&self) -> Result<(), ScheduleError> {
        match self {
            Schedule::Cron(expr) => compile_cron(expr).map(|_| ()),
            Schedule::Interval { every_secs } => {
                if *every_secs == 0 {
                    Err(ScheduleError(tr("The interval must be at least one second").to_string()))
                } else if *every_secs > 366 * 24 * 3600 {
                    Err(ScheduleError(tr("The interval must be at most one year").to_string()))
                } else {
                    Ok(())
                }
            }
            Schedule::Daily { at, weekdays } => {
                parse_time_of_day(at)?;
                match weekdays.iter().find(|d| weekday_of(**d).is_none()) {
                    Some(d) => Err(ScheduleError(trf("Invalid weekday {} (use 1 = Monday ... 7 = Sunday)", &[d]))),
                    None => Ok(()),
                }
            }
            Schedule::Once { .. } => Ok(()),
        }
    }

    /// First run time strictly after `after` (for [`Schedule::Interval`]:
    /// `after` + interval). `None` when the schedule never fires again or is invalid.
    pub fn next_after(&self, after: DateTime<Local>) -> Option<DateTime<Local>> {
        self.next_after_in(&after)
    }

    /// [`Schedule::next_after`] in any time zone.
    pub fn next_after_in<Tz: TimeZone>(&self, after: &DateTime<Tz>) -> Option<DateTime<Tz>> {
        match self {
            Schedule::Cron(expr) => {
                let c = compile_cron(expr).ok()?;
                let a = cron_next(&c.primary, after);
                let b = c.alternate.as_ref().and_then(|s| cron_next(s, after));
                match (a, b) {
                    (Some(x), Some(y)) => Some(if y < x { y } else { x }),
                    (x, y) => x.or(y),
                }
            }
            Schedule::Interval { every_secs } => {
                if *every_secs == 0 {
                    return None;
                }
                after.clone().checked_add_signed(Duration::seconds(i64::try_from(*every_secs).ok()?))
            }
            Schedule::Daily { at, weekdays } => {
                let time = parse_time_of_day(at).ok()?;
                let days: Vec<Weekday> = weekdays.iter().filter_map(|d| weekday_of(*d)).collect();
                if !weekdays.is_empty() && days.is_empty() {
                    return None;
                }
                let tz = after.timezone();
                let start = after.naive_local().date();
                for offset in 0..=8 {
                    let date = start.checked_add_signed(Duration::days(offset))?;
                    if !days.is_empty() && !days.contains(&date.weekday()) {
                        continue;
                    }
                    if let Some(t) = resolve_local(&tz, date.and_time(time))
                        && t > *after
                    {
                        return Some(t);
                    }
                }
                None
            }
            Schedule::Once { at } => {
                let t = at.with_timezone(&after.timezone());
                if t > *after { Some(t) } else { None }
            }
        }
    }

    /// First occurrence after `now`, keeping interval schedules on the grid
    /// anchored at `anchor` (used to skip missed occurrences).
    pub fn next_after_skipping<Tz: TimeZone>(&self, anchor: &DateTime<Tz>, now: &DateTime<Tz>) -> Option<DateTime<Tz>> {
        match self {
            Schedule::Interval { every_secs } if *every_secs > 0 && now >= anchor => {
                let every = i64::try_from(*every_secs).ok()?;
                let elapsed = (now.clone() - anchor.clone()).num_seconds();
                let k = elapsed / every + 1;
                anchor.clone().checked_add_signed(Duration::seconds(every.checked_mul(k)?))
            }
            _ => self.next_after_in(if now > anchor { now } else { anchor }),
        }
    }

    /// Human readable description (localised).
    pub fn describe(&self) -> String {
        match self {
            Schedule::Cron(expr) => describe_cron(expr),
            Schedule::Interval { every_secs } => {
                let s = *every_secs;
                if s > 0 && s % 86_400 == 0 {
                    plural(s / 86_400, "Every day", "Every {} days")
                } else if s > 0 && s % 3600 == 0 {
                    plural(s / 3600, "Every hour", "Every {} hours")
                } else if s > 0 && s % 60 == 0 {
                    plural(s / 60, "Every minute", "Every {} minutes")
                } else {
                    plural(s, "Every second", "Every {} seconds")
                }
            }
            Schedule::Daily { at, weekdays } => {
                let time = parse_time_of_day(at).map(|t| t.format("%H:%M").to_string()).unwrap_or_else(|_| at.clone());
                let days: BTreeSet<u8> = weekdays.iter().map(|d| if *d == 0 { 7 } else { *d }).collect();
                if days.is_empty() || days.len() == 7 {
                    trf("Every day at {}", &[&time])
                } else if days == (1..=5).collect() {
                    trf("Weekdays at {}", &[&time])
                } else if days == [6, 7].into_iter().collect() {
                    trf("Weekends at {}", &[&time])
                } else {
                    let names: Vec<&str> = days.iter().filter_map(|d| weekday_of(*d)).map(|w| tr(short_day(w))).collect();
                    trf("Every {} at {}", &[&names.join(", "), &time])
                }
            }
            Schedule::Once { at } => trf("Once at {}", &[&at.format("%Y-%m-%d %H:%M")]),
        }
    }
}

fn plural(n: u64, one: &str, many: &str) -> String {
    if n == 1 { tr(one).to_string() } else { trf(many, &[&n]) }
}

fn describe_cron(expr: &str) -> String {
    let e = expr.split_whitespace().collect::<Vec<_>>();
    let fallback = || trf("Cron schedule: {}", &[&expr.trim()]);
    match e.as_slice() {
        [m] if m.starts_with('@') => match m.to_ascii_lowercase().as_str() {
            "@hourly" => tr("Every hour").to_string(),
            "@daily" | "@midnight" => trf("Every day at {}", &[&"00:00"]),
            "@weekly" => trf("Every {} at {}", &[&tr("Sun"), &"00:00"]),
            "@monthly" => tr("Monthly on day 1 at 00:00").to_string(),
            "@yearly" | "@annually" => tr("Every year on January 1 at 00:00").to_string(),
            _ => fallback(),
        },
        [min, hour, dom, mon, dow] => {
            let num = |s: &str| s.parse::<u32>().ok();
            match (num(min), num(hour), *dom, *mon, *dow) {
                (Some(m), Some(h), "*", "*", "*") if m < 60 && h < 24 => trf("Every day at {}", &[&format!("{h:02}:{m:02}")]),
                (Some(m), Some(h), "*", "*", d) if m < 60 && h < 24 => match unix_dow_field(d) {
                    Ok(names) => {
                        let time = format!("{h:02}:{m:02}");
                        if names == "MON,TUE,WED,THU,FRI" {
                            trf("Weekdays at {}", &[&time])
                        } else {
                            let pretty: Vec<String> = names
                                .split(',')
                                .map(|n| {
                                    let mut c = n.chars();
                                    match c.next() {
                                        Some(f) => format!("{f}{}", c.as_str().to_lowercase()),
                                        None => String::new(),
                                    }
                                })
                                .map(|n| tr(&n).to_string())
                                .collect();
                            trf("Every {} at {}", &[&pretty.join(", "), &time])
                        }
                    }
                    Err(_) => fallback(),
                },
                (Some(m), None, "*", "*", "*") if *hour == "*" && m < 60 => trf("Every hour at minute {}", &[&m]),
                _ => match (*min, *hour, *dom, *mon, *dow) {
                    ("*", "*", "*", "*", "*") => tr("Every minute").to_string(),
                    (step, "*", "*", "*", "*") if step.starts_with("*/") => match step[2..].parse::<u64>() {
                        Ok(n) => plural(n, "Every minute", "Every {} minutes"),
                        Err(_) => fallback(),
                    },
                    ("0", step, "*", "*", "*") if step.starts_with("*/") => match step[2..].parse::<u64>() {
                        Ok(n) => plural(n, "Every hour", "Every {} hours"),
                        Err(_) => fallback(),
                    },
                    _ => fallback(),
                },
            }
        }
        _ => fallback(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, LocalResult, NaiveDate, Offset, Timelike};

    /// US Eastern style zone: UTC-5, UTC-4 from the second Sunday of March
    /// 02:00 to the first Sunday of November 02:00 (local).
    #[derive(Debug, Clone, Copy)]
    struct Eastern;

    fn nth_sunday(year: i32, month: u32, n: u32) -> NaiveDate {
        let first = NaiveDate::from_ymd_opt(year, month, 1).unwrap();
        let offset = (7 - first.weekday().num_days_from_sunday()) % 7;
        first + Duration::days((offset + 7 * (n - 1)) as i64)
    }

    impl Eastern {
        fn offset_at_utc(utc: &NaiveDateTime) -> FixedOffset {
            let y = utc.date().year();
            let start = nth_sunday(y, 3, 2).and_hms_opt(7, 0, 0).unwrap(); // 02:00 EST
            let end = nth_sunday(y, 11, 1).and_hms_opt(6, 0, 0).unwrap(); // 02:00 EDT
            if *utc >= start && *utc < end { FixedOffset::west_opt(4 * 3600).unwrap() } else { FixedOffset::west_opt(5 * 3600).unwrap() }
        }
    }

    impl TimeZone for Eastern {
        type Offset = FixedOffset;
        fn from_offset(_: &FixedOffset) -> Self {
            Eastern
        }
        fn offset_from_local_date(&self, local: &NaiveDate) -> LocalResult<FixedOffset> {
            self.offset_from_local_datetime(&local.and_hms_opt(12, 0, 0).unwrap())
        }
        fn offset_from_local_datetime(&self, local: &NaiveDateTime) -> LocalResult<FixedOffset> {
            let edt = FixedOffset::west_opt(4 * 3600).unwrap();
            let est = FixedOffset::west_opt(5 * 3600).unwrap();
            let edt_ok = Eastern::offset_at_utc(&(*local + Duration::hours(4))) == edt;
            let est_ok = Eastern::offset_at_utc(&(*local + Duration::hours(5))) == est;
            match (edt_ok, est_ok) {
                (true, true) => LocalResult::Ambiguous(edt, est),
                (true, false) => LocalResult::Single(edt),
                (false, true) => LocalResult::Single(est),
                (false, false) => LocalResult::None,
            }
        }
        fn offset_from_utc_date(&self, utc: &NaiveDate) -> FixedOffset {
            Eastern::offset_at_utc(&utc.and_hms_opt(12, 0, 0).unwrap())
        }
        fn offset_from_utc_datetime(&self, utc: &NaiveDateTime) -> FixedOffset {
            Eastern::offset_at_utc(utc)
        }
    }

    fn et(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Eastern> {
        Eastern.with_ymd_and_hms(y, mo, d, h, mi, 0).earliest().unwrap()
    }

    fn local_fields<Tz: TimeZone>(t: &DateTime<Tz>) -> (u32, u32, u32, u32, i32) {
        let n = t.naive_local();
        (n.month(), n.day(), n.hour(), n.minute(), t.offset().fix().local_minus_utc() / 3600)
    }

    #[test]
    fn five_field_cron() {
        let s = Schedule::Cron("30 9 * * 1-5".into());
        s.validate().unwrap();
        // Saturday 2026-10-17 -> Monday 2026-10-19 09:30
        let n = s.next_after_in(&et(2026, 10, 17, 10, 0)).unwrap();
        assert_eq!(local_fields(&n), (10, 19, 9, 30, -4));
        // Sunday as 0 and 7
        let sun0 = Schedule::Cron("0 12 * * 0".into()).next_after_in(&et(2026, 10, 17, 0, 0)).unwrap();
        let sun7 = Schedule::Cron("0 12 * * 7".into()).next_after_in(&et(2026, 10, 17, 0, 0)).unwrap();
        assert_eq!(sun0, sun7);
        assert_eq!(sun0.naive_local().weekday(), Weekday::Sun);
        // every 15 minutes
        let q = Schedule::Cron("*/15 * * * *".into());
        let n = q.next_after_in(&et(2026, 1, 5, 8, 7)).unwrap();
        assert_eq!(local_fields(&n), (1, 5, 8, 15, -5));
        let n = q.next_after_in(&n).unwrap();
        assert_eq!(local_fields(&n), (1, 5, 8, 30, -5));
        // names and macros
        assert!(Schedule::Cron("0 0 1 JAN *".into()).validate().is_ok());
        assert!(Schedule::Cron("@daily".into()).validate().is_ok());
        assert!(Schedule::Cron("@annually".into()).validate().is_ok());
        assert!(Schedule::Cron("@reboot".into()).validate().is_err());
        assert!(Schedule::Cron("1 2 3".into()).validate().is_err());
        assert!(Schedule::Cron("61 * * * *".into()).validate().is_err());
        assert!(Schedule::Cron("0 0 * * 8".into()).validate().is_err());
        assert!(Schedule::Cron("".into()).validate().is_err());
    }

    #[test]
    fn unix_day_fields_are_ored() {
        // the 13th of the month OR any Friday
        let s = Schedule::Cron("0 0 13 * 5".into());
        let after = et(2026, 3, 1, 0, 0); // Sunday
        let a = s.next_after_in(&after).unwrap(); // Friday March 6
        assert_eq!((a.naive_local().month(), a.naive_local().day()), (3, 6));
        let b = s.next_after_in(&et(2026, 3, 12, 1, 0)).unwrap(); // Friday March 13 is both
        assert_eq!((b.naive_local().month(), b.naive_local().day()), (3, 13));
        let c = s.next_after_in(&et(2026, 4, 11, 0, 0)).unwrap(); // Monday April 13 (13th)
        assert_eq!((c.naive_local().month(), c.naive_local().day()), (4, 13));
        // a step on the day of week counts as unrestricted (Vixie cron)
        assert!(compile_cron("0 0 13 * */2").unwrap().alternate.is_none());
        assert_eq!(unix_dow_field("5-7").unwrap(), "SUN,FRI,SAT");
        assert_eq!(unix_dow_field("mon,wed-fri/2").unwrap(), "MON,WED,FRI");
        assert_eq!(unix_dow_field("*/2").unwrap(), "SUN,TUE,THU,SAT");
    }

    #[test]
    fn six_and_seven_field_cron() {
        let s = Schedule::Cron("15 30 9 * * MON-FRI".into());
        let n = s.next_after_in(&et(2026, 10, 19, 9, 30)).unwrap();
        assert_eq!(n.naive_local().second(), 15);
        let y = Schedule::Cron("0 0 12 1 1 * 2030".into());
        let n = y.next_after_in(&et(2026, 1, 1, 0, 0)).unwrap();
        assert_eq!(n.naive_local().year(), 2030);
        assert!(Schedule::Cron("0 0 12 1 1 * 2020".into()).next_after_in(&et(2026, 1, 1, 0, 0)).is_none());
    }

    #[test]
    fn interval_and_once() {
        let s = Schedule::Interval { every_secs: 3600 };
        let a = et(2026, 11, 1, 0, 30);
        let b = s.next_after_in(&a).unwrap();
        let c = s.next_after_in(&b).unwrap();
        // across the fall-back hour the wall clock repeats 01:30
        assert_eq!(local_fields(&b), (11, 1, 1, 30, -4));
        assert_eq!(local_fields(&c), (11, 1, 1, 30, -5));
        assert_eq!((c - a).num_hours(), 2);
        assert!(Schedule::Interval { every_secs: 0 }.validate().is_err());
        // skipping missed occurrences keeps the grid
        let anchor = et(2026, 1, 1, 0, 0);
        let now = et(2026, 1, 1, 5, 10);
        let n = Schedule::Interval { every_secs: 3600 }.next_after_skipping(&anchor, &now).unwrap();
        assert_eq!(local_fields(&n), (1, 1, 6, 0, -5));
        let at = Local.with_ymd_and_hms(2030, 5, 6, 7, 8, 0).earliest().unwrap();
        let once = Schedule::Once { at };
        assert_eq!(once.next_after(at - Duration::seconds(1)), Some(at));
        assert_eq!(once.next_after(at), None);
    }

    #[test]
    fn daily_with_weekdays() {
        let s = Schedule::Daily { at: "09:00".into(), weekdays: vec![1, 3, 5] };
        s.validate().unwrap();
        let n = s.next_after_in(&et(2026, 10, 19, 9, 0)).unwrap(); // Monday 09:00 exactly -> Wednesday
        assert_eq!(local_fields(&n), (10, 21, 9, 0, -4));
        let n = s.next_after_in(&et(2026, 10, 19, 8, 59)).unwrap();
        assert_eq!(local_fields(&n), (10, 19, 9, 0, -4));
        let every = Schedule::Daily { at: "23:59".into(), weekdays: vec![] };
        assert_eq!(local_fields(&every.next_after_in(&et(2026, 12, 31, 23, 59)).unwrap()), (1, 1, 23, 59, -5));
        let sunday = Schedule::Daily { at: "10:00".into(), weekdays: vec![0] };
        assert_eq!(sunday.next_after_in(&et(2026, 10, 19, 0, 0)).unwrap().naive_local().weekday(), Weekday::Sun);
        assert!(Schedule::Daily { at: "25:00".into(), weekdays: vec![] }.validate().is_err());
        assert!(Schedule::Daily { at: "09:00".into(), weekdays: vec![8] }.validate().is_err());
    }

    #[test]
    fn dst_edges() {
        // spring forward 2026-03-08: 02:30 does not exist -> first valid minute (03:00 EDT)
        let daily = Schedule::Daily { at: "02:30".into(), weekdays: vec![] };
        let n = daily.next_after_in(&et(2026, 3, 7, 12, 0)).unwrap();
        assert_eq!(local_fields(&n), (3, 8, 3, 0, -4));
        let n = daily.next_after_in(&n).unwrap();
        assert_eq!(local_fields(&n), (3, 9, 2, 30, -4));
        // fall back 2026-11-01: 01:30 happens twice -> runs once, at the first one
        let daily = Schedule::Daily { at: "01:30".into(), weekdays: vec![] };
        let first = daily.next_after_in(&et(2026, 10, 31, 12, 0)).unwrap();
        assert_eq!(local_fields(&first), (11, 1, 1, 30, -4));
        let next = daily.next_after_in(&first).unwrap();
        assert_eq!(local_fields(&next), (11, 2, 1, 30, -5));
        // cron skips a non-existent time and does not repeat an ambiguous one
        let hourly = Schedule::Cron("30 * * * *".into());
        let n = hourly.next_after_in(&et(2026, 3, 8, 1, 30)).unwrap();
        assert_eq!(local_fields(&n), (3, 8, 3, 30, -4));
        let n = hourly.next_after_in(&first).unwrap();
        assert!(n > first);
        let cron_daily = Schedule::Cron("30 1 * * *".into());
        let a = cron_daily.next_after_in(&et(2026, 10, 31, 12, 0)).unwrap();
        let b = cron_daily.next_after_in(&a).unwrap();
        assert_eq!(local_fields(&a), (11, 1, 1, 30, -4));
        assert_eq!(local_fields(&b), (11, 2, 1, 30, -5));
        // started during the repeated hour: the second 01:30 is still ahead
        let second_pass = Eastern.from_local_datetime(&NaiveDate::from_ymd_opt(2026, 11, 1).unwrap().and_hms_opt(1, 15, 0).unwrap()).latest().unwrap();
        let n = cron_daily.next_after_in(&second_pass).unwrap();
        assert!(n > second_pass);
        assert_eq!(local_fields(&n), (11, 1, 1, 30, -5));
    }

    #[test]
    fn descriptions() {
        assert_eq!(Schedule::Interval { every_secs: 300 }.describe(), "Every 5 minutes");
        assert_eq!(Schedule::Interval { every_secs: 3600 }.describe(), "Every hour");
        assert_eq!(Schedule::Interval { every_secs: 172_800 }.describe(), "Every 2 days");
        assert_eq!(Schedule::Interval { every_secs: 45 }.describe(), "Every 45 seconds");
        assert_eq!(Schedule::Daily { at: "7:05".into(), weekdays: vec![] }.describe(), "Every day at 07:05");
        assert_eq!(Schedule::Daily { at: "09:00".into(), weekdays: vec![1, 2, 3, 4, 5] }.describe(), "Weekdays at 09:00");
        assert_eq!(Schedule::Daily { at: "10:00".into(), weekdays: vec![6, 0] }.describe(), "Weekends at 10:00");
        assert_eq!(Schedule::Daily { at: "10:00".into(), weekdays: vec![3, 1] }.describe(), "Every Mon, Wed at 10:00");
        assert_eq!(Schedule::Cron("30 9 * * 1-5".into()).describe(), "Weekdays at 09:30");
        assert_eq!(Schedule::Cron("0 2 * * *".into()).describe(), "Every day at 02:00");
        assert_eq!(Schedule::Cron("*/10 * * * *".into()).describe(), "Every 10 minutes");
        assert_eq!(Schedule::Cron("15 * * * *".into()).describe(), "Every hour at minute 15");
        assert_eq!(Schedule::Cron("0 */6 * * *".into()).describe(), "Every 6 hours");
        assert_eq!(Schedule::Cron("0 0 1 * *".into()).describe(), "Cron schedule: 0 0 1 * *");
        assert_eq!(Schedule::Cron("0 8 * * sat,sun".into()).describe(), "Every Sun, Sat at 08:00");
    }

    #[test]
    fn serde_shape() {
        let s = Schedule::Daily { at: "09:00".into(), weekdays: vec![1] };
        assert_eq!(serde_json::to_string(&s).unwrap(), r#"{"daily":{"at":"09:00","weekdays":[1]}}"#);
        let c: Schedule = serde_json::from_str(r#"{"cron":"0 * * * *"}"#).unwrap();
        assert_eq!(c, Schedule::Cron("0 * * * *".into()));
        let i: Schedule = serde_json::from_str(r#"{"interval":{"every_secs":60}}"#).unwrap();
        assert_eq!(i, Schedule::Interval { every_secs: 60 });
    }
}
