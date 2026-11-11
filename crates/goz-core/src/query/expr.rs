//! A query's boolean expression, and how it is lowered onto the buckets a
//! plain AND query has always filled.
//!
//! The parser builds an [`Expr`] for the whole query. Most queries are an AND
//! of terms the buckets of [`ParsedQuery`] hold exactly (name and path
//! substrings, wildcards, the `ext:`, `size:`, `dm:` and kind filters), and
//! for those the lowering leaves no expression at all: the query is the one
//! goz has always built, and runs the same prefilters and fast paths. What a
//! bucket cannot hold (an OR, a NOT, a group, a term with a modifier, a
//! second `ext:` or `size:`) stays an expression, ANDed with the buckets, for
//! the engine to evaluate.

use super::dates::DateFilter;
use super::glob::Wildcard;
use super::parse::{Kind, ParsedQuery, SizeRange};
use super::pattern::RegexTerm;

/// A boolean expression over the query's terms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expr {
    /// Matches everything: an empty term, `foo|` or `<>`.
    True,
    /// Matches nothing: a NOT of an empty term, `!`.
    False,
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    Leaf(Leaf),
}

/// What a text term is matched against: the parts of an entry's full path
/// Everything names (DESIGN.md §2.4 of the query-language work).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// The name.
    Name,
    /// The full path, mount prefix included.
    FullPath,
    /// A file's name without its extension; a folder's whole name.
    Stem,
    /// A file's extension, the text after its name's last dot; a folder,
    /// or a file whose name holds no dot, has none.
    Extension,
    /// The full path of the folder holding the entry, with no separator at
    /// its end (`C:\Windows` for `C:\Windows\notepad.exe`).
    Location,
    /// The name of the folder holding the entry: the last part of its
    /// location.
    ParentName,
}

impl Target {
    /// Whether the name alone gives the target, so a term on it can be
    /// decided in the pass over the distinct names.
    pub fn by_name(self) -> bool {
        self == Target::Name
    }
}

/// Where a text term's match must stand in its target: `start` and `end`
/// at the target's ends (`startwith:`, `endwith:`, both for `whole:`),
/// `word_start` and `word_end` at word boundaries (`prefix:`, `suffix:`,
/// both for `ww:`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Anchors {
    pub start: bool,
    pub end: bool,
    pub word_start: bool,
    pub word_end: bool,
}

impl Anchors {
    /// Whether the match may stand anywhere.
    pub fn is_free(self) -> bool {
        self == Anchors::default()
    }
}

/// The characters of the target a match may skip between the needle's
/// (`ignore-punctuation:`, `ignore-whitespace:`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Skip {
    pub punct: bool,
    pub space: bool,
}

impl Skip {
    pub fn is_none(self) -> bool {
        self == Skip::default()
    }
}

/// Which length a `len:` term compares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LenOf {
    Name,
    FullPath,
    Location,
    Extension,
}

/// What a count function counts, from the links the index keeps: the
/// parent chain, or a folder's children.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CountOf {
    /// `parent-count:` (`parents:`, `depth:`): the folders above the entry,
    /// the volume's root included, so an entry of the root counts 1.
    Parents,
    /// `child-count:`: a folder's files and folders. `empty:` is 0.
    Children,
    /// `child-file-count:`: a folder's files.
    ChildFiles,
    /// `child-folder-count:`: a folder's folders.
    ChildFolders,
    /// `descendant-count:`: the files and folders below a folder, at any
    /// depth.
    Descendants,
    /// `descendant-file-count:`.
    DescendantFiles,
    /// `descendant-folder-count:`.
    DescendantFolders,
    /// `sibling-count:`: the other files and folders of the entry's folder.
    Siblings,
    /// `sibling-file-count:`.
    SiblingFiles,
    /// `sibling-folder-count:`.
    SiblingFolders,
}

impl CountOf {
    /// Whether the count is worked out for the whole volume before the
    /// search (see the `related` module), rather than entry by entry.
    pub fn is_volume_wide(self) -> bool {
        matches!(
            self,
            CountOf::Descendants
                | CountOf::DescendantFiles
                | CountOf::DescendantFolders
                | CountOf::Siblings
                | CountOf::SiblingFiles
                | CountOf::SiblingFolders
        )
    }
}

/// How the entries a sub-search names relate to the entry tested: the
/// functions that search other entries' names (`child:`, `descendant:`,
/// `sibling:`, `ancestor-name:`, `parent-child:`, `parent-sibling:`,
/// `ancestor-child:`, `ancestor-sibling:`), each answered from the parent
/// and child links the index keeps, and `child:` also for a child's size or
/// date (`child-file-size:`, `child-date-modified:`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Relation {
    /// The entry is a folder that holds one directly.
    Child,
    /// The entry is a folder that holds one at any depth.
    Descendant,
    /// Another entry of the entry's folder.
    Sibling,
    /// A folder above the entry, the volume's root included, whose name is
    /// then the mount prefix (`C:`), as Everything names a root by its path.
    Ancestor,
    /// An entry of the entry's folder, the entry itself included.
    ParentChild,
    /// An entry of the folder above the entry's folder, that folder itself
    /// included: a live Everything 1.5 answers `parent-sibling:src` with the
    /// entries of every folder named `src` too (W8), though its help says
    /// the parent is left out.
    ParentSibling,
    /// An entry of a folder above the entry, the volume's root included.
    AncestorChild,
    /// An entry of a folder above the entry's folder, the folders on the
    /// entry's path included (W8, as `ParentSibling`).
    AncestorSibling,
}

/// One term. Text is folded unless the term matches case: with
/// [`crate::fold::fold`], case and accents ignored, or with
/// [`crate::fold::fold_exact`] under `diacritics:`, which keeps accents
/// (`diacritics` is never set with `case`, which keeps them too).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Leaf {
    /// The target holds `needle` (folded unless `case`), where `anchors`
    /// say, skipping in the target what `skip` says. An empty needle
    /// matches a target that exists and is not empty.
    Text {
        target: Target,
        needle: Vec<u8>,
        case: bool,
        diacritics: bool,
        anchors: Anchors,
        skip: Skip,
    },
    /// The target matches a wildcard (folded unless `case`): the name or a
    /// sub-path of the full path, as the wildcard knows, or the whole of
    /// another target.
    Glob {
        target: Target,
        glob: Wildcard,
        case: bool,
        diacritics: bool,
    },
    /// The target, as stored, holds a match of a regular expression.
    Regex { target: Target, regex: RegexTerm },
    /// Files or folders only.
    Kind(Kind),
    /// A file whose extension is one of `values` (folded unless `case`;
    /// never empty): a value of `k` dot-separated parts names the name's
    /// last `k`, a value may hold wildcards, and an empty value names a
    /// name with no dot. Folders have no extension.
    Ext {
        values: Vec<Vec<u8>>,
        case: bool,
        diacritics: bool,
    },
    /// The size is known and in range.
    Size(SizeRange),
    /// The size is unknown: a folder's (goz does not sum them), or a file's
    /// not read yet.
    SizeUnknown,
    /// The date modified passes.
    Date(DateFilter),
    /// A length, in UTF-16 units, is in range.
    Len { of: LenOf, range: SizeRange },
    /// A count is in range. Only a folder has children, so a file matches
    /// no child count, as in Everything, nor any descendant count.
    Count { of: CountOf, range: SizeRange },
    /// A folder whose files' sizes, summed, are in range
    /// (`total-child-size:`); a file of unknown size leaves the total
    /// unknown.
    TotalChildSize(SizeRange),
    /// The entry's folder is one of these.
    Parent(Vec<FolderPath>),
    /// One of the entry's folders, up to the root, is one of these.
    Ancestor(Vec<FolderPath>),
    /// The entry is on this drive (an ASCII letter, lower-cased): `d:`.
    Drive(u8),
    /// An entry related to this one as `relation` says, and of `kind` when
    /// set (`child-file:`), matches `term`: a term on the name (text, a
    /// wildcard or a regular expression, with its modifiers), or an OR of
    /// them, or `True` for an empty value, which any name matches; or, for
    /// `Child` alone, a size or date test, or an OR of them
    /// (`child-file-size:`, `child-date-modified:`).
    Related {
        relation: Relation,
        kind: Option<Kind>,
        term: Box<Expr>,
    },
}

/// A folder `parent:` or `ancestor:` names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderPath {
    /// Its absolute full path, folded, `\` for every separator and none at
    /// the end.
    pub path: Vec<u8>,
    /// Under `diacritics:`, the same path with case folded but accents
    /// kept ([`crate::fold::fold_exact`]): each folder on the way must have
    /// its accents, so a sibling spelled with others is not named.
    pub accented: Option<Vec<u8>>,
}

impl FolderPath {
    /// A folder named ignoring accents.
    pub fn folded(path: Vec<u8>) -> FolderPath {
        FolderPath {
            path,
            accented: None,
        }
    }
}

impl Leaf {
    /// A name or full-path substring term with no modifier: what the
    /// buckets hold.
    pub fn plain(target: Target, needle: Vec<u8>, case: bool) -> Leaf {
        Leaf::Text {
            target,
            needle,
            case,
            diacritics: false,
            anchors: Anchors::default(),
            skip: Skip::default(),
        }
    }
}

impl Expr {
    /// The AND of `a` and `b`, flattened, so that a long run of terms makes a
    /// wide node rather than a deep one.
    pub(super) fn and(a: Expr, b: Expr) -> Expr {
        Expr::join(a, b, true)
    }

    /// The OR of `a` and `b`, flattened.
    pub(super) fn or(a: Expr, b: Expr) -> Expr {
        Expr::join(a, b, false)
    }

    /// Appends to `a`'s own list when `a` is of the same kind, so that a run
    /// of `n` terms costs `n` appends, not `n` copies of a growing list: the
    /// text arrives from the pipe, and a query of a million terms must reach
    /// the term cap's refusal in linear time.
    fn join(a: Expr, b: Expr, and: bool) -> Expr {
        let mut items = match a {
            Expr::And(inner) if and => inner,
            Expr::Or(inner) if !and => inner,
            other => vec![other],
        };
        match b {
            Expr::And(inner) if and => items.extend(inner),
            Expr::Or(inner) if !and => items.extend(inner),
            other => items.push(other),
        }
        if and {
            Expr::And(items)
        } else {
            Expr::Or(items)
        }
    }

    /// `self` negated.
    pub(super) fn not(self) -> Expr {
        Expr::Not(Box::new(self))
    }

    /// Visits every leaf, and the leaves of a sub-search's term after the
    /// sub-search's own.
    pub fn for_each_leaf<'a>(&'a self, f: &mut impl FnMut(&'a Leaf)) {
        match self {
            Expr::True | Expr::False => {}
            Expr::And(items) | Expr::Or(items) => items.iter().for_each(|e| e.for_each_leaf(f)),
            Expr::Not(inner) => inner.for_each_leaf(f),
            Expr::Leaf(leaf) => {
                f(leaf);
                if let Leaf::Related { term, .. } = leaf {
                    term.for_each_leaf(f);
                }
            }
        }
    }
}

/// `e` with the constants folded away and nested ANDs and ORs flattened: an
/// AND drops `True` and is `False` with a `False`, an OR the reverse, a
/// double NOT cancels, and a node of one child is that child.
pub(super) fn simplify(e: Expr) -> Expr {
    match e {
        Expr::And(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match simplify(item) {
                    Expr::True => {}
                    Expr::False => return Expr::False,
                    Expr::And(inner) => out.extend(inner),
                    other => out.push(other),
                }
            }
            match out.len() {
                0 => Expr::True,
                1 => out.pop().unwrap_or(Expr::True),
                _ => Expr::And(out),
            }
        }
        Expr::Or(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match simplify(item) {
                    Expr::False => {}
                    Expr::True => return Expr::True,
                    Expr::Or(inner) => out.extend(inner),
                    other => out.push(other),
                }
            }
            match out.len() {
                0 => Expr::False,
                1 => out.pop().unwrap_or(Expr::False),
                _ => Expr::Or(out),
            }
        }
        Expr::Not(inner) => match simplify(*inner) {
            Expr::True => Expr::False,
            Expr::False => Expr::True,
            Expr::Not(twice) => *twice,
            other => other.not(),
        },
        leaf => leaf,
    }
}

/// Moves each top-level conjunct of `e` (simplified) that a bucket of `q`
/// holds exactly into that bucket, in query order, and leaves the rest as
/// `q.expr`. The first `ext:` and the first `size:` fill their buckets and
/// any other is an expression term, so that all of them must match, as in
/// Everything; two different kinds match nothing; `dm:` terms all apply.
pub(super) fn lower(e: Expr, q: &mut ParsedQuery) {
    let conjuncts = match e {
        Expr::True => Vec::new(),
        Expr::And(items) => items,
        other => vec![other],
    };
    let mut rest = Vec::new();
    for conjunct in conjuncts {
        match conjunct {
            Expr::Leaf(Leaf::Text {
                target: target @ (Target::Name | Target::FullPath),
                needle,
                case,
                diacritics: false,
                anchors,
                skip,
            }) if case == q.match_case && anchors.is_free() && skip.is_none() => {
                if target == Target::Name {
                    q.name_terms.push(needle);
                } else {
                    q.path_terms.push(needle);
                }
            }
            Expr::Leaf(Leaf::Glob {
                target: Target::Name | Target::FullPath,
                glob,
                case,
                diacritics: false,
            }) if case == q.match_case => {
                if glob.is_name() {
                    q.wildcards.push(glob);
                } else {
                    q.path_wildcards.push(glob);
                }
            }
            Expr::Leaf(Leaf::Kind(kind)) => match q.filters.kind {
                None => q.filters.kind = Some(kind),
                Some(have) if have == kind => {}
                // A file is never a folder.
                Some(_) => rest.push(Expr::False),
            },
            Expr::Leaf(Leaf::Ext {
                values,
                case: false,
                diacritics: false,
            }) if q.filters.ext.is_none() && values.iter().all(|v| is_plain_ext(v)) => {
                q.filters.ext = Some(values);
            }
            Expr::Leaf(Leaf::Size(range)) if q.filters.size.is_none() => {
                q.filters.size = Some(range);
            }
            Expr::Leaf(Leaf::Date(date)) => {
                q.filters.date = Some(q.filters.date.map_or(date, |d| d.and(date)));
            }
            other => rest.push(other),
        }
    }
    q.expr = match rest.len() {
        0 => None,
        1 => rest.pop(),
        _ => Some(Expr::And(rest)),
    };
}

/// Whether `ext:` value `v` names one extension plainly: not empty, and no
/// wildcard.
pub(super) fn is_plain_ext(v: &[u8]) -> bool {
    !v.is_empty() && !v.iter().any(|&b| b == b'*' || b == b'?')
}

/// How many `regex:` terms `e` holds.
pub(super) fn regex_terms(e: &Expr) -> usize {
    let mut n = 0;
    e.for_each_leaf(&mut |leaf| n += usize::from(matches!(leaf, Leaf::Regex { .. })));
    n
}

/// How many terms on related entries worked out before the search `e`
/// holds: the sub-searches (`child:`, `parent-sibling:`, ...,
/// `child-file-size:`) and the descendant and sibling counts.
pub(super) fn related_terms(e: &Expr) -> usize {
    let mut n = 0;
    e.for_each_leaf(&mut |leaf| {
        n += usize::from(match leaf {
            Leaf::Related { .. } => true,
            // Any count at all needs no set (see `cexpr::compile`).
            Leaf::Count { of, range } => of.is_volume_wide() && !range.is_any(),
            _ => false,
        })
    });
    n
}

/// How many terms `e`'s leaves count for the term cap: one each, since each
/// costs a candidate one test. An `ext:` list's plain values are one lookup
/// however many (a set, see `cexpr::ExtMatcher`), and each of its wildcard
/// values one test more.
pub(super) fn leaf_terms(e: &Expr) -> usize {
    let mut n = 0;
    e.for_each_leaf(&mut |leaf| {
        n += match leaf {
            Leaf::Ext { values, .. } => {
                1 + values
                    .iter()
                    .filter(|v| v.iter().any(|&b| b == b'*' || b == b'?'))
                    .count()
            }
            _ => 1,
        }
    });
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> Expr {
        Expr::Leaf(Leaf::plain(Target::Name, s.as_bytes().to_vec(), false))
    }

    #[test]
    fn simplification_folds_constants_and_flattens() {
        let (a, b, c) = (name("a"), name("b"), name("c"));
        assert_eq!(
            simplify(Expr::And(vec![
                a.clone(),
                Expr::True,
                Expr::And(vec![b.clone(), c.clone()])
            ])),
            Expr::And(vec![a.clone(), b.clone(), c.clone()])
        );
        assert_eq!(
            simplify(Expr::And(vec![a.clone(), Expr::False])),
            Expr::False
        );
        assert_eq!(simplify(Expr::Or(vec![a.clone(), Expr::True])), Expr::True);
        assert_eq!(simplify(Expr::Or(vec![Expr::False, a.clone()])), a.clone());
        assert_eq!(simplify(a.clone().not().not()), a.clone());
        assert_eq!(simplify(Expr::True.not()), Expr::False);
        assert_eq!(simplify(Expr::And(vec![])), Expr::True);
        assert_eq!(simplify(Expr::Or(vec![])), Expr::False);
        assert_eq!(
            simplify(Expr::Or(vec![
                Expr::Or(vec![a.clone(), b.clone()]),
                c.clone()
            ])),
            Expr::Or(vec![a, b, c])
        );
    }

    #[test]
    fn joins_stay_flat() {
        let mut e = name("x");
        for i in 0..10_000 {
            e = Expr::and(e, name(&i.to_string()));
        }
        let Expr::And(items) = &e else { panic!() };
        assert_eq!(items.len(), 10_001);
        assert_eq!(leaf_terms(&e), 10_001);
    }

    /// A query of a million terms reaches the term cap's refusal in linear
    /// time: each AND or OR appends to the list it extends. Copying the list
    /// at each term took minutes here.
    #[test]
    fn a_huge_query_is_refused_in_linear_time() {
        use crate::query::parse::{QueryError, parse_query};
        let started = std::time::Instant::now();
        for (query, terms) in [
            ("a ".repeat(1_000_000), 1_000_000),
            ("a|".repeat(500_000) + "a", 500_001),
            ("<a b> ".repeat(200_000), 400_000),
        ] {
            assert_eq!(parse_query(&query), Err(QueryError::TooManyTerms(terms)));
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "took {:?}",
            started.elapsed()
        );
    }
}
