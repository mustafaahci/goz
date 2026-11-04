//! The query executor: compile a [`ParsedQuery`] into finders, scan the
//! index, apply scope and sort, and return a page of hits.
//!
//! The scan matches each candidate against the index's precomputed folded name
//! (case-insensitive queries) and tests the cheapest predicates first: kind,
//! size and date, then name substrings and wildcards, then ext, then (only for
//! survivors) scope ancestry and the full path. The full path, the one
//! expensive step, is deferred until a candidate has passed everything else,
//! and is its folder's path, built once per folder and query (see
//! `FolderPaths`), plus its own name.

use super::folder_rank::FolderRanks;
use super::parse::{CodePointBuf, Filters, Kind, ParsedQuery, SizeRange};
use crate::index::volume::MAX_PATH_HOPS;
use crate::index::{NameId, VolumeIndex};
use crate::types::{EntryIdx, Frn, NIL, SortDir, SortKey, SortSpec};
use memchr::memmem;
use rustc_hash::FxHashMap;
use smallvec::SmallVec;
use std::borrow::Cow;

/// One matched entry. The daemon prepends the volume mount prefix to `path`
/// and formats `mtime` for display.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryHit {
    pub idx: EntryIdx,
    /// Full path within the volume (WTF-8, no mount prefix).
    pub path: Vec<u8>,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub mtime: Option<i64>,
}

/// Result of a query: the requested page plus the full match count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryOutcome {
    /// Total matches before offset/limit (es `totitems` semantics).
    pub total: u64,
    /// The requested page of hits, in sort order.
    pub hits: Vec<QueryHit>,
}

/// One matched entry without its path: 16 bytes. The daemon keeps these for
/// a whole result set and reads each row's path, size and date from the index
/// when it encodes the page that carries it, so a multi-million-row export
/// costs bytes per row, not a path per row. The FRN lets the page builder
/// prove the slot still holds the same file (see
/// [`VolumeIndex::entry_if_frn`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HitRef {
    pub idx: EntryIdx,
    pub frn: Frn,
}

/// Result of [`run_query_refs`]: the requested page as references, in sort
/// order, plus the full match count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefOutcome {
    /// Total matches before offset/limit (es `totitems` semantics).
    pub total: u64,
    pub refs: Vec<HitRef>,
}

/// A [`ParsedQuery`]'s substring needles prebuilt into finders once (rather
/// than per candidate). The remaining predicates (wildcards, filters) are read
/// straight off the `ParsedQuery` in the scan loop.
pub struct CompiledQuery<'a> {
    name_finders: Vec<memmem::Finder<'a>>,
    /// The path terms the mount prefix does not already hold. One it holds
    /// is in every full path on the volume, so `C:\ report` tests names only.
    path_finders: Vec<memmem::Finder<'a>>,
    /// The longest literal substring guaranteed present in any match, drawn
    /// from the name terms, from wildcard literals (e.g. `.dll` of `*.dll`), and
    /// from a single `ext:` value (`.ext`). Drives the folded-arena prefilter
    /// scan. `None` when nothing offers a usable literal (then the query
    /// iterates every entry).
    prefilter: Option<Vec<u8>>,
    /// `true` when a prefilter hit IS a name match: the prefilter is the
    /// query's only name term and there is no wildcard or extension filter.
    /// The scan then skips re-testing the name it has just found, which on a
    /// broad query was a second `memmem` per distinct name for nothing.
    prefilter_decides_name: bool,
    /// The volume's mount prefix, folded iff the query is case-insensitive:
    /// path terms and path wildcards match `prefix + path`.
    prefix: Vec<u8>,
    /// `false` when a path term or path wildcard cannot match anywhere under
    /// `prefix` (see [`term_admits`]), so the volume has no hits.
    volume_admits: bool,
    /// Each path wildcard's leading and trailing literals: what a matching
    /// full path starts and ends with. Two byte compares turn most candidates
    /// away before the wildcard decodes the path.
    path_wildcard_ends: Vec<(Vec<u8>, Vec<u8>)>,
    /// The longest separator-free piece of path text that every match must
    /// hold inside one of its names (see [`path_piece`]); drives
    /// [`walk_below_names`]. `None` for a case-sensitive query, whose pieces
    /// the folded arena cannot find.
    path_piece: Option<Vec<u8>>,
}

impl<'a> CompiledQuery<'a> {
    /// Compiles `parsed` for the volume mounted at `mount_prefix` (`C:\`, or a
    /// mount folder such as `C:\mnt\data\`).
    pub fn new(parsed: &'a ParsedQuery, mount_prefix: &str) -> Self {
        // The prefilter is the longest literal from any name term, wildcard, or
        // single `ext:` value;
        // every match must contain it, so scanning for it yields a superset of
        // matches that `evaluate` then verifies.
        // Borrow name terms and move owned wildcard/ext literals, materializing
        // exactly one owned Vec at the end instead of cloning every name term
        // and discarding all but the longest.
        let mut prefilter: Option<Cow<'a, [u8]>> = None;
        let mut from_name_term = false;
        let mut consider = |cand: Cow<'a, [u8]>, is_name_term: bool| {
            if cand.len() > prefilter.as_ref().map_or(0, |c| c.len()) {
                prefilter = Some(cand);
                from_name_term = is_name_term;
            }
        };
        for term in &parsed.name_terms {
            consider(Cow::Borrowed(term.as_slice()), true);
        }
        for w in &parsed.wildcards {
            if let Some(lit) = w.longest_literal() {
                consider(Cow::Owned(lit), false);
            }
        }
        // A single `ext:` value contributes `.ext`, which every match's name
        // contains, so an ext-only query scans the folded arena for `.ext`
        // instead of iterating every entry. Multiple extensions share no single
        // literal, so they keep the slow path.
        if let Some(exts) = &parsed.filters.ext
            && exts.len() == 1
            && !exts[0].is_empty()
        {
            let mut lit = Vec::with_capacity(exts[0].len() + 1);
            lit.push(b'.');
            lit.extend_from_slice(&exts[0]);
            consider(Cow::Owned(lit), false);
        }

        let prefix = if parsed.match_case {
            mount_prefix.as_bytes().to_vec()
        } else {
            fold(mount_prefix.as_bytes())
        };
        let volume_admits = parsed.path_terms.iter().all(|t| term_admits(t, &prefix))
            && parsed.path_wildcards.iter().all(|w| {
                let lit = w.leading_literal();
                let n = lit.len().min(prefix.len());
                lit[..n] == prefix[..n]
            });

        let path_piece = (!parsed.match_case)
            .then(|| path_piece(parsed, &prefix))
            .flatten();

        Self {
            name_finders: parsed
                .name_terms
                .iter()
                .map(|t| memmem::Finder::new(t.as_slice()))
                .collect(),
            path_finders: parsed
                .path_terms
                .iter()
                .filter(|t| memmem::find(&prefix, t).is_none())
                .map(|t| memmem::Finder::new(t.as_slice()))
                .collect(),
            prefilter: prefilter.map(Cow::into_owned),
            prefilter_decides_name: from_name_term
                && parsed.name_terms.len() == 1
                && parsed.wildcards.is_empty()
                && parsed.filters.ext.is_none(),
            prefix,
            volume_admits,
            path_wildcard_ends: parsed
                .path_wildcards
                .iter()
                .map(|w| (w.leading_literal(), w.trailing_literal()))
                .collect(),
            path_piece,
        }
    }

    /// Whether a candidate's full path has anything left to decide.
    fn tests_paths(&self, parsed: &ParsedQuery) -> bool {
        !self.path_finders.is_empty() || !parsed.path_wildcards.is_empty()
    }
}

/// The longest piece of `parsed`'s path terms and path wildcards that holds
/// no separator and does not occur in `prefix`, the later one on a tie.
///
/// A piece lies within one part of the full path between separators, and
/// when it is not in the prefix, that part is the name of the match or of
/// one of its folders. Only the entries at or below a name holding the piece
/// can match, then. The longest piece is usually the rarest, and on a tie the
/// later one usually names the deeper folder, with the smaller subtree.
///
/// `None` when `prefix` does not end with a separator, since then its last
/// part runs into the first name and a piece could span the two.
fn path_piece(parsed: &ParsedQuery, prefix: &[u8]) -> Option<Vec<u8>> {
    if !prefix.is_empty() && !prefix.ends_with(b"\\") {
        return None;
    }
    let mut best: Option<Vec<u8>> = None;
    let mut consider = |piece: &[u8]| {
        if !piece.is_empty()
            && piece.len() >= best.as_ref().map_or(0, Vec::len)
            && memmem::find(prefix, piece).is_none()
        {
            best = Some(piece.to_vec());
        }
    };
    for term in &parsed.path_terms {
        term.split(|&b| b == b'\\').for_each(&mut consider);
    }
    for w in &parsed.path_wildcards {
        for piece in w.separator_free_literals() {
            consider(&piece);
        }
    }
    best
}

/// Whether path term `term` can match a full path that starts with `prefix`
/// (both folded alike). Windows refuses `:` in a name, so a term's first `:` can
/// only be the drive's, the first `:` in `prefix`; that fixes where the term
/// starts, and the part of it that overlaps the prefix must equal the prefix
/// there. `D:\data` thus skips every volume but D: without reading an entry.
fn term_admits(term: &[u8], prefix: &[u8]) -> bool {
    let Some(k) = memchr::memchr(b':', term) else {
        return true;
    };
    let Some(start) = memchr::memchr(b':', prefix).and_then(|c| c.checked_sub(k)) else {
        return false;
    };
    let overlap = (prefix.len() - start).min(term.len());
    term[..overlap] == prefix[start..start + overlap]
}

/// A matched entry before its page is built: 16 bytes, so a multi-million
/// match set costs tens of MB rather than hundreds. Everything else (name,
/// parent, kind, size, date) is read from the index when a comparison or the
/// page needs it.
struct Candidate {
    idx: EntryIdx,
    /// The key the page's sort asks for, filled by [`order`]: the name's rank
    /// key for a name sort, the parent folder's rank and the name's rank
    /// packed together for a path sort (see [`path_keys`]), the size for a
    /// size sort, the order-preserving image of the mtime for a date sort.
    /// Candidates used to carry kind, size, date, an
    /// optional path and a `fold(parent) + NUL + fold(name)` key string each,
    /// ~300 MB under the index lock for a 100-row page of a broad path sort.
    key: u64,
}

/// Runs a query over `index`.
///
/// `scope` (if any) restricts results to descendants of that directory entry;
/// the daemon resolves the `-path` folder via [`resolve_scope`]. Results are
/// sorted per `sort`, then `offset`/`limit` select the page; `total` reports
/// the full match count.
///
/// The common case, a case-insensitive query with at least one substring
/// term, scans the index's contiguous folded-name arena with a single
/// prebuilt `memmem` finder (SIMD over the whole buffer), maps each hit back to
/// its entry, and defers path reconstruction to the final page. That keeps the
/// hot path off per-query folding and off per-match path building.
pub fn run_query(
    index: &VolumeIndex,
    mount_prefix: &str,
    parsed: &ParsedQuery,
    scope: Option<EntryIdx>,
    sort: SortSpec,
    offset: u32,
    limit: Option<u32>,
) -> QueryOutcome {
    let (total, refs) = page_refs(
        index,
        mount_prefix,
        parsed,
        scope,
        sort,
        offset,
        limit,
        true,
    );
    let hits = build_hits(index, &refs);
    QueryOutcome { total, hits }
}

/// Like [`run_query`], but the page comes back as 16-byte [`HitRef`]s and no
/// path is built: the daemon's entry point.
///
/// Any ordering happens here, under the caller's index read lock, with
/// comparators that read the index and allocate nothing; a name-sorted query
/// over names alone does not order at all (see `page_refs`). The daemon
/// builds each page's paths from the live index when it encodes them.
pub fn run_query_refs(
    index: &VolumeIndex,
    mount_prefix: &str,
    parsed: &ParsedQuery,
    scope: Option<EntryIdx>,
    sort: SortSpec,
    offset: u32,
    limit: Option<u32>,
) -> RefOutcome {
    let (total, refs) = page_refs(
        index,
        mount_prefix,
        parsed,
        scope,
        sort,
        offset,
        limit,
        true,
    );
    RefOutcome { total, refs }
}

/// The page of a query as references: the one implementation behind
/// [`run_query`] and [`run_query_refs`].
///
/// A name-sorted query whose predicates are all decided by the file name (see
/// [`is_name_level`]) never builds a candidate: it matches the distinct names
/// (about a quarter of the entries on a real volume, since names are
/// interned), counts matches from each name's refcount, and pages by walking
/// the matched names in name order and their same-name chains. No sort runs,
/// and a 100-row page of a broad query leaves as soon as 100 entries have
/// been walked. Everything else takes the candidate path: scan, order, slice.
/// `allow_name_level` exists for the test that holds the two paths equal.
#[allow(clippy::too_many_arguments)]
fn page_refs(
    index: &VolumeIndex,
    mount_prefix: &str,
    parsed: &ParsedQuery,
    scope: Option<EntryIdx>,
    sort: SortSpec,
    offset: u32,
    limit: Option<u32>,
    allow_name_level: bool,
) -> (u64, Vec<HitRef>) {
    let compiled = CompiledQuery::new(parsed, mount_prefix);
    // A scope at the root admits every entry with a path, as no scope does,
    // so it goes: kept, it would bar the name-level path, and `-path C:\ e`
    // ran three times slower than `e`.
    let scope = scope.filter(|&s| s != index.root());
    if allow_name_level
        && sort.key == SortKey::Name
        && is_name_level(index, parsed, &compiled, scope)
    {
        let mut cp_buf = CodePointBuf::new();
        let mut ext_scratch = Vec::new();
        let (matched, total) = match_names(index, parsed, &compiled, &mut cp_buf, &mut ext_scratch);
        let (start, end) = page_bounds(total as usize, offset, limit);
        return (total, name_level_refs(index, matched, sort.dir, start, end));
    }
    let mut candidates = scan(index, parsed, &compiled, scope);
    let total = candidates.len() as u64;
    let (start, end) = page_bounds(candidates.len(), offset, limit);
    order(index, &mut candidates, sort, end);
    let refs = candidates[start..end]
        .iter()
        .map(|c| HitRef {
            idx: c.idx,
            frn: index.entry(c.idx).frn(),
        })
        .collect();
    (total, refs)
}

/// Whether every predicate of `parsed` is decided by the file name alone, so
/// the query can run over the distinct names instead of the entries: no
/// scope, no path term the mount prefix leaves open, no kind, size or date
/// filter, and no entry without a path (an orphan, or one in a parent cycle)
/// whose presentability would need a per-entry walk. Extensions, wildcards
/// and case sensitivity are properties of the name and pass.
fn is_name_level(
    index: &VolumeIndex,
    parsed: &ParsedQuery,
    compiled: &CompiledQuery,
    scope: Option<EntryIdx>,
) -> bool {
    scope.is_none()
        && compiled.volume_admits
        && !compiled.tests_paths(parsed)
        && parsed.filters.kind.is_none()
        && parsed.filters.size.is_none()
        && parsed.filters.date.is_none()
        && !index.may_have_unpresentable()
}

/// The name predicates of `parsed` against one distinct name: substring
/// terms, wildcards and the extension filter, on the folded bytes
/// (case-insensitive) or the raw bytes (case-sensitive), matching how the
/// needles were folded at parse time. Shared by the per-entry
/// [`evaluate`] and the per-name [`match_names`], so the two paths cannot
/// disagree on what a name matches.
fn name_matches(
    index: &VolumeIndex,
    parsed: &ParsedQuery,
    compiled: &CompiledQuery,
    id: NameId,
    cp_buf: &mut CodePointBuf,
    ext_scratch: &mut Vec<u8>,
) -> bool {
    let hay = if parsed.match_case {
        index.name_raw(id)
    } else {
        index.name_folded(id)
    };
    compiled.name_finders.iter().all(|f| f.find(hay).is_some())
        && parsed.wildcards.iter().all(|w| w.matches_into(hay, cp_buf))
        // The ext check folds the extension, so it runs after the
        // allocation-free predicates that reject most names.
        && ext_matches(&parsed.filters, index.name_raw(id), ext_scratch)
}

/// Calls `f` once per distinct name whose folded bytes contain `needle`: one
/// `memmem` pass over the folded arena, which holds each name once, mapping
/// the ascending hit offsets to ids with a forward cursor over `name_pairs`
/// (amortized O(1) per hit; a per-hit binary search cache-thrashes at scale).
/// The first hit seeds the cursor with one binary search so a rare term whose
/// matches start deep in the arena does not pay a full forward walk. A name
/// is a contiguous NUL-separated region, so repeated hits inside one name are
/// adjacent and a "same as last" check dedups them.
fn for_each_prefilter_hit(index: &VolumeIndex, needle: &[u8], mut f: impl FnMut(NameId)) {
    let driver = memmem::Finder::new(needle);
    let hay = index.folded_bytes();
    let pairs = index.name_pairs();
    if pairs.is_empty() {
        return;
    }
    let mut cursor = 0usize;
    let mut seeded = false;
    let mut last_id = NameId::MAX;
    for pos in driver.find_iter(hay) {
        let pos = pos as u32;
        if !seeded {
            cursor = pairs
                .partition_point(|p| p.folded.off <= pos)
                .saturating_sub(1);
            seeded = true;
        } else {
            while cursor + 1 < pairs.len() && pairs[cursor + 1].folded.off <= pos {
                cursor += 1;
            }
        }
        let id = cursor as NameId;
        if id == last_id {
            continue;
        }
        last_id = id;
        f(id);
    }
}

/// Every distinct name matching `parsed`, in id order, and the number of
/// live non-synthetic entries bearing them. The count comes from each name's
/// refcount, which is its chain length, so no entry is touched; the three
/// names synthetic entries can bear are walked instead.
fn match_names(
    index: &VolumeIndex,
    parsed: &ParsedQuery,
    compiled: &CompiledQuery,
    cp_buf: &mut CodePointBuf,
    ext_scratch: &mut Vec<u8>,
) -> (Vec<NameId>, u64) {
    let mut ids: Vec<NameId> = Vec::new();
    let mut verify = true;
    if !parsed.match_case
        && let Some(needle) = compiled.prefilter.as_deref()
    {
        for_each_prefilter_hit(index, needle, |id| ids.push(id));
        verify = !compiled.prefilter_decides_name;
    } else {
        // Case-sensitive, or no literal to drive the arena scan: every live
        // name is tested. Dead names are zero-filled and have no entries.
        ids.extend((0..index.name_count() as NameId).filter(|&id| index.name_refcount(id) > 0));
    }
    let mut matched = Vec::with_capacity(ids.len());
    let mut total = 0u64;
    for id in ids {
        if verify && !name_matches(index, parsed, compiled, id, cp_buf, ext_scratch) {
            continue;
        }
        total += if index.name_may_bear_synthetic(id) {
            index
                .name_chain(id)
                .filter(|&e| !index.is_synthetic(e))
                .count() as u64
        } else {
            u64::from(index.name_refcount(id))
        };
        matched.push(id);
    }
    (matched, total)
}

/// `matched` (any order) in name order: by folded bytes, then id. A sparse
/// match set is sorted directly; a dense one filters the index's name order
/// through a bitmap and merges in the few names still waiting unranked, so
/// a broad query never sorts a million names.
fn ordered_names(index: &VolumeIndex, mut matched: Vec<NameId>) -> Vec<NameId> {
    let key = |id: NameId| (index.name_folded(id), id);
    let order = index.name_order();
    if order.is_empty() || matched.len() * 8 < order.len() {
        matched.sort_unstable_by(|&a, &b| key(a).cmp(&key(b)));
        return matched;
    }
    let mut bits = vec![0u64; index.name_count().div_ceil(64)];
    let mut waiting: Vec<NameId> = Vec::new();
    for &id in &matched {
        if index.name_is_ranked(id) {
            bits[id as usize / 64] |= 1 << (id % 64);
        } else {
            waiting.push(id);
        }
    }
    waiting.sort_unstable_by(|&a, &b| key(a).cmp(&key(b)));
    let mut ranked = order
        .iter()
        .copied()
        .filter(|&id| bits[id as usize / 64] & (1 << (id % 64)) != 0)
        .peekable();
    let mut waiting = waiting.into_iter().peekable();
    let mut seq = Vec::with_capacity(matched.len());
    while let (Some(r), Some(w)) = (ranked.peek().copied(), waiting.peek().copied()) {
        if key(w) < key(r) {
            seq.push(w);
            waiting.next();
        } else {
            seq.push(r);
            ranked.next();
        }
    }
    seq.extend(ranked);
    seq.extend(waiting);
    seq
}

/// Rows `[start, end)` of a name-level query: walks the matched names in
/// name order (reversed for a descending sort) and each name's same-name
/// chain. Names with equal folded bytes form one group whose entries
/// interleave by idx (descending when the sort is), exactly the
/// `(folded name, idx)` order the candidate comparator produces.
fn name_level_refs(
    index: &VolumeIndex,
    matched: Vec<NameId>,
    dir: SortDir,
    start: usize,
    end: usize,
) -> Vec<HitRef> {
    let mut seq = ordered_names(index, matched);
    if dir == SortDir::Desc {
        seq.reverse();
    }
    let mut refs = Vec::with_capacity((end - start).min(1 << 16));
    let mut group: Vec<EntryIdx> = Vec::new();
    let mut seen = 0usize;
    let mut i = 0;
    while i < seq.len() && seen < end {
        let folded = index.name_folded(seq[i]);
        let mut j = i + 1;
        while j < seq.len() && index.name_folded(seq[j]) == folded {
            j += 1;
        }
        group.clear();
        for &id in &seq[i..j] {
            group.extend(index.name_chain(id).filter(|&e| !index.is_synthetic(e)));
        }
        // A chain is in link order, not idx order.
        group.sort_unstable();
        if dir == SortDir::Desc {
            group.reverse();
        }
        for &e in &group {
            if seen >= end {
                break;
            }
            if seen >= start {
                refs.push(HitRef {
                    idx: e,
                    frn: index.entry(e).frn(),
                });
            }
            seen += 1;
        }
        i = j;
    }
    refs
}

/// A folder-scoped query walks the scope's subtree instead of scanning the
/// whole volume, up to this many visited entries. Past it (a near-volume
/// scope), the walk aborts and the ordinary scan+ancestry-filter path runs:
/// pointer-chasing millions of entries is slower than the SIMD arena scan.
/// The cutoff makes a Downloads-sized scope independent of disk size: the
/// old path cost ~300 ms on a broad query over 3.9M entries because every
/// candidate on the volume paid an ancestry check; the walk touches only
/// the folder.
const SUBTREE_WALK_MAX: usize = 200_000;

/// Collects every match of `parsed` under `scope`, in scan order.
fn scan(
    index: &VolumeIndex,
    parsed: &ParsedQuery,
    compiled: &CompiledQuery,
    scope: Option<EntryIdx>,
) -> Vec<Candidate> {
    if !compiled.volume_admits {
        return Vec::new();
    }
    // Built only when a full path has something left to decide.
    let mut folders = compiled
        .tests_paths(parsed)
        .then(|| FolderPaths::new(index, &compiled.prefix, !parsed.match_case));
    let mut scope_memo: FxHashMap<EntryIdx, bool> = FxHashMap::default();
    // Each parent directory's reconstructed path, or None when it has none (its
    // ancestry ends in lost+found, or hit the hop cap), for the presentability
    // check in `evaluate`.
    let mut dir_paths: FxHashMap<EntryIdx, Option<Vec<u8>>> = FxHashMap::default();
    let mut candidates: Vec<Candidate> = Vec::new();
    // Per-candidate scratch buffers, allocated once and reused across the scan
    // so wildcard decoding and extension folding don't heap-allocate per hit.
    let mut cp_buf = CodePointBuf::new();
    let mut ext_scratch: Vec<u8> = Vec::new();

    // Folder scope: walk the scope subtree via the child chains, so cost
    // tracks the folder, not the volume. Membership is by construction, so
    // `evaluate` runs without the scope (its ancestry walk is the expensive
    // part this path exists to avoid); every other predicate is identical to
    // the scan paths below. Oversized scopes abort to those paths.
    let mut subtree_done = false;
    if let Some(root) = scope {
        let mut visited = 0usize;
        subtree_done = true;
        for idx in index.subtree_entries(root) {
            visited += 1;
            if visited > SUBTREE_WALK_MAX {
                candidates.clear();
                subtree_done = false;
                break;
            }
            if index.is_synthetic(idx) {
                continue;
            }
            if let Some(c) = evaluate(
                index,
                parsed,
                compiled,
                idx,
                None,
                &mut scope_memo,
                &mut dir_paths,
                &mut folders,
                false,
                &mut cp_buf,
                &mut ext_scratch,
            ) {
                candidates.push(c);
            }
        }
    }

    // Fast path: case-insensitive query with a usable literal → scan the
    // folded arena for the prefilter literal and verify the rest per hit.
    if subtree_done {
        // Scoped walk answered the query; the scan paths are skipped.
    } else if !parsed.match_case
        && let Some(needle) = compiled.prefilter.as_deref()
    {
        // The scan cost is proportional to the unique-name bytes (~1/3 of
        // the total on a real volume); a hit fans out to every entry bearing
        // the name through its same-name chain. The chains are exact, so no
        // per-candidate staleness guard is needed.
        for_each_prefilter_hit(index, needle, |id| {
            for idx in index.name_chain(id) {
                if index.is_synthetic(idx) {
                    continue;
                }
                if let Some(c) = evaluate(
                    index,
                    parsed,
                    compiled,
                    idx,
                    scope,
                    &mut scope_memo,
                    &mut dir_paths,
                    &mut folders,
                    compiled.prefilter_decides_name,
                    &mut cp_buf,
                    &mut ext_scratch,
                ) {
                    candidates.push(c);
                }
            }
        });
    } else if let Some(piece) = compiled.path_piece.as_deref()
        && walk_below_names(index, piece, index.len() / PATH_WALK_SHARE, |idx| {
            if index.is_synthetic(idx) {
                return;
            }
            if let Some(c) = evaluate(
                index,
                parsed,
                compiled,
                idx,
                scope,
                &mut scope_memo,
                &mut dir_paths,
                &mut folders,
                false,
                &mut cp_buf,
                &mut ext_scratch,
            ) {
                candidates.push(c);
            }
        })
    {
        // Every match is at or below a name holding the piece, and the walk
        // visited each of those entries once.
    } else {
        // Slow path: case-sensitive, or no positive substring term (only
        // wildcards / filters / path terms), or a path walk that grew past
        // its share. Iterate every live entry.
        candidates.clear();
        for idx in index.live_entries() {
            if index.is_synthetic(idx) {
                continue;
            }
            if let Some(c) = evaluate(
                index,
                parsed,
                compiled,
                idx,
                scope,
                &mut scope_memo,
                &mut dir_paths,
                &mut folders,
                false,
                &mut cp_buf,
                &mut ext_scratch,
            ) {
                candidates.push(c);
            }
        }
    }

    candidates
}

/// `[start, end)` of the requested page within `n` matches.
fn page_bounds(n: usize, offset: u32, limit: Option<u32>) -> (usize, usize) {
    let start = (offset as usize).min(n);
    let end = match limit {
        Some(l) => (start + l as usize).min(n),
        None => n,
    };
    (start, end)
}

/// Orders `candidates` so that `[..end]` holds the first `end` matches under
/// `sort`, in order (see [`order_top`]). A path sort first ranks each distinct
/// parent directory; no candidate carries a key string.
fn order(index: &VolumeIndex, candidates: &mut [Candidate], sort: SortSpec, end: usize) {
    match sort.key {
        // Two integers per comparison instead of two folded names: see
        // `NameStore::rank_key`. Names still waiting outside the order get a
        // key between their neighbours, and ties fall back to the bytes.
        SortKey::Name => {
            for c in candidates.iter_mut() {
                c.key = index.name_rank_key(index.name_id(c.idx));
            }
        }
        SortKey::Path => path_keys(index, candidates),
        SortKey::Size => {
            for c in candidates.iter_mut() {
                c.key = index.entry(c.idx).size().unwrap_or(0);
            }
        }
        SortKey::DateModified => {
            for c in candidates.iter_mut() {
                c.key = mtime_key(index.entry(c.idx).mtime());
            }
        }
    }
    order_top(index, candidates, sort, end);
}

/// Maps an mtime to a `u64` that orders like the `i64`, unknown lowest (as
/// `unwrap_or(i64::MIN)` ordered it), so a date sort compares one integer.
fn mtime_key(mtime: Option<i64>) -> u64 {
    (mtime.unwrap_or(i64::MIN) as u64) ^ (1 << 63)
}

/// Keys a path sort with the parent folder's rank in the high half and the
/// name's rank key in the low half, so the comparator orders by `(folded
/// parent path, folded name, idx)`, es's `(parent path, name)` order, with
/// integers alone and no path built. [`FolderRanks`] ranks each distinct
/// parent by walking the tree; folders with equal folded paths share a rank,
/// and one with no full path (under lost+found, in a cycle, or too deep)
/// ranks last, its candidates dropped at page time anyway.
///
/// A rank key outgrows `u32` only past two billion names, and clamping it
/// keeps the order: clamped keys tie, and the comparator breaks a tie on the
/// folded name, which orders as the rank keys do.
fn path_keys(index: &VolumeIndex, candidates: &mut [Candidate]) {
    use rayon::prelude::*;
    let mut folders = FolderRanks::new(index);
    for c in candidates.iter_mut() {
        c.key = u64::from(folders.node(index, index.entry(c.idx).parent()));
    }
    let ranks = folders.ranks(index);
    // Reads only, so a broad match set splits across cores.
    candidates
        .par_iter_mut()
        .with_min_len(1 << 14)
        .for_each(|c| {
            let name = index.name_rank_key(index.name_id(c.idx));
            c.key = (u64::from(ranks[c.key as usize]) << 32) | name.min(u64::from(u32::MAX));
        });
}

/// Builds the page's hits with their volume-relative paths.
///
/// A broad result set holds millions of files under far fewer distinct
/// directories, so each parent directory's reconstructed path is memoized
/// (`path_of` runs once per directory) and every file's path is
/// `parent + '\' + name` instead of a walk of the whole parent chain per hit.
/// Correct because `path_of(child) == path_of(parent) + '\' + name` for any
/// presentable child (a parent under lost+found or a cycle is `None` and the
/// hit is dropped, exactly as a per-hit `path_of` would); real volumes never
/// approach the MAX_PATH_HOPS depth where the two could diverge.
fn build_hits(index: &VolumeIndex, page: &[HitRef]) -> Vec<QueryHit> {
    let mut dir_paths: FxHashMap<EntryIdx, Option<Vec<u8>>> = FxHashMap::default();
    let mut hits = Vec::with_capacity(page.len());
    for c in page {
        let entry = index.entry(c.idx);
        let parent = entry.parent();
        let parent_path = dir_paths.entry(parent).or_insert_with(|| {
            let mut buf = Vec::new();
            match index.path_of(parent, &mut buf) {
                crate::index::PathStatus::Ok => Some(buf),
                _ => None, // parent under lost+found or a cycle: not presentable
            }
        });
        let Some(parent_path) = parent_path.as_deref() else {
            continue; // parent not presentable -> path_of(idx) would be non-Ok too
        };
        let name = entry.name();
        let mut path = Vec::with_capacity(parent_path.len() + 1 + name.len());
        if parent_path.is_empty() {
            path.extend_from_slice(name); // a file/dir directly under the volume root
        } else {
            path.extend_from_slice(parent_path);
            path.push(b'\\');
            path.extend_from_slice(name);
        }
        hits.push(QueryHit {
            idx: c.idx,
            path,
            is_dir: entry.is_dir(),
            size: entry.size(),
            mtime: entry.mtime(),
        });
    }
    hits
}

/// Verifies all predicates for one candidate, returning it if it matches.
/// `folders` is `Some` exactly when [`CompiledQuery::tests_paths`].
#[allow(clippy::too_many_arguments)]
fn evaluate(
    index: &VolumeIndex,
    parsed: &ParsedQuery,
    compiled: &CompiledQuery,
    idx: EntryIdx,
    scope: Option<EntryIdx>,
    scope_memo: &mut FxHashMap<EntryIdx, bool>,
    dir_paths: &mut FxHashMap<EntryIdx, Option<Vec<u8>>>,
    folders: &mut Option<FolderPaths>,
    name_decided: bool,
    cp_buf: &mut CodePointBuf,
    ext_scratch: &mut Vec<u8>,
) -> Option<Candidate> {
    let entry = index.entry(idx);
    if !kind_matches(parsed.filters.kind, entry.is_dir()) {
        return None;
    }
    // Each column is read only when its filter asks: a broad query visits
    // millions of entries in no particular order, and an unconditional
    // read of the size and date columns cost each of them two cache misses.
    if let Some(range) = parsed.filters.size
        && !size_matches(range, entry.size())
    {
        return None;
    }
    if let Some(filter) = parsed.filters.date
        && !filter.matches(entry.mtime())
    {
        return None;
    }

    // `name_decided`: the caller reached this entry through a prefilter hit
    // that already settles the name predicates (`prefilter_decides_name`).
    if !name_decided
        && !name_matches(
            index,
            parsed,
            compiled,
            index.name_id(idx),
            cp_buf,
            ext_scratch,
        )
    {
        return None;
    }

    if let Some(root) = scope
        && !in_scope(index, idx, root, scope_memo)
    {
        return None;
    }

    // An entry whose parent has no presentable path (ancestry ends in lost+found
    // or runs in a cycle) is dropped by the page loop. Reject it here so it never counts toward
    // `total` and never occupies a page slot: otherwise `goz -n 10 report` can
    // print nothing while reporting a non-zero total, and the same query without
    // `-n` prints the real hits.
    //
    // Calls the very function the page loop uses rather than re-deriving its
    // rules, so the two cannot drift, and memoizes per parent directory rather
    // than per file. Gated on `may_have_unpresentable` because an index with
    // nothing under lost+found and no parent cycle cannot contain an
    // unpresentable entry: the gate is two loads, while the walk it guards is one `path_of` per distinct parent
    // directory, which on a broad limited query is tens of thousands of walks
    // the page (100 rows) would never have needed. Paying it unconditionally
    // cost a broad limited query ~20%.
    //
    // A full path test decides presentability as well: an entry with no
    // path fails it.
    match folders {
        Some(folders) => {
            if !folders.full_path_matches(index, idx, parsed, compiled, cp_buf) {
                return None;
            }
        }
        None => {
            if index.may_have_unpresentable()
                && parent_path(index, entry.parent(), dir_paths).is_none()
            {
                return None;
            }
        }
    }

    Some(Candidate { idx, key: 0 })
}

fn kind_matches(kind: Option<Kind>, is_dir: bool) -> bool {
    match kind {
        None => true,
        Some(Kind::File) => !is_dir,
        Some(Kind::Folder) => is_dir,
    }
}

/// A size filter excludes entries whose size is unknown (not yet enriched):
/// we cannot confirm the predicate, so we do not claim a match.
fn size_matches(range: SizeRange, size: Option<u64>) -> bool {
    size.is_some_and(|s| range.contains(s))
}

/// Extension matching is always case-insensitive (es semantics): the filter
/// values are folded at parse time, so the candidate extension is folded here
/// unconditionally, even under a case-sensitive query.
fn ext_matches(filters: &Filters, name: &[u8], scratch: &mut Vec<u8>) -> bool {
    let Some(exts) = &filters.ext else {
        return true;
    };
    let Some(dot) = name.iter().rposition(|&b| b == b'.') else {
        return false; // no extension → cannot match an ext: filter
    };
    scratch.clear();
    crate::fold::fold_into(&name[dot + 1..], scratch);
    exts.iter().any(|e| e.as_slice() == scratch.as_slice())
}

fn fold(bytes: &[u8]) -> Vec<u8> {
    crate::fold::fold(bytes)
}

/// The reconstructed path of `parent`, or `None` when it has none. Memoized per
/// directory and shared with the page builder.
///
/// Returns the path rather than a bool so ONE walk serves both callers: the
/// candidate filter needs to know whether a hit is presentable at all (so
/// `total` counts only what the page can return), and the page builder needs the
/// path itself. Computing them separately walked every parent chain twice.
fn parent_path<'a>(
    index: &VolumeIndex,
    parent: EntryIdx,
    memo: &'a mut FxHashMap<EntryIdx, Option<Vec<u8>>>,
) -> &'a Option<Vec<u8>> {
    memo.entry(parent).or_insert_with(|| {
        let mut buf = Vec::new();
        match index.path_of(parent, &mut buf) {
            crate::index::PathStatus::Ok => Some(buf),
            _ => None, // parent under lost+found or a cycle: not presentable
        }
    })
}

/// Folder paths longer than this past the prefix are rebuilt from their
/// nearest cached ancestor when needed instead of cached: caching every
/// folder of a deep chain of long names would store each ancestor's path
/// again inside every descendant's, which grows with the square of the depth.
const CACHED_PATH_MAX: usize = 1024;

/// The most path bytes [`FolderPaths`] holds before it starts over: 16 MB,
/// which also bounds the arena's allocation, since its doubling stops there.
/// A query that spans more folders than fit builds some of them again, each
/// at most once per start.
const FOLDER_PATH_BYTES_MAX: usize = 16 << 20;

/// The most folders [`FolderPaths`] knows before it starts over: what a table
/// of 2^19 slots of 21 bytes holds, since the table grows past 7/8 full.
/// With the paths, about 27 MB, under the 32 MB the daemon allows the parent
/// paths it keeps for one query's pages.
const FOLDERS_MAX: usize = (1 << 19) / 8 * 7;

/// A folder [`FolderPaths`] has built: where its path past the prefix lies in
/// the arena, and how many entries its walk to the root visits, itself and
/// the root included.
#[derive(Clone, Copy)]
struct Folder {
    start: u32,
    end: u32,
    depth: u32,
}

/// Each folder's full path as the query compares it: the mount prefix, then
/// the names down from the root joined by `\`, folded iff the query ignores
/// case. A folder's path is built once per query from its parent's, so the
/// test of a candidate appends one name instead of walking to the root and
/// folding the result.
///
/// The bytes equal `prefix + path_of` folded alike, because folding never
/// joins characters across a `\`, and an entry has a path exactly when
/// [`VolumeIndex::path_of`] gives one: its walk ends at the root within
/// [`MAX_PATH_HOPS`] entries.
struct FolderPaths {
    root: EntryIdx,
    fold: bool,
    prefix_len: usize,
    /// [`FOLDER_PATH_BYTES_MAX`] and [`FOLDERS_MAX`], or less in tests.
    max_bytes: usize,
    max_folders: usize,
    /// Each cached folder's path past the prefix, back to back.
    arena: Vec<u8>,
    /// `None` for a folder with no path. A folder missing here is not built
    /// yet, or has a path too long to cache.
    known: FxHashMap<EntryIdx, Option<Folder>>,
    /// The folder whose full path `path` holds, and its depth (`None` when it
    /// has no path). Siblings tested in a row share it without a lookup.
    current: Option<(EntryIdx, Option<u32>)>,
    /// The prefix and the current folder's path, and while a candidate is
    /// tested, its name.
    path: Vec<u8>,
    /// What each path term needs of a name in the folder `needs_for`.
    needs: Vec<Need>,
    needs_for: Option<EntryIdx>,
}

/// What a path term needs of a candidate's name for its full path to hold the
/// term, decided once per folder from the folder's path and the separator
/// that follows it. A name holds no separator, so a term with one either lies
/// in the folder's part, or its last separator is the one before the name.
#[derive(Clone, Copy)]
enum Need {
    /// The folder's part holds the term.
    Nothing,
    /// The folder's part ends with the term up to its last separator; the
    /// name must start with the term's bytes from this offset on.
    Start(usize),
    /// The term holds no separator, so the name must hold it.
    Within,
    /// No name can complete the term.
    Never,
    /// The folder's part does not end with a separator (a mount prefix
    /// without one), so the term is searched in the whole path.
    Whole,
}

impl FolderPaths {
    /// `prefix` is the mount prefix folded iff `fold`, as the query's path
    /// terms are.
    fn new(index: &VolumeIndex, prefix: &[u8], fold: bool) -> Self {
        let mut this = Self {
            root: index.root(),
            fold,
            prefix_len: prefix.len(),
            max_bytes: FOLDER_PATH_BYTES_MAX,
            max_folders: FOLDERS_MAX,
            arena: Vec::new(),
            known: FxHashMap::default(),
            current: None,
            path: prefix.to_vec(),
            needs: Vec::new(),
            needs_for: None,
        };
        this.restart();
        this
    }

    /// Forgets every folder but the root, whose path is the prefix alone.
    fn restart(&mut self) {
        self.arena.clear();
        self.known.clear();
        let root = Folder {
            start: 0,
            end: 0,
            depth: 1,
        };
        self.known.insert(self.root, Some(root));
    }

    /// Whether `idx`'s full path holds every path term the prefix leaves open
    /// and matches every path wildcard. `false` when `idx` has no path.
    fn full_path_matches(
        &mut self,
        index: &VolumeIndex,
        idx: EntryIdx,
        parsed: &ParsedQuery,
        compiled: &CompiledQuery,
        cp_buf: &mut CodePointBuf,
    ) -> bool {
        let folder = index.entry(idx).parent();
        // The entry's own walk is one entry longer than its folder's.
        if !self
            .load(index, folder)
            .is_some_and(|depth| (depth as usize) < MAX_PATH_HOPS)
        {
            return false;
        }
        if self.needs_for != Some(folder) {
            self.decide_needs(compiled);
            self.needs_for = Some(folder);
        }
        let name = name_bytes(index, idx, self.fold);
        // NTFS keeps separators out of names. One that corrupt data put there
        // could complete a term anywhere, so that name takes the whole path.
        let mut whole = memchr::memchr(b'\\', name).is_some();
        if !whole {
            for (finder, need) in compiled.path_finders.iter().zip(&self.needs) {
                let held = match *need {
                    Need::Nothing => true,
                    Need::Start(from) => name.starts_with(&finder.needle()[from..]),
                    Need::Within => finder.find(name).is_some(),
                    Need::Never => false,
                    Need::Whole => {
                        whole = true;
                        true
                    }
                };
                if !held {
                    return false;
                }
            }
        }
        if !whole && parsed.path_wildcards.is_empty() {
            return true;
        }
        self.with_full_path(index, idx, |full| {
            (!whole || compiled.path_finders.iter().all(|f| f.find(full).is_some()))
                && parsed
                    .path_wildcards
                    .iter()
                    .zip(&compiled.path_wildcard_ends)
                    .all(|(w, (lead, trail))| {
                        full.starts_with(lead)
                            && full.ends_with(trail)
                            && w.matches_into(full, cp_buf)
                    })
        })
        .unwrap_or(false)
    }

    /// Decides what each path term needs of a name in the folder whose path
    /// `path` holds.
    fn decide_needs(&mut self, compiled: &CompiledQuery) {
        let folder_end = self.path.len();
        if folder_end > self.prefix_len {
            self.path.push(b'\\');
        }
        let before_name = self.path.as_slice();
        let open = before_name.last() == Some(&b'\\');
        self.needs.clear();
        for finder in &compiled.path_finders {
            let term = finder.needle();
            self.needs.push(if finder.find(before_name).is_some() {
                Need::Nothing
            } else if !open {
                Need::Whole
            } else {
                match memchr::memrchr(b'\\', term) {
                    Some(last) if before_name.ends_with(&term[..=last]) => Need::Start(last + 1),
                    Some(_) => Need::Never,
                    None => Need::Within,
                }
            });
        }
        self.path.truncate(folder_end);
    }

    /// `f` of `idx`'s full path, or `None` when `idx` has no path.
    fn with_full_path<R>(
        &mut self,
        index: &VolumeIndex,
        idx: EntryIdx,
        f: impl FnOnce(&[u8]) -> R,
    ) -> Option<R> {
        // The entry's own walk is one entry longer than its folder's.
        let depth = self.load(index, index.entry(idx).parent())?;
        if depth as usize >= MAX_PATH_HOPS {
            return None;
        }
        let folder_end = self.path.len();
        if folder_end > self.prefix_len {
            self.path.push(b'\\');
        }
        self.path
            .extend_from_slice(name_bytes(index, idx, self.fold));
        let out = f(&self.path);
        self.path.truncate(folder_end);
        Some(out)
    }

    /// Puts `dir`'s full path in `path` and returns its depth, or `None` when
    /// it has no path.
    fn load(&mut self, index: &VolumeIndex, dir: EntryIdx) -> Option<u32> {
        match self.current {
            Some((cur, depth)) if cur == dir => depth,
            _ => {
                let depth = self.build(index, dir);
                self.current = Some((dir, depth));
                depth
            }
        }
    }

    fn build(&mut self, index: &VolumeIndex, dir: EntryIdx) -> Option<u32> {
        self.path.truncate(self.prefix_len);
        // Only a top has no parent, and `path_of` starts a path at any top
        // but lost+found.
        if dir == NIL {
            return Some(0);
        }
        // Climb to the nearest known folder, then build each folder on the
        // way back down from its parent's path.
        let mut chain: SmallVec<[EntryIdx; 32]> = SmallVec::new();
        let mut cur = dir;
        let base = loop {
            if let Some(&known) = self.known.get(&cur) {
                break known;
            }
            if chain.len() == MAX_PATH_HOPS {
                // More than MAX_PATH_HOPS entries from any top, or a cycle.
                self.remember(dir, None);
                return None;
            }
            chain.push(cur);
            let parent = index.entry(cur).parent();
            if parent == NIL {
                // The root is always known.
                break (cur != index.lost_found()).then_some(Folder {
                    start: 0,
                    end: 0,
                    depth: 0,
                });
            }
            cur = parent;
        };
        let Some(base) = base else {
            for &folder in &chain {
                self.remember(folder, None);
            }
            return None;
        };
        self.path
            .extend_from_slice(&self.arena[base.start as usize..base.end as usize]);
        let mut depth = base.depth as usize;
        for (i, &folder) in chain.iter().rev().enumerate() {
            depth += 1;
            if depth > MAX_PATH_HOPS {
                // This folder and every one below it walk too far.
                for &deeper in chain.iter().rev().skip(i) {
                    self.remember(deeper, None);
                }
                return None;
            }
            if self.path.len() > self.prefix_len {
                self.path.push(b'\\');
            }
            self.path
                .extend_from_slice(name_bytes(index, folder, self.fold));
            if self.path.len() - self.prefix_len <= CACHED_PATH_MAX {
                self.cache(folder, depth as u32);
            }
        }
        Some(depth as u32)
    }

    /// Caches `folder` with the path `path` now holds.
    fn cache(&mut self, folder: EntryIdx, depth: u32) {
        let rel = self.path.len() - self.prefix_len;
        if self.arena.len() + rel > self.max_bytes || self.known.len() >= self.max_folders {
            self.restart();
        }
        let needed = self.arena.len() + rel;
        if needed > self.arena.capacity() {
            // Double as a Vec would, but never past the cap: left to itself,
            // doubling from an odd first path ends anywhere up to twice it.
            let target = (self.arena.capacity() * 2).min(self.max_bytes).max(needed);
            self.arena.reserve_exact(target - self.arena.len());
        }
        let start = self.arena.len() as u32;
        self.arena.extend_from_slice(&self.path[self.prefix_len..]);
        let end = self.arena.len() as u32;
        self.known
            .insert(folder, Some(Folder { start, end, depth }));
    }

    fn remember(&mut self, folder: EntryIdx, known: Option<Folder>) {
        if self.known.len() >= self.max_folders {
            self.restart();
        }
        self.known.insert(folder, known);
    }
}

/// `idx`'s name as a full path of the query holds it: folded iff `fold`.
fn name_bytes(index: &VolumeIndex, idx: EntryIdx, fold: bool) -> &[u8] {
    let id = index.name_id(idx);
    if fold {
        index.name_folded(id)
    } else {
        index.name_raw(id)
    }
}

/// A path walk visits at most this share of the volume (a quarter) before
/// the scan of every entry takes over, which reads the entries in order
/// rather than chasing their child chains.
const PATH_WALK_SHARE: usize = 4;

/// Calls `visit` once for each entry at or below an entry whose folded name
/// holds `piece`: when a match can hold `piece` only in a name of its path,
/// the only entries that can match. A walk that tops out at lost+found or
/// runs past [`MAX_PATH_HOPS`] holds no path and is skipped; `visit` still
/// sees entries too deep for a path, for its caller to reject.
///
/// Returns `false`, having visited some entries, once more than `budget`
/// entries would be visited.
fn walk_below_names(
    index: &VolumeIndex,
    piece: &[u8],
    budget: usize,
    mut visit: impl FnMut(EntryIdx),
) -> bool {
    let mut bearing = 0usize;
    let mut tops: Vec<EntryIdx> = Vec::new();
    for_each_prefilter_hit(index, piece, |id| {
        bearing += index.name_refcount(id) as usize;
        if bearing <= budget {
            tops.extend(index.name_chain(id));
        }
    });
    if bearing > budget {
        return false;
    }
    // Sorted so that a climb can ask whether a folder is a top.
    tops.sort_unstable();
    let mut visited = 0usize;
    for &top in &tops {
        if !walk_starts_at(index, top, &tops) {
            continue;
        }
        for idx in core::iter::once(top).chain(index.subtree_entries(top)) {
            visited += 1;
            if visited > budget {
                return false;
            }
            visit(idx);
        }
    }
    true
}

/// Whether [`walk_below_names`] walks from `top`: its chain reaches a top
/// other than lost+found within [`MAX_PATH_HOPS`] entries, and passes no
/// other of `tops` (sorted), whose walk covers it.
fn walk_starts_at(index: &VolumeIndex, top: EntryIdx, tops: &[EntryIdx]) -> bool {
    let mut cur = top;
    for _ in 0..MAX_PATH_HOPS {
        let parent = index.entry(cur).parent();
        if parent == NIL {
            return cur != index.lost_found();
        }
        if tops.binary_search(&parent).is_ok() {
            return false;
        }
        cur = parent;
    }
    false
}

/// Is `node` a descendant of `root`? Memoized parent walk.
fn in_scope(
    index: &VolumeIndex,
    node: EntryIdx,
    root: EntryIdx,
    memo: &mut FxHashMap<EntryIdx, bool>,
) -> bool {
    // A scope excludes the folder itself (es: descendants only).
    if node == root {
        return false;
    }
    // Mirrors `VolumeIndex::path_of`: directory depths under 32 stay on the
    // stack, so the per-candidate ancestry walk doesn't heap-allocate.
    let mut chain: SmallVec<[EntryIdx; 32]> = SmallVec::new();
    let mut cur = node;
    let verdict = loop {
        if cur == root {
            break true;
        }
        if let Some(&v) = memo.get(&cur) {
            break v;
        }
        let parent = index.entry(cur).parent();
        if parent == crate::types::NIL {
            break false;
        }
        chain.push(cur);
        cur = parent;
        if chain.len() > 4096 {
            break false; // cycle guard
        }
    };
    for e in chain {
        memo.insert(e, verdict);
    }
    verdict
}

/// Total ordering of two candidates under `sort` (direction included), so the
/// "smallest" `end` candidates are exactly the page to display.
fn cmp_candidate(
    index: &VolumeIndex,
    sort: SortSpec,
    a: &Candidate,
    b: &Candidate,
) -> core::cmp::Ordering {
    // Folded names are precomputed, so these are cheap byte compares; `key`
    // holds whatever `order` filled for this sort. The idx tiebreak makes the
    // order total, so an unstable sort is deterministic.
    let by_name = || index.folded_name(a.idx).cmp(index.folded_name(b.idx));
    let _ = sort.key; // every key orders the same way: `order` filled `key`
    let base = a
        .key
        .cmp(&b.key)
        .then_with(by_name)
        .then_with(|| a.idx.cmp(&b.idx));
    if sort.dir == SortDir::Desc {
        base.reverse()
    } else {
        base
    }
}

/// Places the top `end` candidates (by `sort`) in `cands[..end]`, sorted. Uses
/// quickselect to partition when `end` is a small slice of a large match set.
fn order_top(index: &VolumeIndex, cands: &mut [Candidate], sort: SortSpec, end: usize) {
    if end == 0 {
        return;
    }
    // `cmp_candidate` breaks ties on the unique `idx`, so the order is total and
    // stability is irrelevant: unstable sort avoids driftsort's scratch buffer
    // and uses the faster ipnsort.
    //
    // A broad query with no limit sorts the whole match set (millions of
    // candidates); that is done in parallel across cores. Quickselect + small
    // sorts stay serial: they touch far less data and the rayon fan-out is not
    // worth it there.
    use rayon::slice::ParallelSliceMut;
    const PARALLEL_SORT_MIN: usize = 100_000;
    if end < cands.len() {
        cands.select_nth_unstable_by(end - 1, |a, b| cmp_candidate(index, sort, a, b));
        // The selected prefix is not always small: a page asking for all but a
        // handful of matches selects nearly everything, and ordering that
        // serially held the index read lock ~6x longer than the parallel branch
        // below does for the same row count, stalling the volume's tail thread
        // for the difference. Same threshold, same work, just not on one core.
        if end >= PARALLEL_SORT_MIN {
            cands[..end].par_sort_unstable_by(|a, b| cmp_candidate(index, sort, a, b));
        } else {
            cands[..end].sort_unstable_by(|a, b| cmp_candidate(index, sort, a, b));
        }
    } else if cands.len() >= PARALLEL_SORT_MIN {
        cands.par_sort_unstable_by(|a, b| cmp_candidate(index, sort, a, b));
    } else {
        cands.sort_unstable_by(|a, b| cmp_candidate(index, sort, a, b));
    }
}

/// Resolves a scope folder path (WTF-8, no volume prefix) to its entry,
/// walking components from the volume root: case-insensitively via the folded
/// directory index, or case-sensitively against raw names when `match_case`. Returns `None`
/// if any component is missing.
pub fn resolve_scope(index: &VolumeIndex, path: &[u8], match_case: bool) -> Option<EntryIdx> {
    let components: Vec<&[u8]> = path
        .split(|&b| b == b'\\' || b == b'/')
        .filter(|c| !c.is_empty())
        .collect();
    if components.is_empty() {
        return Some(index.root());
    }

    // Common case: walk the persistent directory index, O(components). It is
    // keyed by folded name, so it serves case-insensitive scopes directly with
    // no per-query scan of the volume.
    if !match_case {
        let mut cur = index.root();
        for comp in components {
            // Move the folded bytes into the lookup key instead of re-cloning
            // them inside `child_dir` (which previously took `&[u8]`).
            cur = index.child_dir(cur, fold(comp))?;
        }
        return Some(cur);
    }

    // Case-sensitive scopes are rare (the persistent index is folded), so we do
    // not build a whole-volume raw-name map (one key Vec per directory) per
    // query. Walk each component with a linear scan of the live entries,
    // comparing raw names directly (no allocation), stopping at the first
    // matching sibling. NOTE: do not reuse the folded `child_dir` walk with a
    // post-hoc raw-name check: `dir_children` keeps a single entry per
    // (parent, folded name), so it cannot represent case-variant siblings and
    // would miss a legitimately case-matching sibling that folding collapsed.
    let mut cur = index.root();
    'components: for comp in components {
        for idx in index.live_entries() {
            if index.is_synthetic(idx) {
                continue;
            }
            let e = index.entry(idx);
            if e.is_dir() && e.parent() == cur && e.name() == comp {
                cur = idx;
                continue 'components;
            }
        }
        return None;
    }
    Some(cur)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::store::FrnMap;
    use crate::index::{NTFS_ROOT_FRN, VolumeIndex};
    use crate::query::parse::parse_query;
    use crate::types::Frn;
    use crate::usn::record::{
        FILE_ATTRIBUTE_DIRECTORY, ParsedUsnRecord, USN_REASON_FILE_CREATE, USN_REASON_FILE_DELETE,
    };

    fn frn(rec: u64) -> Frn {
        Frn(rec | (1u64 << 48))
    }

    fn enum_rec(f: Frn, parent: Frn, name: &str, is_dir: bool) -> ParsedUsnRecord {
        ParsedUsnRecord {
            major_version: 2,
            frn: f,
            parent_frn: parent,
            usn: 0,
            timestamp_ft: 0,
            reason: USN_REASON_FILE_CREATE,
            attributes: if is_dir { FILE_ATTRIBUTE_DIRECTORY } else { 0 },
            name: name.as_bytes().to_vec(),
            name_lossy: false,
        }
    }

    /// Builds: docs/ (100), docs/sub/ (101), docs/sub/Report.PDF (102, 4KB),
    /// docs/notes.txt (103, 200B), photo.JPG (104, 2MB) at root.
    fn sample_index() -> VolumeIndex {
        let mut idx = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
        let root = NTFS_ROOT_FRN;
        for r in [
            enum_rec(frn(100), root, "docs", true),
            enum_rec(frn(101), frn(100), "sub", true),
            enum_rec(frn(102), frn(101), "Report.PDF", false),
            enum_rec(frn(103), frn(100), "notes.txt", false),
            enum_rec(frn(104), root, "photo.JPG", false),
        ] {
            idx.insert_enum(&r);
        }
        // Enrich sizes via the layout path.
        set_size(&mut idx, frn(102), 4096);
        set_size(&mut idx, frn(103), 200);
        set_size(&mut idx, frn(104), 2 * 1024 * 1024);
        idx
    }

    /// An orphan: a real file whose parent was never seen, so the parent is a
    /// placeholder under lost+found and the file has no presentable path.
    /// Reachable whenever a genuine gap exists (journal wrap, missed parent
    /// record, FRN sequence mismatch), and densest during bootstrap and rescan,
    /// which are exactly the windows the server still queries.
    fn index_with_orphans() -> VolumeIndex {
        let mut idx = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
        let root = NTFS_ROOT_FRN;
        // Two presentable hits.
        idx.insert_enum(&enum_rec(frn(100), root, "real1.log", false));
        idx.insert_enum(&enum_rec(frn(101), root, "real2.log", false));
        // Three orphans: parent 999 is never inserted, so it is synthesized as a
        // placeholder under lost+found and path_of on it is not Ok.
        for (f, n) in [
            (200, "ghost1.log"),
            (201, "ghost2.log"),
            (202, "ghost3.log"),
        ] {
            idx.insert_enum(&enum_rec(frn(f), frn(999), n, false));
        }
        idx
    }

    /// `total` and the page must count the same things.
    ///
    /// The page loop drops any hit whose parent has no presentable path, but
    /// `total` was counted before that and the orphans still occupied page slots.
    /// So an unlimited query returned the 2 real hits while a limited one spent
    /// its whole page on orphans and returned NOTHING, with total claiming 5.
    /// Silent: the user sees an empty result for a file that exists.
    #[test]
    fn orphans_are_excluded_from_total_and_never_eat_the_page() {
        let idx = index_with_orphans();
        let parsed = parse_query("log").unwrap();

        let all = run_query(
            &idx,
            r"C:\",
            &parsed,
            None,
            SortSpec::default_for(SortKey::Name),
            0,
            None,
        );
        assert_eq!(all.hits.len(), 2, "only the presentable files are hits");
        assert_eq!(
            all.total, 2,
            "total must count what the page can actually return, not orphans"
        );

        // The failing case: a limit small enough that orphans would have filled it.
        for key in [SortKey::Name, SortKey::Path] {
            let page = run_query(
                &idx,
                r"C:\",
                &parsed,
                None,
                SortSpec::default_for(key),
                0,
                Some(3),
            );
            assert_eq!(
                page.hits.len(),
                2,
                "{key:?}: a limited query must still return the real hits"
            );
            assert_eq!(
                page.total, 2,
                "{key:?}: total must not count dropped orphans"
            );
        }
    }

    fn set_size(idx: &mut VolumeIndex, f: Frn, size: u64) {
        use crate::layout::{LayoutFile, LayoutName};
        // Reuse the existing name/parent by reading them back.
        let head = idx.head_of(f).unwrap();
        let e = idx.entry(head);
        let name = e.name().to_vec();
        let parent_frn = idx.entry(e.parent()).frn();
        idx.enrich(&LayoutFile {
            frn: f,
            attributes: 0x20,
            size: Some(size),
            mtime_ft: Some(1000),
            names: vec![LayoutName {
                parent_frn,
                name,
                name_lossy: false,
                dos_only: false,
            }],
        });
    }

    fn run(idx: &VolumeIndex, query: &str) -> Vec<String> {
        let parsed = parse_query(query).unwrap();
        let out = run_query(idx, r"C:\", &parsed, None, SortSpec::default(), 0, None);
        out.hits
            .iter()
            .map(|h| crate::wtf8::to_string_lossy(&h.path))
            .collect()
    }

    #[test]
    fn case_insensitive_substring_over_names() {
        let idx = sample_index();
        assert_eq!(run(&idx, "report"), vec!["docs\\sub\\Report.PDF"]);
        assert_eq!(run(&idx, "REPORT"), vec!["docs\\sub\\Report.PDF"]);
    }

    #[test]
    fn multi_term_and() {
        let idx = sample_index();
        // Both terms must be substrings of the same name.
        assert!(run(&idx, "notes zzz").is_empty()); // "zzz" is in no name
        assert_eq!(run(&idx, "notes txt"), vec!["docs\\notes.txt"]);
    }

    #[test]
    fn ext_filter() {
        let idx = sample_index();
        assert_eq!(run(&idx, "ext:pdf"), vec!["docs\\sub\\Report.PDF"]);
        let mut jpg = run(&idx, "ext:jpg");
        jpg.sort();
        assert_eq!(jpg, vec!["photo.JPG"]);
    }

    #[test]
    fn size_filter_excludes_unknown_and_out_of_range() {
        let idx = sample_index();
        // > 1MB → only the 2MB photo.
        assert_eq!(run(&idx, "size:>1mb"), vec!["photo.JPG"]);
        // tiny (≤10KB) → notes (200B) and report (4KB), not the 2MB photo.
        let mut tiny = run(&idx, "size:tiny");
        tiny.sort();
        assert_eq!(tiny, vec!["docs\\notes.txt", "docs\\sub\\Report.PDF"]);
    }

    /// The sample index dated: `docs` and `Report.PDF` today, `notes.txt`
    /// last month, `photo.JPG` in 1601, and `docs\sub` unknown. The clock
    /// reads 2026-11-04T10:00Z in UTC.
    fn dated_index() -> (VolumeIndex, crate::query::dates::tests::FixedClock) {
        use crate::query::dates::tests::{FixedClock, ft};
        let mut idx = sample_index();
        idx.set_stat(frn(100), 0, ft("2026-11-04T08:00:00"));
        idx.set_stat(frn(102), 4096, ft("2026-11-04T09:00:00"));
        idx.set_stat(frn(103), 200, ft("2026-10-01T12:00:00"));
        (idx, FixedClock::utc(ft("2026-11-04T10:00:00")))
    }

    /// `dm:` keeps the entries dated in its range and no entry whose date is
    /// unknown, which only `dm:unknown` finds, under every sort. A name sort
    /// over names alone counts each name's entries without reading them, so a
    /// date filter must keep the query off that path.
    #[test]
    fn date_filter_keeps_dated_entries_under_every_sort() {
        let (mut idx, clock) = dated_index();
        idx.optimize_storage(); // builds the name order the name-level path walks
        let today = ["docs", r"docs\sub\Report.PDF"];
        for (query, expected) in [
            ("dm:today", &today[..]),
            ("dm:thismonth", &today[..]),
            ("dm:last59mins", &[][..]),
            ("dm:last1hours", &[r"docs\sub\Report.PDF"][..]),
            ("dm:lastmonth", &[r"docs\notes.txt"][..]),
            ("dm:<2000", &["photo.JPG"][..]),
            ("dm:unknown", &[r"docs\sub"][..]),
            ("report dm:today", &[r"docs\sub\Report.PDF"][..]),
            ("notes dm:today", &[][..]),
            ("folder: dm:today", &["docs"][..]),
            ("dm:2026-10-01..2026-11-03", &[r"docs\notes.txt"][..]),
        ] {
            let parsed = crate::query::parse_query_at(query, false, &clock).unwrap();
            for key in [SortKey::Name, SortKey::Path, SortKey::DateModified] {
                let out = run_query(
                    &idx,
                    r"C:\",
                    &parsed,
                    None,
                    SortSpec::default_for(key),
                    0,
                    None,
                );
                let mut paths: Vec<String> = out
                    .hits
                    .iter()
                    .map(|h| crate::wtf8::to_string_lossy(&h.path))
                    .collect();
                paths.sort();
                assert_eq!(paths, expected, "{query} {key:?}");
                assert_eq!(out.total, expected.len() as u64, "{query} {key:?}");
            }
        }
    }

    #[test]
    fn kind_filters() {
        let idx = sample_index();
        let mut folders = run(&idx, "folder:");
        folders.sort();
        assert_eq!(folders, vec!["docs", "docs\\sub"]);
    }

    #[test]
    fn wildcard_whole_name() {
        let idx = sample_index();
        assert_eq!(run(&idx, "*.pdf"), vec!["docs\\sub\\Report.PDF"]);
        assert!(run(&idx, "*.zip").is_empty());
    }

    #[test]
    fn path_term_matches_full_path() {
        let idx = sample_index();
        let mut hits = run(&idx, "docs\\sub");
        hits.sort();
        // Every entry whose full path contains "docs\sub".
        assert_eq!(hits, vec!["docs\\sub", "docs\\sub\\Report.PDF"]);
    }

    /// Sorted paths of the hits of `query` on `idx` mounted at `prefix`.
    fn hits_under(idx: &VolumeIndex, prefix: &str, parsed: &ParsedQuery) -> Vec<String> {
        let out = run_query(idx, prefix, parsed, None, SortSpec::default(), 0, None);
        let mut paths: Vec<String> = out
            .hits
            .iter()
            .map(|h| crate::wtf8::to_string_lossy(&h.path))
            .collect();
        paths.sort();
        paths
    }

    /// Path terms match the full path a user sees, drive included, so a typed
    /// `C:\docs\sub` or a rooted `\docs\sub` finds what is below it.
    #[test]
    fn path_terms_match_the_drive_and_a_leading_separator() {
        let idx = sample_index();
        let hits = |prefix, query| hits_under(&idx, prefix, &parse_query(query).unwrap());
        assert_eq!(
            hits(r"C:\", r"C:\docs\sub report"),
            [r"docs\sub\Report.PDF"]
        );
        assert_eq!(
            hits(r"C:\", r"\docs\sub"),
            [r"docs\sub", r"docs\sub\Report.PDF"]
        );
        assert_eq!(hits(r"C:\", "c:/docs/notes"), [r"docs\notes.txt"]);
        assert_eq!(hits(r"C:\mnt\data\", r"C:\mnt\data\photo"), ["photo.JPG"]);
        assert!(hits(r"D:\", r"C:\docs").is_empty(), "another drive's path");
    }

    /// A wildcard with a separator matches the whole full path; `*` crosses
    /// folders, and the drive in front still has to agree.
    #[test]
    fn path_wildcards_match_the_whole_full_path() {
        let idx = sample_index();
        let hits = |prefix, query| hits_under(&idx, prefix, &parse_query(query).unwrap());
        assert_eq!(hits(r"C:\", r"*\sub\*.pdf"), [r"docs\sub\Report.PDF"]);
        assert_eq!(hits(r"C:\", r"C:\docs\*.txt"), [r"docs\notes.txt"]);
        assert_eq!(hits(r"C:\", "path:*.jpg"), ["photo.JPG"]);
        assert!(hits(r"D:\", r"C:\docs\*.txt").is_empty());
    }

    /// Windows refuses `:` in a name, so a term's colon is the drive's, and a
    /// volume whose prefix disagrees with the term is skipped unread.
    #[test]
    fn a_term_with_a_colon_must_line_up_with_the_drive() {
        assert!(term_admits(br"c:\docs", br"c:\"));
        assert!(term_admits(br":\docs", br"c:\"));
        assert!(term_admits(br"c:\mnt", br"c:\mnt\data\"));
        assert!(term_admits(b"docs", br"c:\"));
        assert!(!term_admits(br"d:\docs", br"c:\"));
        assert!(
            !term_admits(br"ab:\docs", br"c:\"),
            "a colon past the drive's"
        );
        assert!(
            !term_admits(br"c:\docs", br"\\?\volume{1}\"),
            "no drive at all"
        );
    }

    /// `-case` reaches the matcher: the parser's default decides how names are
    /// compared, not only how the scope folder is looked up.
    #[test]
    fn the_case_switch_reaches_the_matcher() {
        let idx = sample_index();
        let parsed = |query| crate::query::parse_query_with_case(query, true).unwrap();
        assert!(hits_under(&idx, r"C:\", &parsed("report")).is_empty());
        assert_eq!(
            hits_under(&idx, r"C:\", &parsed("Report")),
            [r"docs\sub\Report.PDF"]
        );
    }

    #[test]
    fn scope_restricts_to_descendants() {
        let idx = sample_index();
        let parsed = parse_query("").unwrap();
        let scope = resolve_scope(&idx, b"docs\\sub", false).unwrap();
        let out = run_query(
            &idx,
            r"C:\",
            &parsed,
            Some(scope),
            SortSpec::default(),
            0,
            None,
        );
        let paths: Vec<String> = out
            .hits
            .iter()
            .map(|h| crate::wtf8::to_string_lossy(&h.path))
            .collect();
        assert_eq!(paths, vec!["docs\\sub\\Report.PDF"]); // descendants only, not "docs\sub" itself
    }

    /// `page_refs` drops a scope at the root because it admits the same
    /// entries as no scope, orphans left out either way: on the subtree walk
    /// a small scope takes, and on the ancestry check per entry that a scope
    /// too large to walk takes. One fixture has a folder whose record put it
    /// under its own subfolder, a cycle cut off from the root that the
    /// orphan check alone does not see.
    #[test]
    fn a_root_scope_admits_what_no_scope_does() {
        let mut sample = sample_index();
        sample.optimize_storage();
        let orphaned = index_with_orphans();
        assert!(orphaned.has_orphans());
        let mut moved_under_itself = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
        for r in [
            enum_rec(frn(100), NTFS_ROOT_FRN, "a", true),
            enum_rec(frn(101), frn(100), "b", true),
            enum_rec(frn(102), frn(101), "xfile.txt", false),
            enum_rec(frn(103), NTFS_ROOT_FRN, "xother.txt", false),
            enum_rec(frn(100), frn(101), "a", true),
        ] {
            moved_under_itself.insert_enum(&r);
        }
        let mut admitted = 0;
        for idx in [&sample, &orphaned, &moved_under_itself] {
            let root = Some(idx.root());
            for q in [
                "",
                "log",
                "e",
                "x",
                "*.pdf",
                "ghost",
                "file:",
                "size:>100",
                r"docs\",
            ] {
                let parsed = parse_query(q).unwrap();
                let compiled = CompiledQuery::new(&parsed, r"C:\");
                let walked = |scope| {
                    let mut found: Vec<EntryIdx> = scan(idx, &parsed, &compiled, scope)
                        .iter()
                        .map(|c| c.idx)
                        .collect();
                    found.sort_unstable();
                    found
                };
                assert_eq!(walked(root), walked(None), "{q:?}");
                let admits = |e, scope| {
                    let mut folders = compiled
                        .tests_paths(&parsed)
                        .then(|| FolderPaths::new(idx, &compiled.prefix, true));
                    evaluate(
                        idx,
                        &parsed,
                        &compiled,
                        e,
                        scope,
                        &mut FxHashMap::default(),
                        &mut FxHashMap::default(),
                        &mut folders,
                        false,
                        &mut CodePointBuf::new(),
                        &mut Vec::new(),
                    )
                    .is_some()
                };
                for e in idx.live_entries().filter(|&e| !idx.is_synthetic(e)) {
                    let unscoped = admits(e, None);
                    assert_eq!(admits(e, root), unscoped, "{q:?} {e:?}");
                    admitted += usize::from(unscoped);
                }
            }
        }
        assert!(admitted > 0);
    }

    /// A folder whose record put it under its own subfolder forms a cycle
    /// cut off from the root, and nothing goes under lost+found. Its files
    /// used to count, and to take the slots of a limited page that could not
    /// list them: `goz -n 1 x` printed nothing while `xother.txt` matched.
    #[test]
    fn a_parent_cycle_is_neither_counted_nor_listed() {
        let mut idx = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
        for r in [
            enum_rec(frn(100), NTFS_ROOT_FRN, "a", true),
            enum_rec(frn(101), frn(100), "b", true),
            enum_rec(frn(102), frn(101), "xfile.txt", false),
            enum_rec(frn(103), NTFS_ROOT_FRN, "xother.txt", false),
            enum_rec(frn(100), frn(101), "a", true),
        ] {
            idx.insert_enum(&r);
        }
        idx.optimize_storage();
        assert!(!idx.has_orphans());
        let parsed = parse_query("x").unwrap();
        for key in [SortKey::Name, SortKey::Path, SortKey::Size] {
            for scope in [None, Some(idx.root())] {
                let sort = SortSpec::default_for(key);
                let page = run_query(&idx, r"C:\", &parsed, scope, sort, 0, Some(1));
                let paths: Vec<String> = page
                    .hits
                    .iter()
                    .map(|h| crate::wtf8::to_string_lossy(&h.path))
                    .collect();
                assert_eq!(paths, ["xother.txt"], "{key:?} {scope:?}");
                assert_eq!(page.total, 1, "{key:?} {scope:?}");
            }
        }
    }

    #[test]
    fn sort_path_orders_by_parent_then_name() {
        let idx = sample_index();
        let parsed = parse_query("").unwrap();
        let out = run_query(
            &idx,
            r"C:\",
            &parsed,
            None,
            SortSpec {
                key: SortKey::Path,
                dir: SortDir::Asc,
            },
            0,
            None,
        );
        let paths: Vec<String> = out
            .hits
            .iter()
            .map(|h| crate::wtf8::to_string_lossy(&h.path))
            .collect();
        // Root-level entries (empty parent) sort first, then docs\, then docs\sub\.
        assert_eq!(
            paths,
            vec![
                "docs",
                "photo.JPG",
                "docs\\notes.txt",
                "docs\\sub",
                "docs\\sub\\Report.PDF",
            ]
        );
    }

    #[test]
    fn offset_and_limit_paginate_but_total_is_full_count() {
        let idx = sample_index();
        let parsed = parse_query("").unwrap();
        let out = run_query(
            &idx,
            r"C:\",
            &parsed,
            None,
            SortSpec {
                key: SortKey::Name,
                dir: SortDir::Asc,
            },
            1,
            Some(2),
        );
        assert_eq!(out.total, 5);
        assert_eq!(out.hits.len(), 2);
    }

    /// The daemon's entry point returns the same page, in the same order, as
    /// `run_query`, under every sort key, direction and slice: references are
    /// the hits minus their paths, nothing else, and each one still resolves.
    #[test]
    fn refs_follow_run_query_order() {
        let idx = sample_index();
        let parsed = parse_query("").unwrap();
        let total =
            run_query(&idx, r"C:\", &parsed, None, SortSpec::default(), 0, None).total as u32;
        assert!(total > 2, "fixture must have several matches to slice");
        for key in [
            SortKey::Name,
            SortKey::Path,
            SortKey::Size,
            SortKey::DateModified,
        ] {
            for dir in [SortDir::Asc, SortDir::Desc] {
                let sort = SortSpec { key, dir };
                for (offset, limit) in [
                    (0, None),
                    (0, Some(1)),
                    (1, Some(total - 2)),
                    (0, Some(u32::MAX)),
                    (total, Some(5)),
                ] {
                    let hits = run_query(&idx, r"C:\", &parsed, None, sort, offset, limit);
                    let refs = run_query_refs(&idx, r"C:\", &parsed, None, sort, offset, limit);
                    assert_eq!(refs.total, hits.total);
                    let want: Vec<EntryIdx> = hits.hits.iter().map(|h| h.idx).collect();
                    let got: Vec<EntryIdx> = refs.refs.iter().map(|r| r.idx).collect();
                    assert_eq!(got, want, "{key:?}/{dir:?} offset {offset} limit {limit:?}");
                    for r in &refs.refs {
                        assert!(idx.entry_if_frn(r.idx, r.frn).is_some());
                    }
                }
            }
        }
    }

    /// The name-level path (no candidates, no sort) must page exactly like the
    /// candidate path, and both like a plain sort of the match set by
    /// `(folded name, idx)`: in both directions, for every slice, with names
    /// that joined after the order was built (unranked), names that fold
    /// alike, a repeated name, and the synthetic names kept out of totals.
    #[test]
    fn name_level_page_matches_candidate_page() {
        let mut idx = sample_index();
        idx.optimize_storage(); // builds the name order
        // Late arrivals: unranked until merged. Two of them fold alike, one
        // repeats a name that is already ranked.
        idx.apply_batch(&[
            enum_rec(frn(890), NTFS_ROOT_FRN, "late", true),
            enum_rec(frn(900), NTFS_ROOT_FRN, "Readme.MD", false),
            enum_rec(frn(901), frn(890), "README.md", false),
            enum_rec(frn(902), frn(890), "notes.txt", false),
            enum_rec(frn(903), frn(890), "a.md", false),
        ]);

        let check = |idx: &VolumeIndex, stage: &str| {
            for q in [
                "",
                "e",
                "readme",
                "*.md",
                "ext:md",
                "notes",
                "Report case:",
                "zzz",
            ] {
                let parsed = parse_query(q).unwrap();
                // Ground truth: the candidate path's match SET (ranks play no
                // part in which entries match), ordered by the bytes alone.
                let mut ground: Vec<EntryIdx> = page_refs(
                    idx,
                    r"C:\",
                    &parsed,
                    None,
                    SortSpec::default(),
                    0,
                    None,
                    false,
                )
                .1
                .iter()
                .map(|r| r.idx)
                .collect();
                ground.sort_by(|&a, &b| idx.folded_name(a).cmp(idx.folded_name(b)).then(a.cmp(&b)));
                assert!(ground.iter().all(|&e| !idx.is_synthetic(e)));
                let total = ground.len() as u32;
                for dir in [SortDir::Asc, SortDir::Desc] {
                    let sort = SortSpec {
                        key: SortKey::Name,
                        dir,
                    };
                    let mut expected_all = ground.clone();
                    if dir == SortDir::Desc {
                        expected_all.reverse();
                    }
                    for (offset, limit) in [
                        (0, None),
                        (0, Some(1)),
                        (1, Some(2)),
                        (2, None),
                        (0, Some(u32::MAX)),
                        (total, Some(5)),
                        (total.saturating_sub(1), Some(5)),
                    ] {
                        let start = (offset as usize).min(expected_all.len());
                        let end = limit.map_or(expected_all.len(), |l| {
                            (start + l as usize).min(expected_all.len())
                        });
                        let expected = &expected_all[start..end];
                        let fast = page_refs(idx, r"C:\", &parsed, None, sort, offset, limit, true);
                        let slow =
                            page_refs(idx, r"C:\", &parsed, None, sort, offset, limit, false);
                        let ctx = format!("{stage}: {q:?} {dir:?} offset {offset} limit {limit:?}");
                        assert_eq!(fast.0, u64::from(total), "total (name-level) {ctx}");
                        assert_eq!(slow.0, u64::from(total), "total (candidates) {ctx}");
                        let fast_idx: Vec<EntryIdx> = fast.1.iter().map(|r| r.idx).collect();
                        let slow_idx: Vec<EntryIdx> = slow.1.iter().map(|r| r.idx).collect();
                        assert_eq!(fast_idx, expected, "name-level page {ctx}");
                        assert_eq!(slow_idx, expected, "candidate page {ctx}");
                        for r in &fast.1 {
                            assert!(idx.entry_if_frn(r.idx, r.frn).is_some());
                        }
                    }
                }
            }
        };

        assert!(
            idx.name_order_stats().1 > 0,
            "fixture must have unranked names"
        );
        check(&idx, "with unranked names");
        idx.force_name_merge();
        assert_eq!(idx.name_order_stats().1, 0);
        check(&idx, "after the merge");
        let mut del = enum_rec(frn(901), frn(890), "", false);
        del.reason = USN_REASON_FILE_DELETE;
        idx.apply_batch(&[del]);
        check(&idx, "after a delete");
    }

    #[test]
    fn case_sensitive_query_respects_case() {
        let idx = sample_index();
        // Report.PDF has capital R; case: makes "report" not match.
        assert!(run(&idx, "report case:").is_empty());
        assert_eq!(run(&idx, "Report case:"), vec!["docs\\sub\\Report.PDF"]);
    }

    #[test]
    fn ext_filter_is_case_insensitive_even_under_case_directive() {
        // Regression: Report.PDF has an uppercase extension; ext: is always
        // case-insensitive, even when the query is otherwise case-sensitive.
        let idx = sample_index();
        assert_eq!(
            run(&idx, "Report ext:pdf case:"),
            vec!["docs\\sub\\Report.PDF"]
        );
        assert_eq!(
            run(&idx, "Report ext:PDF case:"),
            vec!["docs\\sub\\Report.PDF"]
        );
    }

    // -- oracle property test --------------------------------------------

    use proptest::prelude::*;

    /// A flat index of random-named files at the root.
    fn flat_index(names: &[String]) -> VolumeIndex {
        let mut idx = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
        for (i, n) in names.iter().enumerate() {
            idx.insert_enum(&enum_rec(frn(1000 + i as u64), NTFS_ROOT_FRN, n, false));
        }
        idx
    }

    /// Naive oracle: a file matches iff its folded name contains every folded
    /// term as a substring.
    fn oracle(names: &[String], terms: &[String]) -> BTreeSet<String> {
        names
            .iter()
            .filter(|n| {
                let fn_ = crate::fold::fold(n.as_bytes());
                terms.iter().all(|t| {
                    let ft = crate::fold::fold(t.as_bytes());
                    fn_.windows(ft.len().max(1)).any(|w| w == ft.as_slice()) || ft.is_empty()
                })
            })
            .cloned()
            .collect()
    }

    use std::collections::BTreeSet;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]

        /// The engine's name-term matching agrees with the naive oracle over
        /// random corpora and needles (mixed case, unicode letters).
        #[test]
        fn matcher_agrees_with_oracle(
            names in prop::collection::vec("[a-zA-Zçğ0-9._]{1,10}", 1..30),
            terms in prop::collection::vec("[a-zA-Zçğ]{1,4}", 0..3),
        ) {
            // Names must be unique so a path set is a faithful comparison.
            let mut uniq: Vec<String> = Vec::new();
            for (i, n) in names.iter().enumerate() {
                uniq.push(format!("{n}_{i}"));
            }
            let idx = flat_index(&uniq);
            let query = terms.join(" ");
            let parsed = parse_query(&query).unwrap();
            let out = run_query(&idx, r"C:\", &parsed, None, SortSpec::default(), 0, None);
            let engine: BTreeSet<String> = out
                .hits
                .iter()
                .map(|h| crate::wtf8::to_string_lossy(&h.path))
                .collect();
            prop_assert_eq!(engine, oracle(&uniq, &terms));
        }
    }

    /// A child of a removed folder has no full path. The name-level page
    /// trusts each name's refcount only while `may_have_unpresentable` is false, so the
    /// child used to be counted and shown under the folder's zero-filled
    /// name, and once a compaction ran its page panicked.
    #[test]
    fn child_of_a_removed_folder_is_neither_counted_nor_shown() {
        let root = NTFS_ROOT_FRN;
        let delete = |f: Frn| {
            let mut rec = enum_rec(f, root, "", false);
            rec.reason = USN_REASON_FILE_DELETE;
            rec
        };
        let mut idx = VolumeIndex::new(root, FrnMap::sparse());
        let mut recs: Vec<_> = (0..400)
            .map(|i| enum_rec(frn(1000 + i), root, &format!("{i:040}"), false))
            .collect();
        recs.push(enum_rec(frn(100), root, "docs", true));
        recs.push(enum_rec(frn(101), frn(100), "orphan.log", false));
        recs.push(enum_rec(frn(102), root, "real.log", false));
        idx.apply_batch(&recs);
        idx.optimize_storage();
        // orphan.log's own delete is missed.
        let dels: Vec<_> = (0..400)
            .map(|i| delete(frn(1000 + i)))
            .chain([delete(frn(100))])
            .collect();
        idx.apply_batch(&dels);
        let plan = idx.plan_compaction().expect("garbage");
        idx.apply_compaction(plan);

        let parsed = parse_query("log").unwrap();
        for key in [SortKey::Name, SortKey::Path] {
            let out = run_query(
                &idx,
                r"C:\",
                &parsed,
                None,
                SortSpec::default_for(key),
                0,
                Some(10),
            );
            let paths: Vec<String> = out
                .hits
                .iter()
                .map(|h| crate::wtf8::to_string_lossy(&h.path))
                .collect();
            assert_eq!(paths, ["real.log"], "{key:?}");
            assert_eq!(out.total, 1, "{key:?}");
        }
    }

    // -- full paths ------------------------------------------------------------

    /// `prefix + path_of(e)` folded iff `fold`, built the slow way: what a
    /// full path test must compare.
    fn slow_full_path(idx: &VolumeIndex, e: EntryIdx, prefix: &str, fold: bool) -> Option<Vec<u8>> {
        let mut rel = Vec::new();
        if !matches!(idx.path_of(e, &mut rel), crate::index::PathStatus::Ok) {
            return None;
        }
        let full = [prefix.as_bytes(), &rel].concat();
        Some(if fold { crate::fold::fold(&full) } else { full })
    }

    /// [`FolderPaths`] gives every entry `prefix + path_of`, folded alike, and
    /// a path exactly when `path_of` does: none in a cycle or below
    /// lost+found. Caps of a few hundred bytes and six folders make it start
    /// over again and again, and two visiting orders build the folders in
    /// different orders.
    #[test]
    fn folder_paths_agree_with_path_of() {
        let root = NTFS_ROOT_FRN;
        let mut idx = VolumeIndex::new(root, FrnMap::sparse());
        let mut recs = Vec::new();
        for a in 0..4u64 {
            recs.push(enum_rec(frn(100 + a), root, &format!("Dir{a}Ç"), true));
            for b in 0..4u64 {
                let sub = frn(200 + a * 10 + b);
                // NFD: `e` then a combining acute, which folding composes.
                recs.push(enum_rec(sub, frn(100 + a), &format!("e\u{301}{b}X"), true));
                for c in 0..3u64 {
                    recs.push(enum_rec(
                        frn(1000 + a * 100 + b * 10 + c),
                        sub,
                        &format!("F{c}.TxT"),
                        false,
                    ));
                }
            }
        }
        recs.push(enum_rec(frn(300), root, "loopA", true));
        recs.push(enum_rec(frn(301), frn(300), "loopB", true));
        recs.push(enum_rec(frn(302), frn(301), "inside.txt", false));
        recs.push(enum_rec(frn(400), frn(999), "lost.txt", false));
        for r in &recs {
            idx.insert_enum(r);
        }
        let (a, b) = (
            idx.head_of(frn(300)).unwrap(),
            idx.head_of(frn(301)).unwrap(),
        );
        idx.force_parent(a, b);

        let entries: Vec<EntryIdx> = idx
            .live_entries()
            .filter(|&e| e != idx.root() && e != idx.lost_found())
            .collect();
        let mut checked = 0;
        for prefix in [r"C:\", r"C:\Mnt\Dá\"] {
            for fold in [false, true] {
                let mount = if fold {
                    crate::fold::fold(prefix.as_bytes())
                } else {
                    prefix.as_bytes().to_vec()
                };
                for order in [entries.clone(), entries.iter().rev().copied().collect()] {
                    let mut paths = FolderPaths::new(&idx, &mount, fold);
                    paths.max_bytes = 200;
                    paths.max_folders = 6;
                    for &e in &order {
                        let got = paths.with_full_path(&idx, e, <[u8]>::to_vec);
                        let want = slow_full_path(&idx, e, prefix, fold);
                        checked += usize::from(want.is_some());
                        assert_eq!(got, want, "{prefix} fold {fold} entry {e}");
                        assert!(paths.arena.capacity() <= paths.max_bytes);
                        assert!(paths.known.len() <= paths.max_folders);
                    }
                }
            }
        }
        // 4 folders, 16 below them and 48 files, in 8 passes.
        assert_eq!(checked, 8 * 68);
    }

    /// The arena's allocation, not only its length, stays within its cap:
    /// doubling from a first path of 36 bytes passes 1000 at 1152.
    #[test]
    fn the_folder_arena_allocates_no_more_than_its_cap() {
        let root = NTFS_ROOT_FRN;
        let mut idx = VolumeIndex::new(root, FrnMap::sparse());
        for i in 0..60u64 {
            idx.insert_enum(&enum_rec(frn(100 + i), root, &format!("{i:036}"), true));
            idx.insert_enum(&enum_rec(frn(1000 + i), frn(100 + i), "f.txt", false));
        }
        let mut paths = FolderPaths::new(&idx, br"c:\", true);
        paths.max_bytes = 1000;
        for i in 0..60 {
            let file = idx.head_of(frn(1000 + i)).unwrap();
            assert!(paths.with_full_path(&idx, file, <[u8]>::to_vec).is_some());
            assert!(
                paths.arena.capacity() <= paths.max_bytes,
                "capacity {}",
                paths.arena.capacity()
            );
        }
    }

    /// The folder table never doubles past 2^19 slots of 21 bytes, which is
    /// what [`FOLDERS_MAX`] promises.
    #[test]
    fn the_folder_table_stays_at_its_size() {
        let mut known: FxHashMap<EntryIdx, Option<Folder>> = FxHashMap::default();
        for i in 0..FOLDERS_MAX as EntryIdx {
            known.insert(i, None);
        }
        assert_eq!(known.capacity(), FOLDERS_MAX);
        assert_eq!(size_of::<(EntryIdx, Option<Folder>)>() + 1, 21);
    }

    /// An entry has a path when its walk to the root visits at most
    /// `MAX_PATH_HOPS` entries, itself and the root included, as `path_of`
    /// has it; one entry deeper has none, and neither does anything below.
    #[test]
    fn folder_paths_stop_at_the_depth_path_of_stops_at() {
        let root = NTFS_ROOT_FRN;
        let mut idx = VolumeIndex::new(root, FrnMap::sparse());
        let mut parent = root;
        let mut recs = Vec::new();
        for i in 0..MAX_PATH_HOPS as u64 + 2 {
            let f = frn(10_000 + i);
            recs.push(enum_rec(f, parent, "d", true));
            recs.push(enum_rec(frn(20_000 + i), f, "f", false));
            parent = f;
        }
        idx.apply_batch(&recs);
        let mut paths = FolderPaths::new(&idx, br"C:\", false);
        let mut with_path = 0;
        for e in idx
            .live_entries()
            .filter(|&e| e != idx.root() && e != idx.lost_found())
        {
            let got = paths.with_full_path(&idx, e, <[u8]>::to_vec);
            let want = slow_full_path(&idx, e, r"C:\", false);
            with_path += usize::from(want.is_some());
            assert_eq!(got, want, "entry {e}");
        }
        // Folders at depths 2 to MAX_PATH_HOPS, and a file below each of the
        // folders but the deepest of those.
        assert_eq!(with_path, 2 * (MAX_PATH_HOPS - 1) - 1);
    }

    /// The walk visits each entry at or below a name holding the piece once:
    /// a holder below another holder is walked with it, a holder below
    /// lost+found is not walked at all, and past its budget the walk gives up.
    #[test]
    fn the_path_walk_covers_each_holder_subtree_once() {
        let root = NTFS_ROOT_FRN;
        let mut idx = VolumeIndex::new(root, FrnMap::sparse());
        for r in [
            enum_rec(frn(100), root, "Sys32", true),
            enum_rec(frn(101), frn(100), "a.dll", false),
            enum_rec(frn(102), frn(100), "sys32", true),
            enum_rec(frn(103), frn(102), "b.dll", false),
            enum_rec(frn(104), root, "other", true),
            enum_rec(frn(105), frn(104), "c.dll", false),
            enum_rec(frn(106), frn(104), "my_SYS32.txt", false),
            enum_rec(frn(107), frn(999), "sys32.log", false),
        ] {
            idx.insert_enum(&r);
        }
        let e = |f: u64| idx.head_of(frn(f)).unwrap();
        let mut seen = Vec::new();
        assert!(walk_below_names(&idx, b"sys32", 100, |x| seen.push(x)));
        seen.sort_unstable();
        let mut want = vec![e(100), e(101), e(102), e(103), e(106)];
        want.sort_unstable();
        assert_eq!(seen, want);
        assert!(
            !walk_below_names(&idx, b"sys32", 4, |_| {}),
            "five to visit"
        );
        assert!(
            !walk_below_names(&idx, b"sys32", 3, |_| panic!("walked")),
            "four names hold the piece"
        );
    }

    /// The walk is driven by the longest piece of path text between
    /// separators that the mount prefix does not hold, the later on a tie.
    #[test]
    fn the_path_walk_takes_the_longest_piece_the_prefix_lacks() {
        let piece = |query: &str, prefix: &str| {
            let parsed = parse_query(query).unwrap();
            CompiledQuery::new(&parsed, prefix)
                .path_piece
                .map(|p| String::from_utf8(p).unwrap())
        };
        assert_eq!(
            piece(r"windows\system32", r"C:\").as_deref(),
            Some("system32")
        );
        assert_eq!(piece(r"C:\Windows\", r"C:\").as_deref(), Some("windows"));
        assert_eq!(
            piece(r"abc\xyz", r"C:\").as_deref(),
            Some("xyz"),
            "the later on a tie"
        );
        assert_eq!(piece(r"*\bin\*.dll", r"C:\").as_deref(), Some(".dll"));
        assert_eq!(piece("path:sys report", r"C:\").as_deref(), Some("sys"));
        assert_eq!(
            piece(r"data\x", r"C:\mnt\data\").as_deref(),
            Some("x"),
            "the prefix holds data"
        );
        assert_eq!(piece(r"C:\", r"C:\"), None);
        assert_eq!(
            piece(r"C:a\b", "C:"),
            None,
            "C:a spans the prefix and a name"
        );
        assert_eq!(piece(r"Windows\ case:", r"C:\"), None, "case-sensitive");
        assert_eq!(piece("report", r"C:\"), None);
    }

    /// A path term the mount prefix holds is in every full path on the
    /// volume, so `C:\ report` is decided by names alone, as `report` is, and
    /// runs per distinct name. Another drive's term still empties the volume,
    /// and under `case:` a lowercase drive is not in `C:\`.
    #[test]
    fn a_path_term_the_prefix_holds_leaves_the_query_to_names() {
        let mut idx = sample_index();
        idx.optimize_storage();
        let report = parse_query("report").unwrap();
        for query in [r"C:\ report", r"\ report", "path:c: report"] {
            let parsed = parse_query(query).unwrap();
            let compiled = CompiledQuery::new(&parsed, r"C:\");
            assert!(is_name_level(&idx, &parsed, &compiled, None), "{query}");
            assert_eq!(
                hits_under(&idx, r"C:\", &parsed),
                hits_under(&idx, r"C:\", &report),
                "{query}"
            );
        }
        let other_drive = parse_query(r"C:\ report").unwrap();
        assert!(!is_name_level(
            &idx,
            &other_drive,
            &CompiledQuery::new(&other_drive, r"D:\"),
            None
        ));
        assert!(hits_under(&idx, r"D:\", &other_drive).is_empty());
        assert!(hits_under(&idx, r"C:\", &parse_query(r"c:\ Report case:").unwrap()).is_empty());
        assert_eq!(
            hits_under(&idx, r"C:\", &parse_query(r"C:\ Report case:").unwrap()),
            [r"docs\sub\Report.PDF"]
        );
    }

    /// Builds a tree from `nodes` (a parent among the folders so far, a name,
    /// whether it is a folder) and puts `orphans` below a folder that never
    /// arrives. `padded` adds files named by digits at the root, which no
    /// query below holds, so that a path walk over the tree stays within its
    /// budget; without them, a walk may give up part way. Returns the index
    /// and each node's entry.
    fn random_tree(
        nodes: &[(prop::sample::Index, String, bool)],
        orphans: &[String],
        padded: bool,
    ) -> (VolumeIndex, Vec<EntryIdx>) {
        let root = NTFS_ROOT_FRN;
        let mut idx = VolumeIndex::new(root, FrnMap::sparse());
        let mut dirs = vec![root];
        let mut frns = Vec::new();
        for (i, (parent, name, is_dir)) in nodes.iter().enumerate() {
            let f = frn(1000 + i as u64);
            idx.insert_enum(&enum_rec(f, *parent.get(&dirs), name, *is_dir));
            if *is_dir {
                dirs.push(f);
            }
            frns.push(f);
        }
        for (i, name) in orphans.iter().enumerate() {
            idx.insert_enum(&enum_rec(frn(5000 + i as u64), frn(9999), name, false));
        }
        let padding = if padded {
            4 * (nodes.len() + orphans.len() + 8)
        } else {
            0
        };
        for i in 0..padding as u64 {
            idx.insert_enum(&enum_rec(frn(10_000 + i), root, &i.to_string(), false));
        }
        let entries = frns.iter().map(|&f| idx.head_of(f).unwrap()).collect();
        (idx, entries)
    }

    /// `chars` with the case of each letter whose bit in `flip` is set swapped.
    fn flip_case(chars: &[char], flip: u64) -> String {
        chars
            .iter()
            .enumerate()
            .map(|(k, &c)| match (flip >> (k % 64)) & 1 {
                1 if c.is_lowercase() => c.to_uppercase().next().unwrap_or(c),
                1 => c.to_lowercase().next().unwrap_or(c),
                _ => c,
            })
            .collect()
    }

    fn is_below(idx: &VolumeIndex, e: EntryIdx, folder: EntryIdx) -> bool {
        let mut cur = idx.entry(e).parent();
        while cur != NIL {
            if cur == folder {
                return true;
            }
            cur = idx.entry(cur).parent();
        }
        false
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        /// Path terms and path wildcards over random trees agree with an
        /// oracle that builds `prefix + path_of` for every entry and searches
        /// it naively, whichever way the scan runs: driven by a name term,
        /// walking below the names that hold a piece of the path, scanning
        /// every entry (case-sensitive, or after a walk that gave up), walking
        /// a scope, or per distinct name when the prefix holds every path
        /// term. The terms are cut from a real full path with their case
        /// scrambled, so they match. A prefix without a trailing separator,
        /// which the daemon never passes, and names holding one, which NTFS
        /// never stores, are covered too.
        #[test]
        fn path_queries_agree_with_a_full_path_oracle(
            nodes in prop::collection::vec(
                (any::<prop::sample::Index>(), "[aAbBçÇ_\u{301}]{1,3}|[ab]\\\\[ab]", any::<bool>()),
                1..30,
            ),
            orphans in prop::collection::vec("[aAbBçÇ]{1,3}", 0..3),
            pick in any::<prop::sample::Index>(),
            cuts in prop::collection::vec(any::<prop::sample::Index>(), 6),
            shape in 0u8..6,
            flip in any::<u64>(),
            match_case in any::<bool>(),
            mount in 0usize..3,
            scope_pick in proptest::option::of(any::<prop::sample::Index>()),
            padded in any::<bool>(),
        ) {
            let (idx, entries) = random_tree(&nodes, &orphans, padded);
            let prefix = [r"C:\", r"C:\Mnt\aB\", "C:"][mount];
            let target = *pick.get(&entries);
            let full: Vec<char> = String::from_utf8(slow_full_path(&idx, target, prefix, false).unwrap())
                .unwrap()
                .chars()
                .collect();
            let name: Vec<char> = String::from_utf8(idx.entry(target).name().to_vec())
                .unwrap()
                .chars()
                .collect();
            let span = |a: usize, b: usize, len: usize| {
                let (x, y) = (cuts[a].index(len + 1), cuts[b].index(len + 1));
                (x.min(y), x.max(y))
            };
            let path_term = |(i, j): (usize, usize)| {
                let t = flip_case(&full[i..j], flip);
                if t.contains('\\') { t } else { format!("path:{t}") }
            };
            let wildcard = |(i, j): (usize, usize)| {
                format!("path:{}*{}", flip_case(&full[..i], flip), flip_case(&full[j..], flip >> 5))
            };
            let (m, n) = span(4, 5, name.len());
            let name_term = flip_case(&name[m..n], flip >> 11);
            let mut terms = match shape {
                0 => vec![path_term(span(0, 1, full.len()))],
                1 => vec![path_term(span(0, 1, full.len())), name_term],
                2 => vec![path_term(span(0, 1, full.len())), path_term(span(2, 3, full.len()))],
                3 => vec![wildcard(span(0, 1, full.len()))],
                4 => vec![wildcard(span(0, 1, full.len())), name_term],
                _ => vec![path_term(span(0, 1, full.len())), wildcard(span(2, 3, full.len()))],
            };
            if match_case {
                terms.push("case:".into());
            }
            let query = terms.join(" ");
            let parsed = parse_query(&query).unwrap();
            let folders: Vec<EntryIdx> = entries.iter().copied().filter(|&e| idx.entry(e).is_dir()).collect();
            let scope = scope_pick.filter(|_| !folders.is_empty()).map(|p| *p.get(&folders));

            let fold = |b: &[u8]| if parsed.match_case { b.to_vec() } else { crate::fold::fold(b) };
            let holds = |hay: &[u8], t: &[u8]| hay.windows(t.len().max(1)).any(|w| w == t);
            let mut want: Vec<String> = idx
                .live_entries()
                .filter(|&e| !idx.is_synthetic(e))
                .filter(|&e| {
                    let Some(full) = slow_full_path(&idx, e, prefix, !parsed.match_case) else {
                        return false;
                    };
                    let name = fold(idx.entry(e).name());
                    scope.is_none_or(|s| is_below(&idx, e, s))
                        && parsed.name_terms.iter().all(|t| holds(&name, t))
                        && parsed.path_terms.iter().all(|t| holds(&full, t))
                        && parsed.path_wildcards.iter().all(|w| w.matches(&full))
                })
                .map(|e| {
                    let full = slow_full_path(&idx, e, "", false).unwrap();
                    crate::wtf8::to_string_lossy(&full)
                })
                .collect();
            want.sort();
            let out = run_query(&idx, prefix, &parsed, scope, SortSpec::default(), 0, None);
            let mut got: Vec<String> = out
                .hits
                .iter()
                .map(|h| crate::wtf8::to_string_lossy(&h.path))
                .collect();
            got.sort();
            prop_assert_eq!(out.total as usize, got.len());
            prop_assert_eq!(got, want, "query {:?} under {}", query, prefix);
        }
    }

    // -- path order ---------------------------------------------------------

    /// How a path sort keyed candidates before folder ranks: every distinct
    /// parent's folded path built and sorted, equal paths sharing a rank, a
    /// parent with no full path last. The reference [`path_keys`] must match.
    fn path_keys_by_built_paths(index: &VolumeIndex, candidates: &mut [Candidate]) {
        let mut memo = FxHashMap::default();
        let mut folded: FxHashMap<EntryIdx, Option<Vec<u8>>> = FxHashMap::default();
        for c in candidates.iter() {
            let parent = index.entry(c.idx).parent();
            folded
                .entry(parent)
                .or_insert_with(|| parent_path(index, parent, &mut memo).as_deref().map(fold));
        }
        let mut dirs: Vec<(EntryIdx, Option<Vec<u8>>)> = folded.into_iter().collect();
        dirs.sort_unstable_by(|a, b| {
            a.1.is_none()
                .cmp(&b.1.is_none())
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| a.0.cmp(&b.0))
        });
        let mut ranks: FxHashMap<EntryIdx, u32> = FxHashMap::default();
        let mut rank = 0u32;
        for (i, (dir, path)) in dirs.iter().enumerate() {
            if i > 0 && dirs[i - 1].1 != *path {
                rank += 1;
            }
            ranks.insert(*dir, if path.is_some() { rank } else { u32::MAX });
        }
        for c in candidates.iter_mut() {
            c.key = u64::from(ranks[&index.entry(c.idx).parent()]);
        }
    }

    /// The first difference between the path-sorted pages `page_refs` returns
    /// and the ones the built-path keys give, over both directions and a few
    /// slices, or `None` when every page agrees.
    fn path_pages_differ(index: &VolumeIndex, parsed: &ParsedQuery) -> Option<String> {
        let compiled = CompiledQuery::new(parsed, r"C:\");
        for dir in [SortDir::Asc, SortDir::Desc] {
            let sort = SortSpec {
                key: SortKey::Path,
                dir,
            };
            for (offset, limit) in [(0, None), (0, Some(1)), (1, Some(3)), (2, None)] {
                let mut old = scan(index, parsed, &compiled, None);
                let (start, end) = page_bounds(old.len(), offset, limit);
                path_keys_by_built_paths(index, &mut old);
                order_top(index, &mut old, sort, end);
                let old: Vec<EntryIdx> = old[start..end].iter().map(|c| c.idx).collect();
                let (_, refs) = page_refs(index, r"C:\", parsed, None, sort, offset, limit, true);
                let new: Vec<EntryIdx> = refs.iter().map(|r| r.idx).collect();
                if new != old {
                    return Some(format!(
                        "{dir:?} offset {offset} limit {limit:?}: {new:?} != {old:?}"
                    ));
                }
            }
        }
        None
    }

    /// Path-sorted hit paths of `query`.
    fn path_sorted(idx: &VolumeIndex, query: &str) -> Vec<String> {
        let parsed = parse_query(query).unwrap();
        let sort = SortSpec::default_for(SortKey::Path);
        let out = run_query(idx, r"C:\", &parsed, None, sort, 0, None);
        out.hits
            .iter()
            .map(|h| crate::wtf8::to_string_lossy(&h.path))
            .collect()
    }

    /// A space and `[` sort below `\` and `_` and `~` above it, so a folder's
    /// own subfolders land between its siblings: a walk that descends right
    /// after each folder puts `a\x` second.
    #[test]
    fn path_sort_places_a_subtree_after_siblings_that_extend_its_name() {
        let root = NTFS_ROOT_FRN;
        let mut idx = VolumeIndex::new(root, FrnMap::sparse());
        for (i, dir) in ["a~", "a_b", "a", "a[", "a b"].iter().enumerate() {
            let d = frn(100 + i as u64);
            idx.insert_enum(&enum_rec(d, root, dir, true));
            idx.insert_enum(&enum_rec(frn(200 + i as u64), d, "f", false));
        }
        idx.insert_enum(&enum_rec(frn(300), frn(102), "x", true));
        idx.insert_enum(&enum_rec(frn(301), frn(300), "f", false));
        let expected = [r"a\f", r"a b\f", r"a[\f", r"a\x\f", r"a_b\f", r"a~\f"];
        assert_eq!(path_sorted(&idx, "f"), expected, "no name order");
        idx.optimize_storage();
        assert_eq!(path_sorted(&idx, "f"), expected, "with the name order");
        assert_eq!(path_pages_differ(&idx, &parse_query("").unwrap()), None);
    }

    /// Folders whose names fold alike (`B` and `b` in a case-sensitive
    /// folder) have one folded path, so their files interleave by name and
    /// their subfolders merge into one order.
    #[test]
    fn path_sort_merges_folders_whose_names_fold_alike() {
        let root = NTFS_ROOT_FRN;
        let mut idx = VolumeIndex::new(root, FrnMap::sparse());
        for r in [
            enum_rec(frn(100), root, "B", true),
            enum_rec(frn(101), root, "b", true),
            enum_rec(frn(102), frn(100), "y", true),
            enum_rec(frn(103), frn(101), "x", true),
            enum_rec(frn(104), frn(100), "m.txt", false),
            enum_rec(frn(105), frn(101), "k.txt", false),
            enum_rec(frn(106), frn(100), "z.txt", false),
            enum_rec(frn(107), frn(102), "f.txt", false),
            enum_rec(frn(108), frn(103), "f.txt", false),
        ] {
            idx.insert_enum(&r);
        }
        idx.optimize_storage();
        assert_eq!(
            path_sorted(&idx, "txt"),
            [
                r"b\k.txt",
                r"B\m.txt",
                r"B\z.txt",
                r"b\x\f.txt",
                r"B\y\f.txt"
            ]
        );
        assert_eq!(path_pages_differ(&idx, &parse_query("").unwrap()), None);
    }

    /// Past the depth where `path_of` gives up a folder has no full path, so
    /// its files rank after every other, `b\f.txt` included, as they did when
    /// paths were built. A chain of `a` folders holds `f.txt` at every depth;
    /// counting the root as one, the folder at depth `MAX_PATH_HOPS` is the
    /// last with a path.
    #[test]
    fn path_sort_ranks_folders_past_the_hop_cap_last() {
        use crate::index::PathStatus;
        use crate::index::volume::MAX_PATH_HOPS;
        let root = NTFS_ROOT_FRN;
        let mut idx = VolumeIndex::new(root, FrnMap::sparse());
        idx.insert_enum(&enum_rec(frn(100), root, "b", true));
        idx.insert_enum(&enum_rec(frn(101), frn(100), "f.txt", false));
        let chain = MAX_PATH_HOPS as u64 + 3;
        let mut parent = root;
        for i in 0..chain {
            let d = frn(10_000 + i);
            idx.insert_enum(&enum_rec(d, parent, "a", true));
            idx.insert_enum(&enum_rec(frn(20_000 + i), d, "f.txt", false));
            parent = d;
        }
        idx.optimize_storage();
        assert!(!idx.has_orphans());
        let file = |f| idx.head_of(frn(f)).unwrap();
        let status = |e: EntryIdx| idx.path_of(idx.entry(e).parent(), &mut Vec::new());
        // The chain's files with a path by depth, then `b`'s, then the rest.
        let with_path = MAX_PATH_HOPS as u64 - 1;
        let mut want: Vec<EntryIdx> = (0..with_path).map(|i| file(20_000 + i)).collect();
        want.push(file(101));
        want.extend((with_path..chain).map(|i| file(20_000 + i)));
        assert_eq!(status(file(20_000 + with_path - 1)), PathStatus::Ok);
        assert_eq!(status(file(20_000 + with_path)), PathStatus::CycleDetected);
        let parsed = parse_query("f.txt").unwrap();
        for dir in [SortDir::Asc, SortDir::Desc] {
            let sort = SortSpec {
                key: SortKey::Path,
                dir,
            };
            let (_, refs) = page_refs(&idx, r"C:\", &parsed, None, sort, 0, None, true);
            let got: Vec<EntryIdx> = refs.iter().map(|r| r.idx).collect();
            assert_eq!(got, want, "{dir:?}");
            want.reverse();
        }
    }

    /// Name parts that fold alike (`A` and `a`, NFC and NFD `é`), sort below
    /// `\` (space, `[`, `.`, `0`) or above it (`_`, `~`, letters), and repeat.
    const PARTS: &[&str] = &[
        "a", "A", "b", " ", "[", "_", "~", ".", "0", "é", "e\u{301}", "É",
    ];

    /// A volume from `entries`: kind 0..=3 is a folder, 4..=8 a file, 9 a file
    /// whose folder is unknown (an orphan). The name order is built after
    /// `order_at` entries, at the end, or never, so names join it late or not
    /// at all; `cycle` reparents one folder, under its own subtree when the
    /// pick lands there.
    fn random_volume(
        entries: &[(u8, prop::sample::Index, Vec<usize>)],
        order_at: prop::sample::Index,
        cycle: Option<(prop::sample::Index, prop::sample::Index)>,
    ) -> VolumeIndex {
        let mut idx = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
        let mut dirs = vec![NTFS_ROOT_FRN];
        let order_at = order_at.index(entries.len() + 2);
        for (i, (kind, parent, parts)) in entries.iter().enumerate() {
            if i == order_at {
                idx.optimize_storage();
            }
            let name: String = parts.iter().map(|&p| PARTS[p]).collect();
            let f = frn(10_000 + i as u64);
            let parent = match kind {
                9 => frn(99_999),
                _ => dirs[parent.index(dirs.len())],
            };
            idx.insert_enum(&enum_rec(f, parent, &name, *kind < 4));
            if *kind < 4 {
                dirs.push(f);
            }
        }
        if order_at == entries.len() {
            idx.optimize_storage();
        }
        if let Some((a, b)) = cycle
            && dirs.len() > 1
        {
            let pick = |i: prop::sample::Index| idx.head_of(dirs[1 + i.index(dirs.len() - 1)]);
            let (a, b) = (pick(a).unwrap(), pick(b).unwrap());
            idx.force_parent(a, b);
        }
        idx
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// Folder ranks order a path sort exactly as building and sorting
        /// every parent's folded path did, on random trees whose names fold
        /// together, extend one another across `\`, join the name order late,
        /// or sit under lost+found or in a cycle.
        #[test]
        fn path_order_matches_sorting_built_paths(
            entries in prop::collection::vec(
                (0u8..10, any::<prop::sample::Index>(), prop::collection::vec(0..PARTS.len(), 1..4)),
                1..60,
            ),
            order_at in any::<prop::sample::Index>(),
            cycle in prop::option::weighted(0.2, (any::<prop::sample::Index>(), any::<prop::sample::Index>())),
            // Path terms and a path wildcard run the per-folder path test too.
            query in prop::sample::select(vec!["", "a", "é", "A case:", r"a\", r"\b", r"*a\*"]),
        ) {
            let idx = random_volume(&entries, order_at, cycle);
            let parsed = parse_query(query).unwrap();
            prop_assert_eq!(path_pages_differ(&idx, &parsed), None);
        }
    }
}
