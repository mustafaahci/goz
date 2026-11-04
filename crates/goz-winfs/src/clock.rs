//! A clock that stops while the machine sleeps.
//!
//! `Instant` reads `QueryPerformanceCounter`, which counts sleep and
//! hibernation, and `SystemTime` follows every clock change. A liveness check
//! on either reads a laptop's sleep, or a clock step, as work that stopped.

use std::time::Duration;

use windows_sys::Win32::System::WindowsProgramming::QueryUnbiasedInterruptTime;

/// How long the system has been awake since it booted
/// (`QueryUnbiasedInterruptTime`): monotonic, untouched by clock changes, and
/// paused while the machine sleeps or hibernates. It advances once per clock
/// tick, at most 15.625 ms.
pub fn awake_time() -> Duration {
    let mut ticks: u64 = 0;
    // SAFETY: plain Win32 FFI writing one u64 through a valid pointer. The
    // call fails only for an invalid pointer, so its result is not checked.
    unsafe { QueryUnbiasedInterruptTime(&mut ticks) };
    // The count is in 100 ns units.
    Duration::from_nanos(ticks.saturating_mul(100))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A quarter second awake reads as a quarter second, so the 100 ns unit
    /// is converted right.
    #[test]
    fn awake_time_advances_with_time_awake() {
        let before = awake_time();
        std::thread::sleep(Duration::from_millis(250));
        let elapsed = awake_time() - before;
        assert!(elapsed >= Duration::from_millis(200), "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
    }
}
