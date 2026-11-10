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

/// One term. Text is folded unless the term matches case.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Leaf {
    /// The target holds `needle` (folded unless `case`), where `anchors`
    /// say, skipping in the target what `skip` says. An empty needle
    /// matches a target that exists and is not empty.
    Text {
        target: Target,
        needle: Vec<u8>,
        case: bool,
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
    },
    /// Files or folders only.
    Kind(Kind),
    /// A file whose extension is one of `values` (folded unless `case`;
    /// never empty): a value of `k` dot-separated parts names the name's
    /// last `k`, a value may hold wildcards, and an empty value names a
    /// name with no dot. Folders have no extension.
    Ext { values: Vec<Vec<u8>>, case: bool },
    /// The size is known and in range.
    Size(SizeRange),
    /// The size is unknown: a folder's (goz does not sum them), or a file's
    /// not read yet.
    SizeUnknown,
    /// The date modified passes.
    Date(DateFilter),
    /// A length, in UTF-16 units, is in range.
    Len { of: LenOf, range: SizeRange },
    /// The entry's folder is one of these, given by absolute full path,
    /// folded, `\` for every separator and none at the end.
    Parent(Vec<Vec<u8>>),
    /// One of the entry's folders, up to the root, is one of these.
    Ancestor(Vec<Vec<u8>>),
    /// The entry is on this drive (an ASCII letter, lower-cased): `d:`.
    Drive(u8),
}

impl Leaf {
    /// A name or full-path substring term with no modifier: what the
    /// buckets hold.
    pub fn plain(target: Target, needle: Vec<u8>, case: bool) -> Leaf {
        Leaf::Text {
            target,
            needle,
            case,
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

    /// Visits every leaf.
    pub fn for_each_leaf<'a>(&'a self, f: &mut impl FnMut(&'a Leaf)) {
        match self {
            Expr::True | Expr::False => {}
            Expr::And(items) | Expr::Or(items) => items.iter().for_each(|e| e.for_each_leaf(f)),
            Expr::Not(inner) => inner.for_each_leaf(f),
            Expr::Leaf(leaf) => f(leaf),
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
