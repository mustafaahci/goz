//! Shared, thread-safe per-volume state.
//!
//! The index lives behind a `parking_lot::RwLock`: the volume's tail thread
//! takes the write lock for micro-batches of journal updates, while pipe-server
//! query tasks take read locks. The raw volume handle is NOT stored here (it is
//! `Send` but not `Sync`): it is moved into the tail thread, so `VolumeState`
//! stays `Sync` and can be shared across the tokio runtime via `Arc`.

use goz_core::index::VolumeIndex;
use goz_core::types::{EntryIdx, VolumePhase};
use parking_lot::{Mutex, RwLock};

/// The last-resolved `-path` scope for this volume: its volume-relative folded
/// path and the entry it maps to. Repeated searches in one folder (every
/// keystroke) reuse it instead of re-walking the directory tree; each query
/// validates it cheaply against the entry's current path.
pub(crate) struct ScopeCache {
    pub rel: Vec<u8>,
    pub entry: EntryIdx,
}

/// Live state for one indexed volume, shared between its tail thread and the
/// query server.
pub(crate) struct VolumeState {
    pub guid: String,
    pub mounts: Vec<String>,
    pub index: RwLock<VolumeIndex>,
    pub phase: RwLock<VolumePhase>,
    /// Whether the index's last build left sizes and dates unread (the
    /// FILE_LAYOUT pass stopped), so `-size`/`-dm` are unknown for some files.
    /// Set at bootstrap and by every rescan.
    pub metadata_pending: RwLock<bool>,
    /// Maximum size of the volume's USN journal as of the index's last build
    /// (bootstrap or rescan), `None` without a journal. Surfaced by `--status`
    /// because the journal's size bounds how long a downtime can be replayed
    /// instead of rescanned, and a journal that could not be grown was
    /// invisible for months when only the log knew.
    pub journal_max_bytes: RwLock<Option<u64>>,
    /// Memoized `-path` scope resolution (case-insensitive queries only).
    pub scope_cache: Mutex<Option<ScopeCache>>,
}

impl VolumeState {
    /// The display prefix for paths on this volume (its first mount, e.g.
    /// `C:\`, or the GUID path if it has no drive letter).
    pub(crate) fn mount_prefix(&self) -> &str {
        self.mounts.first().unwrap_or(&self.guid).as_str()
    }
}

/// The set of indexed volumes. A plain `Arc<Vec<...>>` for v1 (the volume set
/// only changes on hot-plug, which is post-v1); swap to `arc_swap::ArcSwap`
/// when hot-plug lands.
pub(crate) type VolumeSet = std::sync::Arc<Vec<std::sync::Arc<VolumeState>>>;
