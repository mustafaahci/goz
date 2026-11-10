//! `dm:` values: a date as a query writes it, resolved to the FILETIME
//! bounds the engine compares an entry's date modified against, as
//! Everything 1.5 reads them (DESIGN.md §2.5 of the query-language work,
//! checked against a live Everything: batteries L1, L2, L7 and L8).
//!
//! A date names a period of the local calendar or wall clock: a year
//! (`2024`, or `24` with years 50 to 99 in the 1900s), a month (`2024-05`,
//! `05/2024`, `may2024`, `may`, the most recent May), a day (`2024-05-01`,
//! `20240501`, `240501`, `5/1/2024` in the order of the regional format,
//! `today`, `wed`, the most recent Wednesday, today included), an hour, a
//! minute, a second or a fraction of one (`2024-05-01T10`,
//! `20240501T1030`, `2024-05-01T10:30:15.5`), or a FILETIME, which is one
//! tick. The calendar words name whole periods: `thisweek` (`currentweek`)
//! and `thisday`, month and year; `prevweek` (`previousweek`) the week
//! before, `prev2weeks` the two before; `nextweek` (`comingweek`) the week
//! after, `next2weeks` the two after; `mtd`, `qtd` and `ytd` from the start
//! of the month, quarter or year to the start of today. A day that holds a
//! daylight-saving change lasts 23 or 25 hours.
//!
//! An age reaches back from the query start: `7days` (`last7days`,
//! `past7days`, `lastweek`, `7d`, `1w`) is the instant seven days ago, and
//! alone matches from there on, the future included, as Everything's does.
//! Seconds to weeks have a fixed length; months and years step back on the
//! local calendar. `coming2weeks` reaches forward from now. An age compares
//! as an age, as in Everything: `>7days` is older than seven days, `<7days`
//! newer.
//!
//! A comparison compares at the value's granularity: `>2024` is after the
//! end of 2024, `<=today` up to the end of today. A range `lower..upper` (or
//! `lower-upper`, when the text is no date itself) runs from the start of
//! `lower` to the end of `upper`, either side optional, and a range written
//! backwards is read the right way round. An empty value is any known date.
//! An entry whose date is unknown matches no date, and `unknown` matches
//! only such entries. A list (`2020;2024`), a value group (`<2020|2024>`)
//! and a `!` are the parser's.
//!
//! The calendar comes from a [`LocalClock`]. gozd parses the query, so the
//! clock is the daemon's; a Windows time zone belongs to the machine, so it is
//! every user's too, and the regional format the service runs under is the
//! system account's.

use super::parse::QueryError;
use crate::output::filetime::{DAYS_1601_TO_1970, civil_from_days, days_from_civil};

/// FILETIME ticks in a second.
const SECOND: i64 = 10_000_000;
const MINUTE: i64 = 60 * SECOND;
const HOUR: i64 = 60 * MINUTE;
/// FILETIME ticks in a day of the wall clock.
const DAY: i64 = 24 * HOUR;

/// The clock and calendar a `dm:` value resolves against, read when the query
/// is parsed, so `today` is the day the query runs.
///
/// Times are FILETIME ticks, 100 ns units since 1601-01-01. A local time
/// counts the same ticks on the local wall clock, as `FileTimeToLocalFileTime`
/// does.
pub trait LocalClock {
    /// The current time, UTC.
    fn now(&self) -> i64;
    /// The wall-clock time at UTC time `utc`, or `None` where the time zone
    /// cannot say.
    fn to_local(&self, utc: i64) -> Option<i64>;
    /// The UTC time at which the wall clock shows `local`, or `None` where
    /// the time zone cannot say. A time that a daylight-saving change skips or
    /// repeats maps to an instant within the change.
    fn to_utc(&self, local: i64) -> Option<i64>;
    /// The first day of the week, 0 for Monday through 6 for Sunday.
    fn week_start(&self) -> u8;
    /// The order of a date written with `/` or `.` (`11/4/2026`).
    fn date_order(&self) -> DateOrder;
}

/// The order of the day, month and year in a short date of the regional
/// format, which decides how `11/4/2026` reads, as in Everything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DateOrder {
    /// `11/4/2026` is November 4th (en-US).
    MonthDayYear,
    /// `4/11/2026` is November 4th.
    DayMonthYear,
    /// `2026/11/4` is November 4th.
    YearMonthDay,
}

/// What a query's `dm:` terms accept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DateFilter {
    /// A known date in `min..=max`, FILETIME ticks (UTC).
    Within { min: i64, max: i64 },
    /// An unknown date: `dm:unknown`.
    Unknown,
}

impl DateFilter {
    /// Whether an entry dated `mtime`, `None` when unknown, passes.
    pub fn matches(self, mtime: Option<i64>) -> bool {
        match (self, mtime) {
            (DateFilter::Within { min, max }, Some(t)) => min <= t && t <= max,
            (DateFilter::Unknown, None) => true,
            _ => false,
        }
    }

    /// The filter of two `dm:` terms in one query, which must both pass.
    pub(crate) fn and(self, other: DateFilter) -> DateFilter {
        match (self, other) {
            (DateFilter::Within { min: a, max: b }, DateFilter::Within { min: c, max: d }) => {
                DateFilter::Within {
                    min: a.max(c),
                    max: b.min(d),
                }
            }
            (DateFilter::Unknown, DateFilter::Unknown) => DateFilter::Unknown,
            // No date is both known and unknown.
            _ => DateFilter::Within {
                min: i64::MAX,
                max: i64::MIN,
            },
        }
    }
}

/// What one date value names.
#[derive(Clone, Copy)]
enum Value {
    /// The ticks from `start` up to, not including, `end`.
    Period { start: i64, end: i64 },
    /// An age: the instant that long before the query start.
    Age(i64),
}

impl Value {
    fn first(self) -> i64 {
        match self {
            Value::Period { start, .. } => start,
            Value::Age(t) => t,
        }
    }

    fn last(self) -> i64 {
        match self {
            Value::Period { end, .. } => end - 1,
            Value::Age(t) => t,
        }
    }
}

fn malformed(detail: String) -> QueryError {
    QueryError::Malformed {
        what: "dm:",
        detail,
    }
}

fn within(min: i64, max: i64) -> DateFilter {
    DateFilter::Within { min, max }
}

/// The filter a `dm:` value stands for, read against `clock`.
pub(crate) fn date_filter(value: &str, clock: &dyn LocalClock) -> Result<DateFilter, QueryError> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("unknown") {
        return Ok(DateFilter::Unknown);
    }
    // An empty value is any known date, as in Everything (L1:366).
    if value.is_empty() {
        return Ok(within(i64::MIN, i64::MAX));
    }
    if let Some((lower, upper)) = value.split_once("..") {
        let side = |text: &str| {
            let text = text.trim();
            (!text.is_empty()).then(|| point(text, clock)).transpose()
        };
        return Ok(range(side(lower)?, side(upper)?));
    }

    // Two-character operators before their one-character prefixes.
    for op in [">=", "<=", "==", ">", "<", "="] {
        if let Some(rest) = value.strip_prefix(op) {
            return Ok(compare(op, point(rest, clock)?));
        }
    }
    match point(value, clock) {
        Ok(v) => Ok(alone(v)),
        // `2025-2026` is no date, but a range of two.
        Err(e) => dash_range(value, clock).unwrap_or(Err(e)),
    }
}

/// The filter of `lower..upper`, either side optional: from the start of
/// `lower` to the end of `upper`, the two swapped when `upper` comes first,
/// as Everything reads `2026-11-08..2026-11-04` (L2:62) and `1d..3d`
/// (L7:600).
fn range(lower: Option<Value>, upper: Option<Value>) -> DateFilter {
    match (lower, upper) {
        (Some(a), Some(b)) if a.first() > b.last() => within(b.first(), a.last()),
        (a, b) => within(
            a.map_or(i64::MIN, Value::first),
            b.map_or(i64::MAX, Value::last),
        ),
    }
}

/// `value` as a range `lower-upper`, when it splits at a `-` into two values
/// written alike (`2025-2026`, `2025-12-15-2026-01-20`, L7:602-603): `None`
/// when no split does. Alike means the same runs of digits, letters and
/// other characters, so that a mistyped date such as `2026-02-30` is
/// refused rather than read as `2026-02..2030`.
fn dash_range(value: &str, clock: &dyn LocalClock) -> Option<Result<DateFilter, QueryError>> {
    let shape = |text: &str| {
        let mut out = Vec::new();
        for c in text.chars() {
            let class = if c.is_ascii_digit() {
                '9'
            } else if c.is_alphabetic() {
                'a'
            } else {
                c
            };
            if out.last() != Some(&class) || !matches!(class, '9' | 'a') {
                out.push(class);
            }
        }
        out
    };
    value.match_indices('-').find_map(|(at, _)| {
        let (lower, upper) = (&value[..at], &value[at + 1..]);
        if lower.is_empty() || upper.is_empty() || shape(lower) != shape(upper) {
            return None;
        }
        let (Ok(a), Ok(b)) = (point(lower, clock), point(upper, clock)) else {
            return None;
        };
        Some(Ok(range(Some(a), Some(b))))
    })
}

/// The filter of a comparison. A period compares at its granularity; an age
/// compares as an age, as in Everything (L8:7-14): `>2days` is older than
/// two days, `<2days` newer, `=2days` the instant itself.
fn compare(op: &str, v: Value) -> DateFilter {
    match (v, op) {
        (Value::Period { start, .. }, ">=") => within(start, i64::MAX),
        (Value::Period { end, .. }, ">") => within(end, i64::MAX),
        (Value::Period { end, .. }, "<=") => within(i64::MIN, end - 1),
        (Value::Period { start, .. }, "<") => within(i64::MIN, start.saturating_sub(1)),
        (Value::Age(t), ">=") => within(i64::MIN, t),
        (Value::Age(t), ">") => within(i64::MIN, t.saturating_sub(1)),
        (Value::Age(t), "<=") => within(t, i64::MAX),
        (Value::Age(t), "<") => within(t.saturating_add(1), i64::MAX),
        // `=` and `==`: the value itself.
        (Value::Period { start, end }, _) => within(start, end - 1),
        (Value::Age(t), _) => within(t, t),
    }
}

/// The filter of a value with no operator: a period's own ticks, or from an
/// age's instant on, so a file dated in the future by a skewed clock or an
/// unpacked archive still counts as recent.
fn alone(v: Value) -> DateFilter {
    match v {
        Value::Period { start, end } => within(start, end - 1),
        Value::Age(t) => within(t, i64::MAX),
    }
}

/// A unit of a calendar word or an age.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Unit {
    Second,
    Minute,
    Hour,
    Day,
    Week,
    Month,
    Year,
}

impl Unit {
    /// The unit a calendar word names after `this`, `prev` or `next`, with
    /// or without a count: the word, singular or plural.
    fn calendar(word: &str) -> Option<Unit> {
        Some(match word {
            "day" | "days" => Unit::Day,
            "week" | "weeks" => Unit::Week,
            "month" | "months" => Unit::Month,
            "year" | "years" => Unit::Year,
            _ => return None,
        })
    }

    /// The unit after a count (`7days`, `2h`): the abbreviations and plurals
    /// Everything reads (`2hrs` and `2m`, minutes, but not `2hr`, `2wk`,
    /// `2yr` or `2mos`, L7:589-599, L8:33-44).
    fn counted(word: &str) -> Option<Unit> {
        Some(match word {
            "s" | "sec" | "secs" | "second" | "seconds" => Unit::Second,
            "m" | "min" | "mins" | "minute" | "minutes" => Unit::Minute,
            "h" | "hrs" | "hour" | "hours" => Unit::Hour,
            "d" | "day" | "days" => Unit::Day,
            "w" | "week" | "weeks" => Unit::Week,
            "mo" | "month" | "months" => Unit::Month,
            "y" | "year" | "years" => Unit::Year,
            _ => return None,
        })
    }

    /// The unit of an age with no count (`lastweek`, `pasthour`).
    fn single(word: &str) -> Option<Unit> {
        Some(match word {
            "second" => Unit::Second,
            "minute" => Unit::Minute,
            "hour" => Unit::Hour,
            "day" => Unit::Day,
            "week" => Unit::Week,
            "month" => Unit::Month,
            "year" => Unit::Year,
            _ => return None,
        })
    }

    /// The fixed length of the unit, or `None` for months and years, which
    /// step on the calendar.
    fn ticks(self) -> Option<i64> {
        match self {
            Unit::Second => Some(SECOND),
            Unit::Minute => Some(MINUTE),
            Unit::Hour => Some(HOUR),
            Unit::Day => Some(DAY),
            Unit::Week => Some(7 * DAY),
            Unit::Month | Unit::Year => None,
        }
    }
}

/// The local calendar at the query start: today, counted in days from
/// 1601-01-01 on the wall clock, and the time of day into it.
struct Today {
    day: i64,
    time: i64,
}

impl Today {
    fn read(clock: &dyn LocalClock) -> Option<Today> {
        let local = clock.to_local(clock.now())?;
        Some(Today {
            day: local.div_euclid(DAY),
            time: local.rem_euclid(DAY),
        })
    }
}

/// The period or age one value names.
fn point(text: &str, clock: &dyn LocalClock) -> Result<Value, QueryError> {
    let text = text.trim();
    let lower = text.to_ascii_lowercase();
    let out_of_range = || malformed(format!("{text} is outside the dates goz can compare"));
    if let Some(t) = filetime(&lower) {
        return Ok(Value::Period {
            start: t,
            end: t.checked_add(1).ok_or_else(out_of_range)?,
        });
    }
    if lower == "now" {
        // The current second.
        let start = clock
            .now()
            .div_euclid(SECOND)
            .checked_mul(SECOND)
            .ok_or_else(out_of_range)?;
        return Ok(Value::Period {
            start,
            end: start.checked_add(SECOND).ok_or_else(out_of_range)?,
        });
    }
    let today = || Today::read(clock).ok_or_else(out_of_range);
    let local = match relative(&lower, text, clock)? {
        Some(Relative::Instant(v)) => return Ok(v),
        Some(Relative::Days(first, end)) => Local::days(first, end),
        None => match named(&lower, &today)? {
            Some(local) => local,
            None => match written(&lower, clock.date_order(), &today)? {
                Some(local) => local,
                None => {
                    return Err(malformed(format!(
                        "{text} is not a date; use today, yesterday, thisweek, prevmonth, \
                         nextyear, a count such as 7days, 3h or last2weeks, a month or a day \
                         name such as may or wed, YYYY, YYYY-MM, YYYY-MM-DD[Thh[:mm[:ss]]], \
                         YYYYMMDD, M/D/YYYY in the regional order, mtd, ytd, a FILETIME, or \
                         unknown"
                    )));
                }
            },
        },
    };
    let utc = |t: i64| {
        Some(t)
            .filter(|&t| t >= 0)
            .and_then(|t| clock.to_utc(t))
            .ok_or_else(out_of_range)
    };
    Ok(Value::Period {
        start: utc(local.start)?,
        end: utc(local.end)?,
    })
}

/// A period of local ticks, `[start, end)`.
struct Local {
    start: i64,
    end: i64,
}

impl Local {
    fn days(first: i64, end: i64) -> Local {
        Local {
            start: first.saturating_mul(DAY),
            end: end.saturating_mul(DAY),
        }
    }
}

/// A FILETIME, nine digits or more, or hex after `0x`, as Everything reads
/// one (L7:577-580).
fn filetime(lower: &str) -> Option<i64> {
    if let Some(hex) = lower.strip_prefix("0x") {
        return (!hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| i64::from_str_radix(hex, 16).ok())
            .flatten();
    }
    (lower.len() >= 9 && lower.bytes().all(|b| b.is_ascii_digit()))
        .then(|| lower.parse::<i64>().ok())
        .flatten()
}

/// What a relative value names.
enum Relative {
    /// An age, or a stretch from now: resolved already.
    Instant(Value),
    /// Local days `[first, end)`.
    Days(i64, i64),
}

/// The value of a calendar word or an age (`today`, `thisweek`,
/// `prev2months`, `next3days`, `coming2weeks`, `7days`, `lasthour`, `mtd`):
/// `None` when `lower` is none, an error when its count is not above 0 or it
/// reaches past what goz can compare.
fn relative(
    lower: &str,
    text: &str,
    clock: &dyn LocalClock,
) -> Result<Option<Relative>, QueryError> {
    let out_of_range = || malformed(format!("{text} is outside the dates goz can compare"));
    let today = || Today::read(clock).ok_or_else(out_of_range);
    let days = |offset: i64| -> Result<Option<Relative>, QueryError> {
        let day = today()?.day;
        Ok(Some(Relative::Days(day + offset, day + offset + 1)))
    };
    match lower {
        "today" => return days(0),
        "yesterday" => return days(-1),
        "tomorrow" => return days(1),
        "mtd" | "qtd" | "ytd" => {
            // From the start of the month, quarter or year to the start of
            // today, as Everything (L2:57-59).
            let day = today()?.day;
            let (year, month, _) = civil(day);
            let first_month = match lower {
                "mtd" => month,
                "qtd" => (month - 1) / 3 * 3 + 1,
                _ => 1,
            };
            let first = local_day(year, first_month, 1).ok_or_else(out_of_range)?;
            return Ok(Some(Relative::Days(first, day)));
        }
        _ => {}
    }
    let (prefix, rest) = [
        "current", "previous", "coming", "this", "last", "past", "prev", "next",
    ]
    .iter()
    .find_map(|p| Some((*p, lower.strip_prefix(p)?)))
    .unwrap_or(("", lower));
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let (count, word) = rest.split_at(digits);
    // Whether the word is a unit this prefix takes, settled before the
    // count is read, so that a number such as `20261104` is left to the
    // written dates.
    let takes = match (prefix, count.is_empty()) {
        ("this" | "current" | "coming", true) | ("prev" | "previous" | "next", _) => {
            Unit::calendar(word).is_some()
        }
        ("coming" | "last" | "past" | "", false) => Unit::counted(word).is_some(),
        ("last" | "past", true) => Unit::single(word).is_some(),
        _ => false,
    };
    if !takes {
        return Ok(None);
    }
    let count = if count.is_empty() {
        None
    } else {
        let n = count
            .parse::<i64>()
            .ok()
            .filter(|&n| n <= 1_000_000)
            .ok_or_else(|| malformed(format!("{text} reaches further than goz can compare")))?;
        if n == 0 {
            return Err(malformed(format!("{text}: give a count above 0")));
        }
        Some(n)
    };
    let calendar = |from: i64, n: i64| -> Result<Option<Relative>, QueryError> {
        let Some(unit) = Unit::calendar(word) else {
            return Ok(None);
        };
        let (first, end) = calendar_period(unit, from, n, today()?.day, clock.week_start())
            .ok_or_else(out_of_range)?;
        Ok(Some(Relative::Days(first, end)))
    };
    match (prefix, count) {
        ("this" | "current", None) => calendar(0, 1),
        ("prev" | "previous", n) => {
            let n = n.unwrap_or(1);
            calendar(-n, n)
        }
        ("next", n) => calendar(1, n.unwrap_or(1)),
        ("coming", None) => calendar(1, 1),
        // A count after `coming` reaches forward from now (L8:20-31).
        ("coming", Some(n)) => match Unit::counted(word) {
            Some(unit) => step(clock, unit, n).map(|end| {
                Some(Relative::Instant(Value::Period {
                    start: clock.now(),
                    end,
                }))
            }),
            None => Ok(None),
        },
        ("last" | "past", None) => match Unit::single(word) {
            Some(unit) => step(clock, unit, -1).map(|t| Some(Relative::Instant(Value::Age(t)))),
            None => Ok(None),
        },
        ("last" | "past" | "", Some(n)) => match Unit::counted(word) {
            Some(unit) => step(clock, unit, -n).map(|t| Some(Relative::Instant(Value::Age(t)))),
            None => Ok(None),
        },
        _ => Ok(None),
    }
}

/// The instant `n` units from the query start, back for a negative `n`.
/// Seconds to weeks have a fixed length; months and years step on the local
/// calendar, keeping the time of day and landing on the month's last day
/// when it is shorter.
fn step(clock: &dyn LocalClock, unit: Unit, n: i64) -> Result<i64, QueryError> {
    let too_far = || malformed("the count reaches further than goz can compare".to_string());
    let now = clock.now();
    if let Some(ticks) = unit.ticks() {
        return n
            .checked_mul(ticks)
            .and_then(|d| now.checked_add(d))
            .ok_or_else(too_far);
    }
    let months = if unit == Unit::Year {
        n.checked_mul(12).ok_or_else(too_far)?
    } else {
        n
    };
    let today = Today::read(clock).ok_or_else(too_far)?;
    let (year, month, day) = civil(today.day);
    let (year, month) = add_months(year, month, months).ok_or_else(too_far)?;
    local_day(year, month, day.min(days_in_month(year, month)))
        .and_then(|d| d.checked_mul(DAY))
        .and_then(|t| t.checked_add(today.time))
        .and_then(|t| clock.to_utc(t))
        .ok_or_else(too_far)
}

/// The local days `[first, end)` of `n` weeks, months, years or days
/// starting `from` of them after the current one (`this` 0 and 1, `prev2`
/// -2 and 2, `next` 1 and 1), around local day `today`.
fn calendar_period(
    unit: Unit,
    from: i64,
    n: i64,
    today: i64,
    week_start: u8,
) -> Option<(i64, i64)> {
    match unit {
        Unit::Day => Some((today + from, today + from + n)),
        Unit::Week => {
            // 1601-01-01 was a Monday, so day 0 is one.
            let into_week = (today - i64::from(week_start)).rem_euclid(7);
            let first = today - into_week + 7 * from;
            Some((first, first + 7 * n))
        }
        Unit::Month => {
            let (year, month, _) = civil(today);
            let (y0, m0) = add_months(year, month, from)?;
            let (y1, m1) = add_months(y0, m0, n)?;
            Some((local_day(y0, m0, 1)?, local_day(y1, m1, 1)?))
        }
        Unit::Year => {
            let year = civil(today).0.checked_add(from)?;
            Some((
                local_day(year, 1, 1)?,
                local_day(year.checked_add(n)?, 1, 1)?,
            ))
        }
        _ => None,
    }
}

const MONTHS: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];

/// The month a name or its three-letter form names, 1 to 12.
fn month_named(word: &str) -> Option<u32> {
    MONTHS
        .iter()
        .position(|m| word == *m || (word.len() == 3 && m.starts_with(word)))
        .map(|i| i as u32 + 1)
}

/// The weekday a name names, 0 for Monday through 6 for Sunday: the docs'
/// three-letter forms and full names, and the `tues`, `thur` and `thurs`
/// Everything reads too (L8:59-61). `mon` is Monday, as the docs list it,
/// though a live Everything 1.5 reads it as one month (L2:12).
fn weekday_named(word: &str) -> Option<i64> {
    Some(match word {
        "mon" | "monday" => 0,
        "tue" | "tues" | "tuesday" => 1,
        "wed" | "wednesday" => 2,
        "thu" | "thur" | "thurs" | "thursday" => 3,
        "fri" | "friday" => 4,
        "sat" | "saturday" => 5,
        "sun" | "sunday" => 6,
        _ => return None,
    })
}

/// The period a month or weekday name names: the most recent such month
/// or day that has begun, today included (`nov` is this month, `tue` today
/// on a Tuesday, L2:5-17), or a month of a year (`may2025`, `dec-2025`,
/// `2026nov`, L7:604-607, L8:70-72).
fn named(
    lower: &str,
    today: &dyn Fn() -> Result<Today, QueryError>,
) -> Result<Option<Local>, QueryError> {
    if let Some(weekday) = weekday_named(lower) {
        let day = today()?.day;
        // Day 0, 1601-01-01, was a Monday.
        let back = (day.rem_euclid(7) - weekday).rem_euclid(7);
        return Ok(Some(Local::days(day - back, day - back + 1)));
    }
    let letters = lower.bytes().take_while(u8::is_ascii_alphabetic).count();
    let (word, year) = if letters > 0 {
        let (word, rest) = lower.split_at(letters);
        (word, rest.strip_prefix('-').unwrap_or(rest))
    } else {
        let digits = lower.bytes().take_while(u8::is_ascii_digit).count();
        let (year, word) = lower.split_at(digits);
        (word, year)
    };
    let Some(month) = month_named(word) else {
        return Ok(None);
    };
    let year = match year {
        "" => {
            let (year, current, _) = civil(today()?.day);
            if month <= current { year } else { year - 1 }
        }
        y if y.len() == 4 && y.bytes().all(|b| b.is_ascii_digit()) => {
            y.parse::<i64>().unwrap_or_default()
        }
        _ => return Ok(None),
    };
    let (next_year, next_month) = add_months(year, month, 1).unwrap_or((year + 1, 1));
    Ok(Some(Local::days(
        local_day(year, month, 1).unwrap_or_default(),
        local_day(next_year, next_month, 1).unwrap_or_default(),
    )))
}

/// A date as written down to some unit: the year, and as many of the month,
/// day, hour, minute, second and fraction of a second as it gives.
#[derive(Default)]
struct Stamp {
    year: i64,
    month: Option<u32>,
    day: Option<u32>,
    hour: Option<u32>,
    minute: Option<u32>,
    second: Option<u32>,
    /// The fraction of a second, in ticks, and the ticks its last digit is
    /// worth.
    fraction: Option<(i64, i64)>,
}

impl Stamp {
    /// The local ticks of the period the stamp names, or `None` when it
    /// names no real date or time.
    fn period(&self) -> Option<Local> {
        let month = self.month.unwrap_or(1);
        let day = self.day.unwrap_or(1);
        if !(1..=12).contains(&month)
            || day == 0
            || day > days_in_month(self.year, month)
            || self.hour.is_some_and(|h| h > 23)
            || self.minute.is_some_and(|m| m > 59)
            || self.second.is_some_and(|s| s > 59)
        {
            return None;
        }
        let midnight = local_day(self.year, month, day)?.checked_mul(DAY)?;
        let start = midnight
            + i64::from(self.hour.unwrap_or(0)) * HOUR
            + i64::from(self.minute.unwrap_or(0)) * MINUTE
            + i64::from(self.second.unwrap_or(0)) * SECOND
            + self.fraction.map_or(0, |(f, _)| f);
        let end = if let Some((_, unit)) = self.fraction {
            start + unit
        } else if self.second.is_some() {
            start + SECOND
        } else if self.minute.is_some() {
            start + MINUTE
        } else if self.hour.is_some() {
            start + HOUR
        } else if self.day.is_some() {
            start + DAY
        } else if self.month.is_some() {
            let (y, m) = add_months(self.year, month, 1)?;
            local_day(y, m, 1)?.checked_mul(DAY)?
        } else {
            local_day(self.year + 1, 1, 1)?.checked_mul(DAY)?
        };
        Some(Local { start, end })
    }
}

/// A number of 1 to `max_len` ASCII digits.
fn number(text: &str, max_len: usize) -> Option<u32> {
    (!text.is_empty() && text.len() <= max_len && text.bytes().all(|b| b.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
}

/// A year of four digits, or of two, read as Everything reads them: 00 to
/// 49 in the 2000s, 50 to 99 in the 1900s (L2:1-4).
fn year(text: &str) -> Option<i64> {
    let y = i64::from(number(text, 4)?);
    match text.len() {
        4 => Some(y),
        2 => Some(if y < 50 { 2000 + y } else { 1900 + y }),
        _ => None,
    }
}

/// A fraction of a second, `.` and 1 to 7 digits: its ticks and what its
/// last digit is worth.
fn fraction(text: &str) -> Option<(i64, i64)> {
    let digits = text.strip_prefix('.')?;
    let n = i64::from(number(digits, 7)?);
    let unit = 10i64.pow(7 - digits.len() as u32);
    Some((n * unit, unit))
}

/// The period a written date names, in any of Everything's shapes, or
/// `None` when `lower` has none of them, or an error when it has one but
/// names no real date or time.
fn written(
    lower: &str,
    order: DateOrder,
    today: &dyn Fn() -> Result<Today, QueryError>,
) -> Result<Option<Local>, QueryError> {
    let stamp = if lower.contains(['/', '.']) && !lower.contains(['-', 't', ':']) {
        slashed(lower, order, today)?
    } else if lower.bytes().next().is_some_and(|b| b.is_ascii_digit()) && lower.contains('-') {
        extended(lower)
    } else {
        basic(lower)
    };
    let Some(stamp) = stamp else {
        return Ok(None);
    };
    stamp
        .period()
        .map(Some)
        .ok_or_else(|| malformed(format!("{lower} is not a real date")))
}

/// ISO 8601 extended: `YYYY[-M[M][-D[D][Th[h][:m[m][:s[s][.fff]]]]]]`,
/// months and days written with one digit too (L1:367, L7:547-548).
fn extended(lower: &str) -> Option<Stamp> {
    let (date, time) = match lower.split_once('t') {
        Some((date, time)) => (date, Some(time)),
        None => (lower, None),
    };
    let mut parts = date.split('-');
    let mut stamp = Stamp {
        year: i64::from(number(parts.next()?, 4).filter(|_| date.find('-') == Some(4))?),
        ..Stamp::default()
    };
    stamp.month = parts
        .next()
        .map(|m| number(m, 2))
        .map_or(Some(None), |m| m.map(Some))?;
    stamp.day = parts
        .next()
        .map(|d| number(d, 2))
        .map_or(Some(None), |d| d.map(Some))?;
    if parts.next().is_some() || (time.is_some() && stamp.day.is_none()) {
        return None;
    }
    if let Some(time) = time {
        let (clock, frac) = match time.find('.') {
            Some(dot) => (&time[..dot], Some(fraction(&time[dot..])?)),
            None => (time, None),
        };
        let mut fields = clock.split(':');
        stamp.hour = Some(number(fields.next()?, 2)?);
        stamp.minute = fields
            .next()
            .map(|m| number(m, 2))
            .map_or(Some(None), |m| m.map(Some))?;
        stamp.second = fields
            .next()
            .map(|s| number(s, 2))
            .map_or(Some(None), |s| s.map(Some))?;
        if fields.next().is_some() || (frac.is_some() && stamp.second.is_none()) {
            return None;
        }
        stamp.fraction = frac;
    }
    Some(stamp)
}

/// ISO 8601 basic and the bare years: `YYYYMMDD[Thh[mm[ss[.fff]]]]`,
/// `YYMMDD`, `YYYYMM`, `YYYY`, `YY`. Six digits read as `YYMMDD` when they
/// make a day (`261104`, L2:72) and as `YYYYMM` otherwise (`202611`, L2:71).
fn basic(lower: &str) -> Option<Stamp> {
    let (date, time) = match lower.split_once('t') {
        Some((date, time)) => (date, Some(time)),
        None => (lower, None),
    };
    if !date.bytes().all(|b| b.is_ascii_digit()) || date.is_empty() {
        return None;
    }
    let n = |range: std::ops::Range<usize>| number(&date[range], 4);
    let mut stamp = match date.len() {
        8 => Stamp {
            year: i64::from(n(0..4)?),
            month: Some(n(4..6)?),
            day: Some(n(6..8)?),
            ..Stamp::default()
        },
        6 if time.is_none() => {
            let yymmdd = Stamp {
                year: year(&date[0..2])?,
                month: Some(n(2..4)?),
                day: Some(n(4..6)?),
                ..Stamp::default()
            };
            if yymmdd.period().is_some() {
                yymmdd
            } else {
                Stamp {
                    year: i64::from(n(0..4)?),
                    month: Some(n(4..6)?),
                    ..Stamp::default()
                }
            }
        }
        4 | 2 if time.is_none() => Stamp {
            year: year(date)?,
            ..Stamp::default()
        },
        _ => return None,
    };
    if let Some(time) = time {
        let (clock, frac) = match time.find('.') {
            Some(dot) => (&time[..dot], Some(fraction(&time[dot..])?)),
            None => (time, None),
        };
        let pair = |i: usize| clock.get(i..i + 2).and_then(|p| number(p, 2));
        match clock.len() {
            2 => stamp.hour = Some(pair(0)?),
            4 => (stamp.hour, stamp.minute) = (Some(pair(0)?), Some(pair(2)?)),
            6 => {
                (stamp.hour, stamp.minute, stamp.second) =
                    (Some(pair(0)?), Some(pair(2)?), Some(pair(4)?));
            }
            _ => return None,
        }
        if frac.is_some() && stamp.second.is_none() {
            return None;
        }
        stamp.fraction = frac;
    }
    Some(stamp)
}

/// A date written with `/` or `.`: a part of four digits is the year
/// (`2026/11/4`, `2026.11.04`, `11/2026`, `2026/11`); otherwise the regional
/// order decides (`11/4/2026` and `11/4/26` are November 4th in en-US, and
/// `11/4` the 4th of this year's November, L7:542-545, L8:63-66).
fn slashed(
    lower: &str,
    order: DateOrder,
    today: &dyn Fn() -> Result<Today, QueryError>,
) -> Result<Option<Stamp>, QueryError> {
    let sep = if lower.contains('/') { '/' } else { '.' };
    if lower.contains('/') && lower.contains('.') {
        return Ok(None);
    }
    let parts: Vec<&str> = lower.split(sep).collect();
    let n = |p: &str| number(p, 2);
    let four = |p: &str| p.len() == 4;
    let stamp = |y: i64, m: Option<u32>, d: Option<u32>| Stamp {
        year: y,
        month: m,
        day: d,
        ..Stamp::default()
    };
    Ok(match parts.as_slice() {
        [y, m, d] if four(y) => n(m)
            .zip(n(d))
            .map(|(m, d)| stamp(year(y).unwrap_or(0), Some(m), Some(d))),
        [a, b, c] => {
            let (y, m, d) = match order {
                DateOrder::MonthDayYear => (c, a, b),
                DateOrder::DayMonthYear => (c, b, a),
                DateOrder::YearMonthDay => (a, b, c),
            };
            year(y)
                .zip(n(m))
                .zip(n(d))
                .map(|((y, m), d)| stamp(y, Some(m), Some(d)))
        }
        [y, m] if four(y) => year(y).zip(n(m)).map(|(y, m)| stamp(y, Some(m), None)),
        [m, y] if four(y) => year(y).zip(n(m)).map(|(y, m)| stamp(y, Some(m), None)),
        [a, b] => {
            let (m, d) = match order {
                DateOrder::DayMonthYear => (b, a),
                _ => (a, b),
            };
            match n(m).zip(n(d)) {
                Some((m, d)) => Some(stamp(civil(today()?.day).0, Some(m), Some(d))),
                None => None,
            }
        }
        _ => None,
    })
}

/// The date of local day `day`, counted from 1601-01-01.
fn civil(day: i64) -> (i64, u32, u32) {
    civil_from_days(day - DAYS_1601_TO_1970)
}

/// The local day of a date, counted from 1601-01-01; `None` far outside any
/// date a FILETIME holds.
fn local_day(year: i64, month: u32, day: u32) -> Option<i64> {
    (-1_000_000..=1_000_000)
        .contains(&year)
        .then(|| days_from_civil(year, month, day) + DAYS_1601_TO_1970)
}

/// The month `delta` months after `month` of `year`.
fn add_months(year: i64, month: u32, delta: i64) -> Option<(i64, u32)> {
    let index = year
        .checked_mul(12)?
        .checked_add(i64::from(month) - 1)?
        .checked_add(delta)?;
    Some((index.div_euclid(12), index.rem_euclid(12) as u32 + 1))
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::output::{FILETIME_UNIX_EPOCH, unix_ms_to_filetime};

    /// A clock at `now` in a zone `offset` ticks ahead of UTC, with no
    /// daylight saving.
    pub(crate) struct FixedClock {
        pub(crate) now: i64,
        pub(crate) offset: i64,
        pub(crate) week_start: u8,
        pub(crate) date_order: DateOrder,
    }

    impl FixedClock {
        /// UTC, at `now`, with weeks from Monday and dates written M/D/Y.
        pub(crate) fn utc(now: i64) -> Self {
            FixedClock {
                now,
                offset: 0,
                week_start: 0,
                date_order: DateOrder::MonthDayYear,
            }
        }

        /// The clock of the live Everything the date answers came from:
        /// UTC+3, weeks from Sunday, dates written M/D/Y (en-US), at `now`.
        pub(crate) fn istanbul_en_us(now: i64) -> Self {
            FixedClock {
                now,
                offset: 3 * HOUR,
                week_start: 6,
                date_order: DateOrder::MonthDayYear,
            }
        }
    }

    impl LocalClock for FixedClock {
        fn now(&self) -> i64 {
            self.now
        }
        fn to_local(&self, utc: i64) -> Option<i64> {
            utc.checked_add(self.offset)
        }
        fn to_utc(&self, local: i64) -> Option<i64> {
            local.checked_sub(self.offset)
        }
        fn week_start(&self) -> u8 {
            self.week_start
        }
        fn date_order(&self) -> DateOrder {
            self.date_order
        }
    }

    /// US Eastern time in 2026: daylight saving from 2026-03-08T07:00Z (2:00
    /// standard) to 2026-11-01T06:00Z (2:00 daylight), as Windows applies it.
    struct Eastern {
        now: i64,
    }

    impl Eastern {
        fn offset_at(utc: i64) -> i64 {
            if (ft("2026-03-08T07:00:00")..ft("2026-11-01T06:00:00")).contains(&utc) {
                -4 * HOUR
            } else {
                -5 * HOUR
            }
        }
    }

    impl LocalClock for Eastern {
        fn now(&self) -> i64 {
            self.now
        }
        fn to_local(&self, utc: i64) -> Option<i64> {
            Some(utc + Eastern::offset_at(utc))
        }
        fn to_utc(&self, local: i64) -> Option<i64> {
            // Standard time if it reads back, else daylight time.
            let standard = local + 5 * HOUR;
            Some(if Eastern::offset_at(standard) == -5 * HOUR {
                standard
            } else {
                local + 4 * HOUR
            })
        }
        fn week_start(&self) -> u8 {
            0
        }
        fn date_order(&self) -> DateOrder {
            DateOrder::MonthDayYear
        }
    }

    /// The FILETIME of a UTC `YYYY-MM-DDTHH:MM:SS`.
    pub(crate) fn ft(text: &str) -> i64 {
        let n = |range: std::ops::Range<usize>| text[range].parse::<i64>().unwrap();
        let days = days_from_civil(n(0..4), n(5..7) as u32, n(8..10) as u32);
        let secs = days * 86_400 + n(11..13) * 3_600 + n(14..16) * 60 + n(17..19);
        unix_ms_to_filetime(secs * 1_000)
    }

    fn within(min: &str, until: &str) -> DateFilter {
        DateFilter::Within {
            min: ft(min),
            max: ft(until) - 1,
        }
    }

    fn filter(value: &str, clock: &dyn LocalClock) -> DateFilter {
        date_filter(value, clock).unwrap_or_else(|e| panic!("{value}: {e}"))
    }

    fn bounds(value: &str, clock: &dyn LocalClock) -> (i64, i64) {
        match filter(value, clock) {
            DateFilter::Within { min, max } => (min, max),
            DateFilter::Unknown => panic!("{value}"),
        }
    }

    #[test]
    fn the_test_clock_reads_filetimes() {
        assert_eq!(ft("1970-01-01T00:00:00"), FILETIME_UNIX_EPOCH);
        assert_eq!(ft("1601-01-01T00:00:00"), 0);
    }

    /// A calendar day starts at local midnight, not at UTC midnight or at the
    /// query start: half an hour after midnight in Istanbul (UTC+3), a file
    /// from one tick before that midnight is yesterday's.
    #[test]
    fn a_day_runs_from_local_midnight_to_the_next() {
        let clock = FixedClock {
            now: ft("2026-11-03T21:30:00"),
            offset: 3 * HOUR,
            week_start: 0,
            date_order: DateOrder::MonthDayYear,
        };
        let today = filter("today", &clock);
        assert_eq!(today, within("2026-11-03T21:00:00", "2026-11-04T21:00:00"));
        assert_eq!(filter("2026-11-04", &clock), today);
        assert_eq!(filter("TODAY", &clock), today);
        assert_eq!(filter("thisday", &clock), today);
        let midnight = ft("2026-11-03T21:00:00");
        assert!(today.matches(Some(midnight)));
        assert!(!today.matches(Some(midnight - 1)));
        assert!(filter("yesterday", &clock).matches(Some(midnight - 1)));
        assert!(filter("prevday", &clock).matches(Some(midnight - 1)));
    }

    /// The day of a daylight-saving change lasts 23 or 25 hours, and so do
    /// the weeks that hold one, while an age stays its fixed length.
    #[test]
    fn a_daylight_saving_change_moves_midnight_but_not_an_age() {
        let clock = Eastern {
            now: ft("2026-03-10T12:00:00"),
        };
        assert_eq!(
            filter("2026-03-08", &clock),
            within("2026-03-08T05:00:00", "2026-03-09T04:00:00")
        );
        assert_eq!(
            filter("2026-11-01", &clock),
            within("2026-11-01T04:00:00", "2026-11-02T05:00:00")
        );
        assert_eq!(
            filter("prevweek", &clock),
            within("2026-03-02T05:00:00", "2026-03-09T04:00:00")
        );
        assert_eq!(
            filter("2026-03-08T03", &clock),
            within("2026-03-08T07:00:00", "2026-03-08T08:00:00"),
            "the first hour of daylight saving"
        );
        let age = DateFilter::Within {
            min: ft("2026-03-03T12:00:00"),
            max: i64::MAX,
        };
        assert_eq!(filter("7days", &clock), age);
        assert_eq!(filter("lastweek", &clock), age, "rolling, as in 1.5");
    }

    /// A week starts on the machine's first day of the week, and the
    /// calendar words cross month and year ends. `last` reaches back from
    /// now in Everything 1.5; the calendar period before the current one is
    /// `prev` (`previous`).
    #[test]
    fn calendar_words_follow_the_week_start_and_the_calendar() {
        // A Sunday.
        let mut clock = FixedClock::utc(ft("2026-10-18T12:00:00"));
        assert_eq!(
            filter("thisweek", &clock),
            within("2026-10-12T00:00:00", "2026-10-19T00:00:00")
        );
        clock.week_start = 6;
        assert_eq!(
            filter("thisweek", &clock),
            within("2026-10-18T00:00:00", "2026-10-25T00:00:00")
        );
        assert_eq!(filter("currentweek", &clock), filter("thisweek", &clock));
        for prev in ["prevweek", "previousweek", "prev1week"] {
            assert_eq!(
                filter(prev, &clock),
                within("2026-10-11T00:00:00", "2026-10-18T00:00:00"),
                "{prev}"
            );
        }
        assert_eq!(
            filter("prev2weeks", &clock),
            within("2026-10-04T00:00:00", "2026-10-18T00:00:00")
        );
        for next in ["nextweek", "comingweek", "next1week"] {
            assert_eq!(
                filter(next, &clock),
                within("2026-10-25T00:00:00", "2026-11-01T00:00:00"),
                "{next}"
            );
        }
        assert_eq!(
            filter("next2weeks", &clock),
            within("2026-10-25T00:00:00", "2026-11-08T00:00:00")
        );
        let clock = FixedClock::utc(ft("2024-01-15T12:00:00"));
        for (value, from, until) in [
            ("prevmonth", "2023-12-01T00:00:00", "2024-01-01T00:00:00"),
            ("prev2months", "2023-11-01T00:00:00", "2024-01-01T00:00:00"),
            ("thismonth", "2024-01-01T00:00:00", "2024-02-01T00:00:00"),
            ("nextmonth", "2024-02-01T00:00:00", "2024-03-01T00:00:00"),
            ("prevyear", "2023-01-01T00:00:00", "2024-01-01T00:00:00"),
            ("prev2years", "2022-01-01T00:00:00", "2024-01-01T00:00:00"),
            ("thisyear", "2024-01-01T00:00:00", "2025-01-01T00:00:00"),
            ("nextyear", "2025-01-01T00:00:00", "2026-01-01T00:00:00"),
            ("prev3days", "2024-01-12T00:00:00", "2024-01-15T00:00:00"),
            ("next3days", "2024-01-16T00:00:00", "2024-01-19T00:00:00"),
            ("tomorrow", "2024-01-16T00:00:00", "2024-01-17T00:00:00"),
            ("2024", "2024-01-01T00:00:00", "2025-01-01T00:00:00"),
            ("2024-02", "2024-02-01T00:00:00", "2024-03-01T00:00:00"),
            ("2024-12", "2024-12-01T00:00:00", "2025-01-01T00:00:00"),
            ("2024-02-29", "2024-02-29T00:00:00", "2024-03-01T00:00:00"),
            ("mtd", "2024-01-01T00:00:00", "2024-01-15T00:00:00"),
            ("qtd", "2024-01-01T00:00:00", "2024-01-15T00:00:00"),
            ("ytd", "2024-01-01T00:00:00", "2024-01-15T00:00:00"),
        ] {
            assert_eq!(filter(value, &clock), within(from, until), "{value}");
        }
    }

    /// An age reaches back from the query start and, alone, matches from
    /// there on, the future included. Months and years step back on the
    /// calendar, landing on the month's last day when it is shorter. Compared,
    /// it is an age, as in Everything (L8:7-14): `<30days` is newer than 30
    /// days, `>30days` older (v0.6.0 read them the other way round).
    #[test]
    fn an_age_reaches_back_from_the_query_start() {
        let clock = FixedClock::utc(ft("2026-03-31T12:00:00"));
        let from = |at: &str| DateFilter::Within {
            min: ft(at),
            max: i64::MAX,
        };
        for (value, at) in [
            ("7days", "2026-03-24T12:00:00"),
            ("last7days", "2026-03-24T12:00:00"),
            ("past7days", "2026-03-24T12:00:00"),
            ("7d", "2026-03-24T12:00:00"),
            ("1week", "2026-03-24T12:00:00"),
            ("1w", "2026-03-24T12:00:00"),
            ("lastweek", "2026-03-24T12:00:00"),
            ("pastweek", "2026-03-24T12:00:00"),
            ("last2hours", "2026-03-31T10:00:00"),
            ("2h", "2026-03-31T10:00:00"),
            ("2hrs", "2026-03-31T10:00:00"),
            ("lasthour", "2026-03-31T11:00:00"),
            ("lastday", "2026-03-30T12:00:00"),
            ("10mins", "2026-03-31T11:50:00"),
            ("10m", "2026-03-31T11:50:00"),
            ("30secs", "2026-03-31T11:59:30"),
            ("30s", "2026-03-31T11:59:30"),
            ("1month", "2026-02-28T12:00:00"),
            ("1mo", "2026-02-28T12:00:00"),
            ("lastmonth", "2026-02-28T12:00:00"),
            ("last13months", "2025-02-28T12:00:00"),
            ("2years", "2024-03-31T12:00:00"),
            ("2y", "2024-03-31T12:00:00"),
            ("lastyear", "2025-03-31T12:00:00"),
        ] {
            assert_eq!(filter(value, &clock), from(at), "{value}");
        }
        assert!(filter("7days", &clock).matches(Some(ft("2027-01-01T00:00:00"))));
        let age = ft("2026-03-01T12:00:00");
        assert_eq!(bounds("<30days", &clock), (age + 1, i64::MAX), "newer");
        assert_eq!(bounds("<=30days", &clock), (age, i64::MAX));
        assert_eq!(bounds(">30days", &clock), (i64::MIN, age - 1), "older");
        assert_eq!(bounds(">=30days", &clock), (i64::MIN, age));
        assert_eq!(bounds("=30days", &clock), (age, age), "the instant");
        assert_eq!(
            bounds("30days..7days", &clock),
            (age, ft("2026-03-24T12:00:00"))
        );
        assert_eq!(
            bounds("7days..30days", &clock),
            (age, ft("2026-03-24T12:00:00")),
            "the right way round"
        );
        assert_eq!(
            filter("coming2weeks", &clock),
            DateFilter::Within {
                min: ft("2026-03-31T12:00:00"),
                max: ft("2026-04-14T12:00:00") - 1
            },
            "coming with a count reaches forward from now"
        );
        assert_eq!(
            filter("coming1month", &clock),
            DateFilter::Within {
                min: ft("2026-03-31T12:00:00"),
                max: ft("2026-04-30T12:00:00") - 1
            }
        );
    }

    /// A comparison compares times at the value's granularity, and a range
    /// runs from the start of its lower value to the end of its upper one,
    /// the two swapped when written backwards.
    #[test]
    fn comparisons_and_ranges_bound_by_whole_periods() {
        let clock = FixedClock::utc(ft("2026-10-18T12:00:00"));
        let b = |value: &str| bounds(value, &clock);
        let at = ft;
        assert_eq!(b(">2024"), (at("2025-01-01T00:00:00"), i64::MAX));
        assert_eq!(b(">=2024-01-01"), (at("2024-01-01T00:00:00"), i64::MAX));
        assert_eq!(b("<2024-05"), (i64::MIN, at("2024-05-01T00:00:00") - 1));
        assert_eq!(b("<=today"), (i64::MIN, at("2026-10-19T00:00:00") - 1));
        assert_eq!(filter("=yesterday", &clock), filter("yesterday", &clock));
        assert_eq!(filter("==yesterday", &clock), filter("yesterday", &clock));
        assert_eq!(
            filter("2024-01-01..2024-03-31", &clock),
            within("2024-01-01T00:00:00", "2024-04-01T00:00:00")
        );
        assert_eq!(
            filter("2024-03-31..2024-01-01", &clock),
            within("2024-01-01T00:00:00", "2024-04-01T00:00:00"),
            "backwards"
        );
        assert_eq!(b("2024-05.."), (at("2024-05-01T00:00:00"), i64::MAX));
        assert_eq!(b("..2024"), (i64::MIN, at("2025-01-01T00:00:00") - 1));
        assert_eq!(
            b(" 2024 .. 2025 "),
            (at("2024-01-01T00:00:00"), at("2026-01-01T00:00:00") - 1)
        );
        assert_eq!(b("2024-2025"), b("2024..2025"), "a dash between two years");
        assert_eq!(
            b("2024-12-15-2025-01-20"),
            b("2024-12-15..2025-01-20"),
            "a dash between two days"
        );
        assert_eq!(b("2020-13"), b("2013..2020"), "13 is a two-digit year");
        assert_eq!(b(""), (i64::MIN, i64::MAX), "any known date");
    }

    /// The shapes a date can be written in, each naming its period.
    #[test]
    fn every_written_shape_names_its_period() {
        // A Tuesday, November 10th, at 11:55 local in Istanbul.
        let clock = FixedClock::istanbul_en_us(ft("2026-11-10T08:55:00"));
        let local = |from: &str, until: &str| DateFilter::Within {
            min: ft(from) - 3 * HOUR,
            max: ft(until) - 3 * HOUR - 1,
        };
        let nov4 = local("2026-11-04T00:00:00", "2026-11-05T00:00:00");
        // `wed`: the most recent Wednesday.
        for shape in [
            "2026-11-04",
            "2026-11-4",
            "20261104",
            "261104",
            "11/4/2026",
            "11/04/2026",
            "11/4/26",
            "11/4",
            "2026/11/4",
            "2026/11/04",
            "2026.11.04",
            "WED",
            "wednesday",
        ] {
            assert_eq!(filter(shape, &clock), nov4, "{shape}");
        }
        assert_eq!(filter("tue", &clock), filter("today", &clock), "today");
        assert_eq!(filter("tues", &clock), filter("tuesday", &clock));
        assert_eq!(filter("thur", &clock), filter("thursday", &clock));
        assert_eq!(filter("thurs", &clock), filter("thu", &clock));
        let november = local("2026-11-01T00:00:00", "2026-12-01T00:00:00");
        for shape in [
            "2026-11",
            "202611",
            "11/2026",
            "2026/11",
            "nov",
            "november",
            "nov2026",
            "november2026",
            "2026nov",
            "nov-2026",
        ] {
            assert_eq!(filter(shape, &clock), november, "{shape}");
        }
        assert_eq!(
            filter("dec", &clock),
            local("2025-12-01T00:00:00", "2026-01-01T00:00:00"),
            "the most recent December"
        );
        assert_eq!(
            filter("26", &clock),
            local("2026-01-01T00:00:00", "2027-01-01T00:00:00")
        );
        assert_eq!(
            filter("50", &clock),
            local("1950-01-01T00:00:00", "1951-01-01T00:00:00")
        );
        for (shape, from, until) in [
            (
                "2026-11-04T12",
                "2026-11-04T12:00:00",
                "2026-11-04T13:00:00",
            ),
            ("2026-11-04t9", "2026-11-04T09:00:00", "2026-11-04T10:00:00"),
            ("20261104T12", "2026-11-04T12:00:00", "2026-11-04T13:00:00"),
            (
                "2026-11-04T12:30",
                "2026-11-04T12:30:00",
                "2026-11-04T12:31:00",
            ),
            (
                "20261104T1230",
                "2026-11-04T12:30:00",
                "2026-11-04T12:31:00",
            ),
            (
                "2026-11-04T12:30:15",
                "2026-11-04T12:30:15",
                "2026-11-04T12:30:16",
            ),
            (
                "20261104T123015",
                "2026-11-04T12:30:15",
                "2026-11-04T12:30:16",
            ),
        ] {
            assert_eq!(filter(shape, &clock), local(from, until), "{shape}");
        }
        let half = ft("2026-11-04T12:30:15") - 3 * HOUR + SECOND / 2;
        assert_eq!(
            filter("2026-11-04T12:30:15.5", &clock),
            DateFilter::Within {
                min: half,
                max: half + SECOND / 10 - 1
            }
        );
        let wed = ft("2026-11-04T09:00:00");
        assert_eq!(bounds("134382564000000000", &clock), (wed, wed));
        assert_eq!(bounds("0x1DD6C3ADF39E800", &clock), (wed, wed));
        let second = ft("2026-11-10T08:55:00");
        assert_eq!(bounds("now", &clock), (second, second + SECOND - 1));
        // Another regional order.
        let mut dmy = FixedClock::istanbul_en_us(ft("2026-11-10T08:55:00"));
        dmy.date_order = DateOrder::DayMonthYear;
        assert_eq!(filter("4/11/2026", &dmy), nov4);
        assert_eq!(filter("04.11.2026", &dmy), nov4);
        assert_eq!(filter("4/11", &dmy), nov4);
        let mut ymd = dmy;
        ymd.date_order = DateOrder::YearMonthDay;
        assert_eq!(filter("26/11/4", &ymd), nov4);
    }

    /// An unknown date matches only `unknown`, never a date or its
    /// complement, and two `dm:` terms must both pass.
    #[test]
    fn unknown_dates_match_only_unknown() {
        let clock = FixedClock::utc(ft("2026-10-18T12:00:00"));
        let date = Some(ft("2026-10-18T08:00:00"));
        for value in ["today", "<today", ">=today", "..", "1day", "", ">1day"] {
            assert!(!filter(value, &clock).matches(None), "{value}");
        }
        let unknown = filter("Unknown", &clock);
        assert!(unknown.matches(None));
        assert!(!unknown.matches(date));
        let today = filter("today", &clock);
        assert!(today.matches(date));
        assert!(!today.and(filter(">today", &clock)).matches(date));
        assert!(!today.and(unknown).matches(date));
        assert!(!today.and(unknown).matches(None));
        assert_eq!(unknown.and(unknown), unknown);
    }

    /// A value that names no date is refused rather than matching nothing,
    /// and so is a date FILETIME cannot hold.
    #[test]
    fn a_value_that_names_no_date_is_refused() {
        let clock = FixedClock::utc(ft("2026-10-18T12:00:00"));
        for value in [
            "2026-02-30",
            "2026-13-01",
            "2026-00-01",
            "26-01-01",
            "2026-01-01-01",
            "13/1/2026",
            "1/32/2026",
            "2026/13",
            "0days",
            "lastdays",
            "days",
            "+5days",
            "7fortnights",
            "2hr",
            "2wk",
            "2yr",
            "2mos",
            "this2weeks",
            "thisquarter",
            "thishour",
            "sept",
            ">unknown",
            "unknown..",
            "2026-11-04T24",
            "2026-11-04T12:60",
            "2026-11-04T12.5",
            "20261104T1",
            "2026-W45",
            "1500",
            "12345",
            "1",
            "99999999999999999999weeks",
            "9999999999999999years",
            "0xZZ",
            "99999999999999999999",
        ] {
            assert!(
                matches!(
                    date_filter(value, &clock),
                    Err(QueryError::Malformed { what: "dm:", .. })
                ),
                "{value}: {:?}",
                date_filter(value, &clock)
            );
        }
        for value in REFUSED {
            assert!(date_filter(value, &clock).is_err(), "{value}");
        }
    }

    /// The clock is read from the pipe's side, so any value, against the
    /// earliest or latest clock, fails or resolves without overflowing.
    #[test]
    fn extreme_clocks_never_overflow() {
        for now in [i64::MIN, -1, 0, i64::MAX] {
            for offset in [-14 * HOUR, 0, 14 * HOUR] {
                for date_order in [
                    DateOrder::MonthDayYear,
                    DateOrder::DayMonthYear,
                    DateOrder::YearMonthDay,
                ] {
                    let clock = FixedClock {
                        now,
                        offset,
                        week_start: 3,
                        date_order,
                    };
                    for value in [
                        "today",
                        "yesterday",
                        "tomorrow",
                        "now",
                        "thisweek",
                        "prevweek",
                        "nextweek",
                        "lastweek",
                        "thismonth",
                        "prev999999months",
                        "next999999years",
                        "lastmonth",
                        "thisyear",
                        "lastyear",
                        "mtd",
                        "qtd",
                        "ytd",
                        "dec",
                        "sun",
                        "9999",
                        "1601-01-01",
                        "4294967295weeks",
                        "9223372036854775807secs",
                        "last100years",
                        "coming999999years",
                        "<today",
                        ">today",
                        "today..",
                        ">7days",
                        "<7days",
                        "11/4",
                        "11/4/26",
                        "999999999999999999",
                        "0x7FFFFFFFFFFFFFFF",
                        "1601-01-01T00:00:00.0000001",
                    ] {
                        let _ = date_filter(value, &clock);
                    }
                }
            }
        }
    }

    /// Which battery a live answer came from, and so the clock it ran at:
    /// the batteries of DESIGN.md §0 (L1 to L3, at about 08:56 UTC) and of
    /// step S2 (L7 at 18:28:47-51 UTC, L8 at 18:32:11-12 UTC), all on
    /// 2026-11-10, in UTC+3 with an en-US regional format.
    #[derive(Clone, Copy)]
    enum Battery {
        L12,
        L7,
        L8,
    }

    impl Battery {
        fn clock(self) -> FixedClock {
            FixedClock::istanbul_en_us(ft(match self {
                Battery::L12 => "2026-11-10T08:56:00",
                Battery::L7 => "2026-11-10T18:28:49",
                Battery::L8 => "2026-11-10T18:32:12",
            }))
        }
    }

    /// The fixture folder a live answer's query was scoped to, and its files'
    /// dates modified, read from the fixture tree.
    #[derive(Clone, Copy)]
    enum Folder {
        D2,
        Dates,
    }

    impl Folder {
        fn files(self) -> &'static [(&'static str, i64)] {
            match self {
                Folder::D2 => &[
                    ("dec2025", 134102628000000000),
                    ("fut10d", 134396385360000000),
                    ("fut3d", 134390337360000000),
                    ("jan2026", 134133732000000000),
                    ("may2025", 133917732000000000),
                    ("mon1109", 134386884000000000),
                    ("oct2026b", 134356644000000000),
                    ("rel29d", 134362689360000000),
                    ("rel2h", 134387673360000000),
                    ("rel32d", 134360097360000000),
                    ("rel6d12h", 134382129360000000),
                    ("rel7d1h", 134381661360000000),
                    ("sat1107", 134385156000000000),
                    ("sep2026", 134339364000000000),
                    ("sun1108", 134386020000000000),
                    ("t1015", 132235461000000000),
                    ("thu1105", 134383428000000000),
                    ("wed1104", 134382564000000000),
                    ("y1930", 103952484000000000),
                    ("y1950", 110264004000000000),
                    ("y2029", 135194724000000000),
                    ("y2049", 141506244000000000),
                ],
                Folder::Dates => &[
                    ("d1999", 125649867670000000),
                    ("d2020a", 132235470000000000),
                    ("d2020b", 132539219400000000),
                    ("d2021", 132670008000000000),
                    ("d2024may", 133590276000000000),
                    ("d2024wed", 133596180000000000),
                    ("d2026nov", 134380656000000000),
                    ("d2026oct", 134369424000000000),
                ],
            }
        }
    }

    /// Whether `mtime` passes `value` as the parser builds it: a list or a
    /// value group is an OR of its values, and a `!` (or `!=`) a NOT.
    fn live_matches(value: &str, clock: &dyn LocalClock, mtime: i64) -> Result<bool, QueryError> {
        let any = |values: std::str::Split<'_, char>| -> Result<bool, QueryError> {
            let mut hit = false;
            for v in values {
                hit |= live_matches(v, clock, mtime)?;
            }
            Ok(hit)
        };
        if let Some(group) = value.strip_prefix('<').and_then(|v| v.strip_suffix('>'))
            && group.contains('|')
        {
            return any(group.split('|'));
        }
        if value.contains(';') {
            return any(value.split(';'));
        }
        if let Some(rest) = value.strip_prefix('!') {
            return Ok(!live_matches(rest, clock, mtime)?);
        }
        Ok(date_filter(value, clock)?.matches(Some(mtime)))
    }

    /// Everything 1.5's own answers for its fixture's dated files, as goz
    /// must give them: every `dm:` query the batteries ran against the `d2`
    /// and `dates` folders, with the clock it ran at, but those whose meaning
    /// goz does not share (`mon`, DESIGN.md §3) and those it refuses.
    #[test]
    fn everything_answers_for_the_dated_fixture() {
        let mut failures = Vec::new();
        for &(line, battery, folder, value, want) in LIVE {
            let clock = battery.clock();
            let mut got = Vec::new();
            for &(name, mtime) in folder.files() {
                match live_matches(value, &clock, mtime) {
                    Ok(true) => got.push(name),
                    Ok(false) => {}
                    Err(e) => {
                        failures.push(format!("{line} {value}: {e}"));
                        break;
                    }
                }
            }
            if got != want {
                failures.push(format!("{line} {value}: want {want:?}, got {got:?}"));
            }
        }
        assert!(
            failures.is_empty(),
            "{} of {} differ:\n{}",
            failures.len(),
            LIVE.len(),
            failures.join("\n")
        );
    }

    /// Generated by `scratch\gozd\scripts\mk_date_table.py` from the live
    /// batteries: (source line, clock, folder, value, the files found).
    #[rustfmt::skip]
    const LIVE: &[(&str, Battery, Folder, &str, &[&str])] = &[
        ("L1:304", Battery::L12, Folder::Dates, "2020", &["d2020a", "d2020b"]),
        ("L1:305", Battery::L12, Folder::Dates, "20", &["d2020a", "d2020b"]),
        ("L1:306", Battery::L12, Folder::Dates, "99", &["d1999"]),
        ("L1:307", Battery::L12, Folder::Dates, "2020-01", &["d2020a"]),
        ("L1:308", Battery::L12, Folder::Dates, "2020-01-15", &["d2020a"]),
        ("L1:309", Battery::L12, Folder::Dates, "2020-01-15T10", &["d2020a"]),
        ("L1:310", Battery::L12, Folder::Dates, "2020-01-15T10:30", &["d2020a"]),
        ("L1:311", Battery::L12, Folder::Dates, "2020-01-15T11", &[]),
        ("L1:312", Battery::L12, Folder::Dates, "20200115", &["d2020a"]),
        ("L1:313", Battery::L12, Folder::Dates, "200115", &["d2020a"]),
        ("L1:314", Battery::L12, Folder::Dates, "1/15/2020", &["d2020a"]),
        ("L1:316", Battery::L12, Folder::Dates, "01/2020", &["d2020a"]),
        ("L1:317", Battery::L12, Folder::Dates, "2020/01", &["d2020a"]),
        ("L1:318", Battery::L12, Folder::Dates, "2020/01/15", &["d2020a"]),
        ("L1:319", Battery::L12, Folder::Dates, "jan", &[]),
        ("L1:320", Battery::L12, Folder::Dates, "january", &[]),
        ("L1:321", Battery::L12, Folder::Dates, "may", &[]),
        ("L1:322", Battery::L12, Folder::Dates, "wed", &[]),
        ("L1:323", Battery::L12, Folder::Dates, "wednesday", &[]),
        ("L1:324", Battery::L12, Folder::Dates, "mtd", &["d2026nov"]),
        ("L1:325", Battery::L12, Folder::Dates, "ytd", &["d2026nov", "d2026oct"]),
        ("L1:326", Battery::L12, Folder::Dates, "qtd", &["d2026nov", "d2026oct"]),
        ("L1:327", Battery::L12, Folder::Dates, "today", &[]),
        ("L1:328", Battery::L12, Folder::Dates, "yesterday", &[]),
        ("L1:329", Battery::L12, Folder::Dates, "thisweek", &[]),
        ("L1:330", Battery::L12, Folder::Dates, "currentweek", &[]),
        ("L1:331", Battery::L12, Folder::Dates, "lastweek", &[]),
        ("L1:332", Battery::L12, Folder::Dates, "prevweek", &["d2026nov"]),
        ("L1:333", Battery::L12, Folder::Dates, "previousweek", &["d2026nov"]),
        ("L1:334", Battery::L12, Folder::Dates, "prev2weeks", &["d2026nov"]),
        ("L1:335", Battery::L12, Folder::Dates, "thismonth", &["d2026nov"]),
        ("L1:336", Battery::L12, Folder::Dates, "lastmonth", &["d2026nov", "d2026oct"]),
        ("L1:337", Battery::L12, Folder::Dates, "last2months", &["d2026nov", "d2026oct"]),
        ("L1:338", Battery::L12, Folder::Dates, "past2months", &["d2026nov", "d2026oct"]),
        ("L1:339", Battery::L12, Folder::Dates, "pastmonth", &["d2026nov", "d2026oct"]),
        ("L1:340", Battery::L12, Folder::Dates, "lastyear", &["d2026nov", "d2026oct"]),
        ("L1:341", Battery::L12, Folder::Dates, "thisyear", &["d2026nov", "d2026oct"]),
        ("L1:342", Battery::L12, Folder::Dates, "nextweek", &[]),
        ("L1:343", Battery::L12, Folder::Dates, "comingweek", &[]),
        ("L1:344", Battery::L12, Folder::Dates, "next2days", &[]),
        ("L1:345", Battery::L12, Folder::Dates, "tomorrow", &[]),
        ("L1:346", Battery::L12, Folder::Dates, "now", &[]),
        ("L1:347", Battery::L12, Folder::Dates, "30days", &["d2026nov", "d2026oct"]),
        ("L1:348", Battery::L12, Folder::Dates, "last30days", &["d2026nov", "d2026oct"]),
        ("L1:349", Battery::L12, Folder::Dates, "3weeks", &["d2026nov"]),
        ("L1:350", Battery::L12, Folder::Dates, ">2020", &["d2021", "d2024may", "d2024wed", "d2026nov", "d2026oct"]),
        ("L1:351", Battery::L12, Folder::Dates, ">=2020", &["d2020a", "d2020b", "d2021", "d2024may", "d2024wed", "d2026nov", "d2026oct"]),
        ("L1:352", Battery::L12, Folder::Dates, "<2020", &["d1999"]),
        ("L1:353", Battery::L12, Folder::Dates, "<=2020", &["d1999", "d2020a", "d2020b"]),
        ("L1:354", Battery::L12, Folder::Dates, "2020..2021", &["d2020a", "d2020b", "d2021"]),
        ("L1:355", Battery::L12, Folder::Dates, "2020-2021", &["d2020a", "d2020b", "d2021"]),
        ("L1:356", Battery::L12, Folder::Dates, "!2020", &["d1999", "d2021", "d2024may", "d2024wed", "d2026nov", "d2026oct"]),
        ("L1:357", Battery::L12, Folder::Dates, "!=2020", &["d1999", "d2021", "d2024may", "d2024wed", "d2026nov", "d2026oct"]),
        ("L1:358", Battery::L12, Folder::Dates, "2020;2024", &["d2020a", "d2020b", "d2024may", "d2024wed"]),
        ("L1:365", Battery::L12, Folder::Dates, "unknown", &[]),
        ("L1:366", Battery::L12, Folder::Dates, "", &["d1999", "d2020a", "d2020b", "d2021", "d2024may", "d2024wed", "d2026nov", "d2026oct"]),
        ("L1:367", Battery::L12, Folder::Dates, "2020-1-15", &["d2020a"]),
        ("L1:368", Battery::L12, Folder::Dates, "2020-13", &["d2020a", "d2020b"]),
        ("L2:1", Battery::L12, Folder::D2, "50", &["y1950"]),
        ("L2:2", Battery::L12, Folder::D2, "49", &["y2049"]),
        ("L2:3", Battery::L12, Folder::D2, "30", &[]),
        ("L2:4", Battery::L12, Folder::D2, "29", &["y2029"]),
        ("L2:5", Battery::L12, Folder::D2, "jan", &["jan2026"]),
        ("L2:6", Battery::L12, Folder::D2, "january", &["jan2026"]),
        ("L2:7", Battery::L12, Folder::D2, "may", &[]),
        ("L2:8", Battery::L12, Folder::D2, "nov", &["fut10d", "fut3d", "mon1109", "rel2h", "rel6d12h", "rel7d1h", "sat1107", "sun1108", "thu1105", "wed1104"]),
        ("L2:9", Battery::L12, Folder::D2, "november", &["fut10d", "fut3d", "mon1109", "rel2h", "rel6d12h", "rel7d1h", "sat1107", "sun1108", "thu1105", "wed1104"]),
        ("L2:10", Battery::L12, Folder::D2, "oct", &["oct2026b", "rel29d", "rel32d"]),
        ("L2:11", Battery::L12, Folder::D2, "dec", &["dec2025"]),
        ("L2:13", Battery::L12, Folder::D2, "monday", &["mon1109"]),
        ("L2:14", Battery::L12, Folder::D2, "wed", &["wed1104"]),
        ("L2:15", Battery::L12, Folder::D2, "thu", &["thu1105"]),
        ("L2:16", Battery::L12, Folder::D2, "sat", &["sat1107"]),
        ("L2:17", Battery::L12, Folder::D2, "sun", &["sun1108"]),
        ("L2:18", Battery::L12, Folder::D2, "thisweek", &["fut3d", "mon1109", "rel2h", "sun1108"]),
        ("L2:19", Battery::L12, Folder::D2, "currentweek", &["fut3d", "mon1109", "rel2h", "sun1108"]),
        ("L2:20", Battery::L12, Folder::D2, "lastweek", &["fut10d", "fut3d", "mon1109", "rel2h", "rel6d12h", "sat1107", "sun1108", "thu1105", "wed1104", "y2029", "y2049"]),
        ("L2:21", Battery::L12, Folder::D2, "last1week", &["fut10d", "fut3d", "mon1109", "rel2h", "rel6d12h", "sat1107", "sun1108", "thu1105", "wed1104", "y2029", "y2049"]),
        ("L2:22", Battery::L12, Folder::D2, "1week", &["fut10d", "fut3d", "mon1109", "rel2h", "rel6d12h", "sat1107", "sun1108", "thu1105", "wed1104", "y2029", "y2049"]),
        ("L2:23", Battery::L12, Folder::D2, "pastweek", &["fut10d", "fut3d", "mon1109", "rel2h", "rel6d12h", "sat1107", "sun1108", "thu1105", "wed1104", "y2029", "y2049"]),
        ("L2:24", Battery::L12, Folder::D2, "prevweek", &["rel6d12h", "rel7d1h", "sat1107", "thu1105", "wed1104"]),
        ("L2:25", Battery::L12, Folder::D2, "previousweek", &["rel6d12h", "rel7d1h", "sat1107", "thu1105", "wed1104"]),
        ("L2:26", Battery::L12, Folder::D2, "prevmonth", &["oct2026b", "rel29d", "rel32d"]),
        ("L2:27", Battery::L12, Folder::D2, "previousmonth", &["oct2026b", "rel29d", "rel32d"]),
        ("L2:28", Battery::L12, Folder::D2, "prevyear", &["dec2025", "may2025"]),
        ("L2:29", Battery::L12, Folder::D2, "lastmonth", &["fut10d", "fut3d", "mon1109", "rel29d", "rel2h", "rel6d12h", "rel7d1h", "sat1107", "sun1108", "thu1105", "wed1104", "y2029", "y2049"]),
        ("L2:30", Battery::L12, Folder::D2, "pastmonth", &["fut10d", "fut3d", "mon1109", "rel29d", "rel2h", "rel6d12h", "rel7d1h", "sat1107", "sun1108", "thu1105", "wed1104", "y2029", "y2049"]),
        ("L2:31", Battery::L12, Folder::D2, "thismonth", &["fut10d", "fut3d", "mon1109", "rel2h", "rel6d12h", "rel7d1h", "sat1107", "sun1108", "thu1105", "wed1104"]),
        ("L2:32", Battery::L12, Folder::D2, "currentmonth", &["fut10d", "fut3d", "mon1109", "rel2h", "rel6d12h", "rel7d1h", "sat1107", "sun1108", "thu1105", "wed1104"]),
        ("L2:33", Battery::L12, Folder::D2, "lastyear", &["dec2025", "fut10d", "fut3d", "jan2026", "mon1109", "oct2026b", "rel29d", "rel2h", "rel32d", "rel6d12h", "rel7d1h", "sat1107", "sep2026", "sun1108", "thu1105", "wed1104", "y2029", "y2049"]),
        ("L2:34", Battery::L12, Folder::D2, "pastyear", &["dec2025", "fut10d", "fut3d", "jan2026", "mon1109", "oct2026b", "rel29d", "rel2h", "rel32d", "rel6d12h", "rel7d1h", "sat1107", "sep2026", "sun1108", "thu1105", "wed1104", "y2029", "y2049"]),
        ("L2:35", Battery::L12, Folder::D2, "thisyear", &["fut10d", "fut3d", "jan2026", "mon1109", "oct2026b", "rel29d", "rel2h", "rel32d", "rel6d12h", "rel7d1h", "sat1107", "sep2026", "sun1108", "thu1105", "wed1104"]),
        ("L2:36", Battery::L12, Folder::D2, "currentyear", &["fut10d", "fut3d", "jan2026", "mon1109", "oct2026b", "rel29d", "rel2h", "rel32d", "rel6d12h", "rel7d1h", "sat1107", "sep2026", "sun1108", "thu1105", "wed1104"]),
        ("L2:37", Battery::L12, Folder::D2, "prev2months", &["oct2026b", "rel29d", "rel32d", "sep2026"]),
        ("L2:38", Battery::L12, Folder::D2, "last2weeks", &["fut10d", "fut3d", "mon1109", "rel2h", "rel6d12h", "rel7d1h", "sat1107", "sun1108", "thu1105", "wed1104", "y2029", "y2049"]),
        ("L2:39", Battery::L12, Folder::D2, "7days", &["fut10d", "fut3d", "mon1109", "rel2h", "rel6d12h", "sat1107", "sun1108", "thu1105", "wed1104", "y2029", "y2049"]),
        ("L2:40", Battery::L12, Folder::D2, "today", &["rel2h"]),
        ("L2:41", Battery::L12, Folder::D2, "yesterday", &["mon1109"]),
        ("L2:42", Battery::L12, Folder::D2, "tomorrow", &[]),
        ("L2:43", Battery::L12, Folder::D2, "nextweek", &["fut10d"]),
        ("L2:44", Battery::L12, Folder::D2, "comingweek", &["fut10d"]),
        ("L2:45", Battery::L12, Folder::D2, "next5days", &["fut3d"]),
        ("L2:46", Battery::L12, Folder::D2, "coming5days", &["fut3d"]),
        ("L2:47", Battery::L12, Folder::D2, "nextmonth", &[]),
        ("L2:48", Battery::L12, Folder::D2, "now", &[]),
        ("L2:49", Battery::L12, Folder::D2, ">now", &["fut10d", "fut3d", "y2029", "y2049"]),
        ("L2:50", Battery::L12, Folder::D2, "<now", &["dec2025", "jan2026", "may2025", "mon1109", "oct2026b", "rel29d", "rel2h", "rel32d", "rel6d12h", "rel7d1h", "sat1107", "sep2026", "sun1108", "t1015", "thu1105", "wed1104", "y1930", "y1950"]),
        ("L2:51", Battery::L12, Folder::D2, ">today", &["fut10d", "fut3d", "y2029", "y2049"]),
        ("L2:52", Battery::L12, Folder::D2, ">=today", &["fut10d", "fut3d", "rel2h", "y2029", "y2049"]),
        ("L2:53", Battery::L12, Folder::D2, "<yesterday", &["dec2025", "jan2026", "may2025", "oct2026b", "rel29d", "rel32d", "rel6d12h", "rel7d1h", "sat1107", "sep2026", "sun1108", "t1015", "thu1105", "wed1104", "y1930", "y1950"]),
        ("L2:54", Battery::L12, Folder::D2, "2hours", &["fut10d", "fut3d", "y2029", "y2049"]),
        ("L2:55", Battery::L12, Folder::D2, "3hours", &["fut10d", "fut3d", "rel2h", "y2029", "y2049"]),
        ("L2:56", Battery::L12, Folder::D2, "1hour", &["fut10d", "fut3d", "y2029", "y2049"]),
        ("L2:57", Battery::L12, Folder::D2, "mtd", &["mon1109", "rel6d12h", "rel7d1h", "sat1107", "sun1108", "thu1105", "wed1104"]),
        ("L2:58", Battery::L12, Folder::D2, "qtd", &["mon1109", "oct2026b", "rel29d", "rel32d", "rel6d12h", "rel7d1h", "sat1107", "sun1108", "thu1105", "wed1104"]),
        ("L2:59", Battery::L12, Folder::D2, "ytd", &["jan2026", "mon1109", "oct2026b", "rel29d", "rel32d", "rel6d12h", "rel7d1h", "sat1107", "sep2026", "sun1108", "thu1105", "wed1104"]),
        ("L2:60", Battery::L12, Folder::D2, "lastweek..today", &["mon1109", "rel2h", "rel6d12h", "sat1107", "sun1108", "thu1105", "wed1104"]),
        ("L2:61", Battery::L12, Folder::D2, "2026-11-04..2026-11-08", &["sat1107", "sun1108", "thu1105", "wed1104"]),
        ("L2:62", Battery::L12, Folder::D2, "2026-11-08..2026-11-04", &["sat1107", "sun1108", "thu1105", "wed1104"]),
        ("L2:63", Battery::L12, Folder::D2, "2020-01-15T10:00..2020-01-15T10:20", &["t1015"]),
        ("L2:64", Battery::L12, Folder::D2, ">=2020-01-15T10:15", &["dec2025", "fut10d", "fut3d", "jan2026", "may2025", "mon1109", "oct2026b", "rel29d", "rel2h", "rel32d", "rel6d12h", "rel7d1h", "sat1107", "sep2026", "sun1108", "t1015", "thu1105", "wed1104", "y2029", "y2049"]),
        ("L2:65", Battery::L12, Folder::D2, "2020-01-15T10:15:00", &["t1015"]),
        ("L2:66", Battery::L12, Folder::D2, "11/2026", &["fut10d", "fut3d", "mon1109", "rel2h", "rel6d12h", "rel7d1h", "sat1107", "sun1108", "thu1105", "wed1104"]),
        ("L2:67", Battery::L12, Folder::D2, "2026/11", &["fut10d", "fut3d", "mon1109", "rel2h", "rel6d12h", "rel7d1h", "sat1107", "sun1108", "thu1105", "wed1104"]),
        ("L2:68", Battery::L12, Folder::D2, "11/4/2026", &["wed1104"]),
        ("L2:69", Battery::L12, Folder::D2, "4/11/2026", &[]),
        ("L2:70", Battery::L12, Folder::D2, "2026-11", &["fut10d", "fut3d", "mon1109", "rel2h", "rel6d12h", "rel7d1h", "sat1107", "sun1108", "thu1105", "wed1104"]),
        ("L2:71", Battery::L12, Folder::D2, "202611", &["fut10d", "fut3d", "mon1109", "rel2h", "rel6d12h", "rel7d1h", "sat1107", "sun1108", "thu1105", "wed1104"]),
        ("L2:72", Battery::L12, Folder::D2, "261104", &["wed1104"]),
        ("L2:73", Battery::L12, Folder::D2, "20261104", &["wed1104"]),
        ("L2:100", Battery::L12, Folder::Dates, "<2021", &["d1999", "d2020a", "d2020b"]),
        ("L2:101", Battery::L12, Folder::Dates, "<2021|2024>", &["d2021", "d2024may", "d2024wed"]),
        ("L3:24", Battery::L12, Folder::D2, "tue", &["rel2h"]),
        ("L3:25", Battery::L12, Folder::D2, "tuesday", &["rel2h"]),
        ("L7:162", Battery::L7, Folder::Dates, "2020", &["d2020a", "d2020b"]),
        ("L7:163", Battery::L7, Folder::Dates, "20", &["d2020a", "d2020b"]),
        ("L7:164", Battery::L7, Folder::Dates, "99", &["d1999"]),
        ("L7:165", Battery::L7, Folder::Dates, "2020-01", &["d2020a"]),
        ("L7:166", Battery::L7, Folder::Dates, "2020-01-15", &["d2020a"]),
        ("L7:167", Battery::L7, Folder::Dates, "2020-01-15T10", &["d2020a"]),
        ("L7:168", Battery::L7, Folder::Dates, "2020-01-15T10:30", &["d2020a"]),
        ("L7:169", Battery::L7, Folder::Dates, "2020-01-15T11", &[]),
        ("L7:170", Battery::L7, Folder::Dates, "20200115", &["d2020a"]),
        ("L7:171", Battery::L7, Folder::Dates, "200115", &["d2020a"]),
        ("L7:172", Battery::L7, Folder::Dates, "1/15/2020", &["d2020a"]),
        ("L7:174", Battery::L7, Folder::Dates, "01/2020", &["d2020a"]),
        ("L7:175", Battery::L7, Folder::Dates, "2020/01", &["d2020a"]),
        ("L7:176", Battery::L7, Folder::Dates, "2020/01/15", &["d2020a"]),
        ("L7:177", Battery::L7, Folder::Dates, "jan", &[]),
        ("L7:178", Battery::L7, Folder::Dates, "january", &[]),
        ("L7:179", Battery::L7, Folder::Dates, "may", &[]),
        ("L7:180", Battery::L7, Folder::Dates, "wed", &[]),
        ("L7:181", Battery::L7, Folder::Dates, "wednesday", &[]),
        ("L7:182", Battery::L7, Folder::Dates, "mtd", &["d2026nov"]),
        ("L7:183", Battery::L7, Folder::Dates, "ytd", &["d2026nov", "d2026oct"]),
        ("L7:184", Battery::L7, Folder::Dates, "qtd", &["d2026nov", "d2026oct"]),
        ("L7:185", Battery::L7, Folder::Dates, "today", &[]),
        ("L7:186", Battery::L7, Folder::Dates, "yesterday", &[]),
        ("L7:187", Battery::L7, Folder::Dates, "thisweek", &[]),
        ("L7:188", Battery::L7, Folder::Dates, "currentweek", &[]),
        ("L7:189", Battery::L7, Folder::Dates, "lastweek", &[]),
        ("L7:190", Battery::L7, Folder::Dates, "prevweek", &["d2026nov"]),
        ("L7:191", Battery::L7, Folder::Dates, "previousweek", &["d2026nov"]),
        ("L7:192", Battery::L7, Folder::Dates, "prev2weeks", &["d2026nov"]),
        ("L7:193", Battery::L7, Folder::Dates, "thismonth", &["d2026nov"]),
        ("L7:194", Battery::L7, Folder::Dates, "lastmonth", &["d2026nov", "d2026oct"]),
        ("L7:195", Battery::L7, Folder::Dates, "last2months", &["d2026nov", "d2026oct"]),
        ("L7:196", Battery::L7, Folder::Dates, "past2months", &["d2026nov", "d2026oct"]),
        ("L7:197", Battery::L7, Folder::Dates, "pastmonth", &["d2026nov", "d2026oct"]),
        ("L7:198", Battery::L7, Folder::Dates, "lastyear", &["d2026nov", "d2026oct"]),
        ("L7:199", Battery::L7, Folder::Dates, "thisyear", &["d2026nov", "d2026oct"]),
        ("L7:200", Battery::L7, Folder::Dates, "nextweek", &[]),
        ("L7:201", Battery::L7, Folder::Dates, "comingweek", &[]),
        ("L7:202", Battery::L7, Folder::Dates, "next2days", &[]),
        ("L7:203", Battery::L7, Folder::Dates, "tomorrow", &[]),
        ("L7:204", Battery::L7, Folder::Dates, "now", &[]),
        ("L7:205", Battery::L7, Folder::Dates, "30days", &["d2026nov", "d2026oct"]),
        ("L7:206", Battery::L7, Folder::Dates, "last30days", &["d2026nov", "d2026oct"]),
        ("L7:207", Battery::L7, Folder::Dates, "3weeks", &["d2026nov"]),
        ("L7:208", Battery::L7, Folder::Dates, ">2020", &["d2021", "d2024may", "d2024wed", "d2026nov", "d2026oct"]),
        ("L7:209", Battery::L7, Folder::Dates, ">=2020", &["d2020a", "d2020b", "d2021", "d2024may", "d2024wed", "d2026nov", "d2026oct"]),
        ("L7:210", Battery::L7, Folder::Dates, "<2020", &["d1999"]),
        ("L7:211", Battery::L7, Folder::Dates, "<=2020", &["d1999", "d2020a", "d2020b"]),
        ("L7:212", Battery::L7, Folder::Dates, "2020..2021", &["d2020a", "d2020b", "d2021"]),
        ("L7:213", Battery::L7, Folder::Dates, "2020-2021", &["d2020a", "d2020b", "d2021"]),
        ("L7:214", Battery::L7, Folder::Dates, "!2020", &["d1999", "d2021", "d2024may", "d2024wed", "d2026nov", "d2026oct"]),
        ("L7:215", Battery::L7, Folder::Dates, "!=2020", &["d1999", "d2021", "d2024may", "d2024wed", "d2026nov", "d2026oct"]),
        ("L7:216", Battery::L7, Folder::Dates, "2020;2024", &["d2020a", "d2020b", "d2024may", "d2024wed"]),
        ("L7:223", Battery::L7, Folder::Dates, "unknown", &[]),
        ("L7:224", Battery::L7, Folder::Dates, "", &["d1999", "d2020a", "d2020b", "d2021", "d2024may", "d2024wed", "d2026nov", "d2026oct"]),
        ("L7:225", Battery::L7, Folder::Dates, "2020-1-15", &["d2020a"]),
        ("L7:226", Battery::L7, Folder::Dates, "2020-13", &["d2020a", "d2020b"]),
        ("L7:273", Battery::L7, Folder::D2, "50", &["y1950"]),
        ("L7:274", Battery::L7, Folder::D2, "49", &["y2049"]),
        ("L7:275", Battery::L7, Folder::D2, "30", &[]),
        ("L7:276", Battery::L7, Folder::D2, "29", &["y2029"]),
        ("L7:277", Battery::L7, Folder::D2, "jan", &["jan2026"]),
        ("L7:278", Battery::L7, Folder::D2, "january", &["jan2026"]),
        ("L7:279", Battery::L7, Folder::D2, "may", &[]),
        ("L7:280", Battery::L7, Folder::D2, "nov", &["fut10d", "fut3d", "mon1109", "rel2h", "rel6d12h", "rel7d1h", "sat1107", "sun1108", "thu1105", "wed1104"]),
        ("L7:281", Battery::L7, Folder::D2, "november", &["fut10d", "fut3d", "mon1109", "rel2h", "rel6d12h", "rel7d1h", "sat1107", "sun1108", "thu1105", "wed1104"]),
        ("L7:282", Battery::L7, Folder::D2, "oct", &["oct2026b", "rel29d", "rel32d"]),
        ("L7:283", Battery::L7, Folder::D2, "dec", &["dec2025"]),
        ("L7:285", Battery::L7, Folder::D2, "monday", &["mon1109"]),
        ("L7:286", Battery::L7, Folder::D2, "wed", &["wed1104"]),
        ("L7:287", Battery::L7, Folder::D2, "thu", &["thu1105"]),
        ("L7:288", Battery::L7, Folder::D2, "sat", &["sat1107"]),
        ("L7:289", Battery::L7, Folder::D2, "sun", &["sun1108"]),
        ("L7:290", Battery::L7, Folder::D2, "thisweek", &["fut3d", "mon1109", "rel2h", "sun1108"]),
        ("L7:291", Battery::L7, Folder::D2, "currentweek", &["fut3d", "mon1109", "rel2h", "sun1108"]),
        ("L7:292", Battery::L7, Folder::D2, "lastweek", &["fut10d", "fut3d", "mon1109", "rel2h", "rel6d12h", "sat1107", "sun1108", "thu1105", "wed1104", "y2029", "y2049"]),
        ("L7:293", Battery::L7, Folder::D2, "last1week", &["fut10d", "fut3d", "mon1109", "rel2h", "rel6d12h", "sat1107", "sun1108", "thu1105", "wed1104", "y2029", "y2049"]),
        ("L7:294", Battery::L7, Folder::D2, "1week", &["fut10d", "fut3d", "mon1109", "rel2h", "rel6d12h", "sat1107", "sun1108", "thu1105", "wed1104", "y2029", "y2049"]),
        ("L7:295", Battery::L7, Folder::D2, "pastweek", &["fut10d", "fut3d", "mon1109", "rel2h", "rel6d12h", "sat1107", "sun1108", "thu1105", "wed1104", "y2029", "y2049"]),
        ("L7:296", Battery::L7, Folder::D2, "prevweek", &["rel6d12h", "rel7d1h", "sat1107", "thu1105", "wed1104"]),
        ("L7:297", Battery::L7, Folder::D2, "previousweek", &["rel6d12h", "rel7d1h", "sat1107", "thu1105", "wed1104"]),
        ("L7:298", Battery::L7, Folder::D2, "prevmonth", &["oct2026b", "rel29d", "rel32d"]),
        ("L7:299", Battery::L7, Folder::D2, "previousmonth", &["oct2026b", "rel29d", "rel32d"]),
        ("L7:300", Battery::L7, Folder::D2, "prevyear", &["dec2025", "may2025"]),
        ("L7:301", Battery::L7, Folder::D2, "lastmonth", &["fut10d", "fut3d", "mon1109", "rel29d", "rel2h", "rel6d12h", "rel7d1h", "sat1107", "sun1108", "thu1105", "wed1104", "y2029", "y2049"]),
        ("L7:302", Battery::L7, Folder::D2, "pastmonth", &["fut10d", "fut3d", "mon1109", "rel29d", "rel2h", "rel6d12h", "rel7d1h", "sat1107", "sun1108", "thu1105", "wed1104", "y2029", "y2049"]),
        ("L7:303", Battery::L7, Folder::D2, "thismonth", &["fut10d", "fut3d", "mon1109", "rel2h", "rel6d12h", "rel7d1h", "sat1107", "sun1108", "thu1105", "wed1104"]),
        ("L7:304", Battery::L7, Folder::D2, "currentmonth", &["fut10d", "fut3d", "mon1109", "rel2h", "rel6d12h", "rel7d1h", "sat1107", "sun1108", "thu1105", "wed1104"]),
        ("L7:305", Battery::L7, Folder::D2, "lastyear", &["dec2025", "fut10d", "fut3d", "jan2026", "mon1109", "oct2026b", "rel29d", "rel2h", "rel32d", "rel6d12h", "rel7d1h", "sat1107", "sep2026", "sun1108", "thu1105", "wed1104", "y2029", "y2049"]),
        ("L7:306", Battery::L7, Folder::D2, "pastyear", &["dec2025", "fut10d", "fut3d", "jan2026", "mon1109", "oct2026b", "rel29d", "rel2h", "rel32d", "rel6d12h", "rel7d1h", "sat1107", "sep2026", "sun1108", "thu1105", "wed1104", "y2029", "y2049"]),
        ("L7:307", Battery::L7, Folder::D2, "thisyear", &["fut10d", "fut3d", "jan2026", "mon1109", "oct2026b", "rel29d", "rel2h", "rel32d", "rel6d12h", "rel7d1h", "sat1107", "sep2026", "sun1108", "thu1105", "wed1104"]),
        ("L7:308", Battery::L7, Folder::D2, "currentyear", &["fut10d", "fut3d", "jan2026", "mon1109", "oct2026b", "rel29d", "rel2h", "rel32d", "rel6d12h", "rel7d1h", "sat1107", "sep2026", "sun1108", "thu1105", "wed1104"]),
        ("L7:309", Battery::L7, Folder::D2, "prev2months", &["oct2026b", "rel29d", "rel32d", "sep2026"]),
        ("L7:310", Battery::L7, Folder::D2, "last2weeks", &["fut10d", "fut3d", "mon1109", "rel2h", "rel6d12h", "rel7d1h", "sat1107", "sun1108", "thu1105", "wed1104", "y2029", "y2049"]),
        ("L7:311", Battery::L7, Folder::D2, "7days", &["fut10d", "fut3d", "mon1109", "rel2h", "rel6d12h", "sat1107", "sun1108", "thu1105", "wed1104", "y2029", "y2049"]),
        ("L7:312", Battery::L7, Folder::D2, "today", &["rel2h"]),
        ("L7:313", Battery::L7, Folder::D2, "yesterday", &["mon1109"]),
        ("L7:314", Battery::L7, Folder::D2, "tomorrow", &[]),
        ("L7:315", Battery::L7, Folder::D2, "nextweek", &["fut10d"]),
        ("L7:316", Battery::L7, Folder::D2, "comingweek", &["fut10d"]),
        ("L7:317", Battery::L7, Folder::D2, "next5days", &["fut3d"]),
        ("L7:318", Battery::L7, Folder::D2, "coming5days", &["fut3d"]),
        ("L7:319", Battery::L7, Folder::D2, "nextmonth", &[]),
        ("L7:320", Battery::L7, Folder::D2, "now", &[]),
        ("L7:321", Battery::L7, Folder::D2, ">now", &["fut10d", "fut3d", "y2029", "y2049"]),
        ("L7:322", Battery::L7, Folder::D2, "<now", &["dec2025", "jan2026", "may2025", "mon1109", "oct2026b", "rel29d", "rel2h", "rel32d", "rel6d12h", "rel7d1h", "sat1107", "sep2026", "sun1108", "t1015", "thu1105", "wed1104", "y1930", "y1950"]),
        ("L7:323", Battery::L7, Folder::D2, ">today", &["fut10d", "fut3d", "y2029", "y2049"]),
        ("L7:324", Battery::L7, Folder::D2, ">=today", &["fut10d", "fut3d", "rel2h", "y2029", "y2049"]),
        ("L7:325", Battery::L7, Folder::D2, "<yesterday", &["dec2025", "jan2026", "may2025", "oct2026b", "rel29d", "rel32d", "rel6d12h", "rel7d1h", "sat1107", "sep2026", "sun1108", "t1015", "thu1105", "wed1104", "y1930", "y1950"]),
        ("L7:326", Battery::L7, Folder::D2, "2hours", &["fut10d", "fut3d", "y2029", "y2049"]),
        ("L7:327", Battery::L7, Folder::D2, "3hours", &["fut10d", "fut3d", "y2029", "y2049"]),
        ("L7:328", Battery::L7, Folder::D2, "1hour", &["fut10d", "fut3d", "y2029", "y2049"]),
        ("L7:329", Battery::L7, Folder::D2, "mtd", &["mon1109", "rel6d12h", "rel7d1h", "sat1107", "sun1108", "thu1105", "wed1104"]),
        ("L7:330", Battery::L7, Folder::D2, "qtd", &["mon1109", "oct2026b", "rel29d", "rel32d", "rel6d12h", "rel7d1h", "sat1107", "sun1108", "thu1105", "wed1104"]),
        ("L7:331", Battery::L7, Folder::D2, "ytd", &["jan2026", "mon1109", "oct2026b", "rel29d", "rel32d", "rel6d12h", "rel7d1h", "sat1107", "sep2026", "sun1108", "thu1105", "wed1104"]),
        ("L7:332", Battery::L7, Folder::D2, "lastweek..today", &["mon1109", "rel2h", "rel6d12h", "sat1107", "sun1108", "thu1105", "wed1104"]),
        ("L7:333", Battery::L7, Folder::D2, "2026-11-04..2026-11-08", &["sat1107", "sun1108", "thu1105", "wed1104"]),
        ("L7:334", Battery::L7, Folder::D2, "2026-11-08..2026-11-04", &["sat1107", "sun1108", "thu1105", "wed1104"]),
        ("L7:335", Battery::L7, Folder::D2, "2020-01-15T10:00..2020-01-15T10:20", &["t1015"]),
        ("L7:336", Battery::L7, Folder::D2, ">=2020-01-15T10:15", &["dec2025", "fut10d", "fut3d", "jan2026", "may2025", "mon1109", "oct2026b", "rel29d", "rel2h", "rel32d", "rel6d12h", "rel7d1h", "sat1107", "sep2026", "sun1108", "t1015", "thu1105", "wed1104", "y2029", "y2049"]),
        ("L7:337", Battery::L7, Folder::D2, "2020-01-15T10:15:00", &["t1015"]),
        ("L7:338", Battery::L7, Folder::D2, "11/2026", &["fut10d", "fut3d", "mon1109", "rel2h", "rel6d12h", "rel7d1h", "sat1107", "sun1108", "thu1105", "wed1104"]),
        ("L7:339", Battery::L7, Folder::D2, "2026/11", &["fut10d", "fut3d", "mon1109", "rel2h", "rel6d12h", "rel7d1h", "sat1107", "sun1108", "thu1105", "wed1104"]),
        ("L7:340", Battery::L7, Folder::D2, "11/4/2026", &["wed1104"]),
        ("L7:341", Battery::L7, Folder::D2, "4/11/2026", &[]),
        ("L7:342", Battery::L7, Folder::D2, "2026-11", &["fut10d", "fut3d", "mon1109", "rel2h", "rel6d12h", "rel7d1h", "sat1107", "sun1108", "thu1105", "wed1104"]),
        ("L7:343", Battery::L7, Folder::D2, "202611", &["fut10d", "fut3d", "mon1109", "rel2h", "rel6d12h", "rel7d1h", "sat1107", "sun1108", "thu1105", "wed1104"]),
        ("L7:344", Battery::L7, Folder::D2, "261104", &["wed1104"]),
        ("L7:345", Battery::L7, Folder::D2, "20261104", &["wed1104"]),
        ("L7:363", Battery::L7, Folder::Dates, "<2021", &["d1999", "d2020a", "d2020b"]),
        ("L7:364", Battery::L7, Folder::Dates, "<2021|2024>", &["d2021", "d2024may", "d2024wed"]),
        ("L7:411", Battery::L7, Folder::D2, "tue", &["rel2h"]),
        ("L7:412", Battery::L7, Folder::D2, "tuesday", &["rel2h"]),
        ("L7:536", Battery::L7, Folder::D2, "2026-11-04T09", &[]),
        ("L7:537", Battery::L7, Folder::D2, "2026-11-04T08", &[]),
        ("L7:538", Battery::L7, Folder::D2, "2026-11-04T09:00:00", &[]),
        ("L7:539", Battery::L7, Folder::D2, "2026-11-04T09:00", &[]),
        ("L7:540", Battery::L7, Folder::D2, "20261104T09", &[]),
        ("L7:541", Battery::L7, Folder::D2, "20261104T0900", &[]),
        ("L7:542", Battery::L7, Folder::D2, "11/4/26", &["wed1104"]),
        ("L7:543", Battery::L7, Folder::D2, "11/4", &["wed1104"]),
        ("L7:544", Battery::L7, Folder::D2, "2026/11/4", &["wed1104"]),
        ("L7:545", Battery::L7, Folder::D2, "2026/11/04", &["wed1104"]),
        ("L7:547", Battery::L7, Folder::D2, "2026-11-4", &["wed1104"]),
        ("L7:548", Battery::L7, Folder::D2, "2026-1-20", &["jan2026"]),
        ("L7:549", Battery::L7, Folder::D2, "last2years", &["dec2025", "fut10d", "fut3d", "jan2026", "may2025", "mon1109", "oct2026b", "rel29d", "rel2h", "rel32d", "rel6d12h", "rel7d1h", "sat1107", "sep2026", "sun1108", "thu1105", "wed1104", "y2029", "y2049"]),
        ("L7:550", Battery::L7, Folder::D2, "prev2years", &["dec2025", "may2025"]),
        ("L7:551", Battery::L7, Folder::D2, "previous2years", &["dec2025", "may2025"]),
        ("L7:552", Battery::L7, Folder::D2, "prev2weeks", &["rel6d12h", "rel7d1h", "sat1107", "thu1105", "wed1104"]),
        ("L7:553", Battery::L7, Folder::D2, "prev3days", &["mon1109", "sat1107", "sun1108"]),
        ("L7:554", Battery::L7, Folder::D2, "prevday", &["mon1109"]),
        ("L7:555", Battery::L7, Folder::D2, "next2weeks", &["fut10d"]),
        ("L7:556", Battery::L7, Folder::D2, "coming2months", &["fut10d", "fut3d"]),
        ("L7:557", Battery::L7, Folder::D2, "next1week", &["fut10d"]),
        ("L7:558", Battery::L7, Folder::D2, "nextyear", &[]),
        ("L7:559", Battery::L7, Folder::D2, "nextday", &[]),
        ("L7:560", Battery::L7, Folder::D2, "next2days", &[]),
        ("L7:561", Battery::L7, Folder::D2, "thisweek..today", &["mon1109", "rel2h", "sun1108"]),
        ("L7:563", Battery::L7, Folder::D2, "sep..oct", &["oct2026b", "rel29d", "rel32d", "sep2026"]),
        ("L7:564", Battery::L7, Folder::D2, "==today", &["rel2h"]),
        ("L7:565", Battery::L7, Folder::D2, "=today", &["rel2h"]),
        ("L7:569", Battery::L7, Folder::D2, "thisday", &["rel2h"]),
        ("L7:571", Battery::L7, Folder::D2, "lasthour", &["fut10d", "fut3d", "y2029", "y2049"]),
        ("L7:573", Battery::L7, Folder::D2, "<=yesterday", &["dec2025", "jan2026", "may2025", "mon1109", "oct2026b", "rel29d", "rel32d", "rel6d12h", "rel7d1h", "sat1107", "sep2026", "sun1108", "t1015", "thu1105", "wed1104", "y1930", "y1950"]),
        ("L7:575", Battery::L7, Folder::D2, ">sun", &["fut10d", "fut3d", "mon1109", "rel2h", "y2029", "y2049"]),
        ("L7:576", Battery::L7, Folder::D2, "<2026-11-05T00:00:00", &["dec2025", "jan2026", "may2025", "oct2026b", "rel29d", "rel32d", "rel6d12h", "rel7d1h", "sep2026", "t1015", "wed1104", "y1930", "y1950"]),
        ("L7:577", Battery::L7, Folder::D2, "134382564000000000", &["wed1104"]),
        ("L7:578", Battery::L7, Folder::D2, "0x1dd6c3adf39e800", &["wed1104"]),
        ("L7:579", Battery::L7, Folder::D2, ">=134382564000000000", &["fut10d", "fut3d", "mon1109", "rel2h", "sat1107", "sun1108", "thu1105", "wed1104", "y2029", "y2049"]),
        ("L7:580", Battery::L7, Folder::D2, "134382564000000000..134383428000000000", &["thu1105", "wed1104"]),
        ("L7:581", Battery::L7, Folder::D2, "2026-11-04..", &["fut10d", "fut3d", "mon1109", "rel2h", "sat1107", "sun1108", "thu1105", "wed1104", "y2029", "y2049"]),
        ("L7:582", Battery::L7, Folder::D2, "..2025", &["dec2025", "may2025", "t1015", "y1930", "y1950"]),
        ("L7:583", Battery::L7, Folder::D2, "!today", &["dec2025", "fut10d", "fut3d", "jan2026", "may2025", "mon1109", "oct2026b", "rel29d", "rel32d", "rel6d12h", "rel7d1h", "sat1107", "sep2026", "sun1108", "t1015", "thu1105", "wed1104", "y1930", "y1950", "y2029", "y2049"]),
        ("L7:584", Battery::L7, Folder::D2, "today;yesterday", &["mon1109", "rel2h"]),
        ("L7:586", Battery::L7, Folder::D2, "sun..sat", &["sat1107", "sun1108"]),
        ("L7:587", Battery::L7, Folder::D2, "dec..jan", &["dec2025", "jan2026"]),
        ("L7:588", Battery::L7, Folder::D2, "yesterday..tomorrow", &["mon1109", "rel2h"]),
        ("L7:589", Battery::L7, Folder::D2, "2d", &["fut10d", "fut3d", "mon1109", "rel2h", "y2029", "y2049"]),
        ("L7:590", Battery::L7, Folder::D2, "2w", &["fut10d", "fut3d", "mon1109", "rel2h", "rel6d12h", "rel7d1h", "sat1107", "sun1108", "thu1105", "wed1104", "y2029", "y2049"]),
        ("L7:591", Battery::L7, Folder::D2, "3h", &["fut10d", "fut3d", "y2029", "y2049"]),
        ("L7:592", Battery::L7, Folder::D2, "2y", &["dec2025", "fut10d", "fut3d", "jan2026", "may2025", "mon1109", "oct2026b", "rel29d", "rel2h", "rel32d", "rel6d12h", "rel7d1h", "sat1107", "sep2026", "sun1108", "thu1105", "wed1104", "y2029", "y2049"]),
        ("L7:593", Battery::L7, Folder::D2, "2mo", &["fut10d", "fut3d", "mon1109", "oct2026b", "rel29d", "rel2h", "rel32d", "rel6d12h", "rel7d1h", "sat1107", "sep2026", "sun1108", "thu1105", "wed1104", "y2029", "y2049"]),
        ("L7:594", Battery::L7, Folder::D2, "5min", &["fut10d", "fut3d", "y2029", "y2049"]),
        ("L7:595", Battery::L7, Folder::D2, "30sec", &["fut10d", "fut3d", "y2029", "y2049"]),
        ("L7:596", Battery::L7, Folder::D2, "2hrs", &["fut10d", "fut3d", "y2029", "y2049"]),
        ("L7:600", Battery::L7, Folder::D2, "1d..3d", &["mon1109", "sun1108"]),
        ("L7:601", Battery::L7, Folder::D2, "2026", &["fut10d", "fut3d", "jan2026", "mon1109", "oct2026b", "rel29d", "rel2h", "rel32d", "rel6d12h", "rel7d1h", "sat1107", "sep2026", "sun1108", "thu1105", "wed1104"]),
        ("L7:602", Battery::L7, Folder::D2, "2025-2026", &["dec2025", "fut10d", "fut3d", "jan2026", "may2025", "mon1109", "oct2026b", "rel29d", "rel2h", "rel32d", "rel6d12h", "rel7d1h", "sat1107", "sep2026", "sun1108", "thu1105", "wed1104"]),
        ("L7:603", Battery::L7, Folder::D2, "2025-12-15-2026-01-20", &["dec2025", "jan2026"]),
        ("L7:604", Battery::L7, Folder::D2, "may2025", &["may2025"]),
        ("L7:605", Battery::L7, Folder::D2, "31", &[]),
        ("L7:606", Battery::L7, Folder::D2, "dec2025", &["dec2025"]),
        ("L7:607", Battery::L7, Folder::D2, "dec-2025", &["dec2025"]),
        ("L7:608", Battery::L7, Folder::D2, "december", &["dec2025"]),
        ("L7:609", Battery::L7, Folder::D2, "thismonth..nextmonth", &["fut10d", "fut3d", "mon1109", "rel2h", "rel6d12h", "rel7d1h", "sat1107", "sun1108", "thu1105", "wed1104"]),
        ("L7:610", Battery::L7, Folder::D2, "jan..nov", &["fut10d", "fut3d", "jan2026", "mon1109", "oct2026b", "rel29d", "rel2h", "rel32d", "rel6d12h", "rel7d1h", "sat1107", "sep2026", "sun1108", "thu1105", "wed1104"]),
        ("L7:611", Battery::L7, Folder::D2, "nov..jan", &["fut10d", "fut3d", "jan2026", "mon1109", "oct2026b", "rel29d", "rel2h", "rel32d", "rel6d12h", "rel7d1h", "sat1107", "sep2026", "sun1108", "thu1105", "wed1104"]),
        ("L7:612", Battery::L7, Folder::D2, "mtd..today", &["mon1109", "rel2h", "rel6d12h", "rel7d1h", "sat1107", "sun1108", "thu1105", "wed1104"]),
        ("L7:613", Battery::L7, Folder::D2, "today..", &["fut10d", "fut3d", "rel2h", "y2029", "y2049"]),
        ("L7:614", Battery::L7, Folder::D2, "..yesterday", &["dec2025", "jan2026", "may2025", "mon1109", "oct2026b", "rel29d", "rel32d", "rel6d12h", "rel7d1h", "sat1107", "sep2026", "sun1108", "t1015", "thu1105", "wed1104", "y1930", "y1950"]),
        ("L7:615", Battery::L7, Folder::D2, ">2026-11", &["y2029", "y2049"]),
        ("L7:616", Battery::L7, Folder::D2, "<=2025", &["dec2025", "may2025", "t1015", "y1930", "y1950"]),
        ("L8:1", Battery::L8, Folder::D2, "2026-11-04T12", &["wed1104"]),
        ("L8:2", Battery::L8, Folder::D2, "2026-11-04T11", &[]),
        ("L8:3", Battery::L8, Folder::D2, "20261104T12", &["wed1104"]),
        ("L8:4", Battery::L8, Folder::D2, "20261104T1200", &["wed1104"]),
        ("L8:5", Battery::L8, Folder::D2, "2026-11-04T12:00:00", &["wed1104"]),
        ("L8:6", Battery::L8, Folder::D2, "2026-11-04T12:00", &["wed1104"]),
        ("L8:7", Battery::L8, Folder::D2, ">2days", &["dec2025", "jan2026", "may2025", "oct2026b", "rel29d", "rel32d", "rel6d12h", "rel7d1h", "sat1107", "sep2026", "sun1108", "t1015", "thu1105", "wed1104", "y1930", "y1950"]),
        ("L8:8", Battery::L8, Folder::D2, "<2days", &["fut10d", "fut3d", "mon1109", "rel2h", "y2029", "y2049"]),
        ("L8:9", Battery::L8, Folder::D2, ">=7days", &["dec2025", "jan2026", "may2025", "oct2026b", "rel29d", "rel32d", "rel7d1h", "sep2026", "t1015", "y1930", "y1950"]),
        ("L8:10", Battery::L8, Folder::D2, "<=1week", &["fut10d", "fut3d", "mon1109", "rel2h", "rel6d12h", "sat1107", "sun1108", "thu1105", "wed1104", "y2029", "y2049"]),
        ("L8:11", Battery::L8, Folder::D2, ">1hour", &["dec2025", "jan2026", "may2025", "mon1109", "oct2026b", "rel29d", "rel2h", "rel32d", "rel6d12h", "rel7d1h", "sat1107", "sep2026", "sun1108", "t1015", "thu1105", "wed1104", "y1930", "y1950"]),
        ("L8:12", Battery::L8, Folder::D2, "<1hour", &["fut10d", "fut3d", "y2029", "y2049"]),
        ("L8:13", Battery::L8, Folder::D2, "=2days", &[]),
        ("L8:14", Battery::L8, Folder::D2, "!2days", &["dec2025", "jan2026", "may2025", "oct2026b", "rel29d", "rel32d", "rel6d12h", "rel7d1h", "sat1107", "sep2026", "sun1108", "t1015", "thu1105", "wed1104", "y1930", "y1950"]),
        ("L8:15", Battery::L8, Folder::D2, "2days..7days", &["rel6d12h", "sat1107", "sun1108", "thu1105", "wed1104"]),
        ("L8:16", Battery::L8, Folder::D2, ">last2days", &["dec2025", "jan2026", "may2025", "oct2026b", "rel29d", "rel32d", "rel6d12h", "rel7d1h", "sat1107", "sep2026", "sun1108", "t1015", "thu1105", "wed1104", "y1930", "y1950"]),
        ("L8:17", Battery::L8, Folder::D2, "<last2days", &["fut10d", "fut3d", "mon1109", "rel2h", "y2029", "y2049"]),
        ("L8:18", Battery::L8, Folder::D2, ">past2days", &["dec2025", "jan2026", "may2025", "oct2026b", "rel29d", "rel32d", "rel6d12h", "rel7d1h", "sat1107", "sep2026", "sun1108", "t1015", "thu1105", "wed1104", "y1930", "y1950"]),
        ("L8:19", Battery::L8, Folder::D2, "next2months", &[]),
        ("L8:20", Battery::L8, Folder::D2, "coming2weeks", &["fut10d", "fut3d"]),
        ("L8:21", Battery::L8, Folder::D2, "coming3days", &["fut3d"]),
        ("L8:22", Battery::L8, Folder::D2, "next3days", &["fut3d"]),
        ("L8:23", Battery::L8, Folder::D2, "comingmonth", &[]),
        ("L8:24", Battery::L8, Folder::D2, "coming1month", &["fut10d", "fut3d"]),
        ("L8:25", Battery::L8, Folder::D2, "next1month", &[]),
        ("L8:26", Battery::L8, Folder::D2, "coming1week", &["fut3d"]),
        ("L8:27", Battery::L8, Folder::D2, "coming2days", &[]),
        ("L8:28", Battery::L8, Folder::D2, "coming1day", &[]),
        ("L8:29", Battery::L8, Folder::D2, "comingday", &[]),
        ("L8:30", Battery::L8, Folder::D2, "comingyear", &[]),
        ("L8:31", Battery::L8, Folder::D2, "coming2years", &["fut10d", "fut3d"]),
        ("L8:32", Battery::L8, Folder::D2, "next2years", &[]),
        ("L8:33", Battery::L8, Folder::D2, "30s", &["fut10d", "fut3d", "y2029", "y2049"]),
        ("L8:34", Battery::L8, Folder::D2, "2m", &["fut10d", "fut3d", "y2029", "y2049"]),
        ("L8:40", Battery::L8, Folder::D2, "2secs", &["fut10d", "fut3d", "y2029", "y2049"]),
        ("L8:41", Battery::L8, Folder::D2, "3hours", &["fut10d", "fut3d", "y2029", "y2049"]),
        ("L8:42", Battery::L8, Folder::D2, "2h", &["fut10d", "fut3d", "y2029", "y2049"]),
        ("L8:43", Battery::L8, Folder::D2, "3h", &["fut10d", "fut3d", "y2029", "y2049"]),
        ("L8:44", Battery::L8, Folder::D2, "12h", &["fut10d", "fut3d", "rel2h", "y2029", "y2049"]),
        ("L8:47", Battery::L8, Folder::D2, "previousday", &["mon1109"]),
        ("L8:48", Battery::L8, Folder::D2, "lastday", &["fut10d", "fut3d", "rel2h", "y2029", "y2049"]),
        ("L8:49", Battery::L8, Folder::D2, "pastday", &["fut10d", "fut3d", "rel2h", "y2029", "y2049"]),
        ("L8:50", Battery::L8, Folder::D2, "last1day", &["fut10d", "fut3d", "rel2h", "y2029", "y2049"]),
        ("L8:51", Battery::L8, Folder::D2, "1day", &["fut10d", "fut3d", "rel2h", "y2029", "y2049"]),
        ("L8:52", Battery::L8, Folder::D2, "prev1week", &["rel6d12h", "rel7d1h", "sat1107", "thu1105", "wed1104"]),
        ("L8:53", Battery::L8, Folder::D2, "prevmonth", &["oct2026b", "rel29d", "rel32d"]),
        ("L8:54", Battery::L8, Folder::D2, "prev1month", &["oct2026b", "rel29d", "rel32d"]),
        ("L8:55", Battery::L8, Folder::D2, "nextweek..next2weeks", &["fut10d"]),
        ("L8:56", Battery::L8, Folder::D2, "tue", &["rel2h"]),
        ("L8:58", Battery::L8, Folder::D2, "monday", &["mon1109"]),
        ("L8:59", Battery::L8, Folder::D2, "tues", &["rel2h"]),
        ("L8:60", Battery::L8, Folder::D2, "thur", &["thu1105"]),
        ("L8:61", Battery::L8, Folder::D2, "thurs", &["thu1105"]),
        ("L8:63", Battery::L8, Folder::D2, "4/11", &[]),
        ("L8:64", Battery::L8, Folder::D2, "11/04/2026", &["wed1104"]),
        ("L8:65", Battery::L8, Folder::D2, "2026.11.04", &["wed1104"]),
        ("L8:66", Battery::L8, Folder::D2, "04.11.2026", &[]),
        ("L8:67", Battery::L8, Folder::D2, "2026-11-04..2026-11-05", &["thu1105", "wed1104"]),
        ("L8:68", Battery::L8, Folder::D2, "2026-11-04-2026-11-05", &["thu1105", "wed1104"]),
        ("L8:69", Battery::L8, Folder::D2, "20261104..20261105", &["thu1105", "wed1104"]),
        ("L8:70", Battery::L8, Folder::D2, "nov2026", &["fut10d", "fut3d", "mon1109", "rel2h", "rel6d12h", "rel7d1h", "sat1107", "sun1108", "thu1105", "wed1104"]),
        ("L8:71", Battery::L8, Folder::D2, "november2026", &["fut10d", "fut3d", "mon1109", "rel2h", "rel6d12h", "rel7d1h", "sat1107", "sun1108", "thu1105", "wed1104"]),
        ("L8:72", Battery::L8, Folder::D2, "2026nov", &["fut10d", "fut3d", "mon1109", "rel2h", "rel6d12h", "rel7d1h", "sat1107", "sun1108", "thu1105", "wed1104"]),
        ("L8:73", Battery::L8, Folder::D2, "jan2026..mar2026", &["jan2026"]),
        ("L8:74", Battery::L8, Folder::D2, "yesterday;today", &["mon1109", "rel2h"]),
        ("L8:76", Battery::L8, Folder::D2, "ytd", &["jan2026", "mon1109", "oct2026b", "rel29d", "rel32d", "rel6d12h", "rel7d1h", "sat1107", "sep2026", "sun1108", "thu1105", "wed1104"]),
        ("L8:77", Battery::L8, Folder::D2, "qtd", &["mon1109", "oct2026b", "rel29d", "rel32d", "rel6d12h", "rel7d1h", "sat1107", "sun1108", "thu1105", "wed1104"]),
        ("L8:78", Battery::L8, Folder::D2, "ytd..", &["fut10d", "fut3d", "jan2026", "mon1109", "oct2026b", "rel29d", "rel2h", "rel32d", "rel6d12h", "rel7d1h", "sat1107", "sep2026", "sun1108", "thu1105", "wed1104", "y2029", "y2049"]),
        ("L8:79", Battery::L8, Folder::D2, "unknown", &[]),
        ("L8:80", Battery::L8, Folder::D2, "", &["dec2025", "fut10d", "fut3d", "jan2026", "may2025", "mon1109", "oct2026b", "rel29d", "rel2h", "rel32d", "rel6d12h", "rel7d1h", "sat1107", "sep2026", "sun1108", "t1015", "thu1105", "wed1104", "y1930", "y1950", "y2029", "y2049"]),
    ];

    /// Values a live Everything answers with nothing, as no date, which goz
    /// refuses.
    #[rustfmt::skip]
    const REFUSED: &[&str] = &["15/1/2020", "*-05", "*-*-*T10:00..*-*-*T13:00", "43845", "soon", "2026-W45", "26-11-04", "thisquarter", "lastquarter", "prevquarter", "thishour", "thisminute", "2hr", "2wk", "2yr", "2mos", "2ds", "2ws", "2ys", "2hs", "prevhour", "sept"];
}
