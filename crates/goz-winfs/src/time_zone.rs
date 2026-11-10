//! The machine's time zone, first day of the week and short date order:
//! what turns a calendar day of a query, such as `dm:today` or
//! `dm:11/4/2026`, into the UTC times the index holds.

use core::ptr::null;

use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows_sys::Win32::Globalization::{
    GetLocaleInfoEx, LOCALE_IDATE, LOCALE_IFIRSTDAYOFWEEK, LOCALE_RETURN_NUMBER,
};
use windows_sys::Win32::System::Time::{
    DYNAMIC_TIME_ZONE_INFORMATION, FileTimeToSystemTime, GetDynamicTimeZoneInformation,
    SystemTimeToFileTime, SystemTimeToTzSpecificLocalTimeEx, TIME_ZONE_ID_INVALID,
    TzSpecificLocalTimeToSystemTimeEx,
};

use crate::error::{WinError, last_error};

/// FILETIME ticks in a millisecond, the resolution of a `SYSTEMTIME`.
const TICKS_PER_MS: i64 = 10_000;

/// A time zone with the daylight-saving rules Windows keeps for each year, so
/// a date in another year converts with that year's rules rather than this
/// one's. Times are FILETIME ticks; a local time counts them on the zone's
/// wall clock.
#[derive(Clone, Copy)]
pub struct TimeZone {
    info: DYNAMIC_TIME_ZONE_INFORMATION,
}

impl TimeZone {
    /// The machine's time zone as it is set now
    /// (`GetDynamicTimeZoneInformation`). With "Adjust for daylight saving
    /// time automatically" off it has no daylight saving, as the clock in the
    /// taskbar has none.
    pub fn current() -> Result<Self, WinError> {
        let mut info = DYNAMIC_TIME_ZONE_INFORMATION::default();
        // SAFETY: plain Win32 FFI writing one struct through a valid pointer.
        if unsafe { GetDynamicTimeZoneInformation(&mut info) } == TIME_ZONE_ID_INVALID {
            return Err(last_error("GetDynamicTimeZoneInformation"));
        }
        Ok(Self { info })
    }

    /// The wall-clock time at UTC time `utc`
    /// (`SystemTimeToTzSpecificLocalTimeEx`), or `None` outside the years a
    /// `SYSTEMTIME` holds, 1601 to 30827.
    pub fn local_from_utc(&self, utc: i64) -> Option<i64> {
        if let Some(bias) = self.fixed_bias() {
            return shifted(utc, -bias);
        }
        let (whole, below_ms) = split_ms(utc)?;
        let mut local = SYSTEMTIME::default();
        // SAFETY: plain Win32 FFI; both SYSTEMTIMEs and the zone are valid
        // for the call, which only reads the first two and writes the third.
        let ok = unsafe { SystemTimeToTzSpecificLocalTimeEx(&self.info, &whole, &mut local) };
        if ok == 0 {
            return None;
        }
        from_system_time(&local)?.checked_add(below_ms)
    }

    /// The UTC time at which the wall clock shows `local`
    /// (`TzSpecificLocalTimeToSystemTimeEx`), or `None` outside the years a
    /// `SYSTEMTIME` holds. Windows maps a time that a daylight-saving change
    /// skips or repeats to one within the change.
    pub fn utc_from_local(&self, local: i64) -> Option<i64> {
        if let Some(bias) = self.fixed_bias() {
            return shifted(local, bias);
        }
        let (whole, below_ms) = split_ms(local)?;
        let mut utc = SYSTEMTIME::default();
        // SAFETY: as in `local_from_utc`, with the roles of the two times
        // swapped.
        let ok = unsafe { TzSpecificLocalTimeToSystemTimeEx(&self.info, &whole, &mut utc) };
        if ok == 0 {
            return None;
        }
        from_system_time(&utc)?.checked_add(below_ms)
    }

    /// UTC minus local time, in ticks, when "Adjust for daylight saving time
    /// automatically" is off. The clock in the taskbar then keeps the zone's
    /// standard offset all year, but the `...Ex` conversions look the zone's
    /// rules up by name and still apply daylight saving, so this zone
    /// converts with the fixed offset instead.
    fn fixed_bias(&self) -> Option<i64> {
        self.info
            .DynamicDaylightTimeDisabled
            .then(|| i64::from(self.info.Bias) * 60 * 10_000_000)
    }
}

/// `ticks` moved by `delta`, within the years a `SYSTEMTIME` holds as the
/// `...Ex` conversions are.
fn shifted(ticks: i64, delta: i64) -> Option<i64> {
    split_ms(ticks)?;
    let moved = ticks.checked_add(delta)?;
    split_ms(moved)?;
    Some(moved)
}

/// `ticks` as the `SYSTEMTIME` of its whole milliseconds, and the ticks below
/// a millisecond that a `SYSTEMTIME` cannot hold. A zone's offset is whole
/// minutes, so those ticks carry across a conversion unchanged.
fn split_ms(ticks: i64) -> Option<(SYSTEMTIME, i64)> {
    let below_ms = ticks.checked_rem_euclid(TICKS_PER_MS)?;
    let whole = u64::try_from(ticks - below_ms).ok()?;
    let file = FILETIME {
        dwLowDateTime: whole as u32,
        dwHighDateTime: (whole >> 32) as u32,
    };
    let mut time = SYSTEMTIME::default();
    // SAFETY: plain Win32 FFI reading one FILETIME and writing one
    // SYSTEMTIME, both valid locals; it fails for ticks of 2^63 and up.
    if unsafe { FileTimeToSystemTime(&file, &mut time) } == 0 {
        return None;
    }
    Some((time, below_ms))
}

fn from_system_time(time: &SYSTEMTIME) -> Option<i64> {
    let mut file = FILETIME::default();
    // SAFETY: plain Win32 FFI reading one SYSTEMTIME and writing one
    // FILETIME, both valid for the call.
    if unsafe { SystemTimeToFileTime(time, &mut file) } == 0 {
        return None;
    }
    i64::try_from((u64::from(file.dwHighDateTime) << 32) | u64::from(file.dwLowDateTime)).ok()
}

/// The first day of the week in the calling account's regional format, 0
/// for Monday through 6 for Sunday (`LOCALE_IFIRSTDAYOFWEEK`), or Monday when
/// it cannot be read. A service running as LocalSystem reads the system
/// account's format, which Windows Setup takes from the region it installs
/// for and "Copy settings" in the administrative language settings updates.
pub fn first_day_of_week() -> u8 {
    match locale_number(LOCALE_IFIRSTDAYOFWEEK) {
        Some(day @ 0..=6) => day as u8,
        _ => 0,
    }
}

/// The order of the day, month and year in the calling account's short date
/// format (`LOCALE_IDATE`): 0 for month/day/year, 1 for day/month/year, 2
/// for year/month/day, or month/day/year when it cannot be read. Everything
/// reads a date written with `/` or `.` in this order (`dm:11/4/2026`), and
/// so does goz; under the service, it is the system account's format, as
/// for [`first_day_of_week`].
pub fn short_date_order() -> u8 {
    match locale_number(LOCALE_IDATE) {
        Some(order @ 0..=2) => order as u8,
        _ => 0,
    }
}

/// A numeric field of the calling account's default locale, such as
/// `LOCALE_IFIRSTDAYOFWEEK`, or `None` when it cannot be read.
fn locale_number(field: u32) -> Option<u32> {
    let mut value: u32 = 0;
    // SAFETY: plain Win32 FFI. With LOCALE_RETURN_NUMBER the call writes one
    // DWORD, two UTF-16 units, through the aligned pointer to a local u32;
    // a null name is the calling account's default locale.
    let written = unsafe {
        GetLocaleInfoEx(
            null(),
            field | LOCALE_RETURN_NUMBER,
            (&raw mut value).cast(),
            2,
        )
    };
    (written > 0).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Storage::FileSystem::FileTimeToLocalFileTime;
    use windows_sys::Win32::System::Time::EnumDynamicTimeZoneInformation;

    /// The FILETIME of a UTC `YYYY-MM-DDTHH:MM`.
    fn ft(text: &str) -> i64 {
        let n = |range: core::ops::Range<usize>| text[range].parse::<u16>().unwrap();
        let time = SYSTEMTIME {
            wYear: n(0..4),
            wMonth: n(5..7),
            wDay: n(8..10),
            wHour: n(11..13),
            wMinute: n(14..16),
            ..SYSTEMTIME::default()
        };
        from_system_time(&time).unwrap()
    }

    /// The zone Windows keeps under `key`, such as `Eastern Standard Time`.
    fn named(key: &str) -> TimeZone {
        for index in 0.. {
            let mut info = DYNAMIC_TIME_ZONE_INFORMATION::default();
            // SAFETY: plain Win32 FFI writing one struct through a valid
            // pointer; it fails once `index` passes the last zone.
            if unsafe { EnumDynamicTimeZoneInformation(index, &mut info) } != 0 {
                break;
            }
            let len = info
                .TimeZoneKeyName
                .iter()
                .position(|&u| u == 0)
                .unwrap_or(128);
            if String::from_utf16_lossy(&info.TimeZoneKeyName[..len]) == key {
                return TimeZone { info };
            }
        }
        panic!("no time zone {key}");
    }

    /// Local midnight in US Eastern time moves with daylight saving: the
    /// days of the 2026 changes last 23 and 25 hours, and the 2006 rules
    /// (first Sunday of April) still apply to 2006.
    #[test]
    fn eastern_midnights_follow_each_years_rules() {
        let eastern = named("Eastern Standard Time");
        for (local, utc) in [
            ("2026-03-08T00:00", "2026-03-08T05:00"),
            ("2026-03-09T00:00", "2026-03-09T04:00"),
            ("2026-11-01T00:00", "2026-11-01T04:00"),
            ("2026-11-02T00:00", "2026-11-02T05:00"),
            ("2006-03-13T00:00", "2006-03-13T05:00"),
            ("2006-04-03T00:00", "2006-04-03T04:00"),
        ] {
            assert_eq!(eastern.utc_from_local(ft(local)), Some(ft(utc)), "{local}");
            assert_eq!(eastern.local_from_utc(ft(utc)), Some(ft(local)), "{utc}");
        }
        // Ticks below a millisecond survive the round trip.
        let t = ft("2026-07-01T12:00") + 1_234;
        assert_eq!(
            eastern.local_from_utc(t).map(|l| l - t),
            Some(-4 * 36_000_000_000)
        );
        assert_eq!(eastern.utc_from_local(t - 4 * 36_000_000_000), Some(t));
        assert_eq!(eastern.local_from_utc(-1), None);
        assert_eq!(eastern.utc_from_local(i64::MAX), None);
    }

    /// With "Adjust for daylight saving time automatically" off, Eastern time
    /// stays five hours behind UTC in July, as the clock in the taskbar does,
    /// though the zone's rules would put daylight saving there.
    #[test]
    fn a_zone_without_automatic_daylight_saving_keeps_its_standard_offset() {
        let mut eastern = named("Eastern Standard Time");
        eastern.info.DynamicDaylightTimeDisabled = true;
        assert_eq!(
            eastern.local_from_utc(ft("2026-07-01T12:00")),
            Some(ft("2026-07-01T07:00"))
        );
        assert_eq!(
            eastern.utc_from_local(ft("2026-07-01T00:00")),
            Some(ft("2026-07-01T05:00"))
        );
        assert_eq!(eastern.local_from_utc(-1), None);
    }

    /// The machine's zone agrees, right now, with the conversion the file
    /// system uses for the current time.
    #[test]
    fn the_current_zone_agrees_with_the_file_system_now() {
        let zone = TimeZone::current().unwrap();
        let since_1970 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        let now = ft("1970-01-01T00:00") + i64::try_from(since_1970.as_nanos() / 100).unwrap();
        let utc = FILETIME {
            dwLowDateTime: now as u32,
            dwHighDateTime: (now >> 32) as u32,
        };
        let mut local = FILETIME::default();
        // SAFETY: plain Win32 FFI reading and writing one FILETIME each.
        assert_ne!(unsafe { FileTimeToLocalFileTime(&utc, &mut local) }, 0);
        let local = (i64::from(local.dwHighDateTime) << 32) | i64::from(local.dwLowDateTime);
        assert_eq!(zone.local_from_utc(now), Some(local));
        assert!(first_day_of_week() <= 6);
        assert!(short_date_order() <= 2);
    }
}
