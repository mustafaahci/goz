//! Facts about the calling process.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

/// Seconds from the FILETIME epoch (1601) to the Unix epoch.
const FILETIME_TO_UNIX_SECS: u64 = 11_644_473_600;

/// How long ago Windows created the calling process, or `None` when its
/// creation time cannot be read. Read early in `main`, it is what starting
/// the process cost before any of its own code ran: loading, the C runtime,
/// and any scan of the image an antivirus makes.
pub fn process_age() -> Option<Duration> {
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: plain FFI on the current-process pseudo-handle, which needs no
    // closing; the four out-params are live locals.
    let ok = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    };
    if ok == 0 {
        return None;
    }
    let created = u64::from(created.dwHighDateTime) << 32 | u64::from(created.dwLowDateTime);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
    let now =
        (now.as_secs() + FILETIME_TO_UNIX_SECS) * 10_000_000 + u64::from(now.subsec_nanos() / 100);
    Some(Duration::from_nanos(
        now.checked_sub(created)?.saturating_mul(100),
    ))
}

#[cfg(test)]
mod tests {
    use super::process_age;
    use std::time::Duration;

    /// The test process was created before the test ran, and not long ago.
    #[test]
    fn the_process_was_created_a_moment_ago() {
        let age = process_age().expect("creation time");
        assert!(age > Duration::ZERO);
        assert!(age < Duration::from_secs(600), "{age:?}");
    }
}
