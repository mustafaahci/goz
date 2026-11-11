//! Shared, thread-safe per-volume state, and the registry of indexed volumes.
//!
//! The index lives behind a `parking_lot::RwLock`: the volume's tail thread
//! takes the write lock for micro-batches of journal updates, while pipe-server
//! query tasks take read locks. The raw volume handle is NOT stored here (it is
//! `Send` but not `Sync`): it is moved into the tail thread, so `VolumeState`
//! stays `Sync` and can be shared across the tokio runtime via `Arc`.

use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

use goz_core::index::{Columns, FrnMap, NTFS_ROOT_FRN, VolumeIndex};
use goz_core::types::{EntryIdx, VolumePhase};
use parking_lot::{Mutex, RwLock, RwLockWriteGuard};
use tokio::sync::watch;

/// Counts the changes published by every volume, and the volumes found and
/// removed: what a `Watch` waits on. One for the process, so a volume needs
/// no handle on the watchers to tell them; a watcher looks up which of its
/// volumes moved (see [`VolumeState::published`]).
static CHANGES: LazyLock<watch::Sender<u64>> = LazyLock::new(|| watch::channel(0).0);

/// A receiver that wakes after each change published from now on.
pub(crate) fn subscribe_changes() -> watch::Receiver<u64> {
    CHANGES.subscribe()
}

fn announce_change() {
    CHANGES.send_modify(|n| *n = n.wrapping_add(1));
}

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
    /// Set by every build.
    pub metadata_pending: RwLock<bool>,
    /// Maximum size of the volume's USN journal as of the index's last build,
    /// `None` without a journal. Surfaced by `--status` because the journal's
    /// size bounds how long a downtime can be replayed instead of rescanned,
    /// and a journal that could not be grown was invisible for months when
    /// only the log knew.
    pub journal_max_bytes: RwLock<Option<u64>>,
    /// Whether an index has been built since the volume was found. A tail
    /// started without a cursor reads it to tell a first build, which serves
    /// the index even before the journal can be read, from a rebuild, which
    /// waits for the journal.
    pub indexed: AtomicBool,
    /// Memoized `-path` scope resolution (case-insensitive queries only).
    pub scope_cache: Mutex<Option<ScopeCache>>,
    /// The index generation as of the last change published to watchers,
    /// read without the index lock.
    pub published_generation: AtomicU64,
    /// How the background build of optional columns switched on while the
    /// volume is live stands, for `--status`.
    pub column_build: Mutex<ColumnBuild>,
}

/// A background build of optional columns (see `tail::ColumnBuilder`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ColumnBuild {
    /// The columns the pass running fills; none while none runs.
    pub building: Columns,
    /// Files the pass has filled in so far.
    pub built: u64,
}

impl VolumeState {
    /// A volume just found: an empty index, `Bootstrapping` until its tail
    /// has built one. Served at once, so its results are flagged incomplete
    /// rather than missing.
    pub(crate) fn found(guid: String, mounts: Vec<String>) -> Self {
        Self {
            guid,
            mounts,
            index: RwLock::new(VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse())),
            phase: RwLock::new(VolumePhase::Bootstrapping),
            metadata_pending: RwLock::new(false),
            journal_max_bytes: RwLock::new(None),
            indexed: AtomicBool::new(false),
            scope_cache: Mutex::new(None),
            published_generation: AtomicU64::new(0),
            column_build: Mutex::new(ColumnBuild::default()),
        }
    }

    /// The index under its write lock. A change that moves the generation
    /// (a journal batch, the enricher's sizes and dates, a reconcile, a
    /// rebuild, folder-size deltas: see `VolumeIndex::note_changed`) is
    /// published to watchers when the guard drops, so no writer can forget
    /// to tell them.
    pub(crate) fn write_index(&self) -> IndexWrite<'_> {
        let guard = self.index.write();
        let before = guard.generation();
        IndexWrite {
            state: self,
            guard,
            before,
        }
    }

    /// The generation last published (see [`Self::write_index`]).
    pub(crate) fn published(&self) -> u64 {
        self.published_generation.load(Ordering::Acquire)
    }

    /// Tells watchers this volume is now at `generation`.
    pub(crate) fn publish(&self, generation: u64) {
        self.published_generation
            .store(generation, Ordering::Release);
        announce_change();
    }

    /// The display prefix for paths on this volume (its first mount, e.g.
    /// `C:\`, or the GUID path if it has no drive letter).
    pub(crate) fn mount_prefix(&self) -> &str {
        self.mounts.first().unwrap_or(&self.guid).as_str()
    }
}

/// A volume's index under its write lock, which tells watchers of a change
/// as it drops (see [`VolumeState::write_index`]).
pub(crate) struct IndexWrite<'a> {
    state: &'a VolumeState,
    guard: RwLockWriteGuard<'a, VolumeIndex>,
    before: u64,
}

impl Deref for IndexWrite<'_> {
    type Target = VolumeIndex;
    fn deref(&self) -> &VolumeIndex {
        &self.guard
    }
}

impl DerefMut for IndexWrite<'_> {
    fn deref_mut(&mut self) -> &mut VolumeIndex {
        &mut self.guard
    }
}

impl Drop for IndexWrite<'_> {
    fn drop(&mut self) {
        let now = self.guard.generation();
        if now != self.before {
            self.state.publish(now);
        }
    }
}

/// The indexed volumes at one moment. A request works on one snapshot from
/// start to end, so a volume found or removed meanwhile cannot change its
/// answer halfway.
pub(crate) type VolumeSet = Arc<Vec<Arc<VolumeState>>>;

/// The indexed volumes as they change: the supervisor adds a volume when it
/// is found and removes it when it is gone, and readers take a [`VolumeSet`]
/// from [`VolumeRegistry::current`].
///
/// The lock guards only the swap, so a reader clones one `Arc` and no query
/// runs under it. Adding appends and removing keeps the order of the rest, so
/// two volumes never trade places between snapshots: a query read-locks the
/// indexes of its volumes in snapshot order, and two queries taking them in
/// opposite orders could deadlock behind a waiting tail writer.
#[derive(Default)]
pub(crate) struct VolumeRegistry(RwLock<VolumeSet>);

impl VolumeRegistry {
    /// The indexed volumes now.
    pub(crate) fn current(&self) -> VolumeSet {
        self.0.read().clone()
    }

    /// Indexes `volume` from the next snapshot on.
    pub(crate) fn add(&self, volume: Arc<VolumeState>) {
        {
            let mut set = self.0.write();
            let mut volumes = Vec::clone(&set);
            volumes.push(volume);
            *set = Arc::new(volumes);
        }
        announce_change();
    }

    /// Leaves the volume `guid` out of the next snapshot on.
    pub(crate) fn remove(&self, guid: &str) {
        {
            let mut set = self.0.write();
            let volumes = set.iter().filter(|v| v.guid != guid).cloned().collect();
            *set = Arc::new(volumes);
        }
        announce_change();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const C: &str = r"\\?\Volume{0000000c-0000-0000-0000-000000000000}\";
    const D: &str = r"\\?\Volume{0000000d-0000-0000-0000-000000000000}\";
    const E: &str = r"\\?\Volume{0000000e-0000-0000-0000-000000000000}\";

    fn guids(set: &VolumeSet) -> Vec<&str> {
        set.iter().map(|v| v.guid.as_str()).collect()
    }

    /// A volume is served the moment it is found, as a volume that is still
    /// indexing: every result drawn from it is flagged incomplete until its
    /// first build, and a tail started on it knows no index exists yet.
    #[test]
    fn a_found_volume_is_bootstrapping_until_its_first_build() {
        let found = VolumeState::found(C.into(), vec![r"C:\".into()]);
        assert_eq!(*found.phase.read(), VolumePhase::Bootstrapping);
        assert!(!found.phase.read().is_complete());
        assert!(!found.indexed.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(found.mount_prefix(), r"C:\");
    }

    /// A snapshot taken before a change keeps answering as it was, and the
    /// volumes that stay keep their order, which the page builder's lock
    /// order relies on.
    #[test]
    fn snapshots_keep_their_volumes_and_the_order_survives_changes() {
        let registry = VolumeRegistry::default();
        for guid in [C, D, E] {
            registry.add(Arc::new(VolumeState::found(guid.into(), Vec::new())));
        }
        let before = registry.current();

        registry.remove(D);
        assert_eq!(guids(&before), [C, D, E], "a taken snapshot never changes");
        assert_eq!(guids(&registry.current()), [C, E]);

        registry.add(Arc::new(VolumeState::found(D.into(), Vec::new())));
        assert_eq!(guids(&registry.current()), [C, E, D]);
        registry.remove("not indexed");
        assert_eq!(guids(&registry.current()), [C, E, D]);
    }
}
