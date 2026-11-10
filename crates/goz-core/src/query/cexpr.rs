//! A query's expression compiled for one volume, and its evaluation against
//! a name or an entry.
//!
//! The parser leaves in [`ParsedQuery::expr`](super::ParsedQuery) what the
//! buckets cannot hold: ORs, NOTs, groups, terms with modifiers, functions.
//! [`compile`] turns it into a [`CNode`] tree for one volume: needles become
//! `memmem` finders, `parent:` and `ancestor:` folders are looked up in the
//! volume, and each term the volume's mount prefix decides becomes a
//! constant (`C:\` is in every full path on C: and in none on D:, `d:` is
//! this drive or not), after which the tree is folded again, so that
//! `c:\x|report` runs on D: as `report` does, and `!c:\` reads nothing on
//! C:. [`split`] then parts the top-level terms the name alone decides from
//! those that need the entry, so the first can run in the pass over the
//! distinct names, the fast path of a name-level query.
//!
//! The children of each AND and OR are ordered cheapest first (a kind, a
//! size or date compare, a name search, a wildcard, an extension, a full-path
//! search, a sub-path wildcard), since AND and OR do not care about order and
//! evaluation stops at the first child that decides.

use super::budget::Budget;
use super::dates::DateFilter;
use super::engine::{CACHED_PATH_MAX, anchored_lead, size_matches, term_admits};
use super::expr::{Expr, Leaf, LenOf, Target};
use super::glob::{CodePointBuf, Glob, decode_into};
use super::parse::{Kind, SizeRange, Wildcard};
use super::text::{TextMatcher, utf16_len};
use crate::index::volume::MAX_PATH_HOPS;
use crate::index::{NameId, VolumeIndex};
use crate::types::{EntryIdx, NIL};
use memchr::memmem;
use rustc_hash::FxHashSet;

/// A compiled expression.
pub(super) enum CNode<'a> {
    True,
    False,
    And(Vec<CNode<'a>>),
    Or(Vec<CNode<'a>>),
    Not(Box<CNode<'a>>),
    /// The name holds the needle (folded unless `case`).
    Name {
        finder: memmem::Finder<'a>,
        case: bool,
    },
    /// A text term with anchors or skips, or on a target that is neither
    /// the name nor the full path.
    Text {
        target: Target,
        matcher: Box<TextMatcher<'a>>,
        case: bool,
    },
    /// The name matches a name wildcard.
    NameGlob {
        glob: &'a Wildcard,
        case: bool,
    },
    /// The last part of the name matches a path wildcard of one part and no
    /// `**` (`path:*.md`), which below a prefix ending with a separator is
    /// what the full path's last part matches. `lit`: its longest literal.
    NamePart {
        part: &'a Glob,
        lit: Option<Vec<u8>>,
        case: bool,
    },
    /// The whole of a target other than the name and the full path matches
    /// a wildcard (`stem:doc*`).
    TargetGlob {
        target: Target,
        glob: &'a Wildcard,
        case: bool,
    },
    /// A file whose extension is one of these.
    Ext(Box<ExtMatcher>),
    Kind(Kind),
    Size(SizeRange),
    SizeUnknown,
    Date(DateFilter),
    Len {
        of: LenOf,
        range: SizeRange,
    },
    /// The entry's folder is one of these.
    Parent(Vec<EntryIdx>),
    /// One of the entry's folders is one of these.
    Ancestor(Vec<EntryIdx>),
    /// The full path holds the needle.
    Path {
        finder: memmem::Finder<'a>,
        case: bool,
    },
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
        case: bool,
    },
}

/// What [`compile`] compiles an expression against: one volume.
pub(super) struct Volume<'i> {
    pub(super) index: &'i VolumeIndex,
    /// The mount prefix as typed, and folded.
    pub(super) raw_prefix: &'i [u8],
    pub(super) folded_prefix: &'i [u8],
    /// The prefix ends with a separator, so the last part of a full path is
    /// the last part of the name.
    pub(super) parts_are_names: bool,
}

impl Volume<'_> {
    /// The prefix as a term matching case (`case`) or not compares it.
    fn prefix(&self, case: bool) -> &[u8] {
        if case {
            self.raw_prefix
        } else {
            self.folded_prefix
        }
    }

    /// The folders `paths` (absolute, folded) name on this volume: `Ok`
    /// with the entries of those on it, or `Err(())` when one of them holds
    /// the whole volume (`ancestor:C:`, or a folder above a mount folder). A
    /// path names every folder it matches case-insensitively, siblings that
    /// differ by case alone included, as Everything compares paths.
    fn folders(&self, paths: &[Vec<u8>], ancestor: bool) -> Result<Vec<EntryIdx>, ()> {
        let top = self
            .folded_prefix
            .strip_suffix(b"\\")
            .unwrap_or(self.folded_prefix);
        let mut found = Vec::new();
        for path in paths {
            if path.as_slice() == top {
                if ancestor {
                    return Err(());
                }
                found.push(self.index.root());
            } else if let Some(rest) = path
                .strip_prefix(top)
                .and_then(|rest| rest.strip_prefix(b"\\"))
                .filter(|_| !top.is_empty())
            {
                found.extend(self.below_root(rest));
            } else if ancestor
                && top
                    .strip_prefix(path.as_slice())
                    .is_some_and(|below| below.starts_with(b"\\"))
            {
                // The volume is mounted in a folder below this one.
                return Err(());
            }
        }
        found.sort_unstable();
        found.dedup();
        Ok(found)
    }

    /// The folders at `rest` (folded, `\`-separated) below the root.
    fn below_root(&self, rest: &[u8]) -> Vec<EntryIdx> {
        let mut level = vec![self.index.root()];
        for part in rest.split(|&b| b == b'\\').filter(|p| !p.is_empty()) {
            level = level
                .iter()
                .flat_map(|&dir| self.index.child_dirs(dir, part))
                .collect();
            if level.is_empty() {
                break;
            }
        }
        level
    }
}

/// Compiles `e` for `volume`, folding each term the volume decides into a
/// constant and the tree around it.
pub(super) fn compile<'a>(e: &'a Expr, volume: &Volume) -> CNode<'a> {
    let node = match e {
        Expr::True => CNode::True,
        Expr::False => CNode::False,
        Expr::And(items) => CNode::And(items.iter().map(|i| compile(i, volume)).collect()),
        Expr::Or(items) => CNode::Or(items.iter().map(|i| compile(i, volume)).collect()),
        Expr::Not(inner) => CNode::Not(Box::new(compile(inner, volume))),
        Expr::Leaf(leaf) => compile_leaf(leaf, volume),
    };
    node.folded()
}

fn compile_leaf<'a>(leaf: &'a Leaf, volume: &Volume) -> CNode<'a> {
    match leaf {
        Leaf::Text {
            target,
            needle,
            case,
            anchors,
            skip,
        } => {
            let case = *case;
            let plain = anchors.is_free() && skip.is_none();
            match target {
                Target::Name if plain => CNode::Name {
                    finder: memmem::Finder::new(needle),
                    case,
                },
                Target::FullPath if plain => {
                    let prefix = volume.prefix(case);
                    if memmem::find(prefix, needle).is_some() {
                        // In every full path on the volume.
                        CNode::True
                    } else if !term_admits(needle, prefix) {
                        // Its drive is another volume's.
                        CNode::False
                    } else {
                        CNode::Path {
                            finder: memmem::Finder::new(needle),
                            case,
                        }
                    }
                }
                _ => CNode::Text {
                    target: *target,
                    matcher: Box::new(TextMatcher::new(needle, *anchors, *skip)),
                    case,
                },
            }
        }
        Leaf::Glob { target, glob, case } => {
            let case = *case;
            match target {
                Target::Name | Target::FullPath if glob.is_name() => CNode::NameGlob { glob, case },
                Target::Name | Target::FullPath => match glob.single_part() {
                    Some(part) if volume.parts_are_names => CNode::NamePart {
                        part,
                        lit: glob.longest_literal(),
                        case,
                    },
                    _ => {
                        let prefix = volume.prefix(case);
                        let lead = anchored_lead(glob);
                        if lead.as_deref().is_some_and(|lead| {
                            let n = lead.len().min(prefix.len());
                            lead[..n] != prefix[..n]
                        }) {
                            CNode::False
                        } else {
                            CNode::PathGlob {
                                glob,
                                lead,
                                trail: glob.trailing_literal(),
                                name_lit: glob.name_literal().filter(|_| volume.parts_are_names),
                                case,
                            }
                        }
                    }
                },
                target => CNode::TargetGlob {
                    target: *target,
                    glob,
                    case,
                },
            }
        }
        Leaf::Kind(kind) => CNode::Kind(*kind),
        Leaf::Ext { values, case } => CNode::Ext(Box::new(ExtMatcher::new(values, *case))),
        Leaf::Size(range) => CNode::Size(*range),
        Leaf::SizeUnknown => CNode::SizeUnknown,
        Leaf::Date(date) => CNode::Date(*date),
        Leaf::Len { of, range } => CNode::Len {
            of: *of,
            range: *range,
        },
        Leaf::Parent(paths) => match volume.folders(paths, false) {
            Ok(dirs) if !dirs.is_empty() => CNode::Parent(dirs),
            _ => CNode::False,
        },
        Leaf::Ancestor(paths) => match volume.folders(paths, true) {
            Err(()) => CNode::True,
            Ok(dirs) if dirs.contains(&volume.index.root()) => CNode::True,
            Ok(dirs) if !dirs.is_empty() => CNode::Ancestor(dirs),
            Ok(_) => CNode::False,
        },
        Leaf::Drive(letter) => {
            let p = volume.raw_prefix;
            if p.get(1) == Some(&b':') && p[0].to_ascii_lowercase() == *letter {
                CNode::True
            } else {
                CNode::False
            }
        }
    }
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

/// What the name of every entry `node` matches passes: `node` with each term
/// the name alone does not decide made `True`, but for `ext:`, whose files
/// have one of its extensions, so that a name with none of them has no match
/// whatever its entries are. A NOT keeps none of its terms, as the NOT of a
/// weaker test is no weaker. `None` when nothing is left: `audio:|video:`
/// gives the extensions of both lists, `size:>1mb` nothing.
pub(super) fn name_projection(node: CNode<'_>) -> Option<CNode<'_>> {
    match node.project() {
        CNode::True => None,
        other => Some(other),
    }
}

/// Where the candidates of a query with a top-level `parent:` or
/// `ancestor:` come from: those folders' children, or one folder's subtree,
/// a scope as `-path` gives one.
pub(super) enum Source {
    Children(Vec<EntryIdx>),
    Subtree(EntryIdx),
}

/// The candidate source the top-level terms of `node`, the expression's
/// terms on the entry, give: the folders of a `parent:` term, or the one
/// folder of an `ancestor:` term.
pub(super) fn source(node: &CNode<'_>) -> Option<Source> {
    let items = match node {
        CNode::And(items) => items.as_slice(),
        other => std::slice::from_ref(other),
    };
    let parent = items.iter().find_map(|i| match i {
        CNode::Parent(dirs) => Some(Source::Children(dirs.clone())),
        _ => None,
    });
    parent.or_else(|| {
        items.iter().find_map(|i| match i {
            CNode::Ancestor(dirs) if dirs.len() == 1 => Some(Source::Subtree(dirs[0])),
            _ => None,
        })
    })
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
        let class = match self {
            CNode::True | CNode::False => return (0, 0),
            CNode::Not(inner) => return inner.cost(),
            CNode::And(items) | CNode::Or(items) => {
                return items
                    .iter()
                    .map(CNode::cost)
                    .fold((0, 0), |(class, leaves), (c, l)| (class.max(c), leaves + l));
            }
            CNode::Kind(_) => 1,
            CNode::Size(_) | CNode::SizeUnknown | CNode::Date(_) | CNode::Parent(_) => 2,
            CNode::Name { .. } => 3,
            CNode::Len {
                of: LenOf::Name | LenOf::Extension,
                ..
            } => 3,
            CNode::NameGlob { .. } | CNode::NamePart { .. } => 4,
            CNode::Text { target, .. } | CNode::TargetGlob { target, .. }
                if !target_needs_path(*target) =>
            {
                4
            }
            CNode::Ext(_) | CNode::Ancestor(_) => 5,
            CNode::Path { .. }
            | CNode::Text { .. }
            | CNode::TargetGlob { .. }
            | CNode::Len { .. } => 6,
            CNode::PathGlob { .. } => 7,
        };
        (class, 1)
    }

    /// See [`name_projection`].
    fn project(self) -> CNode<'a> {
        if self.decided_by_name() {
            return self;
        }
        match self {
            // At the name, with no entry, a file's extension.
            CNode::Ext(_) => self,
            CNode::And(items) => {
                CNode::And(items.into_iter().map(CNode::project).collect()).folded()
            }
            CNode::Or(items) => CNode::Or(items.into_iter().map(CNode::project).collect()).folded(),
            _ => CNode::True,
        }
    }

    /// Whether every leaf is decided by the name alone.
    fn decided_by_name(&self) -> bool {
        match self {
            CNode::True
            | CNode::False
            | CNode::Name { .. }
            | CNode::NameGlob { .. }
            | CNode::NamePart { .. }
            | CNode::Len {
                of: LenOf::Name, ..
            } => true,
            CNode::Text { target, .. } => target.by_name(),
            CNode::Not(inner) => inner.decided_by_name(),
            CNode::And(items) | CNode::Or(items) => items.iter().all(CNode::decided_by_name),
            _ => false,
        }
    }

    /// The needles of [`plan`] for this node: a name search's own needle, a
    /// name wildcard's longest literal, `.ext` for each of a few extensions;
    /// the best of an AND's children; all of an OR's, when each has some.
    /// What a NOT, a kind, size or date term, a path term or a term that
    /// matches case matches holds no needle its folded name must hold.
    fn needles(&self) -> Option<Vec<Vec<u8>>> {
        match self {
            CNode::Name {
                finder,
                case: false,
            } => Some(vec![finder.needle().to_vec()]),
            // The stem and the extension are parts of the name.
            CNode::Text {
                target: Target::Name | Target::Stem | Target::Extension,
                matcher,
                case: false,
            } if !matcher.skips() && !matcher.needle().is_empty() => {
                Some(vec![matcher.needle().to_vec()])
            }
            CNode::NameGlob { glob, case: false }
            | CNode::TargetGlob {
                target: Target::Stem | Target::Extension,
                glob,
                case: false,
            } => glob.longest_literal().map(|lit| vec![lit]),
            CNode::NamePart {
                lit, case: false, ..
            }
            | CNode::PathGlob {
                name_lit: lit,
                case: false,
                ..
            } => lit.clone().map(|lit| vec![lit]),
            CNode::Ext(ext) => ext.needles(),
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
            CNode::Path { .. } | CNode::PathGlob { .. } => true,
            CNode::Text { target, .. } | CNode::TargetGlob { target, .. } => {
                target_needs_path(*target)
            }
            CNode::Len { of, .. } => matches!(of, LenOf::FullPath | LenOf::Location),
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
            CNode::Name { finder, case } => finder.find(s.name(*case)).is_some(),
            CNode::Text {
                target,
                matcher,
                case,
            } => {
                let budget = s.budget;
                s.hay(*target, *case).is_some_and(|(hay, cps)| {
                    !long_and_spent(hay, budget) && matcher.matches(hay, cps, budget)
                })
            }
            CNode::NameGlob { glob, case } => {
                let budget = s.budget;
                glob.matches_decoded(s.name_code_points(*case), budget)
            }
            CNode::NamePart { part, case, .. } => {
                let budget = s.budget;
                let name = s.name_code_points(*case);
                let last = name
                    .iter()
                    .rposition(|&c| c == u32::from(b'\\'))
                    .map_or(name, |sep| &name[sep + 1..]);
                part.matches_decoded(last, budget)
            }
            CNode::TargetGlob { target, glob, case } => {
                let budget = s.budget;
                s.hay(*target, *case).is_some_and(|(hay, cps)| {
                    !long_and_spent(hay, budget) && {
                        decode_into(hay, cps);
                        glob.matches_decoded(cps, budget)
                    }
                })
            }
            CNode::Ext(ext) => {
                !s.is_dir() && {
                    let budget = s.budget;
                    let raw = s.name(true);
                    let scratch = &mut *s.scratch;
                    ext.matches(raw, &mut scratch.ext, &mut scratch.text_cps, budget)
                }
            }
            CNode::Kind(kind) => match kind {
                Kind::File => !s.is_dir(),
                Kind::Folder => s.is_dir(),
            },
            CNode::Size(range) => size_matches(*range, s.size()),
            // A folder matches no `size:` term, `unknown` included: goz
            // does not sum folder sizes, and in Everything, which does, a
            // folder's size is known (L7:446).
            CNode::SizeUnknown => s.entry.is_some() && !s.is_dir() && s.size().is_none(),
            CNode::Date(date) => date.matches(s.mtime()),
            CNode::Len { of, range } => {
                let len = match of {
                    LenOf::Name => Some(utf16_len(s.name(true))),
                    // A folder has no extension, and so no length of one
                    // (`extlen:0` keeps files with no dot, L7:432).
                    LenOf::Extension if s.is_dir() => None,
                    LenOf::Extension => Some(
                        s.hay(Target::Extension, true)
                            .map_or(0, |(ext, _)| utf16_len(ext)),
                    ),
                    LenOf::FullPath => s.hay(Target::FullPath, true).map(|(p, _)| utf16_len(p)),
                    LenOf::Location => s.hay(Target::Location, true).map(|(p, _)| utf16_len(p)),
                };
                len.is_some_and(|len| range.contains(len))
            }
            CNode::Parent(dirs) => s
                .entry
                .is_some_and(|e| dirs.binary_search(&s.index.entry(e).parent()).is_ok()),
            CNode::Ancestor(dirs) => s.entry.is_some_and(|e| {
                let mut cur = s.index.entry(e).parent();
                for _ in 0..MAX_PATH_HOPS {
                    if cur == NIL {
                        return false;
                    }
                    if dirs.binary_search(&cur).is_ok() {
                        return true;
                    }
                    cur = s.index.entry(cur).parent();
                }
                false
            }),
            CNode::Path { finder, case } => {
                let budget = s.budget;
                s.hay(Target::FullPath, *case)
                    .is_some_and(|(p, _)| !long_and_spent(p, budget) && finder.find(p).is_some())
            }
            CNode::PathGlob {
                glob,
                lead,
                trail,
                case,
                ..
            } => {
                let budget = s.budget;
                let fits = s.hay(Target::FullPath, *case).is_some_and(|(p, _)| {
                    lead.as_deref().is_none_or(|lead| p.starts_with(lead))
                        && p.ends_with(trail)
                        && !long_and_spent(p, budget)
                });
                fits && s
                    .path_code_points(*case)
                    .is_some_and(|cps| glob.matches_decoded(cps, budget))
            }
        }
    }
}

/// Whether a term on `target` needs the full path.
fn target_needs_path(target: Target) -> bool {
    matches!(
        target,
        Target::FullPath | Target::Location | Target::ParentName
    )
}

/// Whether `path` is longer than any real one and the budget is spent: a
/// search or decode of a path of megabytes checks the budget first, as each
/// term's search of such a path does in `decide_needs`.
fn long_and_spent(path: &[u8], budget: &Budget) -> bool {
    path.len() > CACHED_PATH_MAX && budget.check()
}

/// The values of an `ext:` term or filter, compiled, as Everything matches
/// them: a plain value of `k` dot-separated parts names the name's last `k`
/// parts after a dot (`tar.gz` matches `c.tar.gz`, `a.txt` does not match
/// `a.txt`, L7:531); a wildcard value matches the extension alone, the text
/// after the last dot or nothing when there is no dot (`tar.*` matches no
/// name, `*` every file, L7:528-530); an empty value names a name with no
/// dot.
pub(super) struct ExtMatcher {
    case: bool,
    /// The plain values, folded unless `case`.
    plain: Plain,
    /// How many dot-separated parts the plain values have, fewest first.
    parts: Vec<usize>,
    /// The values with a wildcard.
    globs: Vec<Wildcard>,
    /// An empty value: a name with no dot.
    none: bool,
}

enum Plain {
    None,
    /// One value: a compare.
    One(Vec<u8>),
    /// A list, such as a macro's: one lookup however long.
    Set(FxHashSet<Vec<u8>>),
}

impl ExtMatcher {
    pub(super) fn new(values: &[Vec<u8>], case: bool) -> Self {
        let mut plain: Vec<Vec<u8>> = Vec::new();
        let mut parts = Vec::new();
        let mut globs = Vec::new();
        let mut none = false;
        for v in values {
            if v.is_empty() {
                none = true;
            } else if v.iter().any(|&b| b == b'*' || b == b'?') {
                globs.push(Wildcard::name(v));
            } else {
                plain.push(v.clone());
                parts.push(1 + v.iter().filter(|&&b| b == b'.').count());
            }
        }
        parts.sort_unstable();
        parts.dedup();
        let plain = match plain.len() {
            0 => Plain::None,
            1 => Plain::One(plain.pop().unwrap_or_default()),
            _ => Plain::Set(plain.into_iter().collect()),
        };
        ExtMatcher {
            case,
            plain,
            parts,
            globs,
            none,
        }
    }

    /// Whether the name `raw` (as stored) has one of the extensions; the
    /// caller checks that its entry is a file. `fold_buf` and `cps` are
    /// scratch.
    pub(super) fn matches(
        &self,
        raw: &[u8],
        fold_buf: &mut Vec<u8>,
        cps: &mut CodePointBuf,
        budget: &Budget,
    ) -> bool {
        if memchr::memchr(b'.', raw).is_none() {
            return self.none || self.extension_globbed(b"", cps, budget);
        }
        for &k in &self.parts {
            let Some(tail) = self.tail(raw, k, fold_buf) else {
                break;
            };
            let hit = match &self.plain {
                Plain::None => false,
                Plain::One(v) => v.as_slice() == tail,
                Plain::Set(set) => set.contains(tail),
            };
            if hit {
                return true;
            }
        }
        !self.globs.is_empty()
            && self
                .tail(raw, 1, fold_buf)
                .is_some_and(|ext| self.extension_globbed(ext, cps, budget))
    }

    /// Whether extension `ext` (folded as the values are) matches a
    /// wildcard value.
    fn extension_globbed(&self, ext: &[u8], cps: &mut CodePointBuf, budget: &Budget) -> bool {
        if self.globs.is_empty() {
            return false;
        }
        decode_into(ext, cps);
        self.globs
            .iter()
            .any(|glob| glob.matches_decoded(cps, budget))
    }

    /// The last `k` dot-separated parts of `raw`, folded into `fold_buf`
    /// unless the values match case, or `None` when it has fewer dots.
    fn tail<'b>(&self, raw: &'b [u8], k: usize, fold_buf: &'b mut Vec<u8>) -> Option<&'b [u8]> {
        let dot = memchr::memrchr_iter(b'.', raw).nth(k - 1)?;
        let tail = &raw[dot + 1..];
        Some(if self.case {
            tail
        } else {
            fold_buf.clear();
            crate::fold::fold_into(tail, fold_buf);
            fold_buf.as_slice()
        })
    }

    /// `.ext` for each of a few plain values, one of which the folded name
    /// of every match holds: none for a wildcard or an empty value, or for
    /// values matching case, which the folded names cannot be scanned for.
    fn needles(&self) -> Option<Vec<Vec<u8>>> {
        if self.case || self.none || !self.globs.is_empty() {
            return None;
        }
        let dotted = |v: &Vec<u8>| [&b"."[..], v].concat();
        match &self.plain {
            Plain::None => None,
            Plain::One(v) => Some(vec![dotted(v)]),
            Plain::Set(set) if set.len() <= UNION_MAX => {
                let mut out: Vec<Vec<u8>> = set.iter().map(dotted).collect();
                out.sort();
                Some(out)
            }
            Plain::Set(_) => None,
        }
    }
}

/// Scratch buffers a search reuses from one candidate to the next, so that
/// testing one allocates nothing.
#[derive(Default)]
pub(super) struct Scratch {
    /// A name decoded for a wildcard.
    pub(super) cps: CodePointBuf,
    /// An extension, folded.
    pub(super) ext: Vec<u8>,
    /// A full path, folded as the query folds its paths, for the
    /// expression.
    pub(super) path: Vec<u8>,
    /// That path decoded.
    pub(super) path_cps: CodePointBuf,
    /// A full path folded the other way, for a term that matches case in a
    /// query that does not, or the reverse.
    pub(super) other_path: Vec<u8>,
    /// That path decoded.
    pub(super) other_cps: CodePointBuf,
    /// A target decoded, for a match that skips or a wildcard.
    pub(super) text_cps: CodePointBuf,
}

/// Fills its buffer with an entry's full path (as the query compares it)
/// and says whether it has one.
pub(super) type PathSource<'x> = &'x mut dyn FnMut(&mut Vec<u8>) -> bool;

/// What an expression is evaluated against: a distinct name, or an entry,
/// whose columns and full path are read only when a leaf asks.
pub(super) struct Subject<'s, 'x> {
    index: &'s VolumeIndex,
    id: NameId,
    /// Whether the query matches case: how the full path `full_path` gives
    /// is folded.
    query_case: bool,
    /// The mount prefix as typed, for a full path built unfolded.
    raw_prefix: &'s [u8],
    /// `None` at the name level, where no leaf reads the entry.
    entry: Option<EntryIdx>,
    budget: &'s Budget<'s>,
    scratch: &'x mut Scratch,
    full_path: Option<PathSource<'x>>,
    /// Which way `scratch.cps` holds the name decoded, if it does.
    name_decoded: Option<bool>,
    path: Fetched,
    other: Fetched,
}

/// How far one of the subject's full paths has been read.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fetched {
    Unread,
    /// The entry has no full path.
    Missing,
    Read,
    Decoded,
}

/// Where a target's text lies.
enum Src<'s> {
    Name(&'s [u8]),
    Path,
    Other,
}

impl<'s, 'x> Subject<'s, 'x> {
    /// Name `id`, at the name level.
    pub(super) fn name_level(
        index: &'s VolumeIndex,
        id: NameId,
        query_case: bool,
        budget: &'s Budget<'s>,
        scratch: &'x mut Scratch,
    ) -> Self {
        Subject {
            index,
            id,
            query_case,
            raw_prefix: &[],
            entry: None,
            budget,
            scratch,
            full_path: None,
            name_decoded: None,
            path: Fetched::Unread,
            other: Fetched::Unread,
        }
    }

    /// Entry `idx` on the volume mounted at `raw_prefix`, whose full path,
    /// folded as the query folds paths, `full_path` gives.
    pub(super) fn entry(
        index: &'s VolumeIndex,
        idx: EntryIdx,
        query_case: bool,
        raw_prefix: &'s [u8],
        budget: &'s Budget<'s>,
        scratch: &'x mut Scratch,
        full_path: PathSource<'x>,
    ) -> Self {
        let mut s = Subject::name_level(index, index.name_id(idx), query_case, budget, scratch);
        s.raw_prefix = raw_prefix;
        s.entry = Some(idx);
        s.full_path = Some(full_path);
        s
    }

    /// The name, as stored (`case`) or folded.
    fn name(&self, case: bool) -> &'s [u8] {
        if case {
            self.index.name_raw(self.id)
        } else {
            self.index.name_folded(self.id)
        }
    }

    fn name_code_points(&mut self, case: bool) -> &[u32] {
        if self.name_decoded != Some(case) {
            decode_into(self.name(case), &mut self.scratch.cps);
            self.name_decoded = Some(case);
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

    /// Reads the full path, folded as the query folds paths (`case` is the
    /// query's) or the other way, and says whether the entry has one.
    fn fetch_path(&mut self, case: bool) -> bool {
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
        if self.path == Fetched::Missing {
            return false;
        }
        if case == self.query_case {
            return true;
        }
        if self.other == Fetched::Unread {
            let other = &mut self.scratch.other_path;
            other.clear();
            let found = if self.query_case {
                // The query's paths are as stored: fold this one.
                crate::fold::fold_into(&self.scratch.path, other);
                true
            } else {
                // The query's paths are folded: build this one as stored.
                // An entry has a path exactly when `path_of` gives one, so
                // it has one here too.
                let mut below = Vec::new();
                let ok = self.entry.is_some_and(|e| {
                    matches!(
                        self.index.path_of(e, &mut below),
                        crate::index::PathStatus::Ok
                    )
                });
                other.extend_from_slice(self.raw_prefix);
                other.extend_from_slice(&below);
                ok
            };
            self.other = if found {
                Fetched::Read
            } else {
                Fetched::Missing
            };
        }
        self.other != Fetched::Missing
    }

    /// The text of `target`, folded unless `case`, and a buffer to decode it
    /// into: `None` when the entry has no such text (a folder's extension,
    /// or no full path).
    fn hay(&mut self, target: Target, case: bool) -> Option<(&[u8], &mut CodePointBuf)> {
        let (src, range) = match target {
            Target::Name | Target::Stem | Target::Extension => {
                let name = self.name(case);
                let dot = memchr::memrchr(b'.', name);
                let range = match target {
                    Target::Name => 0..name.len(),
                    // A folder's stem is its name, and it has no extension.
                    Target::Stem if self.is_dir() => 0..name.len(),
                    Target::Stem => 0..dot.unwrap_or(name.len()),
                    _ if self.is_dir() || self.entry.is_none() => return None,
                    _ => (dot? + 1)..name.len(),
                };
                (Src::Name(name), range)
            }
            Target::FullPath | Target::Location | Target::ParentName => {
                if !self.fetch_path(case) {
                    return None;
                }
                let path = if case == self.query_case {
                    &self.scratch.path
                } else {
                    &self.scratch.other_path
                };
                let range = match target {
                    Target::FullPath => 0..path.len(),
                    _ => {
                        let location = memchr::memrchr(b'\\', path).unwrap_or(0);
                        match target {
                            Target::Location => 0..location,
                            _ => {
                                let start = memchr::memrchr(b'\\', &path[..location])
                                    .map_or(0, |sep| sep + 1);
                                start..location
                            }
                        }
                    }
                };
                let src = if case == self.query_case {
                    Src::Path
                } else {
                    Src::Other
                };
                (src, range)
            }
        };
        let scratch = &mut *self.scratch;
        let bytes: &[u8] = match src {
            Src::Name(name) => &name[range],
            Src::Path => &scratch.path[range],
            Src::Other => &scratch.other_path[range],
        };
        Some((bytes, &mut scratch.text_cps))
    }

    fn path_code_points(&mut self, case: bool) -> Option<&[u32]> {
        if !self.fetch_path(case) {
            return None;
        }
        let scratch = &mut *self.scratch;
        let (state, path, cps) = if case == self.query_case {
            (&mut self.path, &scratch.path, &mut scratch.path_cps)
        } else {
            (&mut self.other, &scratch.other_path, &mut scratch.other_cps)
        };
        if *state == Fetched::Read {
            decode_into(path, cps);
            *state = Fetched::Decoded;
        }
        Some(cps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::parse::parse_query;

    /// The compiled expression of `query` on the volume at `prefix`.
    fn compiled(query: &str, prefix: &str, f: impl FnOnce(&CNode)) {
        let parsed = parse_query(query).unwrap();
        let index = VolumeIndex::new(crate::index::NTFS_ROOT_FRN, crate::index::FrnMap::sparse());
        let folded = crate::fold::fold(prefix.as_bytes());
        let volume = Volume {
            index: &index,
            raw_prefix: prefix.as_bytes(),
            folded_prefix: &folded,
            parts_are_names: true,
        };
        let e = parsed.expr.as_ref().expect("an expression");
        f(&compile(e, &volume));
    }

    /// A path term the mount prefix decides becomes a constant, and the
    /// tree folds around it.
    #[test]
    fn the_mount_prefix_folds_path_terms() {
        compiled(r"c:\|report", r"C:\", |n| assert!(matches!(n, CNode::True)));
        compiled(r"c:\|report", r"D:\", |n| {
            assert!(matches!(n, CNode::Name { .. }))
        });
        compiled(r"!c:\", r"C:\", |n| assert!(matches!(n, CNode::False)));
        compiled(r"!c:\", r"D:\", |n| assert!(matches!(n, CNode::True)));
        compiled(r"a|c:\x\*.txt", r"D:\", |n| {
            assert!(matches!(n, CNode::Name { .. }))
        });
        compiled(r"a|c:\x\*.txt", r"C:\", |n| {
            assert!(matches!(n, CNode::Or(items) if items.len() == 2));
        });
        compiled(r"a|\data\", r"C:\mnt\data\", |n| {
            assert!(matches!(n, CNode::True))
        });
        compiled("c:|report", r"C:\", |n| assert!(matches!(n, CNode::True)));
        compiled("C:|report", r"c:\mnt\x\", |n| {
            assert!(matches!(n, CNode::True))
        });
        compiled("!d:", r"D:\", |n| assert!(matches!(n, CNode::False)));
        compiled("d:|report", r"C:\", |n| {
            assert!(matches!(n, CNode::Name { .. }))
        });
        compiled(r"ancestor:C:\|report", r"C:\", |n| {
            assert!(matches!(n, CNode::True))
        });
        compiled(r"ancestor:C:\mnt|report", r"C:\mnt\x\", |n| {
            assert!(matches!(n, CNode::True))
        });
        compiled(r"parent:C:\mnt|report", r"C:\mnt\x\", |n| {
            assert!(matches!(n, CNode::Name { .. }))
        });
        compiled(r"parent:D:\x|report", r"C:\", |n| {
            assert!(matches!(n, CNode::Name { .. }))
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
        let parsed = parse_query("<a|b> <c|file:> <d|x\\y> <ww:e|len:3>").unwrap();
        let index = VolumeIndex::new(crate::index::NTFS_ROOT_FRN, crate::index::FrnMap::sparse());
        let volume = Volume {
            index: &index,
            raw_prefix: br"C:\",
            folded_prefix: br"c:\",
            parts_are_names: true,
        };
        let node = compile(parsed.expr.as_ref().unwrap(), &volume);
        let (name, entry) = split(node);
        let name = name.expect("name terms");
        assert!(!name.tests_paths());
        let CNode::And(items) = &name else { panic!() };
        assert_eq!(items.len(), 2, "<a|b> and <ww:e|len:3>");
        let entry = entry.expect("entry terms");
        assert!(entry.tests_paths());
        let CNode::And(items) = entry else { panic!() };
        assert_eq!(items.len(), 2);
    }

    /// The needles of `query`'s expression on C:, sorted.
    fn needles_of(query: &str) -> Option<Vec<String>> {
        let parsed = parse_query(query).unwrap();
        let index = VolumeIndex::new(crate::index::NTFS_ROOT_FRN, crate::index::FrnMap::sparse());
        let volume = Volume {
            index: &index,
            raw_prefix: br"C:\",
            folded_prefix: br"c:\",
            parts_are_names: true,
        };
        let node = compile(parsed.expr.as_ref().expect("an expression"), &volume);
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
        // Modified terms on the name, the stem and the extension hold
        // their needle; one matching case, skipping, or on a path, not.
        assert_eq!(needles_of("ww:ab|prefix:cd"), s(&["ab", "cd"]));
        assert_eq!(needles_of("stem:ab|extension:cd"), s(&["ab", "cd"]));
        assert_eq!(needles_of("whole:Ab|x"), s(&["ab", "x"]));
        assert_eq!(needles_of("case:Ab|x"), None);
        assert_eq!(needles_of("nopunc:ab|x"), None);
        assert_eq!(needles_of("location:ab|x"), None);
        assert_eq!(
            needles_of("ext:a;;b|x"),
            None,
            "no extension holds no needle"
        );
        assert_eq!(needles_of("audio:|x"), None, "too many extensions");
    }

    fn ext(values: &[&str], case: bool, name: &str) -> bool {
        let values: Vec<Vec<u8>> = values
            .iter()
            .map(|v| {
                if case {
                    v.as_bytes().to_vec()
                } else {
                    crate::fold::fold(v.as_bytes())
                }
            })
            .collect();
        ExtMatcher::new(&values, case).matches(
            name.as_bytes(),
            &mut Vec::new(),
            &mut CodePointBuf::new(),
            &Budget::unlimited(),
        )
    }

    /// Everything's answers for its fixture's `ext` folder (L1:240–263,
    /// L2:102–107).
    #[test]
    fn extensions_match_as_everything_matches_them() {
        assert!(ext(&["txt"], false, "b.TXT") && ext(&["txt"], false, "q.Txt"));
        assert!(!ext(&["txt"], false, "dir.txt.bak"));
        assert!(ext(&["gz"], false, "c.tar.gz"));
        assert!(
            ext(&["tar.gz"], false, "c.tar.gz"),
            "a dotted value is the last parts"
        );
        assert!(!ext(&["ar.gz"], false, "c.tar.gz"), "whole parts");
        assert!(!ext(&["tar"], false, "c.tar.gz"));
        assert!(ext(&["dots"], false, "two..dots") && !ext(&["dot"], false, "two..dots"));
        assert!(ext(&["hidden"], false, ".hidden"));
        assert!(ext(&[""], false, "noext") && !ext(&[""], false, "a.txt"));
        assert!(!ext(&["txt"], false, "noext"));
        assert!(ext(&["mp*"], false, "m.mpeg") && ext(&["mp*"], false, "x.mp3"));
        assert!(ext(&["mp?"], false, "y.mp4") && !ext(&["mp?"], false, "m.mpeg"));
        assert!(ext(&["txt", "exe"], false, "e.exe"));
        let many: Vec<&str> = "aac;ac3;mp3;flac".split(';').collect();
        assert!(ext(&many, false, "x.MP3") && !ext(&many, false, "x.mp4"));
        assert!(ext(&["TXT"], true, "b.TXT") && !ext(&["TXT"], true, "a.txt"));
        // A wildcard value matches the extension alone, which holds no dot,
        // or nothing when the name has none (L7:528-530, L8:111-115).
        assert!(!ext(&["tar.*"], false, "c.tar.gz"));
        assert!(ext(&["*"], false, "noext") && ext(&["*"], false, "a.txt"));
        assert!(ext(&["t*"], false, "a.TXT") && !ext(&["t*"], false, "c.tar.gz"));
        assert!(ext(&["??"], false, "c.tar.gz") && !ext(&["??"], false, "a.txt"));
        assert!(!ext(&["??"], false, "noext") && ext(&["", "??"], false, "noext"));
        assert!(
            !ext(&["a.txt"], false, "a.txt"),
            "a dotted value follows a dot"
        );
    }
}
