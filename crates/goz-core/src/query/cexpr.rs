//! A query's expression compiled for one volume, and its evaluation against
//! a name or an entry.
//!
//! The parser leaves in [`ParsedQuery::expr`](super::ParsedQuery) what the
//! buckets cannot hold: ORs, NOTs, groups. [`compile`] turns it into a
//! [`CNode`] tree for one volume: needles become `memmem` finders, and each
//! path term the volume's mount prefix decides becomes a constant (`C:\` is
//! in every full path on C: and in none on D:), after which the tree is
//! folded again, so that `c: report` runs on C: as `report` does, and on D:
//! reads nothing. [`split`] then parts the top-level terms the name alone
//! decides from those that need the entry, so the first can run in the pass
//! over the distinct names, the fast path of a name-level query.
//!
//! The children of each AND and OR are ordered cheapest first (a kind, a
//! size or date compare, a name search, a wildcard, an extension, a full-path
//! search, a sub-path wildcard), since AND and OR do not care about order and
//! evaluation stops at the first child that decides.

use super::budget::Budget;
use super::dates::DateFilter;
use super::engine::{
    CACHED_PATH_MAX, anchored_lead, ext_in, kind_matches, size_matches, term_admits,
};
use super::expr::{Expr, Leaf, Target};
use super::glob::{CodePointBuf, Glob, decode_into};
use super::parse::{Kind, SizeRange, Wildcard};
use crate::index::{NameId, VolumeIndex};
use crate::types::EntryIdx;
use memchr::memmem;

/// A compiled expression.
pub(super) enum CNode<'a> {
    True,
    False,
    And(Vec<CNode<'a>>),
    Or(Vec<CNode<'a>>),
    Not(Box<CNode<'a>>),
    /// The name holds the needle.
    Name(memmem::Finder<'a>),
    /// The name matches a name wildcard.
    NameGlob(&'a Wildcard),
    /// The last part of the name matches a path wildcard of one part and no
    /// `**` (`path:*.md`), which below a prefix ending with a separator is
    /// what the full path's last part matches. `lit`: its longest literal.
    NamePart {
        part: &'a Glob,
        lit: Option<Vec<u8>>,
    },
    /// The extension is one of these.
    Ext(&'a [Vec<u8>]),
    Kind(Kind),
    Size(SizeRange),
    Date(DateFilter),
    /// The full path holds the needle.
    Path(memmem::Finder<'a>),
    /// The full path matches a sub-path wildcard, whose ends are checked
    /// first: the literal it ends with, and the one it starts with when that
    /// holds the drive's `:` (see `anchored_lead`).
    PathGlob {
        glob: &'a Wildcard,
        lead: Option<Vec<u8>>,
        trail: Vec<u8>,
        /// Text the name of every match holds (see
        /// [`Wildcard::name_literal`]), when the prefix ends with a
        /// separator, so that the last part of a full path is the name.
        name_lit: Option<Vec<u8>>,
    },
}

/// Compiles `e` for the volume mounted at `prefix` (folded iff the query
/// ignores case), folding each path term the prefix decides into a
/// constant and the tree around it. `parts_are_names`: the prefix ends with
/// a separator, so the last part of a full path is the last part of the
/// name.
pub(super) fn compile<'a>(e: &'a Expr, prefix: &[u8], parts_are_names: bool) -> CNode<'a> {
    let node = match e {
        Expr::True => CNode::True,
        Expr::False => CNode::False,
        Expr::And(items) => CNode::And(
            items
                .iter()
                .map(|i| compile(i, prefix, parts_are_names))
                .collect(),
        ),
        Expr::Or(items) => CNode::Or(
            items
                .iter()
                .map(|i| compile(i, prefix, parts_are_names))
                .collect(),
        ),
        Expr::Not(inner) => CNode::Not(Box::new(compile(inner, prefix, parts_are_names))),
        Expr::Leaf(leaf) => match leaf {
            Leaf::Text {
                target: Target::Name,
                needle,
            } => CNode::Name(memmem::Finder::new(needle)),
            Leaf::Text {
                target: Target::FullPath,
                needle,
            } => {
                if memmem::find(prefix, needle).is_some() {
                    // In every full path on the volume.
                    CNode::True
                } else if !term_admits(needle, prefix) {
                    // Its drive is another volume's.
                    CNode::False
                } else {
                    CNode::Path(memmem::Finder::new(needle))
                }
            }
            Leaf::Glob(w) if w.is_name() => CNode::NameGlob(w),
            Leaf::Glob(w) => match w.single_part() {
                Some(part) if parts_are_names => CNode::NamePart {
                    part,
                    lit: w.longest_literal(),
                },
                _ => {
                    let lead = anchored_lead(w);
                    if lead.as_deref().is_some_and(|lead| {
                        let n = lead.len().min(prefix.len());
                        lead[..n] != prefix[..n]
                    }) {
                        CNode::False
                    } else {
                        CNode::PathGlob {
                            glob: w,
                            lead,
                            trail: w.trailing_literal(),
                            name_lit: w.name_literal().filter(|_| parts_are_names),
                        }
                    }
                }
            },
            Leaf::Kind(kind) => CNode::Kind(*kind),
            Leaf::Ext(exts) => CNode::Ext(exts),
            Leaf::Size(range) => CNode::Size(*range),
            Leaf::Date(date) => CNode::Date(*date),
        },
    };
    node.folded()
}

/// The top-level terms of `node` (neither `True` nor `False`) the name
/// alone decides, and the rest, each ANDed together, or `None`.
pub(super) fn split(node: CNode<'_>) -> (Option<CNode<'_>>, Option<CNode<'_>>) {
    let items = match node {
        CNode::True => return (None, None),
        CNode::And(items) => items,
        other => vec![other],
    };
    let (by_name, by_entry): (Vec<CNode>, Vec<CNode>) =
        items.into_iter().partition(CNode::decided_by_name);
    (all_of(by_name), all_of(by_entry))
}

/// The AND of `items`, or `None` for none.
fn all_of(mut items: Vec<CNode<'_>>) -> Option<CNode<'_>> {
    match items.len() {
        0 => None,
        1 => items.pop(),
        _ => Some(CNode::And(items)),
    }
}

/// The most needles a union prefilter scans the names for. Each is a pass
/// over the folded names, and past a few, testing every name costs less
/// than finding them.
pub(super) const UNION_MAX: usize = 8;

/// The prefilter of a case-insensitive expression: folded needles such that
/// the folded name of every entry it matches holds at least one of them, so
/// that scanning the folded names for them finds every match. `conjuncts`
/// must all match; the best plan of any one of them serves. `None` when no
/// plan exists, as for a NOT, or an OR with a branch no needle covers.
pub(super) fn plan<'n, 'a: 'n>(
    conjuncts: impl IntoIterator<Item = &'n CNode<'a>>,
) -> Option<Vec<Vec<u8>>> {
    best(conjuncts.into_iter().filter_map(CNode::needles))
}

/// The best of `plans`: the fewest needles, as each is a pass over the
/// names, and then the longest shortest needle, as a longer needle is
/// usually rarer.
fn best(plans: impl Iterator<Item = Vec<Vec<u8>>>) -> Option<Vec<Vec<u8>>> {
    plans.min_by_key(|p| {
        let shortest = p.iter().map(Vec::len).min().unwrap_or(0);
        (p.len(), std::cmp::Reverse(shortest))
    })
}

/// The needles of an OR: every branch's, less any that holds another (a
/// name holding it holds the other too), or `None` past [`UNION_MAX`].
fn union(mut needles: Vec<Vec<u8>>) -> Option<Vec<Vec<u8>>> {
    needles.sort_by_key(Vec::len);
    let mut kept: Vec<Vec<u8>> = Vec::new();
    for needle in needles {
        if !kept.iter().any(|k| memmem::find(&needle, k).is_some()) {
            kept.push(needle);
        }
    }
    (kept.len() <= UNION_MAX).then_some(kept)
}

impl<'a> CNode<'a> {
    /// `self`, whose children are folded already, with its constants folded
    /// away, nested ANDs and ORs flattened, and its children cheapest first.
    fn folded(self) -> CNode<'a> {
        match self {
            CNode::And(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    match item {
                        CNode::True => {}
                        CNode::False => return CNode::False,
                        CNode::And(inner) => out.extend(inner),
                        other => out.push(other),
                    }
                }
                match out.len() {
                    0 => CNode::True,
                    1 => out.pop().unwrap_or(CNode::True),
                    _ => {
                        out.sort_by_key(CNode::cost);
                        CNode::And(out)
                    }
                }
            }
            CNode::Or(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    match item {
                        CNode::False => {}
                        CNode::True => return CNode::True,
                        CNode::Or(inner) => out.extend(inner),
                        other => out.push(other),
                    }
                }
                match out.len() {
                    0 => CNode::False,
                    1 => out.pop().unwrap_or(CNode::False),
                    _ => {
                        out.sort_by_key(CNode::cost);
                        CNode::Or(out)
                    }
                }
            }
            CNode::Not(inner) => match *inner {
                CNode::True => CNode::False,
                CNode::False => CNode::True,
                CNode::Not(twice) => *twice,
                other => CNode::Not(Box::new(other)),
            },
            leaf => leaf,
        }
    }

    /// How costly the node is to decide, as a rank: its costliest leaf, then
    /// how many leaves it has.
    fn cost(&self) -> (u8, usize) {
        match self {
            CNode::True | CNode::False => (0, 0),
            CNode::Kind(_) => (1, 1),
            CNode::Size(_) | CNode::Date(_) => (2, 1),
            CNode::Name(_) => (3, 1),
            CNode::NameGlob(_) | CNode::NamePart { .. } => (4, 1),
            CNode::Ext(_) => (5, 1),
            CNode::Path(_) => (6, 1),
            CNode::PathGlob { .. } => (7, 1),
            CNode::Not(inner) => inner.cost(),
            CNode::And(items) | CNode::Or(items) => items
                .iter()
                .map(CNode::cost)
                .fold((0, 0), |(class, leaves), (c, l)| (class.max(c), leaves + l)),
        }
    }

    /// Whether every leaf is decided by the name alone.
    fn decided_by_name(&self) -> bool {
        match self {
            CNode::True
            | CNode::False
            | CNode::Name(_)
            | CNode::NameGlob(_)
            | CNode::NamePart { .. }
            | CNode::Ext(_) => true,
            CNode::Kind(_)
            | CNode::Size(_)
            | CNode::Date(_)
            | CNode::Path(_)
            | CNode::PathGlob { .. } => false,
            CNode::Not(inner) => inner.decided_by_name(),
            CNode::And(items) | CNode::Or(items) => items.iter().all(CNode::decided_by_name),
        }
    }

    /// The needles of [`plan`] for this node: a name search's own needle, a
    /// name wildcard's longest literal, `.ext` for each of a few extensions;
    /// the best of an AND's children; all of an OR's, when each has some.
    /// What a NOT, a kind, size, date or path term matches holds no needle
    /// its name must hold.
    fn needles(&self) -> Option<Vec<Vec<u8>>> {
        match self {
            CNode::Name(finder) => Some(vec![finder.needle().to_vec()]),
            CNode::NameGlob(w) => w.longest_literal().map(|lit| vec![lit]),
            CNode::NamePart { lit, .. } | CNode::PathGlob { name_lit: lit, .. } => {
                lit.clone().map(|lit| vec![lit])
            }
            CNode::Ext(exts) if exts.len() <= UNION_MAX => {
                Some(exts.iter().map(|e| [&b"."[..], e].concat()).collect())
            }
            CNode::And(items) => best(items.iter().filter_map(CNode::needles)),
            CNode::Or(items) => {
                let mut all = Vec::new();
                for item in items {
                    all.extend(item.needles()?);
                }
                union(all)
            }
            // Matches nothing, so no name need be scanned for.
            CNode::False => Some(Vec::new()),
            _ => None,
        }
    }

    /// Whether some leaf needs the full path.
    pub(super) fn tests_paths(&self) -> bool {
        match self {
            CNode::Path(_) | CNode::PathGlob { .. } => true,
            CNode::Not(inner) => inner.tests_paths(),
            CNode::And(items) | CNode::Or(items) => items.iter().any(CNode::tests_paths),
            _ => false,
        }
    }

    /// Whether the node matches `s`. Answers `false` where a leaf found the
    /// budget spent, as the query is then refused whatever this answers.
    pub(super) fn eval(&self, s: &mut Subject<'_, '_>) -> bool {
        match self {
            CNode::True => true,
            CNode::False => false,
            CNode::And(items) => items.iter().all(|c| c.eval(s)),
            CNode::Or(items) => items.iter().any(|c| c.eval(s)),
            CNode::Not(inner) => !inner.eval(s),
            CNode::Name(finder) => finder.find(s.name).is_some(),
            CNode::NameGlob(w) => {
                let budget = s.budget;
                w.matches_decoded(s.name_code_points(), budget)
            }
            CNode::NamePart { part, .. } => {
                let budget = s.budget;
                let name = s.name_code_points();
                let last = name
                    .iter()
                    .rposition(|&c| c == u32::from(b'\\'))
                    .map_or(name, |sep| &name[sep + 1..]);
                part.matches_decoded(last, budget)
            }
            CNode::Ext(exts) => ext_in(exts, s.raw, &mut s.scratch.ext),
            CNode::Kind(kind) => kind_matches(Some(*kind), s.is_dir()),
            CNode::Size(range) => size_matches(*range, s.size()),
            CNode::Date(date) => date.matches(s.mtime()),
            CNode::Path(finder) => {
                let budget = s.budget;
                s.full_path()
                    .is_some_and(|p| !long_and_spent(p, budget) && finder.find(p).is_some())
            }
            CNode::PathGlob {
                glob, lead, trail, ..
            } => {
                let budget = s.budget;
                let fits = s.full_path().is_some_and(|p| {
                    lead.as_deref().is_none_or(|lead| p.starts_with(lead))
                        && p.ends_with(trail)
                        && !long_and_spent(p, budget)
                });
                fits && s
                    .path_code_points()
                    .is_some_and(|cps| glob.matches_decoded(cps, budget))
            }
        }
    }
}

/// Whether `path` is longer than any real one and the budget is spent: a
/// search or decode of a path of megabytes checks the budget first, as each
/// term's search of such a path does in `decide_needs`.
fn long_and_spent(path: &[u8], budget: &Budget) -> bool {
    path.len() > CACHED_PATH_MAX && budget.check()
}

/// Scratch buffers a search reuses from one candidate to the next, so that
/// testing one allocates nothing.
#[derive(Default)]
pub(super) struct Scratch {
    /// A name or full path decoded for a wildcard.
    pub(super) cps: CodePointBuf,
    /// An extension, folded.
    pub(super) ext: Vec<u8>,
    /// A full path, for the expression.
    pub(super) path: Vec<u8>,
    /// That path decoded.
    pub(super) path_cps: CodePointBuf,
}

/// Fills its buffer with an entry's full path (as the query compares it)
/// and says whether it has one.
pub(super) type PathSource<'x> = &'x mut dyn FnMut(&mut Vec<u8>) -> bool;

/// What an expression is evaluated against: a distinct name, or an entry,
/// whose columns and full path are read only when a leaf asks.
pub(super) struct Subject<'s, 'x> {
    index: &'s VolumeIndex,
    /// The name as text leaves compare it: folded iff the query ignores case.
    name: &'s [u8],
    /// The name as stored, for the extension.
    raw: &'s [u8],
    /// `None` at the name level, where no leaf reads the entry.
    entry: Option<EntryIdx>,
    budget: &'s Budget<'s>,
    scratch: &'x mut Scratch,
    full_path: Option<PathSource<'x>>,
    name_decoded: bool,
    path: Fetched,
}

/// How far the subject's full path has been read.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fetched {
    Unread,
    /// The entry has no full path.
    Missing,
    Read,
    Decoded,
}

impl<'s, 'x> Subject<'s, 'x> {
    /// Name `id`, at the name level.
    pub(super) fn name(
        index: &'s VolumeIndex,
        id: NameId,
        match_case: bool,
        budget: &'s Budget<'s>,
        scratch: &'x mut Scratch,
    ) -> Self {
        Subject {
            index,
            name: if match_case {
                index.name_raw(id)
            } else {
                index.name_folded(id)
            },
            raw: index.name_raw(id),
            entry: None,
            budget,
            scratch,
            full_path: None,
            name_decoded: false,
            path: Fetched::Unread,
        }
    }

    /// Entry `idx`, whose full path `full_path` gives.
    pub(super) fn entry(
        index: &'s VolumeIndex,
        idx: EntryIdx,
        match_case: bool,
        budget: &'s Budget<'s>,
        scratch: &'x mut Scratch,
        full_path: PathSource<'x>,
    ) -> Self {
        let mut s = Subject::name(index, index.name_id(idx), match_case, budget, scratch);
        s.entry = Some(idx);
        s.full_path = Some(full_path);
        s
    }

    fn name_code_points(&mut self) -> &[u32] {
        if !self.name_decoded {
            decode_into(self.name, &mut self.scratch.cps);
            self.name_decoded = true;
        }
        &self.scratch.cps
    }

    fn is_dir(&self) -> bool {
        self.entry.is_some_and(|e| self.index.entry(e).is_dir())
    }

    fn size(&self) -> Option<u64> {
        self.entry.and_then(|e| self.index.entry(e).size())
    }

    fn mtime(&self) -> Option<i64> {
        self.entry.and_then(|e| self.index.entry(e).mtime())
    }

    fn full_path(&mut self) -> Option<&[u8]> {
        if self.path == Fetched::Unread {
            let found = match self.full_path.as_mut() {
                Some(fill) => {
                    self.scratch.path.clear();
                    fill(&mut self.scratch.path)
                }
                None => false,
            };
            self.path = if found {
                Fetched::Read
            } else {
                Fetched::Missing
            };
        }
        (self.path != Fetched::Missing).then_some(self.scratch.path.as_slice())
    }

    fn path_code_points(&mut self) -> Option<&[u32]> {
        self.full_path()?;
        if self.path == Fetched::Read {
            decode_into(&self.scratch.path, &mut self.scratch.path_cps);
            self.path = Fetched::Decoded;
        }
        Some(&self.scratch.path_cps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::parse::parse_query;

    /// The compiled expression of `query` on the volume at `prefix`.
    fn compiled(query: &str, prefix: &str, f: impl FnOnce(&CNode)) {
        let parsed = parse_query(query).unwrap();
        let prefix = crate::fold::fold(prefix.as_bytes());
        let e = parsed.expr.as_ref().expect("an expression");
        f(&compile(e, &prefix, true));
    }

    /// A path term the mount prefix decides becomes a constant, and the
    /// tree folds around it.
    #[test]
    fn the_mount_prefix_folds_path_terms() {
        compiled(r"c:\|report", r"C:\", |n| assert!(matches!(n, CNode::True)));
        compiled(r"c:\|report", r"D:\", |n| {
            assert!(matches!(n, CNode::Name(_)))
        });
        compiled(r"!c:\", r"C:\", |n| assert!(matches!(n, CNode::False)));
        compiled(r"!c:\", r"D:\", |n| assert!(matches!(n, CNode::True)));
        compiled(r"a|c:\x\*.txt", r"D:\", |n| {
            assert!(matches!(n, CNode::Name(_)))
        });
        compiled(r"a|c:\x\*.txt", r"C:\", |n| {
            assert!(matches!(n, CNode::Or(items) if items.len() == 2));
        });
        compiled(r"a|\data\", r"C:\mnt\data\", |n| {
            assert!(matches!(n, CNode::True))
        });
    }

    /// Cheap children go first; the name decides what holds no entry leaf.
    #[test]
    fn children_go_cheapest_first_and_split_by_what_they_need() {
        compiled(r"x\*.txt|a\b|*.md|ext:txt|abc|size:>1|file:", r"C:\", |n| {
            let CNode::Or(items) = n else { panic!() };
            let costs: Vec<u8> = items.iter().map(|c| c.cost().0).collect();
            assert_eq!(costs, [1, 2, 3, 4, 5, 6, 7]);
        });
        let parsed = parse_query("<a|b> <c|file:> <d|x\\y>").unwrap();
        let node = compile(parsed.expr.as_ref().unwrap(), br"c:\", true);
        let (name, entry) = split(node);
        assert!(name.as_ref().is_some_and(|n| !n.tests_paths()));
        let entry = entry.expect("entry terms");
        assert!(entry.tests_paths());
        let CNode::And(items) = entry else { panic!() };
        assert_eq!(items.len(), 2);
    }

    /// The needles of `query`'s expression on C:, sorted.
    fn needles_of(query: &str) -> Option<Vec<String>> {
        let parsed = parse_query(query).unwrap();
        let node = compile(parsed.expr.as_ref().expect("an expression"), br"c:\", true);
        plan([&node]).map(|needles| {
            let mut out: Vec<String> = needles
                .into_iter()
                .map(|n| String::from_utf8(n).unwrap())
                .collect();
            out.sort();
            out
        })
    }

    /// An OR scans for a needle per branch, an AND for its best branch's;
    /// a NOT, or a branch with no needle, leaves no plan.
    #[test]
    fn a_plan_covers_every_branch_of_an_or() {
        let s = |v: &[&str]| Some(v.iter().map(|x| x.to_string()).collect::<Vec<_>>());
        assert_eq!(needles_of("kernel32|report"), s(&["kernel32", "report"]));
        assert_eq!(needles_of("*.dll|*.TXT"), s(&[".dll", ".txt"]));
        assert_eq!(
            needles_of("<kernel32 dll>|<report v3>"),
            s(&["kernel32", "report"]),
            "the longer needle of each AND"
        );
        assert_eq!(
            needles_of("abc|abcd|xabcx"),
            s(&["abc"]),
            "a needle holding another goes"
        );
        assert_eq!(needles_of("ext:jpg;png|gif"), s(&[".jpg", ".png", "gif"]));
        assert_eq!(needles_of("a|!b"), None);
        assert_eq!(needles_of("a|file:"), None);
        assert_eq!(needles_of("a|*"), None, "a wildcard with no literal");
        assert_eq!(needles_of(r"a|x\y"), None);
        assert_eq!(needles_of("!a"), None);
        assert_eq!(
            needles_of("<a|bb> <ccc|d|e>"),
            s(&["a", "bb"]),
            "fewest needles"
        );
        assert_eq!(
            needles_of("<a|bbb> <cc|dd>"),
            s(&["cc", "dd"]),
            "longest shortest"
        );
        let nine: Vec<String> = (0..=UNION_MAX).map(|i| format!("n{i}x")).collect();
        assert_eq!(needles_of(&nine.join("|")), None, "past the cap");
        assert_eq!(
            needles_of(&nine[..UNION_MAX].join("|")).map(|n| n.len()),
            Some(UNION_MAX)
        );
    }
}
