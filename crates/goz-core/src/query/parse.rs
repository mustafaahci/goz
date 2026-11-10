//! The query grammar: Everything 1.5's search syntax as goz reads it.
//!
//! Terms separated by white space must all match (AND); `a|b` (or `a or:
//! b`) matches either, and binds tighter than AND, as in Everything:
//! `foo bar|baz` is `foo AND (bar OR baz)`; `!a` (or `not:a`) matches what
//! `a` does not, binding tighter still; `< >` groups. `?and-precedence:`
//! makes AND bind tighter for the operators after it, `?or-precedence:`
//! switches back. An empty term matches everything, so `foo|` and `<>` do,
//! and a `!` followed by white space negates an empty term and matches
//! nothing, again as in Everything. `"quoted text"` is literal, operators
//! and all, except that `*` and `?` stay wildcards, and a character entity
//! (`&vert:`, `&#x21:`) stands for one literal character (the `lex`
//! module reads all of this).
//!
//! A term is matched against the name, or against the full path when it holds
//! a separator (`\` or `/`), comes from `path:`, or the query holds
//! `matchpath:`. A term with `*` or `?` is a wildcard: a name wildcard
//! matches the whole name; a path wildcard matches the end of the full path,
//! whole part by whole part, its `*` and `?` stopping at `\` and its `**`
//! not (see the `glob` module); so does a wildcard with `**`, wherever it
//! stands. The functions `ext:`, `size:` and `dm:` filter
//! on the extension, size and date modified; `file:` and `folder:` (and
//! `dir:`) on the kind, and prefix a term or a group like `path:` does. `case:`
//! and `matchpath:` switch the whole query. An unknown `func:` is refused:
//! as text it could match nothing, since no name contains `:`.
//!
//! The parser builds an [`Expr`]; the lowering in the `expr` module then
//! moves the top-level terms the buckets of [`ParsedQuery`] hold into them,
//! so that a query with no new syntax is exactly the query goz has always
//! built.
//!
//! Terms are folded at parse time when the query is case-insensitive, so the
//! engine compares candidate data (folded iff `!match_case`) against
//! ready-to-use needles. A `dm:` value is resolved here too, to FILETIME
//! bounds, against the [`LocalClock`] the query is parsed with.

use super::dates::{DateFilter, LocalClock, date_filter};
use super::expr::{self, Expr, Leaf, Target};
use super::lex::{self, Prefix, Text, Tok, Token};
use crate::fold::fold;

pub use super::glob::{CodePointBuf, Wildcard};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Folder,
}

/// An inclusive byte-size range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SizeRange {
    pub min: u64,
    pub max: u64,
}

impl SizeRange {
    pub fn contains(self, n: u64) -> bool {
        self.min <= n && n <= self.max
    }
}

/// Non-text filters extracted from functions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filters {
    /// Extensions to accept (folded, no leading dot). Empty `Vec` never
    /// occurs: absence is `None`.
    pub ext: Option<Vec<Vec<u8>>>,
    pub size: Option<SizeRange>,
    pub kind: Option<Kind>,
    /// The date modified, from every `dm:` term at once.
    pub date: Option<DateFilter>,
}

/// A fully parsed query: the top-level terms the buckets hold, which must all
/// match, and `expr`, the rest of the query, which must match as well.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedQuery {
    /// Filename substrings (all must match, AND). Folded iff `!match_case`.
    pub name_terms: Vec<Vec<u8>>,
    /// Full-path substrings, mount prefix included (all must match). Folded iff
    /// `!match_case`; `/` is stored as `\`.
    pub path_terms: Vec<Vec<u8>>,
    /// Whole-name wildcard patterns (all must match).
    pub wildcards: Vec<Wildcard>,
    /// Sub-path wildcard patterns, from a wildcard with a separator or `**`,
    /// or a `path:` value with a wildcard (all must match).
    pub path_wildcards: Vec<Wildcard>,
    pub filters: Filters,
    pub match_case: bool,
    /// What the buckets cannot hold: an OR, a NOT, a group or a filter inside
    /// one, as an expression ANDed with the buckets. `None` for a query of
    /// terms that must all match, which is every query goz read before it
    /// had operators.
    pub expr: Option<Expr>,
}

impl ParsedQuery {
    /// `true` when there are no text constraints and no expression: the
    /// query matches everything subject to `filters`.
    pub fn is_text_empty(&self) -> bool {
        self.name_terms.is_empty()
            && self.path_terms.is_empty()
            && self.wildcards.is_empty()
            && self.path_wildcards.is_empty()
            && self.expr.is_none()
    }
}

/// A query that cannot be executed as written.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum QueryError {
    #[error("malformed {what}: {detail}")]
    Malformed { what: &'static str, detail: String },
    /// More terms than a query may hold; carries the query's count.
    #[error("the query has {0} terms; a query may have at most {max}", max = MAX_QUERY_TERMS)]
    TooManyTerms(usize),
    /// A `func:` goz does not know, carried with its colon. Searching it as text
    /// would match nothing, since no name contains `:`.
    #[error(
        "unknown search function '{0}'; goz supports ext:, size:, dm:, path:, file:, folder:, dir:, case: and matchpath:"
    )]
    UnknownFunction(String),
    /// A `dm:` term in a query parsed without a [`LocalClock`] to resolve it.
    #[error("dm: needs a clock to resolve dates against, and this query was parsed without one")]
    NoClock,
    /// A `<` whose group is never closed, or a `>` with no group to close, at
    /// a 1-based character column.
    #[error("{}", unbalanced(.what, .column))]
    Unbalanced { what: char, column: usize },
    /// A group joined to text, which Everything expands (`gr<a|e>y` is
    /// `gray|grey`) and goz does not, at a 1-based character column.
    #[error(
        "'<' or '>' joined to text at column {column}: group expansion such as gr<a|e>y is not \
         supported; write gray|grey, or quote the text"
    )]
    GroupExpansion { column: usize },
    /// Groups and prefixes nested more than 64 deep.
    #[error("the query nests groups and prefixes more than {MAX_DEPTH} deep")]
    TooDeep,
}

fn unbalanced(what: &char, column: &usize) -> String {
    if *what == '<' {
        format!("unbalanced '<' at column {column}: the group it opens is never closed with '>'")
    } else {
        format!("unbalanced '>' at column {column}: there is no '<' for it to close")
    }
}

/// The most terms one query may hold. The engine tests each candidate against
/// every term under the volume's index read lock, so a query's cost grows with
/// its term count. Its [`super::Budget`] bounds how long it holds the lock,
/// and this cap bounds the work on each candidate, so the budget is checked
/// often. Real queries use a few terms.
const MAX_QUERY_TERMS: usize = 256;

/// How deep groups and prefixes may nest. Each level is a frame of the
/// parser and of every walk of the expression, and the text arrives from the
/// pipe, so a query of a million `<` must be refused rather than overflow the
/// stack. Real queries nest a few levels.
pub(crate) const MAX_DEPTH: usize = 64;

/// How many terms `q` holds, as [`MAX_QUERY_TERMS`] counts them: name and path
/// terms, wildcards and `ext:` values, since each costs a candidate one test,
/// and each term of the expression. A top-level `size:`, `dm:` or kind filter
/// costs the same whatever its value.
fn term_count(q: &ParsedQuery) -> usize {
    q.name_terms.len()
        + q.path_terms.len()
        + q.wildcards.len()
        + q.path_wildcards.len()
        + q.filters.ext.as_ref().map_or(0, Vec::len)
        + q.expr.as_ref().map_or(0, expr::leaf_terms)
}

/// Parses a query string into a [`ParsedQuery`], case-insensitive unless a
/// `case:` directive says otherwise.
pub fn parse_query(input: &str) -> Result<ParsedQuery, QueryError> {
    parse_query_with_case(input, false)
}

/// Parses a query string into a [`ParsedQuery`] whose case sensitivity is
/// `match_case` (the `-case` switch) unless a `case:` directive overrides it.
/// A `dm:` term is refused with [`QueryError::NoClock`]; see
/// [`parse_query_at`].
pub fn parse_query_with_case(input: &str, match_case: bool) -> Result<ParsedQuery, QueryError> {
    parse(input, match_case, None)
}

/// Like [`parse_query_with_case`], with `dm:` values resolved against
/// `clock`: the daemon's entry point, called as each query arrives so that
/// `today` is the day it runs.
pub fn parse_query_at(
    input: &str,
    match_case: bool,
    clock: &dyn LocalClock,
) -> Result<ParsedQuery, QueryError> {
    parse(input, match_case, Some(clock))
}

fn parse(
    input: &str,
    match_case: bool,
    clock: Option<&dyn LocalClock>,
) -> Result<ParsedQuery, QueryError> {
    let lexed = lex::lex(input)?;
    // The last `case:` directive wins over the caller's default, wherever it
    // stands, so text is folded rightly from the first term on.
    let match_case = lexed.match_case.unwrap_or(match_case);
    let mut parser = Parser {
        tokens: &lexed.tokens,
        at: 0,
        depth: 0,
        match_case,
        clock,
    };
    let top = Mods {
        path: lexed.match_path.unwrap_or(false),
        folder: false,
    };
    let e = parser.expr(0, top)?;
    if let Some(stray) = parser.peek() {
        // Only a `>` with no group to close stops the top level.
        return Err(QueryError::Unbalanced {
            what: '>',
            column: stray.column,
        });
    }
    let mut q = ParsedQuery {
        name_terms: Vec::new(),
        path_terms: Vec::new(),
        wildcards: Vec::new(),
        path_wildcards: Vec::new(),
        filters: Filters::default(),
        match_case,
        expr: None,
    };
    expr::lower(expr::simplify(e), &mut q);
    let terms = term_count(&q);
    if terms > MAX_QUERY_TERMS {
        return Err(QueryError::TooManyTerms(terms));
    }
    Ok(q)
}

/// What the prefixes around a term set for it.
#[derive(Clone, Copy, Debug)]
struct Mods {
    /// `path:`, or the `matchpath:` directive: text matches the full path.
    path: bool,
    /// `folder:`: the term matches folders only.
    folder: bool,
}

/// The binary operators.
#[derive(Clone, Copy)]
enum Op {
    And,
    Or,
}

/// A recursive-descent parser over the lexer's tokens, with precedence
/// climbing for AND and OR, whose binding powers each operator token carries
/// (see [`Token::and_first`]).
struct Parser<'t, 'c> {
    tokens: &'t [Token],
    at: usize,
    /// The groups and prefixes open around the token at `at`.
    depth: usize,
    match_case: bool,
    clock: Option<&'c dyn LocalClock>,
}

impl<'t> Parser<'t, '_> {
    fn peek(&self) -> Option<&'t Token> {
        self.tokens.get(self.at)
    }

    /// Whether the token at `at` starts an operand that joins what came
    /// before it: no white space before it, and it is no operator that ends
    /// one.
    fn joined(&self) -> bool {
        self.peek()
            .is_some_and(|t| !t.spaced && !matches!(t.kind, Tok::Or | Tok::And | Tok::Close))
    }

    /// Opens a group or prefix, refusing one too deep.
    fn enter(&mut self) -> Result<(), QueryError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(QueryError::TooDeep);
        }
        Ok(())
    }

    /// An AND and OR expression whose operators bind at least `min` tightly:
    /// precedence climbing, AND binding at 1 and OR at 2, or the reverse for
    /// tokens after `?and-precedence:`.
    fn expr(&mut self, min: u8, mods: Mods) -> Result<Expr, QueryError> {
        let mut lhs = self.operand(mods)?;
        while let Some(t) = self.peek() {
            let (op, consumes) = match t.kind {
                Tok::Or => (Op::Or, true),
                Tok::And => (Op::And, true),
                Tok::Close => break,
                // Another operand: white space between terms is an AND.
                _ => (Op::And, false),
            };
            let power = match (op, t.and_first) {
                (Op::Or, false) | (Op::And, true) => 2,
                (Op::Or, true) | (Op::And, false) => 1,
            };
            if power < min {
                break;
            }
            if consumes {
                self.at += 1;
            }
            let rhs = self.expr(power + 1, mods)?;
            lhs = match op {
                Op::And => Expr::and(lhs, rhs),
                Op::Or => Expr::or(lhs, rhs),
            };
        }
        Ok(lhs)
    }

    /// One operand: a NOT, a group, a prefixed term or text, or, where an
    /// operator, a `>` or the end comes instead, an empty term, which matches
    /// everything (`foo|`, `|foo`, `<>`).
    fn operand(&mut self, mods: Mods) -> Result<Expr, QueryError> {
        let Some(t) = self.peek() else {
            return Ok(empty(mods));
        };
        match &t.kind {
            Tok::Or | Tok::And | Tok::Close => Ok(empty(mods)),
            Tok::Not => self.negation(mods),
            Tok::Open => self.group(mods),
            Tok::Prefix(prefix) => self.prefixed(*prefix, mods),
            Tok::Text(text) => {
                self.at += 1;
                self.text(text, mods)
            }
        }
    }

    /// `!` and what it negates: the operand joined to it, or, when white
    /// space or an operator follows it, an empty term, so that a lone `!`
    /// matches nothing, as in Everything. `!!` cancels.
    fn negation(&mut self, mods: Mods) -> Result<Expr, QueryError> {
        let mut nots = 0usize;
        let operand = loop {
            self.at += 1;
            nots += 1;
            if !self.joined() {
                break empty(mods);
            }
            if self.peek().is_some_and(|t| t.kind == Tok::Not) {
                continue;
            }
            break self.operand(mods)?;
        };
        Ok(if nots % 2 == 1 {
            operand.not()
        } else {
            operand
        })
    }

    /// `< ... >`.
    fn group(&mut self, mods: Mods) -> Result<Expr, QueryError> {
        let column = self.peek().map_or(0, |t| t.column);
        self.at += 1;
        self.enter()?;
        let inner = self.expr(0, mods)?;
        match self.peek() {
            Some(t) if t.kind == Tok::Close => self.at += 1,
            _ => return Err(QueryError::Unbalanced { what: '<', column }),
        }
        self.depth -= 1;
        Ok(inner)
    }

    /// A prefix and what it applies to.
    ///
    /// `path:` and `folder:` are modifiers that reach every term below them,
    /// through NOTs and groups: `folder:!temp` is `!folder:temp`, matching
    /// every file and every folder not named temp. `file:` and `dir:` are
    /// filters ANDed with what follows them: `file:!temp` matches the files
    /// not named temp, and no folder. A live Everything 1.5 answers these
    /// differently in just this way (DESIGN.md §2.1, the S1 battery).
    fn prefixed(&mut self, prefix: Prefix, mods: Mods) -> Result<Expr, QueryError> {
        self.at += 1;
        self.enter()?;
        let e = match prefix {
            Prefix::Path => self.prefixed_operand(Mods { path: true, ..mods }),
            Prefix::Folder => self.prefixed_operand(Mods {
                folder: true,
                ..mods
            }),
            Prefix::File => Ok(Expr::and(
                Expr::Leaf(Leaf::Kind(Kind::File)),
                self.prefixed_operand(mods)?,
            )),
            Prefix::Dir => Ok(Expr::and(
                Expr::Leaf(Leaf::Kind(Kind::Folder)),
                self.prefixed_operand(mods)?,
            )),
            Prefix::Ext | Prefix::Size | Prefix::Date => self.function(prefix, mods),
        }?;
        self.depth -= 1;
        Ok(e)
    }

    /// What a modifier prefix applies to: the operand joined to it, or an
    /// empty term (`file:` alone is every file).
    fn prefixed_operand(&mut self, mods: Mods) -> Result<Expr, QueryError> {
        if self.joined() {
            self.operand(mods)
        } else {
            Ok(empty(mods))
        }
    }

    /// A function's value, after its own `!`s, which negate it.
    fn function(&mut self, prefix: Prefix, mods: Mods) -> Result<Expr, QueryError> {
        let mut nots = 0usize;
        while self.peek().is_some_and(|t| t.kind == Tok::Not) {
            self.at += 1;
            nots += 1;
        }
        let Some(Token {
            kind: Tok::Text(value),
            ..
        }) = self.peek()
        else {
            // The lexer reads a value after every function prefix.
            return Err(QueryError::Malformed {
                what: prefix.spelling(),
                detail: "expected a value".to_string(),
            });
        };
        self.at += 1;
        let value = &value.text;
        let leaf = match prefix {
            Prefix::Ext => {
                let exts: Vec<Vec<u8>> = value
                    .split(|&b| b == b';')
                    .filter(|s| !s.is_empty())
                    .map(|s| fold(trim_dots(s)))
                    .collect();
                if exts.is_empty() {
                    // `ext:` with no value filters nothing, as it never has.
                    Expr::True
                } else {
                    Expr::Leaf(Leaf::Ext(exts))
                }
            }
            Prefix::Size => Expr::Leaf(Leaf::Size(parse_size(&String::from_utf8_lossy(value))?)),
            _ => Expr::Leaf(Leaf::Date(date_filter(
                &String::from_utf8_lossy(value),
                self.clock.ok_or(QueryError::NoClock)?,
            )?)),
        };
        let leaf = with_folder(leaf, mods);
        Ok(if nots % 2 == 1 { leaf.not() } else { leaf })
    }

    /// A text term: a name or path substring, or a name or sub-path wildcard.
    ///
    /// A term with a separator, or under `path:` or `matchpath:`, matches the
    /// full path; `/` is a separator like `\`, since no name contains either.
    /// `*` and `?` make a wildcard, in quotes too, as in Everything, unless an
    /// `&ast:` or `&quest:` entity made them literal; a wildcard with `**`
    /// matches sub-paths, as one with a separator does.
    fn text(&self, text: &Text, mods: Mods) -> Result<Expr, QueryError> {
        let bytes = &text.text;
        if bytes.is_empty() {
            return Ok(empty(mods));
        }
        let fold_term =
            |s: &[u8]| -> Vec<u8> { if self.match_case { s.to_vec() } else { fold(s) } };
        let wildcard = !text.literal_wild && bytes.iter().any(|&b| b == b'*' || b == b'?');
        let is_path = mods.path
            || bytes.iter().any(|&b| b == b'\\' || b == b'/')
            || (wildcard && bytes.windows(2).any(|pair| pair == b"**"));
        let leaf = if is_path {
            let unslashed: Vec<u8> = bytes
                .iter()
                .map(|&b| if b == b'/' { b'\\' } else { b })
                .collect();
            let path = fold_term(&unslashed);
            if wildcard {
                Leaf::Glob(Wildcard::sub_path(&path))
            } else {
                Leaf::Text {
                    target: Target::FullPath,
                    needle: path,
                }
            }
        } else if wildcard {
            Leaf::Glob(Wildcard::name(&fold_term(bytes)))
        } else {
            Leaf::Text {
                target: Target::Name,
                needle: fold_term(bytes),
            }
        };
        Ok(with_folder(Expr::Leaf(leaf), mods))
    }
}

/// An empty term under `mods`: everything, or every folder under `folder:`.
fn empty(mods: Mods) -> Expr {
    with_folder(Expr::True, mods)
}

/// `e` restricted to folders when a `folder:` reaches it.
fn with_folder(e: Expr, mods: Mods) -> Expr {
    if mods.folder {
        Expr::and(Expr::Leaf(Leaf::Kind(Kind::Folder)), e)
    } else {
        e
    }
}

/// `s` without its leading dots: `ext:.pdf` is `ext:pdf`.
fn trim_dots(s: &[u8]) -> &[u8] {
    let start = s.iter().position(|&b| b != b'.').unwrap_or(s.len());
    &s[start..]
}

const KB: u64 = 1024;
const MB: u64 = 1024 * KB;
const GB: u64 = 1024 * MB;
const TB: u64 = 1024 * GB;

/// Parses an es-style `size:` expression: a bare number, a comparison
/// (`>`, `>=`, `<`, `<=`, `=`), a range `a..b`, or a named constant
/// (`empty`, `tiny`, …, `gigantic`). Numbers accept `kb`/`mb`/`gb`/`tb`
/// (1024-based) suffixes.
fn parse_size(value: &str) -> Result<SizeRange, QueryError> {
    let v = value.trim();
    let malformed = |detail: &str| QueryError::Malformed {
        what: "size:",
        detail: detail.to_string(),
    };

    // Named constants.
    match v.to_ascii_lowercase().as_str() {
        "empty" => return Ok(SizeRange { min: 0, max: 0 }),
        "tiny" => {
            return Ok(SizeRange {
                min: 0,
                max: 10 * KB,
            });
        }
        "small" => {
            return Ok(SizeRange {
                min: 10 * KB,
                max: 100 * KB,
            });
        }
        "medium" => {
            return Ok(SizeRange {
                min: 100 * KB,
                max: MB,
            });
        }
        "large" => {
            return Ok(SizeRange {
                min: MB,
                max: 16 * MB,
            });
        }
        "huge" => {
            return Ok(SizeRange {
                min: 16 * MB,
                max: 128 * MB,
            });
        }
        "gigantic" => {
            return Ok(SizeRange {
                min: 128 * MB,
                max: u64::MAX,
            });
        }
        _ => {}
    }

    // Range a..b (either side optional).
    if let Some((lo, hi)) = v.split_once("..") {
        let min = if lo.trim().is_empty() {
            0
        } else {
            parse_bytes(lo).ok_or_else(|| malformed(lo))?
        };
        let max = if hi.trim().is_empty() {
            u64::MAX
        } else {
            parse_bytes(hi).ok_or_else(|| malformed(hi))?
        };
        return Ok(SizeRange { min, max });
    }

    // Comparison operators. Order matters: two-char ops before their one-char
    // prefixes so `>=` is not read as `>` plus a stray `=`.
    for op in [">=", "<=", ">", "<"] {
        if let Some(rest) = v.strip_prefix(op) {
            let n = parse_bytes(rest).ok_or_else(|| malformed(rest))?;
            return Ok(match op {
                ">=" => SizeRange {
                    min: n,
                    max: u64::MAX,
                },
                ">" => SizeRange {
                    min: n.saturating_add(1),
                    max: u64::MAX,
                },
                "<=" => SizeRange { min: 0, max: n },
                // The only remaining op.
                _ => SizeRange {
                    min: 0,
                    max: n.saturating_sub(1),
                },
            });
        }
    }

    // Bare number or `=N` → exact.
    let rest = v.strip_prefix('=').unwrap_or(v);
    let n = parse_bytes(rest).ok_or_else(|| malformed(v))?;
    Ok(SizeRange { min: n, max: n })
}

/// Parses `"10mb"` / `"1024"` / `"2gb"` into a byte count (1024-based units).
fn parse_bytes(s: &str) -> Option<u64> {
    let s = s.trim();
    let lower = s.to_ascii_lowercase();
    let (num, mult) = if let Some(n) = lower.strip_suffix("tb") {
        (n, TB)
    } else if let Some(n) = lower.strip_suffix("gb") {
        (n, GB)
    } else if let Some(n) = lower.strip_suffix("mb") {
        (n, MB)
    } else if let Some(n) = lower.strip_suffix("kb") {
        (n, KB)
    } else if let Some(n) = lower.strip_suffix('b') {
        (n, 1)
    } else {
        (lower.as_str(), 1)
    };
    let num = num.trim();
    let value: u64 = num.parse().ok()?;
    value.checked_mul(mult)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(s: &str) -> ParsedQuery {
        parse_query(s).unwrap()
    }

    /// The expression of `s`, which must have one.
    fn ex(s: &str) -> Expr {
        q(s).expr.unwrap_or_else(|| panic!("{s} has no expression"))
    }

    fn name(s: &str) -> Expr {
        Expr::Leaf(Leaf::Text {
            target: Target::Name,
            needle: fold(s.as_bytes()),
        })
    }

    fn path(s: &str) -> Expr {
        Expr::Leaf(Leaf::Text {
            target: Target::FullPath,
            needle: fold(s.as_bytes()),
        })
    }

    fn kind(k: Kind) -> Expr {
        Expr::Leaf(Leaf::Kind(k))
    }

    fn and(items: impl IntoIterator<Item = Expr>) -> Expr {
        Expr::And(items.into_iter().collect())
    }

    fn or(items: impl IntoIterator<Item = Expr>) -> Expr {
        Expr::Or(items.into_iter().collect())
    }

    #[test]
    fn space_separated_terms_are_anded_and_folded() {
        let p = q("Report Invoice");
        assert_eq!(p.name_terms, vec![b"report".to_vec(), b"invoice".to_vec()]);
        assert!(!p.match_case);
        assert_eq!(p.expr, None, "an AND of terms leaves no expression");
    }

    #[test]
    fn quoted_phrase_keeps_spaces() {
        let p = q("\"quarterly report\"");
        assert_eq!(p.name_terms, vec![b"quarterly report".to_vec()]);
    }

    #[test]
    fn case_function_disables_folding() {
        let p = q("Report case:");
        assert!(p.match_case);
        assert_eq!(p.name_terms, vec![b"Report".to_vec()]);
    }

    #[test]
    fn case_directive_variants_are_consumed_not_searched() {
        // Regression: `case:0` must be recognized (and disable case
        // sensitivity), never leak as a literal "case:0" search term.
        let p = q("Report case:0");
        assert!(!p.match_case);
        assert_eq!(p.name_terms, vec![b"report".to_vec()]); // folded, no "case:0"
        assert!(q("Report case:1").match_case);
        // Last directive wins.
        assert!(!q("case:1 foo case:0").match_case);
        assert!(q("case:0 foo case:1").match_case);
        // Inside a group or after an operator it is still a directive.
        let p = q("<Report case:>|x");
        assert!(p.match_case);
        assert_eq!(
            p.expr,
            Some(or([
                Expr::Leaf(Leaf::Text {
                    target: Target::Name,
                    needle: b"Report".to_vec()
                }),
                Expr::Leaf(Leaf::Text {
                    target: Target::Name,
                    needle: b"x".to_vec()
                }),
            ]))
        );
    }

    #[test]
    fn path_term_detected_by_separator() {
        let p = q("projects\\src");
        assert_eq!(p.path_terms, vec![b"projects\\src".to_vec()]);
        assert!(p.name_terms.is_empty());
    }

    #[test]
    fn wildcards_bucketed_and_matched_whole_name() {
        let p = q("*.rs");
        assert_eq!(p.wildcards.len(), 1);
        assert!(p.wildcards[0].matches(&fold(b"main.rs")));
        assert!(!p.wildcards[0].matches(&fold(b"main.rs.bak")));
        let star = q("re*rt");
        assert!(star.wildcards[0].matches(&fold(b"report")));
        assert!(star.wildcards[0].matches(&fold(b"rert"))); // "re" + empty + "rt"
        assert!(!star.wildcards[0].matches(&fold(b"rt"))); // missing the "re" prefix
        assert!(!star.wildcards[0].matches(&fold(b"reprtx")));
    }

    #[test]
    fn question_mark_matches_one_code_point() {
        let p = q("f?o");
        assert!(p.wildcards[0].matches(&fold(b"foo")));
        assert!(p.wildcards[0].matches(&fold(b"fao")));
        assert!(!p.wildcards[0].matches(&fold(b"fo")));
        assert!(!p.wildcards[0].matches(&fold(b"fooo")));
    }

    #[test]
    fn ext_function_splits_and_folds() {
        let p = q("ext:JPG;png;.Gif");
        assert_eq!(
            p.filters.ext,
            Some(vec![b"jpg".to_vec(), b"png".to_vec(), b"gif".to_vec()])
        );
    }

    #[test]
    fn file_and_folder_kinds() {
        assert_eq!(q("file:").filters.kind, Some(Kind::File));
        assert_eq!(q("folder:").filters.kind, Some(Kind::Folder));
        let p = q("file:report");
        assert_eq!(p.filters.kind, Some(Kind::File));
        assert_eq!(p.name_terms, vec![b"report".to_vec()]);
        let p = q("dir:report");
        assert_eq!(p.filters.kind, Some(Kind::Folder));
        assert_eq!(p.name_terms, vec![b"report".to_vec()]);
        assert_eq!(p.expr, None);
    }

    #[test]
    fn size_operators_ranges_and_constants() {
        assert_eq!(
            q("size:>1mb").filters.size,
            Some(SizeRange {
                min: MB + 1,
                max: u64::MAX
            })
        );
        assert_eq!(
            q("size:>=1mb").filters.size,
            Some(SizeRange {
                min: MB,
                max: u64::MAX
            })
        );
        assert_eq!(
            q("size:1mb..10mb").filters.size,
            Some(SizeRange {
                min: MB,
                max: 10 * MB
            })
        );
        assert_eq!(
            q("size:tiny").filters.size,
            Some(SizeRange {
                min: 0,
                max: 10 * KB
            })
        );
        assert_eq!(
            q("size:1024").filters.size,
            Some(SizeRange {
                min: 1024,
                max: 1024
            })
        );
        assert_eq!(
            q("size:..500").filters.size,
            Some(SizeRange { min: 0, max: 500 })
        );
    }

    /// `|` binds tighter than white space, as in Everything: `foo bar|ever`
    /// is foo AND (bar OR ever) (live: 4 hits, not 13). `!` binds tighter
    /// still, and groups regroup.
    #[test]
    fn or_binds_tighter_than_and_and_not_tighter_still() {
        let p = q("foo bar|ever");
        assert_eq!(p.name_terms, [b"foo".to_vec()]);
        assert_eq!(p.expr, Some(or([name("bar"), name("ever")])));
        assert_eq!(
            ex("<foo bar>|ever"),
            or([and([name("foo"), name("bar")]), name("ever")])
        );
        assert_eq!(ex("!foo|ever"), or([name("foo").not(), name("ever")]));
        assert_eq!(ex("!<foo|ever>"), or([name("foo"), name("ever")]).not());
        assert_eq!(
            ex("foo | ever"),
            or([name("foo"), name("ever")]),
            "spaces around | are ignored"
        );
        let p = q("a|b c|d");
        assert_eq!(
            p.expr,
            Some(and([
                or([name("a"), name("b")]),
                or([name("c"), name("d")])
            ]))
        );
        assert_eq!(
            q("!!foo").name_terms,
            [b"foo".to_vec()],
            "a double NOT cancels"
        );
        let p = q("foo !ever");
        assert_eq!(p.name_terms, [b"foo".to_vec()]);
        assert_eq!(p.expr, Some(name("ever").not()));
        assert_eq!(ex("<<a|b>>|c"), or([name("a"), name("b"), name("c")]));
    }

    /// The precedence switches apply to the operators after them; the
    /// default comes back with `?or-precedence:`.
    #[test]
    fn precedence_switches_regroup_the_operators_after_them() {
        let and_first = or([and([name("foo"), name("bar")]), name("ever")]);
        assert_eq!(ex("?and-precedence: foo bar|ever"), and_first);
        assert_eq!(ex("and-precedence: foo bar|ever"), and_first);
        assert_eq!(ex("?AndPrecedence:foo bar|ever"), and_first);
        let p = q("foo bar|ever ?and-precedence:");
        assert_eq!(p.expr, Some(or([name("bar"), name("ever")])), "too late");
        assert_eq!(
            q("?and-precedence: ?or-precedence: foo bar|ever").expr,
            Some(or([name("bar"), name("ever")]))
        );
    }

    /// An empty operand matches everything, and a NOT with nothing joined
    /// to it negates an empty term, matching nothing (live: L1:7–11, 22–29).
    #[test]
    fn empty_operands_match_everything_and_a_lone_bang_nothing() {
        for everything in [
            "foo|",
            "|foo",
            "foo||ever",
            "|",
            "<>",
            "foo|<>",
            "<|>",
            "foo or:",
        ] {
            let p = q(everything);
            assert!(
                p.is_text_empty() && p.filters == Filters::default(),
                "{everything}"
            );
        }
        assert_eq!(q("<> foo").name_terms, [b"foo".to_vec()]);
        assert_eq!(q("| ever foo").name_terms, [b"foo".to_vec()]);
        for nothing in ["!", "foo ! ever", "not: foo", "! <foo>", "<!>"] {
            assert_eq!(q(nothing).expr, Some(Expr::False), "{nothing}");
        }
        assert_eq!(q("!!").expr, None, "NOT NOT nothing");
    }

    /// The keyword operators are the symbols spelled out, in any case.
    #[test]
    fn keyword_operators_are_the_symbols() {
        assert_eq!(q("foo and: bar"), q("foo bar"));
        assert_eq!(q("foo AND: bar"), q("foo bar"));
        assert_eq!(q("foo or: ever"), q("foo|ever"));
        assert_eq!(q("foo Or:ever"), q("foo|ever"));
        assert_eq!(q("not:foo"), q("!foo"));
        assert_eq!(q("NOT:not:foo"), q("foo"));
        assert_eq!(q("foo not:ever"), q("foo !ever"));
        assert_eq!(q("foo bar and:"), q("foo bar"));
        assert_eq!(q("foo AND bar").name_terms.len(), 3, "the word is text");
        // `?and-precedence:` makes the keyword AND bind tighter too.
        assert_eq!(
            ex("?and-precedence: a and: b or: c"),
            or([and([name("a"), name("b")]), name("c")])
        );
    }

    /// `!` is literal inside a word, under `no-!:`, from an entity, and in
    /// quotes.
    #[test]
    fn a_literal_bang_is_text() {
        for (query, text) in [
            ("wow!.txt", "wow!.txt"),
            ("wow!", "wow!"),
            ("no-!:!bang", "!bang"),
            ("no!:!bang", "!bang"),
            ("&excl:bang", "!bang"),
            ("\"!bang\"", "!bang"),
            ("&#33:bang", "!bang"),
        ] {
            let p = q(query);
            assert_eq!(p.name_terms, [text.as_bytes().to_vec()], "{query}");
            assert_eq!(p.expr, None, "{query}");
        }
        let p = q("?no-!: !bang !x");
        assert_eq!(p.name_terms, [b"!bang".to_vec(), b"!x".to_vec()]);
        let p = q("no-!: !bang");
        assert_eq!(
            p.expr,
            Some(name("bang").not()),
            "no-!: reaches only its term"
        );
        assert_eq!(ex("!no-!:!bang"), name("!bang").not());
    }

    #[test]
    fn entities_are_literal_text() {
        assert_eq!(q("foo&sp:bar").name_terms, [b"foo bar".to_vec()]);
        assert_eq!(q("\"foo&sp:bar\"").name_terms, [b"foo bar".to_vec()]);
        assert_eq!(q("spider&vert:man").name_terms, [b"spider|man".to_vec()]);
        assert_eq!(q("a&b &amp:").name_terms, [b"a&b".to_vec(), b"&".to_vec()]);
        assert_eq!(q("w&lt:x&gt:").name_terms, [b"w<x>".to_vec()]);
        assert_eq!(
            q(r"C:\Program&sp:Files").path_terms,
            [br"c:\program files".to_vec()]
        );
        // An escaped wildcard turns every wildcard of its term off.
        assert_eq!(q("&ast:.exe").name_terms, [b"*.exe".to_vec()]);
        assert_eq!(q("*.exe&ast:").name_terms, [b"*.exe*".to_vec()]);
        assert_eq!(q("&quest:.exe").name_terms, [b"?.exe".to_vec()]);
        assert!(
            q("&foo:bar").name_terms == [b"&foo:bar".to_vec()],
            "no entity: text"
        );
    }

    /// `path:` and `folder:` reach the terms below them, through a NOT;
    /// `file:` and `dir:` are ANDed with what follows them; a function's own
    /// `!` negates it. Each as a live Everything 1.5 answers (DESIGN.md §2.1).
    #[test]
    fn prefixes_reach_through_groups_and_nots_as_in_everything() {
        let p = q("file:!a");
        assert_eq!(p.filters.kind, Some(Kind::File), "files without a");
        assert_eq!(p.expr, Some(name("a").not()));
        assert_eq!(q("file:<!a>"), q("file:!a"));
        let p = q("dir:!a");
        assert_eq!(p.filters.kind, Some(Kind::Folder));
        assert_eq!(p.expr, Some(name("a").not()));
        assert_eq!(
            ex("folder:!dot"),
            and([kind(Kind::Folder), name("dot")]).not()
        );
        assert_eq!(ex("folder:<!dot>"), ex("folder:!dot"));
        assert_eq!(ex("!folder:dot"), ex("folder:!dot"));
        assert_eq!(ex("folder:!"), kind(Kind::Folder).not());
        assert_eq!(ex("path:!words"), path("words").not());
        let p = q("path:<words ever>");
        assert_eq!(p.path_terms, [b"words".to_vec(), b"ever".to_vec()]);
        assert_eq!(
            ex("path:<words|case1> foo"),
            or([path("words"), path("case1")])
        );
        let p = q("file:path:words ever");
        assert_eq!(p.filters.kind, Some(Kind::File));
        assert_eq!(p.path_terms, [b"words".to_vec()]);
        assert_eq!(p.name_terms, [b"ever".to_vec()]);
        assert_eq!(
            ex("folder:<a|ext:pdf>"),
            or([
                and([kind(Kind::Folder), name("a")]),
                and([
                    kind(Kind::Folder),
                    Expr::Leaf(Leaf::Ext(vec![b"pdf".to_vec()]))
                ]),
            ])
        );
        assert_eq!(
            ex("ext:!pdf"),
            Expr::Leaf(Leaf::Ext(vec![b"pdf".to_vec()])).not()
        );
        assert_eq!(
            ex("size:!=1kb"),
            Expr::Leaf(Leaf::Size(SizeRange { min: KB, max: KB })).not()
        );
        assert_eq!(
            ex("ext:pdf|txt"),
            or([Expr::Leaf(Leaf::Ext(vec![b"pdf".to_vec()])), name("txt")]),
            "a | ends a function's value"
        );
        assert_eq!(
            ex("<size:>1mb>|x"),
            or([
                Expr::Leaf(Leaf::Size(SizeRange {
                    min: MB + 1,
                    max: u64::MAX
                })),
                name("x"),
            ])
        );
    }

    /// Unbalanced groups, group expansion and runaway nesting are refused,
    /// with the column where it applies.
    #[test]
    fn malformed_groups_are_refused_precisely() {
        assert_eq!(
            parse_query("<foo"),
            Err(QueryError::Unbalanced {
                what: '<',
                column: 1
            })
        );
        assert_eq!(
            parse_query("a <b <c> d"),
            Err(QueryError::Unbalanced {
                what: '<',
                column: 3
            })
        );
        assert_eq!(
            parse_query("foo>"),
            Err(QueryError::Unbalanced {
                what: '>',
                column: 4
            })
        );
        assert_eq!(
            parse_query("<a> b>"),
            Err(QueryError::Unbalanced {
                what: '>',
                column: 6
            })
        );
        assert_eq!(
            parse_query("gr<a|e>y"),
            Err(QueryError::GroupExpansion { column: 3 })
        );
        assert_eq!(
            parse_query("<spider>man"),
            Err(QueryError::GroupExpansion { column: 9 })
        );
        let deep = |n: usize| format!("{}a{}", "<".repeat(n), ">".repeat(n));
        assert!(parse_query(&deep(MAX_DEPTH)).is_ok());
        assert_eq!(parse_query(&deep(MAX_DEPTH + 1)), Err(QueryError::TooDeep));
        assert_eq!(
            parse_query(&"<".repeat(1_000_000)),
            Err(QueryError::TooDeep),
            "refused before the stack runs out"
        );
        assert_eq!(
            parse_query(&format!("{}a", "file:".repeat(MAX_DEPTH + 1))),
            Err(QueryError::TooDeep)
        );
        assert!(parse_query(&format!("{}a", "!".repeat(1_000_001))).is_ok());
        let message = QueryError::Unbalanced {
            what: '<',
            column: 7,
        }
        .to_string();
        assert!(message.contains("column 7"), "{message}");
    }

    /// Searched as text, an unknown function could only match nothing, since no
    /// name contains `:`; it is refused so the user learns why. A drive letter
    /// and text that is no function name are not functions.
    #[test]
    fn unknown_functions_are_refused_but_drives_are_paths() {
        assert_eq!(
            parse_query("dc:today"),
            Err(QueryError::UnknownFunction("dc:".to_string()))
        );
        assert_eq!(
            parse_query("report Parent:foo"),
            Err(QueryError::UnknownFunction("Parent:".to_string()))
        );
        assert_eq!(
            parse_query("a|!dc:x"),
            Err(QueryError::UnknownFunction("dc:".to_string()))
        );
        assert_eq!(q(r"C:\Windows").path_terms, vec![br"c:\windows".to_vec()]);
        assert_eq!(q("12:30").name_terms, vec![b"12:30".to_vec()]);
        assert_eq!(q(r":\docs").path_terms, vec![br":\docs".to_vec()]);
        assert_eq!(q(r#""a:b""#).name_terms, vec![b"a:b".to_vec()]);
        assert_eq!(q("file:a:b").name_terms, vec![b"a:b".to_vec()], "a value");
        assert!(matches!(
            parse_query("case:2"),
            Err(QueryError::Malformed { what: "case:", .. })
        ));
    }

    /// The `-case` switch is the parser's default, and a `case:` directive in
    /// the text still wins, so matching and the scope lookup follow one value.
    #[test]
    fn the_case_switch_is_the_default_a_directive_overrides() {
        let p = parse_query_with_case("Report", true).unwrap();
        assert!(p.match_case);
        assert_eq!(p.name_terms, vec![b"Report".to_vec()]);
        assert!(
            !parse_query_with_case("Report case:0", true)
                .unwrap()
                .match_case
        );
        assert!(
            parse_query_with_case("Report case:", false)
                .unwrap()
                .match_case
        );
        assert!(
            q(r#"Report case:"1""#).match_case,
            "a quoted value still sets it"
        );
        assert!(!q("Report").match_case);
    }

    /// No name contains `/`, so it separates folders like `\`.
    #[test]
    fn a_slash_is_a_separator() {
        assert_eq!(
            q("Projects/src").path_terms,
            vec![br"projects\src".to_vec()]
        );
        assert_eq!(q("path:a/b").path_terms, vec![br"a\b".to_vec()]);
        let p = q("*/docs/*.md");
        assert!(p.wildcards.is_empty());
        assert!(p.path_wildcards[0].matches(br"c:\repo\docs\a.md"));
    }

    /// A wildcard with a separator, a `**` or in a `path:` value matches the
    /// end of the full path, whole part by whole part, and its `*` stops at a
    /// separator: Everything 1.5's rule, which replaced the old one where `*`
    /// crossed folders and the pattern had to match the whole full path.
    #[test]
    fn separator_and_path_wildcards_match_sub_paths() {
        let p = q(r"C:\repo\*.cs");
        assert!(p.wildcards.is_empty() && p.path_terms.is_empty());
        let w = &p.path_wildcards[0];
        assert!(w.matches(br"c:\repo\main.cs"));
        assert!(
            !w.matches(br"c:\repo\sub\main.cs"),
            "* no longer crosses folders"
        );
        assert!(!w.matches(br"d:\repo\main.cs"));
        let w = &q(r"repo\*.cs").path_wildcards[0];
        assert!(w.matches(br"d:\work\repo\main.cs"), "from any part");
        assert!(!w.matches(br"d:\work\myrepo\main.cs"), "whole parts");
        assert!(q(r"repo\**\*.cs").path_wildcards[0].matches(br"d:\repo\a\b\main.cs"));
        assert!(q("path:*.md").path_wildcards[0].matches(br"c:\notes\a.md"));
        let p = q("**cmd*");
        assert!(p.wildcards.is_empty(), "** makes a sub-path wildcard");
        assert!(p.path_wildcards[0].matches(br"c:\windows\system32\cmd.exe"));
        assert_eq!(
            q(r"\windows\*.exe").path_wildcards[0].pattern(),
            br"windows\*.exe"
        );
        assert_eq!(
            q(r"windows\*\").path_wildcards[0].pattern(),
            br"windows\*\**"
        );
    }

    /// Quotes let a value keep its spaces: a quoted term with a separator is a
    /// path, a function's quoted value is still that function's, and `*` and
    /// `?` stay wildcards in quotes, as in Everything (a quoted name used to
    /// keep them literal, which matched nothing, since no name holds them).
    #[test]
    fn quoted_paths_and_function_values() {
        assert_eq!(
            q(r#""C:\Program Files\app""#).path_terms,
            vec![br"c:\program files\app".to_vec()]
        );
        assert!(
            q(r#""C:\Program Files\*.exe""#).path_wildcards[0].matches(br"c:\program files\a.exe")
        );
        assert_eq!(
            q(r#"path:"C:\Program Files""#).path_terms,
            vec![br"c:\program files".to_vec()]
        );
        let p = q(r#"file:"my report""#);
        assert_eq!(p.name_terms, vec![b"my report".to_vec()]);
        assert_eq!(p.filters.kind, Some(Kind::File));
        assert_eq!(q(r#"ext:"rs""#).filters.ext, Some(vec![b"rs".to_vec()]));
        let p = q(r#""*.rs""#);
        assert!(p.name_terms.is_empty());
        assert!(p.wildcards[0].matches(b"main.rs"));
        assert!(q(r#""First Level*""#).wildcards[0].matches(b"first level first"));
    }

    /// `dm:` and its es spellings resolve against the clock the query is
    /// parsed with, next to the text they filter, and two `dm:` terms must
    /// both pass.
    #[test]
    fn dm_terms_resolve_against_the_clock() {
        use crate::query::dates::tests::{FixedClock, ft};
        let clock = FixedClock::utc(ft("2026-11-04T10:00:00"));
        let at = |text| parse_query_at(text, false, &clock).unwrap();
        let day = DateFilter::Within {
            min: ft("2026-11-04T00:00:00"),
            max: ft("2026-11-05T00:00:00") - 1,
        };
        let today = Some(day);
        let p = at("Report dm:today");
        assert_eq!(p.name_terms, vec![b"report".to_vec()]);
        assert_eq!(p.filters.date, today);
        assert_eq!(
            term_count(&p),
            1,
            "a date costs the same whatever its value"
        );
        for alias in [
            "DM:today",
            "datemodified:today",
            "date-modified:today",
            r#"dm:"today""#,
        ] {
            assert_eq!(at(alias).filters.date, today, "{alias}");
        }
        assert_eq!(at("dm:thismonth dm:today").filters.date, today);
        assert_eq!(
            at("dm:today dm:unknown")
                .filters
                .date
                .map(|d| d.matches(None)),
            Some(false)
        );
        assert!(at("report").filters.date.is_none());
        assert_eq!(
            at("dm:today|x").expr,
            Some(or([Expr::Leaf(Leaf::Date(day)), name("x")]))
        );
    }

    /// Without a clock a `dm:` term cannot be resolved, so it is refused
    /// rather than read as some other day; a bad value is malformed.
    #[test]
    fn dm_needs_a_clock_and_a_date() {
        assert_eq!(parse_query("dm:today"), Err(QueryError::NoClock));
        assert_eq!(parse_query("x|!dm:today"), Err(QueryError::NoClock));
        let clock = crate::query::dates::tests::FixedClock::utc(0);
        assert!(matches!(
            parse_query_at("dm:soon", false, &clock),
            Err(QueryError::Malformed { what: "dm:", .. })
        ));
    }

    #[test]
    fn malformed_size_is_reported() {
        assert!(matches!(
            parse_query("size:abc"),
            Err(QueryError::Malformed { what: "size:", .. })
        ));
    }

    #[test]
    fn empty_query_has_no_text_constraints() {
        assert!(q("").is_text_empty());
        assert!(q("   ").is_text_empty());
        assert!(!q("a|b").is_text_empty());
    }

    /// Every term costs each candidate a test under the index read lock, so
    /// the cap bounds the work on each candidate between checks of the budget.
    #[test]
    fn a_query_with_more_than_the_maximum_terms_is_refused() {
        let terms = |n: usize| vec!["a"; n].join(" ");
        assert!(parse_query(&terms(MAX_QUERY_TERMS)).is_ok());
        assert_eq!(
            parse_query(&terms(MAX_QUERY_TERMS + 1)),
            Err(QueryError::TooManyTerms(MAX_QUERY_TERMS + 1))
        );
        let ors = |n: usize| vec!["a"; n].join("|");
        assert!(parse_query(&ors(MAX_QUERY_TERMS)).is_ok());
        assert_eq!(
            parse_query(&ors(MAX_QUERY_TERMS + 1)),
            Err(QueryError::TooManyTerms(MAX_QUERY_TERMS + 1)),
            "an expression's terms count too"
        );
        let sizes = |n: usize| vec!["size:>1"; n].join("|");
        assert_eq!(
            parse_query(&sizes(MAX_QUERY_TERMS + 1)),
            Err(QueryError::TooManyTerms(MAX_QUERY_TERMS + 1)),
            "a filter in an expression costs a test"
        );
    }

    /// A long `ext:` list costs each candidate a test per value, so its values
    /// count toward the cap; `size:` and `file:` do not.
    #[test]
    fn ext_values_count_toward_the_term_cap() {
        let exts = |n: usize| {
            let list: Vec<String> = (0..n).map(|i| format!("e{i}")).collect();
            format!("a size:>1kb file: ext:{}", list.join(";"))
        };
        assert!(parse_query(&exts(MAX_QUERY_TERMS - 1)).is_ok());
        assert_eq!(
            parse_query(&exts(MAX_QUERY_TERMS)),
            Err(QueryError::TooManyTerms(MAX_QUERY_TERMS + 1))
        );
    }

    /// `matchpath:` sends all the search text to the full path, wherever it
    /// stands, and leaves the functions that are not about text alone.
    #[test]
    fn match_path_moves_text_terms_to_the_full_path() {
        let p = q("report matchpath: *.pdf ext:pdf");
        assert_eq!(p.path_terms, [b"report".to_vec()]);
        assert!(p.name_terms.is_empty() && p.wildcards.is_empty());
        assert!(p.path_wildcards[0].matches(br"c:\docs\a.pdf"));
        assert_eq!(p.filters.ext, Some(vec![b"pdf".to_vec()]));
        assert_eq!(q("file:x matchpath:").path_terms, [b"x".to_vec()]);
        assert_eq!(q("x matchpath: matchpath:0").name_terms, [b"x".to_vec()]);
        assert_eq!(ex("a|b matchpath:"), or([path("a"), path("b")]));
        assert!(matches!(
            parse_query("matchpath:maybe"),
            Err(QueryError::Malformed { .. })
        ));
    }

    /// The kind, `ext:` and `size:` filters keep their last top-level term
    /// and `dm:` terms all apply, as they always have; the same filters
    /// inside an expression are its terms.
    #[test]
    fn top_level_filters_keep_their_old_meaning() {
        let p = q("file: folder: ext:a ext:b size:1 size:2 x");
        assert_eq!(p.filters.kind, Some(Kind::Folder));
        assert_eq!(p.filters.ext, Some(vec![b"b".to_vec()]));
        assert_eq!(p.filters.size, Some(SizeRange { min: 2, max: 2 }));
        assert_eq!(p.expr, None);
        let p = q("<file: x> folder:");
        assert_eq!(
            p.filters.kind,
            Some(Kind::Folder),
            "a group at the top is its terms"
        );
        assert_eq!(
            ex("file:|folder:"),
            or([kind(Kind::File), kind(Kind::Folder)])
        );
    }

    /// The pure-AND guarantee: a query written only with v0.6.0's syntax,
    /// in the spellings whose meaning did not change, builds exactly the
    /// buckets v0.6.0's parser built and no expression, so it runs the same
    /// prefilters and fast paths. The spellings left out are the ones
    /// DESIGN.md §5.3 changes on purpose: path wildcards (C1), quoted
    /// wildcards (C2), and `|`, `!`, `< >`, keyword operators and entities,
    /// which v0.6.0 refused (C3); a function's value never holds another
    /// prefix, which v0.6.0 read as text matching nothing.
    mod guarantee {
        use super::*;
        use crate::query::dates::tests::{FixedClock, ft};
        use crate::query::parse_v060::parse_v060;
        use proptest::prelude::*;

        fn fragment() -> impl Strategy<Value = String> {
            prop_oneof![
                "[a-zA-Z0-9çÇéÉ_.-]{1,6}",
                "\"[a-zA-Z .]{0,6}\"",
                "[a-z.]{0,3}[*?][a-z.]{0,3}",
                "[a-z]{1,4}[\\\\/][a-z]{0,4}",
                "[cC]:\\\\[a-z]{0,4}",
                "\\\\[a-z]{1,4}\\\\",
                "ext:[a-zA-Z.;]{0,6}",
                "ext:\"[a-z ]{1,4}\"",
                "(file|folder|dir|FILE):[a-z]{1,4}",
                "path:[a-z\\\\]{0,6}",
                prop::sample::select(vec![
                    "size:>1mb",
                    "size:1kb..2mb",
                    "size:tiny",
                    "size:<=100",
                    "size:1024",
                    "size:abc",
                    "size:>=10kb",
                    "dm:today",
                    "dm:lastweek",
                    "dm:2024",
                    "dm:>2020",
                    "dm:2024-01-01..2024-03-31",
                    "dm:unknown",
                    "dm:soon",
                    "DM:thismonth",
                    "date-modified:yesterday",
                    "file:",
                    "folder:",
                    "dir:",
                    "case:",
                    "case:0",
                    "case:1",
                    "CASE:",
                    "case:\"1\"",
                    "case:x",
                    "dc:x",
                    "Parent:x",
                    "12:30",
                    ":x",
                    "x:",
                    "matchpath:x",
                    "\"\"",
                ])
                .prop_map(str::to_string),
            ]
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(4096))]

            #[test]
            fn a_v060_query_parses_as_it_did(
                parts in prop::collection::vec(fragment(), 0..8),
                spaces in prop::collection::vec(1usize..3, 8),
                match_path in any::<bool>(),
                match_case in any::<bool>(),
            ) {
                let mut query = String::new();
                for (part, gap) in parts.iter().zip(&spaces) {
                    query.push_str(part);
                    query.push_str(&" ".repeat(*gap));
                }
                // `matchpath:` makes a wildcard a path wildcard (C1).
                if match_path && !query.contains(['*', '?']) {
                    query.push_str("matchpath:");
                }
                let clock = FixedClock::utc(ft("2026-11-04T10:00:00"));
                let old = parse_v060(&query, match_case, &clock);
                let new = parse_query_at(&query, match_case, &clock);
                match (old, new) {
                    (Ok(old), Ok(new)) => {
                        prop_assert_eq!(new.expr, None, "{:?}", query);
                        prop_assert_eq!(&new.name_terms, &old.name_terms, "{:?}", query);
                        prop_assert_eq!(&new.path_terms, &old.path_terms, "{:?}", query);
                        let patterns: Vec<Vec<u8>> =
                            new.wildcards.iter().map(|w| w.pattern().to_vec()).collect();
                        prop_assert_eq!(&patterns, &old.wildcards, "{:?}", query);
                        prop_assert!(new.wildcards.iter().all(Wildcard::is_name));
                        prop_assert!(new.path_wildcards.is_empty() && old.path_wildcards.is_empty());
                        prop_assert_eq!(&new.filters, &old.filters, "{:?}", query);
                        prop_assert_eq!(new.match_case, old.match_case, "{:?}", query);
                    }
                    (Err(_), Err(_)) => {}
                    (old, new) => prop_assert!(false, "{:?}: {:?} then, {:?} now", query, old, new),
                }
            }
        }
    }

    mod arbitrary {
        use super::*;
        use proptest::prelude::*;

        /// Query text built from the parser's own syntax and arbitrary
        /// characters, so that functions, ranges, wildcards, operators,
        /// groups, quotes and entities are reached.
        fn arb_query() -> impl Strategy<Value = String> {
            let fragment = prop_oneof![
                prop::sample::select(vec![
                    "size:",
                    "ext:",
                    "path:",
                    "file:",
                    "folder:",
                    "dir:",
                    "case:",
                    "case:0",
                    "..",
                    ">=",
                    "<",
                    ">",
                    "=",
                    "*",
                    "**",
                    "?",
                    ";",
                    ".",
                    "\"",
                    " ",
                    "\\",
                    "/",
                    "kb",
                    "gb",
                    "tiny",
                    "dm:",
                    "today",
                    "lastweek",
                    "thisyear",
                    "days",
                    "months",
                    "last",
                    "-",
                    "unknown",
                    "|",
                    "!",
                    "and:",
                    "or:",
                    "not:",
                    "?and-precedence:",
                    "?or-precedence:",
                    "no-!:",
                    "?no-!:",
                    "matchpath:",
                    "&sp:",
                    "&vert:",
                    "&ast:",
                    "&#x21:",
                    "&#",
                    "&",
                    "<>",
                    "<a|b>",
                    "!<",
                    "?case:",
                    "ww:",
                    "regex:",
                ])
                .prop_map(str::to_string),
                "[0-9]{1,24}",
                "\\PC{0,4}",
            ];
            prop::collection::vec(fragment, 0..16).prop_map(|parts| parts.concat())
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(1024))]

            /// The query text arrives from the pipe, so any text can arrive,
            /// and a panic would fail the query instead of reporting it.
            #[test]
            fn arbitrary_text_errors_instead_of_panicking(
                input in arb_query(),
                match_case in any::<bool>(),
                now in prop_oneof![Just(i64::MIN), Just(0), Just(i64::MAX), any::<i64>()],
            ) {
                let _ = parse_query_with_case(&input, match_case);
                let clock = crate::query::dates::tests::FixedClock::utc(now);
                let _ = parse_query_at(&input, match_case, &clock);
            }
        }
    }
}
