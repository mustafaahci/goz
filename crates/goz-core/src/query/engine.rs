//! The query executor: compile a [`ParsedQuery`] into finders, scan the
//! index, apply scope and sort, and return a page of hits.
//!
//! The scan matches each candidate against the index's precomputed folded name
//! (case-insensitive queries) and tests the cheapest predicates first: kind and
//! size, then name substrings and wildcards, then ext, then (only for
//! survivors) scope ancestry and the reconstructed full path. Path reconstruction, the one expensive step, is
//! deferred until a candidate has passed everything else.

use super::parse::{CodePointBuf, Filters, Kind, ParsedQuery, SizeRange};
use crate::index::VolumeIndex;
use crate::types::{EntryIdx, Frn, SortDir, SortKey, SortSpec};
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
    path_finders: Vec<memmem::Finder<'a>>,
    /// The longest literal substring guaranteed present in any match, drawn
    /// from the name terms, from wildcard literals (e.g. `.dll` of `*.dll`), and
    /// from a single `ext:` value (`.ext`). Drives the folded-arena prefilter
    /// scan. `None` when nothing offers a usable literal (then the query
    /// iterates every entry).
    prefilter: Option<Vec<u8>>,
}

impl<'a> CompiledQuery<'a> {
    pub fn new(parsed: &'a ParsedQuery) -> Self {
        // The prefilter is the longest literal from any name term, wildcard, or
        // single `ext:` value;
        // every match must contain it, so scanning for it yields a superset of
        // matches that `evaluate` then verifies.
        // Borrow name terms and move owned wildcard/ext literals, materializing
        // exactly one owned Vec at the end instead of cloning every name term
        // and discarding all but the longest.
        let mut prefilter: Option<Cow<'a, [u8]>> = None;
        let mut consider = |cand: Cow<'a, [u8]>| {
            if cand.len() > prefilter.as_ref().map_or(0, |c| c.len()) {
                prefilter = Some(cand);
            }
        };
        for term in &parsed.name_terms {
            consider(Cow::Borrowed(term.as_slice()));
        }
        for w in &parsed.wildcards {
            if let Some(lit) = w.longest_literal() {
                consider(Cow::Owned(lit));
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
            consider(Cow::Owned(lit));
        }

        Self {
            name_finders: parsed
                .name_terms
                .iter()
                .map(|t| memmem::Finder::new(t.as_slice()))
                .collect(),
            path_finders: parsed
                .path_terms
                .iter()
                .map(|t| memmem::Finder::new(t.as_slice()))
                .collect(),
            prefilter: prefilter.map(Cow::into_owned),
        }
    }
}

/// A matched entry before its page is built: 16 bytes, so a multi-million
/// match set costs tens of MB rather than hundreds. Everything else (name,
/// parent, kind, size, date) is read from the index when a comparison or the
/// page needs it.
struct Candidate {
    idx: EntryIdx,
    /// The key the page's sort asks for, filled by [`order`]: the parent
    /// directory's rank for a path sort (see [`rank_parents`]), the size for a
    /// size sort, the order-preserving image of the mtime for a date sort,
    /// unused for a name sort. Candidates used to carry kind, size, date, an
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
    parsed: &ParsedQuery,
    scope: Option<EntryIdx>,
    sort: SortSpec,
    offset: u32,
    limit: Option<u32>,
) -> QueryOutcome {
    let Scan {
        mut candidates,
        mut dir_paths,
    } = scan(index, parsed, scope);
    let total = candidates.len() as u64;
    let (start, end) = page_bounds(candidates.len(), offset, limit);
    order(index, &mut candidates, &mut dir_paths, sort, end);
    let hits = build_hits(index, &candidates[start..end], &mut dir_paths);
    QueryOutcome { total, hits }
}

/// Like [`run_query`], but the page comes back as 16-byte [`HitRef`]s and no
/// path is built: the daemon's entry point.
///
/// The ordering happens here, under the caller's index read lock, because the
/// comparators read folded names straight from the index and allocate
/// nothing: a 3M-row set orders in ~160 ms across cores. Building every
/// row's path under that lock, as the page builder of [`run_query`] does, held
/// it ~1 s for the same set and peaked at ~1 GB; the daemon now builds paths
/// per page, from the live index, when it encodes them.
pub fn run_query_refs(
    index: &VolumeIndex,
    parsed: &ParsedQuery,
    scope: Option<EntryIdx>,
    sort: SortSpec,
    offset: u32,
    limit: Option<u32>,
) -> RefOutcome {
    let Scan {
        mut candidates,
        mut dir_paths,
    } = scan(index, parsed, scope);
    let total = candidates.len() as u64;
    let (start, end) = page_bounds(candidates.len(), offset, limit);
    order(index, &mut candidates, &mut dir_paths, sort, end);
    let refs = candidates[start..end]
        .iter()
        .map(|c| HitRef {
            idx: c.idx,
            frn: index.entry(c.idx).frn(),
        })
        .collect();
    RefOutcome { total, refs }
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

/// The match set of a query: every candidate that passed all predicates, in
/// scan order, plus the parent-path memo the scan built. Ordering and page
/// building share the memo so no parent chain is walked twice.
struct Scan {
    candidates: Vec<Candidate>,
    dir_paths: FxHashMap<EntryIdx, Option<Vec<u8>>>,
}

/// Collects every match of `parsed` under `scope`.
fn scan(index: &VolumeIndex, parsed: &ParsedQuery, scope: Option<EntryIdx>) -> Scan {
    let compiled = CompiledQuery::new(parsed);
    let has_path_terms = !parsed.path_terms.is_empty();
    let mut scope_memo: FxHashMap<EntryIdx, bool> = FxHashMap::default();
    // Each parent directory's reconstructed path, or None when it has none (its
    // ancestry ends in lost+found, or hit the hop cap). ONE memo, shared by the
    // candidate filter and the page builder: both need exactly this walk, and
    // doing it twice cost a rare query ~13% for nothing.
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
                &compiled,
                idx,
                None,
                &mut scope_memo,
                &mut dir_paths,
                has_path_terms,
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
        let driver = memmem::Finder::new(needle);
        // The haystack holds each distinct name ONCE (names are interned), so
        // the scan cost is proportional to the unique-name bytes (~1/3 of the
        // total on a real volume) and a hit fans out to every entry bearing
        // the name through its same-name chain. The chains are exact, so no
        // per-candidate staleness guard is needed.
        let hay = index.folded_bytes();
        let pairs = index.name_pairs();
        // Merge-walk: both the `memmem` hits and `pairs[id].folded.off` are
        // ascending, so a forward cursor maps each hit to its name id in
        // amortized O(1) with no per-hit binary search (which cache-thrashes
        // at scale). The first hit seeds the cursor with one binary search so
        // a rare term whose matches start deep in the arena doesn't pay a
        // full-length forward scan.
        let mut cursor = 0usize;
        let mut seeded = false;
        // A name is a contiguous NUL-separated region, so repeated hits in
        // one name are adjacent: a single "same as last" check dedups them.
        let mut last_id = u32::MAX;
        for pos in driver.find_iter(hay) {
            let pos = pos as u32;
            if pairs.is_empty() {
                break;
            }
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
            let id = cursor as u32;
            if id == last_id {
                continue; // driver term appears twice in one name
            }
            last_id = id;
            for idx in index.name_chain(id) {
                if index.is_synthetic(idx) {
                    continue;
                }
                if let Some(c) = evaluate(
                    index,
                    parsed,
                    &compiled,
                    idx,
                    scope,
                    &mut scope_memo,
                    &mut dir_paths,
                    has_path_terms,
                    &mut cp_buf,
                    &mut ext_scratch,
                ) {
                    candidates.push(c);
                }
            }
        }
    } else {
        // Slow path: case-sensitive, or no positive substring term (only
        // wildcards / filters / path terms). Iterate every live entry.
        for idx in index.live_entries() {
            if index.is_synthetic(idx) {
                continue;
            }
            if let Some(c) = evaluate(
                index,
                parsed,
                &compiled,
                idx,
                scope,
                &mut scope_memo,
                &mut dir_paths,
                has_path_terms,
                &mut cp_buf,
                &mut ext_scratch,
            ) {
                candidates.push(c);
            }
        }
    }

    Scan {
        candidates,
        dir_paths,
    }
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
fn order(
    index: &VolumeIndex,
    candidates: &mut [Candidate],
    dir_paths: &mut FxHashMap<EntryIdx, Option<Vec<u8>>>,
    sort: SortSpec,
    end: usize,
) {
    match sort.key {
        SortKey::Name => {}
        SortKey::Path => rank_parents(index, candidates, dir_paths),
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

/// Gives each candidate the rank of its parent directory in folded-path order,
/// so a path sort compares `(parent rank, folded name, idx)` and builds no key
/// per candidate. One folded path is built per DISTINCT parent (siblings share
/// it, and a broad result set has far fewer directories than files), and
/// parents with equal folded paths share a rank, so the order equals the old
/// per-candidate `fold(parent) + NUL + fold(name)` key order exactly: es's
/// `(parent path, name)` tuple order. A parent that is not presentable (under
/// lost+found, or a cycle) ranks last; its candidates are dropped at page time
/// anyway.
fn rank_parents(
    index: &VolumeIndex,
    candidates: &mut [Candidate],
    dir_paths: &mut FxHashMap<EntryIdx, Option<Vec<u8>>>,
) {
    use rayon::slice::ParallelSliceMut;
    let mut folded: FxHashMap<EntryIdx, Option<Vec<u8>>> = FxHashMap::default();
    for c in candidates.iter() {
        let parent = index.entry(c.idx).parent();
        folded
            .entry(parent)
            .or_insert_with(|| parent_path(index, parent, dir_paths).as_deref().map(fold));
    }
    let mut dirs: Vec<(EntryIdx, Option<Vec<u8>>)> = folded.into_iter().collect();
    // Presentable parents first, in folded-path order. The idx tiebreak only
    // makes the sort deterministic; equal paths get one rank below.
    dirs.par_sort_unstable_by(|a, b| {
        a.1.is_none()
            .cmp(&b.1.is_none())
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.0.cmp(&b.0))
    });
    let mut ranks: FxHashMap<EntryIdx, u32> =
        FxHashMap::with_capacity_and_hasher(dirs.len(), Default::default());
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
fn build_hits(
    index: &VolumeIndex,
    page: &[Candidate],
    dir_paths: &mut FxHashMap<EntryIdx, Option<Vec<u8>>>,
) -> Vec<QueryHit> {
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

/// Verifies all predicates for one candidate, returning it (with a
/// pre-reconstructed path when path terms forced one) if it matches.
#[allow(clippy::too_many_arguments)]
fn evaluate(
    index: &VolumeIndex,
    parsed: &ParsedQuery,
    compiled: &CompiledQuery,
    idx: EntryIdx,
    scope: Option<EntryIdx>,
    scope_memo: &mut FxHashMap<EntryIdx, bool>,
    dir_paths: &mut FxHashMap<EntryIdx, Option<Vec<u8>>>,
    has_path_terms: bool,
    cp_buf: &mut CodePointBuf,
    ext_scratch: &mut Vec<u8>,
) -> Option<Candidate> {
    let entry = index.entry(idx);
    if !kind_matches(parsed.filters.kind, entry.is_dir()) {
        return None;
    }
    if !size_matches(parsed.filters.size, entry.size()) {
        return None;
    }

    // Name/wildcard predicates run against the precomputed folded name
    // (case-insensitive) or the raw name (case-sensitive), matching how the
    // needles were folded at parse time.
    let hay_name = if parsed.match_case {
        entry.name()
    } else {
        index.folded_name(idx)
    };
    if !compiled
        .name_finders
        .iter()
        .all(|f| f.find(hay_name).is_some())
    {
        return None;
    }
    if !parsed
        .wildcards
        .iter()
        .all(|w| w.matches_into(hay_name, cp_buf))
    {
        return None;
    }
    // The ext check folds the candidate extension, so it runs after the
    // allocation-free name/wildcard predicates that reject most candidates.
    if !ext_matches(&parsed.filters, entry.name(), ext_scratch) {
        return None;
    }

    if let Some(root) = scope
        && !in_scope(index, idx, root, scope_memo)
    {
        return None;
    }

    // An entry whose parent has no presentable path (ancestry ends in lost+found)
    // is dropped by the page loop. Reject it here so it never counts toward
    // `total` and never occupies a page slot: otherwise `goz -n 10 report` can
    // print nothing while reporting a non-zero total, and the same query without
    // `-n` prints the real hits.
    //
    // Calls the very function the page loop uses rather than re-deriving its
    // rules, so the two cannot drift, and memoizes per parent directory rather
    // than per file. Gated on `has_orphans` because an index with no live
    // placeholder cannot contain an unpresentable entry: `has_orphans` is a
    // single load, while the walk it guards is one `path_of` per distinct parent
    // directory, which on a broad limited query is tens of thousands of walks
    // the page (100 rows) would never have needed. Paying it unconditionally
    // cost a broad limited query ~20%.
    if index.has_orphans() && parent_path(index, entry.parent(), dir_paths).is_none() {
        return None;
    }

    if has_path_terms {
        let mut buf = Vec::new();
        if !matches!(index.path_of(idx, &mut buf), crate::index::PathStatus::Ok) {
            return None;
        }
        let folded_p = if parsed.match_case { buf } else { fold(&buf) };
        if !compiled
            .path_finders
            .iter()
            .all(|f| f.find(&folded_p).is_some())
        {
            return None;
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
fn size_matches(filter: Option<SizeRange>, size: Option<u64>) -> bool {
    match filter {
        None => true,
        Some(range) => size.is_some_and(|s| range.contains(s)),
    }
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
    let base = match sort.key {
        SortKey::Name => by_name().then_with(|| a.idx.cmp(&b.idx)),
        SortKey::Path | SortKey::Size | SortKey::DateModified => a
            .key
            .cmp(&b.key)
            .then_with(by_name)
            .then_with(|| a.idx.cmp(&b.idx)),
    };
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
    use crate::usn::record::{FILE_ATTRIBUTE_DIRECTORY, ParsedUsnRecord, USN_REASON_FILE_CREATE};

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
            let page = run_query(&idx, &parsed, None, SortSpec::default_for(key), 0, Some(3));
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
        let out = run_query(idx, &parsed, None, SortSpec::default(), 0, None);
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

    #[test]
    fn scope_restricts_to_descendants() {
        let idx = sample_index();
        let parsed = parse_query("").unwrap();
        let scope = resolve_scope(&idx, b"docs\\sub", false).unwrap();
        let out = run_query(&idx, &parsed, Some(scope), SortSpec::default(), 0, None);
        let paths: Vec<String> = out
            .hits
            .iter()
            .map(|h| crate::wtf8::to_string_lossy(&h.path))
            .collect();
        assert_eq!(paths, vec!["docs\\sub\\Report.PDF"]); // descendants only, not "docs\sub" itself
    }

    #[test]
    fn sort_path_orders_by_parent_then_name() {
        let idx = sample_index();
        let parsed = parse_query("").unwrap();
        let out = run_query(
            &idx,
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
        let total = run_query(&idx, &parsed, None, SortSpec::default(), 0, None).total as u32;
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
                    let hits = run_query(&idx, &parsed, None, sort, offset, limit);
                    let refs = run_query_refs(&idx, &parsed, None, sort, offset, limit);
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
            let out = run_query(&idx, &parsed, None, SortSpec::default(), 0, None);
            let engine: BTreeSet<String> = out
                .hits
                .iter()
                .map(|h| crate::wtf8::to_string_lossy(&h.path))
                .collect();
            prop_assert_eq!(engine, oracle(&uniq, &terms));
        }
    }
}
