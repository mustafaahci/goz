//! `dm:` values: a date as a query writes it, resolved to the FILETIME
//! bounds the engine compares an entry's date modified against.
//!
//! A calendar value is the period between two local midnights: `today`,
//! `yesterday`, `thisweek` and `lastweek`, `thismonth` and `lastmonth`,
//! `thisyear` and `lastyear`, a year `2024`, a month `2024-05` or a day
//! `2024-05-01`. A day that holds a daylight-saving change therefore lasts 23
//! or 25 hours. A duration such as `7days` or `last2hours` names the instant
//! that long before the query starts, and on its own matches from that
//! instant on, as Everything's `dm:2days` matches "2 days ago or later".
//! Seconds, minutes, hours, days and weeks have a fixed length; months and
//! years step back on the local calendar.
//!
//! A comparison compares times at the value's granularity: `>2024` is after
//! the end of 2024, `<=today` up to the end of today, `<30days` older than 30
//! days. A range `lower..upper` runs from the start of `lower` to the end of
//! `upper`, either side optional. An entry whose date is unknown matches no
//! date, and `unknown` matches only such entries.
//!
//! The calendar comes from a [`LocalClock`]. gozd parses the query, so the
//! clock is the daemon's; a Windows time zone belongs to the machine, so it is
//! every user's too.

use super::parse::QueryError;
use crate::output::filetime::{DAYS_1601_TO_1970, civil_from_days, days_from_civil};

/// FILETIME ticks in a second.
const SECOND: i64 = 10_000_000;
/// FILETIME ticks in a day of the wall clock.
const DAY: i64 = 86_400 * SECOND;

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
    /// The instant a duration that ends at the query start begins.
    Instant(i64),
}

impl Value {
    fn first(self) -> i64 {
        match self {
            Value::Period { start, .. } => start,
            Value::Instant(t) => t,
        }
    }

    fn last(self) -> i64 {
        match self {
            Value::Period { end, .. } => end - 1,
            Value::Instant(t) => t,
        }
    }
}

fn malformed(detail: String) -> QueryError {
    QueryError::Malformed {
        what: "dm:",
        detail,
    }
}

/// The filter a `dm:` value stands for, read against `clock`.
pub(crate) fn date_filter(value: &str, clock: &dyn LocalClock) -> Result<DateFilter, QueryError> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("unknown") {
        return Ok(DateFilter::Unknown);
    }
    let within = |min, max| DateFilter::Within { min, max };

    if let Some((lower, upper)) = value.split_once("..") {
        let side = |text: &str| {
            let text = text.trim();
            (!text.is_empty()).then(|| point(text, clock)).transpose()
        };
        let min = side(lower)?.map_or(i64::MIN, Value::first);
        let max = side(upper)?.map_or(i64::MAX, Value::last);
        if min > max {
            return Err(malformed(format!(
                "{value} runs backwards; write the earlier date first"
            )));
        }
        return Ok(within(min, max));
    }

    // Two-character operators before their one-character prefixes.
    for op in [">=", "<=", ">", "<", "="] {
        if let Some(rest) = value.strip_prefix(op) {
            let v = point(rest, clock)?;
            return Ok(match op {
                ">=" => within(v.first(), i64::MAX),
                ">" => within(v.last().saturating_add(1), i64::MAX),
                "<=" => within(i64::MIN, v.last()),
                "<" => within(i64::MIN, v.first().saturating_sub(1)),
                // `=` is the value itself.
                _ => alone(v),
            });
        }
    }
    Ok(alone(point(value, clock)?))
}

/// The filter of a value with no operator: a period's own ticks, or from a
/// duration's start on, so a file dated in the future by a skewed clock or
/// an unpacked archive still counts as recent.
fn alone(v: Value) -> DateFilter {
    match v {
        Value::Period { start, end } => DateFilter::Within {
            min: start,
            max: end - 1,
        },
        Value::Instant(t) => DateFilter::Within {
            min: t,
            max: i64::MAX,
        },
    }
}

/// The period or instant one value names.
fn point(text: &str, clock: &dyn LocalClock) -> Result<Value, QueryError> {
    let text = text.trim();
    let lower = text.to_ascii_lowercase();
    let out_of_range = || malformed(format!("{text} is outside the dates goz can compare"));
    let now = clock.now();
    // Today's local day: days since 1601-01-01 on the wall clock, and the
    // time of day into it.
    let local_now = || {
        let local = clock.to_local(now).ok_or_else(out_of_range)?;
        Ok::<_, QueryError>((local.div_euclid(DAY), local.rem_euclid(DAY)))
    };

    if let Some(duration) = duration(&lower, text)? {
        let start = match duration {
            Duration::Ticks(ticks) => now.checked_sub(ticks),
            Duration::Months(months) => {
                let (today, time_of_day) = local_now()?;
                let (year, month, day) = civil(today);
                let (year, month) = add_months(year, month, -months).ok_or_else(out_of_range)?;
                local_day(year, month, day.min(days_in_month(year, month)))
                    .and_then(|d| d.checked_mul(DAY))
                    .and_then(|t| t.checked_add(time_of_day))
                    .and_then(|t| clock.to_utc(t))
            }
        };
        return start.map(Value::Instant).ok_or_else(out_of_range);
    }

    let (first, end) = match lower.as_str() {
        "today" => {
            let today = local_now()?.0;
            (today, today + 1)
        }
        "yesterday" => {
            let today = local_now()?.0;
            (today - 1, today)
        }
        _ => {
            let calendar = [("this", 0), ("last", -1)]
                .into_iter()
                .find_map(|(prefix, offset)| Some((lower.strip_prefix(prefix)?, offset)));
            match calendar {
                Some((unit, offset)) if matches!(unit, "week" | "month" | "year") => {
                    let today = local_now()?.0;
                    calendar_period(unit, offset, today, clock.week_start())
                        .ok_or_else(out_of_range)?
                }
                _ => written_date(&lower).ok_or_else(|| {
                    malformed(format!(
                        "{text} is not a date; use today, yesterday, thisweek, lastmonth, \
                         thisyear, a count such as 7days or last2hours, YYYY, YYYY-MM, \
                         YYYY-MM-DD, or unknown"
                    ))
                })??,
            }
        }
    };
    // A local midnight, as UTC ticks. FILETIME starts in 1601.
    let midnight = |day: i64| {
        day.checked_mul(DAY)
            .filter(|&t| t >= 0)
            .and_then(|t| clock.to_utc(t))
            .ok_or_else(out_of_range)
    };
    Ok(Value::Period {
        start: midnight(first)?,
        end: midnight(end)?,
    })
}

/// How far back a duration reaches.
enum Duration {
    /// A fixed length.
    Ticks(i64),
    /// Calendar months, which differ in length.
    Months(i64),
}

/// The duration `lower` writes, such as `7days`, `last2hours` or `3months`:
/// `None` when it is no duration, an error when its count is not above 0 or
/// its length is too long to compare.
fn duration(lower: &str, text: &str) -> Result<Option<Duration>, QueryError> {
    let rest = lower.strip_prefix("last").unwrap_or(lower);
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let (count, unit) = rest.split_at(digits);
    let unit = unit.strip_suffix('s').unwrap_or(unit);
    let seconds = match unit {
        "sec" | "second" => Some(1),
        "min" | "minute" => Some(60),
        "hour" => Some(3_600),
        "day" => Some(86_400),
        "week" => Some(604_800),
        "month" | "year" => None,
        _ => return Ok(None),
    };
    if count.is_empty() {
        return Ok(None);
    }
    let too_long = || malformed(format!("{text} reaches back further than goz can compare"));
    let count = count.parse::<i64>().map_err(|_| too_long())?;
    if count == 0 {
        return Err(malformed(format!("{text}: give a count above 0")));
    }
    Ok(Some(match seconds {
        Some(seconds) => Duration::Ticks(count.checked_mul(seconds * SECOND).ok_or_else(too_long)?),
        None if unit == "year" => Duration::Months(count.checked_mul(12).ok_or_else(too_long)?),
        None => Duration::Months(count),
    }))
}

/// The local days `[first, end)` of this (`offset` 0) or last (`offset` -1)
/// week, month or year around local day `today`.
fn calendar_period(unit: &str, offset: i64, today: i64, week_start: u8) -> Option<(i64, i64)> {
    match unit {
        "week" => {
            // 1601-01-01 was a Monday, so day 0 is one.
            let into_week = (today - i64::from(week_start)).rem_euclid(7);
            let first = today - into_week + 7 * offset;
            Some((first, first + 7))
        }
        "month" => {
            let (year, month, _) = civil(today);
            let (year, month) = add_months(year, month, offset)?;
            let (next_year, next_month) = add_months(year, month, 1)?;
            Some((
                local_day(year, month, 1)?,
                local_day(next_year, next_month, 1)?,
            ))
        }
        _ => {
            let year = civil(today).0 + offset;
            Some((local_day(year, 1, 1)?, local_day(year + 1, 1, 1)?))
        }
    }
}

/// The local days `[first, end)` that `YYYY`, `YYYY-MM` or `YYYY-MM-DD`
/// writes: `None` when `lower` has none of those shapes, an error when it has
/// one but names no real month or day.
fn written_date(lower: &str) -> Option<Result<(i64, i64), QueryError>> {
    let mut parts = lower.split('-');
    let number = |part: Option<&str>, len: usize| {
        part.filter(|p| p.len() == len && p.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|p| p.parse::<u32>().ok())
    };
    let year = i64::from(number(parts.next(), 4)?);
    let month = parts.next().map(|p| number(Some(p), 2));
    let day = parts.next().map(|p| number(Some(p), 2));
    if parts.next().is_some() {
        return None;
    }
    let invalid = || Some(Err(malformed(format!("{lower} is not a real date"))));
    let (first, end) = match (month, day) {
        (None, _) => (local_day(year, 1, 1)?, local_day(year + 1, 1, 1)?),
        (Some(None), _) | (_, Some(None)) => return None,
        (Some(Some(month)), None) => {
            if !(1..=12).contains(&month) {
                return invalid();
            }
            let (next_year, next_month) = add_months(year, month, 1)?;
            (
                local_day(year, month, 1)?,
                local_day(next_year, next_month, 1)?,
            )
        }
        (Some(Some(month)), Some(Some(day))) => {
            if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
                return invalid();
            }
            let first = local_day(year, month, day)?;
            (first, first + 1)
        }
    };
    Some(Ok((first, end)))
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
    }

    impl FixedClock {
        /// UTC, at `now`, with weeks from Monday.
        pub(crate) fn utc(now: i64) -> Self {
            FixedClock {
                now,
                offset: 0,
                week_start: 0,
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
    }

    const HOUR: i64 = 3_600 * SECOND;

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
        };
        let today = filter("today", &clock);
        assert_eq!(today, within("2026-11-03T21:00:00", "2026-11-04T21:00:00"));
        assert_eq!(filter("2026-11-04", &clock), today);
        assert_eq!(filter("TODAY", &clock), today);
        let midnight = ft("2026-11-03T21:00:00");
        assert!(today.matches(Some(midnight)));
        assert!(!today.matches(Some(midnight - 1)));
        assert!(filter("yesterday", &clock).matches(Some(midnight - 1)));
    }

    /// The day of a daylight-saving change lasts 23 or 25 hours, and so do
    /// the weeks that hold one, while a duration stays its fixed length.
    #[test]
    fn a_daylight_saving_change_moves_midnight_but_not_a_duration() {
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
            filter("lastweek", &clock),
            within("2026-03-02T05:00:00", "2026-03-09T04:00:00")
        );
        assert_eq!(
            filter("7days", &clock),
            DateFilter::Within {
                min: ft("2026-03-03T12:00:00"),
                max: i64::MAX
            }
        );
    }

    /// A week starts on the machine's first day of the week, and the
    /// calendar words cross month and year ends.
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
        assert_eq!(
            filter("lastweek", &clock),
            within("2026-10-11T00:00:00", "2026-10-18T00:00:00")
        );
        let clock = FixedClock::utc(ft("2024-01-15T12:00:00"));
        for (value, from, until) in [
            ("lastmonth", "2023-12-01T00:00:00", "2024-01-01T00:00:00"),
            ("thismonth", "2024-01-01T00:00:00", "2024-02-01T00:00:00"),
            ("lastyear", "2023-01-01T00:00:00", "2024-01-01T00:00:00"),
            ("thisyear", "2024-01-01T00:00:00", "2025-01-01T00:00:00"),
            ("2024", "2024-01-01T00:00:00", "2025-01-01T00:00:00"),
            ("2024-02", "2024-02-01T00:00:00", "2024-03-01T00:00:00"),
            ("2024-12", "2024-12-01T00:00:00", "2025-01-01T00:00:00"),
            ("2024-02-29", "2024-02-29T00:00:00", "2024-03-01T00:00:00"),
        ] {
            assert_eq!(filter(value, &clock), within(from, until), "{value}");
        }
    }

    /// A duration reaches back from the query start and, alone, matches
    /// from there on, the future included. Months and years step back on
    /// the calendar, landing on the month's last day when it is shorter.
    #[test]
    fn a_duration_reaches_back_from_the_query_start() {
        let clock = FixedClock::utc(ft("2026-03-31T12:00:00"));
        let from = |at: &str| DateFilter::Within {
            min: ft(at),
            max: i64::MAX,
        };
        assert_eq!(filter("7days", &clock), from("2026-03-24T12:00:00"));
        assert_eq!(filter("last7days", &clock), from("2026-03-24T12:00:00"));
        assert_eq!(filter("1week", &clock), from("2026-03-24T12:00:00"));
        assert_eq!(filter("last2hours", &clock), from("2026-03-31T10:00:00"));
        assert_eq!(filter("10mins", &clock), from("2026-03-31T11:50:00"));
        assert_eq!(filter("30secs", &clock), from("2026-03-31T11:59:30"));
        assert_eq!(filter("1month", &clock), from("2026-02-28T12:00:00"));
        assert_eq!(filter("last13months", &clock), from("2025-02-28T12:00:00"));
        assert_eq!(filter("2years", &clock), from("2024-03-31T12:00:00"));
        assert!(filter("7days", &clock).matches(Some(ft("2027-01-01T00:00:00"))));
        assert_eq!(
            filter("<30days", &clock),
            DateFilter::Within {
                min: i64::MIN,
                max: ft("2026-03-01T12:00:00") - 1
            }
        );
        assert_eq!(
            filter("30days..7days", &clock),
            DateFilter::Within {
                min: ft("2026-03-01T12:00:00"),
                max: ft("2026-03-24T12:00:00")
            }
        );
    }

    /// A comparison compares times at the value's granularity, and a range
    /// runs from the start of its lower value to the end of its upper one.
    #[test]
    fn comparisons_and_ranges_bound_by_whole_periods() {
        let clock = FixedClock::utc(ft("2026-10-18T12:00:00"));
        let bounds = |value: &str| match filter(value, &clock) {
            DateFilter::Within { min, max } => (min, max),
            DateFilter::Unknown => panic!("{value}"),
        };
        let at = ft;
        assert_eq!(bounds(">2024"), (at("2025-01-01T00:00:00"), i64::MAX));
        assert_eq!(
            bounds(">=2024-01-01"),
            (at("2024-01-01T00:00:00"), i64::MAX)
        );
        assert_eq!(
            bounds("<2024-05"),
            (i64::MIN, at("2024-05-01T00:00:00") - 1)
        );
        assert_eq!(bounds("<=today"), (i64::MIN, at("2026-10-19T00:00:00") - 1));
        assert_eq!(filter("=yesterday", &clock), filter("yesterday", &clock));
        assert_eq!(
            filter("2024-01-01..2024-03-31", &clock),
            within("2024-01-01T00:00:00", "2024-04-01T00:00:00")
        );
        assert_eq!(bounds("2024-05.."), (at("2024-05-01T00:00:00"), i64::MAX));
        assert_eq!(bounds("..2024"), (i64::MIN, at("2025-01-01T00:00:00") - 1));
        assert_eq!(
            bounds(" 2024 .. 2025 "),
            (at("2024-01-01T00:00:00"), at("2026-01-01T00:00:00") - 1)
        );
    }

    /// An unknown date matches only `unknown`, never a date or its
    /// complement, and two `dm:` terms must both pass.
    #[test]
    fn unknown_dates_match_only_unknown() {
        let clock = FixedClock::utc(ft("2026-10-18T12:00:00"));
        let date = Some(ft("2026-10-18T08:00:00"));
        for value in ["today", "<today", ">=today", "..", "1day"] {
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
    /// and so is a range that runs backwards or a date FILETIME cannot hold.
    #[test]
    fn a_value_that_names_no_date_is_refused() {
        let clock = FixedClock::utc(ft("2026-10-18T12:00:00"));
        for value in [
            "",
            "2026-02-30",
            "2026-13",
            "2026-00-01",
            "26-01-01",
            "2026-1-1",
            "2026-01-01-01",
            "10/18/2026",
            "0days",
            "lastdays",
            "days",
            "+5days",
            "7fortnights",
            "nextweek",
            "thisday",
            ">unknown",
            "unknown..",
            "2026..2024",
            "7days..30days",
            "1500",
            "99999999999999999999weeks",
            "9999999999999999years",
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
    }

    /// The clock is read from the pipe's side, so any value, against the
    /// earliest or latest clock, fails or resolves without overflowing.
    #[test]
    fn extreme_clocks_never_overflow() {
        for now in [i64::MIN, -1, 0, i64::MAX] {
            for offset in [-14 * HOUR, 0, 14 * HOUR] {
                let clock = FixedClock {
                    now,
                    offset,
                    week_start: 3,
                };
                for value in [
                    "today",
                    "yesterday",
                    "thisweek",
                    "lastweek",
                    "thismonth",
                    "lastmonth",
                    "thisyear",
                    "lastyear",
                    "9999",
                    "1601-01-01",
                    "4294967295weeks",
                    "9223372036854775807secs",
                    "last100years",
                    "<today",
                    ">today",
                    "today..",
                ] {
                    let _ = date_filter(value, &clock);
                }
            }
        }
    }
}
