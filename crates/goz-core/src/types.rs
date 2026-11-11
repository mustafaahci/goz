//! Shared identifier types used across the core.
//!
//! Small enums that cross module boundaries (query engine ↔ protocol ↔
//! es-compat argv ↔ CSV output) live here so no module depends on another
//! for a two-variant type.

use core::fmt;
use serde::{Deserialize, Serialize};

/// NTFS 64-bit file reference number: a 48-bit MFT record index plus a 16-bit
/// sequence number. The sequence number increments when an MFT slot is
/// reused, so a map keyed on the full `u64` can never confuse a dead file
/// with its slot's next tenant.
///
/// ReFS uses 128-bit ids; v1 indexes NTFS only, and the newtype leaves the
/// door open to widen later without touching call sites.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Frn(pub u64);

impl Frn {
    /// The 48-bit MFT record number. Dense and reused by NTFS.
    #[inline]
    pub fn record_number(self) -> u64 {
        self.0 & 0x0000_FFFF_FFFF_FFFF
    }

    /// The 16-bit sequence number, bumped each time the MFT slot is reused.
    #[inline]
    pub fn sequence(self) -> u16 {
        (self.0 >> 48) as u16
    }
}

impl fmt::Debug for Frn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Frn({}#{})", self.record_number(), self.sequence())
    }
}

/// The most bytes an NTFS name component takes in UTF-16: 255 code units.
/// The journal and layout parsers reject a longer name as corrupt, which also
/// keeps every stored name far below the name store's 64 KiB limit.
pub(crate) const MAX_NAME_UTF16_BYTES: usize = 510;

/// Dense index into the `EntryTable` columns.
pub type EntryIdx = u32;

/// Sentinel `EntryIdx`: "no entry".
pub const NIL: EntryIdx = u32::MAX;

/// Result sort key. `Path` is es's `-sort-path`: case-insensitive
/// `(parent_path, name)`, NOT a naive full-path compare.
///
/// A key added after v0.6.0 is listed in `Hello.features` as `sort:<key>`
/// (`sort:extension`): a daemon predating it refuses the request as
/// malformed, so a client asks only for a key the daemon lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SortKey {
    Name,
    Path,
    Size,
    DateModified,
    /// Everything's extension order: folders first, by name (a folder has
    /// no extension), then the files with no extension, then the files by
    /// extension, case-insensitively, and by name within one extension.
    /// Descending is the exact reverse, as in Everything.
    Extension,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SortDir {
    Asc,
    Desc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SortSpec {
    pub key: SortKey,
    pub dir: SortDir,
}

impl SortSpec {
    /// es.exe's default direction per sort key: name/path ascending,
    /// size/date descending.
    pub fn default_for(key: SortKey) -> Self {
        let dir = match key {
            SortKey::Name | SortKey::Path | SortKey::Extension => SortDir::Asc,
            SortKey::Size | SortKey::DateModified => SortDir::Desc,
        };
        Self { key, dir }
    }
}

impl Default for SortSpec {
    /// es.exe with no `-sort` flag sorts by name ascending.
    fn default() -> Self {
        Self::default_for(SortKey::Name)
    }
}

/// Optional CSV columns, in flag order. `Filename` is always emitted last and
/// is not represented here (es semantics: `-size -dm` → `Size,Date
/// Modified,Filename`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EsColumn {
    Size,
    DateModified,
}

/// Lifecycle phase of one indexed volume, reported over IPC.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase")]
pub enum VolumePhase {
    Bootstrapping,
    Live,
    Rescanning,
    /// Fully indexed from the MFT, but the volume has no USN journal and cannot
    /// be given one (`FSCTL_CREATE_USN_JOURNAL` failed), so the index is a
    /// bootstrap snapshot that will never see a change. Rare in practice: even
    /// recovery partitions accept a journal.
    ///
    /// Distinct from [`VolumePhase::Offline`] because nothing is wrong and
    /// nothing will change. See [`VolumePhase::is_complete`].
    Snapshot,
    /// Indexed, but live updates have stopped when they should be working: the
    /// tail thread could not start, or journal reads keep failing. Transient or
    /// broken, and worth telling the user about.
    Offline,
    Failed {
        reason: String,
    },
}

impl VolumePhase {
    /// Whether a result set drawn from this volume is as complete as goz can
    /// ever make it, i.e. it must NOT be flagged incomplete to the user.
    ///
    /// `Snapshot` counts, and that is the whole point of the variant. Such a
    /// volume has no journal and never will, so its index cannot improve and
    /// waiting cannot help. Flagging it would print "results may be incomplete"
    /// on every single query for the life of the machine, and a warning that is
    /// always on is a warning nobody reads: the one that matters, a volume
    /// genuinely mid-rescan, would scroll past unnoticed. The limitation is
    /// reported once, in `Status`, where it is actionable.
    ///
    /// The rule lives here, not at each call site, so the query path, `Status`,
    /// and `Hello.ready` cannot drift apart about what "complete" means.
    pub fn is_complete(&self) -> bool {
        matches!(self, VolumePhase::Live | VolumePhase::Snapshot)
    }
}

impl std::fmt::Display for VolumePhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VolumePhase::Bootstrapping => write!(f, "bootstrapping"),
            VolumePhase::Live => write!(f, "live"),
            VolumePhase::Rescanning => write!(f, "rescanning"),
            VolumePhase::Snapshot => write!(f, "snapshot (no journal; cannot track changes)"),
            VolumePhase::Offline => write!(f, "offline"),
            VolumePhase::Failed { reason } => write!(f, "failed ({reason})"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frn_splits_record_and_sequence() {
        let frn = Frn(0x0005_0000_0000_002A);
        assert_eq!(frn.record_number(), 0x2A);
        assert_eq!(frn.sequence(), 5);
    }

    #[test]
    fn frn_slot_reuse_yields_distinct_values() {
        let dead = Frn((3u64 << 48) | 42);
        let next_tenant = Frn((4u64 << 48) | 42);
        assert_eq!(dead.record_number(), next_tenant.record_number());
        assert_ne!(dead, next_tenant);
    }
}
