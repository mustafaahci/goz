//! The sub-searches of other entries' names: `child:`, `descendant:`,
//! `sibling:` and `ancestor-name:` (and the `-file:` and `-folder:` forms),
//! as a live Everything 1.5 answers them (W7, replayed in the engine's
//! tests).
//!
//! Each is worked out once per volume and query, before the search: the
//! term on the name runs once over the distinct names (the folded names
//! holding its needles, or a pass over every name across cores), and the
//! entries bearing the names it matches are mapped through the parent and
//! child links the index keeps onto a set of folders, one bit an entry.
//! Testing an entry is then a lookup or two:
//!
//! - `child:x`: a folder one of whose entries is named `x`.
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
//!
//! The work is checked against the query's budget as it goes; a spent
//! budget leaves the set unfinished and the query is refused, as any other
//! is. The sets of one query take a bit per entry each, and a query holds at
//! most 8 sub-searches (`MAX_RELATED_TERMS`).

use super::budget::{Budget, Watch};
use super::cexpr::CNode;
use super::engine::{NameSet, names_passing, walk_starts_at};
use super::expr::{Expr, Leaf, Relation, Target};
use super::glob::{CodePointBuf, decode_into};
use super::parse::Kind;
use super::text::TextMatcher;
use crate::fold::{fold, fold_exact};
use crate::index::VolumeIndex;
use crate::types::{EntryIdx, NIL};
use std::borrow::Cow;

/// A sub-search worked out for one volume: which entries it holds for.
pub(super) struct RelatedSet {
    relation: Relation,
    kind: Option<Kind>,
    /// `Child`, `Descendant`: the folders it holds for. `Sibling`: the
    /// folders holding a match. `Ancestor`: the folders at or below a folder
    /// whose name matches.
    marks: Bits,
    /// `Sibling`: the folders holding two matches or more.
    twice: Bits,
    /// `Sibling`: the names the term matches, which tell whether the entry
    /// tested is a match itself.
    names: Option<NameSet>,
}

impl RelatedSet {
    /// Works out the sub-search of `relation` and `kind` whose term on the
    /// name is `name` (compiled for this volume) over `index`, within
    /// `budget`.
    pub(super) fn new(
        index: &VolumeIndex,
        relation: Relation,
        kind: Option<Kind>,
        name: &CNode<'_>,
        budget: &Budget,
    ) -> RelatedSet {
        let names = names_passing(index, name, kind == Some(Kind::Folder), budget);
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
        };
        let mut watch = Watch::new(budget);
        let mut tops = Vec::new();
        'names: for id in names.ids() {
            for e in index.name_chain(id) {
                if watch.tick() {
                    break 'names;
                }
                if !set.counts(index, e) {
                    continue;
                }
                let parent = index.entry(e).parent();
                match relation {
                    Relation::Child if parent != NIL => set.marks.set(parent),
                    Relation::Descendant => {
                        // Each step marks a folder not marked before, so the
                        // climb ends, in a cycle too, and the climbs of all
                        // the matches together cost the folders once: a
                        // marked folder's folders above are marked already.
                        let mut cur = parent;
                        while cur != NIL && !set.marks.get(cur) {
                            set.marks.set(cur);
                            cur = index.entry(cur).parent();
                        }
                    }
                    Relation::Sibling if parent != NIL => {
                        if set.marks.get(parent) {
                            set.twice.set(parent);
                        } else {
                            set.marks.set(parent);
                        }
                    }
                    Relation::Ancestor => tops.push(e),
                    _ => {}
                }
            }
        }
        if relation == Relation::Ancestor {
            set.mark_below(index, &mut tops, &mut watch);
        }
        if relation == Relation::Sibling {
            set.names = Some(names);
        }
        set
    }

    /// Whether `e`, whose name the term matched, is one of the entries the
    /// sub-search looks for: a real entry, of its kind.
    fn counts(&self, index: &VolumeIndex, e: EntryIdx) -> bool {
        !index.is_synthetic(e)
            && self
                .kind
                .is_none_or(|k| (k == Kind::Folder) == index.entry(e).is_dir())
    }

    /// Marks every folder at or below one of `tops` (folders whose names
    /// match): a top below another is skipped, as the walk from that one
    /// covers it, and so is one whose chain reaches no root.
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

    /// Calls `visit` with each entry the sub-search may hold for, once,
    /// until it answers `false`: the marked folders of `child:` and
    /// `descendant:`, the entries of the marked folders of `sibling:` and
    /// `ancestor-name:`. A scan whose only narrow term is a sub-search reads
    /// these instead of the volume.
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
                Relation::Sibling | Relation::Ancestor => {
                    for e in index.children(folder) {
                        if !visit(e) {
                            return;
                        }
                    }
                }
            }
        }
    }

    /// Whether the sub-search holds for entry `e`.
    #[inline]
    pub(super) fn holds(&self, index: &VolumeIndex, e: EntryIdx) -> bool {
        match self.relation {
            // Only a folder is ever marked: only a folder holds entries.
            Relation::Child | Relation::Descendant => self.marks.get(e),
            Relation::Sibling => {
                let parent = index.entry(e).parent();
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
            Relation::Ancestor => {
                let parent = index.entry(e).parent();
                parent != NIL && self.marks.get(parent)
            }
        }
    }
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
        self.0.iter().enumerate().flat_map(|(w, &word)| {
            let mut word = word;
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
