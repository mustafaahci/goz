//! Low-level index storage: the interned name store, the struct-of-arrays
//! entry table, and the FRN → entry map. These carry no policy: [`super::volume`]
//! owns the apply/rename/link rules; this file just stores bytes and columns
//! compactly.
//!
//! Names are INTERNED: only ~31% of the names on a real Windows volume are
//! unique (node_modules alone contributes tens of thousands of copies of
//! `index.js`), so each distinct name is stored once and every entry carries a
//! 4-byte [`NameId`]. Entries sharing a name are linked through a doubly-linked
//! same-name chain (`next_same` / `prev_same` columns, head in the store), which
//! is how a query scan hit over the deduplicated haystack expands back into
//! entries. The chains are kept EXACT (eager O(1) unlink on tombstone), never
//! lazily repaired: a stale singly-linked chain can cycle through reused slots,
//! and an exact chain needs no per-candidate staleness guard at query time.

use crate::types::{EntryIdx, Frn, NIL};
use rustc_hash::FxHashMap;

/// Interned-name handle: an index into the name store's tables.
pub(crate) type NameId = u32;

/// A slice of a `NameStore` byte buffer: byte offset + byte length. Names are
/// capped at 64 KiB (an NTFS component is ≤ 255 UTF-16 units ≈ ≤ 1020 WTF-8
/// bytes) and the arenas are assumed to stay under 2 GiB (bit 31 of `off` is
/// a buffer selector; nothing enforces the bound, `push` truncates if it ever
/// did not hold), so `u32`/`u16` are ample.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct NameRef {
    /// Byte offset into the arena. Bit 31 (`RAW_IN_FOLDED`) on a RAW ref means
    /// the raw bytes live in the FOLDED arena (the name folds to itself, so
    /// one copy serves both).
    pub off: u32,
    /// Byte length of the name (excludes the NUL separator).
    pub len: u16,
}

/// Bit 31 of `NameRef::off` on a raw ref: the bytes live in the folded arena.
const RAW_IN_FOLDED: u32 = 1 << 31;

/// One interned name: a handle into the raw and folded buffers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) struct NamePair {
    /// Original-case bytes for display and path reconstruction. May carry
    /// [`RAW_IN_FOLDED`] in `off`.
    pub raw: NameRef,
    /// Case-folded bytes for the query scan, always in the folded arena.
    pub folded: NameRef,
}

// Per-entry SoA columns are multiplied by millions of files and per-name
// tables by millions of unique names: a silent layout regression (a widened
// column, `EntryFlags` past a `u8`) costs hundreds of MB on a 10M-file volume
// with no other signal. Pin the sizes.
const _: () = assert!(size_of::<NameRef>() == 8);
const _: () = assert!(size_of::<NamePair>() == 16);
const _: () = assert!(size_of::<EntryFlags>() == 1);
const _: () = assert!(size_of::<Frn>() == 8);
const _: () = assert!(size_of::<EntryIdx>() == 4);
const _: () = assert!(size_of::<NameId>() == 4);

/// Freshly compacted name storage plus the id remap for every kept name, built
/// WITHOUT touching the index so the expensive copy runs off the exclusive
/// lock. Apply with [`NameStore::install_compacted`].
pub(crate) struct CompactedNames {
    raw: Vec<u8>,
    folded: Vec<u8>,
    pairs: Vec<NamePair>,
    refcount: Vec<u32>,
    head: Vec<EntryIdx>,
    /// `remap[old_id]` = new id, or [`NIL`] for a dropped (refcount-0) name.
    remap: Vec<NameId>,
}

/// The interned name store: every distinct name once, in two NUL-separated
/// byte buffers (original-case `raw`, case-folded `folded`), plus per-name
/// tables indexed by [`NameId`].
///
/// NUL cannot occur in an NTFS name, so the separators, and the zero-fill
/// written over a dead name's bytes when its last reference drops, mean a
/// substring search over the contiguous `folded` buffer can never match
/// across a boundary or inside a dead name. `pairs[id].folded.off` is
/// ascending in `id` (appends only grow the buffer and compaction preserves
/// order), which is what lets a scan map hit offsets back to ids with a
/// forward merge-walk instead of a per-hit binary search.
///
/// When a name case-folds to itself (56% of real-world names), the raw bytes
/// are NOT stored: the raw ref points into the folded arena via
/// [`RAW_IN_FOLDED`].
#[derive(Debug, Default)]
pub(crate) struct NameStore {
    raw: Vec<u8>,
    folded: Vec<u8>,
    pairs: Vec<NamePair>,
    /// Live references per name; 0 = dead, awaiting compaction.
    refcount: Vec<u32>,
    /// Head of the same-name entry chain (see `EntryTable::next_same`).
    head: Vec<EntryIdx>,
    /// raw bytes → id. Stores only the id (4 bytes/name); key bytes live in
    /// the arenas and are compared through `raw_bytes`.
    by_bytes: hashbrown::HashTable<NameId>,
    /// Dead bytes in `raw` + `folded`, the compaction cue.
    garbage: usize,
    /// Ids in folded-name order (ties by id): what a name-sorted page walks
    /// instead of sorting. Built once by [`Self::build_order`] after
    /// bootstrap; names interned later wait in `unranked` until
    /// [`Self::merge_unranked`] folds them in. Dead ids linger until a merge
    /// or compaction drops them (their chains are empty).
    order: Vec<NameId>,
    /// Per id: the position in `order` of the first live id with the same
    /// folded bytes (equal names share it, so it orders like the bytes), or
    /// [`UNRANKED`] while the id waits.
    rank_of: Vec<u32>,
    /// Ids interned since the last merge, in intern order.
    unranked: Vec<NameId>,
    /// Ranked ids that died since the last merge: each widens the probes a
    /// query's binary search makes around it, so they count toward a merge.
    dead_ranked: usize,
}

/// A name not (yet) in the order.
pub(crate) const UNRANKED: u32 = u32::MAX;

fn hash_bytes(b: &[u8]) -> u64 {
    use core::hash::{Hash, Hasher};
    let mut h = rustc_hash::FxHasher::default();
    b.hash(&mut h);
    h.finish()
}

/// Shrinks a Vec to its length plus ~1.6% headroom (see
/// `NameStore::shrink_with_headroom` for why never to exact-fit).
fn shrink_vec<T>(v: &mut Vec<T>) {
    v.shrink_to(v.len() + v.len() / 64 + 64);
}

/// `NameStore::folded_bytes_of` over explicit tables, for closures that sort
/// or merge ids while another field is mutably borrowed.
fn folded_in<'a>(pairs: &'a [NamePair], folded: &'a [u8], id: NameId) -> &'a [u8] {
    let r = pairs[id as usize].folded;
    &folded[r.off as usize..r.off as usize + r.len as usize]
}

/// `NameStore::raw_bytes` over explicit tables: the intern table's rehash
/// closures need to hash names while `by_bytes` is mutably borrowed, so they
/// borrow the byte tables disjointly and go through this.
fn raw_bytes_in<'a>(
    pairs: &'a [NamePair],
    raw: &'a [u8],
    folded: &'a [u8],
    id: NameId,
) -> &'a [u8] {
    let r = pairs[id as usize].raw;
    let buf = if r.off & RAW_IN_FOLDED != 0 {
        folded
    } else {
        raw
    };
    let off = (r.off & !RAW_IN_FOLDED) as usize;
    &buf[off..off + r.len as usize]
}

impl NameStore {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Number of name ids ever allocated (live + dead-awaiting-compaction).
    pub(crate) fn ids(&self) -> usize {
        self.pairs.len()
    }

    /// Interns `name`, bumping its refcount (creating it on first sight).
    /// Panics only if `name` exceeds 64 KiB, which no NTFS component can.
    pub(crate) fn intern(&mut self, name: &[u8]) -> NameId {
        debug_assert!(name.len() <= u16::MAX as usize, "name exceeds 64 KiB");
        let hash = hash_bytes(name);
        if let Some(&id) = self.by_bytes.find(hash, |&id| {
            self.raw_bytes_of(self.pairs[id as usize]) == name
        }) {
            self.refcount[id as usize] += 1;
            return id;
        }

        let folded_bytes = crate::fold::fold(name);
        debug_assert!(
            folded_bytes.len() <= u16::MAX as usize,
            "folded name exceeds 64 KiB"
        );
        let folded = Self::push(&mut self.folded, &folded_bytes);
        let raw = if folded_bytes.as_slice() == name {
            NameRef {
                off: folded.off | RAW_IN_FOLDED,
                len: folded.len,
            }
        } else {
            Self::push(&mut self.raw, name)
        };

        let id = self.pairs.len() as NameId;
        self.pairs.push(NamePair { raw, folded });
        self.refcount.push(1);
        self.head.push(NIL);
        self.rank_of.push(UNRANKED);
        self.unranked.push(id);
        let Self {
            by_bytes,
            pairs,
            raw: raw_buf,
            folded: folded_buf,
            ..
        } = self;
        by_bytes.insert_unique(hash, id, |&i| {
            hash_bytes(raw_bytes_in(pairs, raw_buf, folded_buf, i))
        });
        id
    }

    fn push(buf: &mut Vec<u8>, bytes: &[u8]) -> NameRef {
        let off = buf.len() as u32;
        buf.extend_from_slice(bytes);
        buf.push(0);
        NameRef {
            off,
            len: bytes.len() as u16,
        }
    }

    /// The raw (original-case) bytes of `id`.
    #[inline]
    pub(crate) fn raw_bytes(&self, id: NameId) -> &[u8] {
        self.raw_bytes_of(self.pairs[id as usize])
    }

    #[inline]
    fn raw_bytes_of(&self, pair: NamePair) -> &[u8] {
        let r = pair.raw;
        let buf = if r.off & RAW_IN_FOLDED != 0 {
            &self.folded
        } else {
            &self.raw
        };
        let off = (r.off & !RAW_IN_FOLDED) as usize;
        &buf[off..off + r.len as usize]
    }

    /// The folded bytes of `id`.
    #[inline]
    pub(crate) fn folded_bytes_of(&self, id: NameId) -> &[u8] {
        let r = self.pairs[id as usize].folded;
        &self.folded[r.off as usize..r.off as usize + r.len as usize]
    }

    /// The whole folded buffer, for a contiguous query scan.
    #[inline]
    pub(crate) fn folded_haystack(&self) -> &[u8] {
        &self.folded
    }

    /// All interned pairs, indexed by id; `folded.off` ascending. The scan's
    /// merge-walk indexes this directly.
    #[inline]
    pub(crate) fn pairs(&self) -> &[NamePair] {
        &self.pairs
    }

    #[inline]
    pub(crate) fn chain_head(&self, id: NameId) -> EntryIdx {
        self.head[id as usize]
    }

    #[inline]
    pub(crate) fn set_chain_head(&mut self, id: NameId, e: EntryIdx) {
        self.head[id as usize] = e;
    }

    /// Drops one reference to `id`. When the last reference goes, the name's
    /// bytes are zero-filled (so the scan can never match them), the intern
    /// entry is removed (so the id can never be returned again), and the
    /// bytes are accounted as garbage for the compaction trigger.
    pub(crate) fn release(&mut self, id: NameId) {
        let i = id as usize;
        debug_assert!(self.refcount[i] > 0, "release of a dead name");
        self.refcount[i] -= 1;
        if self.refcount[i] > 0 {
            return;
        }
        debug_assert!(
            self.head[i] == NIL,
            "a name died while entries still chain to it"
        );
        let pair = self.pairs[i];
        let hash = hash_bytes(self.raw_bytes_of(pair));
        if let Ok(entry) = self.by_bytes.find_entry(hash, |&cand| cand == id) {
            entry.remove();
        }
        if self.rank_of[i] != UNRANKED {
            self.dead_ranked += 1;
        }
        // Zero the folded bytes (the scan haystack) and, if separately
        // stored, the raw bytes. Garbage counts both plus separators.
        let f = pair.folded;
        for b in &mut self.folded[f.off as usize..f.off as usize + f.len as usize] {
            *b = 0;
        }
        self.garbage += f.len as usize + 1;
        if pair.raw.off & RAW_IN_FOLDED == 0 {
            let r = pair.raw;
            for b in &mut self.raw[r.off as usize..r.off as usize + r.len as usize] {
                *b = 0;
            }
            self.garbage += r.len as usize + 1;
        }
    }

    /// `true` once the buffers exceed 8 KiB and dead bytes exceed 25% of
    /// them, the compaction cue.
    pub(crate) fn should_compact(&self) -> bool {
        let total = self.raw.len() + self.folded.len();
        total > 8192 && self.garbage * 4 > total
    }

    /// Builds compacted storage keeping only live (refcount > 0) names, in id
    /// order, WITHOUT mutating `self`: the copy is the whole cost of a
    /// compaction and must not run while queries are blocked. The caller
    /// applies with [`Self::install_compacted`] under a brief exclusive lock.
    pub(crate) fn plan_compact(&self) -> CompactedNames {
        let live = self.refcount.iter().filter(|&&c| c > 0).count();
        let mut c = CompactedNames {
            raw: Vec::with_capacity(self.raw.len().saturating_sub(self.garbage)),
            folded: Vec::with_capacity(self.folded.len()),
            pairs: Vec::with_capacity(live),
            refcount: Vec::with_capacity(live),
            head: Vec::with_capacity(live),
            remap: vec![NIL; self.pairs.len()],
        };
        for (i, &rc) in self.refcount.iter().enumerate() {
            if rc == 0 {
                continue;
            }
            let pair = self.pairs[i];
            let folded = Self::push(&mut c.folded, self.folded_bytes_of(i as NameId));
            let raw = if pair.raw.off & RAW_IN_FOLDED != 0 {
                NameRef {
                    off: folded.off | RAW_IN_FOLDED,
                    len: folded.len,
                }
            } else {
                Self::push(&mut c.raw, self.raw_bytes_of(pair))
            };
            c.remap[i] = c.pairs.len() as NameId;
            c.pairs.push(NamePair { raw, folded });
            c.refcount.push(rc);
            c.head.push(self.head[i]);
        }
        c
    }

    /// Installs storage built by [`Self::plan_compact`], returning the id
    /// remap (`remap[old_id]` = new id or [`NIL`]). The caller must rewrite
    /// every entry's name id through it. Buffer swaps plus one intern-table
    /// rebuild; this is the half that runs under the exclusive lock.
    ///
    /// The plan is only valid if the index has not changed since it was
    /// built; the volume's single-writer discipline (one tail thread, which
    /// plans and applies back to back under an upgradable lock) provides that.
    pub(crate) fn install_compacted(&mut self, c: CompactedNames) -> Vec<NameId> {
        // The order and the waiting list carry old ids: renumber them before
        // the tables change, dropping the dead. Renumbering keeps relative
        // order, so the order stays sorted.
        let renumber = |ids: &[NameId]| -> Vec<NameId> {
            ids.iter()
                .filter_map(|&id| {
                    let n = c.remap[id as usize];
                    (n != NIL).then_some(n)
                })
                .collect()
        };
        self.order = renumber(&self.order);
        self.unranked = renumber(&self.unranked);
        self.raw = c.raw;
        self.folded = c.folded;
        self.pairs = c.pairs;
        self.refcount = c.refcount;
        self.head = c.head;
        self.garbage = 0;
        let mut table = hashbrown::HashTable::with_capacity(self.pairs.len());
        let (pairs, raw, folded) = (&self.pairs, &self.raw, &self.folded);
        for id in 0..pairs.len() as NameId {
            let hash = hash_bytes(raw_bytes_in(pairs, raw, folded, id));
            table.insert_unique(hash, id, |&i| {
                hash_bytes(raw_bytes_in(pairs, raw, folded, i))
            });
        }
        self.by_bytes = table;
        self.rank_of = vec![UNRANKED; self.pairs.len()];
        self.dead_ranked = 0;
        self.rerank();
        c.remap
    }

    // -- name order ---------------------------------------------------------

    /// Ids in folded-name order (ties by id). Dead ids may linger until the
    /// next merge or compaction; their chains are empty and their refcount 0.
    #[inline]
    pub(crate) fn order(&self) -> &[NameId] {
        &self.order
    }

    /// Ids interned since the last merge, in intern order.
    #[inline]
    pub(crate) fn unranked(&self) -> &[NameId] {
        &self.unranked
    }

    #[cfg(test)]
    pub(crate) fn is_ranked(&self, id: NameId) -> bool {
        self.rank_of[id as usize] != UNRANKED
    }

    /// The rank of `id`: the position in the order of the first live id with
    /// the same folded bytes, or [`UNRANKED`]. Read by the test invariant.
    #[cfg(test)]
    pub(crate) fn rank(&self, id: NameId) -> u32 {
        self.rank_of[id as usize]
    }

    #[inline]
    pub(crate) fn refcount(&self, id: NameId) -> u32 {
        self.refcount[id as usize]
    }

    /// Sorts every live id into the name order and ranks it: the one full
    /// sort, run once after bootstrap (about 100 ms per million names across
    /// cores). Names that arrive later wait in `unranked` until
    /// [`Self::merge_unranked`] folds them in.
    pub(crate) fn build_order(&mut self) {
        use rayon::slice::ParallelSliceMut;
        let mut order: Vec<NameId> = {
            let Self {
                pairs,
                folded,
                refcount,
                ..
            } = &*self;
            let mut order: Vec<NameId> = (0..pairs.len() as NameId)
                .filter(|&id| refcount[id as usize] > 0)
                .collect();
            order.par_sort_unstable_by(|&a, &b| {
                folded_in(pairs, folded, a)
                    .cmp(folded_in(pairs, folded, b))
                    .then(a.cmp(&b))
            });
            order
        };
        core::mem::swap(&mut self.order, &mut order);
        self.unranked.clear();
        self.dead_ranked = 0;
        self.rerank();
    }

    /// Whether enough names sit outside the order that queries pay for it: an
    /// unranked name costs a query a binary search, a dead ranked name widens
    /// the probes around it. Past 1024 of either, or 0.1% of the ranked
    /// names, the tail folds them in with [`Self::merge_unranked`].
    pub(crate) fn wants_merge(&self) -> bool {
        let limit = 1024.max(self.order.len() / 1000);
        self.unranked.len() > limit || self.dead_ranked > limit
    }

    /// Folds the unranked ids into the order and drops the dead ones: one
    /// O(n) merge, a few milliseconds per million names, run under the write
    /// lock on the tail's apply path. Deliberately a merge, never a re-sort.
    pub(crate) fn merge_unranked(&mut self) {
        if self.order.is_empty() {
            self.build_order();
            return;
        }
        let mut new: Vec<NameId> = core::mem::take(&mut self.unranked);
        let old: Vec<NameId> = core::mem::take(&mut self.order);
        let merged = {
            let Self {
                pairs,
                folded,
                refcount,
                ..
            } = &*self;
            let key = |id: NameId| (folded_in(pairs, folded, id), id);
            new.retain(|&id| refcount[id as usize] > 0);
            new.sort_unstable_by(|&a, &b| key(a).cmp(&key(b)));
            let mut merged = Vec::with_capacity(old.len() + new.len());
            let mut old_live = old
                .iter()
                .copied()
                .filter(|&id| refcount[id as usize] > 0)
                .peekable();
            let mut new_it = new.into_iter().peekable();
            while let (Some(o), Some(n)) = (old_live.peek().copied(), new_it.peek().copied()) {
                if key(n) < key(o) {
                    merged.push(n);
                    new_it.next();
                } else {
                    merged.push(o);
                    old_live.next();
                }
            }
            merged.extend(old_live);
            merged.extend(new_it);
            merged
        };
        self.order = merged;
        self.dead_ranked = 0;
        self.rerank();
    }

    /// Recomputes every rank from the order: equal folded names share the
    /// position of the first live one of them; dead and waiting ids rank with
    /// nothing.
    fn rerank(&mut self) {
        let Self {
            order,
            rank_of,
            pairs,
            folded,
            refcount,
            ..
        } = self;
        rank_of.iter_mut().for_each(|r| *r = UNRANKED);
        let mut group_start = 0usize;
        let mut prev_live: Option<&[u8]> = None;
        for (pos, &id) in order.iter().enumerate() {
            if refcount[id as usize] == 0 {
                continue;
            }
            let f = folded_in(pairs, folded, id);
            if prev_live != Some(f) {
                group_start = pos;
                prev_live = Some(f);
            }
            rank_of[id as usize] = group_start as u32;
        }
    }

    /// The integer a name-sorted candidate is ordered by: `2 * rank + 1` for
    /// a ranked id; for an unranked one, `2 * rank + 1` of the ranked group
    /// its folded bytes equal, else `2 * p` for the position `p` where they
    /// would be inserted, which lands strictly between the neighbouring
    /// ranked keys. So an unranked name sorts exactly where a full re-sort
    /// would put it. Two unranked names between the same neighbours tie on
    /// the key; the caller breaks that on the folded bytes.
    pub(crate) fn rank_key(&self, id: NameId) -> u64 {
        let r = self.rank_of[id as usize];
        if r != UNRANKED {
            return 2 * u64::from(r) + 1;
        }
        let x = self.folded_bytes_of(id);
        let p = self.insertion_point(x);
        match self.first_live_at(p) {
            Some(pos) if self.folded_bytes_of(self.order[pos]) == x => {
                2 * u64::from(self.rank_of[self.order[pos] as usize]) + 1
            }
            _ => 2 * p as u64,
        }
    }

    /// First position at or after `from` holding a live id.
    fn first_live_at(&self, from: usize) -> Option<usize> {
        (from..self.order.len()).find(|&p| self.refcount[self.order[p] as usize] > 0)
    }

    /// A position `p` in the order such that every live id before `p` folds
    /// below `x` and every live id from `p` on folds at or above it. Dead ids
    /// are zero-filled and no longer in byte order, so the search probes past
    /// them; any `p` inside a run of dead ids is equally valid, since nothing
    /// live lies between.
    fn insertion_point(&self, x: &[u8]) -> usize {
        let (mut lo, mut hi) = (0usize, self.order.len());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match self.first_live_at(mid).filter(|&p| p < hi) {
                None => hi = mid, // [mid, hi) is all dead
                Some(p) => {
                    if self.folded_bytes_of(self.order[p]) < x {
                        lo = p + 1;
                    } else {
                        hi = mid;
                    }
                }
            }
        }
        lo
    }

    pub(crate) fn raw_mem(&self) -> ComponentBytes {
        ComponentBytes::of_vec(&self.raw)
    }

    pub(crate) fn folded_mem(&self) -> ComponentBytes {
        ComponentBytes::of_vec(&self.folded)
    }

    /// Per-name table bytes (pairs + refcounts + chain heads + intern table).
    pub(crate) fn tables_mem(&self) -> ComponentBytes {
        let intern_per = (size_of::<NameId>() + 1) as u64;
        let intern_buckets = ((self.by_bytes.capacity() as u64 * 8) / 7).next_power_of_two();
        ComponentBytes::of_vec(&self.pairs)
            + ComponentBytes::of_vec(&self.refcount)
            + ComponentBytes::of_vec(&self.head)
            + ComponentBytes::of_vec(&self.order)
            + ComponentBytes::of_vec(&self.rank_of)
            + ComponentBytes::of_vec(&self.unranked)
            + ComponentBytes {
                used: self.by_bytes.len() as u64 * intern_per,
                allocated: intern_buckets * intern_per,
            }
    }

    /// Returns growth slack to the OS, keeping ~1.6% headroom. Called once
    /// post-bootstrap: the buffers grew by doubling, so up to half their
    /// allocation can be slack. NOT an exact-fit shrink: a Vec shrunk to
    /// exactly its length doubles its whole allocation on the very next
    /// journal append, which un-does the shrink for the daemon's lifetime.
    /// The headroom absorbs live churn instead (tens of thousands of new
    /// names on a large volume before any buffer regrows).
    pub(crate) fn shrink_with_headroom(&mut self) {
        shrink_vec(&mut self.raw);
        shrink_vec(&mut self.folded);
        shrink_vec(&mut self.pairs);
        shrink_vec(&mut self.refcount);
        shrink_vec(&mut self.head);
        shrink_vec(&mut self.order);
        shrink_vec(&mut self.rank_of);
        shrink_vec(&mut self.unranked);
        let Self {
            by_bytes,
            pairs,
            raw,
            folded,
            ..
        } = self;
        by_bytes.shrink_to_fit(|&id| hash_bytes(raw_bytes_in(pairs, raw, folded, id)));
    }
}

/// Heap bytes one index component holds. `used` is what the live data needs;
/// `allocated` is what the component actually reserved (capacity), so
/// `allocated - used` is reclaimable slack. Both count heap payload only, not
/// the struct headers (a few dozen bytes against hundreds of MB).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ComponentBytes {
    pub used: u64,
    pub allocated: u64,
}

impl ComponentBytes {
    pub(crate) fn of_vec<T>(v: &Vec<T>) -> Self {
        let w = size_of::<T>() as u64;
        ComponentBytes {
            used: v.len() as u64 * w,
            allocated: v.capacity() as u64 * w,
        }
    }
}

impl core::ops::Add for ComponentBytes {
    type Output = ComponentBytes;
    fn add(self, other: ComponentBytes) -> ComponentBytes {
        ComponentBytes {
            used: self.used + other.used,
            allocated: self.allocated + other.allocated,
        }
    }
}

/// Per-entry flag bits.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct EntryFlags(u8);

impl EntryFlags {
    pub const DIR: EntryFlags = EntryFlags(1 << 0);
    /// The slot is dead and on the free list.
    pub const TOMBSTONE: EntryFlags = EntryFlags(1 << 1);
    /// The synthetic lost+found root.
    pub const LOST_FOUND: EntryFlags = EntryFlags(1 << 2);
    /// A parent stand-in created before its real record was seen; carries a
    /// synthetic name until the real record fills it in.
    pub const PLACEHOLDER: EntryFlags = EntryFlags(1 << 3);
    /// The name contained unpaired UTF-16 surrogates (WTF-8, not UTF-8).
    pub const NON_UNICODE_NAME: EntryFlags = EntryFlags(1 << 4);

    pub const EMPTY: EntryFlags = EntryFlags(0);

    #[inline]
    pub fn contains(self, other: EntryFlags) -> bool {
        self.0 & other.0 == other.0
    }

    #[inline]
    #[must_use]
    pub fn with(self, other: EntryFlags, on: bool) -> EntryFlags {
        if on {
            EntryFlags(self.0 | other.0)
        } else {
            EntryFlags(self.0 & !other.0)
        }
    }
}

impl core::fmt::Debug for EntryFlags {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut parts = Vec::new();
        for (bit, name) in [
            (Self::DIR, "DIR"),
            (Self::TOMBSTONE, "TOMBSTONE"),
            (Self::LOST_FOUND, "LOST_FOUND"),
            (Self::PLACEHOLDER, "PLACEHOLDER"),
            (Self::NON_UNICODE_NAME, "NON_UNICODE_NAME"),
        ] {
            if self.contains(bit) {
                parts.push(name);
            }
        }
        write!(f, "EntryFlags({})", parts.join("|"))
    }
}

/// Sentinel size meaning "unknown" (not yet enriched from FILE_LAYOUT / stat).
pub(crate) const SIZE_UNKNOWN: u64 = u64::MAX;
/// Sentinel mtime meaning "unknown".
pub(crate) const MTIME_UNKNOWN: i64 = i64::MIN;

/// Struct-of-arrays entry storage. Every column is indexed by [`EntryIdx`];
/// one entry exists per link name, so a hard-linked file has several entries
/// sharing an [`Frn`], chained through `next_link`. Entries sharing an
/// interned name are chained through `next_same`/`prev_same` (head in
/// [`NameStore`]); [`super::volume`] keeps those chains exact.
#[derive(Debug, Default)]
pub(crate) struct EntryTable {
    frn: Vec<Frn>,
    parent: Vec<EntryIdx>,
    name_id: Vec<NameId>,
    flags: Vec<EntryFlags>,
    size: Vec<u64>,
    mtime: Vec<i64>,
    next_link: Vec<EntryIdx>,
    next_same: Vec<EntryIdx>,
    prev_same: Vec<EntryIdx>,
    /// Head of this entry's CHILD chain (meaningful for directories; NIL for
    /// files). With `next_child`/`prev_child` this makes a scope subtree
    /// enumerable, which turns a folder-scoped query from an O(volume) scan
    /// with a per-candidate ancestry check into an O(subtree) walk.
    first_child: Vec<EntryIdx>,
    /// Doubly-linked sibling chain (same discipline as `next_same`: exact,
    /// eagerly unlinked, because a lazily-repaired singly-linked chain can
    /// cycle through reused slots).
    next_child: Vec<EntryIdx>,
    prev_child: Vec<EntryIdx>,
    free: Vec<EntryIdx>,
    live: usize,
}

impl EntryTable {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Allocates a slot (reusing a freed one when available). The caller
    /// links the entry into its name's chain.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn alloc(
        &mut self,
        frn: Frn,
        parent: EntryIdx,
        name_id: NameId,
        flags: EntryFlags,
        size: u64,
        mtime: i64,
        next_link: EntryIdx,
    ) -> EntryIdx {
        self.live += 1;
        if let Some(idx) = self.free.pop() {
            let i = idx as usize;
            self.frn[i] = frn;
            self.parent[i] = parent;
            self.name_id[i] = name_id;
            self.flags[i] = flags;
            self.size[i] = size;
            self.mtime[i] = mtime;
            self.next_link[i] = next_link;
            self.next_same[i] = NIL;
            self.prev_same[i] = NIL;
            self.first_child[i] = NIL;
            self.next_child[i] = NIL;
            self.prev_child[i] = NIL;
            idx
        } else {
            let idx = self.frn.len() as EntryIdx;
            self.frn.push(frn);
            self.parent.push(parent);
            self.name_id.push(name_id);
            self.flags.push(flags);
            self.size.push(size);
            self.mtime.push(mtime);
            self.next_link.push(next_link);
            self.next_same.push(NIL);
            self.prev_same.push(NIL);
            self.first_child.push(NIL);
            self.next_child.push(NIL);
            self.prev_child.push(NIL);
            idx
        }
    }

    /// Marks a slot dead and pushes it on the free list. The caller must have
    /// already unlinked the entry from its same-name chain, moved its
    /// children out and released its name.
    pub(crate) fn tombstone(&mut self, idx: EntryIdx) {
        let i = idx as usize;
        debug_assert!(!self.flags[i].contains(EntryFlags::TOMBSTONE));
        self.flags[i] = EntryFlags::TOMBSTONE;
        // A dead slot keeps no link into the tree or the name store: its
        // parent may die too, and a compaction gives its name id to another
        // name or to none.
        self.parent[i] = NIL;
        self.name_id[i] = NIL;
        self.next_link[i] = NIL;
        self.next_same[i] = NIL;
        self.prev_same[i] = NIL;
        debug_assert!(
            self.first_child[i] == NIL,
            "tombstoning a directory that still has chained children"
        );
        self.first_child[i] = NIL;
        self.next_child[i] = NIL;
        self.prev_child[i] = NIL;
        self.free.push(idx);
        self.live -= 1;
    }

    #[inline]
    pub(crate) fn is_tombstone(&self, idx: EntryIdx) -> bool {
        self.flags[idx as usize].contains(EntryFlags::TOMBSTONE)
    }

    /// Number of live (non-tombstoned) entries, including root and lost+found.
    pub(crate) fn live(&self) -> usize {
        self.live
    }

    /// Total allocated slots (live + tombstoned-awaiting-reuse).
    pub(crate) fn capacity(&self) -> usize {
        self.frn.len()
    }

    #[inline]
    pub(crate) fn frn(&self, idx: EntryIdx) -> Frn {
        self.frn[idx as usize]
    }
    #[inline]
    pub(crate) fn parent(&self, idx: EntryIdx) -> EntryIdx {
        self.parent[idx as usize]
    }
    #[inline]
    pub(crate) fn name_id(&self, idx: EntryIdx) -> NameId {
        self.name_id[idx as usize]
    }
    #[inline]
    pub(crate) fn flags(&self, idx: EntryIdx) -> EntryFlags {
        self.flags[idx as usize]
    }
    #[inline]
    pub(crate) fn size(&self, idx: EntryIdx) -> u64 {
        self.size[idx as usize]
    }
    #[inline]
    pub(crate) fn mtime(&self, idx: EntryIdx) -> i64 {
        self.mtime[idx as usize]
    }
    #[inline]
    pub(crate) fn next_link(&self, idx: EntryIdx) -> EntryIdx {
        self.next_link[idx as usize]
    }
    #[inline]
    pub(crate) fn next_same(&self, idx: EntryIdx) -> EntryIdx {
        self.next_same[idx as usize]
    }
    #[inline]
    pub(crate) fn prev_same(&self, idx: EntryIdx) -> EntryIdx {
        self.prev_same[idx as usize]
    }
    #[inline]
    pub(crate) fn first_child(&self, idx: EntryIdx) -> EntryIdx {
        self.first_child[idx as usize]
    }
    #[inline]
    pub(crate) fn next_child(&self, idx: EntryIdx) -> EntryIdx {
        self.next_child[idx as usize]
    }
    #[inline]
    pub(crate) fn prev_child(&self, idx: EntryIdx) -> EntryIdx {
        self.prev_child[idx as usize]
    }

    #[inline]
    pub(crate) fn set_parent(&mut self, idx: EntryIdx, parent: EntryIdx) {
        self.parent[idx as usize] = parent;
    }
    #[inline]
    pub(crate) fn set_name_id(&mut self, idx: EntryIdx, id: NameId) {
        self.name_id[idx as usize] = id;
    }
    #[inline]
    pub(crate) fn set_flags(&mut self, idx: EntryIdx, flags: EntryFlags) {
        self.flags[idx as usize] = flags;
    }
    #[inline]
    pub(crate) fn set_size(&mut self, idx: EntryIdx, size: u64) {
        self.size[idx as usize] = size;
    }
    #[inline]
    pub(crate) fn set_mtime(&mut self, idx: EntryIdx, mtime: i64) {
        self.mtime[idx as usize] = mtime;
    }
    #[inline]
    pub(crate) fn set_next_link(&mut self, idx: EntryIdx, next: EntryIdx) {
        self.next_link[idx as usize] = next;
    }
    #[inline]
    pub(crate) fn set_next_same(&mut self, idx: EntryIdx, next: EntryIdx) {
        self.next_same[idx as usize] = next;
    }
    #[inline]
    pub(crate) fn set_prev_same(&mut self, idx: EntryIdx, prev: EntryIdx) {
        self.prev_same[idx as usize] = prev;
    }
    #[inline]
    pub(crate) fn set_first_child(&mut self, idx: EntryIdx, child: EntryIdx) {
        self.first_child[idx as usize] = child;
    }
    #[inline]
    pub(crate) fn set_next_child(&mut self, idx: EntryIdx, next: EntryIdx) {
        self.next_child[idx as usize] = next;
    }
    #[inline]
    pub(crate) fn set_prev_child(&mut self, idx: EntryIdx, prev: EntryIdx) {
        self.prev_child[idx as usize] = prev;
    }

    pub(crate) fn iter_live(&self) -> impl Iterator<Item = EntryIdx> + '_ {
        (0..self.frn.len() as EntryIdx).filter(|&i| !self.is_tombstone(i))
    }

    pub(crate) fn memory(&self) -> ComponentBytes {
        ComponentBytes::of_vec(&self.frn)
            + ComponentBytes::of_vec(&self.parent)
            + ComponentBytes::of_vec(&self.name_id)
            + ComponentBytes::of_vec(&self.flags)
            + ComponentBytes::of_vec(&self.size)
            + ComponentBytes::of_vec(&self.mtime)
            + ComponentBytes::of_vec(&self.next_link)
            + ComponentBytes::of_vec(&self.next_same)
            + ComponentBytes::of_vec(&self.prev_same)
            + ComponentBytes::of_vec(&self.first_child)
            + ComponentBytes::of_vec(&self.next_child)
            + ComponentBytes::of_vec(&self.prev_child)
            + ComponentBytes::of_vec(&self.free)
    }

    /// Returns Vec-doubling slack to the OS, keeping ~1.6% headroom so live
    /// churn does not re-double every column (see
    /// `NameStore::shrink_with_headroom`). Called once post-bootstrap.
    pub(crate) fn shrink_with_headroom(&mut self) {
        shrink_vec(&mut self.frn);
        shrink_vec(&mut self.parent);
        shrink_vec(&mut self.name_id);
        shrink_vec(&mut self.flags);
        shrink_vec(&mut self.size);
        shrink_vec(&mut self.mtime);
        shrink_vec(&mut self.next_link);
        shrink_vec(&mut self.next_same);
        shrink_vec(&mut self.prev_same);
        shrink_vec(&mut self.first_child);
        shrink_vec(&mut self.next_child);
        shrink_vec(&mut self.prev_child);
        shrink_vec(&mut self.free);
    }
}

/// How many records past twice its length a dense [`FrnMap`] still grows to
/// before [`FrnMap::set`] switches it to the sparse backing.
const DENSE_GROWTH_SLACK: u64 = 1 << 16;

/// Maps a 48-bit MFT record number to its chain-head [`EntryIdx`].
///
/// Keyed by record number (not the full FRN) so slot reuse is detectable:
/// the caller re-checks the stored entry's full FRN and, on a sequence
/// mismatch, treats the slot as free. Bootstrap builds the `Sparse` variant
/// (record numbers arrive before their density is known);
/// [`FrnMap::densify_if_smaller`] swaps to `Dense` at publish when the record
/// space is compact, which on a real NTFS volume it essentially always is.
#[derive(Debug)]
pub enum FrnMap {
    /// `Vec` indexed by record number; [`NIL`] marks an empty slot.
    Dense(Vec<EntryIdx>),
    /// Fallback for sparse / arbitrary record-number spaces.
    Sparse(FxHashMap<u64, EntryIdx>),
}

impl FrnMap {
    /// A hash-backed map (the default; safe for any record-number space).
    pub fn sparse() -> Self {
        FrnMap::Sparse(FxHashMap::default())
    }

    /// A dense map pre-sized for record numbers in `0..capacity`. `capacity` is
    /// only a sizing hint: `set` grows the `Vec` for a larger record number, or
    /// goes sparse for a far one, and `get` bounds-checks.
    pub fn dense(capacity: usize) -> Self {
        FrnMap::Dense(vec![NIL; capacity])
    }

    #[inline]
    pub fn get(&self, record: u64) -> Option<EntryIdx> {
        match self {
            FrnMap::Dense(v) => v.get(record as usize).copied().filter(|&i| i != NIL),
            FrnMap::Sparse(m) => m.get(&record).copied(),
        }
    }

    /// Maps `record` to `idx`. A dense map switches to the sparse backing for
    /// a record number far past its end: a real MFT grows a little at a time,
    /// but a corrupt journal record can carry any 48-bit number, and growing
    /// the `Vec` to it would abort the daemon on a failed allocation.
    #[inline]
    pub fn set(&mut self, record: u64, idx: EntryIdx) {
        if let FrnMap::Dense(v) = self
            && record > v.len() as u64 * 2 + DENSE_GROWTH_SLACK
        {
            let records = v.iter().enumerate().filter(|&(_, &i)| i != NIL);
            *self = FrnMap::Sparse(records.map(|(r, &i)| (r as u64, i)).collect());
        }
        match self {
            FrnMap::Dense(v) => {
                let r = record as usize;
                if r >= v.len() {
                    v.resize(r + 1, NIL);
                }
                v[r] = idx;
            }
            FrnMap::Sparse(m) => {
                m.insert(record, idx);
            }
        }
    }

    #[inline]
    pub fn remove(&mut self, record: u64) {
        match self {
            FrnMap::Dense(v) => {
                if let Some(slot) = v.get_mut(record as usize) {
                    *slot = NIL;
                }
            }
            FrnMap::Sparse(m) => {
                m.remove(&record);
            }
        }
    }

    /// Switches a sparse map to the dense backing when a `Vec` indexed by
    /// record number would cost less than the hash map's bucket array.
    ///
    /// MFT record numbers are dense by construction (record N sits at byte
    /// N x 1024 of the MFT, and NTFS reuses freed records before extending),
    /// so on a real volume the record space is barely larger than the live
    /// file count and the dense form wins by ~5x: 4 bytes per record versus
    /// ~17 per hash bucket at power-of-two capacity. The size check keeps a
    /// pathological space (sparse snapshots, non-NTFS IDs) on the hash map.
    ///
    /// Called once per volume when it goes live: bootstrap has seen every
    /// record by then, so `max key` is the true bound, and `set` still grows
    /// the Vec if the journal later mints a higher record number (or goes
    /// back to sparse if that number is far past the end).
    pub fn densify_if_smaller(&mut self) {
        let FrnMap::Sparse(m) = self else { return };
        let Some(&max) = m.keys().max() else { return };
        let per_bucket = (size_of::<(u64, EntryIdx)>() + 1) as u128;
        let buckets = ((m.capacity() as u128 * 8) / 7).next_power_of_two();
        let dense_bytes = (max as u128 + 1) * size_of::<EntryIdx>() as u128;
        if dense_bytes < buckets * per_bucket {
            let mut v = vec![NIL; max as usize + 1];
            for (&record, &idx) in m.iter() {
                v[record as usize] = idx;
            }
            *self = FrnMap::Dense(v);
        }
    }

    /// A short label for diagnostics ("dense" / "sparse").
    pub fn kind(&self) -> &'static str {
        match self {
            FrnMap::Dense(_) => "dense",
            FrnMap::Sparse(_) => "sparse",
        }
    }

    pub(crate) fn memory(&self) -> ComponentBytes {
        match self {
            FrnMap::Dense(v) => ComponentBytes::of_vec(v),
            FrnMap::Sparse(m) => {
                // hashbrown allocates power-of-two bucket arrays at 7/8 max
                // load; estimate from the usable capacity it reports. Each
                // bucket holds a (u64, u32) pair (padded to 16) + 1 control
                // byte. An estimate, but within ~2x of truth is enough to
                // rank this map against the other components.
                let buckets = ((m.capacity() as u64 * 8) / 7).next_power_of_two();
                let per_bucket = (size_of::<(u64, EntryIdx)>() + 1) as u64;
                let used = m.len() as u64 * per_bucket;
                ComponentBytes {
                    used,
                    allocated: buckets * per_bucket,
                }
            }
        }
    }
}
