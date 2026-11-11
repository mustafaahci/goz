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

use super::budget::{Budget, Stopped, Watch};
use super::cexpr::{self, CNode, ExtMatcher, Form, Scratch, Source, Subject, Volume};
use super::expr::{Expr, Leaf, Target};
use super::folder_rank::FolderRanks;
use super::glob::{CodePointBuf, Glob, decode_into};
use super::parse::{Kind, ParsedQuery, SizeRange, Wildcard};
use super::related::Worked;
use crate::index::store::NamePair;
use crate::index::volume::MAX_PATH_HOPS;
use crate::index::{NameId, VolumeIndex};
use crate::types::{EntryIdx, Frn, NIL, SortDir, SortKey, SortSpec};
use memchr::memmem;
use rustc_hash::FxHashMap;
use smallvec::SmallVec;
use std::borrow::Cow;
use std::ops::Range;
use std::sync::{Mutex, OnceLock, PoisonError};

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
    /// The summed size of every match that is a file, when asked for (see
    /// [`run_query_refs`]).
    pub total_size: Option<u64>,
}

/// A [`ParsedQuery`]'s substring needles prebuilt into finders once (rather
/// than per candidate), and the [`Budget`] it runs within. The remaining
/// predicates (wildcards, filters) are read straight off the `ParsedQuery` in
/// the scan loop.
pub struct CompiledQuery<'a> {
    budget: &'a Budget<'a>,
    name_finders: Vec<memmem::Finder<'a>>,
    /// The path terms the mount prefix does not already hold. One it holds
    /// is in every full path on the volume, so `C:\ report` tests names only.
    path_finders: Vec<memmem::Finder<'a>>,
    /// The longest literal substring guaranteed present in any match, drawn
    /// from the name terms, from wildcard literals (e.g. `.dll` of `*.dll`), and
    /// from a single `ext:` value (`.ext`), or else the one needle of the
    /// expression's plan. Drives the folded-arena prefilter scan. `None` when
    /// nothing offers a usable literal (then the query iterates every entry,
    /// unless `union` drives it).
    prefilter: Option<Vec<u8>>,
    /// The needles of a union prefilter, two or more, when the buckets give
    /// no prefilter and the expression gives several (`kernel32|report`):
    /// every match's folded name holds one of them (see [`cexpr::plan`]).
    /// One needle from the expression is `prefilter` itself.
    union: Vec<Vec<u8>>,
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
    /// The path wildcards that test the full path: all of them, but those
    /// of one part that run with the name (see `name_parts`).
    path_globs: Vec<&'a Wildcard>,
    /// Each of `path_globs`' ends: the literal a matching full path ends
    /// with, and, for a pattern whose leading literal holds the drive's `:`,
    /// that literal, which a match must start with. Two byte compares turn
    /// most candidates away before the wildcard decodes the path.
    path_wildcard_ends: Vec<(Option<Vec<u8>>, Vec<u8>)>,
    /// For each of `path_globs` of literals and a trailing `\**`
    /// (`module_group_007\`), a `\` and the literals (see
    /// [`Wildcard::folder_lead`]): the folder of an entry decides such a
    /// pattern for all of its entries, which [`FolderPaths`] does once per
    /// folder instead of matching each entry's path.
    path_glob_leads: Vec<Option<Vec<u8>>>,
    /// The path wildcards of one part and no `**` (`path:*.md`, or `*.pdf`
    /// under `matchpath:`), when the mount prefix ends with a separator:
    /// such a pattern matches the last part of the full path, which is the
    /// last part of the name, so it is a test of the name, and the query can
    /// run over the distinct names.
    name_parts: Vec<&'a Glob>,
    /// The longest separator-free piece of path text that every match must
    /// hold inside one of its names (see [`path_piece`]), or else the rarest
    /// piece of a full-path `regex:` (see [`regex_path_piece`]); drives
    /// [`walk_below_names`]. `None` for a case-sensitive query, whose pieces
    /// the folded arena cannot find.
    path_piece: Option<Vec<u8>>,
    /// Whether a name has more to pass than the buckets' tests: a path
    /// wildcard of one part (`name_parts`) or the expression's terms on the
    /// name (`expr_name`). One flag, so that the test of each name of a query
    /// without either stays one branch longer than it was.
    name_extra: bool,
    /// The terms of the query's expression that the name alone decides,
    /// compiled for this volume: tested with the name, so on the pass over
    /// the distinct names too (see [`cexpr`]).
    expr_name: Option<CNode<'a>>,
    /// The rest of the expression, tested last, on each entry.
    expr_entry: Option<CNode<'a>>,
    /// Whether `expr_entry` reads full paths.
    expr_paths: bool,
    /// The mount prefix as typed, for a full path a term matching case
    /// compares in a query that does not (see [`FolderPaths`]).
    raw_prefix: Vec<u8>,
    /// The kind every match has: the kind filter's, or files for an `ext:`
    /// filter, since a folder has no extension.
    kind: Option<Kind>,
    /// The `ext:` filter, compiled.
    ext: Option<ExtMatcher>,
    /// Where a top-level `parent:` or `ancestor:` term says the matches
    /// lie, which the scan walks instead of the volume.
    source: Option<Source>,
    /// A top-level term on related entries (`child:`, `descendant-count:`
    /// ...), whose set of folders bounds the candidates when nothing
    /// narrower does (see [`Worked::for_each_candidate`]).
    related: Option<Worked>,
    /// The expression's terms on the entry, projected onto the name (see
    /// [`cexpr::name_projection`]): what the name of every match passes,
    /// beyond [`name_matches`].
    name_proj: Option<CNode<'a>>,
    /// Whether the scan, when no needle drives it, finds the names first
    /// (see [`match_scan_names`]): the query has something to test of the
    /// name. Then only the entries of the names that pass are read, not
    /// every entry of the volume: `ext:mp3;wav dm:2024`, whose list gives
    /// no needle, reads the date of the audio files alone.
    names_first: bool,
}

impl<'a> CompiledQuery<'a> {
    /// Compiles `parsed` for `index`, the volume mounted at `mount_prefix`
    /// (`C:\`, or a mount folder such as `C:\mnt\data\`), to run within
    /// `budget`.
    pub fn new(
        index: &VolumeIndex,
        parsed: &'a ParsedQuery,
        mount_prefix: &str,
        budget: &'a Budget<'a>,
    ) -> Self {
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
        // Below a prefix that ends with a separator, the last part of a full
        // path is the last part of the name: a path wildcard of one part
        // tests the name, and the last part of any other one names text the
        // name must hold, which the prefilter can scan for, so that
        // `*\bin\*.dll` finds the names holding `.dll` instead of walking
        // every entry below each `bin`.
        let parts_are_names = prefix.is_empty() || prefix.ends_with(b"\\");
        let mut name_parts = Vec::new();
        let mut path_globs = Vec::new();
        for w in &parsed.path_wildcards {
            match w.single_part() {
                Some(part) if parts_are_names => {
                    name_parts.push(part);
                    if let Some(lit) = w.longest_literal() {
                        consider(Cow::Owned(lit), false);
                    }
                }
                _ => {
                    path_globs.push(w);
                    if parts_are_names && let Some(lit) = w.name_literal() {
                        consider(Cow::Owned(lit), false);
                    }
                }
            }
        }
        let path_wildcard_ends: Vec<(Option<Vec<u8>>, Vec<u8>)> = path_globs
            .iter()
            .map(|w| (anchored_lead(w), w.trailing_literal()))
            .collect();
        let path_glob_leads = path_globs
            .iter()
            .map(|w| {
                w.folder_lead()
                    .map(|lead| [b"\\".as_slice(), &lead].concat())
            })
            .collect();
        // The expression, with the path terms this volume's prefix decides
        // folded to constants: one that folds to `False` leaves the volume
        // nothing to read, as a path term of another drive does.
        let folded_prefix = if parsed.match_case {
            fold(mount_prefix.as_bytes())
        } else {
            prefix.clone()
        };
        let volume = Volume {
            index,
            raw_prefix: mount_prefix.as_bytes(),
            folded_prefix: &folded_prefix,
            parts_are_names,
            budget,
            related: Default::default(),
        };
        let expr = parsed.expr.as_ref().map(|e| cexpr::compile(e, &volume));
        let expr_admits = !matches!(expr, Some(CNode::False));
        let (expr_name, expr_entry) = match expr {
            Some(e) if expr_admits => cexpr::split(e),
            _ => (None, None),
        };
        // The name tests the expression's terms on the entry imply, from a
        // second compilation, which the split consumes.
        let name_proj = expr_entry
            .as_ref()
            .and(parsed.expr.as_ref())
            .and_then(|e| cexpr::split(cexpr::compile(e, &volume)).1)
            .and_then(cexpr::name_projection);
        // An `ext:` filter keeps files, as a folder has no extension.
        let (kind, kinds_agree) = match (parsed.filters.kind, parsed.filters.ext.is_some()) {
            (Some(Kind::Folder), true) => (None, false),
            (None, true) => (Some(Kind::File), true),
            (kind, _) => (kind, true),
        };
        let volume_admits = expr_admits
            && kinds_agree
            && parsed.path_terms.iter().all(|t| term_admits(t, &prefix))
            && path_wildcard_ends.iter().all(|(lead, _)| {
                lead.as_deref().is_none_or(|lead| {
                    let n = lead.len().min(prefix.len());
                    lead[..n] == prefix[..n]
                })
            });

        let path_piece = (!parsed.match_case)
            .then(|| {
                path_piece(parsed, &prefix).or_else(|| regex_path_piece(index, parsed, &prefix))
            })
            .flatten();

        // The buckets' prefilter keeps priority, so a query of plain terms
        // scans as it always has. Without one, the expression may still
        // name what every match's name holds: one needle is a prefilter
        // like any other, several a union of passes over the names.
        let mut prefilter = prefilter.map(Cow::into_owned);
        let mut union = Vec::new();
        if prefilter.is_none()
            && !parsed.match_case
            && let Some(mut needles) = cexpr::plan(expr_name.iter().chain(&expr_entry))
        {
            if needles.len() == 1 {
                prefilter = needles.pop();
            } else {
                union = needles;
            }
        }

        let names_first = name_proj.is_some()
            || expr_name.is_some()
            || parsed.filters.ext.is_some()
            || !parsed.name_terms.is_empty()
            || !parsed.wildcards.is_empty()
            || !name_parts.is_empty();
        Self {
            name_proj,
            names_first,
            source: expr_entry.as_ref().and_then(cexpr::source),
            related: expr_entry.as_ref().and_then(cexpr::related_source),
            raw_prefix: mount_prefix.as_bytes().to_vec(),
            kind,
            ext: parsed
                .filters
                .ext
                .as_ref()
                .map(|values| ExtMatcher::new(values, Form::Folded)),
            budget,
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
            prefilter,
            union,
            prefilter_decides_name: from_name_term
                && parsed.name_terms.len() == 1
                && parsed.wildcards.is_empty()
                && parsed.filters.ext.is_none()
                && name_parts.is_empty()
                && expr_name.is_none(),
            prefix,
            volume_admits,
            path_globs,
            path_wildcard_ends,
            path_glob_leads,
            path_piece,
            expr_paths: expr_entry.as_ref().is_some_and(CNode::tests_paths),
            name_extra: !name_parts.is_empty() || expr_name.is_some(),
            name_parts,
            expr_name,
            expr_entry,
        }
    }

    /// The needles the folded names are scanned for, each match's name
    /// holding one: the prefilter, or a union's; none for a case-sensitive
    /// query, whose needles the folded names cannot hold, or a query with no
    /// literal to scan for.
    fn needles(&self, parsed: &ParsedQuery) -> &[Vec<u8>] {
        match &self.prefilter {
            _ if parsed.match_case => &[],
            Some(needle) => std::slice::from_ref(needle),
            None => &self.union,
        }
    }

    /// Whether a candidate's full path has anything left to decide.
    fn tests_paths(&self) -> bool {
        !self.path_finders.is_empty() || !self.path_globs.is_empty() || self.expr_paths
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

/// The shortest piece of a `regex:` pattern [`regex_path_piece`] walks
/// below: a name holding a shorter one is too common to narrow the walk.
const REGEX_PIECE_MIN: usize = 3;

/// The most pieces of `regex:` patterns [`regex_path_piece`] weighs, the
/// longest: each costs a scan of the folded names.
const REGEX_PIECES_WEIGHED: usize = 4;

/// A separator-free piece of text that every match of the query's top-level
/// case-insensitive `regex:` terms on the full path (or its location or
/// parent's name) holds, folded (see [`super::pattern::Need`]), as
/// [`path_piece`] takes one from path terms: when the prefix does not hold
/// it, it lies inside one name of the match's path. Of several, the one the
/// fewest entries bear a name holding, of the [`REGEX_PIECES_WEIGHED`]
/// longest: a pattern's pieces name its folders and its file alike, and
/// `regex:bin_x64_05\\kernel32` must walk below the few names holding `kernel32`,
/// not below every `bin_x64_05` folder, though that piece is longer.
fn regex_path_piece(index: &VolumeIndex, parsed: &ParsedQuery, prefix: &[u8]) -> Option<Vec<u8>> {
    if !prefix.is_empty() && !prefix.ends_with(b"\\") {
        return None;
    }
    let top = match parsed.expr.as_ref()? {
        Expr::And(items) => items.as_slice(),
        other => std::slice::from_ref(other),
    };
    let mut pieces: Vec<&[u8]> = Vec::new();
    for e in top {
        if let Expr::Leaf(Leaf::Regex {
            target: Target::FullPath | Target::Location | Target::ParentName,
            regex,
        }) = e
            && let Some(need) = regex.need().filter(|need| need.folded)
        {
            for piece in need.required_pieces() {
                pieces.extend(
                    piece.split(|&b| b == b'\\').filter(|p| {
                        p.len() >= REGEX_PIECE_MIN && memmem::find(prefix, p).is_none()
                    }),
                );
            }
        }
    }
    pieces.sort_unstable_by(|a, b| b.len().cmp(&a.len()).then(a.cmp(b)));
    pieces.dedup();
    pieces.truncate(REGEX_PIECES_WEIGHED);
    let rarest = if pieces.len() > 1 {
        // The first of the fewest: the longest of them.
        pieces.into_iter().min_by_key(|p| {
            let mut bearing = 0u64;
            for_each_prefilter_hit(index, p, |id| bearing += u64::from(index.name_refcount(id)));
            bearing
        })
    } else {
        pieces.pop()
    };
    rarest.map(<[u8]>::to_vec)
}

/// The leading literal of sub-path wildcard `w` when it holds a `:`: then
/// the match starts where the full path does, since Windows refuses `:` in a
/// name, so the only `:` of a full path is the drive's, in its first part,
/// and a match that covers it starts at that part. Elsewhere the match may
/// start after any separator, so its leading literal tells nothing about
/// where the full path starts.
pub(super) fn anchored_lead(w: &Wildcard) -> Option<Vec<u8>> {
    let lead = w.leading_literal();
    lead.contains(&b':').then_some(lead)
}

/// Whether path term `term` can match a full path that starts with `prefix`
/// (both folded alike). Windows refuses `:` in a name, so a term's first `:` can
/// only be the drive's, the first `:` in `prefix`; that fixes where the term
/// starts, and the part of it that overlaps the prefix must equal the prefix
/// there. `D:\data` thus skips every volume but D: without reading an entry.
pub(super) fn term_admits(term: &[u8], prefix: &[u8]) -> bool {
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
#[derive(Clone, Copy)]
struct Candidate {
    idx: EntryIdx,
    /// The key the page's sort asks for, filled by [`entry_key`] or, for a
    /// path sort, [`path_keys`]: the name's rank key for a name sort, the
    /// parent folder's rank and the name's rank packed together for a path
    /// sort, [`size_key`] for a size sort, [`mtime_key`] for a date sort.
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
    let unlimited = Budget::unlimited();
    let Ok(RefOutcome { total, refs, .. }) = page_refs(
        index,
        mount_prefix,
        parsed,
        scope,
        sort,
        offset,
        limit,
        false,
        true,
        &unlimited,
    ) else {
        unreachable!("an unlimited budget is never spent");
    };
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
///
/// `sum_sizes` also sums the size of every match that is a file, for es's
/// `-get-total-size`: a folder adds nothing, nor a file whose size is not
/// read yet, and each name of a hard-linked file adds its size, as each is a
/// row of its own. A sum past `u64::MAX` stops there.
///
/// Fails with [`Stopped`] when `budget` is spent before the matches are all
/// found. Only the steps a query's terms make costlier check it: a pass over
/// the names that tests none of them, like `e`, and the ordering after any
/// search, cost the same whatever the query says, so they always finish.
#[allow(clippy::too_many_arguments)]
pub fn run_query_refs(
    index: &VolumeIndex,
    mount_prefix: &str,
    parsed: &ParsedQuery,
    scope: Option<EntryIdx>,
    sort: SortSpec,
    offset: u32,
    limit: Option<u32>,
    sum_sizes: bool,
    budget: &Budget,
) -> Result<RefOutcome, Stopped> {
    page_refs(
        index,
        mount_prefix,
        parsed,
        scope,
        sort,
        offset,
        limit,
        sum_sizes,
        true,
        budget,
    )
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
/// been walked. A bounded page sorted by an entry key keeps only its best
/// rows as it scans (see [`top_k_refs`]). Everything else takes the
/// candidate path: scan, order, slice. `fast` exists for the tests that hold
/// these paths equal to the candidate path. See [`run_query_refs`] for what
/// `budget` stops.
#[allow(clippy::too_many_arguments)]
fn page_refs(
    index: &VolumeIndex,
    mount_prefix: &str,
    parsed: &ParsedQuery,
    scope: Option<EntryIdx>,
    sort: SortSpec,
    offset: u32,
    limit: Option<u32>,
    sum_sizes: bool,
    fast: bool,
    budget: &Budget,
) -> Result<RefOutcome, Stopped> {
    let compiled = CompiledQuery::new(index, parsed, mount_prefix, budget);
    // A top-level `ancestor:` is a scope: its subtree is walked as one is.
    let scope = scope.or(match compiled.source {
        Some(Source::Subtree(dir)) => Some(dir),
        _ => None,
    });
    // A scope at the root admits every entry with a path, as no scope does,
    // so it goes: kept, it would bar the name-level path, and `-path C:\ e`
    // ran three times slower than `e`.
    let scope = scope.filter(|&s| s != index.root());
    if fast && sort.key == SortKey::Name && is_name_level(index, parsed, &compiled, scope) {
        let (matched, total) = match_names(index, parsed, &compiled);
        if budget.spent() {
            return Err(Stopped);
        }
        // Folders add no size, and a name a folder bears may be a file's
        // too, whose size a folders-only query must not add.
        let total_size = sum_sizes.then(|| match compiled.kind {
            Some(Kind::Folder) => 0,
            _ => names_files_size(index, &matched),
        });
        let (start, end) = page_bounds(total as usize, offset, limit);
        let refs = match compiled.kind {
            None => name_level_refs(index, &matched, sort.dir, start, end, |e| {
                !index.is_synthetic(e)
            }),
            Some(kind) => name_level_refs(index, &matched, sort.dir, start, end, |e| {
                !index.is_synthetic(e) && kind_matches(Some(kind), index.entry(e).is_dir())
            }),
        };
        return Ok(RefOutcome {
            total,
            refs,
            total_size,
        });
    }
    if fast
        && let Some(limit) = limit
        && top_k_applies(index, parsed, &compiled, scope, sort, offset, Some(limit))
    {
        return top_k_refs(index, parsed, &compiled, sort, offset, limit, sum_sizes);
    }
    let mut candidates = scan(index, parsed, &compiled, scope);
    if budget.spent() {
        return Err(Stopped);
    }
    let total = candidates.len() as u64;
    let total_size = sum_sizes.then(|| files_size(index, candidates.iter().map(|c| c.idx)));
    let (start, end) = page_bounds(candidates.len(), offset, limit);
    order(index, &mut candidates, sort, end);
    let refs = candidates[start..end]
        .iter()
        .map(|c| HitRef {
            idx: c.idx,
            frn: index.entry(c.idx).frn(),
        })
        .collect();
    Ok(RefOutcome {
        total,
        refs,
        total_size,
    })
}

/// The summed size of the files among `entries`: a folder adds nothing, nor
/// a file whose size is not read yet.
fn files_size(index: &VolumeIndex, entries: impl Iterator<Item = EntryIdx>) -> u64 {
    entries
        .map(|idx| index.entry(idx))
        .filter(|e| !e.is_dir())
        .filter_map(|e| e.size())
        .fold(0, u64::saturating_add)
}

/// Whether every predicate of `parsed` is decided by the file name alone, so
/// the query can run over the distinct names instead of the entries: no
/// scope, no path term the mount prefix leaves open, no size or date filter,
/// and no entry without a path (an orphan, or one in a parent cycle) whose
/// presentability would need a per-entry walk. Extensions, wildcards and
/// case sensitivity are properties of the name and pass, and so does an
/// expression whose terms all are. A kind filter passes too: a name no
/// folder bears counts its files by refcount, and only the chain of one a
/// folder may bear is walked (see [`retain_kind`]), so `ext:pdf`, which
/// keeps files, runs over the names as it did when it kept folders too.
fn is_name_level(
    index: &VolumeIndex,
    parsed: &ParsedQuery,
    compiled: &CompiledQuery,
    scope: Option<EntryIdx>,
) -> bool {
    scope.is_none()
        && compiled.volume_admits
        && !compiled.tests_paths()
        && parsed.filters.size.is_none()
        && parsed.filters.date.is_none()
        && compiled.expr_entry.is_none()
        && !index.may_have_unpresentable()
}

/// The name predicates of `parsed` against one distinct name: substring
/// terms, wildcards, the extension filter and the expression's terms on the
/// name, on the folded bytes (case-insensitive) or the raw bytes
/// (case-sensitive), matching how the needles were folded at parse time.
/// Shared by the per-entry [`evaluate`] and the per-name [`match_names`], so
/// the two paths cannot disagree on what a name matches.
fn name_matches(
    index: &VolumeIndex,
    parsed: &ParsedQuery,
    compiled: &CompiledQuery,
    id: NameId,
    scratch: &mut Scratch,
) -> bool {
    name_matches_as::<true>(index, parsed, compiled, id, scratch)
}

/// [`name_matches`], stamped out for whether the query may have tests beyond
/// the buckets' (`EXTRA`): with `EXTRA` false, for a query that has none
/// (`!compiled.name_extra`), it is the test v0.6.0 ran, not one branch
/// longer, for the pass over every distinct name that tests nothing else.
fn name_matches_as<const EXTRA: bool>(
    index: &VolumeIndex,
    parsed: &ParsedQuery,
    compiled: &CompiledQuery,
    id: NameId,
    scratch: &mut Scratch,
) -> bool {
    let hay = if parsed.match_case {
        index.name_raw(id)
    } else {
        index.name_folded(id)
    };
    compiled.name_finders.iter().all(|f| f.find(hay).is_some())
        && (parsed.wildcards.is_empty()
            || wildcards_match(&parsed.wildcards, hay, &mut scratch.cps, compiled.budget))
        // The ext check folds the extension, so it runs after the
        // allocation-free predicates that reject most names.
        && compiled.ext.as_ref().is_none_or(|ext| {
            ext.matches(
                index.name_raw(id),
                &mut scratch.ext,
                &mut scratch.text_cps,
                compiled.budget,
            )
        })
        && (!EXTRA
            || !compiled.name_extra
            || name_extra_matches(index, parsed, compiled, id, hay, scratch))
}

/// What name `id` (`hay` as the query compares it) must pass beyond the
/// buckets' tests: the path wildcards of one part, and the expression's
/// terms on the name. Kept out of line, as is [`entry_expr_matches`], so that
/// the test of every name of a query without them stays the code it was.
#[inline(never)]
fn name_extra_matches(
    index: &VolumeIndex,
    parsed: &ParsedQuery,
    compiled: &CompiledQuery,
    id: NameId,
    hay: &[u8],
    scratch: &mut Scratch,
) -> bool {
    (compiled.name_parts.is_empty()
        || last_part_matches(&compiled.name_parts, hay, &mut scratch.cps, compiled.budget))
        && compiled.expr_name.as_ref().is_none_or(|e| {
            e.eval(&mut Subject::name_level(
                index,
                id,
                parsed.match_case,
                compiled.budget,
                scratch,
            ))
        })
}

/// Whether entry `idx` matches `e`, the expression's terms on the entry,
/// reading its full path through `folders` when a term asks.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn entry_expr_matches(
    e: &CNode,
    index: &VolumeIndex,
    idx: EntryIdx,
    match_case: bool,
    budget: &Budget,
    folders: &mut Option<FolderPaths>,
    scratch: &mut Scratch,
) -> bool {
    let mut full_path = |out: &mut Vec<u8>, stored: bool| {
        folders
            .as_mut()
            .and_then(|f| f.with_full_path_as(index, idx, stored, |p| out.extend_from_slice(p)))
            .is_some()
    };
    e.eval(&mut Subject::entry(
        index,
        idx,
        match_case,
        budget,
        scratch,
        &mut full_path,
    ))
}

/// Whether the last part of `name` matches every one of `parts`: the
/// part after its last separator, which is all of it, since NTFS keeps
/// separators out of names. One that corrupt data put there ends a part of
/// the full path, so the name's last part is the full path's.
fn last_part_matches(
    parts: &[&Glob],
    name: &[u8],
    cp_buf: &mut CodePointBuf,
    budget: &Budget,
) -> bool {
    let last = memchr::memrchr(b'\\', name).map_or(name, |sep| &name[sep + 1..]);
    decode_into(last, cp_buf);
    parts
        .iter()
        .all(|part| part.matches_decoded(cp_buf, budget))
}

/// Whether `text`, a name or full path, matches every one of `wildcards`,
/// decoded once for them all.
fn wildcards_match(
    wildcards: &[Wildcard],
    text: &[u8],
    cp_buf: &mut CodePointBuf,
    budget: &Budget,
) -> bool {
    decode_into(text, cp_buf);
    wildcards.iter().all(|w| w.matches_decoded(cp_buf, budget))
}

/// Calls `f` once per distinct name whose folded bytes contain `needle`: one
/// `memmem` pass over the folded arena, which holds each name once, mapping
/// the ascending hit offsets to ids with a forward cursor over `name_pairs`
/// (see [`name_holding`]). A name is a contiguous NUL-separated region, so
/// after a hit the search resumes at the end of that name: a short needle
/// like `e` occurs about three times in each name it is in, and finding the
/// rest only to drop them was most of the pass.
fn for_each_prefilter_hit(index: &VolumeIndex, needle: &[u8], f: impl FnMut(NameId)) {
    for_each_prefilter_hit_in(index, needle, 0..index.name_count(), f);
}

/// [`for_each_prefilter_hit`] over the names with ids in `ids`. The arena
/// holds the names in id order, so their bytes are one stretch of it, and a
/// match cannot run past a name's end into the next range.
fn for_each_prefilter_hit_in(
    index: &VolumeIndex,
    needle: &[u8],
    ids: Range<usize>,
    mut f: impl FnMut(NameId),
) {
    // No name holds a NUL, but one separates each name from the next, so a
    // needle holding a NUL could only match across names.
    if needle.contains(&0) || ids.is_empty() {
        return;
    }
    let driver = memmem::Finder::new(needle);
    let all = index.name_pairs();
    let hay = index.folded_bytes();
    let hay = &hay[..all
        .get(ids.end)
        .map_or(hay.len(), |p| p.folded.off as usize)];
    let pairs = &all[..ids.end];
    let mut cursor = ids.start;
    let mut at = pairs[ids.start].folded.off as usize;
    while let Some(rel) = hay.get(at..).and_then(|rest| driver.find(rest)) {
        let pos = at + rel;
        cursor = name_holding(pairs, cursor, pos);
        f(cursor as NameId);
        // On to the next name. An empty needle, which no caller passes, also
        // matches at the end of the arena: `max` and `get` still end the loop.
        let name = pairs[cursor].folded;
        at = (name.off as usize + name.len as usize + 1).max(pos + 1);
    }
}

/// The id of the name holding byte `pos` of the folded arena: the last of
/// `pairs` (ascending `folded.off`) from `from` on whose bytes start at or
/// before `pos`, where `pairs[from]`'s do.
///
/// A needle most names hold finds the next name or the one after, which the
/// two steps in line settle with the loads a walk name by name makes. A
/// rarer one gallops out of line from there, so the names between two hits
/// cost a logarithm of their count. A walk over them read every pair up to
/// the last hit, 16 bytes a name: for a rare needle on 2.1 M names, 2.1 M
/// turns of a loop and 34 MB beside the arena's 67, about as long as the
/// `memmem` pass itself, and most likely that loop's address alone (whether
/// it crossed a 64-byte fetch window) moved a rare path term's time by a
/// fifth (see `docs/DESIGN.md`).
#[inline(always)]
fn name_holding(pairs: &[NamePair], from: usize, pos: usize) -> usize {
    let starts_by = |i: usize| pairs.get(i).is_some_and(|p| p.folded.off as usize <= pos);
    if !starts_by(from + 1) {
        from
    } else if !starts_by(from + 2) {
        from + 1
    } else {
        gallop_to(pairs, from + 2, pos)
    }
}

/// [`name_holding`] past its two steps: doubling strides from `from`, then
/// a binary search of the last stride.
#[inline(never)]
fn gallop_to(pairs: &[NamePair], from: usize, pos: usize) -> usize {
    let (mut lo, mut stride) = (from, 1);
    while pairs
        .get(lo + stride)
        .is_some_and(|p| p.folded.off as usize <= pos)
    {
        lo += stride;
        stride *= 2;
    }
    let end = (lo + stride).min(pairs.len());
    lo + pairs[lo + 1..end].partition_point(|p| p.folded.off as usize <= pos)
}

/// The names a scan for several needles has handed on already, so that a
/// name holding two of them is tested once. A scan for one needle finds each
/// name once and needs none.
///
/// Every scan of the names, for one needle or several, goes through the one
/// instantiation of [`for_each_prefilter_hit_in`] its pass makes, once per
/// needle: a second generic path for unions beside it made the compiler stop
/// inlining each pass's per-name closure into its loop, and a plain query's
/// pass over every name ran 15% slower.
struct Seen {
    base: usize,
    words: Vec<u64>,
}

impl Seen {
    /// The set for a scan of `needles` over the names with ids in `ids`:
    /// `None` for one needle.
    fn of(needles: &[Vec<u8>], ids: &Range<usize>) -> Option<Seen> {
        (needles.len() > 1).then(|| Seen {
            base: ids.start,
            words: vec![0; ids.len().div_ceil(64)],
        })
    }

    /// Whether `id` comes for the first time, marking it.
    fn first(&mut self, id: NameId) -> bool {
        let at = id as usize - self.base;
        let bit = 1u64 << (at % 64);
        let first = self.words[at / 64] & bit == 0;
        self.words[at / 64] |= bit;
        first
    }
}

/// A pass over the names splits them into chunks of at least this many,
/// which run across cores, and one this small runs as a single chunk. Tests
/// use one 64-id word, so that small indexes split too.
const CHUNK_NAMES_MIN: usize = if cfg!(test) { 64 } else { 1 << 15 };

/// How many 64-id words of a name set each chunk of a pass over `names`
/// names covers: about four chunks per thread, so a chunk dense in matches
/// does not hold the others up, and none below [`CHUNK_NAMES_MIN`].
fn chunk_words(names: usize) -> usize {
    names
        .div_ceil(64)
        .div_ceil(name_threads() * 4)
        .max(CHUNK_NAMES_MIN / 64)
}

/// The pool the passes over the names run their chunks on. Rayon's global
/// pool also runs the long sorts, of every match of a broad unlimited query
/// or of a rebuilt name order, and a pass queued behind one of them held its
/// caller's read lock, and so the volume's journal tail, for as long. `None`
/// when the pool's threads could not start: the passes then use the global
/// pool.
fn name_pool() -> Option<&'static rayon::ThreadPool> {
    static POOL: OnceLock<Option<rayon::ThreadPool>> = OnceLock::new();
    POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .thread_name(|i| format!("goz-names-{i}"))
            .build()
            .ok()
    })
    .as_ref()
}

/// How many threads run the chunks of a pass over the names.
fn name_threads() -> usize {
    name_pool().map_or_else(rayon::current_num_threads, |pool| {
        pool.current_num_threads()
    })
}

/// Runs `pass`, a pass over `chunks` chunks of the names, on [`name_pool`],
/// or on the calling thread when there is one chunk, so a small volume's
/// query never waits for a pool.
fn on_name_pool<R: Send>(chunks: usize, pass: impl FnOnce() -> R + Send) -> R {
    match name_pool() {
        Some(pool) if chunks > 1 => pool.install(pass),
        _ => pass(),
    }
}

/// The name ids that chunk `i` of a pass over `names` names covers, each
/// chunk being `per` 64-id words.
fn chunk_ids(i: usize, per: usize, names: usize) -> Range<usize> {
    (i * per * 64).min(names)..((i + 1) * per * 64).min(names)
}

/// A set of name ids, one bit per id.
pub(super) struct NameSet {
    words: Vec<u64>,
    len: usize,
}

impl NameSet {
    pub(super) fn contains(&self, id: NameId) -> bool {
        self.words[id as usize / 64] & (1 << (id % 64)) != 0
    }

    /// The ids in the set, ascending.
    pub(super) fn ids(&self) -> impl Iterator<Item = NameId> + '_ {
        word_ids(0, &self.words)
    }
}

/// The ids whose bits are set in `words`, the first word holding ids from
/// `base` on, ascending.
fn word_ids(base: usize, words: &[u64]) -> impl Iterator<Item = NameId> + '_ {
    words.iter().enumerate().flat_map(move |(w, &word)| {
        let mut word = word;
        std::iter::from_fn(move || {
            if word == 0 {
                return None;
            }
            let bit = word.trailing_zeros() as usize;
            word &= word - 1;
            Some((base + w * 64 + bit) as NameId)
        })
    })
}

/// How many live entries of `kind`, synthetic ones aside, bear name `id`:
/// for a name no folder bears, its files are all its entries, counted as
/// [`entries_bearing`] counts them, and it has no folder; only the chain of a
/// name a folder may bear is walked, testing each entry.
fn entries_of_kind(index: &VolumeIndex, id: NameId, kind: Kind) -> u64 {
    if !index.name_may_bear_folder(id) {
        return match kind {
            Kind::File => entries_bearing(index, id),
            Kind::Folder => 0,
        };
    }
    index
        .name_chain(id)
        .filter(|&e| !index.is_synthetic(e) && kind_matches(Some(kind), index.entry(e).is_dir()))
        .count() as u64
}

/// How many live entries, synthetic ones aside, bear name `id`: its refcount,
/// which is its chain length, so no entry is touched, except for the three
/// names synthetic entries can bear, whose chains are walked. Inline in
/// each pass over the names, as in v0.6.0: a call per name more cost a
/// pass that tests every name a few percent.
#[inline(always)]
fn entries_bearing(index: &VolumeIndex, id: NameId) -> u64 {
    if index.name_may_bear_synthetic(id) {
        index
            .name_chain(id)
            .filter(|&e| !index.is_synthetic(e))
            .count() as u64
    } else {
        u64::from(index.name_refcount(id))
    }
}

/// Every distinct name matching `parsed`, and the number of live
/// non-synthetic entries bearing them (see [`entries_bearing`]), or some of
/// them once the query's budget is spent.
///
/// The names are matched in chunks across cores. Each chunk sets bits in its
/// own words of a set the caller allocated, and frees what it allocates
/// itself: a buffer freed by another thread than the one that allocated it
/// stays committed in that thread's heap, and the pool's threads never exit.
fn match_names(
    index: &VolumeIndex,
    parsed: &ParsedQuery,
    compiled: &CompiledQuery,
) -> (NameSet, u64) {
    // Stamped out for the query's shape, so that a query with one needle or
    // none and no name tests beyond the buckets' runs the loop it always has:
    // it tests every distinct name, and an instruction more per name showed.
    // A kind filter is applied after each chunk's loop (see
    // [`retain_kind`]), which stays the same.
    match (compiled.needles(parsed).len() > 1, compiled.name_extra) {
        (false, false) => match_names_as::<false, false>(index, parsed, compiled),
        (false, true) => match_names_as::<false, true>(index, parsed, compiled),
        (true, false) => match_names_as::<true, false>(index, parsed, compiled),
        (true, true) => match_names_as::<true, true>(index, parsed, compiled),
    }
}

/// [`match_names`] for a scan of several needles (`UNION`), whose repeats
/// it skips, and for a query with name tests beyond the buckets' (`EXTRA`).
/// With a kind filter, a name with no entry of the kind is left out of the
/// set, so the page walk never visits its chain.
fn match_names_as<const UNION: bool, const EXTRA: bool>(
    index: &VolumeIndex,
    parsed: &ParsedQuery,
    compiled: &CompiledQuery,
) -> (NameSet, u64) {
    use rayon::prelude::*;
    let names = index.name_count();
    let per = chunk_words(names);
    let needles = compiled.needles(parsed);
    // Case-sensitive, or no literal to drive the arena scan: every live name
    // is tested. Dead names are zero-filled and have no entries.
    let verify = needles.is_empty() || !compiled.prefilter_decides_name;
    let mut words = vec![0u64; names.div_ceil(64)];
    let chunks = words.len().div_ceil(per);
    let (len, total) = on_name_pool(chunks, || {
        words
            .par_chunks_mut(per)
            .enumerate()
            .map(|(i, words)| {
                let ids = chunk_ids(i, per, names);
                let base = ids.start;
                let mut scratch = Scratch::default();
                let mut watch = Watch::new(compiled.budget);
                let (mut len, mut total) = (0usize, 0u64);
                let mut seen = if UNION { Seen::of(needles, &ids) } else { None };
                let mut mark = |id: NameId| {
                    if UNION
                        && let Some(seen) = seen.as_mut()
                        && !seen.first(id)
                    {
                        return;
                    }
                    // Only testing a name costs more with more terms, so only
                    // that counts against the budget. Without it this pass is
                    // bounded by the volume's names, whatever the query.
                    if verify
                        && (watch.tick()
                            || !name_matches_as::<EXTRA>(index, parsed, compiled, id, &mut scratch))
                    {
                        return;
                    }
                    let at = id as usize - base;
                    words[at / 64] |= 1 << (at % 64);
                    len += 1;
                    total += entries_bearing(index, id);
                };
                if needles.is_empty() {
                    ids.map(|id| id as NameId)
                        .filter(|&id| index.name_refcount(id) > 0)
                        .for_each(&mut mark);
                } else {
                    for needle in needles {
                        for_each_prefilter_hit_in(index, needle, ids.clone(), &mut mark);
                    }
                }
                if let Some(kind) = compiled.kind {
                    let (dropped, others) = retain_kind(index, kind, base, words);
                    len -= dropped;
                    total -= others;
                }
                (len, total)
            })
            .reduce(|| (0, 0), |a, b| (a.0 + b.0, a.1 + b.1))
    });
    (NameSet { words, len }, total)
}

/// Narrows the names one chunk of [`match_names_as`] matched (`words`, bit
/// `i` for id `base + i`) to those bearing an entry of `kind`, and returns
/// how many names it dropped and how many of the entries counted for the
/// names are of another kind. A file filter visits only the names a folder
/// may bear, a few, 64 names a word; a folder filter every matched name.
/// After the loop over the names, so that the test of each, as hot as any
/// loop in the engine, is v0.6.0's: a test of the kind in it cost an `ext:`
/// list, which keeps files, 4% (and more at first, with the calls it
/// brought).
#[inline(never)]
fn retain_kind(index: &VolumeIndex, kind: Kind, base: usize, words: &mut [u64]) -> (usize, u64) {
    debug_assert_eq!(base % 64, 0);
    let (mut dropped, mut others) = (0usize, 0u64);
    for (w, word) in words.iter_mut().enumerate() {
        let mut visit = match kind {
            Kind::File => *word & index.name_folder_word(base / 64 + w),
            Kind::Folder => *word,
        };
        while visit != 0 {
            let bit = visit.trailing_zeros();
            visit &= visit - 1;
            let id = (base + w * 64 + bit as usize) as NameId;
            let of_kind = entries_of_kind(index, id, kind);
            others += entries_bearing(index, id).saturating_sub(of_kind);
            if of_kind == 0 {
                *word &= !(1 << bit);
                dropped += 1;
            }
        }
    }
    (dropped, others)
}

/// The names a scan with [`CompiledQuery::names_first`] visits: every live
/// name that passes [`name_matches`] and the projection of the expression's
/// terms on the entry, matched in chunks across cores as in [`match_names`],
/// and how many entries bear them. Only testing a name counts against the
/// budget, as there.
fn match_scan_names(
    index: &VolumeIndex,
    parsed: &ParsedQuery,
    compiled: &CompiledQuery,
) -> (NameSet, u64) {
    use rayon::prelude::*;
    let names = index.name_count();
    let per = chunk_words(names);
    let mut words = vec![0u64; names.div_ceil(64)];
    let chunks = words.len().div_ceil(per);
    let (len, bearing) = on_name_pool(chunks, || {
        words
            .par_chunks_mut(per)
            .enumerate()
            .map(|(i, words)| {
                let ids = chunk_ids(i, per, names);
                let base = ids.start;
                let mut scratch = Scratch::default();
                let mut watch = Watch::new(compiled.budget);
                let (mut len, mut bearing) = (0usize, 0u64);
                for id in ids.map(|id| id as NameId) {
                    if index.name_refcount(id) == 0 {
                        continue;
                    }
                    if watch.tick() {
                        break;
                    }
                    let passes = name_matches(index, parsed, compiled, id, &mut scratch)
                        && compiled.name_proj.as_ref().is_none_or(|p| {
                            p.eval(&mut Subject::name_level(
                                index,
                                id,
                                parsed.match_case,
                                compiled.budget,
                                &mut scratch,
                            ))
                        });
                    if passes {
                        let at = id as usize - base;
                        words[at / 64] |= 1 << (at % 64);
                        len += 1;
                        bearing += u64::from(index.name_refcount(id));
                    }
                }
                (len, bearing)
            })
            .reduce(|| (0, 0), |a, b| (a.0 + b.0, a.1 + b.1))
    });
    (NameSet { words, len }, bearing)
}

/// The distinct names `node`, a term on the name alone, matches: those of
/// a sub-search (see the `related` module), with only the names a folder
/// may bear when `folders`. The names are matched in chunks across cores,
/// as in [`match_names`]: in each, the names holding the term's needles
/// are found in the folded names and tested, or with no needle every live
/// name is. Each test counts against `budget`, and a spent one leaves the
/// set unfinished.
pub(super) fn names_passing(
    index: &VolumeIndex,
    node: &CNode,
    folders: bool,
    budget: &Budget,
) -> NameSet {
    use rayon::prelude::*;
    let names = index.name_count();
    let per = chunk_words(names);
    let needles = cexpr::plan(std::iter::once(node)).unwrap_or_default();
    let mut words = vec![0u64; names.div_ceil(64)];
    let chunks = words.len().div_ceil(per);
    let len = on_name_pool(chunks, || {
        words
            .par_chunks_mut(per)
            .enumerate()
            .map(|(i, words)| {
                let ids = chunk_ids(i, per, names);
                let base = ids.start;
                let mut scratch = Scratch::default();
                let mut watch = Watch::new(budget);
                let mut seen = Seen::of(&needles, &ids);
                let mut len = 0usize;
                let mut mark = |id: NameId| {
                    if seen.as_mut().is_some_and(|seen| !seen.first(id))
                        || index.name_refcount(id) == 0
                        || (folders && !index.name_may_bear_folder(id))
                        || watch.tick()
                    {
                        return;
                    }
                    // A name's own test: the query's case does not reach
                    // it, as it tells only how full paths are folded.
                    if node.eval(&mut Subject::name_level(
                        index,
                        id,
                        false,
                        budget,
                        &mut scratch,
                    )) {
                        let at = id as usize - base;
                        words[at / 64] |= 1 << (at % 64);
                        len += 1;
                    }
                };
                if needles.is_empty() {
                    ids.map(|id| id as NameId).for_each(&mut mark);
                } else {
                    for needle in &needles {
                        for_each_prefilter_hit_in(index, needle, ids.clone(), &mut mark);
                    }
                }
                len
            })
            .sum()
    });
    NameSet { words, len }
}

/// The summed size of the files bearing a name in `matched`, as
/// [`files_size`] counts it, chunk by chunk across cores.
fn names_files_size(index: &VolumeIndex, matched: &NameSet) -> u64 {
    use rayon::prelude::*;
    let per = chunk_words(index.name_count());
    on_name_pool(matched.words.len().div_ceil(per), || {
        matched
            .words
            .par_chunks(per)
            .enumerate()
            .map(|(i, words)| {
                files_size(
                    index,
                    word_ids(i * per * 64, words)
                        .flat_map(|id| index.name_chain(id))
                        .filter(|&e| !index.is_synthetic(e)),
                )
            })
            .reduce(|| 0, u64::saturating_add)
    })
}

/// The ids of `matched` in name order, by folded bytes then id, or the
/// reverse for a descending sort, made as they are read: a broad query's
/// first page reads a few names of the order, not all of it. A sparse set is
/// sorted directly; a dense one is read off the index's name order and
/// merged with the few matched names still waiting unranked, so a broad query
/// never sorts a million names.
fn names_in_order<'a>(
    index: &'a VolumeIndex,
    matched: &'a NameSet,
    dir: SortDir,
) -> Box<dyn Iterator<Item = NameId> + 'a> {
    let key = move |id: NameId| (index.name_folded(id), id);
    let desc = dir == SortDir::Desc;
    let order = index.name_order();
    if order.is_empty() || matched.len * 8 < order.len() {
        let mut ids: Vec<NameId> = matched.ids().collect();
        ids.sort_unstable_by(|&a, &b| key(a).cmp(&key(b)));
        if desc {
            ids.reverse();
        }
        return Box::new(ids.into_iter());
    }
    let mut waiting: Vec<NameId> = index
        .name_unranked()
        .iter()
        .copied()
        .filter(|&id| matched.contains(id))
        .collect();
    waiting.sort_unstable_by(|&a, &b| key(a).cmp(&key(b)));
    if desc {
        waiting.reverse();
    }
    let ranked: Box<dyn Iterator<Item = NameId> + 'a> = if desc {
        Box::new(order.iter().rev().copied())
    } else {
        Box::new(order.iter().copied())
    };
    let mut ranked = ranked.filter(move |&id| matched.contains(id)).peekable();
    let mut waiting = waiting.into_iter().peekable();
    Box::new(std::iter::from_fn(move || {
        match (ranked.peek().copied(), waiting.peek().copied()) {
            (Some(r), Some(w)) if (key(w) < key(r)) != desc => waiting.next(),
            (Some(_), _) => ranked.next(),
            (None, _) => waiting.next(),
        }
    }))
}

/// Rows `[start, end)` of a name-level query: walks the matched names in
/// name order (reversed for a descending sort) and each name's same-name
/// chain, its entries that are `listed` (not synthetic, and of the query's
/// kind when it keeps one), and stops at `end`. Names with equal folded
/// bytes form one group whose entries interleave by idx (descending when the
/// sort is), exactly the `(folded name, idx)` order the candidate comparator
/// produces. Stamped out per `listed`, so that a query with no kind lists
/// a chain as v0.6.0 did.
fn name_level_refs(
    index: &VolumeIndex,
    matched: &NameSet,
    dir: SortDir,
    start: usize,
    end: usize,
    listed: impl Fn(EntryIdx) -> bool,
) -> Vec<HitRef> {
    if start >= end {
        return Vec::new();
    }
    let mut names = names_in_order(index, matched, dir).peekable();
    let mut refs = Vec::with_capacity((end - start).min(1 << 16));
    let mut group: Vec<EntryIdx> = Vec::new();
    let mut seen = 0usize;
    while seen < end
        && let Some(first) = names.next()
    {
        let folded = index.name_folded(first);
        group.clear();
        group.extend(index.name_chain(first).filter(|&e| listed(e)));
        while let Some(id) = names.next_if(|&id| index.name_folded(id) == folded) {
            group.extend(index.name_chain(id).filter(|&e| listed(e)));
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
    }
    refs
}

/// The most rows a page can end at to be gathered by [`top_k_refs`]: each
/// thread holds up to twice this many candidates, and past it collecting
/// every match and sorting once costs less.
const TOP_K_MAX: usize = 10_000;

/// Whether the page `[offset, offset + limit)` of `parsed` can be gathered
/// by [`top_k_refs`], the key `sort` asks for being one entry key: a name,
/// size or date sort of an unscoped query that a prefilter or a union of
/// needles drives, with no path to test and no entry without one.
#[allow(clippy::too_many_arguments)]
fn top_k_applies(
    index: &VolumeIndex,
    parsed: &ParsedQuery,
    compiled: &CompiledQuery,
    scope: Option<EntryIdx>,
    sort: SortSpec,
    offset: u32,
    limit: Option<u32>,
) -> bool {
    sort.key != SortKey::Path
        && limit.is_some_and(|l| (offset as usize).saturating_add(l as usize) <= TOP_K_MAX)
        && scope.is_none()
        && compiled.source.is_none()
        && !compiled.needles(parsed).is_empty()
        && compiled.volume_admits
        && !compiled.tests_paths()
        && !index.may_have_unpresentable()
}

/// How many candidates a buffer of [`top_k_refs`] has room for at first.
const BEST_ROOM: usize = 4096;

/// The best candidates one thread of [`top_k_refs`] was offered.
struct Best {
    kept: Vec<Candidate>,
    /// The worst candidate kept at the last prune: one that sorts after it
    /// can no longer be among the best `want` this buffer was offered.
    cutoff: Option<Candidate>,
}

/// The page `[offset, offset + limit)` of a query [`top_k_applies`] to,
/// gathered without collecting every match: the names are matched in chunks
/// across cores, as in [`match_names`], and each thread keeps the best
/// `offset + limit` entries of the chunks it ran, pruning back to them each
/// time its buffer holds twice as many. The buffers are allocated by the
/// caller, one per thread rather than per chunk, with room for at most
/// [`BEST_ROOM`] candidates, so a rare term asking for a long page reserves
/// little; a buffer whose chunks match more grows on its thread, up to twice
/// [`TOP_K_MAX`]. Every match is still counted, and summed
/// when `sum_sizes` asks, so the totals are the scan's, and the page is the
/// scan's too: the same candidates, ordered by the same comparator. Fails
/// with [`Stopped`] when the query's budget is spent before the end.
#[allow(clippy::too_many_arguments)]
fn top_k_refs(
    index: &VolumeIndex,
    parsed: &ParsedQuery,
    compiled: &CompiledQuery,
    sort: SortSpec,
    offset: u32,
    limit: u32,
    sum_sizes: bool,
) -> Result<RefOutcome, Stopped> {
    use rayon::prelude::*;
    let needles = compiled.needles(parsed);
    let want = offset as usize + limit as usize;
    let names = index.name_count();
    let per = chunk_words(names);
    let chunks = names.div_ceil(per * 64);
    let bests: Vec<Mutex<Best>> = (0..chunks.min(name_threads()))
        .map(|_| {
            Mutex::new(Best {
                kept: Vec::with_capacity((2 * want).min(BEST_ROOM)),
                cutoff: None,
            })
        })
        .collect();
    let cmp = |a: &Candidate, b: &Candidate| cmp_candidate(index, sort, a, b);
    let scan_chunk = |i: usize| -> (u64, u64) {
        // A thread runs one chunk at a time and calls into no pool while it
        // does, so its buffer is free unless there are fewer buffers than
        // threads.
        let slot = rayon::current_thread_index().unwrap_or(0) % bests.len();
        let mut best = bests[slot].lock().unwrap_or_else(PoisonError::into_inner);
        let Best { kept, cutoff } = &mut *best;
        let mut scratch = Scratch::default();
        let mut scope_memo = FxHashMap::default();
        let mut dir_paths = FxHashMap::default();
        let mut no_folders = None;
        let mut watch = Watch::new(compiled.budget);
        let (mut count, mut size) = (0u64, 0u64);
        let ids = chunk_ids(i, per, names);
        let mut seen = Seen::of(needles, &ids);
        let mut hit = |id: NameId| {
            if let Some(seen) = seen.as_mut()
                && !seen.first(id)
            {
                return;
            }
            // As in `match_names`: only testing a name costs more with more
            // terms. An entry below costs little whatever the query: no query
            // here tests a path, and an expression's terms on the entry are
            // kind, size and date compares, at most the term cap of them.
            if !compiled.prefilter_decides_name
                && (watch.tick() || !name_matches(index, parsed, compiled, id, &mut scratch))
            {
                return;
            }
            for idx in index.name_chain(id) {
                if index.is_synthetic(idx) {
                    continue;
                }
                let Some(mut c) = evaluate(
                    index,
                    parsed,
                    compiled,
                    idx,
                    None,
                    &mut scope_memo,
                    &mut dir_paths,
                    &mut no_folders,
                    true,
                    &mut scratch,
                ) else {
                    continue;
                };
                count += 1;
                if sum_sizes {
                    size = size.saturating_add(files_size(index, std::iter::once(idx)));
                }
                if want == 0 {
                    continue;
                }
                c.key = entry_key(index, sort, idx);
                if kept.len() == 2 * want {
                    kept.select_nth_unstable_by(want - 1, cmp);
                    kept.truncate(want);
                    *cutoff = Some(kept[want - 1]);
                }
                if cutoff.is_some_and(|cut| cmp(&c, &cut).is_gt()) {
                    continue;
                }
                kept.push(c);
            }
        };
        for needle in needles {
            for_each_prefilter_hit_in(index, needle, ids.clone(), &mut hit);
        }
        (count, size)
    };
    let (total, total_size) = on_name_pool(chunks, || {
        (0..chunks)
            .into_par_iter()
            .map(scan_chunk)
            .reduce(|| (0, 0), |a, b| (a.0 + b.0, a.1.saturating_add(b.1)))
    });
    if compiled.budget.spent() {
        return Err(Stopped);
    }
    let bests: Vec<Best> = bests
        .into_iter()
        .map(|best| best.into_inner().unwrap_or_else(PoisonError::into_inner))
        .collect();
    let mut kept: Vec<Candidate> = Vec::with_capacity(bests.iter().map(|b| b.kept.len()).sum());
    for best in &bests {
        kept.extend_from_slice(&best.kept);
    }
    drop(bests);
    let (start, end) = page_bounds(total as usize, offset, Some(limit));
    order_top(index, &mut kept, sort, end);
    Ok(RefOutcome {
        total,
        refs: kept[start..end]
            .iter()
            .map(|c| HitRef {
                idx: c.idx,
                frn: index.entry(c.idx).frn(),
            })
            .collect(),
        total_size: sum_sizes.then_some(total_size),
    })
}

/// A folder-scoped query walks the scope's subtree instead of scanning the
/// whole volume, so a Downloads-sized scope costs the same on any disk: the
/// old path cost ~300 ms on a broad query over 3.9M entries because every
/// candidate on the volume paid an ancestry check. The walk goes on
/// unconditionally up to this many visited entries, and past it only while
/// it has visited no more entries than the scan would check (see
/// [`scan_cost`]): a rare term in a big folder scans the names instead, while
/// a broad term, or a filter with no name to scan for, walks the folder. An
/// entry walked and an entry scanned are weighed alike: on a real volume, in
/// MFT order, the walk stayed the cheaper even for a folder holding two thirds
/// of it.
const SUBTREE_WALK_MAX: usize = 200_000;

/// How many entries the scan of `parsed` checks, for the scope walk to weigh
/// itself against (see [`SUBTREE_WALK_MAX`]): those bearing a name the
/// prefilter finds, whose ids go to `hits` so the scan does not search the
/// names again; none for a query that walks below the names holding a piece
/// of its path, whose cost shows only by walking, so the scope walk gives up
/// at its floor as it always has; or else every entry.
fn scan_cost(
    index: &VolumeIndex,
    parsed: &ParsedQuery,
    compiled: &CompiledQuery,
    hits: &mut Option<Vec<NameId>>,
) -> usize {
    let needles = compiled.needles(parsed);
    if needles.is_empty() {
        return if compiled.path_piece.is_some() {
            0
        } else {
            index.len()
        };
    }
    let mut ids = Vec::new();
    let mut entries = 0usize;
    let mut seen = Seen::of(needles, &(0..index.name_count()));
    for needle in needles {
        for_each_prefilter_hit(index, needle, |id| {
            if seen.as_mut().is_none_or(|seen| seen.first(id)) {
                entries += index.name_refcount(id) as usize;
                ids.push(id);
            }
        });
    }
    *hits = Some(ids);
    entries
}

/// Collects every match of `parsed` under `scope`, in scan order.
fn scan(
    index: &VolumeIndex,
    parsed: &ParsedQuery,
    compiled: &CompiledQuery,
    scope: Option<EntryIdx>,
) -> Vec<Candidate> {
    scan_with(index, parsed, compiled, scope, SUBTREE_WALK_MAX)
}

/// [`scan`], with a scope walked unconditionally up to `walk_floor` entries
/// instead of [`SUBTREE_WALK_MAX`], so a test can make a small tree decide
/// between walking and scanning.
fn scan_with(
    index: &VolumeIndex,
    parsed: &ParsedQuery,
    compiled: &CompiledQuery,
    scope: Option<EntryIdx>,
    walk_floor: usize,
) -> Vec<Candidate> {
    if !compiled.volume_admits {
        return Vec::new();
    }
    // Built only when a full path has something left to decide.
    let mut folders = compiled.tests_paths().then(|| {
        FolderPaths::new(
            index,
            &compiled.prefix,
            !parsed.match_case,
            &compiled.raw_prefix,
        )
    });
    let mut scope_memo: FxHashMap<EntryIdx, bool> = FxHashMap::default();
    // Each parent directory's reconstructed path, or None when it has none (its
    // ancestry ends in lost+found, or hit the hop cap), for the presentability
    // check in `evaluate`.
    let mut dir_paths: FxHashMap<EntryIdx, Option<Vec<u8>>> = FxHashMap::default();
    let mut candidates: Vec<Candidate> = Vec::new();
    // Per-candidate scratch buffers, allocated once and reused across the scan
    // so wildcard decoding and extension folding don't heap-allocate per hit.
    let mut scratch = Scratch::default();
    // Each path below stops testing entries once the budget is spent, and
    // the caller refuses what it found.
    let mut watch = Watch::new(compiled.budget);

    // Folder scope: walk the scope subtree via the child chains, so cost
    // tracks the folder, not the volume. Membership is by construction, so
    // `evaluate` runs without the scope (its ancestry walk is the expensive
    // part this path exists to avoid); every other predicate is identical to
    // the scan paths below. A scope that grows costlier to walk than the scan
    // aborts to the scan (see [`SUBTREE_WALK_MAX`]).
    let mut subtree_done = false;
    let mut prefilter_hits: Option<Vec<NameId>> = None;
    if let Some(root) = scope {
        let mut visited = 0usize;
        let mut cap = walk_floor;
        let mut priced = false;
        subtree_done = true;
        for idx in index.subtree_entries(root) {
            visited += 1;
            if visited > cap && !priced {
                priced = true;
                cap = cap.max(scan_cost(index, parsed, compiled, &mut prefilter_hits));
            }
            if visited > cap {
                candidates.clear();
                subtree_done = false;
                break;
            }
            if watch.tick() {
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
                &mut scratch,
            ) {
                candidates.push(c);
            }
        }
    }

    // Fast path: case-insensitive query with a usable literal → scan the
    // folded arena for the prefilter literal, or each needle of a union, and
    // verify the rest per hit.
    if subtree_done {
        // Scoped walk answered the query; the scan paths are skipped.
    } else if let Some(Source::Children(dirs)) = &compiled.source {
        // A top-level `parent:` term: only those folders' children can
        // match, a few entries however big the volume.
        scan_children(
            index,
            parsed,
            compiled,
            dirs,
            scope,
            &mut watch,
            &mut scope_memo,
            &mut dir_paths,
            &mut folders,
            &mut scratch,
            &mut candidates,
        );
    } else if !compiled.needles(parsed).is_empty() {
        // The scan cost is proportional to the unique-name bytes (~1/3 of
        // the total on a real volume); a hit fans out to every entry bearing
        // the name through its same-name chain. The chains are exact, so no
        // per-candidate staleness guard is needed. A scope walk that priced
        // this scan already found the names, each once.
        let needles = compiled.needles(parsed);
        let mut seen = Seen::of(needles, &(0..index.name_count()));
        let mut on_hit = |id: NameId| {
            if let Some(seen) = seen.as_mut()
                && !seen.first(id)
            {
                return;
            }
            for idx in index.name_chain(id) {
                if watch.tick() {
                    return;
                }
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
                    &mut scratch,
                ) {
                    candidates.push(c);
                }
            }
        };
        match prefilter_hits {
            Some(ids) => ids.into_iter().for_each(on_hit),
            None => {
                for needle in needles {
                    for_each_prefilter_hit(index, needle, &mut on_hit);
                }
            }
        }
    } else if let Some(piece) = compiled.path_piece.as_deref()
        && walk_below_names(
            index,
            piece,
            index.len() / PATH_WALK_SHARE,
            compiled.budget,
            |idx| {
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
                    &mut scratch,
                ) {
                    candidates.push(c);
                }
            },
        )
    {
        // Every match is at or below a name holding the piece, and the walk
        // visited each of those entries once.
    } else if let Some(set) = &compiled.related {
        // A top-level term on related entries: only the entries its folders give can
        // match, each once. (A path walk that grew past its share may have
        // left some.)
        candidates.clear();
        set.for_each_candidate(index, |idx| {
            if watch.tick() {
                return false;
            }
            if !index.is_synthetic(idx)
                && let Some(c) = evaluate(
                    index,
                    parsed,
                    compiled,
                    idx,
                    scope,
                    &mut scope_memo,
                    &mut dir_paths,
                    &mut folders,
                    false,
                    &mut scratch,
                )
            {
                candidates.push(c);
            }
            true
        });
    } else if compiled.names_first {
        // Something to test of the name: the names come first.
        candidates.clear();
        scan_names_first(
            index,
            parsed,
            compiled,
            scope,
            &mut watch,
            &mut scope_memo,
            &mut dir_paths,
            &mut folders,
            &mut scratch,
            &mut candidates,
        );
    } else {
        // Slow path: case-sensitive, or no positive substring term (only
        // wildcards / filters / path terms), or a path walk that grew past
        // its share. Iterate every live entry.
        candidates.clear();
        for idx in index.live_entries() {
            if watch.tick() {
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
                scope,
                &mut scope_memo,
                &mut dir_paths,
                &mut folders,
                false,
                &mut scratch,
            ) {
                candidates.push(c);
            }
        }
    }

    candidates
}

/// The candidates of a query with a top-level `parent:` term (see
/// [`scan_with`]): the children of its folders. Out of line, as is
/// [`scan_names_first`], so that the scans every query had before S2 keep
/// their code: each call site of [`evaluate`] in one function weighs on
/// whether the compiler inlines it into the others.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn scan_children(
    index: &VolumeIndex,
    parsed: &ParsedQuery,
    compiled: &CompiledQuery,
    dirs: &[EntryIdx],
    scope: Option<EntryIdx>,
    watch: &mut Watch,
    scope_memo: &mut FxHashMap<EntryIdx, bool>,
    dir_paths: &mut FxHashMap<EntryIdx, Option<Vec<u8>>>,
    folders: &mut Option<FolderPaths>,
    scratch: &mut Scratch,
    candidates: &mut Vec<Candidate>,
) {
    for &dir in dirs {
        for idx in index.children(dir) {
            if watch.tick() {
                return;
            }
            if index.is_synthetic(idx) {
                continue;
            }
            if let Some(c) = evaluate(
                index, parsed, compiled, idx, scope, scope_memo, dir_paths, folders, false, scratch,
            ) {
                candidates.push(c);
            }
        }
    }
}

/// The candidates of a query with something to test of the name and no
/// needle to scan for (see [`CompiledQuery::names_first`]): the names that
/// pass the name's tests and the projection come first, from a pass across
/// cores, and only their entries are evaluated, their names settled. When
/// they bear more than a share of the volume, every entry is read in order
/// instead, those of a name the pass turned away skipped. Out of line, as
/// [`scan_children`].
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn scan_names_first(
    index: &VolumeIndex,
    parsed: &ParsedQuery,
    compiled: &CompiledQuery,
    scope: Option<EntryIdx>,
    watch: &mut Watch,
    scope_memo: &mut FxHashMap<EntryIdx, bool>,
    dir_paths: &mut FxHashMap<EntryIdx, Option<Vec<u8>>>,
    folders: &mut Option<FolderPaths>,
    scratch: &mut Scratch,
    candidates: &mut Vec<Candidate>,
) {
    let (matched, bearing) = match_scan_names(index, parsed, compiled);
    let mut visit = |idx: EntryIdx, candidates: &mut Vec<Candidate>| {
        if index.is_synthetic(idx) {
            return;
        }
        if let Some(c) = evaluate(
            index, parsed, compiled, idx, scope, scope_memo, dir_paths, folders, true, scratch,
        ) {
            candidates.push(c);
        }
    };
    if bearing as usize <= index.len() / PATH_WALK_SHARE {
        for id in matched.ids() {
            for idx in index.name_chain(id) {
                if watch.tick() {
                    return;
                }
                visit(idx, candidates);
            }
        }
    } else {
        for idx in index.live_entries() {
            if watch.tick() {
                return;
            }
            if matched.contains(index.name_id(idx)) {
                visit(idx, candidates);
            }
        }
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
fn order(index: &VolumeIndex, candidates: &mut [Candidate], sort: SortSpec, end: usize) {
    if sort.key == SortKey::Path {
        path_keys(index, candidates);
    } else {
        for c in candidates.iter_mut() {
            c.key = entry_key(index, sort, c.idx);
        }
    }
    order_top(index, candidates, sort, end);
}

/// The key a name, size or date sort orders entry `idx` by. A path sort's
/// keys need every candidate's folder ranked first, which [`path_keys`]
/// does, so a path sort is never asked here.
fn entry_key(index: &VolumeIndex, sort: SortSpec, idx: EntryIdx) -> u64 {
    match sort.key {
        // Two integers per comparison instead of two folded names: see
        // `NameStore::rank_key`. Names still waiting outside the order get a
        // key between their neighbours, and ties fall back to the bytes.
        SortKey::Name => index.name_rank_key(index.name_id(idx)),
        SortKey::Size => size_key(index.entry(idx).size(), sort.dir),
        SortKey::DateModified => mtime_key(index.entry(idx).mtime(), sort.dir),
        SortKey::Path => 0,
    }
}

/// The key a size sort in direction `dir` orders an entry of `size` by. An
/// entry with no size, a folder or a file not read yet, comes after every
/// entry with one, whichever way the sort runs. The engine and the daemon's
/// merge of several volumes key by this one function.
pub fn size_key(size: Option<u64>, dir: SortDir) -> u64 {
    // The index stores an unknown size as `u64::MAX`, so a known one is below.
    unknown_last(size, dir)
}

/// The key a date sort in direction `dir` orders an entry of `mtime` by:
/// the mtime as a `u64` in the same order, an unknown one last, as in
/// [`size_key`].
pub fn mtime_key(mtime: Option<i64>, dir: SortDir) -> u64 {
    // Flipping the sign bit orders an `i64` as a `u64`. The index stores an
    // unknown date as `i64::MIN`, so a known one maps to 1 or more, and one
    // less keeps it below `u64::MAX`.
    unknown_last(mtime.map(|t| ((t as u64) ^ (1 << 63)) - 1), dir)
}

/// `known`, below `u64::MAX`, as a key that orders like it, or for no value
/// the key that sorts last in `dir`: the largest ascending, and the smallest
/// descending, since [`cmp_candidate`] reverses a descending comparison.
fn unknown_last(known: Option<u64>, dir: SortDir) -> u64 {
    match (known, dir) {
        (Some(k), SortDir::Asc) => k,
        (Some(k), SortDir::Desc) => k + 1,
        (None, SortDir::Asc) => u64::MAX,
        (None, SortDir::Desc) => 0,
    }
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
    scratch: &mut Scratch,
) -> Option<Candidate> {
    let entry = index.entry(idx);
    if !kind_matches(compiled.kind, entry.is_dir()) {
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
    if !name_decided && !name_matches(index, parsed, compiled, index.name_id(idx), scratch) {
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
            if !folders.full_path_matches(index, idx, compiled, &mut scratch.cps) {
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

    // The expression's terms on the entry go last, once the entry is known
    // to have a path: a NOT must not admit an entry the page cannot list.
    if let Some(e) = &compiled.expr_entry
        && !entry_expr_matches(
            e,
            index,
            idx,
            parsed.match_case,
            compiled.budget,
            folders,
            scratch,
        )
    {
        return None;
    }

    Some(Candidate { idx, key: 0 })
}

pub(super) fn kind_matches(kind: Option<Kind>, is_dir: bool) -> bool {
    match kind {
        None => true,
        Some(Kind::File) => !is_dir,
        Some(Kind::Folder) => is_dir,
    }
}

/// A size filter excludes entries whose size is unknown (not yet enriched):
/// we cannot confirm the predicate, so we do not claim a match.
pub(super) fn size_matches(range: SizeRange, size: Option<u64>) -> bool {
    size.is_some_and(|s| range.contains(s))
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
pub(super) const CACHED_PATH_MAX: usize = 1024;

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
    /// For each path wildcard, whether every entry of the folder `needs_for`
    /// matches it, or `None` when its folder does not decide it (see
    /// `CompiledQuery::path_glob_leads`).
    verdicts: Vec<Option<bool>>,
    needs_for: Option<EntryIdx>,
    /// The mount prefix as typed: the prefix of the paths as stored.
    stored_prefix: Vec<u8>,
    /// When the paths here are folded, the paths as stored, which a term
    /// that matches text as stored asks for (`regex:`, `case:`, `len:`),
    /// each built from its folder's as these are, instead of walked up to
    /// the root for every entry. Made when first asked for; the two then
    /// share the caps one had.
    stored: Option<Box<FolderPaths>>,
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
    /// terms are; `stored_prefix` is the mount prefix as typed.
    fn new(index: &VolumeIndex, prefix: &[u8], fold: bool, stored_prefix: &[u8]) -> Self {
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
            verdicts: Vec::new(),
            needs_for: None,
            stored_prefix: stored_prefix.to_vec(),
            stored: None,
        };
        this.restart();
        this
    }

    /// [`Self::with_full_path`] of the path as stored when `stored`, else
    /// as the query compares it.
    fn with_full_path_as<R>(
        &mut self,
        index: &VolumeIndex,
        idx: EntryIdx,
        stored: bool,
        f: impl FnOnce(&[u8]) -> R,
    ) -> Option<R> {
        if !stored || !self.fold {
            return self.with_full_path(index, idx, f);
        }
        if self.stored.is_none() {
            // Half the caps each, so that the two hold what one did.
            self.max_bytes /= 2;
            self.max_folders /= 2;
            if self.arena.capacity() > self.max_bytes || self.known.len() > self.max_folders {
                self.restart();
                self.arena.shrink_to(self.max_bytes);
                self.known.shrink_to(self.max_folders);
            }
            let mut paths =
                FolderPaths::new(index, &self.stored_prefix, false, &self.stored_prefix);
            paths.max_bytes = self.max_bytes;
            paths.max_folders = self.max_folders;
            self.stored = Some(Box::new(paths));
        }
        self.stored
            .as_deref_mut()
            .and_then(|paths| paths.with_full_path(index, idx, f))
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
        // The path wildcards the folder decides (bit `i` for the `i`th),
        // unless a separator in the name makes the whole path decide.
        let mut decided = 0u64;
        if !whole {
            for (i, verdict) in self.verdicts.iter().enumerate() {
                match verdict {
                    Some(false) => return false,
                    Some(true) if i < 64 => decided |= 1 << i,
                    _ => {}
                }
            }
        }
        let open_globs = compiled.path_globs.len() - decided.count_ones() as usize;
        if !whole && open_globs == 0 {
            return true;
        }
        self.with_full_path(index, idx, |full| {
            (!whole || compiled.path_finders.iter().all(|f| f.find(full).is_some()))
                && compiled.path_wildcard_ends.iter().all(|(lead, trail)| {
                    lead.as_deref().is_none_or(|lead| full.starts_with(lead))
                        && full.ends_with(trail)
                })
                && (open_globs == 0
                    // Decoding and matching a path of megabytes, which no
                    // real path is, checks the budget first, as each term's
                    // search of such a path does in `decide_needs`.
                    || !(full.len() > CACHED_PATH_MAX && compiled.budget.check()) && {
                        decode_into(full, cp_buf);
                        compiled.path_globs.iter().enumerate().all(|(i, w)| {
                            (i < 64 && decided & (1 << i) != 0)
                                || w.matches_decoded(cp_buf, compiled.budget)
                        })
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
            // Every term searches the whole path at each new folder, which on
            // a path of megabytes is the costliest step of a candidate. A
            // spent budget says no name can complete the term, and the search
            // stops at its next check.
            if before_name.len() > CACHED_PATH_MAX && compiled.budget.check() {
                self.needs.push(Need::Never);
                continue;
            }
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
        // A wildcard of literals and a trailing `\**` matches where a part
        // starts with its literals, which end with a separator. A name holds
        // none, so they lie before the name: at the start of the path, or
        // after a separator of the folder's part. Literals holding the
        // drive's `:` must start the path (see `anchored_lead`).
        self.verdicts.clear();
        for sep_lead in &compiled.path_glob_leads {
            self.verdicts.push(match sep_lead {
                Some(_) if !open => None,
                Some(_) if before_name.len() > CACHED_PATH_MAX && compiled.budget.check() => {
                    Some(false)
                }
                Some(sep_lead) => Some(
                    before_name.starts_with(&sep_lead[1..])
                        || (!sep_lead.contains(&b':')
                            && memmem::find(before_name, sep_lead).is_some()),
                ),
                None => None,
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
/// Returns `false`, having visited some entries, once more than `max_visits`
/// entries would be visited. It stops as well once `budget` is spent, which
/// the caller finds out from the budget: the climb from each top can cost
/// thousands of hops, and it runs even for a top too deep to visit.
fn walk_below_names(
    index: &VolumeIndex,
    piece: &[u8],
    max_visits: usize,
    budget: &Budget,
    mut visit: impl FnMut(EntryIdx),
) -> bool {
    let mut bearing = 0usize;
    let mut tops: Vec<EntryIdx> = Vec::new();
    for_each_prefilter_hit(index, piece, |id| {
        bearing += index.name_refcount(id) as usize;
        if bearing <= max_visits {
            tops.extend(index.name_chain(id));
        }
    });
    if bearing > max_visits {
        return false;
    }
    // Sorted so that a climb can ask whether a folder is a top.
    tops.sort_unstable();
    let mut watch = Watch::new(budget);
    let mut visited = 0usize;
    for &top in &tops {
        if watch.tick() {
            return true;
        }
        if !walk_starts_at(index, top, &tops) {
            continue;
        }
        for idx in core::iter::once(top).chain(index.subtree_entries(top)) {
            visited += 1;
            if visited > max_visits {
                return false;
            }
            if watch.tick() {
                return true;
            }
            visit(idx);
        }
    }
    true
}

/// Whether [`walk_below_names`] walks from `top`: its chain reaches a top
/// other than lost+found within [`MAX_PATH_HOPS`] entries, and passes no
/// other of `tops` (sorted), whose walk covers it.
pub(super) fn walk_starts_at(index: &VolumeIndex, top: EntryIdx, tops: &[EntryIdx]) -> bool {
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
    // holds what `entry_key` or `path_keys` gave for this sort. The idx tiebreak makes the
    // order total, so an unstable sort is deterministic.
    let by_name = || index.folded_name(a.idx).cmp(index.folded_name(b.idx));
    let _ = sort.key; // every key orders the same way: `key` was filled for it
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
/// walking components from the volume root: case- and accent-insensitively
/// via the folded directory index, or case-sensitively against raw names
/// when `match_case`. Returns `None` if any component is missing. Where
/// sibling folders fold alike (`café` and `cafe`), the one whose accents
/// the component has wins, and else the first.
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
            let folded = fold(comp);
            let mut found = index.child_dirs(cur, &folded);
            let first = found.next()?;
            cur = match found.next() {
                None => first,
                Some(second) => {
                    let exact = crate::fold::fold_exact(comp);
                    [first, second]
                        .into_iter()
                        .chain(found)
                        .find(|&d| crate::fold::fold_exact(index.entry(d).name()) == exact)
                        .unwrap_or(first)
                }
            };
        }
        return Some(cur);
    }

    // Case-sensitive scopes are rare (the persistent index is folded), so we do
    // not build a whole-volume raw-name map (one key Vec per directory) per
    // query. Walk each component through its folder's child chain, comparing
    // raw names directly (no allocation), stopping at the first matching
    // sibling. Each folder on the path is read once, so a whole path costs at
    // most one pass over the volume, however many components it has; a scan of
    // the live entries per component held the lock for a minute on a chain of
    // 16,000 folders. NOTE: do not reuse the folded
    // `child_dir` walk with a post-hoc raw-name check: `dir_children` keeps a
    // single entry per (parent, folded name), so it cannot represent
    // case-variant siblings and would miss a legitimately case-matching sibling
    // that folding collapsed.
    let mut cur = index.root();
    for comp in components {
        cur = index.children(cur).find(|&idx| {
            let e = index.entry(idx);
            !index.is_synthetic(idx) && e.is_dir() && e.name() == comp
        })?;
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

    /// A budget for the tests that never runs out.
    static UNLIMITED: Budget<'static> = Budget::unlimited();

    /// [`page_refs`] with a budget that never runs out.
    #[allow(clippy::too_many_arguments)]
    fn unlimited_refs(
        index: &VolumeIndex,
        mount_prefix: &str,
        parsed: &ParsedQuery,
        scope: Option<EntryIdx>,
        sort: SortSpec,
        offset: u32,
        limit: Option<u32>,
        sum_sizes: bool,
        fast: bool,
    ) -> RefOutcome {
        page_refs(
            index,
            mount_prefix,
            parsed,
            scope,
            sort,
            offset,
            limit,
            sum_sizes,
            fast,
            &UNLIMITED,
        )
        .unwrap()
    }

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

    /// Every way a query can search checks its budget as it goes, wherever
    /// more terms can make it cost more, so none holds the lock past it, and
    /// a search the budget stopped fails rather than answering with what it
    /// found so far.
    #[test]
    fn a_spent_budget_stops_every_way_of_searching() {
        use std::sync::atomic::AtomicBool;
        use std::time::{Duration, Instant};
        let mut idx = sample_index();
        // Enough other files that the walk below `docs` stays within its
        // share of the volume, instead of giving up for the scan of every
        // entry.
        for i in 0..20 {
            idx.insert_enum(&enum_rec(frn(200 + i), NTFS_ROOT_FRN, "filler.bin", false));
        }
        let docs = idx.head_of(frn(100));
        let name = SortSpec::default_for(SortKey::Name);
        let size = SortSpec::default_for(SortKey::Size);
        let cancelled = AtomicBool::new(true);
        for (way, q, scope, sort, limit) in [
            ("distinct names", "*e*", None, name, None),
            ("best of the matches", "*e*", None, size, Some(10)),
            ("prefilter hits", "e size:>0", None, name, None),
            ("every entry", "size:>0", None, name, None),
            ("below a path piece", r"docs\", None, name, None),
            ("every full path", r"*\*", None, name, None),
            ("a folder", "", docs, name, None),
            ("an expression on names", "!zzz", None, name, None),
            ("an expression on entries", "!zzz|file:", None, name, None),
            ("an expression on paths", r"zzz|docs\", None, name, None),
            ("the best of an expression", "e !zzz", None, size, Some(10)),
            ("a sub-path automaton", r"**\*e*", None, name, None),
            ("a regular expression", "regex:e", None, name, None),
            (
                "a regular expression on paths",
                r"regex:docs\\",
                None,
                name,
                None,
            ),
        ] {
            let parsed = parse_query(q).unwrap();
            let run = |budget: &Budget| {
                page_refs(
                    &idx, r"C:\", &parsed, scope, sort, 0, limit, true, true, budget,
                )
            };
            let found = run(&UNLIMITED).unwrap();
            assert!(found.total > 0, "{way}: matches nothing");
            let spent = Budget::until(Instant::now() + Duration::from_secs(3600), &cancelled);
            assert_eq!(run(&spent), Err(Stopped), "{way}");
        }
    }

    /// A budget that a cancel flag, set already or later, spends.
    fn cancelled_by(cancel: &std::sync::atomic::AtomicBool) -> Budget<'_> {
        use std::time::{Duration, Instant};
        Budget::until(Instant::now() + Duration::from_secs(3600), cancel)
    }

    /// The climb from a top to the root costs thousands of hops and visits
    /// nothing when the top is too deep for a path, so it checks the budget
    /// itself: a million such tops held the lock for 100 s, and answered with
    /// no matches.
    #[test]
    fn a_spent_budget_stops_the_climb_from_a_top_too_deep_to_visit() {
        use std::sync::atomic::AtomicBool;
        let mut idx = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
        let mut parent = NTFS_ROOT_FRN;
        for i in 0..MAX_PATH_HOPS as u64 + 4 {
            idx.insert_enum(&enum_rec(frn(1000 + i), parent, "d", true));
            parent = frn(1000 + i);
        }
        idx.insert_enum(&enum_rec(frn(100), parent, "zq", true));
        let parsed = parse_query(r"zq\").unwrap();
        let run = |budget: &Budget| {
            page_refs(
                &idx,
                r"C:\",
                &parsed,
                None,
                SortSpec::default(),
                0,
                None,
                false,
                true,
                budget,
            )
        };
        assert_eq!(run(&UNLIMITED).unwrap().total, 0, "too deep to have a path");
        let cancel = AtomicBool::new(true);
        assert_eq!(run(&cancelled_by(&cancel)), Err(Stopped));
    }

    /// A walk under way ends soon after its budget is spent, instead of going
    /// through every entry below the name.
    #[test]
    fn a_spent_budget_ends_a_walk_under_way() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let mut idx = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
        idx.insert_enum(&enum_rec(frn(100), NTFS_ROOT_FRN, "zq", true));
        let files = 1000;
        for i in 0..files {
            idx.insert_enum(&enum_rec(frn(1000 + i), frn(100), "file", false));
        }
        let cancel = AtomicBool::new(false);
        let budget = cancelled_by(&cancel);
        let mut visits = 0;
        walk_below_names(&idx, b"zq", 10 * files as usize, &budget, |_| {
            visits += 1;
            cancel.store(true, Ordering::Relaxed);
        });
        assert!(budget.spent());
        assert!(visits < files, "visited {visits} of {files}");
    }

    /// Past [`CACHED_PATH_MAX`], each term's search of a folder's path checks
    /// the budget first: 256 terms over a path of megabytes cost a folder 0.9 s,
    /// and a real path never gets here.
    #[test]
    fn a_spent_budget_answers_for_each_term_on_a_long_path() {
        use std::sync::atomic::AtomicBool;
        let mut idx = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
        let long = "a".repeat(250);
        let mut parent = NTFS_ROOT_FRN;
        for i in 0..5 {
            idx.insert_enum(&enum_rec(frn(100 + i), parent, &long, true));
            parent = frn(100 + i);
        }
        idx.insert_enum(&enum_rec(frn(200), NTFS_ROOT_FRN, "aaaa", true));
        let parsed = parse_query(r"aaaa\").unwrap();
        let cancel = AtomicBool::new(true);
        let spent = cancelled_by(&cancel);
        let needs = |budget: &Budget, folder: Frn| {
            let compiled = CompiledQuery::new(&idx, &parsed, r"C:\", budget);
            let mut folders = FolderPaths::new(&idx, &compiled.prefix, true, &compiled.raw_prefix);
            folders.load(&idx, idx.head_of(folder).unwrap());
            folders.decide_needs(&compiled);
            folders.needs.clone()
        };
        let (deep, near) = (frn(104), frn(200));
        assert!(matches!(needs(&UNLIMITED, deep)[..], [Need::Nothing]));
        assert!(matches!(needs(&spent, deep)[..], [Need::Never]));
        assert!(matches!(needs(&spent, near)[..], [Need::Nothing]));
    }

    /// A case-sensitive scope follows the exact names down through the child
    /// chains: a sibling that differs by case is another folder, a file is no
    /// folder, and a missing name ends the walk.
    #[test]
    fn a_case_sensitive_scope_takes_the_exact_names() {
        let mut idx = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
        for r in [
            enum_rec(frn(100), NTFS_ROOT_FRN, "Docs", true),
            enum_rec(frn(101), NTFS_ROOT_FRN, "docs", true),
            enum_rec(frn(102), frn(100), "Sub", true),
            enum_rec(frn(103), frn(101), "sub", true),
            enum_rec(frn(104), frn(101), "file", false),
        ] {
            idx.insert_enum(&r);
        }
        let exact = |path: &[u8]| resolve_scope(&idx, path, true);
        assert_eq!(exact(b"Docs"), idx.head_of(frn(100)));
        assert_eq!(exact(b"docs"), idx.head_of(frn(101)));
        assert_eq!(exact(br"Docs\Sub"), idx.head_of(frn(102)));
        assert_eq!(exact(br"docs/sub"), idx.head_of(frn(103)));
        assert_eq!(exact(br"Docs\sub"), None);
        assert_eq!(exact(b"DOCS"), None);
        assert_eq!(exact(br"docs\file"), None);
        assert_eq!(exact(b""), Some(idx.root()));
    }

    /// Each live name holding the needle is reported once, however often it
    /// holds it, in id order, and a name right after a match or after a dead
    /// name is still found: the same names a check of every live name gives.
    /// A needle holding a NUL matches no name, though the separators and a
    /// dead name's zeroed bytes hold NULs.
    #[test]
    fn the_prefilter_reports_each_matching_name_once() {
        let mut idx = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
        let names = [
            "eee.txt", "x", "be", "ee", "zzz", "Gone.e", "eabc", "the end", "e",
        ];
        for (i, name) in names.iter().enumerate() {
            idx.insert_enum(&enum_rec(frn(100 + i as u64), NTFS_ROOT_FRN, name, false));
        }
        let mut gone = enum_rec(frn(105), NTFS_ROOT_FRN, "Gone.e", false);
        gone.reason = USN_REASON_FILE_DELETE;
        idx.apply_batch(&[gone]);
        for needle in [&b"e"[..], b"ee", b"zz", b"the end", b"q", b"\0", b"e\0b"] {
            let mut hits = Vec::new();
            for_each_prefilter_hit(&idx, needle, |id| hits.push(id));
            let every: Vec<NameId> = (0..idx.name_count() as NameId)
                .filter(|&id| idx.name_refcount(id) > 0)
                .filter(|&id| memmem::find(idx.name_folded(id), needle).is_some())
                .collect();
            assert_eq!(hits, every, "needle {:?}", String::from_utf8_lossy(needle));
        }
    }

    /// A hit maps to its name wherever it lies after the last one: in the
    /// next name, the one after, or thousands of names on (the gallop of
    /// `name_holding`), in the whole arena and in any range of ids of it,
    /// as the passes across cores split it, a dead name among them.
    #[test]
    fn the_prefilter_maps_hits_far_apart_in_any_range() {
        let mut idx = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
        let keyed = [
            0usize, 1, 2, 4, 7, 8, 9, 40, 41, 300, 301, 302, 1500, 2998, 2999,
        ];
        let name = |i: usize| {
            if keyed.contains(&i) {
                format!("n{i:04}KEY")
            } else {
                format!("n{i:04}")
            }
        };
        for i in 0..3000 {
            idx.insert_enum(&enum_rec(
                frn(100 + i as u64),
                NTFS_ROOT_FRN,
                &name(i),
                false,
            ));
        }
        let mut gone = enum_rec(frn(400), NTFS_ROOT_FRN, &name(300), false);
        gone.reason = USN_REASON_FILE_DELETE;
        idx.apply_batch(&[gone]);
        let names = idx.name_count();
        let every = |ids: Range<usize>| -> Vec<NameId> {
            ids.map(|id| id as NameId)
                .filter(|&id| idx.name_refcount(id) > 0)
                .filter(|&id| memmem::find(idx.name_folded(id), b"key").is_some())
                .collect()
        };
        assert_eq!(every(0..names).len(), keyed.len() - 1);
        for ids in [
            0..names,
            0..1,
            1..names,
            5..9,
            9..41,
            40..1501,
            302..303,
            1501..names,
            names - 1..names,
        ] {
            let mut got = Vec::new();
            for_each_prefilter_hit_in(&idx, b"key", ids.clone(), |id| got.push(id));
            assert_eq!(got, every(ids.clone()), "ids {ids:?}");
        }
    }

    proptest! {
        /// `name_holding` lands where a walk over every name from `from`
        /// does, whatever the names' lengths (empty ones too) and however
        /// far on the byte lies.
        #[test]
        fn name_holding_agrees_with_a_walk(
            lens in prop::collection::vec(0u16..6, 1..400),
            from in any::<prop::sample::Index>(),
            at in any::<prop::sample::Index>(),
        ) {
            let mut pairs = Vec::with_capacity(lens.len());
            let mut off = 0u32;
            for &len in &lens {
                pairs.push(NamePair {
                    folded: crate::index::NameRef { off, len },
                    ..NamePair::default()
                });
                off += u32::from(len) + 1;
            }
            let from = from.index(pairs.len());
            let start = pairs[from].folded.off as usize;
            let pos = start + at.index(off as usize - start);
            let mut want = from;
            while want + 1 < pairs.len() && pairs[want + 1].folded.off as usize <= pos {
                want += 1;
            }
            prop_assert_eq!(name_holding(&pairs, from, pos), want);
        }
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

    /// The summed size of `query`'s matches, as the daemon asks for it.
    fn total_size(idx: &VolumeIndex, query: &str, scope: Option<EntryIdx>) -> Option<u64> {
        let parsed = parse_query(query).unwrap();
        run_query_refs(
            idx,
            r"C:\",
            &parsed,
            scope,
            SortSpec::default(),
            0,
            Some(0),
            true,
            &UNLIMITED,
        )
        .unwrap()
        .total_size
    }

    /// es's `-get-total-size` sums the files it matches: a folder adds
    /// nothing, even one whose files match too, and a file whose size is not
    /// read yet adds nothing (its sentinel would add `u64::MAX`). Unasked,
    /// no sum is made.
    #[test]
    fn total_size_sums_the_matching_files_only() {
        let mut idx = sample_index();
        idx.insert_enum(&enum_rec(frn(105), NTFS_ROOT_FRN, "unsized.txt", false));
        let files = 4096 + 200 + 2 * 1024 * 1024;
        assert_eq!(total_size(&idx, "", None), Some(files));
        assert_eq!(
            total_size(&idx, "docs", None),
            Some(0),
            "a folder adds nothing"
        );
        assert_eq!(total_size(&idx, "unsized", None), Some(0));
        let docs = idx.head_of(frn(100));
        assert_eq!(
            total_size(&idx, "", docs),
            Some(4096 + 200),
            "the scope bounds it"
        );
        let parsed = parse_query("").unwrap();
        let unasked = run_query_refs(
            &idx,
            r"C:\",
            &parsed,
            None,
            SortSpec::default(),
            0,
            None,
            false,
            &UNLIMITED,
        )
        .unwrap();
        assert_eq!(unasked.total_size, None);
    }

    /// Each name of a hard-linked file is a row of its own, so each adds the
    /// file's size, and the total is what the listed sizes add up to.
    #[test]
    fn each_name_of_a_hard_link_adds_its_size() {
        use crate::layout::{LayoutFile, LayoutName};
        let mut idx = sample_index();
        idx.insert_enum(&enum_rec(frn(300), NTFS_ROOT_FRN, "twin1.bin", false));
        let name = |parent, name: &str| LayoutName {
            parent_frn: parent,
            name: name.as_bytes().to_vec(),
            name_lossy: false,
            dos_only: false,
        };
        idx.enrich(&LayoutFile {
            frn: frn(300),
            attributes: 0x20,
            size: Some(1000),
            mtime_ft: Some(1000),
            names: vec![
                name(NTFS_ROOT_FRN, "twin1.bin"),
                name(frn(100), "twin2.bin"),
            ],
        });
        for (query, rows, size) in [("twin", 2, 2000), ("twin2", 1, 1000)] {
            let parsed = parse_query(query).unwrap();
            let listed = run_query(&idx, r"C:\", &parsed, None, SortSpec::default(), 0, None);
            assert_eq!(listed.hits.len(), rows, "{query}");
            let shown: u64 = listed.hits.iter().filter_map(|h| h.size).sum();
            assert_eq!(shown, size, "{query}");
            assert_eq!(total_size(&idx, query, None), Some(size), "{query}");
        }
    }

    /// The sum covers the set the count does: files without a full path,
    /// under lost+found or in a parent cycle, add nothing.
    #[test]
    fn files_without_a_path_add_no_size() {
        let mut idx = index_with_orphans();
        for (f, size) in [(100, 1), (101, 2), (200, 100), (201, 200), (202, 400)] {
            idx.set_stat(frn(f), size, 0);
        }
        assert_eq!(total_size(&idx, "log", None), Some(3));

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
        idx.set_stat(frn(102), 50, 0);
        idx.set_stat(frn(103), 7, 0);
        assert!(idx.has_parent_cycle());
        assert_eq!(total_size(&idx, "x", None), Some(7));
    }

    /// The names of a few huge sparse files can claim more than `u64` holds:
    /// the sum stops at `u64::MAX` instead of wrapping, or panicking in a
    /// debug build.
    #[test]
    fn the_total_size_stops_at_u64_max() {
        let mut idx = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
        for f in [100, 101] {
            idx.insert_enum(&enum_rec(
                frn(f),
                NTFS_ROOT_FRN,
                &format!("huge{f}.bin"),
                false,
            ));
            idx.set_stat(frn(f), u64::MAX - 1, 0);
        }
        assert_eq!(total_size(&idx, "huge", None), Some(u64::MAX));
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
            ("dm:prevmonth", &[r"docs\notes.txt"][..]),
            // Rolling in Everything 1.5: from a month ago on.
            ("dm:lastmonth", &today[..]),
            // An age compares as one: older than an hour.
            ("dm:>1hour", &["docs", r"docs\notes.txt", "photo.JPG"][..]),
            ("dm:<2000", &["photo.JPG"][..]),
            ("dm:unknown", &[r"docs\sub"][..]),
            ("report dm:today", &[r"docs\sub\Report.PDF"][..]),
            ("notes dm:today", &[][..]),
            ("folder: dm:today", &["docs"][..]),
            ("dm:2026-10-01..2026-11-03", &[r"docs\notes.txt"][..]),
            // In an expression, a date is a term like any other: a NOT of
            // one admits the entries whose date is unknown.
            (
                "dm:today|notes",
                &["docs", r"docs\notes.txt", r"docs\sub\Report.PDF"][..],
            ),
            (
                "!dm:today",
                &[r"docs\notes.txt", r"docs\sub", "photo.JPG"][..],
            ),
            (
                "<notes dm:prevmonth>|dm:unknown",
                &[r"docs\notes.txt", r"docs\sub"][..],
            ),
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

    /// The dated sample plus an empty file and a file never read, whose size
    /// and date are both unknown.
    fn unknowns_index() -> VolumeIndex {
        use crate::query::dates::tests::ft;
        let (mut idx, _) = dated_index();
        idx.insert_enum(&enum_rec(frn(105), NTFS_ROOT_FRN, "empty.txt", false));
        idx.set_stat(frn(105), 0, ft("2026-11-03T00:00:00"));
        idx.insert_enum(&enum_rec(frn(106), NTFS_ROOT_FRN, "unread.bin", false));
        idx
    }

    /// A size or date sort lists the entries that have the value first, an
    /// empty file and a file dated 1601 among them, then those without it, a
    /// folder's size and a file not read yet, in either direction; a limited
    /// page is the start of that list.
    #[test]
    fn unknown_sizes_and_dates_sort_last_in_either_direction() {
        let idx = unknowns_index();
        let parsed = parse_query("").unwrap();
        let sizes = (
            &[
                "empty.txt",
                r"docs\notes.txt",
                r"docs\sub\Report.PDF",
                "photo.JPG",
            ][..],
            &["docs", r"docs\sub", "unread.bin"][..],
        );
        let dates = (
            &[
                "photo.JPG",
                r"docs\notes.txt",
                "empty.txt",
                "docs",
                r"docs\sub\Report.PDF",
            ][..],
            &[r"docs\sub", "unread.bin"][..],
        );
        for (key, (known, unknown)) in [(SortKey::Size, sizes), (SortKey::DateModified, dates)] {
            for dir in [SortDir::Asc, SortDir::Desc] {
                let mut want = known.to_vec();
                let mut last = unknown.to_vec();
                if dir == SortDir::Desc {
                    want.reverse();
                    last.reverse();
                }
                want.extend(last);
                let sort = SortSpec { key, dir };
                for k in 0..=want.len() {
                    let out = run_query(&idx, r"C:\", &parsed, None, sort, 0, Some(k as u32));
                    let got: Vec<String> = out
                        .hits
                        .iter()
                        .map(|h| crate::wtf8::to_string_lossy(&h.path))
                        .collect();
                    assert_eq!(got, want[..k], "{key:?} {dir:?} -n {k}");
                }
            }
        }
    }

    /// The keys keep the known values in the sort's order and the unknown
    /// one after all of them, at the ends of each range too: an empty file,
    /// the largest size the index can hold, the first and last dates.
    #[test]
    fn size_and_date_keys_put_the_unknown_after_every_known_value() {
        for dir in [SortDir::Asc, SortDir::Desc] {
            // Where a key lands in the listing: a descending sort reverses
            // the comparison.
            let place = |key: u64| match dir {
                SortDir::Asc => i128::from(key),
                SortDir::Desc => -i128::from(key),
            };
            let sizes = [0, 1, u64::MAX - 1].map(|s| place(size_key(Some(s), dir)));
            let dates = [i64::MIN + 1, -1, 0, 1, i64::MAX].map(|t| place(mtime_key(Some(t), dir)));
            for places in [&sizes[..], &dates[..]] {
                let ascending = places.windows(2).all(|w| w[0] < w[1]);
                let descending = places.windows(2).all(|w| w[0] > w[1]);
                assert!(
                    if dir == SortDir::Asc {
                        ascending
                    } else {
                        descending
                    },
                    "{dir:?}"
                );
            }
            assert!(
                sizes.iter().all(|&p| p < place(size_key(None, dir))),
                "{dir:?}"
            );
            assert!(
                dates.iter().all(|&p| p < place(mtime_key(None, dir))),
                "{dir:?}"
            );
        }
    }

    #[test]
    fn kind_filters() {
        let idx = sample_index();
        let mut folders = run(&idx, "folder:");
        folders.sort();
        assert_eq!(folders, vec!["docs", "docs\\sub"]);
    }

    /// ORs, NOTs and groups are decided per entry, with the filters and
    /// paths inside them, and each count and page lists exactly what they
    /// match, under every sort.
    #[test]
    fn boolean_queries_match_per_entry() {
        let mut idx = sample_index();
        idx.optimize_storage();
        let every = [
            "docs",
            r"docs\notes.txt",
            r"docs\sub",
            r"docs\sub\Report.PDF",
            "photo.JPG",
        ];
        for (query, expected) in [
            (
                "report|notes",
                &[r"docs\notes.txt", r"docs\sub\Report.PDF"][..],
            ),
            ("report notes|pdf", &[r"docs\sub\Report.PDF"][..]),
            ("<report notes>|jpg", &["photo.JPG"][..]),
            (
                "!report",
                &["docs", r"docs\notes.txt", r"docs\sub", "photo.JPG"][..],
            ),
            ("docs !sub", &["docs"][..]),
            (
                "<*.pdf|*.jpg> size:>1kb",
                &[r"docs\sub\Report.PDF", "photo.JPG"][..],
            ),
            (
                "*.pdf|size:>1mb",
                &[r"docs\sub\Report.PDF", "photo.JPG"][..],
            ),
            ("file:|folder:", &every[..]),
            (
                "folder:!sub",
                &[
                    "docs",
                    r"docs\notes.txt",
                    r"docs\sub\Report.PDF",
                    "photo.JPG",
                ][..],
            ),
            ("file:!pdf", &[r"docs\notes.txt", "photo.JPG"][..]),
            (r"sub\|photo", &[r"docs\sub\Report.PDF", "photo.JPG"][..]),
            (
                r"*.txt|**\sub\*",
                &[r"docs\notes.txt", r"docs\sub\Report.PDF"][..],
            ),
            (r"!docs\", &["docs", "photo.JPG"][..]),
            ("<>", &every[..]),
            ("|", &every[..]),
            ("!", &[][..]),
            ("foo ! bar", &[][..]),
            ("!<!report>", &[r"docs\sub\Report.PDF"][..]),
        ] {
            let parsed = parse_query(query).unwrap();
            for key in SORT_KEYS {
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

    /// The mount prefix decides a path term in an expression, as it does a
    /// plain one: on C:, `c:\docs` is a full-path test and `!c:\` matches
    /// nothing; on D:, `c:\docs` matches nothing, so `c:\docs|photo` is
    /// `photo`, decided by names alone, and `!c:\` matches everything.
    #[test]
    fn the_mount_prefix_decides_path_terms_in_an_expression() {
        let mut idx = sample_index();
        idx.optimize_storage();
        let hits = |prefix, query| hits_under(&idx, prefix, &parse_query(query).unwrap());
        assert_eq!(
            hits(r"C:\", r"c:\docs|photo"),
            [
                "docs",
                r"docs\notes.txt",
                r"docs\sub",
                r"docs\sub\Report.PDF",
                "photo.JPG"
            ]
        );
        assert_eq!(hits(r"D:\", r"c:\docs|photo"), ["photo.JPG"]);
        let parsed = parse_query(r"c:\docs|photo").unwrap();
        let on_d = CompiledQuery::new(&idx, &parsed, r"D:\", &UNLIMITED);
        assert!(is_name_level(&idx, &parsed, &on_d, None), "names decide it");
        assert!(hits(r"C:\", r"!c:\").is_empty());
        assert_eq!(hits(r"D:\", r"!c:\").len(), 5);
        let parsed = parse_query(r"!c:\").unwrap();
        assert!(!CompiledQuery::new(&idx, &parsed, r"C:\", &UNLIMITED).volume_admits);
    }

    /// An OR whose every branch holds a needle scans the folded names for
    /// them, a NOT scans every name, and each finds what testing every name
    /// finds, on every path: the scan, the name-level pass and the best rows.
    #[test]
    fn a_union_prefilter_finds_what_every_name_does() {
        let mut idx = sample_index();
        for (i, name) in ["report2.txt", "notes.md", "kernel32.dll", "x.jpg"]
            .iter()
            .enumerate()
        {
            idx.insert_enum(&enum_rec(frn(500 + i as u64), frn(100), name, false));
        }
        idx.optimize_storage();
        for (query, needles) in [
            ("report|notes", 2),
            ("*.pdf|*.jpg", 2),
            ("<report txt>|<kernel dll>", 2),
            ("report|report2", 1),
            ("ext:pdf;jpg|x", 3),
            ("!report", 0),
            ("report|file:", 0),
            ("report|notes size:>1", 2),
            (r"report|notes docs\", 2),
            // An `ext:` filter of several values scans as v0.6.0 did.
            ("ext:pdf;jpg", 0),
            ("ext:md;dll;pdf size:>1", 0),
            ("ext:pdf;PDF;jpg", 0),
            ("report ext:txt;md", 1),
            ("ext:pdf;j*", 0),
            ("ext:a;b;c;d;e;f;g;h;md", 0),
        ] {
            let parsed = parse_query(query).unwrap();
            let compiled = CompiledQuery::new(&idx, &parsed, r"C:\", &UNLIMITED);
            let planned = compiled
                .union
                .len()
                .max(usize::from(compiled.prefilter.is_some()));
            assert_eq!(planned, needles, "{query}");
            let mut unplanned = CompiledQuery::new(&idx, &parsed, r"C:\", &UNLIMITED);
            unplanned.prefilter = None;
            unplanned.union.clear();
            let found = |compiled: &CompiledQuery| {
                let mut found: Vec<EntryIdx> = scan(&idx, &parsed, compiled, None)
                    .iter()
                    .map(|c| c.idx)
                    .collect();
                found.sort_unstable();
                found
            };
            assert_eq!(found(&compiled), found(&unplanned), "{query}");
            assert!(!found(&compiled).is_empty() || needles == 0, "{query}");
            for key in SORT_KEYS {
                let sort = SortSpec::default_for(key);
                for limit in [None, Some(1), Some(3)] {
                    let fast =
                        unlimited_refs(&idx, r"C:\", &parsed, None, sort, 0, limit, true, true);
                    let slow =
                        unlimited_refs(&idx, r"C:\", &parsed, None, sort, 0, limit, true, false);
                    assert_eq!(fast, slow, "{query} {key:?} {limit:?}");
                }
            }
        }
    }

    /// A NOT never admits an entry without a path: an orphan, or one below
    /// lost+found, fails whatever the expression says.
    #[test]
    fn a_not_admits_no_entry_without_a_path() {
        let idx = index_with_orphans();
        for query in [
            "!ghost",
            "!zzz",
            "ghost|!ghost",
            "<ghost|real> !zzz",
            r"!x\",
        ] {
            let parsed = parse_query(query).unwrap();
            for key in [SortKey::Name, SortKey::Path, SortKey::Size] {
                let out = run_query(
                    &idx,
                    r"C:\",
                    &parsed,
                    None,
                    SortSpec::default_for(key),
                    0,
                    Some(10),
                );
                let mut paths: Vec<String> = out
                    .hits
                    .iter()
                    .map(|h| crate::wtf8::to_string_lossy(&h.path))
                    .collect();
                paths.sort();
                assert_eq!(paths, ["real1.log", "real2.log"], "{query} {key:?}");
                assert_eq!(out.total, 2, "{query} {key:?}");
            }
        }
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

    /// A wildcard with a separator matches the end of the full path, whole
    /// part by whole part, as in Everything 1.5: `*` and `?` stop at a
    /// separator and `**` does not, a match starts at any part, the mount
    /// prefix's parts included, and a pattern with the drive starts where
    /// the full path does, so the drive in front still has to agree.
    #[test]
    fn path_wildcards_match_sub_paths() {
        let idx = sample_index();
        let hits = |prefix, query| hits_under(&idx, prefix, &parse_query(query).unwrap());
        assert_eq!(hits(r"C:\", r"*\sub\*.pdf"), [r"docs\sub\Report.PDF"]);
        assert_eq!(hits(r"C:\", r"C:\docs\*.txt"), [r"docs\notes.txt"]);
        assert_eq!(hits(r"C:\", "path:*.jpg"), ["photo.JPG"]);
        assert!(hits(r"D:\", r"C:\docs\*.txt").is_empty());
        assert!(
            hits(r"C:\", r"C:\docs\*.pdf").is_empty(),
            "* stops at a separator"
        );
        assert_eq!(hits(r"C:\", r"C:\docs\**.pdf"), [r"docs\sub\Report.PDF"]);
        assert_eq!(hits(r"C:\", r"docs\*"), [r"docs\notes.txt", r"docs\sub"]);
        assert_eq!(hits(r"C:\", r"docs\*\"), [r"docs\sub\Report.PDF"], "below");
        assert_eq!(
            hits(r"C:\mnt\data\", r"data\*.jpg"),
            ["photo.JPG"],
            "the prefix's parts count"
        );
        assert_eq!(hits(r"C:\", r"\*.jpg"), ["photo.JPG"]);
        assert_eq!(hits(r"C:\", r"**\report.pdf"), [r"docs\sub\Report.PDF"]);
        assert_eq!(hits(r"C:\", r#""docs\*.txt""#), [r"docs\notes.txt"]);
    }

    /// Below a prefix that ends with a separator, a path wildcard's last part
    /// is matched by the name, so its literal drives the prefilter instead of
    /// a walk below every folder the path names, and a path wildcard of one
    /// part is a test of the name, run over the distinct names. Below a
    /// prefix without one (which the daemon never passes), the first name
    /// runs into the prefix's last part, and neither holds.
    #[test]
    fn a_path_wildcard_prefilters_by_its_last_part() {
        let mut idx = sample_index();
        idx.optimize_storage();
        for (query, needle, name_level) in [
            (r"*\sub\*.pdf", Some(".pdf"), false),
            (r"**\report*", Some("report"), false),
            (r"docs\**.txt", Some(".txt"), false),
            (r"*\*", None, false),
            ("path:*.jpg", Some(".jpg"), true),
            ("*.jpg matchpath:", Some(".jpg"), true),
            ("path:*", None, true),
            ("zz|path:*.jpg", Some(".jpg"), true),
            (r"zz|*\sub\*.pdf", Some(".pdf"), false),
        ] {
            let parsed = parse_query(query).unwrap();
            let compiled = CompiledQuery::new(&idx, &parsed, r"C:\", &UNLIMITED);
            let union = compiled.union.iter().any(|n| n == b"zz");
            let prefilter = compiled
                .prefilter
                .as_deref()
                .map(|p| String::from_utf8_lossy(p));
            assert!(
                prefilter.as_deref() == needle || (union && needle.is_some()),
                "{query}: {prefilter:?}"
            );
            assert_eq!(
                is_name_level(&idx, &parsed, &compiled, None),
                name_level,
                "{query}"
            );
            let mut unplanned = CompiledQuery::new(&idx, &parsed, r"C:\", &UNLIMITED);
            unplanned.prefilter = None;
            unplanned.union.clear();
            let found = |compiled: &CompiledQuery| {
                let mut found: Vec<EntryIdx> = scan(&idx, &parsed, compiled, None)
                    .iter()
                    .map(|c| c.idx)
                    .collect();
                found.sort_unstable();
                found
            };
            assert_eq!(found(&compiled), found(&unplanned), "{query}");
            let below = CompiledQuery::new(&idx, &parsed, "C:", &UNLIMITED);
            assert!(
                !is_name_level(&idx, &parsed, &below, None),
                "{query} under C:"
            );
        }
        let hits = |query| hits_under(&idx, r"C:\", &parse_query(query).unwrap());
        assert_eq!(hits("path:*.jpg"), ["photo.JPG"]);
        assert_eq!(hits(r"*\sub\*.pdf"), [r"docs\sub\Report.PDF"]);
        assert_eq!(hits("zz|path:*.jpg"), ["photo.JPG"]);
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
                "x|log",
                "!e",
                r"!docs\",
                "file:|ghost",
            ] {
                let parsed = parse_query(q).unwrap();
                let compiled = CompiledQuery::new(idx, &parsed, r"C:\", &UNLIMITED);
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
                    let mut folders = compiled.tests_paths().then(|| {
                        FolderPaths::new(idx, &compiled.prefix, true, &compiled.raw_prefix)
                    });
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
                        &mut Scratch::default(),
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
                    let refs = run_query_refs(
                        &idx, r"C:\", &parsed, None, sort, offset, limit, false, &UNLIMITED,
                    )
                    .unwrap();
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
                "readme|notes",
                "!e",
                "<*.md !a>|txt",
            ] {
                let parsed = parse_query(q).unwrap();
                // Ground truth: the candidate path's match SET (ranks play no
                // part in which entries match), ordered by the bytes alone.
                let mut ground: Vec<EntryIdx> = unlimited_refs(
                    idx,
                    r"C:\",
                    &parsed,
                    None,
                    SortSpec::default(),
                    0,
                    None,
                    false,
                    false,
                )
                .refs
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
                        let fast = unlimited_refs(
                            idx, r"C:\", &parsed, None, sort, offset, limit, true, true,
                        );
                        let slow = unlimited_refs(
                            idx, r"C:\", &parsed, None, sort, offset, limit, true, false,
                        );
                        let ctx = format!("{stage}: {q:?} {dir:?} offset {offset} limit {limit:?}");
                        assert_eq!(fast.total, u64::from(total), "total (name-level) {ctx}");
                        assert_eq!(slow.total, u64::from(total), "total (candidates) {ctx}");
                        let ground_size = files_size(idx, ground.iter().copied());
                        assert_eq!(
                            fast.total_size,
                            Some(ground_size),
                            "size (name-level) {ctx}"
                        );
                        assert_eq!(
                            slow.total_size,
                            Some(ground_size),
                            "size (candidates) {ctx}"
                        );
                        let fast_idx: Vec<EntryIdx> = fast.refs.iter().map(|r| r.idx).collect();
                        let slow_idx: Vec<EntryIdx> = slow.refs.iter().map(|r| r.idx).collect();
                        assert_eq!(fast_idx, expected, "name-level page {ctx}");
                        assert_eq!(slow_idx, expected, "candidate page {ctx}");
                        for r in &fast.refs {
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

    /// `ext:`, which keeps files, and the kinds run over the distinct names,
    /// as `ext:` did when it kept folders too: a name no folder bears counts
    /// its files by refcount, and the chain of one a folder bears is walked,
    /// so a folder and files of the same name are told apart. The bit stays
    /// set after the folder goes, which costs a walk and nothing else, until
    /// a compaction recomputes it.
    #[test]
    fn kinds_run_over_the_names() {
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
        recs.extend([
            enum_rec(frn(100), root, "x.txt", true),
            enum_rec(frn(101), frn(100), "x.txt", false),
            enum_rec(frn(102), root, "y.txt", false),
            enum_rec(frn(103), root, "z.pdf", true),
            // An empty folder named as a file is.
            enum_rec(frn(104), frn(103), "y.txt", true),
        ]);
        idx.apply_batch(&recs);
        idx.optimize_storage();
        let id = |idx: &VolumeIndex, f: u64| idx.name_id(idx.head_of(frn(f)).unwrap());
        assert!(idx.name_may_bear_folder(id(&idx, 101)));
        assert!(idx.name_may_bear_folder(id(&idx, 102)));
        assert!(!idx.name_may_bear_folder(id(&idx, 1000)), "files alone");
        let check = |idx: &VolumeIndex, query: &str, want: &[&str]| {
            let parsed = parse_query(query).unwrap();
            let compiled = CompiledQuery::new(idx, &parsed, r"C:\", &UNLIMITED);
            assert!(is_name_level(idx, &parsed, &compiled, None), "{query}");
            let mut got = run(idx, query);
            got.sort();
            assert_eq!(got, want, "{query}");
            for fast in [true, false] {
                let out = unlimited_refs(
                    idx,
                    r"C:\",
                    &parsed,
                    None,
                    SortSpec::default(),
                    0,
                    None,
                    true,
                    fast,
                );
                assert_eq!(out.total, want.len() as u64, "{query} fast {fast}");
                assert_eq!(out.total_size, Some(0), "{query} fast {fast}");
            }
        };
        check(&idx, "ext:txt", &[r"x.txt\x.txt", "y.txt"]);
        check(&idx, "ext:pdf", &[]);
        check(&idx, "file:x.t", &[r"x.txt\x.txt"]);
        check(&idx, "folder:x.t", &["x.txt"]);
        check(&idx, "folder:txt", &["x.txt", r"z.pdf\y.txt"]);
        check(&idx, "file:y", &["y.txt"]);
        check(&idx, "folder:y", &[r"z.pdf\y.txt"]);
        check(&idx, r"\ .", &["x.txt", "z.pdf", r"z.pdf\y.txt"]);
        check(&idx, "x.txt", &["x.txt", r"x.txt\x.txt"]);
        // The empty folder goes: the bit of the name it shared with a file
        // stays, and the counts hold.
        idx.apply_batch(&[delete(frn(104))]);
        assert!(idx.name_may_bear_folder(id(&idx, 102)));
        check(&idx, "folder:y", &[]);
        check(&idx, "file:y", &["y.txt"]);
        check(&idx, "ext:txt", &[r"x.txt\x.txt", "y.txt"]);
        // A compaction recomputes the bits from the folders still there.
        idx.apply_batch(&(0..400).map(|i| delete(frn(1000 + i))).collect::<Vec<_>>());
        let plan = idx.plan_compaction().expect("garbage");
        idx.apply_compaction(plan);
        assert!(idx.name_refcounts_match_chains());
        assert!(!idx.name_may_bear_folder(id(&idx, 102)));
        assert!(idx.name_may_bear_folder(id(&idx, 101)));
        assert!(idx.name_may_bear_folder(id(&idx, 103)));
        check(&idx, "folder:", &["x.txt", "z.pdf"]);
        check(&idx, "ext:txt", &[r"x.txt\x.txt", "y.txt"]);
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
                    let mut paths = FolderPaths::new(&idx, &mount, fold, prefix.as_bytes());
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
        let mut paths = FolderPaths::new(&idx, br"c:\", true, br"c:\");
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
        let mut paths = FolderPaths::new(&idx, br"C:\", false, br"C:\");
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
        assert!(walk_below_names(&idx, b"sys32", 100, &UNLIMITED, |x| seen.push(x)));
        seen.sort_unstable();
        let mut want = vec![e(100), e(101), e(102), e(103), e(106)];
        want.sort_unstable();
        assert_eq!(seen, want);
        assert!(
            !walk_below_names(&idx, b"sys32", 4, &UNLIMITED, |_| {}),
            "five to visit"
        );
        assert!(
            !walk_below_names(&idx, b"sys32", 3, &UNLIMITED, |_| panic!("walked")),
            "four names hold the piece"
        );
    }

    /// The walk is driven by the longest piece of path text between
    /// separators that the mount prefix does not hold, the later on a tie.
    #[test]
    fn the_path_walk_takes_the_longest_piece_the_prefix_lacks() {
        let piece = |query: &str, prefix: &str| {
            let parsed = parse_query(query).unwrap();
            let idx = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
            CompiledQuery::new(&idx, &parsed, prefix, &UNLIMITED)
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
    /// runs per distinct name, and so is the drive term `c:`. Another
    /// drive's term still empties the volume, and under `case:` a lowercase
    /// drive is not in `C:\`. A lone `\` keeps folders (Everything 1.5),
    /// which the names decide too (see `entries_of_kind`).
    #[test]
    fn a_path_term_the_prefix_holds_leaves_the_query_to_names() {
        let mut idx = sample_index();
        idx.optimize_storage();
        let report = parse_query("report").unwrap();
        for query in [r"C:\ report", "c: report", "path:c: report"] {
            let parsed = parse_query(query).unwrap();
            let compiled = CompiledQuery::new(&idx, &parsed, r"C:\", &UNLIMITED);
            assert!(is_name_level(&idx, &parsed, &compiled, None), "{query}");
            assert_eq!(
                hits_under(&idx, r"C:\", &parsed),
                hits_under(&idx, r"C:\", &report),
                "{query}"
            );
        }
        let folders = parse_query(r"\ report").unwrap();
        assert!(is_name_level(
            &idx,
            &folders,
            &CompiledQuery::new(&idx, &folders, r"C:\", &UNLIMITED),
            None
        ));
        let report_folders = parse_query("folder: report").unwrap();
        assert_eq!(
            hits_under(&idx, r"C:\", &folders),
            hits_under(&idx, r"C:\", &report_folders)
        );
        let other_drive = parse_query(r"C:\ report").unwrap();
        assert!(!is_name_level(
            &idx,
            &other_drive,
            &CompiledQuery::new(&idx, &other_drive, r"D:\", &UNLIMITED),
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
            shape in 0u8..9,
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
            // Literals and a last `**`, which a folder decides when they end
            // with a separator.
            let lead = |(i, j): (usize, usize), tail: &str| {
                format!("path:{}{tail}", flip_case(&full[i..j], flip >> 3))
            };
            let (m, n) = span(4, 5, name.len());
            let name_term = flip_case(&name[m..n], flip >> 11);
            let mut terms = match shape {
                0 => vec![path_term(span(0, 1, full.len()))],
                1 => vec![path_term(span(0, 1, full.len())), name_term],
                2 => vec![path_term(span(0, 1, full.len())), path_term(span(2, 3, full.len()))],
                3 => vec![wildcard(span(0, 1, full.len()))],
                4 => vec![wildcard(span(0, 1, full.len())), name_term],
                5 => vec![path_term(span(0, 1, full.len())), wildcard(span(2, 3, full.len()))],
                6 => vec![lead(span(0, 1, full.len()), r"\**")],
                7 => vec![lead(span(0, 1, full.len()), "**"), name_term],
                _ => vec![lead(span(0, 1, full.len()), r"\**"), path_term(span(2, 3, full.len()))],
            };
            // The wildcards as typed, for the reference to match on its own.
            let patterns: Vec<String> = terms
                .iter()
                .filter_map(|t| t.strip_prefix("path:"))
                .filter(|t| t.contains('*'))
                .map(str::to_string)
                .collect();
            if match_case {
                terms.push("case:".into());
            }
            // A lone separator keeps folders, as in Everything 1.5.
            let folders_only = terms.iter().any(|t| t == "\\");
            let query = terms.join(" ");
            let parsed = parse_query(&query).unwrap();
            let folders: Vec<EntryIdx> = entries.iter().copied().filter(|&e| idx.entry(e).is_dir()).collect();
            let scope = scope_pick.filter(|_| !folders.is_empty()).map(|p| *p.get(&folders));

            let fold = |b: &[u8]| if parsed.match_case { b.to_vec() } else { crate::fold::fold(b) };
            let patterns: Vec<String> = patterns
                .iter()
                .map(|p| String::from_utf8(fold(p.as_bytes())).unwrap())
                .collect();
            // A term all of whose characters fold away (a lone combining
            // mark) is an empty term, which matches everything.
            let holds = |hay: &[u8], t: &[u8]| t.is_empty() || hay.windows(t.len()).any(|w| w == t);
            let mut want: Vec<String> = idx
                .live_entries()
                .filter(|&e| !idx.is_synthetic(e))
                .filter(|&e| {
                    let Some(full) = slow_full_path(&idx, e, prefix, !parsed.match_case) else {
                        return false;
                    };
                    let name = fold(idx.entry(e).name());
                    let full_text = String::from_utf8(full.clone()).unwrap();
                    scope.is_none_or(|s| is_below(&idx, e, s))
                        && (!folders_only || idx.entry(e).is_dir())
                        && parsed.name_terms.iter().all(|t| holds(&name, t))
                        && parsed.path_terms.iter().all(|t| holds(&full, t))
                        && patterns
                            .iter()
                            .all(|p| crate::query::glob::tests::reference_sub_path(p, &full_text))
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

        /// A query that tests the name and has no needle finds the same
        /// entries when the scan finds the names first, across cores, as
        /// when it reads every entry: the names passed on, or every entry
        /// with the rest skipped once they bear too much of the volume.
        #[test]
        fn names_first_finds_what_every_entry_does(
            nodes in prop::collection::vec(
                (any::<prop::sample::Index>(), "[aAbB]{1,3}(\\.(txt|exe|a))?", any::<bool>()),
                1..40,
            ),
            stats in prop::collection::vec((0u64..8, 0i64..9), 40),
            padded in any::<bool>(),
            query in prop::sample::select(vec![
                // No needle drives these, and each has a test of the name.
                "exe:|ext:txt", "audio:|exe:", "case:A <size:|ext:a>", "case:a !ext:",
                "case:B|ext:TXT", "<ext:txt|exe:> !a", "!a <exe:|size:>3>",
                "*?* <exe:|folder:>", "nopunc:ab <ext:txt;a|folder:>",
                "case:A|case:b <size:>2|ext:exe>", "!b <ext:|ext:exe>", "zip:|doc:|exe:",
                "nows:ab <file:|ext:a>", "a <file:|ext:exe> case:",
                // No expression, as v0.6.0 read them: an `ext:` list, a
                // wildcard with no literal, or a case-sensitive term.
                "ext:txt;a size:>3", "ext:exe;txt", "*?* size:<5 file:",
                "A size:>2 case:", "b case: ext:txt;exe", "ext:a;exe folder:",
            ]),
        ) {
            let (mut idx, _) = random_tree(&nodes, &[], padded);
            for (i, (size, mtime)) in stats.iter().enumerate().take(nodes.len()) {
                idx.set_stat(frn(1000 + i as u64), *size, *mtime);
            }
            let parsed = parse_query(query).unwrap();
            let compiled = CompiledQuery::new(&idx, &parsed, r"C:\", &UNLIMITED);
            prop_assert!(compiled.names_first && compiled.needles(&parsed).is_empty(), "{}", query);
            let mut every = CompiledQuery::new(&idx, &parsed, r"C:\", &UNLIMITED);
            every.names_first = false;
            let found = |compiled: &CompiledQuery| {
                let mut found: Vec<EntryIdx> =
                    scan(&idx, &parsed, compiled, None).iter().map(|c| c.idx).collect();
                found.sort_unstable();
                found
            };
            prop_assert_eq!(found(&compiled), found(&every), "{}", query);
        }

        /// A scoped query finds the same entries however far its walk goes
        /// before the scan is priced: walked to the end, or given up for the
        /// names the pricing found, or for a walk below the names holding a
        /// piece of its path and the scan of every entry that walk falls back
        /// to once it grows too large.
        #[test]
        fn a_scope_finds_the_same_walked_or_scanned(
            nodes in prop::collection::vec(
                (any::<prop::sample::Index>(), "[aAbB]{1,3}(\\.txt)?", any::<bool>()),
                1..40,
            ),
            orphans in prop::collection::vec("[aAbB]{1,3}", 0..3),
            scope_pick in any::<prop::sample::Index>(),
            query in prop::sample::select(vec![
                "ab", "b.txt", "*.txt", "ab b", "b ext:txt", "b.txt file:", "file:", "folder:",
                "A case:", r"a\", r"path:*\b*", "ab|b.txt", "!ab", r"a|b\", "<a b>|*.txt",
                "file:!a", "folder:!b", r"ab !a\", r"**\b*",
                // S2: functions and modifiers, and the folders a `parent:` or
                // `ancestor:` names, which may or may not be there.
                r"parent:C:\a", r"parent:C:\A\b|b", r"ancestor:C:\a", r"ancestor:c:\B a",
                "ext:txt", "ext:", "ww:a", "prefix:b|suffix:a", "whole:ab", "nopunc:a.t",
                "len:>3", "stem:a", "extension:t", r"\ a", "c:", "!d:", "audio:|ext:txt",
                "size:", "case:A|nocase:b",
            ]),
        ) {
            let (idx, entries) = random_tree(&nodes, &orphans, false);
            let folders: Vec<EntryIdx> =
                entries.iter().copied().filter(|&e| idx.entry(e).is_dir()).collect();
            prop_assume!(!folders.is_empty());
            let scope = *scope_pick.get(&folders);
            let parsed = parse_query(query).unwrap();
            let compiled = CompiledQuery::new(&idx, &parsed, r"C:\", &UNLIMITED);
            let found = |floor| {
                let mut found: Vec<EntryIdx> = scan_with(&idx, &parsed, &compiled, Some(scope), floor)
                    .iter()
                    .map(|c| c.idx)
                    .collect();
                found.sort_unstable();
                found
            };
            let walked = found(usize::MAX);
            for floor in [0, 1, 2, 5] {
                prop_assert_eq!(&found(floor), &walked, "{:?} with floor {}", query, floor);
            }
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
        let compiled = CompiledQuery::new(index, parsed, r"C:\", &UNLIMITED);
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
                let refs = unlimited_refs(
                    index, r"C:\", parsed, None, sort, offset, limit, false, true,
                )
                .refs;
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
            let refs = unlimited_refs(&idx, r"C:\", &parsed, None, sort, 0, None, false, true).refs;
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

    /// Name `i` of the fast-path corpus: pairs that fold alike (`e3.txt` and
    /// `E3.TXT`), and enough names to split into many chunks.
    fn corpus_name(i: usize) -> String {
        let n = i / 4;
        match i % 4 {
            0 => format!("e{n}.txt"),
            1 => format!("E{n}.TXT"),
            2 => format!("a{n}.md"),
            _ => format!("zz{n}"),
        }
    }

    const FAST_QUERIES: [&str; 20] = [
        "e",
        "E1",
        "a",
        ".txt",
        "*.md",
        "ext:txt",
        "file:e",
        "folder:a",
        "e size:>10",
        "zz size:<20",
        "x",
        "",
        "e|zz",
        "*.md|ext:txt",
        "!e",
        "e !E1",
        "<e a>|zz",
        "file:|a",
        "e1|e2 size:>10",
        "!<a|zz>|e3",
    ];

    const SORT_KEYS: [SortKey; 4] = [
        SortKey::Name,
        SortKey::Size,
        SortKey::DateModified,
        SortKey::Path,
    ];

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]
        /// The name-level pass and the bounded top-k page exactly like the
        /// candidate path, with the same total and summed size, for every
        /// sort and slice, over random indexes whose names split into many
        /// chunks, fold alike, repeat, join the order late, or lose their
        /// last entry, with sizes and dates known or not.
        #[test]
        fn fast_paths_page_like_the_candidates(
            files in prop::collection::vec(
                (
                    0usize..320,
                    0usize..4,
                    any::<bool>(),
                    prop::option::of(0u64..40),
                    prop::option::of(1i64..40),
                ),
                1..300,
            ),
            late in prop::collection::vec(0usize..320, 0..24),
            gone in prop::collection::vec(any::<prop::sample::Index>(), 0..12),
            query in 0usize..FAST_QUERIES.len(),
            key in 0usize..SORT_KEYS.len(),
            desc in any::<bool>(),
            offset in 0u32..24,
            limit in prop::option::of(0u32..40),
        ) {
            let mut idx = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
            let dirs = [NTFS_ROOT_FRN, frn(10), frn(11), frn(12)];
            for (i, &d) in dirs[1..].iter().enumerate() {
                idx.insert_enum(&enum_rec(d, NTFS_ROOT_FRN, &format!("dir{i}"), true));
            }
            for (n, &(name, dir, is_dir, size, mtime)) in files.iter().enumerate() {
                let f = frn(1000 + n as u64);
                idx.insert_enum(&enum_rec(f, dirs[dir], &corpus_name(name), is_dir));
                if size.is_some() || mtime.is_some() {
                    idx.set_stat(f, size.unwrap_or(u64::MAX), mtime.unwrap_or(i64::MIN));
                }
            }
            idx.optimize_storage();
            // Interned after the order was built, so they wait unranked.
            let late: Vec<ParsedUsnRecord> = late
                .iter()
                .enumerate()
                .map(|(n, &name)| {
                    enum_rec(frn(5000 + n as u64), NTFS_ROOT_FRN, &corpus_name(name), false)
                })
                .collect();
            idx.apply_batch(&late);
            // Files only, so no entry is left without a parent.
            let gone: Vec<ParsedUsnRecord> = gone
                .iter()
                .map(|g| {
                    let f = frn(1000 + g.index(files.len()) as u64);
                    let mut r = enum_rec(f, NTFS_ROOT_FRN, "", false);
                    r.reason = USN_REASON_FILE_DELETE;
                    r
                })
                .collect();
            idx.apply_batch(&gone);
            prop_assert!(!idx.may_have_unpresentable());

            let parsed = parse_query(FAST_QUERIES[query]).unwrap();
            let dir = if desc { SortDir::Desc } else { SortDir::Asc };
            let sort = SortSpec { key: SORT_KEYS[key], dir };
            // A count alone, as `-get-result-count` asks, and a single row.
            for (offset, limit) in [(offset, limit), (0, Some(0)), (0, Some(1))] {
                let fast = unlimited_refs(&idx, r"C:\", &parsed, None, sort, offset, limit, true, true);
                let slow = unlimited_refs(&idx, r"C:\", &parsed, None, sort, offset, limit, true, false);
                prop_assert_eq!(fast.total, slow.total);
                prop_assert_eq!(fast.total_size, slow.total_size);
                let fast: Vec<EntryIdx> = fast.refs.iter().map(|r| r.idx).collect();
                let slow: Vec<EntryIdx> = slow.refs.iter().map(|r| r.idx).collect();
                prop_assert_eq!(fast, slow);
            }
        }
    }

    // -- boolean queries against a naive evaluator ---------------------------

    /// The engine against a naive reference over random trees and random
    /// queries of every kind of term S1 and S2 read: operators and groups,
    /// name and path text and wildcards, every modifier (`case:`, `path:`,
    /// `nopath:`, the word and anchor modifiers, `nopunc:`, `nows:`) and kind
    /// prefix (`file:`, `folder:`, `dir:`, `no-file:`, `no-folder:`), the
    /// functions (`ext:`, `size:`, the lengths, `dm:`, `parent:`,
    /// `ancestor:`, the name parts), drive terms, the lone `\` and a macro.
    /// The reference reads each term with code of its own (a DP for
    /// wildcards, a search of every placement for skips, path strings built
    /// by `path_of`), and the engine must give its matches, totals, summed
    /// sizes and every sorted page, whichever way it runs.
    mod boolean_reference {
        use super::*;
        use crate::query::dates::tests::FixedClock;
        use crate::query::glob::tests::{reference_match, reference_sub_path};
        use prop::sample::Index;

        /// A prefix over a term or a group.
        #[derive(Clone, Copy, Debug)]
        enum P {
            Case(bool),
            Path(bool),
            Ww,
            Prefix,
            Suffix,
            Whole,
            StartWith,
            EndWith,
            NoPunc,
            NoWs,
            File,
            Folder,
            Dir,
            NoFile,
            NoFolder,
            Diacritics,
        }

        impl P {
            fn text(self) -> &'static str {
                match self {
                    P::Case(true) => "case:",
                    P::Case(false) => "nocase:",
                    P::Path(true) => "path:",
                    P::Path(false) => "nopath:",
                    P::Ww => "ww:",
                    P::Prefix => "prefix:",
                    P::Suffix => "suffix:",
                    P::Whole => "whole:",
                    P::StartWith => "startwith:",
                    P::EndWith => "endwith:",
                    P::NoPunc => "nopunc:",
                    P::NoWs => "nows:",
                    P::File => "file:",
                    P::Folder => "folder:",
                    P::Dir => "dir:",
                    P::NoFile => "no-file:",
                    P::NoFolder => "no-folder:",
                    P::Diacritics => "diacritics:",
                }
            }
        }

        /// A function on a part of the full path.
        #[derive(Clone, Copy, Debug, PartialEq)]
        enum Part {
            Name,
            Stem,
            Extension,
            FullPath,
            Location,
            ParentName,
        }

        /// A length function.
        #[derive(Clone, Copy, Debug)]
        enum LenFn {
            Len,
            Name,
            FullPath,
            Location,
            Extension,
        }

        /// A count function: `parent-count:` under one of its three
        /// spellings, or a folder's children.
        #[derive(Clone, Copy, Debug)]
        enum CountFn {
            Parents(u8),
            Children,
            ChildFiles,
            ChildFolders,
        }

        /// One term.
        #[derive(Clone, Debug)]
        enum L {
            /// A count function, its value as written, its range, and a `!`.
            Count(CountFn, String, SizeRange, bool),
            /// `empty:` alone.
            EmptyFolder,
            /// Search text: a wildcard when it holds `*` or `?`.
            Text(String),
            Part(Part, String),
            /// `ext:` values, as written.
            Ext(Vec<String>),
            /// `size:` values as written, the ranges the test means, and
            /// whether the value carries its own `!`.
            Size(Vec<(String, SizeRange)>, bool),
            SizeUnknown,
            /// A length function, its value as written, its range, and a `!`.
            Len(LenFn, String, SizeRange, bool),
            /// `dm:` as written, the FILETIME range it means, and a `!`.
            Date(String, SizeRange, bool),
            Drive(char),
            /// `\` alone.
            Lone,
            /// `parent:` or `ancestor:` (`true`) of a folder: the path as
            /// written, and the folded path the test means.
            Folder(bool, String, String),
            /// `exe:`.
            Macro,
            /// `regex:` and its pattern, every character but a letter or a
            /// digit escaped, so that the lexer reads it whole.
            Regex(String),
        }

        #[derive(Clone, Debug)]
        enum Q {
            Leaf(L),
            Prefixed(P, Box<Q>),
            /// `<>`.
            Empty,
            Not(Box<Q>),
            And(Vec<Q>),
            Or(Vec<Q>),
        }

        /// The shape of a query, drawn before the tree its terms are cut
        /// from exists.
        #[derive(Clone, Debug)]
        enum Spec {
            Leaf(u8, Index, Index, Index, u64, u64),
            Prefixed(u8, Box<Spec>),
            Not(Box<Spec>),
            And(Vec<Spec>),
            Or(Vec<Spec>),
        }

        fn spec() -> impl Strategy<Value = Spec> {
            let leaf = (
                0u8..19,
                any::<Index>(),
                any::<Index>(),
                any::<Index>(),
                any::<u64>(),
                any::<u64>(),
            )
                .prop_map(|(kind, pick, a, b, flip, n)| Spec::Leaf(kind, pick, a, b, flip, n));
            leaf.prop_recursive(3, 8, 3, |inner| {
                prop_oneof![
                    inner.clone().prop_map(|s| Spec::Not(Box::new(s))),
                    (0u8..18, inner.clone()).prop_map(|(p, s)| Spec::Prefixed(p, Box::new(s))),
                    prop::collection::vec(inner.clone(), 2..4).prop_map(Spec::And),
                    prop::collection::vec(inner, 2..4).prop_map(Spec::Or),
                ]
            })
        }

        /// What the terms are cut from: each presentable entry's name and
        /// full path as typed, and each folder's full path.
        struct Vocabulary {
            names: Vec<Vec<char>>,
            paths: Vec<Vec<char>>,
            folders: Vec<String>,
            /// Whether `parent:` and `ancestor:` can name the folders: the
            /// prefix ends with a separator, as every mount prefix the daemon
            /// passes does.
            absolute: bool,
            mtimes: Vec<i64>,
        }

        fn cut(chars: &[char], a: Index, b: Index) -> &[char] {
            let (x, y) = (a.index(chars.len() + 1), b.index(chars.len() + 1));
            &chars[x.min(y)..x.max(y)]
        }

        /// `chars` with some replaced by wildcards, as `n`'s bits say, never
        /// a separator, and runs of `*` kept single; at least one wildcard.
        fn wild(chars: &[char], n: u64) -> String {
            let mut out = String::new();
            for (k, &c) in chars.iter().enumerate() {
                let bits = (n >> ((k * 2) % 62)) & 3;
                let w = match bits {
                    0 if c != '\\' => Some('*'),
                    1 if c != '\\' && k % 3 == 0 => Some('?'),
                    _ => None,
                };
                match w {
                    Some('*') if out.ends_with('*') => {}
                    Some(w) => out.push(w),
                    None => out.push(c),
                }
            }
            if !out.contains(['*', '?']) {
                out.push('*');
            }
            out
        }

        fn chars(s: &str) -> Vec<char> {
            s.chars().collect()
        }

        /// A size value as written and the range it means: a comparison, a
        /// range, bytes, a unit's granule, a constant, or an empty value.
        fn size_value(n: u64) -> (String, SizeRange) {
            let k = 1 + n % 40;
            let m = k + (n >> 8) % 20;
            let r = |min, max| SizeRange { min, max };
            match (n >> 16) % 12 {
                0 => (format!(">{k}"), r(k + 1, u64::MAX)),
                1 => (format!("<{k}"), r(0, k - 1)),
                2 => (format!(">={k}"), r(k, u64::MAX)),
                3 => (format!("<={k}"), r(0, k)),
                4 => (format!("{k}..{m}"), r(k, m)),
                5 => (format!("{m}-{k}"), r(k, m)),
                6 => ("1kb".into(), r(1024, 2047)),
                7 => ("=1kb".into(), r(1024, 1024)),
                8 => ("tiny".into(), r(1, 10_240)),
                9 => ("empty".into(), r(0, 0)),
                10 => (String::new(), r(1, u64::MAX)),
                _ => (format!("{k}"), r(k, k)),
            }
        }

        fn materialize(spec: &Spec, v: &Vocabulary) -> Q {
            match spec {
                Spec::Not(s) => Q::Not(Box::new(materialize(s, v))),
                Spec::And(items) => Q::And(items.iter().map(|s| materialize(s, v)).collect()),
                Spec::Or(items) => Q::Or(items.iter().map(|s| materialize(s, v)).collect()),
                Spec::Prefixed(p, s) => {
                    let p = [
                        P::Case(true),
                        P::Case(false),
                        P::Path(true),
                        P::Path(false),
                        P::Ww,
                        P::Prefix,
                        P::Suffix,
                        P::Whole,
                        P::StartWith,
                        P::EndWith,
                        P::NoPunc,
                        P::NoWs,
                        P::File,
                        P::Folder,
                        P::Dir,
                        P::NoFile,
                        P::NoFolder,
                        P::Diacritics,
                    ][*p as usize];
                    Q::Prefixed(p, Box::new(materialize(s, v)))
                }
                Spec::Leaf(15, ..) => Q::Empty,
                Spec::Leaf(kind, pick, a, b, flip, n) => {
                    let name = pick.get(&v.names);
                    let path = pick.get(&v.paths);
                    let nonempty = |s: String| if s.is_empty() { "a".to_string() } else { s };
                    Q::Leaf(match kind {
                        0 => L::Text(nonempty(flip_case(cut(name, *a, *b), *flip))),
                        // With its punctuation and spaces dropped, which only
                        // a skip (`nopunc:`, `nows:`) finds again.
                        1 => {
                            let text: String = flip_case(cut(name, *a, *b), *flip)
                                .chars()
                                .filter(|&c| word(c) || (n % 2 == 0 && c.is_whitespace()))
                                .collect();
                            L::Text(nonempty(text))
                        }
                        2 => L::Text(nonempty(flip_case(cut(path, *a, *b), *flip))),
                        3 => {
                            let text = wild(&chars(&flip_case(name, *flip)), *n);
                            L::Text(text.replace("**", "*"))
                        }
                        4 => {
                            // The last `k + 1` parts, with wildcards, maybe a
                            // `**` part, maybe a separator in front or behind.
                            let parts: Vec<String> = flip_case(path, *flip)
                                .split('\\')
                                .map(str::to_string)
                                .collect();
                            let k = (*n % 3) as usize;
                            let mut parts: Vec<String> =
                                parts[parts.len().saturating_sub(k + 1)..].to_vec();
                            if n % 5 == 0 && parts.len() > 2 {
                                let mid = parts.len() / 2;
                                parts[mid] = "**".into();
                            }
                            let mut text = wild(&chars(&parts.join("\\")), n.rotate_left(17));
                            if n % 3 == 0 {
                                text.push('\\');
                            }
                            L::Text(text)
                        }
                        5 => {
                            // Values from the name's extension, written the
                            // ways Everything reads them.
                            let lower: String = name.iter().collect();
                            let ext = lower.rsplit_once('.').map_or("", |(_, e)| e);
                            let two = lower
                                .rsplitn(3, '.')
                                .take(2)
                                .collect::<Vec<_>>()
                                .into_iter()
                                .rev()
                                .collect::<Vec<_>>()
                                .join(".");
                            let ext = flip_case(&chars(ext), *flip);
                            let mut values = vec![match n % 7 {
                                0 => format!(".{ext}"),
                                1 => format!("*.{ext}"),
                                2 => String::new(),
                                3 => format!("{}*", ext.chars().take(1).collect::<String>()),
                                4 => "?".repeat(ext.chars().count().max(1)),
                                5 => two,
                                _ => ext,
                            }];
                            if (n >> 8) % 3 == 0 {
                                values.push("b".into());
                            }
                            if (n >> 8) % 5 == 0 {
                                values.push(String::new());
                            }
                            L::Ext(values)
                        }
                        6 => {
                            let mut values = vec![size_value(*n)];
                            if (n >> 24) % 3 == 0 {
                                values.push(size_value(n.rotate_left(29)));
                            }
                            if (n >> 26) % 9 == 0 {
                                L::SizeUnknown
                            } else {
                                L::Size(values, (n >> 30) % 4 == 0)
                            }
                        }
                        7 => {
                            let f = [
                                Part::Name,
                                Part::Stem,
                                Part::Extension,
                                Part::FullPath,
                                Part::Location,
                                Part::ParentName,
                            ][(n % 6) as usize];
                            let from = if matches!(f, Part::Name | Part::Stem | Part::Extension) {
                                name
                            } else {
                                path
                            };
                            let mut text = flip_case(cut(from, *a, *b), *flip);
                            if (n >> 8) % 7 == 0 && !text.is_empty() {
                                text = wild(&chars(&text), n.rotate_left(11));
                            }
                            L::Part(f, text)
                        }
                        8 => {
                            let f = [
                                LenFn::Len,
                                LenFn::Name,
                                LenFn::FullPath,
                                LenFn::Location,
                                LenFn::Extension,
                            ][(n % 5) as usize];
                            let k = (n >> 8) % 12;
                            let m = k + (n >> 16) % 30;
                            let r = |min, max| SizeRange { min, max };
                            let (text, range) = match (n >> 24) % 4 {
                                0 => (format!("{k}"), r(k, k)),
                                1 => (format!(">{k}"), r(k + 1, u64::MAX)),
                                2 => (format!("<={m}"), r(0, m)),
                                _ => (format!("{k}..{m}"), r(k, m)),
                            };
                            L::Len(f, text, range, (n >> 30) % 4 == 0)
                        }
                        9 => {
                            // FILETIME values, exact to the tick.
                            let t = *pick.get(&v.mtimes) as u64;
                            let u = t + (n % 3) * ((n >> 8) % 5);
                            let r = |min, max| SizeRange { min, max };
                            let (text, range) = match (n >> 16) % 5 {
                                0 => (format!("{t}"), r(t, t)),
                                1 => (format!(">={t}"), r(t, u64::MAX)),
                                2 => (format!("<{t}"), r(0, t - 1)),
                                3 => (format!("{u}..{t}"), r(t, u)),
                                _ => (format!("0x{t:x}"), r(t, t)),
                            };
                            L::Date(text, range, (n >> 30) % 4 == 0)
                        }
                        10 => L::Drive(if n % 2 == 0 { 'C' } else { 'd' }),
                        11 => L::Lone,
                        17 => {
                            let f = [
                                CountFn::Parents((n % 3) as u8),
                                CountFn::Children,
                                CountFn::ChildFiles,
                                CountFn::ChildFolders,
                            ][((n >> 2) % 4) as usize];
                            let k = (n >> 8) % 5;
                            let m = k + (n >> 16) % 4;
                            let r = |min, max| SizeRange { min, max };
                            let (text, range) = match (n >> 24) % 4 {
                                0 => (format!("{k}"), r(k, k)),
                                1 => (format!(">{k}"), r(k + 1, u64::MAX)),
                                2 => (format!("<={m}"), r(0, m)),
                                _ => (format!("{k}..{m}"), r(k, m)),
                            };
                            L::Count(f, text, range, (n >> 30) % 4 == 0)
                        }
                        18 => L::EmptyFolder,
                        12 | 13 if v.absolute && !v.folders.is_empty() => {
                            let folded = pick.get(&v.folders).clone();
                            let mut text = flip_case(&chars(&folded), *flip);
                            if n % 3 == 0 {
                                text.push('\\');
                            }
                            L::Folder(
                                *kind == 13,
                                text,
                                crate::wtf8::to_string_lossy(&crate::fold::fold(folded.as_bytes())),
                            )
                        }
                        14 => L::Macro,
                        16 => {
                            // A cut of a name, or of a full path, which only
                            // `path:` matches.
                            let from = if n % 3 == 0 { path } else { name };
                            let text = flip_case(cut(from, *a, *b), *flip);
                            let mut pattern: String = text
                                .chars()
                                .map(|c| {
                                    if c.is_alphanumeric() {
                                        c.to_string()
                                    } else {
                                        format!("\\x{{{:X}}}", c as u32)
                                    }
                                })
                                .collect();
                            if (n >> 8) % 4 == 0 {
                                pattern.insert(0, '^');
                            }
                            if (n >> 10) % 4 == 0 {
                                pattern.push('$');
                            }
                            if (n >> 12) % 5 == 0 && pattern.chars().count() > 2 {
                                // One character any.
                                pattern.push_str(".?");
                            }
                            L::Regex(nonempty(pattern))
                        }
                        _ => L::Text(nonempty(flip_case(cut(name, *a, *b), *flip))),
                    })
                }
            }
        }

        fn is_composite(q: &Q) -> bool {
            matches!(q, Q::And(_) | Q::Or(_))
        }

        /// A value or search text as the query writes it: quoted when it
        /// holds a space.
        fn quoted(s: &str) -> String {
            if s.contains(' ') {
                format!("\"{s}\"")
            } else {
                s.to_string()
            }
        }

        /// A value's own `!`.
        fn bang(neg: bool) -> &'static str {
            if neg { "!" } else { "" }
        }

        fn print_leaf(l: &L) -> String {
            match l {
                L::Text(s) => quoted(s),
                L::Part(f, s) => {
                    let f = match f {
                        Part::Name => "name:",
                        Part::Stem => "stem:",
                        Part::Extension => "extension:",
                        Part::FullPath => "full-path:",
                        Part::Location => "location:",
                        Part::ParentName => "parent-name:",
                    };
                    format!("{f}{}", quoted(s))
                }
                L::Ext(values) => {
                    let v: Vec<String> = values.iter().map(|v| quoted(v)).collect();
                    format!("ext:{}", v.join(";"))
                }
                L::Size(values, neg) => {
                    let v: Vec<&str> = values.iter().map(|(t, _)| t.as_str()).collect();
                    format!("size:{}{}", bang(*neg), v.join(";"))
                }
                L::SizeUnknown => "size:unknown".into(),
                L::Len(f, text, _, neg) => {
                    let f = match f {
                        LenFn::Len => "len:",
                        LenFn::Name => "namelen:",
                        LenFn::FullPath => "fullpathlen:",
                        LenFn::Location => "path-part-length:",
                        LenFn::Extension => "extlen:",
                    };
                    format!("{f}{}{text}", bang(*neg))
                }
                L::Count(f, text, _, neg) => {
                    let f = match f {
                        CountFn::Parents(0) => "parent-count:",
                        CountFn::Parents(1) => "depth:",
                        CountFn::Parents(_) => "parents:",
                        CountFn::Children => "child-count:",
                        CountFn::ChildFiles => "childfilecount:",
                        CountFn::ChildFolders => "child-folder-count:",
                    };
                    format!("{f}{}{text}", bang(*neg))
                }
                L::EmptyFolder => "empty:".into(),
                L::Date(text, _, neg) => format!("dm:{}{text}", bang(*neg)),
                L::Drive(d) => format!("{d}:"),
                L::Lone => "\\".into(),
                L::Folder(ancestor, text, _) => {
                    let f = if *ancestor { "ancestor:" } else { "parent:" };
                    format!("{f}{}", quoted(text))
                }
                L::Macro => "exe:".into(),
                L::Regex(pattern) => format!("regex:{pattern}"),
            }
        }

        /// The query text: every group bracketed, or only where Everything's
        /// precedence (OR binds tighter than AND, NOT tighter still) needs it.
        /// An OR next to a numeric comparison is spaced, so that its `<`
        /// stays a comparison rather than open a value group.
        fn print(q: &Q, minimal: bool) -> String {
            match q {
                Q::Leaf(l) => print_leaf(l),
                Q::Empty => "<>".into(),
                Q::Prefixed(p, inner) => match &**inner {
                    inner if is_composite(inner) => {
                        format!("{}<{}>", p.text(), print(inner, minimal))
                    }
                    // `case:` alone is the whole-query switch, a goz
                    // extension (DESIGN.md §3.1): over an empty term it is
                    // written with an empty group.
                    Q::Empty if matches!(p, P::Case(true)) => "case:<>".to_string(),
                    // The prefix alone: white space ends it.
                    Q::Empty => format!("{} ", p.text()),
                    inner => format!("{}{}", p.text(), print(inner, minimal)),
                },
                Q::Not(inner) if is_composite(inner) => format!("!<{}>", print(inner, minimal)),
                Q::Not(inner) => format!("!{}", print(inner, minimal)),
                Q::And(items) => {
                    let text: Vec<String> = items.iter().map(|i| print(i, minimal)).collect();
                    if minimal {
                        text.join(" ")
                    } else {
                        format!("<{}>", text.join(" "))
                    }
                }
                Q::Or(items) => {
                    let text: Vec<String> = items
                        .iter()
                        .map(|i| match i {
                            Q::And(_) if minimal => format!("<{}>", print(i, minimal)),
                            _ => print(i, minimal),
                        })
                        .collect();
                    // A regular expression eats a `|` joined to it.
                    let sep = if text.iter().any(|t| {
                        t.contains(":<")
                            || t.contains(":!<")
                            || t.ends_with(' ')
                            || t.contains("regex:")
                    }) {
                        " | "
                    } else {
                        "|"
                    };
                    if minimal {
                        text.join(sep)
                    } else {
                        format!("<{}>", text.join(sep))
                    }
                }
            }
        }

        /// The entry as the reference reads it.
        struct Seen {
            name: String,
            /// The full path as typed.
            full: String,
            is_dir: bool,
            size: Option<u64>,
            mtime: Option<i64>,
            /// The parent folder's full path, folded, and every ancestor's.
            parent: String,
            ancestors: Vec<String>,
            prefix: String,
            /// The folders above, up to the root and with it.
            parents: u64,
            /// A folder's files and folders.
            children: Option<(u64, u64)>,
        }

        /// The modifiers a term has, the outermost set first.
        #[derive(Clone, Copy, Default)]
        struct Ctx {
            case: Option<bool>,
            path: Option<bool>,
            ww: Option<bool>,
            prefix: Option<bool>,
            suffix: Option<bool>,
            whole: Option<bool>,
            start: Option<bool>,
            end: Option<bool>,
            punct: Option<bool>,
            space: Option<bool>,
            diacritics: Option<bool>,
            folder: bool,
        }

        fn word(c: char) -> bool {
            c.is_alphanumeric() || unicode_normalization::char::is_combining_mark(c)
        }

        /// Whether `needle` stands in `hay` where `ctx` says: at each
        /// placement of its characters at positions `p0 < p1 < ...`, only
        /// characters a skip allows lie between them, and the anchors hold.
        fn holds_at(hay: &[char], needle: &[char], ctx: Ctx) -> bool {
            let on = |o: Option<bool>| o == Some(true);
            let skippable = |c: char| {
                (on(ctx.punct) && !word(c) && !c.is_whitespace())
                    || (on(ctx.space) && c.is_whitespace())
            };
            let (start, end) = (on(ctx.whole) || on(ctx.start), on(ctx.whole) || on(ctx.end));
            let (ws, we) = (on(ctx.ww) || on(ctx.prefix), on(ctx.ww) || on(ctx.suffix));
            fn place(
                hay: &[char],
                needle: &[char],
                k: usize,
                from: usize,
                skippable: &dyn Fn(char) -> bool,
                done: &mut dyn FnMut(usize) -> bool,
            ) -> bool {
                if k == needle.len() {
                    return done(from);
                }
                for p in from..hay.len() {
                    if hay[p] == needle[k] && place(hay, needle, k + 1, p + 1, skippable, done) {
                        return true;
                    }
                    if !skippable(hay[p]) {
                        return false;
                    }
                }
                false
            }
            (0..hay.len()).any(|s| {
                hay[s] == needle[0]
                    && (!start || hay[..s].iter().all(|&c| skippable(c)))
                    && (!ws || s == 0 || !word(hay[s - 1]))
                    && place(hay, needle, 1, s + 1, &skippable, &mut |after| {
                        (!end || hay[after..].iter().all(|&c| skippable(c)))
                            && (!we || after == hay.len() || !word(hay[after]))
                    })
            })
        }

        fn fold_str(s: &str) -> String {
            crate::wtf8::to_string_lossy(&crate::fold::fold(s.as_bytes()))
        }

        fn fold_exact_str(s: &str) -> String {
            crate::wtf8::to_string_lossy(&crate::fold::fold_exact(s.as_bytes()))
        }

        fn utf16(s: &str) -> u64 {
            s.encode_utf16().count() as u64
        }

        /// The location of a full path: up to its last separator.
        fn location(full: &str) -> &str {
            full.rfind('\\').map_or("", |i| &full[..i])
        }

        /// A name's stem, extension (`None` for a folder or a name with no
        /// dot), as Everything parts them.
        fn stem(e: &Seen) -> &str {
            match e.name.rfind('.') {
                Some(dot) if !e.is_dir => &e.name[..dot],
                _ => &e.name,
            }
        }

        fn extension(e: &Seen) -> Option<&str> {
            (!e.is_dir)
                .then(|| e.name.rfind('.').map(|dot| &e.name[dot + 1..]))
                .flatten()
        }

        /// Whether `q` matches the entry, worked out plainly. `case`: the
        /// query matches case.
        fn eval(q: &Q, e: &Seen, case: bool, ctx: Ctx) -> bool {
            match q {
                Q::Not(inner) => !eval(inner, e, case, ctx),
                Q::And(items) => items.iter().all(|i| eval(i, e, case, ctx)),
                Q::Or(items) => items.iter().any(|i| eval(i, e, case, ctx)),
                Q::Prefixed(p, inner) => {
                    let mut c = ctx;
                    let set = |slot: &mut Option<bool>, on: bool| {
                        slot.get_or_insert(on);
                    };
                    match p {
                        P::Case(on) => set(&mut c.case, *on),
                        P::Path(on) => set(&mut c.path, *on),
                        P::Ww => set(&mut c.ww, true),
                        P::Prefix => set(&mut c.prefix, true),
                        P::Suffix => set(&mut c.suffix, true),
                        P::Whole => set(&mut c.whole, true),
                        P::StartWith => set(&mut c.start, true),
                        P::EndWith => set(&mut c.end, true),
                        P::NoPunc => set(&mut c.punct, true),
                        P::NoWs => set(&mut c.space, true),
                        P::Diacritics => set(&mut c.diacritics, true),
                        // ANDed with what follows.
                        P::File | P::NoFolder => return !e.is_dir && eval(inner, e, case, ctx),
                        P::Dir => return e.is_dir && eval(inner, e, case, ctx),
                        // Carried down to every term below.
                        P::Folder | P::NoFile => c.folder = true,
                    }
                    eval(inner, e, case, c)
                }
                Q::Empty => {
                    let whole = ctx.whole == Some(true)
                        || (ctx.start == Some(true) && ctx.end == Some(true));
                    (!ctx.folder || e.is_dir) && !whole
                }
                Q::Leaf(l) => (!ctx.folder || e.is_dir) && eval_leaf(l, e, case, ctx),
            }
        }

        fn eval_leaf(l: &L, e: &Seen, case: bool, ctx: Ctx) -> bool {
            let case = ctx.case.unwrap_or(case);
            let accents = ctx.diacritics == Some(true);
            let fold = |s: &str| {
                if case {
                    s.to_string()
                } else if accents {
                    fold_exact_str(s)
                } else {
                    fold_str(s)
                }
            };
            match l {
                L::Lone => e.is_dir,
                L::Drive(d) if ctx.path != Some(false) => {
                    e.prefix
                        .chars()
                        .next()
                        .is_some_and(|c| c.eq_ignore_ascii_case(d))
                        && e.prefix.as_bytes().get(1) == Some(&b':')
                }
                L::Drive(d) => fold(&e.name).contains(&fold(&format!("{d}:"))),
                // A separator alone keeps folders; a letter and a colon is a
                // drive, unless `nopath:` keeps the term on the name.
                L::Text(t) if t == "\\" || t == "/" => e.is_dir,
                L::Text(t)
                    if ctx.path != Some(false)
                        && t.len() == 2
                        && t.as_bytes()[0].is_ascii_alphabetic()
                        && t.as_bytes()[1] == b':' =>
                {
                    eval_leaf(&L::Drive(t.as_bytes()[0] as char), e, case, ctx)
                }
                L::Text(t) => {
                    let t = t.replace('/', "\\");
                    let wildcard = t.contains(['*', '?']);
                    let path = ctx
                        .path
                        .unwrap_or(t.contains('\\') || (wildcard && t.contains("**")));
                    if wildcard {
                        if path {
                            reference_sub_path(&fold(&t), &fold(&e.full))
                        } else {
                            reference_match(&chars(&fold(&t)), &chars(&fold(&e.name)), false)
                        }
                    } else {
                        let hay = if path { &e.full } else { &e.name };
                        holds_at(&chars(&fold(hay)), &chars(&fold(&t)), ctx)
                    }
                }
                L::Part(f, v) => {
                    let full = e.full.as_str();
                    let target: Option<&str> = match f {
                        Part::Name => Some(&e.name),
                        Part::Stem => Some(stem(e)),
                        Part::Extension => extension(e),
                        Part::FullPath => Some(full),
                        Part::Location => Some(location(full)),
                        Part::ParentName => {
                            let loc = location(full);
                            Some(loc.rfind('\\').map_or(loc, |i| &loc[i + 1..]))
                        }
                    };
                    let Some(target) = target else {
                        return false;
                    };
                    let v = if *f == Part::FullPath || *f == Part::Location {
                        v.replace('/', "\\")
                    } else {
                        v.clone()
                    };
                    if v.is_empty() {
                        return matches!(
                            f,
                            Part::Name | Part::FullPath | Part::Location | Part::ParentName
                        ) || !target.is_empty();
                    }
                    if v.contains(['*', '?']) {
                        if *f == Part::FullPath {
                            reference_sub_path(&fold(&v), &fold(target))
                        } else {
                            reference_match(&chars(&fold(&v)), &chars(&fold(target)), false)
                        }
                    } else {
                        holds_at(&chars(&fold(target)), &chars(&fold(&v)), ctx)
                    }
                }
                L::Ext(values) => {
                    if e.is_dir {
                        return false;
                    }
                    let case = ctx.case.unwrap_or(false);
                    let fold = |s: &str| {
                        if case {
                            s.to_string()
                        } else if accents {
                            fold_exact_str(s)
                        } else {
                            fold_str(s)
                        }
                    };
                    let mut values: Vec<&str> = values.iter().map(String::as_str).collect();
                    while values.len() > 1 && values.last() == Some(&"") {
                        values.pop();
                    }
                    let values: Vec<&str> = values
                        .iter()
                        .map(|v| {
                            v.strip_prefix("*.")
                                .or_else(|| v.strip_prefix('.'))
                                .unwrap_or(v)
                        })
                        .collect();
                    let name = fold(&e.name);
                    values.iter().any(|v| {
                        let v = fold(v);
                        if v.is_empty() {
                            !name.contains('.')
                        } else if v.contains(['*', '?']) {
                            let ext = name.rsplit_once('.').map_or("", |(_, x)| x);
                            reference_match(&chars(&v), &chars(ext), false)
                        } else {
                            name.ends_with(&format!(".{v}"))
                        }
                    })
                }
                // A value's own `!` keeps the entries that have a value,
                // and not that one.
                L::Size(values, neg) => {
                    !e.is_dir
                        && e.size
                            .is_some_and(|s| values.iter().any(|(_, r)| r.contains(s)) != *neg)
                }
                L::SizeUnknown => !e.is_dir && e.size.is_none(),
                L::Len(f, _, range, neg) => {
                    let len = match f {
                        LenFn::Len if ctx.path == Some(true) => Some(utf16(&e.full)),
                        LenFn::Len | LenFn::Name => Some(utf16(&e.name)),
                        LenFn::FullPath => Some(utf16(&e.full)),
                        LenFn::Location => Some(utf16(location(&e.full))),
                        LenFn::Extension if e.is_dir => None,
                        LenFn::Extension => Some(extension(e).map_or(0, utf16)),
                    };
                    len.is_some_and(|l| range.contains(l) != *neg)
                }
                L::Count(f, _, range, neg) => {
                    let n = match f {
                        CountFn::Parents(_) => Some(e.parents),
                        CountFn::Children => e.children.map(|(files, folders)| files + folders),
                        CountFn::ChildFiles => e.children.map(|(files, _)| files),
                        CountFn::ChildFolders => e.children.map(|(_, folders)| folders),
                    };
                    n.is_some_and(|n| range.contains(n) != *neg)
                }
                // `empty:` alone: folders with no children, and an empty
                // term, which under `whole:` matches nothing.
                L::EmptyFolder => {
                    !(ctx.whole == Some(true) || (ctx.start == Some(true) && ctx.end == Some(true)))
                        && e.children == Some((0, 0))
                }
                L::Date(_, range, neg) => e
                    .mtime
                    .is_some_and(|t| t >= 0 && range.contains(t as u64) != *neg),
                // On the name or the full path as stored.
                L::Regex(pattern) => {
                    let path = ctx.path.unwrap_or(pattern.contains("\\\\"));
                    regex::RegexBuilder::new(pattern)
                        .case_insensitive(!case)
                        .build()
                        .unwrap()
                        .is_match(if path { &e.full } else { &e.name })
                }
                L::Folder(false, _, folded) => e.parent == *folded,
                L::Folder(true, _, folded) => e.ancestors.contains(folded),
                // `exe:` alone: its extensions, and an empty term, which
                // under `whole:` matches nothing.
                L::Macro => {
                    !(ctx.whole == Some(true) || (ctx.start == Some(true) && ctx.end == Some(true)))
                        && !e.is_dir
                        && fold_str(&e.name).rsplit_once('.').is_some_and(|(_, x)| {
                            [
                                "bat", "cmd", "exe", "msi", "msp", "msu", "ps1", "scr", "vbs",
                            ]
                            .contains(&x)
                        })
                }
            }
        }

        /// The page `sort` gives the matches `found`, ordered plainly.
        fn naive_order(idx: &VolumeIndex, found: &[EntryIdx], sort: SortSpec) -> Vec<EntryIdx> {
            let fold_name = |e: EntryIdx| crate::fold::fold(idx.entry(e).name());
            let parent = |e: EntryIdx| {
                let mut path = Vec::new();
                idx.path_of(idx.entry(e).parent(), &mut path);
                crate::fold::fold(&path)
            };
            /// A match and what it sorts by: a number, then two byte strings.
            type Keyed = ((u64, Vec<u8>, Vec<u8>), EntryIdx);
            let mut keyed: Vec<Keyed> = found
                .iter()
                .map(|&e| {
                    let entry = idx.entry(e);
                    let key = match sort.key {
                        SortKey::Name => (0, Vec::new(), fold_name(e)),
                        SortKey::Path => (0, parent(e), fold_name(e)),
                        SortKey::Size => {
                            (size_key(entry.size(), sort.dir), Vec::new(), fold_name(e))
                        }
                        SortKey::DateModified => {
                            (mtime_key(entry.mtime(), sort.dir), Vec::new(), fold_name(e))
                        }
                    };
                    (key, e)
                })
                .collect();
            keyed.sort();
            if sort.dir == SortDir::Desc {
                keyed.reverse();
            }
            keyed.into_iter().map(|(_, e)| e).collect()
        }

        /// The folded full path of a folder, as `parent:` names it: the
        /// prefix, its separator dropped, for the root.
        fn folder_path(idx: &VolumeIndex, dir: EntryIdx, prefix: &str) -> Option<String> {
            let full = if dir == idx.root() {
                prefix.trim_end_matches('\\').as_bytes().to_vec()
            } else {
                slow_full_path(idx, dir, prefix, false)?
            };
            Some(fold_str(&String::from_utf8(full).ok()?))
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(1024))]

            #[test]
            fn queries_agree_with_a_naive_evaluator(
                nodes in prop::collection::vec(
                    (
                        any::<Index>(),
                        "[aAbBé. _-]{1,4}(\\.(exe|txt|TXT|bat|a))?",
                        any::<bool>(),
                    ),
                    1..30,
                ),
                orphans in prop::collection::vec("[aAbB]{1,3}", 0..3),
                stats in prop::collection::vec(
                    prop::option::of((prop_oneof![0u64..40, 1000u64..2100], 0i64..40)),
                    30,
                ),
                shape in spec(),
                mount in 0usize..5,
                case in prop::bool::weighted(0.15),
                padded in any::<bool>(),
                ordered in any::<bool>(),
                minimal in any::<bool>(),
                offset in 0u32..6,
                limit in prop::option::of(0u32..8),
            ) {
                let (mut idx, entries) = random_tree(&nodes, &orphans, padded);
                // FILETIMEs of nine digits and more, which `dm:` reads as
                // ticks.
                const T0: i64 = 1_000_000_000;
                for (i, stat) in stats.iter().enumerate().take(nodes.len()) {
                    if let Some((size, mtime)) = stat {
                        idx.set_stat(frn(1000 + i as u64), *size, T0 + *mtime);
                    }
                }
                if ordered {
                    idx.optimize_storage();
                }
                let prefix = [r"C:\", r"C:\", r"C:\Mnt\aB\", r"D:\Mnt\aB\", "C:"][mount];
                let typed = |e: EntryIdx| {
                    slow_full_path(&idx, e, prefix, false).map(|p| String::from_utf8(p).unwrap())
                };
                let vocabulary = Vocabulary {
                    names: entries
                        .iter()
                        .map(|&e| String::from_utf8(idx.entry(e).name().to_vec()).unwrap().chars().collect())
                        .collect(),
                    paths: entries.iter().filter_map(|&e| typed(e)).map(|p| p.chars().collect()).collect(),
                    folders: std::iter::once(idx.root())
                        .chain(entries.iter().copied().filter(|&e| idx.entry(e).is_dir()))
                        .filter_map(|d| {
                            if d == idx.root() {
                                Some(prefix.trim_end_matches('\\').to_string())
                            } else {
                                typed(d)
                            }
                        })
                        .collect(),
                    absolute: prefix.ends_with('\\'),
                    mtimes: entries
                        .iter()
                        .filter_map(|&e| idx.entry(e).mtime())
                        .chain([T0 + 5])
                        .collect(),
                };
                prop_assume!(!vocabulary.paths.is_empty());
                let q = materialize(&shape, &vocabulary);
                let mut text = print(&q, minimal);
                if case {
                    text.push_str(" case:");
                }
                let clock = FixedClock::utc(T0);
                let parsed = crate::query::parse_query_at(&text, false, &clock)
                    .unwrap_or_else(|e| panic!("{text}: {e}"));
                // Brackets where precedence needs none change nothing.
                let other = print(&q, !minimal) + if case { " case:" } else { "" };
                prop_assert_eq!(
                    &crate::query::parse_query_at(&other, false, &clock).unwrap(),
                    &parsed,
                    "{} vs {}",
                    text,
                    other
                );

                let mut found: Vec<EntryIdx> = Vec::new();
                for e in idx.live_entries().filter(|&e| !idx.is_synthetic(e)) {
                    let Some(full) = typed(e) else {
                        continue;
                    };
                    let entry = idx.entry(e);
                    let mut ancestors = Vec::new();
                    let mut cur = entry.parent();
                    while cur != NIL {
                        if let Some(p) = folder_path(&idx, cur, prefix) {
                            ancestors.push(p);
                        }
                        cur = idx.entry(cur).parent();
                    }
                    // The entry has a path, so its walk ends at the root.
                    let mut parents = 0u64;
                    let mut cur = entry.parent();
                    while cur != NIL {
                        parents += 1;
                        if cur == idx.root() {
                            break;
                        }
                        cur = idx.entry(cur).parent();
                    }
                    let children = entry.is_dir().then(|| {
                        let of = |dirs: bool| {
                            idx.live_entries()
                                .filter(|&c| !idx.is_synthetic(c) && idx.entry(c).parent() == e)
                                .filter(|&c| idx.entry(c).is_dir() == dirs)
                                .count() as u64
                        };
                        (of(false), of(true))
                    });
                    let seen = Seen {
                        name: String::from_utf8(entry.name().to_vec()).unwrap(),
                        full,
                        is_dir: entry.is_dir(),
                        size: entry.size(),
                        mtime: entry.mtime(),
                        parent: ancestors.first().cloned().unwrap_or_default(),
                        ancestors,
                        prefix: prefix.to_string(),
                        parents,
                        children,
                    };
                    if eval(&q, &seen, case, Ctx::default()) {
                        found.push(e);
                    }
                }
                let total_size = files_size(&idx, found.iter().copied());
                for key in SORT_KEYS {
                    for dir in [SortDir::Asc, SortDir::Desc] {
                        let sort = SortSpec { key, dir };
                        let all = naive_order(&idx, &found, sort);
                        for (offset, limit) in [(offset, limit), (0, Some(0)), (0, None), (1, Some(3))] {
                            let (start, end) = page_bounds(all.len(), offset, limit);
                            for fast in [true, false] {
                                let out = unlimited_refs(&idx, prefix, &parsed, None, sort, offset, limit, true, fast);
                                let ctx = format!("{text:?} under {prefix} {key:?} {dir:?} {offset}+{limit:?} fast {fast}");
                                prop_assert_eq!(out.total, found.len() as u64, "total of {}", ctx);
                                prop_assert_eq!(out.total_size, Some(total_size), "size of {}", ctx);
                                let page: Vec<EntryIdx> = out.refs.iter().map(|r| r.idx).collect();
                                prop_assert_eq!(&page[..], &all[start..end], "page of {}", ctx);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Folders `café` and `cafe`, a decomposed `café` in each and the
    /// rest of Everything's accent pairs (DESIGN.md §10.1).
    /// The tree `w` of the live Everything fixture, at the root: what its
    /// folder index held below its root.
    fn count_index() -> VolumeIndex {
        let mut idx = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
        let tree: &[(u64, u64, &str, bool)] = &[
            (100, 0, "w", true),
            (101, 100, "mywindows", true),
            (102, 101, "c.exe", false),
            (103, 100, "windows", true),
            (104, 103, "cmd.exe", false),
            (105, 103, "emptydir", true),
            (106, 103, "notepad.exe", false),
            (107, 103, "sub", true),
            (108, 107, "deep", true),
            (109, 108, "a.exe", false),
            (110, 103, "system32", true),
            (111, 110, "cmd.exe", false),
            (112, 110, "drivers", true),
            (113, 112, "x.sys", false),
            (114, 100, "windows.old", true),
            (115, 114, "d.exe", false),
            (116, 100, "windows2", true),
            (117, 116, "b.exe", false),
        ];
        for &(rec, parent, name, is_dir) in tree {
            let parent = if parent == 0 {
                NTFS_ROOT_FRN
            } else {
                frn(parent)
            };
            idx.insert_enum(&enum_rec(frn(rec), parent, name, is_dir));
        }
        idx
    }

    /// `parent-count:` (`depth:`, `parents:`), `empty:` and the child counts
    /// answer as a live Everything 1.5.0.1424b did over the same tree (W6):
    /// the root counts as a parent, and only folders have children, so a
    /// value's own `!` keeps folders alone while a NOT of the term keeps
    /// the files too.
    #[test]
    fn counts_answer_as_everything_does() {
        let idx = count_index();
        let check = |query: &str, want: &[&str]| {
            let mut got = run(&idx, query);
            got.sort();
            let mut want: Vec<String> = want.iter().map(|w| w.to_string()).collect();
            want.sort();
            assert_eq!(got, want, "{query}");
        };
        check("parents:1", &["w"]);
        check(
            r"parent-count:2 \w\",
            &[
                r"w\mywindows",
                r"w\windows",
                r"w\windows.old",
                r"w\windows2",
            ],
        );
        check(
            r"depth:3 \w\",
            &[
                r"w\windows\emptydir",
                r"w\windows\sub",
                r"w\windows\system32",
                r"w\windows2\b.exe",
                r"w\mywindows\c.exe",
                r"w\windows\cmd.exe",
                r"w\windows.old\d.exe",
                r"w\windows\notepad.exe",
            ],
        );
        check(
            r"parent-count:>3 \w\",
            &[
                r"w\windows\sub\deep",
                r"w\windows\system32\drivers",
                r"w\windows\sub\deep\a.exe",
                r"w\windows\system32\cmd.exe",
                r"w\windows\system32\drivers\x.sys",
            ],
        );
        check(
            r"parentcount:2..3 \windows\",
            &[
                r"w\windows\emptydir",
                r"w\windows\sub",
                r"w\windows\system32",
                r"w\windows\cmd.exe",
                r"w\windows\notepad.exe",
            ],
        );
        check(r"empty: \w\", &[r"w\windows\emptydir"]);
        check("empty:emp", &[r"w\windows\emptydir"]);
        check(
            r"!empty: \windows\",
            &[
                r"w\windows\sub\deep",
                r"w\windows\system32\drivers",
                r"w\windows\sub",
                r"w\windows\system32",
                r"w\windows\sub\deep\a.exe",
                r"w\windows\cmd.exe",
                r"w\windows\system32\cmd.exe",
                r"w\windows\notepad.exe",
                r"w\windows\system32\drivers\x.sys",
            ],
        );
        check(r"child-count:0 \w\", &[r"w\windows\emptydir"]);
        let ones = [
            r"w\windows\sub\deep",
            r"w\windows\system32\drivers",
            r"w\mywindows",
            r"w\windows\sub",
            r"w\windows.old",
            r"w\windows2",
        ];
        check(r"child-count:1 \w\", &ones);
        check(r"childcount:2 \w\", &[r"w\windows\system32"]);
        check(r"child-count:>2 \w\", &[r"w\windows"]);
        check(
            r"child-file-count:1 \w\",
            &[
                r"w\windows\sub\deep",
                r"w\windows\system32\drivers",
                r"w\mywindows",
                r"w\windows\system32",
                r"w\windows.old",
                r"w\windows2",
            ],
        );
        check(
            r"child-folder-count:1 \w\",
            &[r"w\windows\sub", r"w\windows\system32"],
        );
        check(
            r"child-file-count:0 \w\",
            &[r"w\windows\emptydir", r"w\windows\sub"],
        );
        let mut not_two: Vec<&str> = ones.to_vec();
        not_two.extend([r"w\windows\emptydir", r"w\windows"]);
        check(r"child-count:!2 \w\", &not_two);
        let mut all_but: Vec<&str> = not_two.clone();
        all_but.extend([
            r"w\windows\sub\deep\a.exe",
            r"w\windows2\b.exe",
            r"w\mywindows\c.exe",
            r"w\windows\cmd.exe",
            r"w\windows\system32\cmd.exe",
            r"w\windows.old\d.exe",
            r"w\windows\notepad.exe",
            r"w\windows\system32\drivers\x.sys",
        ]);
        check(r"!child-count:2 \w\", &all_but);
        check(r"file: child-count:0 \w\", &[]);
    }

    /// The live Everything fixture's folder index stood at a folder
    /// `C:\Users\musta\...\gozd\ev\fx` (a temporary folder, shortened
    /// here): Everything names that root by its path, which
    /// `ancestor-name:` reads, so its answers hold for this one alike.
    const FIXTURE_ROOT: &str = r"C:\Users\musta\scratch\gozd\ev\fx\";

    /// [`count_index`]'s tree `w` and a few more of the live fixture's
    /// folders: `words` (`!bang.txt`), `p` and `dia` (an accent each way),
    /// `desktop` (names with spaces).
    fn related_index() -> VolumeIndex {
        let mut idx = count_index();
        let tree: &[(u64, u64, &str, bool)] = &[
            (200, 0, "words", true),
            (201, 200, "!bang.txt", false),
            (202, 200, "ever.txt", false),
            (203, 200, "wow!.txt", false),
            (210, 0, "p", true),
            (211, 210, "aéb.txt", false),
            (212, 210, "a b.txt", false),
            (220, 0, "dia", true),
            (221, 220, "aeble.txt", false),
            (222, 220, "café.txt", false),
            (230, 0, "desktop", true),
            (231, 230, "First Level First", true),
            (232, 231, "f.txt", false),
            (233, 231, "inner", true),
            (234, 233, "g.txt", false),
            (235, 230, "other", true),
            (236, 235, "h.txt", false),
        ];
        for &(rec, parent, name, is_dir) in tree {
            let parent = if parent == 0 {
                NTFS_ROOT_FRN
            } else {
                frn(parent)
            };
            idx.insert_enum(&enum_rec(frn(rec), parent, name, is_dir));
        }
        idx.optimize_storage();
        idx
    }

    /// `child:`, `descendant:`, `sibling:` and `ancestor-name:` (and the
    /// `-file:` and `-folder:` forms) answer as a live Everything
    /// 1.5.0.1424b did over the same tree (W7, 65 queries): the value is a
    /// term on the other entry's name with the term's modifiers (`whole:`,
    /// `case:`, `diacritics:`, `regex:`), a `;` list or a group of values;
    /// its leading `!` is text; a sibling is another entry, not the entry
    /// itself; and the root is named by its path, so `ancestor-name:musta`
    /// holds for every entry while `whole:ancestor-name:fx` holds for none.
    /// Only the root, which goz never lists and Everything did, is left out
    /// of the `descendant:` answers.
    #[test]
    fn related_searches_answer_as_everything_does() {
        let idx = related_index();
        let check = |query: &str, want: &[&str]| {
            let parsed = parse_query(query).unwrap_or_else(|e| panic!("{query}: {e}"));
            let out = run_query(
                &idx,
                FIXTURE_ROOT,
                &parsed,
                None,
                SortSpec::default(),
                0,
                None,
            );
            let mut got: Vec<String> = out
                .hits
                .iter()
                .map(|h| crate::wtf8::to_string_lossy(&h.path))
                .collect();
            got.sort();
            let mut want: Vec<String> = want.iter().map(|w| w.to_string()).collect();
            want.sort();
            assert_eq!(got, want, "{query}");
        };
        let w_all: &[&str] = &[
            r"w\windows\sub\deep",
            r"w\windows\system32\drivers",
            r"w\windows\emptydir",
            r"w\mywindows",
            r"w\windows\sub",
            r"w\windows\system32",
            r"w\windows",
            r"w\windows.old",
            r"w\windows2",
            r"w\windows\sub\deep\a.exe",
            r"w\windows2\b.exe",
            r"w\mywindows\c.exe",
            r"w\windows\cmd.exe",
            r"w\windows\system32\cmd.exe",
            r"w\windows.old\d.exe",
            r"w\windows\notepad.exe",
            r"w\windows\system32\drivers\x.sys",
        ];
        let but = |out: &[&str]| -> Vec<&str> {
            w_all.iter().copied().filter(|p| !out.contains(p)).collect()
        };
        let cmd_dirs = [r"w\windows\system32", r"w\windows"];
        let exe_dirs = [
            r"w\windows\sub\deep",
            r"w\mywindows",
            r"w\windows\system32",
            r"w\windows",
            r"w\windows.old",
            r"w\windows2",
        ];
        let nonempty = [
            r"w\windows\sub\deep",
            r"w\windows\system32\drivers",
            r"w\mywindows",
            r"w\windows\sub",
            r"w\windows\system32",
            r"w\windows",
            r"w\windows.old",
            r"w\windows2",
        ];

        // child:
        check(r"child:cmd \w\", &cmd_dirs);
        check(r"child:*.exe \w\", &exe_dirs);
        check(r"child:exe \w\", &exe_dirs);
        check("child:windows", &["w"]);
        check(r"child-file:sub \w\", &[]);
        check(r"child-folder:sub \w\", &[r"w\windows"]);
        check(r"childfolder:sub \w\", &[r"w\windows"]);
        check(r"!child:cmd \w\", &but(&cmd_dirs));
        check(r"child: \w\", &nonempty);
        check(r"whole:child:cmd \w\", &[]);
        check(r"whole:child:cmd.exe \w\", &cmd_dirs);
        check(r"case:child:CMD \w\", &[]);
        check(r"child:CMD \w\", &cmd_dirs);
        check(r"child:cmd;notepad \w\", &cmd_dirs);
        check(r"child:sub\deep \w\", &[]);
        check("child:aeb", &["dia", "p"]);
        check("diacritics:child:aeb", &["dia"]);
        check("child:aéb", &["dia", "p"]);
        check(r"file: child:cmd \w\", &[]);
        check(
            r"child:<cmd|x.sys> \w\",
            &[
                r"w\windows\system32\drivers",
                r"w\windows\system32",
                r"w\windows",
            ],
        );
        check(r"child:!cmd \w\", &[]);
        check(r"child:!!cmd \w\", &[]);
        check("child:!bang", &["words"]);
        check("whole:child:!bang.txt", &["words"]);
        check("child-file:windows", &[]);
        check("child-folder:windows", &["w"]);
        check(r#"child:"first level first""#, &["desktop"]);
        check(r"regex:child:^cmd \w\", &cmd_dirs);
        check(r"child:regex:^cmd \w\", &[]);

        // descendant:
        let to_x = [
            r"w\windows\system32\drivers",
            r"w\windows\system32",
            "w",
            r"w\windows",
        ];
        check("descendant:x.sys", &to_x);
        check(
            r"descendant-folder:deep \w\",
            &[r"w\windows\sub", r"w\windows"],
        );
        check(r"descendant-file:deep \w\", &[]);
        check(
            "descendant:a.exe",
            &[r"w\windows\sub\deep", r"w\windows\sub", "w", r"w\windows"],
        );
        check(
            r"!descendant:exe \w\",
            &[
                r"w\windows\system32\drivers",
                r"w\windows\emptydir",
                r"w\windows\sub\deep\a.exe",
                r"w\windows2\b.exe",
                r"w\mywindows\c.exe",
                r"w\windows\cmd.exe",
                r"w\windows\system32\cmd.exe",
                r"w\windows.old\d.exe",
                r"w\windows\notepad.exe",
                r"w\windows\system32\drivers\x.sys",
            ],
        );
        check(r"descendant: \w\", &nonempty);
        check("descendant:windows", &["w"]);
        check("descendant:!bang", &["words"]);
        check("descendant-file:windows", &[]);

        // sibling:
        check(
            r"sibling:notepad \w\",
            &[
                r"w\windows\emptydir",
                r"w\windows\sub",
                r"w\windows\system32",
                r"w\windows\cmd.exe",
            ],
        );
        let cmd_siblings = [
            r"w\windows\system32\drivers",
            r"w\windows\emptydir",
            r"w\windows\sub",
            r"w\windows\system32",
            r"w\windows\notepad.exe",
        ];
        check(r"sibling:cmd \w\", &cmd_siblings);
        check(r"sibling-file:cmd \w\", &cmd_siblings);
        check(
            r"sibling-folder:sub \w\",
            &[
                r"w\windows\emptydir",
                r"w\windows\system32",
                r"w\windows\cmd.exe",
                r"w\windows\notepad.exe",
            ],
        );
        check(
            "sibling:windows",
            &[
                r"w\mywindows",
                r"w\windows",
                r"w\windows.old",
                r"w\windows2",
            ],
        );
        check(
            "whole:sibling:windows",
            &[r"w\mywindows", r"w\windows.old", r"w\windows2"],
        );
        check("sibling:c.exe", &[]);
        check(
            r"sibling: \w\",
            &[
                r"w\windows\system32\drivers",
                r"w\windows\emptydir",
                r"w\mywindows",
                r"w\windows\sub",
                r"w\windows\system32",
                r"w\windows",
                r"w\windows.old",
                r"w\windows2",
                r"w\windows\cmd.exe",
                r"w\windows\system32\cmd.exe",
                r"w\windows\notepad.exe",
            ],
        );
        check(r"!sibling:cmd \w\", &but(&cmd_siblings));
        check(
            r"sibling:*.exe \w\",
            &[
                r"w\windows\system32\drivers",
                r"w\windows\emptydir",
                r"w\windows\sub",
                r"w\windows\system32",
                r"w\windows\cmd.exe",
                r"w\windows\notepad.exe",
            ],
        );
        check(
            r"sibling:!bang \words\",
            &[r"words\ever.txt", r"words\wow!.txt"],
        );
        check(r"sibling-file:sub \w\", &[]);
        check(r"sibling-folder:cmd \w\", &[]);

        // ancestor-name:
        let below_windows = [
            r"w\windows\sub\deep",
            r"w\windows\system32\drivers",
            r"w\windows\emptydir",
            r"w\windows\sub",
            r"w\windows\system32",
            r"w\windows\sub\deep\a.exe",
            r"w\windows\cmd.exe",
            r"w\windows\system32\cmd.exe",
            r"w\windows\notepad.exe",
            r"w\windows\system32\drivers\x.sys",
        ];
        let mut below_any: Vec<&str> = below_windows.to_vec();
        below_any.extend([
            r"w\windows2\b.exe",
            r"w\mywindows\c.exe",
            r"w\windows.old\d.exe",
        ]);
        check(r"ancestor-name:windows \w\", &below_any);
        check("whole:ancestor-name:windows", &below_windows);
        check(
            "ancestor-name:sub",
            &[r"w\windows\sub\deep", r"w\windows\sub\deep\a.exe"],
        );
        let everything: Vec<String> = {
            let parsed = parse_query("").unwrap();
            let out = run_query(
                &idx,
                FIXTURE_ROOT,
                &parsed,
                None,
                SortSpec::default(),
                0,
                None,
            );
            out.hits
                .iter()
                .map(|h| crate::wtf8::to_string_lossy(&h.path))
                .collect()
        };
        let everything: Vec<&str> = everything.iter().map(String::as_str).collect();
        check("ancestor-name:fx", &everything);
        check("whole:ancestor-name:fx", &[]);
        check(r"ancestor-name:musta \w\", w_all);
        check("whole:ancestor-name:w", w_all);
        check(r"ancestor-name:w* \w\", w_all);
        check(
            r"!ancestor-name:windows \w\",
            &[
                r"w\mywindows",
                r"w\windows",
                r"w\windows.old",
                r"w\windows2",
            ],
        );
        check(
            "ancestor-name:system32;sub",
            &[
                r"w\windows\sub\deep",
                r"w\windows\system32\drivers",
                r"w\windows\sub\deep\a.exe",
                r"w\windows\system32\cmd.exe",
                r"w\windows\system32\drivers\x.sys",
            ],
        );
        check(r"ancestor-name: \w\", w_all);
        check(
            "ancestorname:sub",
            &[r"w\windows\sub\deep", r"w\windows\sub\deep\a.exe"],
        );
        check(r"ancestor-name:!windows \w\", &[]);
        check(
            r#"ancestor-name:"First Level" \desktop\"#,
            &[
                r"desktop\First Level First\inner",
                r"desktop\First Level First\f.txt",
                r"desktop\First Level First\inner\g.txt",
            ],
        );
    }

    /// The live fixture's tree `r8` (W8): record, parent record (0 for the
    /// root), name, folder or not, size, and the day of its date modified,
    /// 12:00 UTC here as it was 12:00 local time there.
    const R8: &[(u64, u64, &str, bool, u64, &str)] = &[
        (300, 0, "r8", true, 0, "2026-11-11"),
        (301, 300, "proj", true, 0, "2023-03-10"),
        (302, 301, ".gitignore", false, 10, "2024-01-15"),
        (303, 301, "src", true, 0, "2019-05-05"),
        (304, 303, "main.rs", false, 2000, "2025-06-01"),
        (305, 303, "util", true, 0, "2022-02-02"),
        (306, 305, "lib.rs", false, 5000, "2025-06-02"),
        (307, 305, "deep", true, 0, "2020-07-07"),
        (308, 307, "x.txt", false, 0, "2021-08-08"),
        (309, 301, "docs", true, 0, "2023-04-04"),
        (310, 309, "readme.md", false, 1500, "2024-09-09"),
        (311, 300, "other", true, 0, "2018-01-01"),
        (312, 311, "src", true, 0, "2019-06-06"),
        (313, 312, "app.rs", false, 100, "2016-01-01"),
        (314, 311, "notes.txt", false, 2048, "2025-06-03"),
        (315, 300, "solo", true, 0, "2017-01-01"),
        (316, 315, "only.txt", false, 1, "2015-05-05"),
        (317, 300, "empty", true, 0, "2014-04-04"),
        (318, 300, "mixed", true, 0, "2013-03-03"),
        (319, 318, "f1.bin", false, 1024, "2012-02-02"),
        (320, 318, "f2.bin", false, 1025, "2012-02-03"),
        (321, 318, "sub1", true, 0, "2011-01-01"),
        (322, 318, "sub2", true, 0, "2010-10-10"),
        (323, 322, "g.bin", false, 4096, "2025-06-04"),
    ];

    /// [`related_index`] and the tree `r8` of the live fixture, with its
    /// sizes and dates.
    fn w8_index() -> VolumeIndex {
        use crate::query::dates::tests::ft;
        let mut idx = related_index();
        for &(rec, parent, name, is_dir, _, _) in R8 {
            let parent = if parent == 0 {
                NTFS_ROOT_FRN
            } else {
                frn(parent)
            };
            idx.insert_enum(&enum_rec(frn(rec), parent, name, is_dir));
        }
        for &(rec, _, _, _, size, day) in R8 {
            idx.set_stat(frn(rec), size, ft(&format!("{day}T12:00:00")));
        }
        idx.optimize_storage();
        idx
    }

    /// `parent-child:`, `parent-sibling:`, `ancestor-child:`,
    /// `ancestor-sibling:` (and their `-file:` and `-folder:` forms),
    /// `descendant-count:`, `sibling-count:` (and their `-file-` and
    /// `-folder-` forms), `child-file-size:`, `child-date-modified:` (and
    /// its forms and aliases), `total-child-size:` and the `-name:` aliases
    /// of `child:` and `sibling:` answer as a live Everything 1.5.0.1424b
    /// did over the same tree (W8, 102 queries): a parent's or an
    /// ancestor's own name counts among its siblings, the root counts as an
    /// ancestor, a value's own `!` keeps folders alone for the descendant
    /// counts and the child size and date tests, and the empty value of a
    /// count is any count but 0. What Everything answered from folder
    /// sizes, and a `$stem:` substitution, goz refuses.
    #[test]
    fn related_entry_functions_answer_as_everything_does() {
        use crate::query::dates::tests::{FixedClock, ft};
        use crate::query::parse::{QueryError, parse_query_at};
        let idx = w8_index();
        let clock = FixedClock::utc(ft("2026-11-11T12:00:00"));
        let check = |query: &str, want: &[&str]| {
            let parsed =
                parse_query_at(query, false, &clock).unwrap_or_else(|e| panic!("{query}: {e}"));
            let out = run_query(
                &idx,
                FIXTURE_ROOT,
                &parsed,
                None,
                SortSpec::default(),
                0,
                None,
            );
            let mut got: Vec<String> = out
                .hits
                .iter()
                .map(|h| crate::wtf8::to_string_lossy(&h.path))
                .collect();
            got.sort();
            let mut want: Vec<String> = want.iter().map(|w| w.to_string()).collect();
            want.sort();
            assert_eq!(got, want, "{query}");
        };
        let all = [
            r"r8\proj\src\util\deep",
            r"r8\proj\docs",
            r"r8\empty",
            r"r8\mixed",
            r"r8\other",
            r"r8\proj",
            r"r8\solo",
            r"r8\other\src",
            r"r8\proj\src",
            r"r8\mixed\sub1",
            r"r8\mixed\sub2",
            r"r8\proj\src\util",
            r"r8\proj\.gitignore",
            r"r8\other\src\app.rs",
            r"r8\mixed\f1.bin",
            r"r8\mixed\f2.bin",
            r"r8\mixed\sub2\g.bin",
            r"r8\proj\src\util\lib.rs",
            r"r8\proj\src\main.rs",
            r"r8\other\notes.txt",
            r"r8\solo\only.txt",
            r"r8\proj\docs\readme.md",
            r"r8\proj\src\util\deep\x.txt",
        ];
        check(r"\r8\", &all);
        check(
            r"parent-sibling:src \r8\",
            &[
                r"r8\proj\src\util",
                r"r8\other\src\app.rs",
                r"r8\proj\src\main.rs",
                r"r8\proj\docs\readme.md",
            ],
        );
        check(
            r"parent-sibling:.gitignore \r8\",
            &[
                r"r8\proj\src\util",
                r"r8\proj\src\main.rs",
                r"r8\proj\docs\readme.md",
            ],
        );
        check(
            r#"whole:parent-sibling:".gitignore" \r8\"#,
            &[
                r"r8\proj\src\util",
                r"r8\proj\src\main.rs",
                r"r8\proj\docs\readme.md",
            ],
        );
        check(r"parent-sibling-file:src \r8\", &[]);
        check(
            r"parent-sibling-folder:docs \r8\",
            &[
                r"r8\proj\src\util",
                r"r8\proj\src\main.rs",
                r"r8\proj\docs\readme.md",
            ],
        );
        check(
            r"parent-sibling-file:gitignore \r8\",
            &[
                r"r8\proj\src\util",
                r"r8\proj\src\main.rs",
                r"r8\proj\docs\readme.md",
            ],
        );
        check(
            r"parentsibling:docs \r8\",
            &[
                r"r8\proj\src\util",
                r"r8\proj\src\main.rs",
                r"r8\proj\docs\readme.md",
            ],
        );
        check(r"parent-sibling: \r8\", &all);
        check(
            r"!parent-sibling:src \r8\",
            &[
                r"r8\proj\src\util\deep",
                r"r8\proj\docs",
                r"r8\empty",
                r"r8\mixed",
                r"r8\other",
                r"r8\proj",
                r"r8\solo",
                r"r8\other\src",
                r"r8\proj\src",
                r"r8\mixed\sub1",
                r"r8\mixed\sub2",
                r"r8\proj\.gitignore",
                r"r8\mixed\f1.bin",
                r"r8\mixed\f2.bin",
                r"r8\mixed\sub2\g.bin",
                r"r8\proj\src\util\lib.rs",
                r"r8\other\notes.txt",
                r"r8\solo\only.txt",
                r"r8\proj\src\util\deep\x.txt",
            ],
        );
        check(
            r"parent-sibling:words \r8\",
            &[
                r"r8\empty",
                r"r8\mixed",
                r"r8\other",
                r"r8\proj",
                r"r8\solo",
            ],
        );
        check(r"parent-sibling:words whole:r8", &[]);
        check(
            r"regex:parent-sibling:^do \r8\",
            &[
                r"r8\proj\src\util",
                r"r8\proj\src\main.rs",
                r"r8\proj\docs\readme.md",
            ],
        );
        check(r"parent-sibling:!docs \r8\", &[]);
        check(
            r"file: parent-sibling:docs \r8\",
            &[r"r8\proj\src\main.rs", r"r8\proj\docs\readme.md"],
        );
        check(
            r"whole:parent-sibling:src \r8\",
            &[
                r"r8\proj\src\util",
                r"r8\other\src\app.rs",
                r"r8\proj\src\main.rs",
                r"r8\proj\docs\readme.md",
            ],
        );
        check(
            r"ancestor-sibling:docs \r8\",
            &[
                r"r8\proj\src\util\deep",
                r"r8\proj\src\util",
                r"r8\proj\src\util\lib.rs",
                r"r8\proj\src\main.rs",
                r"r8\proj\docs\readme.md",
                r"r8\proj\src\util\deep\x.txt",
            ],
        );
        check(
            r"ancestor-sibling:.gitignore \r8\",
            &[
                r"r8\proj\src\util\deep",
                r"r8\proj\src\util",
                r"r8\proj\src\util\lib.rs",
                r"r8\proj\src\main.rs",
                r"r8\proj\docs\readme.md",
                r"r8\proj\src\util\deep\x.txt",
            ],
        );
        check(
            r"ancestor-sibling-file:notes \r8\",
            &[r"r8\other\src\app.rs"],
        );
        check(
            r"ancestor-sibling-folder:other \r8\",
            &[
                r"r8\proj\src\util\deep",
                r"r8\proj\docs",
                r"r8\other\src",
                r"r8\proj\src",
                r"r8\mixed\sub1",
                r"r8\mixed\sub2",
                r"r8\proj\src\util",
                r"r8\proj\.gitignore",
                r"r8\other\src\app.rs",
                r"r8\mixed\f1.bin",
                r"r8\mixed\f2.bin",
                r"r8\mixed\sub2\g.bin",
                r"r8\proj\src\util\lib.rs",
                r"r8\proj\src\main.rs",
                r"r8\other\notes.txt",
                r"r8\solo\only.txt",
                r"r8\proj\docs\readme.md",
                r"r8\proj\src\util\deep\x.txt",
            ],
        );
        check(r"ancestor-sibling-folder:notes \r8\", &[]);
        check(r"ancestor-sibling:words \r8\", &all);
        check(r"ancestor-sibling: \r8\", &all);
        check(
            r"!ancestor-sibling:docs \r8\",
            &[
                r"r8\proj\docs",
                r"r8\empty",
                r"r8\mixed",
                r"r8\other",
                r"r8\proj",
                r"r8\solo",
                r"r8\other\src",
                r"r8\proj\src",
                r"r8\mixed\sub1",
                r"r8\mixed\sub2",
                r"r8\proj\.gitignore",
                r"r8\other\src\app.rs",
                r"r8\mixed\f1.bin",
                r"r8\mixed\f2.bin",
                r"r8\mixed\sub2\g.bin",
                r"r8\other\notes.txt",
                r"r8\solo\only.txt",
            ],
        );
        check(
            r"whole:ancestor-sibling:src \r8\",
            &[
                r"r8\proj\src\util\deep",
                r"r8\proj\src\util",
                r"r8\other\src\app.rs",
                r"r8\proj\src\util\lib.rs",
                r"r8\proj\src\main.rs",
                r"r8\proj\docs\readme.md",
                r"r8\proj\src\util\deep\x.txt",
            ],
        );
        check(
            r"ancestorsibling:solo \r8\",
            &[
                r"r8\proj\src\util\deep",
                r"r8\proj\docs",
                r"r8\other\src",
                r"r8\proj\src",
                r"r8\mixed\sub1",
                r"r8\mixed\sub2",
                r"r8\proj\src\util",
                r"r8\proj\.gitignore",
                r"r8\other\src\app.rs",
                r"r8\mixed\f1.bin",
                r"r8\mixed\f2.bin",
                r"r8\mixed\sub2\g.bin",
                r"r8\proj\src\util\lib.rs",
                r"r8\proj\src\main.rs",
                r"r8\other\notes.txt",
                r"r8\solo\only.txt",
                r"r8\proj\docs\readme.md",
                r"r8\proj\src\util\deep\x.txt",
            ],
        );
        check(r"ancestor-sibling:words whole:r8", &[]);
        check(
            r"ancestor-child:.gitignore \r8\",
            &[
                r"r8\proj\src\util\deep",
                r"r8\proj\docs",
                r"r8\proj\src",
                r"r8\proj\src\util",
                r"r8\proj\.gitignore",
                r"r8\proj\src\util\lib.rs",
                r"r8\proj\src\main.rs",
                r"r8\proj\docs\readme.md",
                r"r8\proj\src\util\deep\x.txt",
            ],
        );
        check(
            r"!whole:ancestor-child-file:.gitignore \r8\",
            &[
                r"r8\empty",
                r"r8\mixed",
                r"r8\other",
                r"r8\proj",
                r"r8\solo",
                r"r8\other\src",
                r"r8\mixed\sub1",
                r"r8\mixed\sub2",
                r"r8\other\src\app.rs",
                r"r8\mixed\f1.bin",
                r"r8\mixed\f2.bin",
                r"r8\mixed\sub2\g.bin",
                r"r8\other\notes.txt",
                r"r8\solo\only.txt",
            ],
        );
        check(
            r"ancestor-child-folder:src \r8\",
            &[
                r"r8\proj\src\util\deep",
                r"r8\proj\docs",
                r"r8\other\src",
                r"r8\proj\src",
                r"r8\proj\src\util",
                r"r8\proj\.gitignore",
                r"r8\other\src\app.rs",
                r"r8\proj\src\util\lib.rs",
                r"r8\proj\src\main.rs",
                r"r8\other\notes.txt",
                r"r8\proj\docs\readme.md",
                r"r8\proj\src\util\deep\x.txt",
            ],
        );
        check(r"ancestor-child-file:src \r8\", &[]);
        check(r"ancestor-child:only \r8\", &[r"r8\solo\only.txt"]);
        check(r"ancestor-child:r8 \r8\", &all);
        check(r"ancestor-child:words \r8\", &all);
        check(r"ancestor-child:words whole:r8", &[r"r8"]);
        check(r"ancestor-child: \r8\", &all);
        check(
            r"ancestorchild:lib \r8\",
            &[
                r"r8\proj\src\util\deep",
                r"r8\proj\src\util\lib.rs",
                r"r8\proj\src\util\deep\x.txt",
            ],
        );
        check(
            r"ancestor-child:x.txt \r8\",
            &[r"r8\proj\src\util\deep\x.txt"],
        );
        check(r"parent-child:only \r8\", &[r"r8\solo\only.txt"]);
        check(
            r"parent-child:.gitignore \r8\",
            &[r"r8\proj\docs", r"r8\proj\src", r"r8\proj\.gitignore"],
        );
        check(r"parent-child-file:src \r8\", &[]);
        check(
            r"parent-child-folder:src \r8\",
            &[
                r"r8\proj\docs",
                r"r8\other\src",
                r"r8\proj\src",
                r"r8\proj\.gitignore",
                r"r8\other\notes.txt",
            ],
        );
        check(r"parent-child:words whole:r8", &[r"r8"]);
        check(r"parent-child: \r8\", &all);
        check(
            r"sibling-count:0 \r8\",
            &[
                r"r8\other\src\app.rs",
                r"r8\mixed\sub2\g.bin",
                r"r8\solo\only.txt",
                r"r8\proj\docs\readme.md",
                r"r8\proj\src\util\deep\x.txt",
            ],
        );
        check(
            r"sibling-count:1 \r8\",
            &[
                r"r8\proj\src\util\deep",
                r"r8\other\src",
                r"r8\proj\src\util",
                r"r8\proj\src\util\lib.rs",
                r"r8\proj\src\main.rs",
                r"r8\other\notes.txt",
            ],
        );
        check(
            r"sibling-count:2 \r8\",
            &[r"r8\proj\docs", r"r8\proj\src", r"r8\proj\.gitignore"],
        );
        check(
            r"sibling-count:>2 \r8\",
            &[
                r"r8\empty",
                r"r8\mixed",
                r"r8\other",
                r"r8\proj",
                r"r8\solo",
                r"r8\mixed\sub1",
                r"r8\mixed\sub2",
                r"r8\mixed\f1.bin",
                r"r8\mixed\f2.bin",
            ],
        );
        check(
            r"sibling-count: \r8\",
            &[
                r"r8\proj\src\util\deep",
                r"r8\proj\docs",
                r"r8\empty",
                r"r8\mixed",
                r"r8\other",
                r"r8\proj",
                r"r8\solo",
                r"r8\other\src",
                r"r8\proj\src",
                r"r8\mixed\sub1",
                r"r8\mixed\sub2",
                r"r8\proj\src\util",
                r"r8\proj\.gitignore",
                r"r8\mixed\f1.bin",
                r"r8\mixed\f2.bin",
                r"r8\proj\src\util\lib.rs",
                r"r8\proj\src\main.rs",
                r"r8\other\notes.txt",
            ],
        );
        check(
            r"sibling-file-count:1 \r8\",
            &[
                r"r8\proj\src\util\deep",
                r"r8\proj\docs",
                r"r8\other\src",
                r"r8\proj\src",
                r"r8\proj\src\util",
                r"r8\mixed\f1.bin",
                r"r8\mixed\f2.bin",
            ],
        );
        check(
            r"sibling-folder-count:0 \r8\",
            &[
                r"r8\proj\src\util\deep",
                r"r8\other\src",
                r"r8\proj\src\util",
                r"r8\other\src\app.rs",
                r"r8\mixed\sub2\g.bin",
                r"r8\solo\only.txt",
                r"r8\proj\docs\readme.md",
                r"r8\proj\src\util\deep\x.txt",
            ],
        );
        check(
            r"sibling-folder-count:2 \r8\",
            &[
                r"r8\proj\.gitignore",
                r"r8\mixed\f1.bin",
                r"r8\mixed\f2.bin",
            ],
        );
        check(
            r"sibling-count:!1 \r8\",
            &[
                r"r8\proj\docs",
                r"r8\empty",
                r"r8\mixed",
                r"r8\other",
                r"r8\proj",
                r"r8\solo",
                r"r8\proj\src",
                r"r8\mixed\sub1",
                r"r8\mixed\sub2",
                r"r8\proj\.gitignore",
                r"r8\other\src\app.rs",
                r"r8\mixed\f1.bin",
                r"r8\mixed\f2.bin",
                r"r8\mixed\sub2\g.bin",
                r"r8\solo\only.txt",
                r"r8\proj\docs\readme.md",
                r"r8\proj\src\util\deep\x.txt",
            ],
        );
        check(
            r"!sibling-count:1 \r8\",
            &[
                r"r8\proj\docs",
                r"r8\empty",
                r"r8\mixed",
                r"r8\other",
                r"r8\proj",
                r"r8\solo",
                r"r8\proj\src",
                r"r8\mixed\sub1",
                r"r8\mixed\sub2",
                r"r8\proj\.gitignore",
                r"r8\other\src\app.rs",
                r"r8\mixed\f1.bin",
                r"r8\mixed\f2.bin",
                r"r8\mixed\sub2\g.bin",
                r"r8\solo\only.txt",
                r"r8\proj\docs\readme.md",
                r"r8\proj\src\util\deep\x.txt",
            ],
        );
        check(
            r"siblingcount:4 \r8\",
            &[
                r"r8\empty",
                r"r8\mixed",
                r"r8\other",
                r"r8\proj",
                r"r8\solo",
            ],
        );
        check(
            r"file: sibling-count:0 \r8\",
            &[
                r"r8\other\src\app.rs",
                r"r8\mixed\sub2\g.bin",
                r"r8\solo\only.txt",
                r"r8\proj\docs\readme.md",
                r"r8\proj\src\util\deep\x.txt",
            ],
        );
        check(r"sibling-count:4 whole:r8", &[]);
        check(r"descendant-count:0 \r8\", &[r"r8\empty", r"r8\mixed\sub1"]);
        check(
            r"descendant-count:1 \r8\",
            &[
                r"r8\proj\src\util\deep",
                r"r8\proj\docs",
                r"r8\solo",
                r"r8\other\src",
                r"r8\mixed\sub2",
            ],
        );
        check(
            r"descendant-count:3 \r8\",
            &[r"r8\other", r"r8\proj\src\util"],
        );
        check(
            r"descendant-count:>4 \r8\",
            &[r"r8\mixed", r"r8\proj", r"r8\proj\src"],
        );
        check(r"descendant-count:9 \r8\", &[r"r8\proj"]);
        check(
            r"descendant-file-count:2 \r8\",
            &[r"r8\other", r"r8\proj\src\util"],
        );
        check(
            r"descendant-folder-count:0 \r8\",
            &[
                r"r8\proj\src\util\deep",
                r"r8\proj\docs",
                r"r8\empty",
                r"r8\solo",
                r"r8\other\src",
                r"r8\mixed\sub1",
                r"r8\mixed\sub2",
            ],
        );
        check(r"descendant-folder-count:4 \r8\", &[r"r8\proj"]);
        check(
            r"descendant-count:!1 \r8\",
            &[
                r"r8\empty",
                r"r8\mixed",
                r"r8\other",
                r"r8\proj",
                r"r8\proj\src",
                r"r8\mixed\sub1",
                r"r8\proj\src\util",
            ],
        );
        check(
            r"!descendant-count:1 \r8\",
            &[
                r"r8\empty",
                r"r8\mixed",
                r"r8\other",
                r"r8\proj",
                r"r8\proj\src",
                r"r8\mixed\sub1",
                r"r8\proj\src\util",
                r"r8\proj\.gitignore",
                r"r8\other\src\app.rs",
                r"r8\mixed\f1.bin",
                r"r8\mixed\f2.bin",
                r"r8\mixed\sub2\g.bin",
                r"r8\proj\src\util\lib.rs",
                r"r8\proj\src\main.rs",
                r"r8\other\notes.txt",
                r"r8\solo\only.txt",
                r"r8\proj\docs\readme.md",
                r"r8\proj\src\util\deep\x.txt",
            ],
        );
        check(r"descendantcount:5 \r8\", &[r"r8\mixed", r"r8\proj\src"]);
        check(
            r"descendant-count: \r8\",
            &[
                r"r8\proj\src\util\deep",
                r"r8\proj\docs",
                r"r8\mixed",
                r"r8\other",
                r"r8\proj",
                r"r8\solo",
                r"r8\other\src",
                r"r8\proj\src",
                r"r8\mixed\sub2",
                r"r8\proj\src\util",
            ],
        );
        check(r"descendant-count:23 whole:r8", &[r"r8"]);
        check(r"descendant-file-count:12 whole:r8", &[]);
        check(
            r"child-file-size:>1kb \r8\",
            &[
                r"r8\proj\docs",
                r"r8\mixed",
                r"r8\other",
                r"r8\proj\src",
                r"r8\mixed\sub2",
                r"r8\proj\src\util",
            ],
        );
        check(r"child-file-size:1025 \r8\", &[r"r8\mixed"]);
        check(
            r"child-file-size:<10 \r8\",
            &[r"r8\proj\src\util\deep", r"r8\solo"],
        );
        check(r"childfilesize:0 \r8\", &[r"r8\proj\src\util\deep"]);
        check(
            r"child-file-size:!0 \r8\",
            &[
                r"r8\proj\docs",
                r"r8\empty",
                r"r8\mixed",
                r"r8\other",
                r"r8\proj",
                r"r8\solo",
                r"r8\other\src",
                r"r8\proj\src",
                r"r8\mixed\sub1",
                r"r8\mixed\sub2",
                r"r8\proj\src\util",
            ],
        );
        check(
            r"!child-file-size:>1kb \r8\",
            &[
                r"r8\proj\src\util\deep",
                r"r8\empty",
                r"r8\proj",
                r"r8\solo",
                r"r8\other\src",
                r"r8\mixed\sub1",
                r"r8\proj\.gitignore",
                r"r8\other\src\app.rs",
                r"r8\mixed\f1.bin",
                r"r8\mixed\f2.bin",
                r"r8\mixed\sub2\g.bin",
                r"r8\proj\src\util\lib.rs",
                r"r8\proj\src\main.rs",
                r"r8\other\notes.txt",
                r"r8\solo\only.txt",
                r"r8\proj\docs\readme.md",
                r"r8\proj\src\util\deep\x.txt",
            ],
        );
        check(
            r"child-file-size:1kb;5000 \r8\",
            &[
                r"r8\proj\docs",
                r"r8\mixed",
                r"r8\proj\src",
                r"r8\proj\src\util",
            ],
        );
        check(
            r"total-child-size:>2kb \r8\",
            &[r"r8\mixed", r"r8\mixed\sub2", r"r8\proj\src\util"],
        );
        check(
            r"total-child-size:0 \r8\",
            &[r"r8\proj\src\util\deep", r"r8\empty", r"r8\mixed\sub1"],
        );
        check(
            r"child-date-modified:2025 \r8\",
            &[
                r"r8\other",
                r"r8\proj\src",
                r"r8\mixed\sub2",
                r"r8\proj\src\util",
            ],
        );
        check(r"childdm:2024 \r8\", &[r"r8\proj\docs", r"r8\proj"]);
        check(
            r"child-file-date-modified:2025-06-01 \r8\",
            &[r"r8\proj\src"],
        );
        check(
            r"child-folder-date-modified:2019 \r8\",
            &[r"r8\other", r"r8\proj"],
        );
        check(r"childfiledm:<2015 \r8\", &[r"r8\mixed"]);
        check(
            r"childdm:!2025 \r8\",
            &[
                r"r8\proj\src\util\deep",
                r"r8\proj\docs",
                r"r8\empty",
                r"r8\mixed",
                r"r8\proj",
                r"r8\solo",
                r"r8\other\src",
                r"r8\mixed\sub1",
            ],
        );
        check(
            r"!childdm:2025 \r8\",
            &[
                r"r8\proj\src\util\deep",
                r"r8\proj\docs",
                r"r8\empty",
                r"r8\mixed",
                r"r8\proj",
                r"r8\solo",
                r"r8\other\src",
                r"r8\mixed\sub1",
                r"r8\proj\.gitignore",
                r"r8\other\src\app.rs",
                r"r8\mixed\f1.bin",
                r"r8\mixed\f2.bin",
                r"r8\mixed\sub2\g.bin",
                r"r8\proj\src\util\lib.rs",
                r"r8\proj\src\main.rs",
                r"r8\other\notes.txt",
                r"r8\solo\only.txt",
                r"r8\proj\docs\readme.md",
                r"r8\proj\src\util\deep\x.txt",
            ],
        );
        check(r"childfolderdm:2023 \r8\", &[r"r8\proj"]);
        check(r"child-dm:2012 \r8\", &[r"r8\mixed"]);
        check(r"child-date-modified:2010..2012 \r8\", &[r"r8\mixed"]);
        check(
            r"child-file-dm:2025 \r8\",
            &[
                r"r8\other",
                r"r8\proj\src",
                r"r8\mixed\sub2",
                r"r8\proj\src\util",
            ],
        );
        check(r"child-name:main \r8\", &[r"r8\proj\src"]);
        check(r"child-file-name:main \r8\", &[r"r8\proj\src"]);
        check(r"child-folder-name:util \r8\", &[r"r8\proj\src"]);
        check(r"siblingname:main \r8\", &[r"r8\proj\src\util"]);
        check(r"sibling-name:main \r8\", &[r"r8\proj\src\util"]);
        check(r"sibling-file-name:notes \r8\", &[r"r8\other\src"]);
        check(
            r"sibling-folder-name:src \r8\",
            &[
                r"r8\proj\docs",
                r"r8\proj\.gitignore",
                r"r8\other\notes.txt",
            ],
        );
        for query in [
            r"child-size:>1kb \r8\",
            r"child-folder-size:>1kb \r8\",
            r"child-size:<10 \r8\",
            r"*.rs !sibling:$stem:.md \r8\",
        ] {
            assert!(
                matches!(
                    parse_query_at(query, false, &clock),
                    Err(QueryError::Unsupported { .. })
                ),
                "{query}"
            );
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// The functions of related entries of W8 against a plain reference
        /// over random trees whose names repeat and fold alike and whose
        /// entries carry sizes and dates: each answer is worked out entry by
        /// entry from the parent links (the entry's folder's entries, those
        /// of the folder above it, the folders above it and theirs, the
        /// entries below a folder, a folder's own entries), and the engine
        /// must give the same entries, alone, negated, ANDed with a name
        /// term, ORed with one, and with the value's own `!` where the value
        /// is a number or a date (with a path term for the sub-searches,
        /// whose `!` is text).
        #[test]
        fn related_entry_functions_agree_with_a_reference(
            nodes in prop::collection::vec(
                (any::<prop::sample::Index>(), 0usize..5, 0u8..3, 0usize..5, 0usize..4),
                1..60,
            ),
            func in 0usize..23,
            value in 0usize..4,
            shape in 0u8..5,
        ) {
            use crate::query::dates::tests::{FixedClock, ft};
            use crate::query::parse::parse_query_at;
            const NAMES: [&str; 5] = ["a", "A", "ab", "b.txt", "é"];
            const SIZES: [u64; 5] = [0, 1, 1024, 1025, 5000];
            const YEARS: [&str; 4] = ["2019", "2020", "2021", "2022"];
            let mut idx = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
            let mut dirs = vec![NTFS_ROOT_FRN];
            for (i, (parent, name, kind, size, year)) in nodes.iter().enumerate() {
                let f = frn(1000 + i as u64);
                let is_dir = *kind == 0;
                idx.insert_enum(&enum_rec(f, dirs[parent.index(dirs.len())], NAMES[*name], is_dir));
                idx.set_stat(f, SIZES[*size], ft(&format!("{}-06-15T12:00:00", YEARS[*year])));
                if is_dir {
                    dirs.push(f);
                }
            }
            idx.optimize_storage();
            let funcs = [
                "parent-child:", "parent-child-file:", "parent-child-folder:",
                "parent-sibling:", "parent-sibling-file:", "parent-sibling-folder:",
                "ancestor-child:", "ancestor-child-file:", "ancestor-child-folder:",
                "ancestor-sibling:", "ancestor-sibling-file:", "ancestor-sibling-folder:",
                "descendant-count:", "descendant-file-count:", "descendant-folder-count:",
                "sibling-count:", "sibling-file-count:", "sibling-folder-count:",
                "child-file-size:",
                "child-date-modified:", "child-file-date-modified:", "child-folder-date-modified:",
                "total-child-size:",
            ];
            let text = match func {
                0..=11 => ["a", "b", "ab", ""][value],
                12..=17 => ["0", "1", ">1", "<3"][value],
                18 | 22 => ["0", "1kb", ">1kb", "<10"][value],
                _ => ["2019", "2020", "2021..2022", "2019;2022"][value],
            };
            let term = format!("{}{text}", funcs[func]);
            let query = match shape {
                0 => term.clone(),
                1 => format!("!{term}"),
                2 => format!("{term} a"),
                3 => format!("{term}|b"),
                _ if func < 12 => format!("{term} a\\"),
                _ => format!("{}!{text}", funcs[func]),
            };

            // The reference.
            let live: Vec<EntryIdx> =
                idx.live_entries().filter(|&e| !idx.is_synthetic(e)).collect();
            let parent = |e: EntryIdx| idx.entry(e).parent();
            let is_dir = |e: EntryIdx| idx.entry(e).is_dir();
            // The kind each function counts or tests, by its place in a
            // group of three: any, files, folders.
            let kind_ok = |e: EntryIdx| {
                let place = match func {
                    18 | 22 => 1,
                    19..=21 => func - 19,
                    _ => func % 3,
                };
                match place {
                    1 => !is_dir(e),
                    2 => is_dir(e),
                    _ => true,
                }
            };
            let named = |e: EntryIdx| {
                let n = crate::fold::fold(idx.entry(e).name());
                text.is_empty() || memmem::find(&n, text.as_bytes()).is_some()
            };
            let hit = |e: EntryIdx| named(e) && kind_ok(e);
            let holds_hit = |folder: EntryIdx| {
                folder != NIL && live.iter().any(|&c| parent(c) == folder && hit(c))
            };
            // The folders above an entry, its folder first, the root last.
            let above = |e: EntryIdx| {
                let mut out = Vec::new();
                let mut cur = parent(e);
                while cur != NIL {
                    out.push(cur);
                    cur = parent(cur);
                }
                out
            };
            let count_ok = |n: u64| match value {
                0 => n == 0,
                1 => n == 1,
                2 => n > 1,
                _ => n < 3,
            };
            let size_ok = |s: u64| match value {
                0 => s == 0,
                1 => (1024..2048).contains(&s),
                2 => s > 1024,
                _ => s < 10,
            };
            let year_ok = |e: EntryIdx| {
                let year = YEARS
                    .iter()
                    .position(|y| idx.entry(e).mtime() == Some(ft(&format!("{y}-06-15T12:00:00"))));
                match value {
                    0 => year == Some(0),
                    1 => year == Some(1),
                    2 => matches!(year, Some(2 | 3)),
                    _ => matches!(year, Some(0 | 3)),
                }
            };
            let children = |e: EntryIdx| live.iter().copied().filter(move |&c| parent(c) == e);
            // Whether the function's test holds for `e`.
            let related = |e: EntryIdx| -> bool {
                match func {
                    0..=2 => holds_hit(parent(e)),
                    3..=5 => parent(e) != NIL && holds_hit(parent(parent(e))),
                    6..=8 => above(e).into_iter().any(holds_hit),
                    9..=11 => above(e).into_iter().skip(1).any(holds_hit),
                    12..=14 => {
                        if !is_dir(e) {
                            return false;
                        }
                        let n = live
                            .iter()
                            .filter(|&&c| kind_ok(c) && above(c).contains(&e))
                            .count();
                        count_ok(n as u64)
                    }
                    15..=17 => {
                        let n = live
                            .iter()
                            .filter(|&&c| c != e && parent(c) == parent(e) && kind_ok(c))
                            .count();
                        count_ok(n as u64)
                    }
                    18 => {
                        if !is_dir(e) {
                            return false;
                        }
                        children(e).any(|c| {
                            kind_ok(c) && idx.entry(c).size().is_some_and(size_ok)
                        })
                    }
                    19..=21 => {
                        if !is_dir(e) {
                            return false;
                        }
                        children(e).any(|c| kind_ok(c) && year_ok(c))
                    }
                    _ => {
                        if !is_dir(e) {
                            return false;
                        }
                        let total: u64 = children(e)
                            .filter(|&c| !is_dir(c))
                            .map(|c| idx.entry(c).size().unwrap_or(0))
                            .sum();
                        size_ok(total)
                    }
                }
            };
            let name_of = |e: EntryIdx| crate::fold::fold(idx.entry(e).name());
            let path_of = |e: EntryIdx| {
                let mut p = Vec::new();
                idx.path_of(e, &mut p);
                p
            };
            let mut want: Vec<String> = live
                .iter()
                .copied()
                .filter(|&e| match shape {
                    0 => related(e),
                    1 => !related(e),
                    2 => related(e) && memmem::find(&name_of(e), b"a").is_some(),
                    3 => related(e) || memmem::find(&name_of(e), b"b").is_some(),
                    _ if func < 12 => {
                        related(e)
                            && memmem::find(&crate::fold::fold(&path_of(e)), b"a\\").is_some()
                    }
                    // The value's own `!`: among the folders for the
                    // descendant counts and the child tests, among every
                    // entry for the sibling counts.
                    _ if (15..=17).contains(&func) => !related(e),
                    _ => is_dir(e) && !related(e),
                })
                .map(|e| String::from_utf8(path_of(e)).unwrap())
                .collect();
            want.sort();
            let clock = FixedClock::utc(ft("2026-11-11T12:00:00"));
            let parsed = parse_query_at(&query, false, &clock).unwrap();
            let out = run_query(&idx, r"C:\", &parsed, None, SortSpec::default(), 0, None);
            let mut got: Vec<String> = out
                .hits
                .iter()
                .map(|h| crate::wtf8::to_string_lossy(&h.path))
                .collect();
            got.sort();
            prop_assert_eq!(got, want, "{}", query);
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// The sub-searches against a plain reference over random trees
        /// whose names repeat, fold alike and nest: each term's answer is
        /// worked out entry by entry from the parent links (a folder's
        /// children, its subtree, the entry's folder's other entries, the
        /// folders above it), and the engine must give the same entries,
        /// alone, negated, and ANDed or ORed with a name term.
        #[test]
        fn related_searches_agree_with_a_reference(
            nodes in prop::collection::vec(
                (any::<prop::sample::Index>(), 0usize..6, 0u8..3),
                1..60,
            ),
            value in 0usize..6,
            func in 0usize..10,
            whole in any::<bool>(),
            shape in 0u8..5,
        ) {
            const NAMES: [&str; 6] = ["a", "A", "ab", "b.txt", "é", "e\u{301}x"];
            let mut idx = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
            let mut dirs = vec![NTFS_ROOT_FRN];
            for (i, (parent, name, kind)) in nodes.iter().enumerate() {
                let f = frn(1000 + i as u64);
                let is_dir = *kind == 0;
                idx.insert_enum(&enum_rec(f, dirs[parent.index(dirs.len())], NAMES[*name], is_dir));
                if is_dir {
                    dirs.push(f);
                }
            }
            idx.optimize_storage();
            let funcs = [
                "child:", "child-file:", "child-folder:", "descendant:", "descendant-file:",
                "descendant-folder:", "sibling:", "sibling-file:", "sibling-folder:",
                "ancestor-name:",
            ];
            let text = ["a", "b", "ab", "e", "x", ""][value];
            let term = format!("{}{}{text}", if whole { "whole:" } else { "" }, funcs[func]);
            let query = match shape {
                0 => term.clone(),
                1 => format!("!{term}"),
                2 => format!("{term} a"),
                3 => format!("{term}|b"),
                // A path term, whose walk below the names holding `a` may
                // grow past its share and leave the scan to the sub-search.
                _ => format!("{term} a\\"),
            };
            // The reference: what the name term matches, and how entries
            // relate, from the links.
            let live: Vec<EntryIdx> = idx.live_entries().filter(|&e| !idx.is_synthetic(e)).collect();
            let name_of = |e: EntryIdx| crate::fold::fold(idx.entry(e).name());
            let named = |e: EntryIdx| {
                let n = name_of(e);
                let t = text.as_bytes();
                // An empty value is any name, `whole:` or not.
                t.is_empty() || if whole { n == t } else { memmem::find(&n, t).is_some() }
            };
            let kind_ok = |e: EntryIdx| match func % 3 {
                _ if func == 9 => idx.entry(e).is_dir(),
                1 => !idx.entry(e).is_dir(),
                2 => idx.entry(e).is_dir(),
                _ => true,
            };
            let hit = |e: EntryIdx| named(e) && kind_ok(e);
            let above = |e: EntryIdx| {
                let mut out = Vec::new();
                let mut cur = idx.entry(e).parent();
                while cur != NIL && cur != idx.root() {
                    out.push(cur);
                    cur = idx.entry(cur).parent();
                }
                out
            };
            let related = |e: EntryIdx| -> bool {
                match func {
                    0..=2 => live.iter().any(|&c| idx.entry(c).parent() == e && hit(c)),
                    3..=5 => live.iter().any(|&c| above(c).contains(&e) && hit(c)),
                    6..=8 => live
                        .iter()
                        .any(|&s| s != e && idx.entry(s).parent() == idx.entry(e).parent() && hit(s)),
                    // The root's name is the prefix, `C:`, which none of the
                    // values but the empty one is in.
                    _ => text.is_empty() || above(e).into_iter().any(hit),
                }
            };
            let name_a = |e: EntryIdx| memmem::find(&name_of(e), b"a").is_some();
            let name_b = |e: EntryIdx| memmem::find(&name_of(e), b"b").is_some();
            let path_of = |e: EntryIdx| {
                let mut p = Vec::new();
                idx.path_of(e, &mut p);
                p
            };
            let mut want: Vec<String> = live
                .iter()
                .copied()
                .filter(|&e| match shape {
                    0 => related(e),
                    1 => !related(e),
                    2 => related(e) && name_a(e),
                    3 => related(e) || name_b(e),
                    _ => {
                        related(e)
                            && memmem::find(&crate::fold::fold(&path_of(e)), b"a\\").is_some()
                    }
                })
                .map(|e| String::from_utf8(path_of(e)).unwrap())
                .collect();
            want.sort();
            let mut got = run(&idx, &query);
            got.sort();
            prop_assert_eq!(got, want, "{}", query);
        }
    }

    fn accent_index() -> VolumeIndex {
        let mut idx = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
        let root = NTFS_ROOT_FRN;
        idx.insert_enum(&enum_rec(frn(100), root, "café", true));
        idx.insert_enum(&enum_rec(frn(101), root, "cafe", true));
        let names = [
            "cafe.txt",
            "café.txt",
            "CAFÉ2.txt",
            "cafe\u{301}nfd.txt",
            "Straße.txt",
            "strasse.txt",
            "kırmızı.txt",
            "İstanbul.TXT",
            "ﬁle.txt",
            "x²y.txt",
            "a\u{3000}b.txt",
            "ext.café",
            "ext.cafe",
        ];
        for (i, name) in names.iter().enumerate() {
            idx.insert_enum(&enum_rec(frn(200 + i as u64), frn(100), name, false));
        }
        idx.insert_enum(&enum_rec(frn(300), frn(101), "café.txt", false));
        idx
    }

    /// Accents are ignored unless `diacritics:` asks for them, on names,
    /// full paths, wildcards and extensions, as Everything 1.5 does
    /// (L1:169-226); `case:` compares as stored.
    #[test]
    fn accents_are_ignored_unless_diacritics() {
        let idx = accent_index();
        let names = |q: &str| {
            let mut got: Vec<String> = run(&idx, q)
                .into_iter()
                .map(|p| p.rsplit('\\').next().unwrap_or_default().to_string())
                .collect();
            got.sort();
            got.dedup();
            got
        };
        let set = |v: &[&str]| {
            let mut v: Vec<String> = v.iter().map(|s| s.to_string()).collect();
            v.sort();
            v
        };
        assert_eq!(
            names("cafe file:"),
            set(&[
                "cafe.txt",
                "café.txt",
                "CAFÉ2.txt",
                "cafe\u{301}nfd.txt",
                "ext.cafe",
                "ext.café"
            ])
        );
        assert_eq!(names("CAFÉ file:"), names("cafe file:"));
        assert_eq!(
            names("diacritics:café"),
            set(&[
                "café",
                "café.txt",
                "CAFÉ2.txt",
                "cafe\u{301}nfd.txt",
                "ext.café"
            ])
        );
        assert_eq!(
            names("diacritics:cafe file:"),
            set(&["cafe.txt", "ext.cafe"])
        );
        assert_eq!(
            names("diacritics:CAFE"),
            set(&["cafe", "cafe.txt", "ext.cafe"])
        );
        assert_eq!(names("case:café"), set(&["café", "café.txt", "ext.café"]));
        assert_eq!(names("strasse"), set(&["Straße.txt", "strasse.txt"]));
        assert_eq!(names("diacritics:straße"), set(&["Straße.txt"]));
        assert_eq!(names("kirmizi"), set(&["kırmızı.txt"]));
        assert_eq!(names("istanbul"), set(&["İstanbul.TXT"]));
        assert_eq!(names("file"), set(&["ﬁle.txt"]));
        assert_eq!(names("x2y"), set(&["x²y.txt"]));
        assert_eq!(names(r#""a b""#), set(&["a\u{3000}b.txt"]));
        // Wildcards count characters after folding.
        assert_eq!(names("caf?.txt"), set(&["cafe.txt", "café.txt"]));
        assert_eq!(
            names("diacritics:café*.txt"),
            set(&["café.txt", "CAFÉ2.txt", "cafe\u{301}nfd.txt"])
        );
        // A full path, and an extension.
        assert_eq!(run(&idx, r"cafe\cafe.txt").len(), 3);
        assert_eq!(
            run(&idx, r"diacritics:café\cafe.txt"),
            vec![r"café\cafe.txt"]
        );
        assert_eq!(
            run(&idx, r"diacritics:path:cafe\café"),
            vec![r"cafe\café.txt"]
        );
        assert_eq!(names("ext:cafe"), set(&["ext.cafe", "ext.café"]));
        assert_eq!(names("diacritics:ext:café"), set(&["ext.café"]));
        assert_eq!(names("ext:CAFÉ"), names("ext:cafe"));
    }

    /// `regex:` matches the name as stored (L1:425-433): no accent folding,
    /// case-insensitive unless `case:`, the full path with `path:` or `\\`.
    #[test]
    fn regex_terms_match_names_and_paths_as_stored() {
        let idx = accent_index();
        let sorted = |q: &str| {
            let mut v = run(&idx, q);
            v.sort();
            v
        };
        assert_eq!(
            sorted("regex:^ca.e"),
            vec!["cafe", r"café\cafe.txt", "café\\cafe\u{301}nfd.txt"]
        );
        assert_eq!(
            sorted("regex:é"),
            vec![
                r"cafe\café.txt",
                "café",
                r"café\CAFÉ2.txt",
                r"café\café.txt",
                r"café\ext.café"
            ]
        );
        assert_eq!(run(&idx, "case:regex:^CAF"), vec![r"café\CAFÉ2.txt"]);
        assert_eq!(run(&idx, r"regex:^cafe\\caf.\.txt$"), Vec::<String>::new());
        assert_eq!(run(&idx, r"regex:e\\caf.\.txt$"), vec![r"cafe\café.txt"]);
        assert_eq!(run(&idx, r"path:regex:^C:\\cafe\\"), vec![r"cafe\café.txt"]);
        assert_eq!(sorted(r"regex:\.txt$ !regex:^c"), sorted("*.txt !c*"));
        assert_eq!(run(&idx, "regex:^x.y|^ﬁ").len(), 2);
    }

    /// Name parts with case variants in and beyond ASCII, accents
    /// precomposed and not, a ligature and Hangul, for the `regex:`
    /// prefilter's test.
    const REGEX_PARTS: &[&str] = &[
        "a", "K", "k", "\u{212A}", "s", "S", "\u{17F}", "ß", "ẞ", "SS", "é", "e\u{301}", "É", "가",
        "\u{11A8}", "x_1", ".", "b", "ﬁ", "fi",
    ];

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        /// `regex:` answers with its literal prefilter (the pieces tested
        /// before the pattern, the names they find, the folder names a
        /// full-path pattern walks below) exactly as a plain test of the
        /// pattern on every entry's name or full path does: patterns cut
        /// from the tree's own names and paths, in other cases, with a
        /// letter widened to a class, a letter made optional, an
        /// alternation with text no name holds, or a wildcard part, matching
        /// case or not.
        #[test]
        fn regex_prefilters_change_no_answer(
            nodes in prop::collection::vec(
                (
                    any::<prop::sample::Index>(),
                    prop::collection::vec(0..REGEX_PARTS.len(), 1..5),
                    any::<bool>(),
                ),
                1..40,
            ),
            from in any::<prop::sample::Index>(),
            cut in (any::<prop::sample::Index>(), any::<prop::sample::Index>()),
            shape in 0u64..1 << 16,
            case in prop::bool::weighted(0.2),
        ) {
            let mut idx = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
            let mut dirs = vec![NTFS_ROOT_FRN];
            for (i, (parent, parts, is_dir)) in nodes.iter().enumerate() {
                let f = frn(1000 + i as u64);
                let name: String = parts.iter().map(|&p| REGEX_PARTS[p]).collect();
                idx.insert_enum(&enum_rec(f, dirs[parent.index(dirs.len())], &name, *is_dir));
                if *is_dir {
                    dirs.push(f);
                }
            }
            idx.optimize_storage();
            // Every entry with its name and full path as stored.
            let entries: Vec<(String, String, String)> = idx
                .live_entries()
                .filter(|&e| !idx.is_synthetic(e))
                .filter_map(|e| {
                    let mut p = Vec::new();
                    (idx.path_of(e, &mut p) == crate::index::volume::PathStatus::Ok).then(|| {
                        let path = String::from_utf8(p).unwrap();
                        let name = String::from_utf8(idx.entry(e).name().to_vec()).unwrap();
                        (name, format!(r"C:\{path}"), path)
                    })
                })
                .collect();
            prop_assume!(!entries.is_empty());
            // A pattern cut from a full path crosses folders when it holds
            // a separator; one cut from a name tests names.
            let (name, full, _) = &entries[from.index(entries.len())];
            let on_path = shape % 3 == 0;
            let source: Vec<char> = if on_path { full.chars().collect() } else { name.chars().collect() };
            let (a, b) = (cut.0.index(source.len() + 1), cut.1.index(source.len() + 1));
            let piece = &source[a.min(b)..a.max(b)];
            prop_assume!(!piece.is_empty());
            let mut pattern = String::new();
            for (k, &c) in piece.iter().enumerate() {
                let c = if (shape >> 2) & 1 == 1 {
                    c.to_uppercase().next().unwrap_or(c)
                } else {
                    c
                };
                // Every character but a letter or a digit escaped, so that
                // the lexer reads the pattern whole (`^:` is a prefix).
                let lit = if c.is_alphanumeric() {
                    c.to_string()
                } else {
                    format!("\\x{{{:X}}}", c as u32)
                };
                match (shape >> (3 + (k % 4) * 2)) & 3 {
                    // A class of it and another letter.
                    0 if k % 3 == 1 => pattern.push_str(&format!("[{lit}z]")),
                    // Optional.
                    1 if k % 3 == 2 => pattern.push_str(&format!("{lit}?")),
                    _ => pattern.push_str(&lit),
                }
            }
            match (shape >> 12) % 4 {
                0 => pattern = format!("(?:{pattern}|qqq)"),
                1 => pattern.push_str(".*b"),
                2 => pattern.insert(0, '^'),
                _ => {}
            }
            let query = format!(
                "{}{}regex:{pattern}",
                if case { "case:" } else { "" },
                if on_path { "path:" } else { "" }
            );
            let regex = regex::RegexBuilder::new(&pattern)
                .case_insensitive(!case)
                .build()
                .unwrap();
            let mut want: Vec<String> = entries
                .iter()
                .filter(|(name, full, _)| regex.is_match(if on_path { full } else { name }))
                .map(|(_, _, path)| path.clone())
                .collect();
            want.sort();
            let mut got = run(&idx, &query);
            got.sort();
            prop_assert_eq!(got, want, "{}", query);
        }
    }

    /// A scope names a folder ignoring accents, but of two that fold alike
    /// the one spelled as asked wins.
    #[test]
    fn a_scope_prefers_the_folder_spelled_as_asked() {
        let idx = accent_index();
        let (cafe, accented) = (idx.head_of(frn(101)), idx.head_of(frn(100)));
        assert_eq!(resolve_scope(&idx, "café".as_bytes(), false), accented);
        assert_eq!(resolve_scope(&idx, "CAFÉ".as_bytes(), false), accented);
        assert_eq!(
            resolve_scope(&idx, "cafe\u{301}".as_bytes(), false),
            accented
        );
        assert_eq!(resolve_scope(&idx, b"cafe", false), cafe);
        assert_eq!(resolve_scope(&idx, b"CAFE\\", false), cafe);
        assert!(resolve_scope(&idx, "cafè".as_bytes(), false).is_some());
        assert_eq!(resolve_scope(&idx, "café".as_bytes(), true), accented);
    }

    /// `diacritics:` reaches `parent:` and `ancestor:` (and so `goz -a
    /// -parent`): a sibling folder spelled with other accents is not named,
    /// as in Everything with Match Diacritics on.
    #[test]
    fn diacritics_reach_the_folder_functions() {
        let idx = accent_index();
        let sorted = |q: &str| {
            let mut v = run(&idx, q);
            v.sort();
            v
        };
        let accented = sorted(r"parent:C:\café");
        assert!(accented.contains(&r"cafe\café.txt".to_string()));
        assert!(accented.contains(&r"café\cafe.txt".to_string()));
        for q in [
            r"diacritics:parent:C:\café",
            r"?diacritics: parent:C:\CAFÉ",
            "diacritics:ancestor:C:\\cafe\u{301}",
            // `goz -a -parent C:\café`, as the CLI sends it.
            "?diacritics: parent:\"C:\\café\"",
        ] {
            let got = sorted(q);
            assert!(!got.is_empty(), "{q}");
            assert!(got.iter().all(|p| p.starts_with("café\\")), "{q}: {got:?}");
        }
        assert_eq!(sorted(r"diacritics:parent:C:\cafe"), vec![r"cafe\café.txt"]);
        assert!(sorted(r"diacritics:parent:C:\cafè").is_empty());
    }

    /// Characters whose compatibility form is path or name syntax keep
    /// their own (DESIGN.md §10): `＼` is no separator for `parent:` or a
    /// path term, `․` and `．` no extension dot, and compatibility jamo are
    /// not joined into a syllable.
    #[test]
    fn folding_makes_no_separators_dots_or_syllables() {
        let mut idx = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
        let root = NTFS_ROOT_FRN;
        idx.insert_enum(&enum_rec(frn(100), root, "a\u{FF3C}b", true));
        idx.insert_enum(&enum_rec(frn(200), frn(100), "z.txt", false));
        for (i, name) in ["report\u{2024}pdf", "a\u{FF0E}txt", "\u{3131}\u{314F}.txt"]
            .iter()
            .enumerate()
        {
            idx.insert_enum(&enum_rec(frn(300 + i as u64), root, name, false));
        }
        let sorted = |q: &str| {
            let mut v = run(&idx, q);
            v.sort();
            v
        };
        let z = vec!["a\u{FF3C}b\\z.txt".to_string()];
        assert_eq!(sorted("parent:C:\\a\u{FF3C}b"), z);
        assert_eq!(sorted("ancestor:C:\\a\u{FF3C}b"), z);
        assert_eq!(sorted("infolder:C:\\A\u{FF3C}B\\"), z);
        // No folder `a` holds a `b`: no path term finds the one folder.
        assert!(sorted(r"a\b").is_empty());
        assert!(sorted(r"\a\b\").is_empty());
        // No extension, for every term that reads one: the stem is the
        // whole name.
        for q in ["ext:pdf", "*.pdf", "extension:pdf", "diacritics:*.pdf"] {
            assert!(sorted(q).is_empty(), "{q}");
        }
        assert_eq!(sorted("stem:pdf"), vec!["report\u{2024}pdf"]);
        assert_eq!(
            sorted("*.txt"),
            vec!["a\u{FF3C}b\\z.txt", "\u{3131}\u{314F}.txt"]
        );
        assert_eq!(sorted("report\u{2024}"), vec!["report\u{2024}pdf"]);
        assert_eq!(sorted("a\u{FF0E}t"), vec!["a\u{FF0E}txt"]);
        // The jamo as they are.
        assert_eq!(sorted("\u{3131}"), vec!["\u{3131}\u{314F}.txt"]);
        assert_eq!(sorted("??.txt"), vec!["\u{3131}\u{314F}.txt"]);
        assert!(sorted("\u{AC00}").is_empty());
    }
}
