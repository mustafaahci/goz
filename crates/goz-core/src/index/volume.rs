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
//!   lost+found, filled in when the parent's real record arrives.
//! - Slot reuse can never alias. The FRN map is keyed by record number and
//!   every hit is re-checked against the full FRN (sequence included).

use super::store::{
    ComponentBytes, EntryFlags, EntryTable, FrnMap, MTIME_UNKNOWN, NameId, NamePair, NameStore,
    SIZE_UNKNOWN,
};
use crate::layout::LayoutFile;
use crate::types::{EntryIdx, Frn, NIL};
use crate::usn::record::FILE_ATTRIBUTE_DIRECTORY;
use crate::usn::{ParsedUsnRecord, UsnOp, ops_for};
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
const MAX_PATH_HOPS: usize = 4096;

/// The `dir_children` key hash of a folded name. One fixed function for
/// insert, remove, and lookup: the three must agree or the scope index
/// silently loses directories.
fn dir_key_hash(folded: &[u8]) -> u64 {
    use core::hash::{Hash, Hasher};
    let mut h = rustc_hash::FxHasher::default();
    folded.hash(&mut h);
    h.finish()
}

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
/// [`VolumeIndex::apply_compaction`] consumes it, so a plan can never be applied
/// to an index it was not built from.
pub struct CompactPlan(crate::index::store::CompactedNames);

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
    /// NOT complete (one that failed or stopped part way, a file locked or
    /// pending delete, or a link in a folder not indexed yet): those are
    /// skipped rather than reconciled to a partial set, so a few names may be
    /// stale until the next rescan. A file that is gone is not counted. Counted so the gap is visible in
    /// `Status` rather than silent.
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
    /// The `(parent, folded name) → dir` scope index (bucket estimate + exact
    /// key-heap bytes).
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
    /// FRNs whose size/mtime changed and need an external stat.
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
    /// Placeholders currently alive, i.e. parents synthesized under lost+found
    /// because their real record has not arrived. While this is zero, nothing in
    /// the index is an orphan and every entry reconstructs to a real path.
    ///
    /// Exists to keep the query engine's presentability filter off the hot path:
    /// on a healthy index the filter is a single load, not a parent walk per
    /// candidate directory.
    live_placeholders: u64,
    generation: u64,
    stats: IndexStats,
    /// Entries structurally mutated since the last [`Self::take_write_count`].
    dirty: u64,
    /// Persistent `(parent entry, hash of folded name) → entry` index over
    /// real DIRECTORIES only, maintained on every mutation. Turns `-path`
    /// scope resolution into an O(depth) walk instead of an O(all-entries)
    /// scan. Keyed by the folded name's hash rather than owned name bytes:
    /// the bytes live once in the interned name store, and duplicating them
    /// here cost ~13 MB of key heap on a large volume. Lookups verify the
    /// candidate's actual folded name, so a hash collision can never resolve
    /// a scope to the wrong directory. Directories are ~10x rarer than files,
    /// so this stays small; placeholders and root/lost+found are excluded
    /// (they are never a scope component).
    /// Values are a 1-inline SmallVec: a genuine 64-bit hash collision
    /// between two sibling directories is astronomically rare, but when it
    /// happens both live here and `child_dir` disambiguates by comparing the
    /// actual folded names.
    dir_children: FxHashMap<(EntryIdx, u64), SmallVec<[EntryIdx; 1]>>,
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

        let mut this = Self {
            entries,
            names,
            by_frn: frn_map,
            root,
            lost_found,
            live_placeholders: 0,
            generation: 0,
            stats: IndexStats::default(),
            dirty: 0,
            dir_children: FxHashMap::default(),
        };
        this.link_same_name(root, root_name);
        this.link_same_name(lost_found, lf_name);
        this
    }

    /// Whether any entry could fail to reconstruct to a real path.
    ///
    /// False means no placeholder is alive, so nothing sits under lost+found and
    /// every entry is presentable. The query engine uses this to skip its
    /// per-directory presentability walk entirely on a healthy index.
    pub fn has_orphans(&self) -> bool {
        self.live_placeholders > 0
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
        let is_dir = file.attributes & FILE_ATTRIBUTE_DIRECTORY != 0;

        // Enrichment is ADD-ONLY: it never tombstones or replaces an existing
        // entry. This is critical: the entry may be a directory that already
        // has children pointing at its EntryIdx, and (during the bootstrap
        // race) its name may legitimately differ from the LAYOUT snapshot
        // because a rename landed between the ENUM and LAYOUT passes. Tombstone
        // + realloc here would orphan those children under a reused slot.
        if self.head_of(file.frn).is_none() {
            // No entry of this FRN exists yet (nothing to orphan): build the
            // chain from LAYOUT.
            let desired: Vec<LinkTarget> = file
                .names
                .iter()
                .filter(|n| !n.dos_only)
                .map(|n| LinkTarget {
                    parent: self.resolve_or_placeholder_parent(n.parent_frn),
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
                let parent_idx = self.resolve_or_placeholder_parent(n.parent_frn);
                if !self.chain_has_link(file.frn, parent_idx, &n.name) {
                    self.add_link(file.frn, parent_idx, &n.name, n.name_lossy);
                }
            }
        }

        // Set size/mtime on EVERY link of the file, not just the head: a query
        // may return any of a hard-linked file's names, and they all describe
        // the same bytes. (Directories keep unknown size.)
        if let Some(chain_head) = self.head_of(file.frn) {
            let mut cur = chain_head;
            while cur != NIL {
                if !is_dir && let Some(sz) = file.size {
                    self.entries.set_size(cur, sz);
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
        let mut outcome = ApplyOutcome::default();
        for rec in records {
            let ops: SmallVec<[UsnOp; 2]> = ops_for(rec);
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
        // have gathered (see `NameStore::wants_merge`): O(names), a few
        // milliseconds per million, and far rarer than a batch.
        if self.names.wants_merge() {
            self.names.merge_unranked();
        }
        self.generation += 1;
        // Compaction is NOT performed here: it is reported so the caller can do
        // the expensive part off the write lock. Doing it inline made a single
        // journal batch block every query for ~184 ms.
        outcome.wants_compact = self.names.should_compact();
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
    /// change and applies it here. Directories keep unknown size (mtime only).
    /// A no-op if the FRN is unknown (e.g. deleted between apply and stat).
    pub fn set_stat(&mut self, frn: Frn, size: u64, mtime: i64) {
        let Some(head) = self.head_of(frn) else {
            return;
        };
        let is_dir = self.entries.flags(head).contains(EntryFlags::DIR);
        let mut cur = head;
        while cur != NIL {
            if !is_dir {
                self.entries.set_size(cur, size);
            }
            self.entries.set_mtime(cur, mtime);
            cur = self.entries.next_link(cur);
        }
    }

    // -- path reconstruction ---------------------------------------------

    /// Appends `dir\dir\name` (no volume/mount prefix) for `idx` into `out` as
    /// WTF-8, returning how the walk terminated.
    pub fn path_of(&self, idx: EntryIdx, out: &mut Vec<u8>) -> PathStatus {
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
        let key = (parent, dir_key_hash(&folded_name));
        let cands = self.dir_children.get(&key)?;
        cands
            .iter()
            .copied()
            .find(|&d| self.folded_name(d) == folded_name.as_slice())
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

    /// Forces an entry's parent link. Test-only: the public API never
    /// produces a cycle, so this exists solely to exercise the defensive
    /// cycle guard in [`Self::path_of`].
    #[cfg(test)]
    pub fn force_parent(&mut self, idx: EntryIdx, parent: EntryIdx) {
        self.entries.set_parent(idx, parent);
    }

    /// The invariant every mutation must preserve: the incrementally-maintained
    /// directory index equals one rebuilt from a full scan of the live entries.
    /// The model-op-tape proptest asserts this after every applied op.
    #[cfg(test)]
    pub fn dir_index_matches_rebuild(&self) -> bool {
        let mut fresh: FxHashMap<(EntryIdx, u64), SmallVec<[EntryIdx; 1]>> = FxHashMap::default();
        for idx in self.entries.iter_live() {
            let flags = self.entries.flags(idx);
            if !flags.contains(EntryFlags::DIR)
                || flags.contains(EntryFlags::PLACEHOLDER)
                || idx == self.root
                || idx == self.lost_found
            {
                continue;
            }
            let key = (
                self.entries.parent(idx),
                dir_key_hash(self.folded_name(idx)),
            );
            fresh.entry(key).or_default().push(idx);
        }
        if fresh.len() != self.dir_children.len() {
            return false;
        }
        fresh.iter().all(|(k, want)| {
            self.dir_children.get(k).is_some_and(|got| {
                let mut a: Vec<_> = want.to_vec();
                let mut b: Vec<_> = got.to_vec();
                a.sort_unstable();
                b.sort_unstable();
                a == b
            })
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
        let flags = self.entries.flags(idx);
        if flags.contains(EntryFlags::CHAIN_DETACHED) {
            self.entries
                .set_flags(idx, flags.with(EntryFlags::CHAIN_DETACHED, false));
        }
    }

    /// O(1) removal from the parent's child chain. Must run BEFORE the
    /// entry's parent is overwritten or its slot tombstoned, for the same
    /// reason as [`Self::unlink_same_name`].
    fn unlink_child(&mut self, idx: EntryIdx) {
        let parent = self.entries.parent(idx);
        if parent == NIL {
            return;
        }
        // A severed entry is in NO chain; its parent slot may have been
        // reused by an unrelated entry, so touching that slot's chain head
        // here would corrupt the new tenant's children.
        if self.entries.flags(idx).contains(EntryFlags::CHAIN_DETACHED) {
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

    /// Detaches every chained child of a directory that is about to be
    /// tombstoned with its children still live (a missed delete followed by
    /// record reuse). The children keep their dangling parent pointer, the
    /// existing semantics (they resolve under lost+found once the slot's new
    /// tenant is a placeholder, and their own records re-parent them), but
    /// they leave the chain world entirely until relinked, so no later
    /// mutation can cross-link a reused slot's chain.
    fn sever_children(&mut self, dir: EntryIdx) {
        let mut cur = self.entries.first_child(dir);
        while cur != NIL {
            let next = self.entries.next_child(cur);
            self.entries.set_next_child(cur, NIL);
            self.entries.set_prev_child(cur, NIL);
            let flags = self.entries.flags(cur);
            self.entries
                .set_flags(cur, flags.with(EntryFlags::CHAIN_DETACHED, true));
            cur = next;
        }
        self.entries.set_first_child(dir, NIL);
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
        let key = (
            self.entries.parent(idx),
            dir_key_hash(self.folded_name(idx)),
        );
        let slot = self.dir_children.entry(key).or_default();
        if !slot.contains(&idx) {
            slot.push(idx);
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
        let key = (
            self.entries.parent(idx),
            dir_key_hash(self.folded_name(idx)),
        );
        if let Some(slot) = self.dir_children.get_mut(&key) {
            slot.retain(|&mut d| d != idx);
            if slot.is_empty() {
                self.dir_children.remove(&key);
            }
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
        let parent_idx = self.resolve_or_placeholder_parent(parent_frn);

        if let Some(head) = self.by_frn.get(record) {
            if self.entries.frn(head) == frn {
                if self.entries.next_link(head) == NIL {
                    self.rewrite_entry(head, parent_idx, name, name_lossy, is_dir);
                    return UpsertResult::Ok;
                }
                return UpsertResult::NeedsReconcile;
            }
            // Record number reused by a different FRN: reclaim the stale chain.
            self.reclaim_stale_slot(record, head);
        }

        let id = self.names.intern(name);
        let flags = EntryFlags::EMPTY
            .with(EntryFlags::DIR, is_dir)
            .with(EntryFlags::NON_UNICODE_NAME, name_lossy);
        let idx = self
            .entries
            .alloc(frn, parent_idx, id, flags, SIZE_UNKNOWN, MTIME_UNKNOWN, NIL);
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
        // Drop the OLD (parent, name) dir key before the fields are overwritten;
        // re-index under the NEW ones afterward. This covers rename, a
        // placeholder being filled in, and dir↔file transitions uniformly.
        self.dir_index_remove(idx);
        self.unlink_same_name(idx);
        self.names.release(self.entries.name_id(idx));
        let id = self.names.intern(name);
        self.entries.set_name_id(idx, id);
        self.link_same_name(idx, id);
        // Unlink from the OLD parent's child chain while `parent` still
        // points there, then relink under the new parent.
        self.unlink_child(idx);
        self.entries.set_parent(idx, parent_idx);
        self.link_child(idx);
        let was_placeholder = self.entries.flags(idx).contains(EntryFlags::PLACEHOLDER);
        let flags = self
            .entries
            .flags(idx)
            .with(EntryFlags::DIR, is_dir)
            .with(EntryFlags::NON_UNICODE_NAME, name_lossy)
            .with(EntryFlags::PLACEHOLDER, false);
        self.entries.set_flags(idx, flags);
        if was_placeholder {
            // The real record arrived and moved it out of lost+found.
            self.live_placeholders = self.live_placeholders.saturating_sub(1);
        }
        self.dir_index_insert(idx);
        self.dirty += 1;
    }

    /// Reclaims a record number NTFS has re-tenanted: tombstones the stale chain
    /// and drops the map entry, so the dead names stop matching and the slots
    /// return to the free list.
    ///
    /// Every site that finds a live `by_frn` entry whose FRN does not match must
    /// call this before overwriting the map. It exists as one function precisely
    /// so the two callers cannot drift apart again.
    /// Reported through `IndexStats::stale_slots` rather than an `Anomaly`: the
    /// two callers are deep helpers with no access to the batch's
    /// `ApplyOutcome`, and buffering per-event anomalies on the index would grow
    /// unbounded across a multi-million-entry bootstrap. The counter reaches
    /// `Status` either way.
    fn reclaim_stale_slot(&mut self, record: u64, head: EntryIdx) {
        self.tombstone_chain(head);
        self.by_frn.remove(record);
        self.stats.stale_slots += 1;
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
        let idx = self.entries.alloc(
            parent_frn,
            self.lost_found,
            id,
            EntryFlags::DIR.with(EntryFlags::PLACEHOLDER, true),
            SIZE_UNKNOWN,
            MTIME_UNKNOWN,
            NIL,
        );
        self.link_same_name(idx, id);
        self.link_child(idx);
        self.by_frn.set(record, idx);
        self.stats.placeholders_created += 1;
        self.live_placeholders += 1;
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
            if self.entries.flags(cur).contains(EntryFlags::PLACEHOLDER) {
                self.live_placeholders = self.live_placeholders.saturating_sub(1);
            }
            self.dir_index_remove(cur); // while the name/parent are still readable
            self.unlink_same_name(cur);
            self.unlink_child(cur);
            self.sever_children(cur);
            self.names.release(self.entries.name_id(cur));
            self.entries.tombstone(cur);
            self.dirty += 1;
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
    /// has no children) and by the ENUM-missed branch of [`Self::enrich`]
    /// (nothing exists yet to orphan).
    fn set_chain_links(&mut self, frn: Frn, is_dir: bool, desired: &[LinkTarget]) {
        let record = frn.record_number();
        // Collect the current chain (only if it is really this FRN's).
        let mut current: SmallVec<[EntryIdx; 4]> = SmallVec::new();
        if let Some(head) = self.by_frn.get(record)
            && self.entries.frn(head) == frn
        {
            let mut cur = head;
            while cur != NIL {
                current.push(cur);
                cur = self.entries.next_link(cur);
            }
        }

        // Every link names the same file record, so a new link starts with the
        // size and date its chain already carries. Nothing stats a link added
        // here: the tail stats the file before it reconciles the names.
        let (size, mtime) = current.first().map_or((SIZE_UNKNOWN, MTIME_UNKNOWN), |&e| {
            (self.entries.size(e), self.entries.mtime(e))
        });

        // Match each desired link to an existing entry, else allocate.
        let mut kept: SmallVec<[EntryIdx; 4]> = SmallVec::new();
        let mut used: SmallVec<[bool; 4]> = SmallVec::from_elem(false, current.len());
        for target in desired {
            let existing = current.iter().enumerate().position(|(i, &e)| {
                !used[i]
                    && self.entries.parent(e) == target.parent
                    && self.names.raw_bytes(self.entries.name_id(e)) == target.name.bytes.as_slice()
            });
            if let Some(pos) = existing {
                used[pos] = true;
                kept.push(current[pos]);
            } else {
                let id = self.names.intern(&target.name.bytes);
                let flags = EntryFlags::EMPTY
                    .with(EntryFlags::DIR, is_dir)
                    .with(EntryFlags::NON_UNICODE_NAME, target.name.lossy);
                let idx = self
                    .entries
                    .alloc(frn, target.parent, id, flags, size, mtime, NIL);
                self.link_same_name(idx, id);
                self.link_child(idx);
                self.dir_index_insert(idx);
                kept.push(idx);
                self.dirty += 1;
            }
        }

        // Tombstone entries that no desired link matched.
        for (i, &e) in current.iter().enumerate() {
            if !used[i] {
                self.dir_index_remove(e);
                self.unlink_same_name(e);
                self.unlink_child(e);
                self.sever_children(e);
                self.names.release(self.entries.name_id(e));
                self.entries.tombstone(e);
                self.dirty += 1;
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
    /// are not dirty enough to be worth it.
    ///
    /// Takes `&self` on purpose. This is the expensive half (it copies every
    /// live name into fresh arenas, hundreds of MB on a large volume) and it
    /// used to run inline inside `apply_batch`, under the write lock, blocking
    /// every query for as long as it took. Callers plan here, off the exclusive
    /// lock, then [`Self::apply_compaction`] the result.
    ///
    /// The plan is only valid while the index has not changed. That is not a
    /// hazard in practice because a volume has exactly one writer (its tail
    /// thread), which is also the only caller: hold an upgradable read across
    /// both calls and no write can interleave.
    pub fn plan_compaction(&self) -> Option<CompactPlan> {
        if !self.names.should_compact() {
            return None;
        }
        Some(CompactPlan(self.names.plan_compact()))
    }

    /// Installs a [`CompactPlan`]: swaps in the compacted name storage and
    /// rewrites every live entry's name id through the remap. Dead ids cannot
    /// be referenced by construction (an id only dies when its refcount hits
    /// zero, and each live entry holds a reference), so the remap lookup
    /// always yields a live new id.
    ///
    /// This is the half that must hold the exclusive lock: column writes plus
    /// one intern-table rebuild, no byte-buffer copying.
    pub fn apply_compaction(&mut self, plan: CompactPlan) {
        let remap = self.names.install_compacted(plan.0);
        for idx in 0..self.entries.capacity() as EntryIdx {
            if self.entries.is_tombstone(idx) {
                continue;
            }
            let new_id = remap[self.entries.name_id(idx) as usize];
            debug_assert_ne!(new_id, NIL, "live entry referenced a dead name id");
            self.entries.set_name_id(idx, new_id);
        }
    }

    /// Per-component heap accounting for this index. O(directories) for the
    /// exact `dir_children` key bytes; everything else is O(1) reads of
    /// len/capacity. Called on `Status`, not on any hot path.
    pub fn memory(&self) -> IndexMemory {
        let dir_children = {
            // Spilled SmallVecs (hash collisions) are so rare their heap is
            // ignored; the bucket estimate mirrors `FrnMap::memory`.
            let buckets = ((self.dir_children.capacity() as u64 * 8) / 7).next_power_of_two();
            let per_bucket = (size_of::<((EntryIdx, u64), SmallVec<[EntryIdx; 1]>)>() + 1) as u64;
            ComponentBytes {
                used: self.dir_children.len() as u64 * per_bucket,
                allocated: buckets * per_bucket,
            }
        };
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

    /// Returns growth slack (Vec doubling, hash-map overshoot) to the
    /// allocator, keeping ~1.6% headroom so post-publish journal churn does
    /// not immediately re-double every column. Called once when a volume goes
    /// live: bootstrap grows every component by doubling, so up to half the
    /// allocation at that moment is slack that would otherwise be pinned for
    /// the daemon's lifetime.
    pub fn shrink_to_fit(&mut self) {
        self.entries.shrink_with_headroom();
        self.names.shrink_with_headroom();
        self.dir_children.shrink_to_fit();
        if let FrnMap::Dense(v) = &mut self.by_frn {
            // Dense map growth is resize-to-record-number, not doubling, so
            // exact fit is safe here.
            v.shrink_to_fit();
        }
    }

    /// One-shot storage optimization for a freshly built volume: swap the
    /// FRN map to its cheaper dense backing when the record space allows it,
    /// build the name order while every name is known (one parallel sort,
    /// ~100 ms per million names, instead of the tail merging them in a
    /// thousand at a time), then return all growth slack. Runs before the
    /// index is published; do not call it per batch.
    pub fn optimize_storage(&mut self) {
        self.by_frn.densify_if_smaller();
        self.names.build_order();
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

    #[inline]
    pub(crate) fn name_is_ranked(&self, id: NameId) -> bool {
        self.names.is_ranked(id)
    }

    /// See `NameStore::rank_key`.
    #[inline]
    pub(crate) fn name_rank_key(&self, id: NameId) -> u64 {
        self.names.rank_key(id)
    }

    /// Whether entries bearing `id` may be synthetic (the root's empty name,
    /// lost+found's, a placeholder's), so a count must walk the chain instead
    /// of trusting the refcount. A real file can carry the latter two names.
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

    /// Test-only: folds the waiting names into the order now.
    #[cfg(test)]
    pub fn force_name_merge(&mut self) {
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
    /// query count matches without touching an entry.
    #[cfg(test)]
    pub fn name_refcounts_match_chains(&self) -> bool {
        (0..self.names.ids() as NameId)
            .all(|id| self.names.refcount(id) as usize == self.name_chain(id).count())
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

    /// Depth-first walk of every live entry strictly under `root` (the scope
    /// directory itself is not yielded, matching `in_scope` semantics). Cost
    /// is proportional to the SUBTREE, not the volume: this is what makes a
    /// folder-scoped query independent of disk size.
    pub(crate) fn subtree_entries(&self, root: EntryIdx) -> SubtreeWalk<'_> {
        SubtreeWalk {
            entries: &self.entries,
            root,
            cur: self.entries.first_child(root),
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
            if p != NIL && !self.entries.flags(idx).contains(EntryFlags::CHAIN_DETACHED) {
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
        } else {
            let mut at = yielded;
            loop {
                let sib = self.entries.next_child(at);
                if sib != NIL {
                    self.cur = sib;
                    break;
                }
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
