//! The functions of related entries: the sub-searches of other entries'
//! names (`child:`, `descendant:`, `sibling:`, `ancestor-name:`,
//! `parent-child:`, `parent-sibling:`, `ancestor-child:`,
//! `ancestor-sibling:`, and their `-file:` and `-folder:` forms), the child
//! size and date tests (`child-file-size:`, `child-date-modified:` and its
//! forms), and the descendant and sibling counts (`descendant-count:`,
//! `sibling-count:` and their forms), as a live Everything 1.5 answers them
//! (W7 and W8, replayed in the engine's tests).
//!
//! Each is worked out once per volume and query, before the search. A
//! sub-search's term on the name runs once over the distinct names (the
//! folded names holding its needles, or a pass over every name across
//! cores), a size or date test once over the entries, and the entries that
//! match are mapped through the parent and child links the index keeps onto
//! a set of folders, one bit an entry. Testing an entry is then a lookup or
//! two:
//!
//! - `child:x`: a folder one of whose entries is named `x`
//!   (`child-file-size:`, `child-date-modified:`: of that size or date).
//! - `descendant:x`: a folder with such an entry at any depth below it: each
//!   match marks the folders above it, up to one marked already, so the
//!   marking costs the volume's folders at most.
//! - `sibling:x`: an entry whose folder holds another entry named `x`, the
//!   entry itself not counted (Everything leaves it out): a folder's matches
//!   are counted to two.
//! - `ancestor-name:x`: an entry below a folder named `x`: each such folder
//!   with none above it marks the folders of its subtree. The root's name
//!   is the mount prefix, as Everything names a root by its path (a folder
//!   index's `C:\data`, a drive's `C:`), so when the term matches it every
//!   entry of the volume matches.
//! - `parent-child:x`: an entry whose folder holds an entry named `x`, the
//!   entry itself counted: `child:`'s folders, looked up at the entry's
//!   folder.
//! - `parent-sibling:x`: the same, looked up at the folder above the
//!   entry's folder, whose own name counts too. Everything's help says the
//!   parent is left out, but a live 1.5.0.1424b answers
//!   `parent-sibling:src` with the entries of each folder named `src` (W8).
//! - `ancestor-child:x`: an entry below a folder that holds an entry named
//!   `x`: those folders' subtrees are marked, as `ancestor-name:`'s are.
//! - `ancestor-sibling:x`: the same, looked up at the folder above the
//!   entry's folder, so the folders on the entry's path count too (W8).
//!
//! The counts are a pass too: `descendant-count:` a depth-first walk from
//! the root that sums each folder's subtree as it closes it, `sibling-count:`
//! one count of each folder's entries; each marks the folders whose count is
//! in range (for `sibling-count:`, twice: for an entry the count includes,
//! and for one it does not).
//!
//! The work is checked against the query's budget as it goes; a spent
//! budget leaves the set unfinished and the query is refused, as any other
//! is. The sets of one query take a bit or two per entry each, and a query
//! holds at most 8 of these terms (`MAX_RELATED_TERMS`).

use super::budget::{Budget, Watch};
use super::cexpr::CNode;
use super::engine::{NameSet, kind_matches, names_passing, size_matches, walk_starts_at};
use super::expr::{CountOf, Expr, Leaf, Relation, Target};
use super::glob::{CodePointBuf, decode_into};
use super::parse::{Kind, SizeRange};
use super::text::TextMatcher;
use crate::fold::{fold, fold_exact};
use crate::index::VolumeIndex;
use crate::index::volume::MAX_PATH_HOPS;
use crate::types::{EntryIdx, NIL};
use std::borrow::Cow;
use std::sync::Arc;

/// A term on related entries worked out for one volume, as the compiled
/// expression keeps it.
#[derive(Clone)]
pub(super) enum Worked {
    Related(Arc<RelatedSet>),
    Counted(Arc<CountSet>),
}

impl Worked {
    /// Calls `visit` with each entry the term may hold for, once, until it
    /// answers `false` (see [`RelatedSet::for_each_candidate`] and
    /// [`CountSet::for_each_candidate`]). A scan whose only narrow term is
    /// this one reads these instead of the volume.
    pub(super) fn for_each_candidate(
        &self,
        index: &VolumeIndex,
        visit: impl FnMut(EntryIdx) -> bool,
    ) {
        match self {
            Worked::Related(set) => set.for_each_candidate(index, visit),
            Worked::Counted(set) => set.for_each_candidate(index, visit),
        }
    }
}

/// A sub-search worked out for one volume: which entries it holds for.
pub(super) struct RelatedSet {
    relation: Relation,
    kind: Option<Kind>,
    /// `Child`, `Descendant`: the folders it holds for. `Sibling`,
    /// `ParentChild`, `ParentSibling`: the folders holding a match.
    /// `Ancestor`: the folders at or below a folder whose name matches.
    /// `AncestorChild`, `AncestorSibling`: the folders at or below a folder
    /// holding a match.
    marks: Bits,
    /// `Sibling`: the folders holding two matches or more.
    twice: Bits,
    /// `Sibling`: the names the term matches, which tell whether the entry
    /// tested is a match itself.
    names: Option<NameSet>,
    /// `AncestorChild`, `AncestorSibling`: the root holds a match, so every
    /// folder is at or below one (and none is marked).
    whole: bool,
}

impl RelatedSet {
    /// Works out the sub-search of `relation` and `kind` whose term (compiled
    /// for this volume) is `term` over `index`, within `budget`: a term on
    /// the name when `by_name`, else a size or date test (see
    /// [`prop_holds`]).
    pub(super) fn new(
        index: &VolumeIndex,
        relation: Relation,
        kind: Option<Kind>,
        term: &CNode<'_>,
        by_name: bool,
        budget: &Budget,
    ) -> RelatedSet {
        let mut set = RelatedSet {
            relation,
            kind,
            marks: Bits::new(index.allocated()),
            twice: Bits::new(if relation == Relation::Sibling {
                index.allocated()
            } else {
                0
            }),
            names: None,
            whole: false,
        };
        let mut watch = Watch::new(budget);
        let mut tops = Vec::new();
        if by_name {
            let names = names_passing(index, term, kind == Some(Kind::Folder), budget);
            'names: for id in names.ids() {
                for e in index.name_chain(id) {
                    if watch.tick() {
                        break 'names;
                    }
                    set.visit(index, e, &mut tops);
                }
            }
            if relation == Relation::Sibling {
                set.names = Some(names);
            }
        } else {
            for e in index.live_entries() {
                if watch.tick() {
                    break;
                }
                if prop_holds(term, index, e) {
                    set.visit(index, e, &mut tops);
                }
            }
        }
        if matches!(
            relation,
            Relation::Ancestor | Relation::AncestorChild | Relation::AncestorSibling
        ) {
            if tops.contains(&index.root()) {
                // Below the root is everything: no subtree need be marked.
                set.whole = true;
            } else {
                set.mark_below(index, &mut tops, &mut watch);
            }
        }
        set
    }

    /// Marks what match `e` (its name or its size or date matched the term)
    /// gives the set, when it is one of the entries the sub-search looks
    /// for; a top whose subtree to mark goes to `tops`.
    fn visit(&mut self, index: &VolumeIndex, e: EntryIdx, tops: &mut Vec<EntryIdx>) {
        if !self.counts(index, e) {
            return;
        }
        let parent = index.entry(e).parent();
        match self.relation {
            Relation::Child | Relation::ParentChild | Relation::ParentSibling if parent != NIL => {
                self.marks.set(parent)
            }
            Relation::Descendant => {
                // Each step marks a folder not marked before, so the climb
                // ends, in a cycle too, and the climbs of all the matches
                // together cost the folders once: a marked folder's folders
                // above are marked already.
                let mut cur = parent;
                while cur != NIL && !self.marks.get(cur) {
                    self.marks.set(cur);
                    cur = index.entry(cur).parent();
                }
            }
            Relation::Sibling if parent != NIL => {
                if self.marks.get(parent) {
                    self.twice.set(parent);
                } else {
                    self.marks.set(parent);
                }
            }
            Relation::Ancestor => tops.push(e),
            // A folder's matches make it a top once.
            Relation::AncestorChild | Relation::AncestorSibling
                if parent != NIL && !self.marks.get(parent) =>
            {
                self.marks.set(parent);
                tops.push(parent);
            }
            _ => {}
        }
    }

    /// Whether `e`, whose name the term matched, is one of the entries the
    /// sub-search looks for: a real entry, of its kind.
    fn counts(&self, index: &VolumeIndex, e: EntryIdx) -> bool {
        !index.is_synthetic(e)
            && self
                .kind
                .is_none_or(|k| (k == Kind::Folder) == index.entry(e).is_dir())
    }

    /// Marks every folder at or below one of `tops` (folders): a top below
    /// another is skipped, as the walk from that one covers it, and so is
    /// one whose chain reaches no root.
    fn mark_below(&mut self, index: &VolumeIndex, tops: &mut [EntryIdx], watch: &mut Watch) {
        tops.sort_unstable();
        for &top in tops.iter() {
            if watch.tick() {
                return;
            }
            if !walk_starts_at(index, top, tops) {
                continue;
            }
            self.marks.set(top);
            for e in index.subtree_entries(top) {
                if watch.tick() {
                    return;
                }
                if index.entry(e).is_dir() {
                    self.marks.set(e);
                }
            }
        }
    }

    /// Whether [`RelatedSet::for_each_candidate`] gives fewer entries than
    /// the volume: not when the root holds a match of `ancestor-child:` or
    /// `ancestor-sibling:`.
    pub(super) fn bounds(&self) -> bool {
        !self.whole
    }

    /// Calls `visit` with each entry the sub-search may hold for, once,
    /// until it answers `false`: the marked folders of `child:` and
    /// `descendant:`, the entries of the marked folders of `sibling:`,
    /// `ancestor-name:`, `parent-child:` and `ancestor-child:`, and the
    /// entries of their folders for `parent-sibling:` and
    /// `ancestor-sibling:`. A scan whose only narrow term is a sub-search
    /// reads these instead of the volume.
    pub(super) fn for_each_candidate(
        &self,
        index: &VolumeIndex,
        mut visit: impl FnMut(EntryIdx) -> bool,
    ) {
        for folder in self.marks.ones() {
            match self.relation {
                Relation::Child | Relation::Descendant => {
                    if !visit(folder) {
                        return;
                    }
                }
                Relation::Sibling
                | Relation::Ancestor
                | Relation::ParentChild
                | Relation::AncestorChild => {
                    for e in index.children(folder) {
                        if !visit(e) {
                            return;
                        }
                    }
                }
                Relation::ParentSibling | Relation::AncestorSibling => {
                    for sub in index.children(folder) {
                        if !index.entry(sub).is_dir() {
                            continue;
                        }
                        for e in index.children(sub) {
                            if !visit(e) {
                                return;
                            }
                        }
                    }
                }
            }
        }
    }

    /// Whether the sub-search holds for entry `e`.
    #[inline]
    pub(super) fn holds(&self, index: &VolumeIndex, e: EntryIdx) -> bool {
        let parent = index.entry(e).parent();
        // The folder above the entry's folder.
        let grandparent = || {
            if parent == NIL {
                NIL
            } else {
                index.entry(parent).parent()
            }
        };
        match self.relation {
            // Only a folder is ever marked: only a folder holds entries.
            Relation::Child | Relation::Descendant => self.marks.get(e),
            Relation::Sibling => {
                if parent == NIL {
                    return false;
                }
                let itself = self
                    .names
                    .as_ref()
                    .is_some_and(|names| names.contains(index.name_id(e)))
                    && self.counts(index, e);
                if itself {
                    self.twice.get(parent)
                } else {
                    self.marks.get(parent)
                }
            }
            Relation::Ancestor | Relation::ParentChild => parent != NIL && self.marks.get(parent),
            Relation::AncestorChild => parent != NIL && (self.whole || self.marks.get(parent)),
            Relation::ParentSibling => {
                let up = grandparent();
                up != NIL && self.marks.get(up)
            }
            Relation::AncestorSibling => {
                let up = grandparent();
                up != NIL && (self.whole || self.marks.get(up))
            }
        }
    }
}

/// Whether entry `e` passes `term`, a child size or date test: the size and
/// date leaves the parser builds for `child-file-size:` and
/// `child-date-modified:`, and ORs of them. Any other leaf passes nothing.
fn prop_holds(term: &CNode<'_>, index: &VolumeIndex, e: EntryIdx) -> bool {
    let entry = index.entry(e);
    match term {
        CNode::True => true,
        CNode::False => false,
        CNode::And(items) => items.iter().all(|t| prop_holds(t, index, e)),
        CNode::Or(items) => items.iter().any(|t| prop_holds(t, index, e)),
        CNode::Not(inner) => !prop_holds(inner, index, e),
        CNode::Size(range) => size_matches(*range, entry.size()),
        // A folder's size is unknown to goz, but no `size:` names it, as
        // the search's own `size:unknown` does not.
        CNode::SizeUnknown => !entry.is_dir() && entry.size().is_none(),
        CNode::Date(date) => date.matches(entry.mtime()),
        _ => false,
    }
}

/// A descendant or sibling count worked out for one volume: which entries'
/// counts are in range.
pub(super) struct CountSet {
    /// The kind counted (`None`: files and folders).
    kind: Option<Kind>,
    siblings: bool,
    /// Descendants: the folders whose count is in range. Siblings: the
    /// folders whose entries of the kind counted, less one, are in range,
    /// which is the count of each of those entries.
    marks: Bits,
    /// Siblings: the folders whose entries of the kind counted are in
    /// range, the count of each of their other entries.
    others: Bits,
}

impl CountSet {
    /// Works out count `of` (one worked out for the volume, see
    /// [`CountOf::is_volume_wide`]) in `range` over `index`, within
    /// `budget`.
    pub(super) fn new(
        index: &VolumeIndex,
        of: CountOf,
        range: SizeRange,
        budget: &Budget,
    ) -> CountSet {
        let kind = match of {
            CountOf::DescendantFiles | CountOf::SiblingFiles => Some(Kind::File),
            CountOf::DescendantFolders | CountOf::SiblingFolders => Some(Kind::Folder),
            _ => None,
        };
        let siblings = matches!(
            of,
            CountOf::Siblings | CountOf::SiblingFiles | CountOf::SiblingFolders
        );
        let mut set = CountSet {
            kind,
            siblings,
            marks: Bits::new(index.allocated()),
            others: Bits::new(if siblings { index.allocated() } else { 0 }),
        };
        let mut watch = Watch::new(budget);
        if siblings {
            set.count_siblings(index, range, &mut watch);
        } else {
            set.count_descendants(index, range, &mut watch);
        }
        set
    }

    /// Whether `e` is one of the entries counted: a real entry, of the
    /// kind.
    fn counts(&self, index: &VolumeIndex, e: EntryIdx) -> bool {
        !index.is_synthetic(e) && kind_matches(self.kind, index.entry(e).is_dir())
    }

    /// Counts each folder's entries of the kind, and marks the folders
    /// whose count, less one or not, is in `range`. Every child chain is
    /// read once.
    fn count_siblings(&mut self, index: &VolumeIndex, range: SizeRange, watch: &mut Watch) {
        for folder in index.live_entries() {
            if watch.tick() {
                return;
            }
            if !index.entry(folder).is_dir() {
                continue;
            }
            let mut n = 0u64;
            for c in index.children(folder) {
                if watch.tick() {
                    return;
                }
                n += u64::from(self.counts(index, c));
            }
            if n > 0 && range.contains(n - 1) {
                self.marks.set(folder);
            }
            if range.contains(n) {
                self.others.set(folder);
            }
        }
    }

    /// Walks the volume from its root, depth first, summing each folder's
    /// entries of the kind at any depth as it closes the folder, and marks
    /// the folders whose sum is in `range`. A folder deeper than
    /// [`MAX_PATH_HOPS`], which has no path, is not entered, and the folders
    /// above it, whose sums it leaves short, are not marked.
    fn count_descendants(&mut self, index: &VolumeIndex, range: SizeRange, watch: &mut Watch) {
        let root = index.root();
        // Each open folder, its sum so far, whether a folder below it was
        // left out, and the rest of its entries.
        let mut open = vec![(root, 0u64, false, index.children(root))];
        loop {
            let depth = open.len();
            let Some(top) = open.last_mut() else {
                return;
            };
            if watch.tick() {
                return;
            }
            if let Some(c) = top.3.next() {
                top.1 += u64::from(self.counts(index, c));
                if index.entry(c).is_dir() {
                    if depth > MAX_PATH_HOPS {
                        top.2 = true;
                    } else {
                        open.push((c, 0, false, index.children(c)));
                    }
                }
                continue;
            }
            let Some((folder, n, cut, _)) = open.pop() else {
                return;
            };
            if !cut && folder != root && range.contains(n) {
                self.marks.set(folder);
            }
            if let Some(above) = open.last_mut() {
                above.1 += n;
                above.2 |= cut;
            }
        }
    }

    /// Calls `visit` with each entry the count may hold for, once, until it
    /// answers `false`: the marked folders of `descendant-count:`, the
    /// entries of the marked folders of `sibling-count:`.
    fn for_each_candidate(&self, index: &VolumeIndex, mut visit: impl FnMut(EntryIdx) -> bool) {
        if !self.siblings {
            for folder in self.marks.ones() {
                if !visit(folder) {
                    return;
                }
            }
            return;
        }
        for folder in self.marks.or_ones(Some(&self.others)) {
            for e in index.children(folder) {
                if !visit(e) {
                    return;
                }
            }
        }
    }

    /// Whether the count of entry `e` is in range.
    #[inline]
    pub(super) fn holds(&self, index: &VolumeIndex, e: EntryIdx) -> bool {
        if !self.siblings {
            // Only a folder is ever marked: only a folder has descendants.
            return self.marks.get(e);
        }
        let parent = index.entry(e).parent();
        if parent == NIL {
            return false;
        }
        if self.counts(index, e) {
            self.marks.get(parent)
        } else {
            self.others.get(parent)
        }
    }
}

/// The sizes of folder `e`'s files, summed (`total-child-size:`): `None`
/// for a file, or when one of the files' sizes is unknown. Reads the
/// folder's entries once.
pub(super) fn total_child_size(index: &VolumeIndex, e: EntryIdx) -> Option<u64> {
    if !index.entry(e).is_dir() {
        return None;
    }
    let mut total = 0u64;
    for c in index.children(e) {
        let child = index.entry(c);
        if index.is_synthetic(c) || child.is_dir() {
            continue;
        }
        total = total.saturating_add(child.size()?);
    }
    Some(total)
}

/// One bit per entry slot.
struct Bits(Vec<u64>);

impl Bits {
    fn new(len: usize) -> Bits {
        Bits(vec![0; len.div_ceil(64)])
    }

    #[inline]
    fn get(&self, e: EntryIdx) -> bool {
        let i = e as usize;
        self.0.get(i / 64).is_some_and(|w| w & (1 << (i % 64)) != 0)
    }

    #[inline]
    fn set(&mut self, e: EntryIdx) {
        let i = e as usize;
        if let Some(w) = self.0.get_mut(i / 64) {
            *w |= 1 << (i % 64);
        }
    }

    /// The entries whose bits are set, ascending.
    fn ones(&self) -> impl Iterator<Item = EntryIdx> + '_ {
        self.or_ones(None)
    }

    /// The entries whose bits are set here or in `other`, ascending.
    fn or_ones<'a>(&'a self, other: Option<&'a Bits>) -> impl Iterator<Item = EntryIdx> + 'a {
        self.0.iter().enumerate().flat_map(move |(w, &word)| {
            let mut word = word | other.and_then(|o| o.0.get(w)).copied().unwrap_or(0);
            std::iter::from_fn(move || {
                if word == 0 {
                    return None;
                }
                let bit = word.trailing_zeros() as usize;
                word &= word - 1;
                Some((w * 64 + bit) as EntryIdx)
            })
        })
    }
}

/// Whether `name`, a sub-search's term on the name (text, a wildcard, a
/// regular expression, or an OR of them), matches `raw`, a name that is no
/// entry's: the root's, the mount prefix.
pub(super) fn name_term_matches(name: &Expr, raw: &[u8], budget: &Budget) -> bool {
    let mut cps = CodePointBuf::new();
    let form = |case: bool, diacritics: bool| -> Cow<'_, [u8]> {
        if case {
            Cow::Borrowed(raw)
        } else if diacritics {
            Cow::Owned(fold_exact(raw))
        } else {
            Cow::Owned(fold(raw))
        }
    };
    match name {
        Expr::True => true,
        Expr::False => false,
        Expr::And(items) => items.iter().all(|e| name_term_matches(e, raw, budget)),
        Expr::Or(items) => items.iter().any(|e| name_term_matches(e, raw, budget)),
        Expr::Not(inner) => !name_term_matches(inner, raw, budget),
        Expr::Leaf(Leaf::Text {
            target: Target::Name,
            needle,
            case,
            diacritics,
            anchors,
            skip,
        }) => TextMatcher::new(needle, *anchors, *skip).matches(
            &form(*case, *diacritics),
            &mut cps,
            budget,
        ),
        Expr::Leaf(Leaf::Glob {
            target: Target::Name,
            glob,
            case,
            diacritics,
        }) => {
            decode_into(&form(*case, *diacritics), &mut cps);
            glob.matches_decoded(&cps, budget)
        }
        Expr::Leaf(Leaf::Regex {
            target: Target::Name,
            regex,
        }) => regex.is_match(raw),
        // A sub-search's term is on the name alone.
        Expr::Leaf(_) => false,
    }
}
