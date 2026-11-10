//! The clock a query's `dm:` dates resolve against.

use std::time::{SystemTime, UNIX_EPOCH};

use goz_core::output::FILETIME_UNIX_EPOCH;
use goz_core::query::{DateOrder, LocalClock};
use goz_winfs::TimeZone;

/// The machine's clock and calendar as a query arrives: the time, the time
/// zone with its daylight-saving rules for every year, the first day of the
/// week and the order of a short date. A Windows time zone belongs to the
/// machine, so the daemon's is every user's. The week start and the date
/// order are the daemon's regional format: under the service, the system
/// account's (see [`goz_winfs::first_day_of_week`]).
pub(crate) struct MachineClock {
    now: i64,
    /// `None` when the zone cannot be read, so calendar dates are refused
    /// while durations still work.
    zone: Option<TimeZone>,
    week_start: u8,
    date_order: DateOrder,
}

impl MachineClock {
    /// Reads the clock: about a microsecond and a half, so every query reads
    /// it whether it has a `dm:` term or not.
    pub(crate) fn read() -> Self {
        let zone = TimeZone::current()
            .inspect_err(|e| tracing::debug!(error = %e, "time zone unreadable; dm: dates refused"))
            .ok();
        Self {
            now: now_filetime(),
            zone,
            week_start: goz_winfs::first_day_of_week(),
            date_order: match goz_winfs::short_date_order() {
                1 => DateOrder::DayMonthYear,
                2 => DateOrder::YearMonthDay,
                _ => DateOrder::MonthDayYear,
            },
        }
    }
}

impl LocalClock for MachineClock {
    fn now(&self) -> i64 {
        self.now
    }
    fn to_local(&self, utc: i64) -> Option<i64> {
        self.zone?.local_from_utc(utc)
    }
    fn to_utc(&self, local: i64) -> Option<i64> {
        self.zone?.utc_from_local(local)
    }
    fn week_start(&self) -> u8 {
        self.week_start
    }
    fn date_order(&self) -> DateOrder {
        self.date_order
    }
}

/// The system time as a FILETIME.
fn now_filetime() -> i64 {
    let ticks = |d: std::time::Duration| i64::try_from(d.as_nanos() / 100).unwrap_or(i64::MAX);
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(since) => FILETIME_UNIX_EPOCH.saturating_add(ticks(since)),
        Err(before) => FILETIME_UNIX_EPOCH.saturating_sub(ticks(before.duration())),
    }
}
