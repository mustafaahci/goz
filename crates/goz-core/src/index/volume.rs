//! The per-volume index and the rules that mutate it.
//!
//! [`VolumeIndex`] owns the entry table, name arena, and FRN map, and applies
//! USN ops and FILE_LAYOUT enrichment to them. The invariants it upholds:
//!
//! - Paths are never stored. They are rebuilt on demand by walking parent
//!   links, which makes a directory rename O(1): renaming a directory touches
//!   exactly its own entry, and every descendant's path is correct on the next
//!   reconstruction.
//! - Every op is idempotent. USN reason bits accumulate per open-close window,
//!   so the same logical change reappears; applying a batch twice equals
//!   applying it once.
//! - Orphans are never dropped. An unknown parent becomes a placeholder under
//!   lost+found, filled in when the parent's real record arrives, and a removed
//!   folder's children move there until their own records re-parent them.
//! - Slot reuse can never alias. The FRN map is keyed by record number and
//!   every hit is re-checked against the full FRN (sequence included).
//! - Folder totals stay exact. While folder sizes are on, every folder's size
//!   slot holds the total of the files below it: each change carries its
//!   difference up the parent links, O(depth), a folder move included, and
//!   every compaction sums them all again.
//! - No growth under the write lock. A buffer that must grow is copied while
//!   queries keep reading ([`VolumeIndex::plan_room`]), and so are a
//!   compaction and a merge of the name order; the write lock only swaps.

use super::store::{
    CompactedNames, ComponentBytes, EntryFlags, EntryRoom, EntryTable, FrnMap, FrnRoom,
    MTIME_UNKNOWN, NameId, NameMerge, NamePair, NameRoom, NameStore, SIZE_UNKNOWN,
};
use crate::layout::LayoutFile;
use crate::types::{EntryIdx, Frn, NIL};
use crate::usn::record::FILE_ATTRIBUTE_DIRECTORY;
use crate::usn::{ParsedUsnRecord, UsnOp, ops_for};
use hashbrown::HashTable;
#[cfg(test)]
use rustc_hash::FxHashMap;
use smallvec::SmallVec;

/// NTFS reserves MFT record 5 for the volume root directory.
pub const NTFS_ROOT_FRN: Frn = Frn(0x0005_0000_0000_0005);

/// Synthetic name for the lost+found root (never collides: `$` names are NTFS
/// metafiles, and this FRN is unreachable).
const LOST_FOUND_NAME: &[u8] = b"$LostFound";
/// Synthetic name a placeholder carries until its real record fills it in.
const PLACEHOLDER_NAME: &[u8] = b"$Placeholder";
/// FRN for the synthetic lost+found root (record number above any real MFT
/// record; never appears in a USN record).
const LOST_FOUND_FRN: Frn = Frn(u64::MAX);

/// Maximum parent hops before [`VolumeIndex::path_of`] declares a cycle. Deep
/// but bounded; real NTFS trees are far shallower.
pub(crate) const MAX_PATH_HOPS: usize = 4096;

/// The `dir_children` hash of a folder's key, its parent and folded name.
/// One fixed function for insert, remove, lookup and the table's rehash:
/// they must agree or the scope index silently loses folders.
fn dir_key_hash(parent: EntryIdx, folded: &[u8]) -> u64 {
    use core::hash::{Hash, Hasher};
    let mut h = rustc_hash::FxHasher::default();
    parent.hash(&mut h);
    folded.hash(&mut h);
    h.finish()
}

/// [`dir_key_hash`] of folder `d`, from its parent and name as the columns
/// hold them now: what the scope index rehashes by.
fn dir_hash_of(entries: &EntryTable, names: &NameStore, d: EntryIdx) -> u64 {
    dir_key_hash(entries.parent(d), names.folded_bytes_of(entries.name_id(d)))
}

/// The most MFT records [`VolumeIndex::reserve_for_records`] sizes for: an
/// [`EntryIdx`] is 32 bits, and a number past this is no real volume's.
const MAX_RESERVED_RECORDS: usize = 1 << 30;

/// A name plus whether it required WTF-8 (contained unpaired surrogates).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WtfName {
    pub bytes: Vec<u8>,
    pub lossy: bool,
}

impl WtfName {
    pub fn new(bytes: Vec<u8>, lossy: bool) -> Self {
        Self { bytes, lossy }
    }
}

/// A compaction built off the exclusive lock, ready to install.
///
/// Opaque: only [`VolumeIndex::plan_compaction`] makes one and only
/// [`VolumeIndex::apply_compaction`] consumes it, and a plan built before the
/// index last changed is dropped unused, so a plan can never be applied to an
/// index it was not built from. After the install it holds the old buffers,
/// for the caller to free once the write lock is released.
pub struct CompactPlan {
    /// [`VolumeIndex::epoch`] when planned.
    epoch: u64,
    /// The compacted names, when the arenas were worth compacting.
    names: Option<CompactedNames>,
    /// The name-id column rewritten through the names' remap.
    name_ids: Vec<NameId>,
    /// Folders whose stored total is not their files' sum, with the sum:
    /// none on a healthy index (see [`VolumeIndex::folder_sizes`]).
    folder_fixes: Vec<(EntryIdx, u64)>,
}

impl CompactPlan {
    /// How many folder totals the compaction corrects: drift the live
    /// updates let in, which a healthy index never has.
    pub fn folder_fixes(&self) -> usize {
        self.folder_fixes.len()
    }
}

/// The waiting names merged into the name order off the write lock (see
/// [`VolumeIndex::plan_name_merge`]). After the install it holds the old
/// order, for the caller to free once the lock is released.
pub struct NameMergePlan {
    epoch: u64,
    merge: NameMerge,
}

/// Folder totals summed off the write lock (see
/// [`VolumeIndex::plan_folder_sizes`]).
pub struct FolderSizePlan {
    epoch: u64,
    fixes: Vec<(EntryIdx, u64)>,
}

/// The most one change can add to an index, so that the buffers it would
/// grow are grown before it, off the write lock (see
/// [`VolumeIndex::plan_room`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Room {
    /// Entries.
    pub entries: usize,
    /// Distinct names.
    pub names: usize,
    /// Bytes of original-case names, separators included.
    pub raw_bytes: usize,
    /// Bytes of folded names, separators included.
    pub folded_bytes: usize,
    /// Folders the scope index takes.
    pub folders: usize,
    /// FRN map records.
    pub records: usize,
    /// The highest MFT record number.
    pub max_record: u64,
}

impl Room {
    /// The most a journal batch of `records` can add: each record its own
    /// entry and a placeholder for a parent not seen yet, their names, and a
    /// folder. A folded name is bounded at three times its bytes, which
    /// holds for every name but ones of compatibility characters that
    /// decompose further, whose growth the apply then makes under the lock.
    pub fn for_records(records: &[ParsedUsnRecord]) -> Room {
        let mut room = Room::default();
        for r in records {
            let len = r.name.len();
            room.entries += 2;
            room.names += 2;
            room.raw_bytes += len + 1 + PLACEHOLDER_NAME.len() + 1;
            room.folded_bytes += 3 * len + 1 + PLACEHOLDER_NAME.len() + 1;
            room.folders += 1;
            room.records += 2;
            room.max_record = room
                .max_record
                .max(r.frn.record_number())
                .max(r.parent_frn.record_number());
        }
        room
    }

    /// The most a reconcile of hard links can add: an entry, a name and its
    /// bytes per link, `link_bytes` being each link name's length in bytes.
    pub fn for_links(link_bytes: impl IntoIterator<Item = usize>) -> Room {
        let mut room = Room::default();
        for len in link_bytes {
            room.entries += 1;
            room.names += 1;
            room.raw_bytes += len + 1;
            room.folded_bytes += 3 * len + 1;
            room.records += 1;
        }
        room
    }
}

/// Grown copies of the buffers a change would grow (see
/// [`VolumeIndex::plan_room`]), built off the write lock. After the install
/// it holds the old buffers, for the caller to free once the lock is
/// released.
pub struct RoomPlan {
    epoch: u64,
    entries: Option<EntryRoom>,
    names: Option<NameRoom>,
    frn: Option<FrnRoom>,
    /// The scope index's length when planned, and the grown table.
    dirs: Option<(usize, HashTable<EntryIdx>)>,
}

/// One resolved hard link: the containing directory's entry plus the link
/// name. Passed to [`VolumeIndex::reconcile_links`] by the daemon after it
/// walks `FindFirstFileNameW`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkTarget {
    pub parent: EntryIdx,
    pub name: WtfName,
}

/// Result of walking parent links to build a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathStatus {
    /// A normal path rooted at the volume root.
    Ok,
    /// The chain terminates in lost+found (an unresolved orphan).
    InLostFound,
    /// The hop cap was hit: corrupt/raced parent links; the volume should
    /// rescan rather than trust this path.
    CycleDetected,
    /// The entry was removed. Only an index held across changes, such as the
    /// daemon's cached `-path` scope, can name one.
    Removed,
}

/// A non-fatal irregularity observed during apply, counted for diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Anomaly {
    /// A delete arrived for an FRN not in the index.
    DeleteOfUnknown { frn: Frn },
}

/// Running counters for observability, surfaced per volume in `Status`.
///
/// These are the only outward sign that the index and the volume have drifted:
/// individually they are normal on a busy volume, but a number that climbs
/// steadily is the signal that something is wrong.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexStats {
    pub placeholders_created: u64,
    pub delete_of_unknown: u64,
    pub rename_old_seen: u64,
    pub stale_slots: u64,
    /// Hard-link changes the daemon saw but could not reconcile.
    ///
    /// Reconciling walks the file's real link set from Win32 and applies it
    /// (see [`VolumeIndex::reconcile_links`]). This counts the walks that could
    /// NOT complete (one that failed or stopped part way, a file pending
    /// delete, a link folder the walk may not read, or a link in a folder not
    /// indexed yet): those are skipped rather than reconciled to a partial set,
    /// so a few names may be stale until the next rescan. A file that is gone
    /// is not counted. Counted so the gap is visible in `Status` rather than
    /// silent.
    pub link_reconciles_dropped: u64,
}

/// Where one volume index's heap bytes live, component by component.
/// `used` is live data; `allocated` includes capacity slack. The point of
/// this existing at all: memory work is a fight between components measured
/// in hundreds of MB, and without a per-component breakdown every change is
/// guesswork against a single opaque process number.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IndexMemory {
    /// Struct-of-arrays entry columns (frn/parent/name/flags/size/mtime/links).
    pub entries: ComponentBytes,
    /// Original-case name bytes.
    pub arena_raw: ComponentBytes,
    /// Case-folded name bytes (the query haystack).
    pub arena_folded: ComponentBytes,
    /// FRN → entry map.
    pub frn_map: ComponentBytes,
    /// Per-unique-name tables (pairs, refcounts, chain heads, intern table).
    pub name_tables: ComponentBytes,
    /// The `(parent, folded name) → folder` scope index (bucket estimate).
    pub dir_children: ComponentBytes,
    /// "dense" / "sparse" (which `FrnMap` backing is in use).
    pub frn_map_kind: &'static str,
}

impl IndexMemory {
    pub fn total(&self) -> ComponentBytes {
        self.entries
            + self.arena_raw
            + self.arena_folded
            + self.frn_map
            + self.name_tables
            + self.dir_children
    }
}

/// Side effects a batch produced that the daemon must service out of band.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApplyOutcome {
    /// FRNs whose size/mtime changed and need an external stat, only those
    /// still in the index once the batch is applied.
    pub needs_stat: Vec<Frn>,
    /// FRNs whose hard links changed and need an external link-set walk.
    ///
    /// The daemon resolves each FRN's real link set with a Win32 link walk and
    /// applies it through [`VolumeIndex::reconcile_links`]. A walk that cannot
    /// complete is skipped and counted in `IndexStats::link_reconciles_dropped`
    /// rather than applied as a partial set.
    pub needs_link_reconcile: Vec<Frn>,
    /// Irregularities worth logging.
    pub anomalies: Vec<Anomaly>,
    /// The name arenas are dirty enough to be worth compacting. The caller
    /// should [`VolumeIndex::plan_compaction`] off the write lock and then
    /// [`VolumeIndex::apply_compaction`].
    pub wants_compact: bool,
    /// Enough names wait outside the name order to merge them in. The
    /// caller should [`VolumeIndex::plan_name_merge`] off the write lock and
    /// then [`VolumeIndex::install_name_merge`].
    pub wants_merge: bool,
}

enum UpsertResult {
    Ok,
    /// The FRN has multiple links; a rename cannot be localized from the NEW
    /// record alone, so the daemon must reconcile the file's links.
    NeedsReconcile,
}

/// One indexed volume.
pub struct VolumeIndex {
    entries: EntryTable,
    names: NameStore,
    by_frn: FrnMap,
    root: EntryIdx,
    lost_found: EntryIdx,
    generation: u64,
    stats: IndexStats,
    /// Entries structurally mutated since the last [`Self::take_write_count`].
    dirty: u64,
    /// Persistent `(parent entry, folded name) → folder` index over real
    /// FOLDERS only, maintained on every mutation. Turns `-path` scope
    /// resolution into an O(depth) walk instead of an O(all-entries) scan.
    /// The table holds only the folder's slot, 4 bytes and a control byte a
    /// bucket: the key is hashed from the folder's own parent and name in
    /// the columns ([`dir_hash_of`]), so it is never stored. It was a map
    /// keyed by `(parent, name hash)` holding a one-slot SmallVec, 41 bytes a
    /// bucket: 43 MB on a 5.1M-entry volume, where this takes 5. Lookups
    /// compare the candidate's parent and folded name, so a hash collision
    /// costs a compare, never a wrong folder, and siblings that differ by
    /// case alone sit side by side. A folder leaves the table before its
    /// parent or name changes and comes back after, since the table rehashes
    /// from the columns. Placeholders and root/lost+found are excluded (they
    /// are never a scope component).
    dir_children: HashTable<EntryIdx>,
    /// The entries whose record closed a parent cycle, until
    /// [`Self::prune_cycle_suspects`] finds the cycle gone. Empty on a
    /// healthy index.
    cycle_suspects: Vec<EntryIdx>,
    /// Bumped by every change, so a plan built off the write lock
    /// ([`Self::plan_room`], [`Self::plan_compaction`],
    /// [`Self::plan_folder_sizes`]) is installed only into the index it was
    /// built from.
    epoch: u64,
    /// Whether each folder's size slot holds the total of the files below
    /// it (see [`Self::folder_sizes`]); unknown otherwise.
    folder_sizes: bool,
    /// A parent cycle let the live totals drift: walking up from inside a
    /// cycle never reaches the root. Once no cycle is left, the next
    /// compaction sums them again.
    folder_sizes_stale: bool,
}

impl VolumeIndex {
    /// Builds an empty index with a root (FRN `root_frn`) and a lost+found
    /// root. `frn_map` picks the backing storage; every caller today,
    /// the real-volume bootstrap included, passes `FrnMap::sparse()`.
    pub fn new(root_frn: Frn, mut frn_map: FrnMap) -> Self {
        let mut entries = EntryTable::new();
        let mut names = NameStore::new();

        let root_name = names.intern(b"");
        let root = entries.alloc(
            root_frn,
            NIL,
            root_name,
            EntryFlags::DIR,
            SIZE_UNKNOWN,
            MTIME_UNKNOWN,
            NIL,
        );
        frn_map.set(root_frn.record_number(), root);

        // Lost+found: synthetic, not in the FRN map (never a USN target).
        let lf_name = names.intern(LOST_FOUND_NAME);
        let lost_found = entries.alloc(
            LOST_FOUND_FRN,
            NIL,
            lf_name,
            EntryFlags::DIR.with(EntryFlags::LOST_FOUND, true),
            SIZE_UNKNOWN,
            MTIME_UNKNOWN,
            NIL,
        );

        // Both are folders.
        names.mark_folder(root_name);
        names.mark_folder(lf_name);
        let mut this = Self {
            entries,
            names,
            by_frn: frn_map,
            root,
            lost_found,
            generation: 0,
            stats: IndexStats::default(),
            dirty: 0,
            dir_children: HashTable::new(),
            cycle_suspects: Vec::new(),
            epoch: 0,
            folder_sizes: false,
            folder_sizes_stale: false,
        };
        this.link_same_name(root, root_name);
        this.link_same_name(lost_found, lf_name);
        this
    }

    /// Whether anything sits under lost+found: a placeholder for a parent
    /// whose record has not arrived, or a child of a removed folder. Such an
    /// entry has no real path. A parent cycle is the other way to lose one
    /// ([`Self::has_parent_cycle`]); [`Self::may_have_unpresentable`] covers
    /// both.
    pub fn has_orphans(&self) -> bool {
        self.entries.first_child(self.lost_found) != NIL
    }

    /// Whether a record closed a parent cycle that no record has broken yet:
    /// folders cut off from the root, whose entries have no real path.
    pub fn has_parent_cycle(&self) -> bool {
        !self.cycle_suspects.is_empty()
    }

    /// Whether any entry could fail to reconstruct to a real path: one is
    /// under lost+found, or sits in a parent cycle. False on a healthy index,
    /// where the query engine skips its per-directory presentability walk
    /// entirely.
    pub fn may_have_unpresentable(&self) -> bool {
        self.has_orphans() || self.has_parent_cycle()
    }

    pub fn root(&self) -> EntryIdx {
        self.root
    }
    pub fn lost_found(&self) -> EntryIdx {
        self.lost_found
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn stats(&self) -> &IndexStats {
        &self.stats
    }

    /// Live entry count (includes root + lost+found).
    pub fn len(&self) -> usize {
        self.entries.live()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Total allocated entry slots (live + reusable tombstones).
    pub fn allocated(&self) -> usize {
        self.entries.capacity()
    }

    /// Entries structurally mutated since the previous call. The
    /// directory-rename test asserts this is exactly 1.
    pub fn take_write_count(&mut self) -> u64 {
        core::mem::take(&mut self.dirty)
    }

    // -- bootstrap ingestion ---------------------------------------------

    /// Ingests one `FSCTL_ENUM_USN_DATA` record (bootstrap): the file exists
    /// with this name/parent. Idempotent.
    pub fn insert_enum(&mut self, rec: &ParsedUsnRecord) {
        self.epoch += 1;
        self.upsert_link(
            rec.frn,
            rec.parent_frn,
            &rec.name,
            rec.name_lossy,
            rec.is_dir(),
        );
    }

    /// Applies FILE_LAYOUT enrichment: sets size/mtime and reconciles the
    /// file's hard-link names (dropping 8.3 DOS-only aliases).
    pub fn enrich(&mut self, file: &LayoutFile) {
        self.epoch += 1;
        let is_dir = file.attributes & FILE_ATTRIBUTE_DIRECTORY != 0;

        // Enrichment is ADD-ONLY: it never tombstones or replaces an existing
        // entry. This is critical: the entry may be a directory that already
        // has children pointing at its EntryIdx, and (during the bootstrap
        // race) its name may legitimately differ from the LAYOUT snapshot
        // because a rename landed between the ENUM and LAYOUT passes. Tombstone
        // + realloc here would orphan those children under a reused slot.
        if self.head_of(file.frn).is_none() {
            // No entry of this FRN exists yet (nothing to orphan), though a
            // previous tenant of its record may: build the chain from LAYOUT.
            let desired: Vec<LinkTarget> = file
                .names
                .iter()
                .filter(|n| !n.dos_only)
                .map(|n| LinkTarget {
                    parent: self.resolve_link_parent(file.frn, n.parent_frn),
                    name: WtfName::new(n.name.clone(), n.name_lossy),
                })
                .collect();
            if desired.is_empty() {
                return;
            }
            self.set_chain_links(file.frn, is_dir, &desired);
        }

        // Add hard-link names ENUM did not report (files only: a directory
        // has exactly one link and its name is authoritative from ENUM/the
        // journal, so we never touch it here).
        if !is_dir {
            for n in file.names.iter().filter(|n| !n.dos_only) {
                let parent_idx = self.resolve_link_parent(file.frn, n.parent_frn);
                if !self.chain_has_link(file.frn, parent_idx, &n.name) {
                    self.add_link(file.frn, parent_idx, &n.name, n.name_lossy);
                }
            }
        }

        // Set size/mtime on EVERY link of the file, not just the head: a query
        // may return any of a hard-linked file's names, and they all describe
        // the same bytes. (A folder's size is its files' total, never read.)
        if let Some(chain_head) = self.head_of(file.frn) {
            let mut cur = chain_head;
            while cur != NIL {
                if !is_dir && let Some(sz) = file.size {
                    self.set_file_size(cur, sz);
                }
                if let Some(mt) = file.mtime_ft {
                    self.entries.set_mtime(cur, mt);
                }
                cur = self.entries.next_link(cur);
            }
        }
    }

    /// Ingests one FILE_LAYOUT record when FILE_LAYOUT is the bootstrap's
    /// only pass: the first real (non 8.3) name stands in for the ENUM record,
    /// so a directory an earlier child left as a placeholder gets its name,
    /// then [`Self::enrich`] adds the other hard-link names, the size and the
    /// date. Idempotent, like [`Self::insert_enum`].
    ///
    /// The root's own record names the root as its parent, which no ENUM
    /// record does. Taking that name would make the root its own parent and
    /// leave every entry without a path, so the root is only enriched.
    pub fn insert_layout(&mut self, file: &LayoutFile) {
        let is_dir = file.attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
        if let Some(name) = file
            .names
            .iter()
            .find(|n| !n.dos_only)
            .filter(|n| n.parent_frn != file.frn)
        {
            self.upsert_link(
                file.frn,
                name.parent_frn,
                &name.name,
                name.name_lossy,
                is_dir,
            );
        }
        self.enrich(file);
    }

    // -- live apply -------------------------------------------------------

    /// Applies a batch of journal records, returning the side effects the
    /// daemon must service. Bumps the generation once and compacts the arena
    /// if it has accumulated too much garbage.
    pub fn apply_batch(&mut self, records: &[ParsedUsnRecord]) -> ApplyOutcome {
        self.epoch += 1;
        let mut outcome = ApplyOutcome::default();
        for rec in records {
            let ops: SmallVec<[UsnOp; 3]> = ops_for(rec);
            for op in ops {
                match op {
                    UsnOp::Upsert {
                        frn,
                        parent,
                        name,
                        name_lossy,
                        is_dir,
                    } => {
                        if let UpsertResult::NeedsReconcile =
                            self.upsert_link(frn, parent, &name, name_lossy, is_dir)
                        {
                            outcome.needs_link_reconcile.push(frn);
                        }
                    }
                    UsnOp::Delete { frn } => {
                        if !self.delete_frn(frn) {
                            self.stats.delete_of_unknown += 1;
                            outcome.anomalies.push(Anomaly::DeleteOfUnknown { frn });
                        }
                    }
                    UsnOp::LinkDirty { frn } => outcome.needs_link_reconcile.push(frn),
                    UsnOp::StatDirty { frn } => outcome.needs_stat.push(frn),
                    UsnOp::RenameOldSeen { .. } => self.stats.rename_old_seen += 1,
                }
            }
        }
        // Names interned since the last merge join the name order once enough
        // have gathered (see `NameStore::wants_merge`). The merge is O(names),
        // 25 ms on 1.6M names, so the caller makes it off the write lock
        // (`wants_merge`); only a backlog eight times that, which a caller
        // that never merges leaves, is merged here.
        if self.names.wants_merge_now() {
            self.names.merge_unranked();
        }
        outcome.wants_merge = self.names.wants_merge();
        // A stat of an entry the batch removed would be a blocking open for
        // nothing, and deleting a tree asks for one per folder in it.
        outcome
            .needs_stat
            .retain(|&frn| self.head_of(frn).is_some());
        self.prune_cycle_suspects();
        self.generation += 1;
        // Compaction is NOT performed here: it is reported so the caller can do
        // the expensive part off the write lock. Doing it inline made a single
        // journal batch block every query for ~184 ms.
        outcome.wants_compact = self.names.should_compact() || self.folder_sizes_want_resum();
        outcome
    }

    /// Records that `n` hard-link walks could not complete and so were not
    /// applied, so the gap is visible in `Status` rather than silent. The
    /// successful walks are applied by [`Self::reconcile_links`].
    pub fn note_link_reconciles_dropped(&mut self, n: u64) {
        self.stats.link_reconciles_dropped += n;
    }

    /// Reconciles the hard-link set of `frn` to exactly `actual` (the daemon's
    /// `FindFirstFileNameW` result). An empty `actual` deletes the file.
    pub fn reconcile_links(&mut self, frn: Frn, actual: &[LinkTarget]) {
        self.epoch += 1;
        if actual.is_empty() {
            self.delete_frn(frn);
            return;
        }
        let is_dir = self
            .head_of(frn)
            .map(|h| self.entries.flags(h).contains(EntryFlags::DIR))
            .unwrap_or(false);
        self.set_chain_links(frn, is_dir, actual);
    }

    /// Live enricher: sets a file's fresh `size`/`mtime` on every entry in its
    /// hard-link chain (a query may return any of its names). USN records carry
    /// neither, so the daemon stats the FRN after a create/data/basic-info
    /// change and applies it here. A folder takes the date only: its size is
    /// its files' total. A no-op if the FRN is unknown (e.g. deleted between
    /// apply and stat).
    pub fn set_stat(&mut self, frn: Frn, size: u64, mtime: i64) {
        self.epoch += 1;
        let Some(head) = self.head_of(frn) else {
            return;
        };
        let is_dir = self.entries.flags(head).contains(EntryFlags::DIR);
        let mut cur = head;
        while cur != NIL {
            if !is_dir {
                self.set_file_size(cur, size);
            }
            self.entries.set_mtime(cur, mtime);
            cur = self.entries.next_link(cur);
        }
    }

    // -- path reconstruction ---------------------------------------------

    /// Appends `dir\dir\name` (no volume/mount prefix) for `idx` into `out` as
    /// WTF-8, returning how the walk terminated.
    pub fn path_of(&self, idx: EntryIdx, out: &mut Vec<u8>) -> PathStatus {
        // A live entry's parent is always live, so only the start can be dead.
        if self.entries.is_tombstone(idx) {
            return PathStatus::Removed;
        }
        let mut chain: SmallVec<[EntryIdx; 32]> = SmallVec::new();
        let mut cur = idx;
        for _ in 0..MAX_PATH_HOPS {
            chain.push(cur);
            let p = self.entries.parent(cur);
            if p == NIL {
                break;
            }
            cur = p;
        }
        // The loop above pushes `cur` on its first iteration (MAX_PATH_HOPS > 0),
        // so `chain` always holds at least the starting entry.
        let top = *chain
            .last()
            .expect("path_of chain always contains the starting entry");
        if self.entries.parent(top) != NIL {
            return PathStatus::CycleDetected;
        }
        let in_lost_found = top == self.lost_found;

        // Emit from the top down, skipping the volume root (its name is empty
        // and the mount prefix is added by the daemon).
        for &e in chain.iter().rev() {
            if e == self.root {
                continue;
            }
            if !out.is_empty() {
                out.push(b'\\');
            }
            out.extend_from_slice(self.names.raw_bytes(self.entries.name_id(e)));
        }

        if in_lost_found {
            PathStatus::InLostFound
        } else {
            PathStatus::Ok
        }
    }

    /// The chain-head entry for `frn`, if present and not stale.
    pub fn head_of(&self, frn: Frn) -> Option<EntryIdx> {
        let head = self.by_frn.get(frn.record_number())?;
        (self.entries.frn(head) == frn).then_some(head)
    }

    /// The entry at `idx` if it is still alive and still the one with FRN
    /// `frn`. The daemon resolves a query's hits this way when it encodes a
    /// page: between the scan and the page a journal apply may have reused
    /// the slot, or a rescan may have swapped the whole index, so a bare
    /// `idx` can name another file or nothing. `None` means the row is gone
    /// and is dropped. Never panics, whatever `idx` is.
    pub fn entry_if_frn(&self, idx: EntryIdx, frn: Frn) -> Option<EntryView<'_>> {
        if (idx as usize) >= self.entries.capacity() || self.entries.is_tombstone(idx) {
            return None;
        }
        (self.entries.frn(idx) == frn).then(|| self.entry(idx))
    }

    /// Looks up a child directory by its folded name via the persistent scope
    /// index: O(1), no scan. Case-insensitive by construction (the key hashes
    /// the folded name); the query engine's `resolve_scope` walks it component
    /// by component. Every candidate under the hash is verified against the
    /// real folded name, so a collision costs one extra compare, never a
    /// wrong directory.
    pub fn child_dir(&self, parent: EntryIdx, folded_name: Vec<u8>) -> Option<EntryIdx> {
        self.child_dirs(parent, &folded_name).next()
    }

    /// Every child directory of `parent` whose folded name is `folded_name`:
    /// more than one only where siblings differ by case alone, which a
    /// case-sensitive NTFS directory allows. A case-insensitive path names
    /// them all.
    pub(crate) fn child_dirs<'a>(
        &'a self,
        parent: EntryIdx,
        folded_name: &'a [u8],
    ) -> impl Iterator<Item = EntryIdx> + 'a {
        self.dir_children
            .iter_hash(dir_key_hash(parent, folded_name))
            .copied()
            .filter(move |&d| {
                self.entries.parent(d) == parent && self.folded_name(d) == folded_name
            })
    }

    /// Read access to a live entry's fields (for the query engine / tests).
    pub fn entry(&self, idx: EntryIdx) -> EntryView<'_> {
        EntryView { index: self, idx }
    }

    /// Every live entry index (includes root, lost+found, placeholders).
    pub fn live_entries(&self) -> impl Iterator<Item = EntryIdx> + '_ {
        self.entries.iter_live()
    }

    /// `true` for the root, lost+found, and any placeholder/orphan entry: the
    /// entries that do not correspond to a real, fully-resolved filesystem
    /// object.
    pub fn is_synthetic(&self, idx: EntryIdx) -> bool {
        let flags = self.entries.flags(idx);
        idx == self.root
            || idx == self.lost_found
            || flags.contains(EntryFlags::PLACEHOLDER)
            || flags.contains(EntryFlags::LOST_FOUND)
    }

    /// Forces an entry's parent link, past the cycle bookkeeping of
    /// [`Self::rewrite_entry`]. Test-only: it builds the corrupt links the
    /// cycle guards of [`Self::path_of`] and the walks must survive.
    #[cfg(test)]
    pub fn force_parent(&mut self, idx: EntryIdx, parent: EntryIdx) {
        self.epoch += 1;
        self.dir_index_remove(idx);
        self.entries.set_parent(idx, parent);
        self.dir_index_insert(idx);
    }

    /// Forces an entry's size slot, past the folder totals. Test-only: it
    /// leaves the drift a missed update would, for a compaction to correct.
    #[cfg(test)]
    pub fn force_size(&mut self, idx: EntryIdx, size: u64) {
        self.epoch += 1;
        self.entries.set_size(idx, size);
    }

    /// The invariant every mutation must preserve: the incrementally-maintained
    /// directory index equals one rebuilt from a full scan of the live entries.
    /// The model-op-tape proptest asserts this after every applied op.
    #[cfg(test)]
    pub fn dir_index_matches_rebuild(&self) -> bool {
        let mut want: Vec<EntryIdx> = self
            .entries
            .iter_live()
            .filter(|&idx| {
                let flags = self.entries.flags(idx);
                flags.contains(EntryFlags::DIR)
                    && !flags.contains(EntryFlags::PLACEHOLDER)
                    && idx != self.root
                    && idx != self.lost_found
            })
            .collect();
        let mut got: Vec<EntryIdx> = self.dir_children.iter().copied().collect();
        want.sort_unstable();
        got.sort_unstable();
        // Each folder once, and each found where its key now hashes.
        want == got
            && want.iter().all(|&d| {
                self.dir_children
                    .find(dir_hash_of(&self.entries, &self.names, d), |&x| x == d)
                    .is_some()
            })
    }

    /// Collects the reconstructed path of every real entry as a lossy `String`
    /// (a test/diagnostic convenience; orphans in lost+found are excluded).
    pub fn collect_real_paths(&self) -> Vec<String> {
        let mut paths = Vec::new();
        let mut buf = Vec::new();
        for idx in self.live_entries() {
            if self.is_synthetic(idx) {
                continue;
            }
            buf.clear();
            if let PathStatus::Ok = self.path_of(idx, &mut buf) {
                paths.push(crate::wtf8::to_string_lossy(&buf));
            }
        }
        paths
    }

    // -- internals --------------------------------------------------------

    /// Pushes `idx` onto the front of `id`'s same-name chain. Every entry
    /// allocation must pair with exactly one of these, and every tombstone
    /// with one [`Self::unlink_same_name`]: the chains are EXACT (a scan hit
    /// expands to precisely the live entries bearing the name), which is what
    /// spares the query path any staleness guard.
    fn link_same_name(&mut self, idx: EntryIdx, id: NameId) {
        let old_head = self.names.chain_head(id);
        self.entries.set_next_same(idx, old_head);
        self.entries.set_prev_same(idx, NIL);
        if old_head != NIL {
            self.entries.set_prev_same(old_head, idx);
        }
        self.names.set_chain_head(id, idx);
    }

    /// Pushes `idx` onto the front of its parent's child chain. Every entry
    /// allocation (and every parent change) must pair with exactly one of
    /// these; the chains make a scope subtree enumerable, so a folder-scoped
    /// query walks the folder instead of scanning the volume.
    fn link_child(&mut self, idx: EntryIdx) {
        let parent = self.entries.parent(idx);
        if parent == NIL {
            return; // the root and lost+found have no parent chain
        }
        let old_head = self.entries.first_child(parent);
        self.entries.set_next_child(idx, old_head);
        self.entries.set_prev_child(idx, NIL);
        if old_head != NIL {
            self.entries.set_prev_child(old_head, idx);
        }
        self.entries.set_first_child(parent, idx);
    }

    /// O(1) removal from the parent's child chain. Must run BEFORE the
    /// entry's parent is overwritten or its slot tombstoned, for the same
    /// reason as [`Self::unlink_same_name`].
    fn unlink_child(&mut self, idx: EntryIdx) {
        let parent = self.entries.parent(idx);
        if parent == NIL {
            return;
        }
        let prev = self.entries.prev_child(idx);
        let next = self.entries.next_child(idx);
        if prev == NIL {
            self.entries.set_first_child(parent, next);
        } else {
            self.entries.set_next_child(prev, next);
        }
        if next != NIL {
            self.entries.set_prev_child(next, prev);
        }
        self.entries.set_next_child(idx, NIL);
        self.entries.set_prev_child(idx, NIL);
    }

    /// Moves every child of a directory that is being removed under
    /// lost+found. A removed directory still has children only after a missed
    /// record (a child's delete or move, or the directory's own delete before
    /// NTFS gave its record to another file) or while a hard link in it waits
    /// for its link walk. Moving them keeps every live entry's parent live, so
    /// no path runs through a freed slot or the entry that reuses it. They have
    /// no full path until their own records re-parent them.
    fn orphan_children(&mut self, dir: EntryIdx) {
        let mut cur = self.entries.first_child(dir);
        while cur != NIL {
            let next = self.entries.next_child(cur);
            self.dir_index_remove(cur); // keyed under `dir`
            self.entries.set_parent(cur, self.lost_found);
            self.link_child(cur);
            self.dir_index_insert(cur);
            self.carry(self.lost_found, self.weight(cur), 0);
            self.dirty += 1;
            cur = next;
        }
        self.entries.set_first_child(dir, NIL);
    }

    /// Removes one entry: takes it out of the scope index and both chains,
    /// moves its children under lost+found, releases its name and frees its
    /// slot. The one removal path, so the order cannot drift between callers.
    fn remove_entry(&mut self, idx: EntryIdx) {
        // The whole subtree leaves the folders above; the children it still
        // holds come back under lost+found (`orphan_children`).
        self.carry(self.entries.parent(idx), 0, self.weight(idx));
        self.dir_index_remove(idx); // while the name and parent are readable
        self.unlink_same_name(idx);
        self.unlink_child(idx);
        self.orphan_children(idx);
        self.names.release(self.entries.name_id(idx));
        self.entries.tombstone(idx);
        self.dirty += 1;
    }

    /// O(1) removal from the doubly-linked same-name chain. Must run BEFORE
    /// the entry's name id is overwritten or its slot tombstoned: a stale
    /// chain pointer surviving into a reused slot can cross-link chains.
    fn unlink_same_name(&mut self, idx: EntryIdx) {
        let id = self.entries.name_id(idx);
        let prev = self.entries.prev_same(idx);
        let next = self.entries.next_same(idx);
        if prev == NIL {
            self.names.set_chain_head(id, next);
        } else {
            self.entries.set_next_same(prev, next);
        }
        if next != NIL {
            self.entries.set_prev_same(next, prev);
        }
        self.entries.set_next_same(idx, NIL);
        self.entries.set_prev_same(idx, NIL);
    }

    /// Records a real directory's current `(parent, folded name) → idx` in the
    /// persistent scope index. No-op for files, placeholders, and
    /// root/lost+found (none are ever a `-path` component). Reads the entry's
    /// CURRENT fields, so call it AFTER the entry's parent/name/flags are set.
    fn dir_index_insert(&mut self, idx: EntryIdx) {
        let flags = self.entries.flags(idx);
        if !flags.contains(EntryFlags::DIR)
            || flags.contains(EntryFlags::PLACEHOLDER)
            || idx == self.root
            || idx == self.lost_found
        {
            return;
        }
        let Self {
            dir_children,
            entries,
            names,
            ..
        } = self;
        let hash = dir_hash_of(entries, names, idx);
        if dir_children.find(hash, |&d| d == idx).is_none() {
            dir_children.insert_unique(hash, idx, |&d| dir_hash_of(entries, names, d));
        }
    }

    /// Removes a directory's current `(parent, folded name)` key. Call BEFORE a
    /// rename/tombstone overwrites its parent/name (it reads them). Guarded to
    /// only remove a key that still points at THIS entry, so it never disturbs a
    /// sibling that legitimately owns the same key.
    fn dir_index_remove(&mut self, idx: EntryIdx) {
        if !self.entries.flags(idx).contains(EntryFlags::DIR) {
            return;
        }
        let hash = dir_hash_of(&self.entries, &self.names, idx);
        if let Ok(found) = self.dir_children.find_entry(hash, |&d| d == idx) {
            found.remove();
        }
    }

    fn upsert_link(
        &mut self,
        frn: Frn,
        parent_frn: Frn,
        name: &[u8],
        name_lossy: bool,
        is_dir: bool,
    ) -> UpsertResult {
        let record = frn.record_number();
        // Record number reused by a different FRN: reclaim the stale chain
        // first, so the parent resolved below is never a slot it frees.
        if let Some(head) = self.by_frn.get(record)
            && self.entries.frn(head) != frn
        {
            self.reclaim_stale_slot(record, head);
        }
        let parent_idx = self.resolve_link_parent(frn, parent_frn);

        if let Some(head) = self.by_frn.get(record) {
            if self.entries.next_link(head) == NIL {
                self.rewrite_entry(head, parent_idx, name, name_lossy, is_dir);
                return UpsertResult::Ok;
            }
            return UpsertResult::NeedsReconcile;
        }

        let id = self.names.intern(name);
        if is_dir {
            self.names.mark_folder(id);
        }
        let flags = EntryFlags::EMPTY
            .with(EntryFlags::DIR, is_dir)
            .with(EntryFlags::NON_UNICODE_NAME, name_lossy);
        // A new file adds nothing to its folders until its size is read; a
        // new folder holds nothing yet.
        let size = if is_dir {
            self.new_folder_size()
        } else {
            SIZE_UNKNOWN
        };
        let idx = self
            .entries
            .alloc(frn, parent_idx, id, flags, size, MTIME_UNKNOWN, NIL);
        self.link_same_name(idx, id);
        self.link_child(idx);
        self.by_frn.set(record, idx);
        self.dir_index_insert(idx);
        self.dirty += 1;
        UpsertResult::Ok
    }

    fn rewrite_entry(
        &mut self,
        idx: EntryIdx,
        parent_idx: EntryIdx,
        name: &[u8],
        name_lossy: bool,
        is_dir: bool,
    ) {
        // What the entry adds to the folders above it, and where it sat: a
        // move takes it off the old folders and onto the new ones.
        let (old_parent, old_weight) = (self.entries.parent(idx), self.weight(idx));
        let was_dir = self.entries.flags(idx).contains(EntryFlags::DIR);
        // Drop the OLD (parent, name) dir key before the fields are overwritten;
        // re-index under the NEW ones afterward. This covers rename, a
        // placeholder being filled in, and dir↔file transitions uniformly.
        self.dir_index_remove(idx);
        self.unlink_same_name(idx);
        self.names.release(self.entries.name_id(idx));
        let id = self.names.intern(name);
        if is_dir {
            self.names.mark_folder(id);
        }
        self.entries.set_name_id(idx, id);
        self.link_same_name(idx, id);
        // A parent at or below the entry closes a cycle cut off from the root,
        // which only a stale link gives: a snapshot read while folders moved,
        // a missed record, corrupt data. The link stays as the record says,
        // since the moves still to be replayed may belong to other entries
        // and heal it only that way; the entry is noted so queries check
        // presentability until then (see `prune_cycle_suspects`).
        if self.is_at_or_below(parent_idx, idx) {
            if !self.cycle_suspects.contains(&idx) {
                self.cycle_suspects.push(idx);
            }
            // The totals walked up from inside a cycle never reach the root.
            self.folder_sizes_stale = true;
        }
        // Unlink from the OLD parent's child chain while `parent` still
        // points there, then relink under the new parent.
        self.unlink_child(idx);
        self.entries.set_parent(idx, parent_idx);
        self.link_child(idx);
        let flags = self
            .entries
            .flags(idx)
            .with(EntryFlags::DIR, is_dir)
            .with(EntryFlags::NON_UNICODE_NAME, name_lossy)
            .with(EntryFlags::PLACEHOLDER, false);
        self.entries.set_flags(idx, flags);
        if was_dir != is_dir {
            // A file's size is not a folder's total, nor the reverse: a new
            // folder sums what it holds, a new file waits for its stat.
            let size = if is_dir && self.folder_sizes {
                self.children(idx)
                    .map(|c| self.weight(c))
                    .fold(0, u64::wrapping_add)
            } else {
                SIZE_UNKNOWN
            };
            self.entries.set_size(idx, size);
        }
        let new_weight = self.weight(idx);
        if old_parent != parent_idx || old_weight != new_weight {
            self.carry(old_parent, 0, old_weight);
            self.carry(parent_idx, new_weight, 0);
        }
        self.dir_index_insert(idx);
        self.dirty += 1;
    }

    /// Drops the cycle suspects no cycle runs through any more: their parents
    /// lead out again, to the root or to lost+found (where
    /// [`Self::has_orphans`] covers them), or the entry was removed, which
    /// leaves its slot no parent. Every cycle still holds a suspect: only
    /// [`Self::rewrite_entry`] closes one, noting the entry that did. A slot
    /// reused by another entry stays only while a cycle runs through it,
    /// which the rewrite that closed that cycle noted as well.
    fn prune_cycle_suspects(&mut self) {
        let mut suspects = core::mem::take(&mut self.cycle_suspects);
        suspects.retain(|&idx| self.is_at_or_below(self.entries.parent(idx), idx));
        self.cycle_suspects = suspects;
    }

    /// Whether `node` is `ancestor` or below it by parent links, giving up
    /// after [`MAX_PATH_HOPS`] as [`Self::path_of`] does.
    fn is_at_or_below(&self, mut node: EntryIdx, ancestor: EntryIdx) -> bool {
        for _ in 0..MAX_PATH_HOPS {
            if node == ancestor {
                return true;
            }
            if node == NIL || node == self.root || node == self.lost_found {
                return false;
            }
            node = self.entries.parent(node);
        }
        false
    }

    /// Reclaims a record number NTFS has re-tenanted: tombstones the stale chain
    /// and drops the map entry, so the dead names stop matching and the slots
    /// return to the free list.
    ///
    /// Every site that finds a live `by_frn` entry whose FRN does not match must
    /// call this before overwriting the map. It exists as one function precisely
    /// so the callers cannot drift apart again.
    /// Reported through `IndexStats::stale_slots` rather than an `Anomaly`: the
    /// callers are deep helpers with no access to the batch's
    /// `ApplyOutcome`, and buffering per-event anomalies on the index would grow
    /// unbounded across a multi-million-entry bootstrap. The counter reaches
    /// `Status` either way.
    fn reclaim_stale_slot(&mut self, record: u64, head: EntryIdx) {
        self.tombstone_chain(head);
        self.by_frn.remove(record);
        self.stats.stale_slots += 1;
    }

    /// Resolves the parent of a name of `frn`. A parent with `frn`'s record
    /// number and another sequence number is that record's previous tenant,
    /// which only corrupt data names: resolving it would reclaim `frn` itself
    /// or put a placeholder in its place in the map, so the name goes under
    /// lost+found, with no full path.
    fn resolve_link_parent(&mut self, frn: Frn, parent_frn: Frn) -> EntryIdx {
        if parent_frn != frn && parent_frn.record_number() == frn.record_number() {
            return self.lost_found;
        }
        self.resolve_or_placeholder_parent(parent_frn)
    }

    /// Resolves a parent FRN to its entry, synthesizing a placeholder under
    /// lost+found if the parent has not been seen yet.
    fn resolve_or_placeholder_parent(&mut self, parent_frn: Frn) -> EntryIdx {
        let record = parent_frn.record_number();
        if let Some(idx) = self.by_frn.get(record) {
            if self.entries.frn(idx) == parent_frn {
                return idx;
            }
            // NTFS handed this record number to a different file. Reclaim it the
            // same way `upsert_link` does before overwriting the map entry
            // below: skipping this leaves the stale chain live in the entry
            // table but unreachable from `by_frn`, so its names keep matching
            // queries and its slots never return to the free list.
            self.reclaim_stale_slot(record, idx);
        }
        let id = self.names.intern(PLACEHOLDER_NAME);
        self.names.mark_folder(id);
        let idx = self.entries.alloc(
            parent_frn,
            self.lost_found,
            id,
            EntryFlags::DIR.with(EntryFlags::PLACEHOLDER, true),
            self.new_folder_size(),
            MTIME_UNKNOWN,
            NIL,
        );
        self.link_same_name(idx, id);
        self.link_child(idx);
        self.by_frn.set(record, idx);
        self.stats.placeholders_created += 1;
        self.dirty += 1;
        idx
    }

    fn delete_frn(&mut self, frn: Frn) -> bool {
        let record = frn.record_number();
        let Some(head) = self.by_frn.get(record) else {
            return false;
        };
        if self.entries.frn(head) != frn {
            return false;
        }
        self.tombstone_chain(head);
        self.by_frn.remove(record);
        true
    }

    fn tombstone_chain(&mut self, head: EntryIdx) {
        let mut cur = head;
        while cur != NIL {
            let next = self.entries.next_link(cur);
            self.remove_entry(cur);
            cur = next;
        }
    }

    /// Is a link with this `(parent, name)` already in `frn`'s chain?
    fn chain_has_link(&self, frn: Frn, parent_idx: EntryIdx, name: &[u8]) -> bool {
        let Some(head) = self.head_of(frn) else {
            return false;
        };
        let mut cur = head;
        while cur != NIL {
            if self.entries.parent(cur) == parent_idx
                && self.names.raw_bytes(self.entries.name_id(cur)) == name
            {
                return true;
            }
            cur = self.entries.next_link(cur);
        }
        false
    }

    /// Appends a new link (a file's additional hard-link name) to the tail of
    /// `frn`'s chain, leaving the existing head, and its identity, untouched.
    fn add_link(&mut self, frn: Frn, parent_idx: EntryIdx, name: &[u8], name_lossy: bool) {
        let id = self.names.intern(name);
        let flags = EntryFlags::EMPTY.with(EntryFlags::NON_UNICODE_NAME, name_lossy);
        let idx = self
            .entries
            .alloc(frn, parent_idx, id, flags, SIZE_UNKNOWN, MTIME_UNKNOWN, NIL);
        self.link_same_name(idx, id);
        self.link_child(idx);
        let record = frn.record_number();
        if let Some(head) = self.by_frn.get(record) {
            let mut tail = head;
            while self.entries.next_link(tail) != NIL {
                tail = self.entries.next_link(tail);
            }
            self.entries.set_next_link(tail, idx);
        } else {
            self.by_frn.set(record, idx);
        }
        self.dirty += 1;
    }

    /// Makes the hard-link chain for `frn` exactly `desired`, preserving
    /// entries that already match by (parent, name) so their EntryIdx, and
    /// thus any children pointing at them, survive. Only used by
    /// [`Self::reconcile_links`] (live `HARD_LINK_CHANGE`, always a file, which
    /// has no children) and by the branch of [`Self::enrich`] for an FRN with
    /// no entry yet (nothing of it exists to orphan).
    fn set_chain_links(&mut self, frn: Frn, is_dir: bool, desired: &[LinkTarget]) {
        let record = frn.record_number();
        // Collect the current chain if it is really this FRN's; a chain of the
        // record's previous tenant is reclaimed before the map is overwritten.
        let mut current: SmallVec<[EntryIdx; 4]> = SmallVec::new();
        if let Some(head) = self.by_frn.get(record) {
            if self.entries.frn(head) == frn {
                let mut cur = head;
                while cur != NIL {
                    current.push(cur);
                    cur = self.entries.next_link(cur);
                }
            } else {
                self.reclaim_stale_slot(record, head);
            }
        }
        // A link under the tenant just reclaimed, which only a missed record
        // names, goes under lost+found. Read before any allocation below, which
        // can give the freed slot to another entry.
        let parents: SmallVec<[EntryIdx; 4]> = desired
            .iter()
            .map(|t| {
                if self.entries.is_tombstone(t.parent) {
                    self.lost_found
                } else {
                    t.parent
                }
            })
            .collect();

        // Every link names the same file record, so a new link starts with the
        // size and date its chain already carries. Nothing stats a link added
        // here: the tail stats the file before it reconciles the names.
        let (size, mtime) = current.first().map_or((SIZE_UNKNOWN, MTIME_UNKNOWN), |&e| {
            (self.entries.size(e), self.entries.mtime(e))
        });

        // Match each desired link to an existing entry, else allocate.
        let mut kept: SmallVec<[EntryIdx; 4]> = SmallVec::new();
        let mut used: SmallVec<[bool; 4]> = SmallVec::from_elem(false, current.len());
        for (target, &parent) in desired.iter().zip(&parents) {
            let existing = current.iter().enumerate().position(|(i, &e)| {
                !used[i]
                    && self.entries.parent(e) == parent
                    && self.names.raw_bytes(self.entries.name_id(e)) == target.name.bytes.as_slice()
            });
            if let Some(pos) = existing {
                used[pos] = true;
                kept.push(current[pos]);
            } else {
                let id = self.names.intern(&target.name.bytes);
                if is_dir {
                    self.names.mark_folder(id);
                }
                let flags = EntryFlags::EMPTY
                    .with(EntryFlags::DIR, is_dir)
                    .with(EntryFlags::NON_UNICODE_NAME, target.name.lossy);
                let size = if is_dir { self.new_folder_size() } else { size };
                let idx = self.entries.alloc(frn, parent, id, flags, size, mtime, NIL);
                self.link_same_name(idx, id);
                self.link_child(idx);
                self.dir_index_insert(idx);
                // A new name of a sized file adds that size to its folders.
                self.carry(parent, self.weight(idx), 0);
                kept.push(idx);
                self.dirty += 1;
            }
        }

        // Tombstone entries that no desired link matched.
        for (i, &e) in current.iter().enumerate() {
            if !used[i] {
                self.remove_entry(e);
            }
        }

        // Relink the surviving chain and repoint the FRN map at the new head.
        for w in 0..kept.len() {
            let next = if w + 1 < kept.len() { kept[w + 1] } else { NIL };
            self.entries.set_next_link(kept[w], next);
        }
        self.by_frn.set(record, kept[0]);
    }

    /// Builds a compaction WITHOUT mutating the index, or `None` if the arenas
    /// are not dirty enough to be worth it and no folder total needs summing
    /// again.
    ///
    /// Takes `&self` on purpose. This is the expensive half (it copies every
    /// live name into fresh arenas, hundreds of MB on a large volume, builds
    /// their intern table, order and ranks, and rewrites the name-id column)
    /// and it used to run inline inside `apply_batch`, under the write lock,
    /// blocking every query for as long as it took. Callers plan here, off
    /// the exclusive lock, then [`Self::apply_compaction`] the result.
    ///
    /// While folder sizes are on, every compaction also sums each folder's
    /// files again and corrects the totals that drifted, so a missed update
    /// heals at the next compaction; a parent cycle the live updates could not
    /// follow asks for one once it is gone.
    ///
    /// The plan is only valid while the index has not changed, and an
    /// out-of-date plan is dropped unused. A volume has exactly one writer
    /// (its tail thread), which is also the only caller: hold an upgradable
    /// read across both calls and no write can interleave.
    pub fn plan_compaction(&self) -> Option<CompactPlan> {
        let compact_names = self.names.should_compact();
        if !compact_names && !self.folder_sizes_want_resum() {
            return None;
        }
        Some(self.plan_compaction_now(compact_names))
    }

    /// [`Self::plan_compaction`] whatever the garbage, for measuring a
    /// compaction at full scale (the `memsim` example) and for tests.
    #[doc(hidden)]
    pub fn plan_compaction_now(&self, compact_names: bool) -> CompactPlan {
        let (names, name_ids) = if compact_names {
            let mut c = self.names.plan_compact();
            let old = self.entries.name_ids();
            let mut ids = Vec::with_capacity(old.capacity());
            for (idx, &id) in old.iter().enumerate() {
                if self.entries.is_tombstone(idx as EntryIdx) {
                    ids.push(NIL);
                    continue;
                }
                let new_id = c.remap[id as usize];
                debug_assert_ne!(new_id, NIL, "live entry referenced a dead name id");
                ids.push(new_id);
                if self
                    .entries
                    .flags(idx as EntryIdx)
                    .contains(EntryFlags::DIR)
                {
                    c.mark_folder(new_id);
                }
            }
            (Some(c), ids)
        } else {
            (None, Vec::new())
        };
        let folder_fixes = if self.folder_sizes {
            self.folder_sum_fixes()
        } else {
            Vec::new()
        };
        CompactPlan {
            epoch: self.epoch,
            names,
            name_ids,
            folder_fixes,
        }
    }

    /// Installs a [`CompactPlan`]: swaps in the compacted name storage and
    /// the rewritten name-id column, and writes the corrected folder totals.
    /// Returns the plan holding the old buffers, to free once the lock is
    /// released. A plan built before the index last changed is returned
    /// unused.
    ///
    /// This is the half that must hold the exclusive lock: swaps and the
    /// few corrected totals, no copying, hashing or sorting.
    pub fn apply_compaction(&mut self, mut plan: CompactPlan) -> CompactPlan {
        if plan.epoch != self.epoch {
            return plan;
        }
        self.epoch += 1;
        if let Some(names) = &mut plan.names {
            self.names.install_compacted(names);
            self.entries.swap_name_ids(&mut plan.name_ids);
        }
        if self.folder_sizes {
            for &(dir, total) in &plan.folder_fixes {
                self.entries.set_size(dir, total);
            }
            if self.cycle_suspects.is_empty() {
                self.folder_sizes_stale = false;
            }
        }
        plan
    }

    // -- room ---------------------------------------------------------------

    /// Grown copies of every buffer that what `room` describes would grow,
    /// or `None` when they all have the room. Built off the write lock, under
    /// a read that excludes other writers (the tail's upgradable read):
    /// growing a buffer copies all of it and faults its new pages in, and in
    /// place under the write lock that held every query 28.5 ms when the
    /// entry columns of a 5.1M-entry volume grew. [`Self::install_room`] then
    /// only swaps buffers.
    pub fn plan_room(&self, room: Room) -> Option<RoomPlan> {
        let entries = self.entries.plan_room(room.entries);
        let names = self
            .names
            .plan_room(room.names, room.raw_bytes, room.folded_bytes);
        let frn = self.by_frn.plan_room(room.records, room.max_record);
        let dirs =
            (self.dir_children.capacity() - self.dir_children.len() < room.folders).then(|| {
                let (entries, names) = (&self.entries, &self.names);
                let mut table = HashTable::with_capacity(
                    (self.dir_children.capacity() * 2).max(self.dir_children.len() + room.folders),
                );
                for &d in self.dir_children.iter() {
                    table.insert_unique(dir_hash_of(entries, names, d), d, |&x| {
                        dir_hash_of(entries, names, x)
                    });
                }
                (self.dir_children.len(), table)
            });
        if entries.is_none() && names.is_none() && frn.is_none() && dirs.is_none() {
            return None;
        }
        Some(RoomPlan {
            epoch: self.epoch,
            entries,
            names,
            frn,
            dirs,
        })
    }

    /// Installs a [`RoomPlan`]: swaps only. Returns the plan holding the old
    /// buffers, to free once the lock is released; a plan built before the
    /// index last changed is returned unused.
    pub fn install_room(&mut self, mut plan: RoomPlan) -> RoomPlan {
        if plan.epoch != self.epoch {
            return plan;
        }
        if let Some(room) = &mut plan.entries {
            self.entries.install_room(room);
        }
        if let Some(room) = &mut plan.names {
            self.names.install_room(room);
        }
        if let Some(room) = &mut plan.frn {
            self.by_frn.install_room(room);
        }
        if let Some((len, table)) = &mut plan.dirs
            && *len == self.dir_children.len()
        {
            core::mem::swap(table, &mut self.dir_children);
        }
        plan
    }

    /// Sizes the index for a volume of `records` MFT records before its
    /// build (`FSCTL_GET_NTFS_VOLUME_DATA`): the entry columns get room for
    /// one entry a record and settle, so past it they grow by a sixteenth
    /// instead of doubling, and an FRN map that holds only the root becomes
    /// dense over the records. A build used to pass through columns twice
    /// what they hold and a hash map of every record (143 MB for 5.1M
    /// entries) before publish trimmed them. Hard links add entries past
    /// the records and free records leave some room unused; publish trims
    /// what is left over. A record count no volume has is ignored.
    pub fn reserve_for_records(&mut self, records: u64) {
        let Ok(records) = usize::try_from(records) else {
            return;
        };
        if records == 0 || records > MAX_RESERVED_RECORDS {
            return;
        }
        self.epoch += 1;
        self.entries.reserve(records);
        if let FrnMap::Sparse(m) = &self.by_frn
            && m.len() <= 2
        {
            let mut dense = FrnMap::dense(records);
            for (&record, &idx) in m {
                dense.set(record, idx);
            }
            self.by_frn = dense;
        }
    }

    // -- folder sizes -------------------------------------------------------

    /// Whether folder sizes are summed (Everything's `index_folder_size`): a
    /// folder's size is then the total of every file below it, at any depth,
    /// kept current as files are created, resized, removed and moved, and
    /// as folders move. A file adds its size once per name, so a hard link
    /// adds it in each folder that holds one of its names, as Everything
    /// counts it; a file whose size is not read yet adds nothing until it
    /// is. A junction or other reparse point is a folder with nothing in it
    /// on NTFS, so it adds nothing; a sparse file or a cloud placeholder adds
    /// its size, not what it takes on disk. Off, every folder's size is
    /// unknown.
    pub fn folder_sizes(&self) -> bool {
        self.folder_sizes
    }

    /// Turns folder sizes on, summing every folder now, or off. The daemon
    /// sums a live index off the write lock instead
    /// ([`Self::plan_folder_sizes`]).
    pub fn set_folder_sizes(&mut self, on: bool) {
        if on {
            let plan = self.plan_folder_sizes();
            self.install_folder_sizes(plan);
        } else {
            self.clear_folder_sizes();
        }
    }

    /// Sums every folder's files, without changing the index: one walk of
    /// the child chains, post-order, about as long as the build's own
    /// passes take per entry. Install with [`Self::install_folder_sizes`].
    pub fn plan_folder_sizes(&self) -> FolderSizePlan {
        FolderSizePlan {
            epoch: self.epoch,
            fixes: self.folder_sum_fixes(),
        }
    }

    /// Writes the totals [`Self::plan_folder_sizes`] summed and keeps them
    /// current from then on. `false`, and nothing changed, when the index
    /// changed since the plan.
    pub fn install_folder_sizes(&mut self, plan: FolderSizePlan) -> bool {
        if plan.epoch != self.epoch {
            return false;
        }
        self.epoch += 1;
        for (dir, total) in plan.fixes {
            self.entries.set_size(dir, total);
        }
        self.folder_sizes = true;
        // A cycle's folders are not reached from the root; their totals
        // are summed again once the cycle is gone.
        self.folder_sizes_stale = !self.cycle_suspects.is_empty();
        true
    }

    /// Turns folder sizes off: every folder's size is unknown again.
    pub fn clear_folder_sizes(&mut self) {
        if !self.folder_sizes {
            return;
        }
        self.epoch += 1;
        for idx in 0..self.entries.capacity() as EntryIdx {
            if !self.entries.is_tombstone(idx) && self.entries.flags(idx).contains(EntryFlags::DIR)
            {
                self.entries.set_size(idx, SIZE_UNKNOWN);
            }
        }
        self.folder_sizes = false;
        self.folder_sizes_stale = false;
    }

    /// Whether the next compaction should sum the folders again: a parent
    /// cycle let the live totals drift, and no cycle is left to drift them.
    fn folder_sizes_want_resum(&self) -> bool {
        self.folder_sizes && self.folder_sizes_stale && self.cycle_suspects.is_empty()
    }

    /// Every folder reached from the root or lost+found by the child chains
    /// whose stored size is not the total of the files below it, with that
    /// total. One post-order walk with a stack of open entries, so no
    /// per-entry buffer: a file adds its size (nothing while unknown) to the
    /// innermost open entry, and a closed folder adds its total to the one
    /// around it. A file that a corrupt record made a parent is walked into
    /// too, so the folders under it get their totals, but what it holds
    /// counts nowhere above it, as [`Self::carry`] stops at it. Entries deeper
    /// than [`MAX_PATH_HOPS`], which no path reaches, add nothing.
    fn folder_sum_fixes(&self) -> Vec<(EntryIdx, u64)> {
        let mut fixes = Vec::new();
        let mut open: Vec<(EntryIdx, u64)> = Vec::new();
        for top in [self.root, self.lost_found] {
            open.push((top, 0));
            let mut cur = self.entries.first_child(top);
            loop {
                if cur != NIL {
                    if !self.entries.flags(cur).contains(EntryFlags::DIR)
                        && let Some(inner) = open.last_mut()
                    {
                        inner.1 = inner.1.wrapping_add(self.weight(cur));
                    }
                    let down = self.entries.first_child(cur);
                    if down != NIL && open.len() < MAX_PATH_HOPS {
                        open.push((cur, 0));
                        cur = down;
                    } else if self.entries.flags(cur).contains(EntryFlags::DIR) {
                        // An empty folder, or one past the depth bound.
                        let total = if down == NIL {
                            0
                        } else {
                            self.entries.size(cur)
                        };
                        if self.entries.size(cur) != total {
                            fixes.push((cur, total));
                        }
                        cur = self.entries.next_child(cur);
                    } else {
                        cur = self.entries.next_child(cur);
                    }
                    continue;
                }
                let Some((closed, total)) = open.pop() else {
                    break;
                };
                let is_dir = self.entries.flags(closed).contains(EntryFlags::DIR);
                if is_dir && self.entries.size(closed) != total {
                    fixes.push((closed, total));
                }
                match open.last_mut() {
                    Some(outer) => {
                        if is_dir {
                            outer.1 = outer.1.wrapping_add(total);
                        }
                        cur = self.entries.next_child(closed);
                    }
                    None => break,
                }
            }
        }
        fixes
    }

    /// How many folders' stored totals differ from their files' sum: 0 on a
    /// healthy index with folder sizes on.
    #[cfg(test)]
    pub fn folder_size_drift(&self) -> usize {
        self.folder_sum_fixes().len()
    }

    /// What `idx` adds to the folders above it: a file its size, nothing
    /// while unknown; a folder its total while folder sizes are on.
    #[inline]
    fn weight(&self, idx: EntryIdx) -> u64 {
        let size = self.entries.size(idx);
        if size == SIZE_UNKNOWN { 0 } else { size }
    }

    /// The size slot of a new, empty folder.
    #[inline]
    fn new_folder_size(&self) -> u64 {
        if self.folder_sizes { 0 } else { SIZE_UNKNOWN }
    }

    /// Sets file entry `idx`'s size and carries the change to its folders.
    fn set_file_size(&mut self, idx: EntryIdx, size: u64) {
        let old = self.weight(idx);
        self.entries.set_size(idx, size);
        let new = self.weight(idx);
        self.carry(self.entries.parent(idx), new, old);
    }

    /// Adds `add` to and takes `sub` off the total of folder `dir` and of
    /// every folder above it: O(depth). Wrapping, so the halves of a move
    /// can come in either order. Nothing while folder sizes are off. The
    /// walk stops at an entry that is no folder, which only a corrupt record
    /// makes a parent: what sits under a file counts nowhere, as the sum's
    /// walk never goes into a file ([`Self::folder_sum_fixes`]). Bounded like
    /// [`Self::path_of`], so a walk from inside a parent cycle ends, with
    /// that cycle's totals left to the next sum (`folder_sizes_stale`).
    fn carry(&mut self, mut dir: EntryIdx, add: u64, sub: u64) {
        if !self.folder_sizes || add == sub {
            return;
        }
        for _ in 0..MAX_PATH_HOPS {
            if dir == NIL || !self.entries.flags(dir).contains(EntryFlags::DIR) {
                return;
            }
            let total = self.entries.size(dir);
            self.entries
                .set_size(dir, total.wrapping_add(add).wrapping_sub(sub));
            dir = self.entries.parent(dir);
        }
    }

    /// Per-component heap accounting for this index. O(directories) for the
    /// exact `dir_children` key bytes; everything else is O(1) reads of
    /// len/capacity. Called on `Status`, not on any hot path.
    pub fn memory(&self) -> IndexMemory {
        let dir_children = self.dir_children_mem();
        IndexMemory {
            entries: self.entries.memory(),
            arena_raw: self.names.raw_mem(),
            arena_folded: self.names.folded_mem(),
            frn_map: self.by_frn.memory(),
            name_tables: self.names.tables_mem(),
            dir_children,
            frn_map_kind: self.by_frn.kind(),
        }
    }

    /// The scope index's bytes: a slot and a control byte a bucket, the
    /// bucket estimate mirroring `FrnMap::memory`.
    fn dir_children_mem(&self) -> ComponentBytes {
        let buckets = ((self.dir_children.capacity() as u64 * 8) / 7).next_power_of_two();
        let per_bucket = (size_of::<EntryIdx>() + 1) as u64;
        ComponentBytes {
            used: self.dir_children.len() as u64 * per_bucket,
            allocated: buckets * per_bucket,
        }
    }

    /// Every buffer's bytes, by name: each entry column, each name table,
    /// the FRN map and the scope index. For the `memsim` example.
    pub fn memory_parts(&self) -> Vec<(&'static str, ComponentBytes)> {
        let mut parts = self.entries.columns_mem();
        parts.extend(self.names.parts_mem());
        parts.push(("frn map", self.by_frn.memory()));
        parts.push(("scope index", self.dir_children_mem()));
        parts
    }

    /// Returns growth slack (Vec doubling, hash-map overshoot) to the
    /// allocator, keeping ~1.6% headroom for post-publish journal churn, and
    /// settles the Vec buffers so that past it they grow by a sixteenth
    /// instead of doubling; the hash tables keep power-of-two buckets. Called once when a volume goes live: bootstrap grows
    /// every component by doubling, so up to half the allocation at that
    /// moment is slack that would otherwise be pinned for the daemon's
    /// lifetime.
    pub fn shrink_to_fit(&mut self) {
        self.epoch += 1;
        self.entries.shrink_with_headroom();
        self.names.shrink_with_headroom();
        let Self {
            dir_children,
            entries,
            names,
            ..
        } = self;
        dir_children.shrink_to_fit(|&d| dir_hash_of(entries, names, d));
        self.by_frn.shrink_to_fit();
    }

    /// One-shot storage optimization for a freshly built volume: swap the
    /// FRN map to its cheaper dense backing when the record space allows it,
    /// build the name order while every name is known (one parallel sort,
    /// ~100 ms per million names, instead of the tail merging them in a
    /// thousand at a time), drop the cycle suspects the build healed, then
    /// return all growth slack. Runs before the index is published; do not
    /// call it per batch.
    pub fn optimize_storage(&mut self) {
        self.epoch += 1;
        self.by_frn.densify_if_smaller();
        self.names.build_order();
        self.prune_cycle_suspects();
        self.shrink_to_fit();
    }

    // -- name order ---------------------------------------------------------

    #[inline]
    pub(crate) fn name_id(&self, idx: EntryIdx) -> NameId {
        self.entries.name_id(idx)
    }

    #[inline]
    pub(crate) fn name_folded(&self, id: NameId) -> &[u8] {
        self.names.folded_bytes_of(id)
    }

    #[inline]
    pub(crate) fn name_raw(&self, id: NameId) -> &[u8] {
        self.names.raw_bytes(id)
    }

    /// Whether a folder may bear `id`: `false` means files alone do, so
    /// [`Self::name_refcount`] counts files. See `NameStore::folder_bearer`.
    #[inline]
    pub(crate) fn name_may_bear_folder(&self, id: NameId) -> bool {
        self.names.may_bear_folder(id)
    }

    /// [`Self::name_may_bear_folder`] for ids `64 * w..64 * w + 64` at once,
    /// bit `i` for id `64 * w + i`.
    #[inline]
    pub(crate) fn name_folder_word(&self, w: usize) -> u64 {
        self.names.folder_word(w)
    }

    /// Live entries bearing `id`: the chain's length without walking it.
    #[inline]
    pub(crate) fn name_refcount(&self, id: NameId) -> u32 {
        self.names.refcount(id)
    }

    /// Ids ever allocated, live and dead: the bound of a per-name loop.
    #[inline]
    pub(crate) fn name_count(&self) -> usize {
        self.names.ids()
    }

    /// Ids in folded-name order; see `NameStore::order`.
    #[inline]
    pub(crate) fn name_order(&self) -> &[NameId] {
        self.names.order()
    }

    /// Ids interned since the name order was last merged, so not in it; see
    /// `NameStore::unranked`.
    #[inline]
    pub(crate) fn name_unranked(&self) -> &[NameId] {
        self.names.unranked()
    }

    /// See `NameStore::rank_key`.
    #[inline]
    pub(crate) fn name_rank_key(&self, id: NameId) -> u64 {
        self.names.rank_key(id)
    }

    /// Whether entries bearing `id` may be synthetic (the root's empty name,
    /// lost+found's, a placeholder's), so a count must walk the chain instead
    /// of trusting the refcount. A real file can carry the latter two names.
    /// Inline in each pass over the names, which asks it of every name.
    #[inline(always)]
    pub(crate) fn name_may_bear_synthetic(&self, id: NameId) -> bool {
        let raw = self.names.raw_bytes(id);
        raw.is_empty() || raw == LOST_FOUND_NAME || raw == PLACEHOLDER_NAME
    }

    /// `(in the order, waiting)` name counts for `--status`. Waiting names
    /// join the order on the tail's next merge; until then each costs a
    /// name-sorted query a binary search.
    pub fn name_order_stats(&self) -> (u64, u64) {
        (
            self.names.order().len() as u64,
            self.names.unranked().len() as u64,
        )
    }

    /// The waiting names merged into the name order, built off the write
    /// lock, or `None` when too few wait (see [`ApplyOutcome::wants_merge`]).
    pub fn plan_name_merge(&self) -> Option<NameMergePlan> {
        if !self.names.wants_merge() {
            return None;
        }
        Some(NameMergePlan {
            epoch: self.epoch,
            merge: self.names.plan_merge()?,
        })
    }

    /// Installs a [`NameMergePlan`]: swaps only. Returns the plan holding the
    /// old order, to free once the lock is released; a plan built before the
    /// index last changed is returned unused.
    pub fn install_name_merge(&mut self, mut plan: NameMergePlan) -> NameMergePlan {
        if plan.epoch == self.epoch && self.names.install_merge(&mut plan.merge) {
            self.epoch += 1;
        }
        plan
    }

    /// Test-only: folds the waiting names into the order now.
    #[cfg(test)]
    pub fn force_name_merge(&mut self) {
        self.epoch += 1;
        self.names.merge_unranked();
    }

    /// The name order's invariant: its live part is every live id not
    /// waiting, sorted by (folded bytes, id); a ranked id's rank is the
    /// position of the first live id of its folded group; waiting ids are
    /// unranked. Asserted by the model-op-tape proptests.
    #[cfg(test)]
    pub fn name_order_matches_rebuild(&self) -> bool {
        let live = |id: NameId| self.names.refcount(id) > 0;
        let key = |id: NameId| (self.names.folded_bytes_of(id), id);
        let order = self.names.order();
        let order_live: Vec<NameId> = order.iter().copied().filter(|&id| live(id)).collect();
        if !order_live.windows(2).all(|w| key(w[0]) < key(w[1])) {
            return false;
        }
        let mut all: Vec<NameId> = order_live
            .iter()
            .copied()
            .chain(self.names.unranked().iter().copied().filter(|&id| live(id)))
            .collect();
        let n = all.len();
        all.sort_unstable();
        all.dedup();
        if all.len() != n {
            return false; // an id both ranked and waiting
        }
        let want: Vec<NameId> = (0..self.names.ids() as NameId)
            .filter(|&id| live(id))
            .collect();
        if all != want {
            return false;
        }
        let mut group_start = 0usize;
        let mut prev: Option<&[u8]> = None;
        for (pos, &id) in order.iter().enumerate() {
            if !live(id) {
                continue;
            }
            let f = self.names.folded_bytes_of(id);
            if prev != Some(f) {
                group_start = pos;
                prev = Some(f);
            }
            if !self.names.is_ranked(id) || self.names.rank(id) as usize != group_start {
                return false;
            }
        }
        self.names
            .unranked()
            .iter()
            .all(|&id| !self.names.is_ranked(id))
    }

    /// Every name's refcount equals its chain length: what lets a name-level
    /// query count matches without touching an entry. And every name a
    /// folder bears says so (see [`Self::name_may_bear_folder`]), which lets
    /// a query that keeps files or folders count by refcount the names that
    /// do not.
    #[cfg(test)]
    pub fn name_refcounts_match_chains(&self) -> bool {
        (0..self.names.ids() as NameId).all(|id| {
            self.names.refcount(id) as usize == self.name_chain(id).count()
                && (self.names.may_bear_folder(id)
                    || self
                        .name_chain(id)
                        .all(|e| !self.entries.flags(e).contains(EntryFlags::DIR)))
        })
    }

    // -- folded-arena query support --------------------------------------

    /// The contiguous folded UNIQUE-name buffer to `memmem`-scan. Each
    /// distinct name appears once; a hit expands to entries through the
    /// same-name chain (`name_chain`, crate-internal).
    #[inline]
    pub fn folded_bytes(&self) -> &[u8] {
        self.names.folded_haystack()
    }

    /// This entry's folded name (precomputed, shared with every entry bearing
    /// the same name).
    #[inline]
    pub fn folded_name(&self, idx: EntryIdx) -> &[u8] {
        self.names.folded_bytes_of(self.entries.name_id(idx))
    }

    /// The interned pairs, indexed by [`NameId`], `folded.off` ascending: the
    /// scan's merge-walk maps ascending hit offsets to ids with a forward
    /// cursor over this.
    #[inline]
    pub(crate) fn name_pairs(&self) -> &[NamePair] {
        self.names.pairs()
    }

    /// Iterates the live entries bearing name `id` (exact: no staleness
    /// filtering required).
    #[inline]
    pub(crate) fn name_chain(&self, id: NameId) -> NameChain<'_> {
        NameChain {
            entries: &self.entries,
            cur: self.names.chain_head(id),
        }
    }

    /// The live entries directly in `dir`, in no particular order. Cost is the
    /// folder's size, not the volume's.
    pub(crate) fn children(&self, dir: EntryIdx) -> impl Iterator<Item = EntryIdx> + '_ {
        let mut cur = self.entries.first_child(dir);
        std::iter::from_fn(move || {
            (cur != NIL).then(|| {
                let child = cur;
                cur = self.entries.next_child(child);
                child
            })
        })
    }

    /// Depth-first walk of every live entry strictly under `root` (the scope
    /// directory itself is not yielded, matching `in_scope` semantics). Cost
    /// is proportional to the SUBTREE, not the volume: this is what makes a
    /// folder-scoped query independent of disk size.
    pub(crate) fn subtree_entries(&self, root: EntryIdx) -> SubtreeWalk<'_> {
        SubtreeWalk {
            entries: &self.entries,
            root,
            cur: self.entries.first_child(root),
            depth: 1,
        }
    }

    /// The invariant the child chains must uphold: every live entry with a
    /// parent appears exactly once in that parent's child chain, and chains
    /// contain nothing else. The model-op-tape proptest asserts this after
    /// every applied op.
    #[cfg(test)]
    pub fn child_chains_match_rebuild(&self) -> bool {
        use rustc_hash::FxHashSet;
        let mut want: FxHashMap<EntryIdx, FxHashSet<EntryIdx>> = FxHashMap::default();
        for idx in self.entries.iter_live() {
            let p = self.entries.parent(idx);
            if p != NIL {
                want.entry(p).or_default().insert(idx);
            }
        }
        for parent in self.entries.iter_live() {
            let mut seen: FxHashSet<EntryIdx> = FxHashSet::default();
            let mut cur = self.entries.first_child(parent);
            while cur != NIL {
                if self.entries.parent(cur) != parent || !seen.insert(cur) {
                    return false; // foreign member or revisit = corrupt chain
                }
                cur = self.entries.next_child(cur);
            }
            if seen != want.remove(&parent).unwrap_or_default() {
                return false;
            }
        }
        want.is_empty()
    }

    /// Every live entry's parent is live (or NIL for the root and
    /// lost+found), so no path walk ever reads a freed slot.
    #[cfg(test)]
    pub fn parents_are_live(&self) -> bool {
        self.entries.iter_live().all(|idx| {
            let p = self.entries.parent(idx);
            p == NIL || !self.entries.is_tombstone(p)
        })
    }

    /// The invariant the same-name chains must uphold: walking every name's
    /// chain visits exactly the live entries bearing that name, each once.
    /// The model-op-tape proptest asserts this after every applied op.
    #[cfg(test)]
    pub fn name_chains_match_rebuild(&self) -> bool {
        use rustc_hash::FxHashSet;
        let mut want: FxHashMap<NameId, FxHashSet<EntryIdx>> = FxHashMap::default();
        for idx in self.entries.iter_live() {
            want.entry(self.entries.name_id(idx))
                .or_default()
                .insert(idx);
        }
        for id in 0..self.names.ids() as NameId {
            let mut seen: FxHashSet<EntryIdx> = FxHashSet::default();
            for e in self.name_chain(id) {
                if !seen.insert(e) {
                    return false; // revisit = corrupt chain
                }
            }
            if seen != want.remove(&id).unwrap_or_default() {
                return false;
            }
        }
        want.is_empty()
    }
}

/// Depth-first iterator over a directory's live descendants, driven purely
/// by the child chains (no allocation at all: the walk backtracks through
/// parent pointers, so there is no explicit stack).
pub(crate) struct SubtreeWalk<'a> {
    entries: &'a EntryTable,
    root: EntryIdx,
    cur: EntryIdx,
    /// Levels below `root` of `cur`, which bounds the climb back.
    depth: usize,
}

impl Iterator for SubtreeWalk<'_> {
    type Item = EntryIdx;
    fn next(&mut self) -> Option<EntryIdx> {
        if self.cur == NIL {
            return None;
        }
        let yielded = self.cur;
        // Advance: descend first, else next sibling, else climb until a
        // sibling exists or the root is reached.
        let down = self.entries.first_child(yielded);
        if down != NIL {
            self.cur = down;
            self.depth += 1;
        } else {
            let mut at = yielded;
            loop {
                let sib = self.entries.next_child(at);
                if sib != NIL {
                    self.cur = sib;
                    break;
                }
                // A consistent tree climbs back to the root in as many steps
                // as the walk went down. Needing more means a parent link and
                // a child chain disagree, which only corrupt links give, and
                // climbing on would loop here forever under the read lock.
                if self.depth <= 1 {
                    self.cur = NIL;
                    break;
                }
                self.depth -= 1;
                at = self.entries.parent(at);
                if at == self.root || at == NIL {
                    self.cur = NIL;
                    break;
                }
            }
        }
        Some(yielded)
    }
}

/// Iterator over one interned name's live entries (see
/// [`VolumeIndex::name_chain`]).
pub(crate) struct NameChain<'a> {
    entries: &'a EntryTable,
    cur: EntryIdx,
}

impl Iterator for NameChain<'_> {
    type Item = EntryIdx;
    fn next(&mut self) -> Option<EntryIdx> {
        if self.cur == NIL {
            return None;
        }
        let e = self.cur;
        self.cur = self.entries.next_same(e);
        Some(e)
    }
}

/// A borrowed view of one entry's fields.
pub struct EntryView<'a> {
    index: &'a VolumeIndex,
    idx: EntryIdx,
}

impl EntryView<'_> {
    pub fn frn(&self) -> Frn {
        self.index.entries.frn(self.idx)
    }
    pub fn name(&self) -> &[u8] {
        self.index
            .names
            .raw_bytes(self.index.entries.name_id(self.idx))
    }
    pub fn folded_name(&self) -> &[u8] {
        self.index.folded_name(self.idx)
    }
    pub fn is_dir(&self) -> bool {
        self.index.entries.flags(self.idx).contains(EntryFlags::DIR)
    }
    /// A file's size, or a folder's total while folder sizes are on (see
    /// [`VolumeIndex::folder_sizes`]); `None` while unknown.
    pub fn size(&self) -> Option<u64> {
        let s = self.index.entries.size(self.idx);
        (s != SIZE_UNKNOWN).then_some(s)
    }
    pub fn mtime(&self) -> Option<i64> {
        let m = self.index.entries.mtime(self.idx);
        (m != MTIME_UNKNOWN).then_some(m)
    }
    pub fn parent(&self) -> EntryIdx {
        self.index.entries.parent(self.idx)
    }
    /// The next entry in this file's hard-link chain, or [`NIL`].
    pub fn next_link(&self) -> EntryIdx {
        self.index.entries.next_link(self.idx)
    }
}
