//! Cursor validation: decide "replay from saved USN" vs "full rescan".
//!
//! [`validate_cursor`] is a pure total function holding the entire
//! wrap/staleness decision so it can be exhaustively unit-tested.
//!
//! It is deliberately unwired, not unfinished. v1 always cold-bootstraps at
//! daemon start (see `bootstrap::ensure_journal`, which acquires a fresh cursor
//! each run), so there is no saved cursor to validate. Do not "complete" this by
//! persisting a cursor on its own: a cursor without a persisted index would tell
//! a freshly empty index it is already current, and the daemon would serve an
//! empty volume as `Live`. Persistence lands as cursor plus index columns
//! together, CRC'd and atomically replaced, as an optimization that is never a
//! source of truth.
//!
//! Runtime failures that surface during tailing (read errors, parser aborts) are
//! mapped to the same [`RescanReason`] enum by the daemon today.

/// A per-volume cursor: the journal identity and next USN a caller would need
/// to persist in order to replay instead of rescan. Not persisted today; see the
/// module docs for why persisting it on its own would be a bug.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SavedCursor {
    /// `UsnJournalID` of the journal the cursor was taken from.
    pub journal_id: u64,
    /// Next USN to read (the leading value of the last processed buffer).
    pub next_usn: i64,
}

/// Live journal state, decoded from `USN_JOURNAL_DATA_V*`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JournalInfo {
    /// `UsnJournalID`: the journal instance identifier; changes when the
    /// journal is deleted and recreated (or restamped).
    pub journal_id: u64,
    /// `FirstUsn`: oldest record still readable; everything below has been
    /// purged from the front (the "wrap").
    pub first_usn: i64,
    /// `NextUsn`: the USN the next written record will get.
    pub next_usn: i64,
    /// `LowestValidUsn`: USNs below this predate the current journal
    /// stamping; a cursor below it means a discontinuity.
    pub lowest_valid_usn: i64,
    /// `MaxUsn`: hard ceiling; the journal must be recreated as `NextUsn`
    /// approaches it. Not consulted by [`validate_cursor`] (approaching the
    /// ceiling is an operational concern, not a staleness one).
    pub max_usn: i64,
}

/// Outcome of cursor validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resync {
    /// The saved cursor is valid: replay the journal from `saved.next_usn`.
    InSync,
    /// Changes were (or may have been) missed: rebuild the volume index.
    FullRescan(RescanReason),
}

/// Why a full rescan was decided. [`validate_cursor`] produces only
/// `JournalIdChanged` / `CursorPurged` / `RestampDiscontinuity`; `ParserAnomaly`,
/// `EntryDeletedError`, and `TailRevived` are mapped by the daemon from runtime
/// conditions. `JournalNotActive`, `JournalDeleteInProgress`, and
/// `VolumeReattached` are reserved for causes the daemon does not yet map. All
/// live here so every rescan cause shares one telemetry vocabulary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RescanReason {
    /// Saved `UsnJournalID` differs from the live journal's: the journal
    /// was deleted/recreated while we were away.
    JournalIdChanged,
    /// `saved.next_usn < FirstUsn`: the journal wrapped past our cursor
    /// (oldest records purged from the front).
    CursorPurged,
    /// `ERROR_JOURNAL_NOT_ACTIVE` (1179): no journal on the volume.
    JournalNotActive,
    /// `ERROR_JOURNAL_DELETE_IN_PROGRESS` (1178): a deletion is running;
    /// wait for it, then rescan.
    JournalDeleteInProgress,
    /// `ERROR_JOURNAL_ENTRY_DELETED` (1181) from a read: our `StartUsn` was
    /// purged between validation and the read (daemon-mapped).
    EntryDeletedError,
    /// Cursor is outside the journal's valid USN window without a purge
    /// explaining it: below `LowestValidUsn`, or ahead of the live
    /// `NextUsn` (a cursor from the future means the journal was
    /// restamped).
    RestampDiscontinuity,
    /// A corrupt buffer or cycle was detected while parsing/applying; we
    /// cannot prove what was missed (daemon-mapped).
    ParserAnomaly,
    /// The volume's tail thread exited and its supervisor revived it; the
    /// volume went untailed in between, so nothing proves the index is still
    /// in sync (daemon-mapped).
    TailRevived,
    /// The volume went away and came back; its journal may have advanced on
    /// another machine.
    VolumeReattached,
}

/// Decides whether a persisted cursor can be replayed against the live
/// journal, or the volume needs a full rescan.
///
/// Truth table, first match wins:
///
/// | Condition                               | Result                                   |
/// |-----------------------------------------|------------------------------------------|
/// | `saved.journal_id != live.journal_id`   | `FullRescan(JournalIdChanged)`           |
/// | `saved.next_usn < live.first_usn`       | `FullRescan(CursorPurged)` (the wrap)    |
/// | `saved.next_usn < live.lowest_valid_usn`| `FullRescan(RestampDiscontinuity)`       |
/// | `saved.next_usn > live.next_usn`        | `FullRescan(RestampDiscontinuity)` (cursor from the future) |
/// | otherwise                               | `InSync`                                 |
pub fn validate_cursor(saved: &SavedCursor, live: &JournalInfo) -> Resync {
    if saved.journal_id != live.journal_id {
        return Resync::FullRescan(RescanReason::JournalIdChanged);
    }
    if saved.next_usn < live.first_usn {
        return Resync::FullRescan(RescanReason::CursorPurged);
    }
    if saved.next_usn < live.lowest_valid_usn {
        return Resync::FullRescan(RescanReason::RestampDiscontinuity);
    }
    if saved.next_usn > live.next_usn {
        return Resync::FullRescan(RescanReason::RestampDiscontinuity);
    }
    Resync::InSync
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live() -> JournalInfo {
        JournalInfo {
            journal_id: 7,
            first_usn: 100,
            next_usn: 1000,
            lowest_valid_usn: 0,
            max_usn: i64::MAX,
        }
    }

    fn saved(next_usn: i64) -> SavedCursor {
        SavedCursor {
            journal_id: 7,
            next_usn,
        }
    }

    #[test]
    fn cursor_inside_window_is_in_sync() {
        assert_eq!(validate_cursor(&saved(500), &live()), Resync::InSync);
    }

    #[test]
    fn cursor_at_first_usn_is_in_sync() {
        assert_eq!(validate_cursor(&saved(100), &live()), Resync::InSync);
    }

    #[test]
    fn cursor_at_next_usn_is_in_sync() {
        // Equal to live NextUsn = fully caught up, nothing new to read.
        assert_eq!(validate_cursor(&saved(1000), &live()), Resync::InSync);
    }

    #[test]
    fn journal_id_mismatch_forces_rescan() {
        let stale = SavedCursor {
            journal_id: 6,
            next_usn: 500,
        };
        assert_eq!(
            validate_cursor(&stale, &live()),
            Resync::FullRescan(RescanReason::JournalIdChanged)
        );
    }

    #[test]
    fn journal_id_mismatch_wins_over_purge() {
        let stale = SavedCursor {
            journal_id: 6,
            next_usn: 50,
        };
        assert_eq!(
            validate_cursor(&stale, &live()),
            Resync::FullRescan(RescanReason::JournalIdChanged)
        );
    }

    #[test]
    fn cursor_below_first_usn_is_purged() {
        assert_eq!(
            validate_cursor(&saved(50), &live()),
            Resync::FullRescan(RescanReason::CursorPurged)
        );
    }

    #[test]
    fn purge_is_reported_before_restamp() {
        let live = JournalInfo {
            lowest_valid_usn: 200,
            ..live()
        };
        // 50 is below both FirstUsn and LowestValidUsn; the wrap wins.
        assert_eq!(
            validate_cursor(&saved(50), &live),
            Resync::FullRescan(RescanReason::CursorPurged)
        );
    }

    #[test]
    fn cursor_below_lowest_valid_usn_is_restamp_discontinuity() {
        let live = JournalInfo {
            lowest_valid_usn: 200,
            ..live()
        };
        assert_eq!(
            validate_cursor(&saved(150), &live),
            Resync::FullRescan(RescanReason::RestampDiscontinuity)
        );
    }

    #[test]
    fn cursor_at_lowest_valid_usn_is_in_sync() {
        let live = JournalInfo {
            lowest_valid_usn: 200,
            ..live()
        };
        assert_eq!(validate_cursor(&saved(200), &live), Resync::InSync);
    }

    #[test]
    fn cursor_from_the_future_is_restamp_discontinuity() {
        assert_eq!(
            validate_cursor(&saved(2000), &live()),
            Resync::FullRescan(RescanReason::RestampDiscontinuity)
        );
    }
}
