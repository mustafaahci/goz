//! FILETIME conversion and formatting helpers.
//!
//! Windows FILETIME counts 100-nanosecond ticks since 1601-01-01T00:00:00
//! UTC. Everything's IPC, the USN/MFT records, and goz's own wire protocol all speak
//! FILETIME; these helpers are the single conversion point for the client-side
//! formatting and date arithmetic that need Unix milliseconds.
//!
//! All functions are total: negative ticks (pre-1601) and pre-1970 inputs
//! never panic. Conversions use floor (Euclidean) division, so pre-epoch
//! values map to the correct earlier millisecond/date rather than rounding
//! toward zero; only at the extreme ends of the `i64` domain (where the
//! result would not be representable) do the linear conversions saturate.

/// Ticks (100 ns units) from 1601-01-01T00:00:00 UTC to 1970-01-01T00:00:00
/// UTC, the offset between the FILETIME epoch and the Unix epoch.
pub const FILETIME_UNIX_EPOCH: i64 = 116_444_736_000_000_000;

/// Whole days from 1601-01-01 to 1970-01-01 (the same offset in days:
/// `FILETIME_UNIX_EPOCH / 10_000_000 / 86_400`).
pub(crate) const DAYS_1601_TO_1970: i64 = 134_774;

/// Converts a FILETIME tick count to Unix milliseconds.
///
/// Uses floor division, so pre-1970 inputs yield the correct (more negative)
/// millisecond, not a value rounded toward zero. Saturates instead of
/// overflowing for ticks within `FILETIME_UNIX_EPOCH` of `i64::MIN`.
pub fn filetime_to_unix_ms(ft: i64) -> i64 {
    ft.saturating_sub(FILETIME_UNIX_EPOCH).div_euclid(10_000)
}

/// Converts Unix milliseconds to a FILETIME tick count.
///
/// Exact for every representable result. When the millisecond value lies
/// outside the FILETIME domain (beyond roughly the year 31 million either
/// way) the tick multiplication saturates instead of overflowing: the high
/// corner clamps to `i64::MAX`, the low corner to
/// `i64::MIN + FILETIME_UNIX_EPOCH` (the epoch offset is added after the
/// saturated multiply).
pub fn unix_ms_to_filetime(ms: i64) -> i64 {
    ms.saturating_mul(10_000)
        .saturating_add(FILETIME_UNIX_EPOCH)
}

/// Civil UTC `YYYY-MM-DDTHH:MM:SS` from a FILETIME (es `-date-format 1`
/// shape, but UTC).
///
/// Pure days-since-epoch math, no timezone database. The CLI formats LOCAL
/// time with `jiff`; this is the UTC fallback and the test surface for the
/// calendar math. Sub-second ticks are truncated (floored), so pre-1601
/// inputs render the correct earlier proleptic-Gregorian date. Years outside
/// `0000..=9999` render with more digits (and a leading `-` before year 0);
/// the entire `i64` tick domain formats without panicking.
pub fn format_filetime_utc(ft: i64) -> String {
    let secs = ft.div_euclid(10_000_000);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days - DAYS_1601_TO_1970);
    let (hh, mm, ss) = (tod / 3_600, (tod % 3_600) / 60, tod % 60);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}")
}

/// Howard Hinnant's `civil_from_days`: proleptic-Gregorian (year, month,
/// day) from days since 1970-01-01.
///
/// Reference: <https://howardhinnant.github.io/date_algorithms.html#civil_from_days>.
/// The first two divisions are Euclidean so arbitrarily negative day counts
/// land in the right 400-year era; every later division operates on
/// non-negative intermediates exactly as in the reference.
pub(crate) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

/// Howard Hinnant's `days_from_civil`, the inverse of [`civil_from_days`]:
/// days since 1970-01-01 of a proleptic-Gregorian date. `month` is `1..=12`
/// and `day` is `1..=31`; the year may be any value whose day count fits.
///
/// Reference: <https://howardhinnant.github.io/date_algorithms.html#days_from_civil>.
pub(crate) fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400); // [0, 399]
    let mp = i64::from((month + 9) % 12); // March is 0
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn epoch_constant_is_1970() {
        assert_eq!(
            format_filetime_utc(FILETIME_UNIX_EPOCH),
            "1970-01-01T00:00:00"
        );
        assert_eq!(filetime_to_unix_ms(FILETIME_UNIX_EPOCH), 0);
        assert_eq!(unix_ms_to_filetime(0), FILETIME_UNIX_EPOCH);
    }

    #[test]
    fn filetime_zero_is_1601() {
        assert_eq!(format_filetime_utc(0), "1601-01-01T00:00:00");
    }

    #[test]
    fn known_unix_instants_format_correctly() {
        // (unix seconds, expected civil UTC)
        let cases: &[(i64, &str)] = &[
            (951_782_400, "2000-02-29T00:00:00"),   // century leap day
            (1_709_078_400, "2024-02-28T00:00:00"), // day before leap day
            (1_709_251_199, "2024-02-29T23:59:59"), // leap day, end of day
            (2_678_399, "1970-01-31T23:59:59"),     // end of month
            (946_684_799, "1999-12-31T23:59:59"),   // end of year
        ];
        for &(unix_s, expected) in cases {
            let ft = unix_ms_to_filetime(unix_s * 1_000);
            assert_eq!(format_filetime_utc(ft), expected, "unix {unix_s}");
        }
    }

    #[test]
    fn pre_1970_dates_are_correct_earlier_dates() {
        let ft = unix_ms_to_filetime(-86_400_000); // one day before Unix epoch
        assert_eq!(format_filetime_utc(ft), "1969-12-31T00:00:00");
        assert_eq!(filetime_to_unix_ms(ft), -86_400_000);
    }

    #[test]
    fn pre_1601_negative_ticks_do_not_panic() {
        // One second before the FILETIME epoch: 1600 is a leap year.
        assert_eq!(format_filetime_utc(-10_000_000), "1600-12-31T23:59:59");
    }

    #[test]
    fn sub_millisecond_ticks_floor() {
        // 9_999 ticks < 1 ms after the epoch → still ms 0.
        assert_eq!(filetime_to_unix_ms(FILETIME_UNIX_EPOCH + 9_999), 0);
        // 1 tick BEFORE the epoch floors to ms -1, not 0.
        assert_eq!(filetime_to_unix_ms(FILETIME_UNIX_EPOCH - 1), -1);
    }

    #[test]
    fn extremes_do_not_panic_and_max_matches_known_date() {
        // The well-known maximum FILETIME civil date.
        assert_eq!(format_filetime_utc(i64::MAX), "30828-09-14T02:48:05");
        let _ = format_filetime_utc(i64::MIN);
        let _ = filetime_to_unix_ms(i64::MIN);
        let _ = filetime_to_unix_ms(i64::MAX);
        // Saturating corners: high clamps to i64::MAX; low saturates the
        // multiply then adds the epoch offset.
        assert_eq!(unix_ms_to_filetime(i64::MAX), i64::MAX);
        assert_eq!(
            unix_ms_to_filetime(i64::MIN),
            i64::MIN + FILETIME_UNIX_EPOCH
        );
    }

    #[test]
    fn days_from_civil_inverts_civil_from_days() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1601, 1, 1), -DAYS_1601_TO_1970);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        for days in [
            -1_000_000,
            -DAYS_1601_TO_1970,
            -1,
            0,
            59,
            60,
            11_016,
            2_932_896,
        ] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "{y}-{m}-{d}");
        }
    }

    proptest! {
        /// ms → FILETIME → ms is exact for every ms whose FILETIME is
        /// representable (~year 31 million either way).
        #[test]
        fn ms_round_trips_exactly(ms in -900_000_000_000_000i64..=900_000_000_000_000) {
            prop_assert_eq!(filetime_to_unix_ms(unix_ms_to_filetime(ms)), ms);
        }

        /// FILETIME → ms → FILETIME floors to the containing millisecond:
        /// never later than the input, less than 10_000 ticks earlier.
        #[test]
        fn filetime_round_trip_floors(
            ft in (i64::MIN + FILETIME_UNIX_EPOCH + 10_000)..=i64::MAX
        ) {
            let back = unix_ms_to_filetime(filetime_to_unix_ms(ft));
            prop_assert!(back <= ft);
            prop_assert!(ft - back < 10_000);
        }

        /// The formatter is total over the whole i64 domain.
        #[test]
        fn format_never_panics(ft in proptest::num::i64::ANY) {
            let s = format_filetime_utc(ft);
            prop_assert!(s.contains('T'));
        }
    }
}
