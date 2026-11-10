//! A query's boolean expression, and how it is lowered onto the buckets a
//! plain AND query has always filled.
//!
//! The parser builds an [`Expr`] for the whole query. Most queries are an AND
//! of terms the buckets of [`ParsedQuery`] hold exactly (name and path
//! substrings, wildcards, the `ext:`, `size:`, `dm:` and kind filters), and
//! for those the lowering leaves no expression at all: the query is the one
//! goz has always built, and runs the same prefilters and fast paths. What a
//! bucket cannot hold (an OR, a NOT, a group or filter inside one) stays an
//! expression, ANDed with the buckets, for the engine to evaluate.

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

/// What a text term is matched against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// The name.
    Name,
    /// The full path, mount prefix included.
    FullPath,
}

/// One term. Text is folded iff the query is case-insensitive, as the
/// buckets' is; `ext:` values always are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Leaf {
    /// The target holds `needle`.
    Text { target: Target, needle: Vec<u8> },
    /// A name or sub-path wildcard (the wildcard knows which).
    Glob(Wildcard),
    /// Files or folders only.
    Kind(Kind),
    /// The extension is one of these (never empty).
    Ext(Vec<Vec<u8>>),
    /// The size is known and in range.
    Size(SizeRange),
    /// The date modified passes.
    Date(DateFilter),
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
/// `q.expr`. The kind, `ext:` and `size:` filters keep their last term, as
/// they always have; `dm:` terms all apply.
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
                target: Target::Name,
                needle,
            }) => q.name_terms.push(needle),
            Expr::Leaf(Leaf::Text {
                target: Target::FullPath,
                needle,
            }) => q.path_terms.push(needle),
            Expr::Leaf(Leaf::Glob(w)) if w.is_name() => q.wildcards.push(w),
            Expr::Leaf(Leaf::Glob(w)) => q.path_wildcards.push(w),
            Expr::Leaf(Leaf::Kind(kind)) => q.filters.kind = Some(kind),
            Expr::Leaf(Leaf::Ext(exts)) => q.filters.ext = Some(exts),
            Expr::Leaf(Leaf::Size(range)) => q.filters.size = Some(range),
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

/// How many terms `e`'s leaves count for the term cap: one each, and an
/// `ext:` one per value, since each costs a candidate one test.
pub(super) fn leaf_terms(e: &Expr) -> usize {
    let mut n = 0;
    e.for_each_leaf(&mut |leaf| {
        n += match leaf {
            Leaf::Ext(exts) => exts.len(),
            _ => 1,
        }
    });
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> Expr {
        Expr::Leaf(Leaf::Text {
            target: Target::Name,
            needle: s.as_bytes().to_vec(),
        })
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
