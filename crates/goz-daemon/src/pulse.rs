//! A tail thread's self-reported liveness, packed into one atomic word.
//!
//! The old watchdog inferred the tail's state from a bare heartbeat timestamp,
//! so "silent" had two indistinguishable causes: wedged in a syscall, or
//! returned for good. A tail that gave up (journal permanently lost, rescan
//! budget exhausted) then looked exactly like the stuck-IRP case the watchdog
//! exists for, and it error-logged and cancelled I/O on the dead thread every
//! scan, forever. The pulse makes the tail say which it is, and makes the
//! "returned for good" report impossible to forget: it is published by a
//! [`Drop`] guard, so every exit path, including a panic unwind and any
//! `return` added later, retires the pulse.
//!
//! State and timestamp share one `u64` (tag in the top two bits, unix seconds
//! below) so a reader always sees a consistent pair; two separate atomics
//! could tear between a supervisor's loads.

use std::sync::atomic::{AtomicU64, Ordering};

const TAG_SHIFT: u32 = 62;
const SECS_MASK: u64 = (1 << TAG_SHIFT) - 1;
const TAG_BEAT: u64 = 0;
const TAG_BUSY: u64 = 1;
const TAG_RETIRED: u64 = 2;

/// One decoded pulse observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pulse {
    /// The tail loop is turning; unix seconds of its last stamp.
    Beat(u64),
    /// The tail is inside a legitimately long, non-stamping operation (a full
    /// MFT rebuild); unix seconds of when it entered.
    Busy(u64),
    /// The tail thread returned. Not stuck, and not coming back on its own.
    Retired,
}

/// The shared word a tail writes and its supervisor reads.
///
/// Ordering invariant: `Retired` must be the last value ever stored, and that
/// is guaranteed structurally rather than atomically. Busy scopes live inside
/// `tail_loop` while the retire guard wraps the whole call, so guard drop
/// order publishes any pending `Beat` restore before `Retired`. This keeps the
/// hot-path stamp a plain relaxed store instead of a compare-exchange.
pub(crate) struct TailPulse(AtomicU64);

impl TailPulse {
    /// A fresh pulse, beating as of now (the tail is about to start).
    pub(crate) fn new() -> Self {
        Self(AtomicU64::new(pack(TAG_BEAT, now_unix())))
    }

    /// Stamps liveness. Called once per tail-loop turn.
    pub(crate) fn beat(&self) {
        self.0.store(pack(TAG_BEAT, now_unix()), Ordering::Relaxed);
    }

    /// The current observation.
    pub(crate) fn load(&self) -> Pulse {
        let word = self.0.load(Ordering::Relaxed);
        let secs = word & SECS_MASK;
        match word >> TAG_SHIFT {
            TAG_BUSY => Pulse::Busy(secs),
            TAG_RETIRED => Pulse::Retired,
            _ => Pulse::Beat(secs),
        }
    }

    /// Marks the tail busy until the returned guard drops, which restores a
    /// fresh beat so a long rebuild does not read as an instant stall the
    /// moment it finishes. Scopes must not nest (the inner drop would end the
    /// outer scope early); the tail has exactly one, around `rescan`.
    pub(crate) fn busy_scope(&self) -> BusyGuard<'_> {
        self.0.store(pack(TAG_BUSY, now_unix()), Ordering::Relaxed);
        BusyGuard(self)
    }

    /// Publishes `Retired` when the returned guard drops. Held across the
    /// whole `tail_loop` call so every exit, panic unwinds included, reports
    /// the thread as gone rather than leaving a heartbeat that merely went
    /// quiet.
    pub(crate) fn retire_on_drop(&self) -> RetireGuard<'_> {
        RetireGuard(self)
    }
}

/// Restores a fresh [`Pulse::Beat`] on drop. See [`TailPulse::busy_scope`].
pub(crate) struct BusyGuard<'a>(&'a TailPulse);

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.beat();
    }
}

/// Publishes [`Pulse::Retired`] on drop. See [`TailPulse::retire_on_drop`].
pub(crate) struct RetireGuard<'a>(&'a TailPulse);

impl Drop for RetireGuard<'_> {
    fn drop(&mut self) {
        self.0.0.store(pack(TAG_RETIRED, 0), Ordering::Relaxed);
    }
}

fn pack(tag: u64, secs: u64) -> u64 {
    (tag << TAG_SHIFT) | (secs & SECS_MASK)
}

pub(crate) fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_pulse_beats_now() {
        let p = TailPulse::new();
        let before = now_unix();
        match p.load() {
            Pulse::Beat(at) => assert!(at <= before && before - at <= 1),
            other => panic!("fresh pulse must beat, got {other:?}"),
        }
    }

    #[test]
    fn busy_scope_restores_a_fresh_beat() {
        let p = TailPulse::new();
        {
            let _busy = p.busy_scope();
            assert!(matches!(p.load(), Pulse::Busy(_)));
        }
        assert!(matches!(p.load(), Pulse::Beat(_)));
    }

    /// The design pillar: retirement is published by unwinding, not by
    /// remembering to call something, so a panicking tail still reports
    /// itself gone instead of leaving a heartbeat that merely went quiet.
    #[test]
    fn a_panicking_thread_still_retires() {
        let p = std::sync::Arc::new(TailPulse::new());
        let thread_p = p.clone();
        let joined = std::thread::spawn(move || {
            let _retired = thread_p.retire_on_drop();
            panic!("tail died unexpectedly");
        })
        .join();
        assert!(joined.is_err(), "the thread must have panicked");
        assert_eq!(p.load(), Pulse::Retired);
    }

    /// Guard drop order is the whole ordering story: a busy scope inside the
    /// retire guard must end first, so `Retired` is the final word even
    /// though nothing enforces it atomically.
    #[test]
    fn retire_wins_over_an_unwinding_busy_scope() {
        let p = TailPulse::new();
        {
            let _retired = p.retire_on_drop();
            let _busy = p.busy_scope();
            // _busy drops first (restores Beat), then _retired publishes.
        }
        assert_eq!(p.load(), Pulse::Retired);
    }

    /// Timestamps survive the tag bits for any plausible clock value,
    /// including far-future ones.
    #[test]
    fn tag_bits_never_corrupt_the_seconds() {
        for secs in [0u64, 1, 1_752_800_000, SECS_MASK] {
            assert_eq!(pack(TAG_BEAT, secs) & SECS_MASK, secs);
            assert_eq!(pack(TAG_BUSY, secs) >> TAG_SHIFT, TAG_BUSY);
        }
    }
}
