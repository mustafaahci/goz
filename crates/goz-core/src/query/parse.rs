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
//! stands. `d:` alone is everything on drive D:, and `\` alone every
//! folder.
//!
//! Search modifiers prefix a term or a group and say how its text matches:
//! `case:`, `diacritics:` (accents count; by default they do not),
//! `regex:` (the text is a regular expression, see the `pattern` module),
//! `path:`, `ww:` (whole words), `prefix:` and `suffix:` (word
//! starts and ends), `whole:`, `startwith:`, `endwith:`, `nopunc:` and
//! `nows:` (skip punctuation or white space in the name), `starall:`,
//! `nowildcards:`, `metric:`, each with Everything's aliases and `no`
//! forms; the outermost of two that disagree wins, and `?mod:` sets one for
//! the rest of the search. `file:` and `folder:` (and `dir:`) keep files or
//! folders. Functions test what the name alone does not hold: `ext:`,
//! `extension:`, `size:`, `dm:`, the lengths (`len:`, ...), `parent:`,
//! `ancestor:`, and the parts of a full path (`name:`, `stem:`,
//! `full-path:`, `location:`, `parent-name:`), the counts (`parent-count:`,
//! `child-count:`, `descendant-count:`, `sibling-count:` ...), the
//! sub-searches of other entries' names (`child:`, `descendant:`,
//! `sibling:`, `ancestor-name:`, `parent-child:`, `parent-sibling:`,
//! `ancestor-child:`, `ancestor-sibling:`, whose value is a term on those
//! names, modifiers and all), and the tests of a folder's entries' sizes
//! and dates (`child-file-size:`, `child-date-modified:`,
//! `total-child-size:`); a function's value may be a
//! `;` list (any one matches) or a group (`ext:<pdf|txt>`). The macros
//! `audio:`, `zip:`, `doc:`, `exe:`, `image:` (`pic:`) and `video:` keep
//! the files of Everything's own filters. `case:` and `matchpath:` with no
//! text switch the whole query. An unknown `func:` is refused: as text it
//! could match nothing, since no name contains `:`; so are Everything's
//! functions that need what goz does not index (`child-size:`, which
//! compares folder sizes), and its `$name:` substitutions.
//!
//! The parser builds an [`Expr`]; the lowering in the `expr` module then
//! moves the top-level terms the buckets of [`ParsedQuery`] hold into them,
//! so that a query with no new syntax is exactly the query goz has always
//! built.
//!
//! Terms are folded at parse time unless they match case (with
//! [`crate::fold::fold`], or [`crate::fold::fold_exact`] under
//! `diacritics:`), so the engine compares candidate data against
//! ready-to-use needles. A `dm:` value is
//! resolved here too, to FILETIME bounds, against the [`LocalClock`] the
//! query is parsed with.

use super::content::ContentEncoding;
use super::dates::{DateFilter, LocalClock, date_filter};
use super::expr::{self, Anchors, CountOf, Expr, FolderPath, Leaf, LenOf, Relation, Skip, Target};
use super::lex::{self, Func, Macro, ModSet, Prefix, Text, Tok, Token};
use super::pattern::RegexTerm;
use super::value::{self, Number};
use crate::fold::{fold, fold_exact};

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

    /// Whether every number is in range.
    pub fn is_any(self) -> bool {
        self.min == 0 && self.max == u64::MAX
    }
}

/// Non-text filters extracted from functions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filters {
    /// The extensions a file may have, folded, each naming its name's last
    /// dot-separated parts (`tar.gz`); folders have none. Empty `Vec` never
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
    /// What the buckets cannot hold: an OR, a NOT, a group, a term with a
    /// modifier, a second filter, as an expression ANDed with the buckets.
    /// `None` for a query of terms that must all match, which is every query
    /// goz read before it had operators.
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
        "unknown search function or modifier '{0}'; goz supports ext:, extension:, size:, dm:, \
         len:, parent:, ancestor:, name:, stem:, full-path:, location:, parent-name:, \
         parent-count:, child-count:, child-file-count:, child-folder-count:, empty:, child:, \
         descendant:, sibling:, parent-child:, parent-sibling:, ancestor-child:, \
         ancestor-sibling: (each with -file: and -folder: forms), ancestor-name:, \
         descendant-count:, sibling-count: (each with -file- and -folder- forms), \
         child-file-size:, child-date-modified: (with -file- and -folder- forms), \
         total-child-size:, the macros audio:, zip:, doc:, exe:, image: and video:, and the \
         modifiers case:, diacritics:, regex:, path:, file:, folder:, ww:, prefix:, suffix:, \
         whole:, startwith:, endwith:, nopunc:, nows:, starall:, nowildcards: and metric: (each \
         with Everything's aliases and no- forms), and matchpath:"
    )]
    UnknownFunction(String),
    /// A function Everything defines that needs what goz's index does not
    /// hold (`child-size:`, which compares folder sizes), or a `$name:`
    /// substitution: what was written, and why goz cannot answer it
    /// exactly.
    #[error("{what} is not supported: {why}")]
    Unsupported { what: String, why: &'static str },
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
    /// A `regex:` pattern the `regex` crate refuses (look-around,
    /// backreferences, a syntax error, a pattern too large), with its
    /// reason.
    #[error("bad regular expression {pattern:?}: {detail}")]
    BadRegex { pattern: String, detail: String },
    /// More `content:` terms than the goz client splits one query for
    /// (each doubles the queries it asks gozd), with the query's count.
    #[error(
        "the query has {0} content: terms; goz searches at most {max} in one query",
        max = super::content::MAX_CONTENT_TERMS
    )]
    TooManyContentTerms(usize),
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

/// The most `regex:` terms one query may hold. Each is a regular expression
/// matched against every name it tests, the costliest test a term makes,
/// and each keeps caches on every thread of the pass over the names (see
/// the `pattern` module): 8 keep a hostile query's memory to tens of MiB.
const MAX_REGEX_TERMS: usize = 8;

/// The most functions of related entries worked out before the search one
/// query may hold: the sub-searches (`child:`, `descendant:`, `sibling:`,
/// `parent-sibling:`, `ancestor-child:` ..., a `;` list being one), the
/// child size and date tests, and the descendant and sibling counts. Each
/// is a pass over the names or the entries, and one or two sets of the
/// volume's entries, a bit each: 8 keep a hostile query's to a few MB.
const MAX_RELATED_TERMS: usize = 8;

/// How deep groups and prefixes may nest. Each level is a frame of the
/// parser and of every walk of the expression, and the text arrives from the
/// pipe, so a query of a million `<` must be refused rather than overflow the
/// stack. Real queries nest a few levels.
pub(crate) const MAX_DEPTH: usize = 64;

/// How many terms `q` holds, as [`MAX_QUERY_TERMS`] counts them: name and path
/// terms and wildcards, since each costs a candidate one test, and each term
/// of the expression. A top-level `ext:`, `size:`, `dm:` or kind filter
/// costs one test whatever its value, and an `ext:` list is one lookup.
fn term_count(q: &ParsedQuery) -> usize {
    q.name_terms.len()
        + q.path_terms.len()
        + q.wildcards.len()
        + q.path_wildcards.len()
        + usize::from(q.filters.ext.is_some())
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
    parse_as(input, match_case, clock, None).map(|(q, _)| q)
}

/// A `content:` term (or one of its encodings' forms) of a query parsed
/// for the goz client by [`parse_content_terms`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ContentFound {
    pub(super) encoding: ContentEncoding,
    /// The characters `[start, end)` of the query the term's function and
    /// value were read from: what the client replaces with a constant.
    pub(super) span: (usize, usize),
    /// The value under the term's modifiers: text and `regex:` leaves on
    /// the "name" (the content, here), an OR of a value group's
    /// alternatives, and so on. Kinds are left out: they stay in the query
    /// around the constant.
    pub(super) value: Expr,
}

/// Parses `input` as the goz client does before it searches content: every
/// `content:` term is found and read, under its modifiers, rather than
/// refused, and `dm:` values are left to gozd. The rest of the query is
/// checked as gozd checks it.
pub(super) fn parse_content_terms(
    input: &str,
    match_case: bool,
) -> Result<Vec<ContentFound>, QueryError> {
    parse_as(input, match_case, None, Some(Vec::new())).map(|(_, found)| found.unwrap_or_default())
}

fn parse_as(
    input: &str,
    match_case: bool,
    clock: Option<&dyn LocalClock>,
    content: Option<Vec<ContentFound>>,
) -> Result<(ParsedQuery, Option<Vec<ContentFound>>), QueryError> {
    let lexed = lex::lex(input)?;
    // The last `case:` directive wins over the caller's default, wherever it
    // stands, so text is folded rightly from the first term on.
    let match_case = lexed.match_case.unwrap_or(match_case);
    let mut parser = Parser {
        tokens: &lexed.tokens,
        at: 0,
        depth: 0,
        match_case,
        match_path: lexed.match_path.unwrap_or(false),
        clock,
        content,
        in_content: false,
    };
    let e = parser.expr(0, Mods::default())?;
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
    let regexes = q.expr.as_ref().map_or(0, expr::regex_terms);
    if regexes > MAX_REGEX_TERMS {
        return Err(QueryError::Malformed {
            what: "regex:",
            detail: format!(
                "the query has {regexes} regex: terms; a query may have at most {MAX_REGEX_TERMS}"
            ),
        });
    }
    let related = q.expr.as_ref().map_or(0, expr::related_terms);
    if related > MAX_RELATED_TERMS {
        return Err(QueryError::Malformed {
            what: "sub-search",
            detail: format!(
                "the query has {related} terms on related entries (child:, descendant:, \
                 sibling:, ancestor-name:, parent-child:, parent-sibling:, ancestor-child:, \
                 ancestor-sibling:, child-file-size:, child-date-modified:, descendant-count:, \
                 sibling-count: and their forms); a query may have at most {MAX_RELATED_TERMS}"
            ),
        });
    }
    Ok((q, parser.content))
}

/// What the prefixes around a term set for it.
#[derive(Clone, Copy, Debug, Default)]
struct Mods {
    /// The modifiers, the outermost set first.
    set: ModSet,
    /// `folder:`: the term matches folders only.
    folder: bool,
    /// Inside a function's value group (`ext:<pdf|txt>`): the function
    /// whose values the group's text is.
    func: Option<Func>,
    /// The kind a `folder:` or `?folder:` puts around a term is put around
    /// it later, outside the NOT of a value's own `!` (see
    /// [`Parser::function`]).
    kindless: bool,
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
    /// The `matchpath:` directive.
    match_path: bool,
    clock: Option<&'c dyn LocalClock>,
    /// For the goz client, the `content:` terms found so far; `None` in
    /// gozd, which refuses them (see [`Parser::content`]).
    content: Option<Vec<ContentFound>>,
    /// Inside a `content:` term's value.
    in_content: bool,
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
            return self.empty(mods);
        };
        match &t.kind {
            Tok::Or | Tok::And | Tok::Close => self.empty(mods),
            Tok::Not => self.negation(mods),
            Tok::Open => self.group(mods),
            Tok::Prefix(prefix) => self.prefixed(*prefix, mods),
            Tok::Text(text) => {
                self.at += 1;
                self.text(t, text, mods)
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
                break self.empty(mods)?;
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
    /// Modifiers, `path:` and `folder:` among them, reach every term below
    /// them, through NOTs and groups: `folder:!temp` is `!folder:temp`,
    /// matching every file and every folder not named temp. `file:`, `dir:`
    /// and the macros are filters ANDed with what follows them: `file:!temp`
    /// matches the files not named temp, and no folder. A live Everything
    /// 1.5 answers these differently in just this way (DESIGN.md §2.1, the
    /// S1 battery).
    fn prefixed(&mut self, prefix: Prefix, mods: Mods) -> Result<Expr, QueryError> {
        self.at += 1;
        self.enter()?;
        let e = match prefix {
            Prefix::Mod(m, on) => {
                let mut inner = mods;
                inner.set.set_outer(m, on);
                self.prefixed_operand(inner)
            }
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
            Prefix::Macro(m) => Ok(Expr::and(macro_ext(m), self.prefixed_operand(mods)?)),
            Prefix::Empty => Ok(Expr::and(
                Expr::Leaf(Leaf::Count {
                    of: CountOf::Children,
                    range: SizeRange { min: 0, max: 0 },
                }),
                self.prefixed_operand(mods)?,
            )),
            Prefix::Func(Func::Content(encoding)) => self.content(encoding, mods),
            Prefix::Func(func) => self.function(func, mods),
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
            self.empty(mods)
        }
    }

    /// A function's value, after its own `!`s, which negate it: its text,
    /// or a group of values (`ext:<pdf|txt>`).
    fn function(&mut self, func: Func, mods: Mods) -> Result<Expr, QueryError> {
        // The `?mod:`s where the prefix stands.
        let global = self
            .at
            .checked_sub(1)
            .and_then(|at| self.tokens.get(at))
            .map_or_else(ModSet::default, |t| t.global);
        let mut nots = 0usize;
        while self.peek().is_some_and(|t| t.kind == Tok::Not) {
            self.at += 1;
            nots += 1;
        }
        // A value's own `!` goes inside the kind around the term:
        // `folder:len:!0` is the folders whose name is not empty, not every
        // file and those folders.
        let negated = nots % 2 == 1;
        let mods_in = Mods {
            kindless: mods.kindless || negated,
            ..mods
        };
        let inner = Mods {
            func: Some(func),
            ..mods_in
        };
        let e = match self.peek() {
            Some(t) if t.kind == Tok::Open && !t.spaced => self.group(inner)?,
            Some(
                t @ Token {
                    kind: Tok::Text(value),
                    ..
                },
            ) => {
                self.at += 1;
                self.value(func, value, t.global, mods_in)?
            }
            // The lexer reads a value or a value group after every function
            // prefix.
            _ => {
                return Err(QueryError::Malformed {
                    what: func.spelling(),
                    detail: "expected a value".to_string(),
                });
            }
        };
        Ok(if negated {
            with_kind(not_value(func, e), mods, global)
        } else {
            e
        })
    }

    /// A `content:` term, the prefix just read. gozd never reads a file, so
    /// it refuses one; the goz client reads its value here, and searches
    /// the files itself (see the `content` module). The term stands for a
    /// constant in the query: a kind around it stays there, and applies to
    /// the constant as it would to the term.
    fn content(&mut self, encoding: ContentEncoding, mods: Mods) -> Result<Expr, QueryError> {
        let what = Func::Content(encoding).spelling();
        let start = self.tokens[self.at - 1].column - 1;
        if self.content.is_none() {
            return Err(QueryError::Unsupported {
                what: what.to_string(),
                why: "gozd answers from its index and never reads a file; the goz CLI \
                      searches content itself, under your own account",
            });
        }
        if self.in_content || mods.func.is_some() {
            return Err(QueryError::Malformed {
                what,
                detail: "a content: term stands on its own, not in another function's value"
                    .to_string(),
            });
        }
        self.in_content = true;
        let value = self.function(
            Func::Content(encoding),
            Mods {
                folder: false,
                kindless: true,
                ..mods
            },
        );
        self.in_content = false;
        let value = expr::simplify(value?);
        let end = self.tokens[self.at - 1].end;
        if let Some(found) = self.content.as_mut() {
            found.push(ContentFound {
                encoding,
                span: (start, end),
                value,
            });
        }
        Ok(Expr::True)
    }

    /// An empty term under `mods`: everything (or every folder under
    /// `folder:`), but nothing under `whole:`, as no name is empty (L8:131,
    /// while `startwith:` and `ww:` alone match everything, L8:133-134); or,
    /// in a function's value group, the function's empty value
    /// (`ext:<pdf|>`).
    fn empty(&self, mods: Mods) -> Result<Expr, QueryError> {
        self.empty_under(mods, ModSet::default())
    }

    /// [`Parser::empty`] under the `?mod:`s of `global` too.
    fn empty_under(&self, mods: Mods, global: ModSet) -> Result<Expr, QueryError> {
        match mods.func {
            Some(func) => self.value(func, &Text::default(), global, mods),
            None => {
                let set = mods.set.over(global);
                let whole = set.whole == Some(true)
                    || (set.start_with == Some(true) && set.end_with == Some(true));
                let e = if whole { Expr::False } else { Expr::True };
                Ok(with_kind(e, mods, global))
            }
        }
    }

    /// A text token: a search term, or in a function's value group one of
    /// the function's values.
    fn text(&self, token: &Token, text: &Text, mods: Mods) -> Result<Expr, QueryError> {
        if let Some(func) = mods.func {
            return self.value(func, text, token.global, mods);
        }
        let set = mods.set.over(token.global);
        let bytes = &text.text;
        if bytes.is_empty() {
            return self.empty_under(mods, token.global);
        }
        let leaf = if is_lone_separator(bytes) {
            // `\` or `/` alone is every folder (Everything's quick folder
            // search), quoted or under any modifier, `path:` and `nopath:`
            // included (L7:419-424, L8:102-108).
            Leaf::Kind(Kind::Folder)
        } else if set.path != Some(false)
            && bytes.len() == 2
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
        {
            // `d:` alone is everything on that drive.
            Leaf::Drive(bytes[0].to_ascii_lowercase())
        } else {
            return Ok(with_kind(
                self.term(None, bytes, text.literal_wild, set)?,
                mods,
                token.global,
            ));
        };
        Ok(with_kind(Expr::Leaf(leaf), mods, token.global))
    }

    /// A text term on `target`, or, for `None`, search text: the name, or
    /// the full path when it holds a separator, `path:` or `matchpath:` says
    /// so, or it is a wildcard with `**`; `nopath:` keeps it on the name. `/`
    /// is a separator like `\`, since no name contains either. `*` and `?`
    /// make a wildcard, in quotes too, as in Everything, unless an `&ast:` or
    /// `&quest:` entity or `nowildcards:` made them literal; a wildcard
    /// matches the whole of its target, so the word and anchor modifiers do
    /// not apply to it, as in Everything (`ww:ev*` is `ev*`). Under
    /// `diacritics:` the text keeps its accents.
    ///
    /// Under `regex:` the text is a regular expression (see the `pattern`
    /// module), on the name, or on the full path under `path:` or
    /// `matchpath:` or when it holds an escaped separator, `\\`; no other
    /// modifier but `case:` applies to it.
    fn term(
        &self,
        target: Option<Target>,
        bytes: &[u8],
        literal_wild: bool,
        set: ModSet,
    ) -> Result<Expr, QueryError> {
        if let Some(name) = substitution(bytes) {
            return Err(QueryError::Unsupported {
                what: format!("the substitution {name}"),
                why: "Everything replaces it with a value of each entry it tests (its own \
                      $stem:, $name: ..., or a regex: capture, $1:), so the term differs \
                      from entry to entry, which is no rewrite of the query; goz runs one \
                      term over every entry",
            });
        }
        let case = set.case.unwrap_or(self.match_case);
        if set.regex == Some(true) {
            let target = target.unwrap_or_else(|| {
                let escaped_separator = bytes.windows(2).any(|pair| pair == br"\\");
                if set.path.unwrap_or(self.match_path || escaped_separator) {
                    Target::FullPath
                } else {
                    Target::Name
                }
            });
            if bytes.is_empty() && matches!(target, Target::Name | Target::FullPath) {
                // An empty pattern matches every name and path.
                return Ok(Expr::True);
            }
            return Ok(Expr::Leaf(Leaf::Regex {
                target,
                regex: RegexTerm::new(bytes, case)?,
            }));
        }
        let diacritics = !case && set.diacritics == Some(true);
        let wildcard = set.wildcards != Some(false)
            && !literal_wild
            && bytes.iter().any(|&b| b == b'*' || b == b'?');
        let target = target.unwrap_or_else(|| {
            let separator = bytes.iter().any(|&b| b == b'\\' || b == b'/');
            let path = set.path.unwrap_or_else(|| {
                self.match_path
                    || separator
                    || (wildcard && bytes.windows(2).any(|pair| pair == b"**"))
            });
            if path { Target::FullPath } else { Target::Name }
        });
        if bytes.is_empty()
            && matches!(
                target,
                Target::Name | Target::FullPath | Target::Location | Target::ParentName
            )
        {
            // Every entry has a name, a full path and a location, none
            // empty, and an empty value matches any text but the empty one.
            return Ok(Expr::True);
        }
        let unslashed: Vec<u8> = if matches!(target, Target::FullPath | Target::Location) {
            bytes
                .iter()
                .map(|&b| if b == b'/' { b'\\' } else { b })
                .collect()
        } else {
            bytes.to_vec()
        };
        let text = if case {
            unslashed
        } else if diacritics {
            fold_exact(&unslashed)
        } else {
            fold(&unslashed)
        };
        if wildcard {
            let glob = if target == Target::FullPath {
                Wildcard::sub_path(&text, set.star_all == Some(true))
            } else {
                Wildcard::name(&text)
            };
            return Ok(Expr::Leaf(Leaf::Glob {
                target,
                glob,
                case,
                diacritics,
            }));
        }
        let on = |o: Option<bool>| o == Some(true);
        let words = on(set.whole_words);
        Ok(Expr::Leaf(Leaf::Text {
            target,
            needle: text,
            case,
            diacritics,
            anchors: Anchors {
                start: on(set.whole) || on(set.start_with),
                end: on(set.whole) || on(set.end_with),
                word_start: words || on(set.word_start),
                word_end: words || on(set.word_end),
            },
            skip: Skip {
                punct: on(set.skip_punct),
                space: on(set.skip_space),
            },
        }))
    }

    /// A function's value: a `;` list of values any one of which matches,
    /// under the modifiers of `global` (the `?mod:`s before it) over `mods`.
    fn value(
        &self,
        func: Func,
        text: &Text,
        global: ModSet,
        mods: Mods,
    ) -> Result<Expr, QueryError> {
        let set = mods.set.over(global);
        let e = if func == Func::Ext {
            ext_leaf(text, set)
        } else if let Func::Content(_) = func {
            // Everything reads a full-text function's `;` as text.
            self.item(func, &text.text, text.literal_wild, set)?
        } else if related_of(func).is_some() {
            // A list is one sub-search, of any of its values.
            self.related(func, text.items(), text.literal_wild, set)?
        } else if child_prop_kind(func).is_some() {
            // A list is one test of the children, of any of its values.
            self.child_props(func, text.items(), set)?
        } else {
            let mut items = Vec::new();
            for item in text.items() {
                items.push(self.item(func, item, text.literal_wild, set)?);
            }
            items.into_iter().reduce(Expr::or).unwrap_or(Expr::False)
        };
        Ok(with_kind(e, mods, global))
    }

    /// One value of a function's list.
    fn item(
        &self,
        func: Func,
        item: &[u8],
        literal_wild: bool,
        set: ModSet,
    ) -> Result<Expr, QueryError> {
        let what = func.spelling();
        let target = match func {
            Func::Name => Target::Name,
            Func::Stem => Target::Stem,
            Func::FullPath => Target::FullPath,
            Func::Location => Target::Location,
            Func::ParentName => Target::ParentName,
            Func::Extension => Target::Extension,
            Func::Content(_) => {
                // Content is matched as written: `*` and `?` are text
                // (`content:hel*` finds nothing in Everything), and an empty
                // value matches a file whose content is not empty.
                if item.is_empty() && set.regex != Some(true) {
                    let case = set.case.unwrap_or(self.match_case);
                    return Ok(Expr::Leaf(Leaf::Text {
                        target: Target::Name,
                        needle: Vec::new(),
                        case,
                        diacritics: !case && set.diacritics == Some(true),
                        anchors: Anchors::default(),
                        skip: Skip::default(),
                    }));
                }
                return self.term(Some(Target::Name), item, true, set);
            }
            Func::Parent | Func::Ancestor => {
                // Under `diacritics:` the folders' accents count too.
                let path = FolderPath {
                    path: value::folder_path(item, what)?,
                    accented: if set.diacritics == Some(true) {
                        Some(value::folder_path_as(item, what, fold_exact)?)
                    } else {
                        None
                    },
                };
                return Ok(Expr::Leaf(if func == Func::Parent {
                    Leaf::Parent(vec![path])
                } else {
                    Leaf::Ancestor(vec![path])
                }));
            }
            // `value` reads an `ext:` list whole, and a sub-search's.
            Func::Ext => return Ok(ext_leaf(&Text::default(), set)),
            Func::Child
            | Func::ChildFile
            | Func::ChildFolder
            | Func::Descendant
            | Func::DescendantFile
            | Func::DescendantFolder
            | Func::Sibling
            | Func::SiblingFile
            | Func::SiblingFolder
            | Func::AncestorName
            | Func::ParentChild
            | Func::ParentChildFile
            | Func::ParentChildFolder
            | Func::ParentSibling
            | Func::ParentSiblingFile
            | Func::ParentSiblingFolder
            | Func::AncestorChild
            | Func::AncestorChildFile
            | Func::AncestorChildFolder
            | Func::AncestorSibling
            | Func::AncestorSiblingFile
            | Func::AncestorSiblingFolder => {
                return self.related(func, std::iter::once(item), literal_wild, set);
            }
            // `value` reads these lists whole too.
            Func::ChildFileSize | Func::ChildDate | Func::ChildFileDate | Func::ChildFolderDate => {
                return self.child_props(func, std::iter::once(item), set);
            }
            // The client leaves dates to gozd.
            Func::Date if self.content.is_some() => return Ok(Expr::True),
            Func::Date => {
                // A value's own `!` negates it, here as before the list.
                let (nots, item) = bangs(item);
                let leaf = Expr::Leaf(Leaf::Date(date_filter(
                    &String::from_utf8_lossy(item),
                    self.clock.ok_or(QueryError::NoClock)?,
                )?));
                return Ok(if nots % 2 == 1 {
                    not_value(func, leaf)
                } else {
                    leaf
                });
            }
            Func::Size
            | Func::Len
            | Func::NameLen
            | Func::PathLen
            | Func::LocationLen
            | Func::ExtLen
            | Func::ParentCount
            | Func::ChildCount
            | Func::ChildFileCount
            | Func::ChildFolderCount
            | Func::DescendantCount
            | Func::DescendantFileCount
            | Func::DescendantFolderCount
            | Func::SiblingCount
            | Func::SiblingFileCount
            | Func::SiblingFolderCount
            | Func::TotalChildSize => {
                let (nots, item) = bangs(item);
                let s = String::from_utf8_lossy(item);
                let leaf = if func == Func::Size {
                    match value::number(&s, what, true, set.metric == Some(true))? {
                        Number::Range(range) => Expr::Leaf(Leaf::Size(range)),
                        Number::Unknown => Expr::Leaf(Leaf::SizeUnknown),
                    }
                } else if func == Func::TotalChildSize {
                    match value::number(&s, what, true, set.metric == Some(true))? {
                        Number::Range(range) => Expr::Leaf(Leaf::TotalChildSize(range)),
                        // A file of unknown size leaves its folder's total
                        // unknown, which no value names.
                        Number::Unknown => Expr::False,
                    }
                } else if let Some(of) = count_of(func) {
                    match value::number(&s, what, false, false)? {
                        Number::Range(range) => Expr::Leaf(Leaf::Count { of, range }),
                        // A count is always known.
                        Number::Unknown => Expr::False,
                    }
                } else {
                    let of = match func {
                        Func::NameLen => LenOf::Name,
                        Func::PathLen => LenOf::FullPath,
                        Func::LocationLen => LenOf::Location,
                        Func::ExtLen => LenOf::Extension,
                        // `len:` measures what a term would match: the full
                        // path under `path:`.
                        _ if set.path.unwrap_or(self.match_path) => LenOf::FullPath,
                        _ => LenOf::Name,
                    };
                    match value::number(&s, what, false, false)? {
                        Number::Range(range) => Expr::Leaf(Leaf::Len { of, range }),
                        // A length is always known.
                        Number::Unknown => Expr::False,
                    }
                };
                return Ok(if nots % 2 == 1 {
                    not_value(func, leaf)
                } else {
                    leaf
                });
            }
        };
        self.term(Some(target), item, literal_wild, set)
    }
}

impl Parser<'_, '_> {
    /// A sub-search (`child:`, `descendant:`, `sibling:`, `ancestor-name:`)
    /// of `values`, any one of which an other entry's name matches: each a
    /// term on the name under the term's modifiers, so `whole:child:cmd.exe`
    /// names a child called `cmd.exe` and `regex:child:^cmd` one whose name
    /// starts so. A value's leading `!` is text, as in Everything
    /// (`child:!bang` is a folder holding `!bang.txt`), and so is a `\`: a
    /// name holds none, so `child:sub\deep` matches nothing, as there.
    fn related<'v>(
        &self,
        func: Func,
        values: impl Iterator<Item = &'v [u8]>,
        literal_wild: bool,
        set: ModSet,
    ) -> Result<Expr, QueryError> {
        let Some((relation, kind)) = related_of(func) else {
            return Ok(Expr::False);
        };
        let mut name = Expr::False;
        for value in values {
            name = Expr::or(
                name,
                self.term(Some(Target::Name), value, literal_wild, set)?,
            );
        }
        Ok(Expr::Leaf(Leaf::Related {
            relation,
            kind,
            term: Box::new(expr::simplify(name)),
        }))
    }

    /// `child-file-size:` and `child-date-modified:` (and its `-file-` and
    /// `-folder-` forms): a folder holding an entry, of the function's kind,
    /// whose size or date modified one of `values` names (a size or a date,
    /// as `size:` and `dm:` read them). A value's own `!` keeps the folders
    /// that hold no such entry, as a live Everything 1.5 answers
    /// `child-file-size:!0` (W8): only a folder holds entries, so the NOT
    /// stays among the folders, as `child-count:!2`'s does.
    fn child_props<'v>(
        &self,
        func: Func,
        values: impl Iterator<Item = &'v [u8]>,
        set: ModSet,
    ) -> Result<Expr, QueryError> {
        let kind = child_prop_kind(func).unwrap_or(None);
        let what = func.spelling();
        let held = |term: Expr| {
            Expr::Leaf(Leaf::Related {
                relation: Relation::Child,
                kind,
                term: Box::new(term),
            })
        };
        let mut any = Expr::False;
        let mut negated = Vec::new();
        for item in values {
            let (nots, item) = bangs(item);
            let s = String::from_utf8_lossy(item);
            let leaf = if func == Func::ChildFileSize {
                match value::number(&s, what, true, set.metric == Some(true))? {
                    Number::Range(range) => Leaf::Size(range),
                    Number::Unknown => Leaf::SizeUnknown,
                }
            } else if self.content.is_some() {
                // The client leaves dates to gozd.
                Leaf::Kind(Kind::Folder)
            } else {
                Leaf::Date(date_filter(&s, self.clock.ok_or(QueryError::NoClock)?)?)
            };
            if nots % 2 == 1 {
                negated.push(not_value(func, held(Expr::Leaf(leaf))));
            } else {
                any = Expr::or(any, Expr::Leaf(leaf));
            }
        }
        let any = expr::simplify(any);
        let mut e = if any == Expr::False {
            Expr::False
        } else {
            held(any)
        };
        for n in negated {
            e = Expr::or(e, n);
        }
        Ok(expr::simplify(e))
    }
}

/// The relation and kind of a sub-search function; `None` for any other.
fn related_of(func: Func) -> Option<(Relation, Option<Kind>)> {
    Some(match func {
        Func::Child => (Relation::Child, None),
        Func::ChildFile => (Relation::Child, Some(Kind::File)),
        Func::ChildFolder => (Relation::Child, Some(Kind::Folder)),
        Func::Descendant => (Relation::Descendant, None),
        Func::DescendantFile => (Relation::Descendant, Some(Kind::File)),
        Func::DescendantFolder => (Relation::Descendant, Some(Kind::Folder)),
        Func::Sibling => (Relation::Sibling, None),
        Func::SiblingFile => (Relation::Sibling, Some(Kind::File)),
        Func::SiblingFolder => (Relation::Sibling, Some(Kind::Folder)),
        Func::AncestorName => (Relation::Ancestor, Some(Kind::Folder)),
        Func::ParentChild => (Relation::ParentChild, None),
        Func::ParentChildFile => (Relation::ParentChild, Some(Kind::File)),
        Func::ParentChildFolder => (Relation::ParentChild, Some(Kind::Folder)),
        Func::ParentSibling => (Relation::ParentSibling, None),
        Func::ParentSiblingFile => (Relation::ParentSibling, Some(Kind::File)),
        Func::ParentSiblingFolder => (Relation::ParentSibling, Some(Kind::Folder)),
        Func::AncestorChild => (Relation::AncestorChild, None),
        Func::AncestorChildFile => (Relation::AncestorChild, Some(Kind::File)),
        Func::AncestorChildFolder => (Relation::AncestorChild, Some(Kind::Folder)),
        Func::AncestorSibling => (Relation::AncestorSibling, None),
        Func::AncestorSiblingFile => (Relation::AncestorSibling, Some(Kind::File)),
        Func::AncestorSiblingFolder => (Relation::AncestorSibling, Some(Kind::Folder)),
        _ => return None,
    })
}

/// The kind of the entries a child size or date function tests (`None`:
/// files and folders); `None` for any other function.
fn child_prop_kind(func: Func) -> Option<Option<Kind>> {
    Some(match func {
        Func::ChildFileSize | Func::ChildFileDate => Some(Kind::File),
        Func::ChildDate => None,
        Func::ChildFolderDate => Some(Kind::Folder),
        _ => return None,
    })
}

/// A number's or a date's own `!` (`size:!1kb`, `dm:!today`, `extlen:!3`):
/// the entry has such a value, and not the one written, as Everything reads
/// it, where `extlen:!3` keeps no folder, since a folder has no extension
/// (L8:90), while `!extlen:3`, the NOT of the whole term, keeps them all.
/// goz knows no folder's size, so `size:!1kb` keeps files alone, and no
/// unknown date passes `dm:!today`. Every entry has a name, a full path, a
/// location and folders above it, so their lengths and its parent count
/// need nothing more; only a folder has children to count.
fn not_value(func: Func, e: Expr) -> Expr {
    let any = SizeRange {
        min: 0,
        max: u64::MAX,
    };
    let known = match func {
        Func::Size => Expr::Leaf(Leaf::Size(any)),
        Func::Date => Expr::Leaf(Leaf::Date(DateFilter::Within {
            min: i64::MIN,
            max: i64::MAX,
        })),
        Func::ExtLen => Expr::Leaf(Leaf::Len {
            of: LenOf::Extension,
            range: any,
        }),
        // Only a folder has children to count (`child-count:!2` keeps no
        // file, while `!child-count:2` keeps them all, as in Everything),
        // and descendants (`descendant-count:!1`, W8).
        Func::ChildCount
        | Func::ChildFileCount
        | Func::ChildFolderCount
        | Func::DescendantCount
        | Func::DescendantFileCount
        | Func::DescendantFolderCount => Expr::Leaf(Leaf::Count {
            of: count_of(func).unwrap_or(CountOf::Children),
            range: any,
        }),
        // Only a folder holds entries whose size or date to test
        // (`child-file-size:!0` keeps the folders holding no empty file,
        // the empty ones too, W8), or a total of its files' sizes.
        Func::ChildFileSize | Func::ChildDate | Func::ChildFileDate | Func::ChildFolderDate => {
            Expr::Leaf(Leaf::Kind(Kind::Folder))
        }
        Func::TotalChildSize => Expr::Leaf(Leaf::TotalChildSize(any)),
        _ => Expr::True,
    };
    Expr::and(known, e.not())
}

/// What a count function counts; `None` for any other function.
fn count_of(func: Func) -> Option<CountOf> {
    Some(match func {
        Func::ParentCount => CountOf::Parents,
        Func::ChildCount => CountOf::Children,
        Func::ChildFileCount => CountOf::ChildFiles,
        Func::ChildFolderCount => CountOf::ChildFolders,
        Func::DescendantCount => CountOf::Descendants,
        Func::DescendantFileCount => CountOf::DescendantFiles,
        Func::DescendantFolderCount => CountOf::DescendantFolders,
        Func::SiblingCount => CountOf::Siblings,
        Func::SiblingFileCount => CountOf::SiblingFiles,
        Func::SiblingFolderCount => CountOf::SiblingFolders,
        _ => return None,
    })
}

/// The first `$name:` in `text`, Everything's substitution of a property
/// of the entry tested (`$stem:`, `$size:`) or of a `regex:` capture
/// (`$1:`): a `$`, letters, digits or hyphens, and a colon. As text it
/// could match nothing, since no name holds a colon and a full path holds
/// one only after its drive letter.
fn substitution(text: &[u8]) -> Option<String> {
    let mut at = 0;
    while let Some(dollar) = text[at..].iter().position(|&b| b == b'$') {
        let start = at + dollar;
        let body = text[start + 1..]
            .iter()
            .take_while(|&&b| b.is_ascii_alphanumeric() || b == b'-')
            .count();
        let colon = start + 1 + body;
        if body > 0 && text.get(colon) == Some(&b':') {
            return Some(String::from_utf8_lossy(&text[start..=colon]).into_owned());
        }
        at = start + 1;
    }
    None
}

/// `item` without its leading `!`s, and how many there were.
fn bangs(item: &[u8]) -> (usize, &[u8]) {
    let n = item.iter().take_while(|&&b| b == b'!').count();
    (n, &item[n..])
}

/// Whether `bytes` is one separator, `\` or `/`.
fn is_lone_separator(bytes: &[u8]) -> bool {
    bytes == b"\\" || bytes == b"/"
}

/// The `ext:` term of `text`'s list: its values with their leading `.`s and
/// `*`s dropped, folded unless `case:` (only a modifier: `ext:` ignores the
/// whole-query `case:` switch, as it always has); a value holding a wildcard
/// an entity made literal is dropped, as no extension holds one. An empty
/// item names no extension (`ext:txt;;exe`, L2:106), but empty items at the
/// end of a list are dropped (`ext:txt;exe;;` keeps two, L8:117), unless the
/// list holds nothing else (`ext:;` is no extension, L2:107).
fn ext_leaf(text: &Text, set: ModSet) -> Expr {
    let case = set.case.unwrap_or(false);
    let diacritics = !case && set.diacritics == Some(true);
    let mut values: Vec<Vec<u8>> = Vec::new();
    let mut items: Vec<&[u8]> = text.items().collect();
    while items.len() > 1 && items.last().is_some_and(|i| i.is_empty()) {
        items.pop();
    }
    for item in items {
        let v = value::ext_value(item);
        if text.literal_wild && v.iter().any(|&b| b == b'*' || b == b'?') {
            continue;
        }
        let v = if case {
            v.to_vec()
        } else if diacritics {
            fold_exact(v)
        } else {
            fold(v)
        };
        if !values.contains(&v) {
            values.push(v);
        }
    }
    if values.is_empty() {
        Expr::False
    } else {
        Expr::Leaf(Leaf::Ext {
            values,
            case,
            diacritics,
        })
    }
}

/// The files a macro keeps: those with one of its extensions.
fn macro_ext(m: Macro) -> Expr {
    Expr::Leaf(Leaf::Ext {
        values: m
            .extensions()
            .split(';')
            .map(|v| v.as_bytes().to_vec())
            .collect(),
        case: false,
        diacritics: false,
    })
}

/// `e` restricted to folders when a `folder:` reaches it, or to the kind a
/// `?file:` or `?folder:` before it names.
fn with_kind(e: Expr, mods: Mods, global: ModSet) -> Expr {
    if mods.kindless {
        return e;
    }
    let kind = if mods.folder {
        Some(Kind::Folder)
    } else {
        global.kind
    };
    match kind {
        Some(kind) => Expr::and(Expr::Leaf(Leaf::Kind(kind)), e),
        None => e,
    }
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

    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;

    fn name(s: &str) -> Expr {
        Expr::Leaf(Leaf::plain(Target::Name, fold(s.as_bytes()), false))
    }

    fn path(s: &str) -> Expr {
        Expr::Leaf(Leaf::plain(Target::FullPath, fold(s.as_bytes()), false))
    }

    fn ext(values: &[&str]) -> Expr {
        Expr::Leaf(Leaf::Ext {
            values: values.iter().map(|v| v.as_bytes().to_vec()).collect(),
            case: false,
            diacritics: false,
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
                Expr::Leaf(Leaf::plain(Target::Name, b"Report".to_vec(), true)),
                Expr::Leaf(Leaf::plain(Target::Name, b"x".to_vec(), true)),
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
                min: 1,
                max: 10 * KB
            }),
            "tiny leaves out empty files, as in Everything"
        );
        assert_eq!(
            q("size:1kb").filters.size,
            Some(SizeRange {
                min: KB,
                max: 2 * KB - 1
            }),
            "a value alone names its last digit's granule"
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
                and([kind(Kind::Folder), ext(&["pdf"])]),
            ])
        );
        assert_eq!(
            q("ext:!pdf").filters.ext,
            Some(vec![b"!pdf".to_vec()]),
            "an extension's ! is text (L7:523)"
        );
        assert_eq!(ex("!ext:pdf"), ext(&["pdf"]).not());
        assert_eq!(
            ex("size:!=1kb"),
            Expr::Leaf(Leaf::Size(SizeRange { min: KB, max: KB })).not()
        );
        assert_eq!(
            ex("ext:pdf|txt"),
            or([ext(&["pdf"]), name("txt")]),
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
            parse_query("report Bogus:foo"),
            Err(QueryError::UnknownFunction("Bogus:".to_string()))
        );
        assert!(
            matches!(
                parse_query("report Parent:foo"),
                Err(QueryError::Malformed {
                    what: "parent:",
                    ..
                })
            ),
            "parent: takes an absolute path"
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
        assert_eq!(
            ex("case:2"),
            Expr::Leaf(Leaf::plain(Target::Name, b"2".to_vec(), true)),
            "case: with text other than 0 or 1 is the per-term modifier"
        );
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

    /// An `ext:` list's plain values are one lookup in a set however many
    /// there are, as a macro's are, so the list counts once toward the cap;
    /// each wildcard value is a test of its own and counts.
    #[test]
    fn ext_lists_count_once_and_their_wildcards_each() {
        let exts = |n: usize, wild: &str| {
            let list: Vec<String> = (0..n).map(|i| format!("e{i}{wild}")).collect();
            format!("ext:{}", list.join(";"))
        };
        assert_eq!(term_count(&q(&format!("a {}", exts(10_000, "")))), 2);
        assert_eq!(
            term_count(&q(&format!("a ext:x {}", exts(10_000, "")))),
            3,
            "a second list is a term of the expression"
        );
        let wild = |n: usize| format!("a ext:x {}", exts(n, "*"));
        assert!(parse_query(&wild(MAX_QUERY_TERMS - 3)).is_ok());
        assert_eq!(
            parse_query(&wild(MAX_QUERY_TERMS - 2)),
            Err(QueryError::TooManyTerms(MAX_QUERY_TERMS + 1))
        );
        assert_eq!(term_count(&q("x audio:")), 2, "a macro's 40 values");
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

    /// Several `ext:`, `size:` and kind filters must all match, as in
    /// Everything (`ext:txt ext:exe` finds nothing, L1:249; `file: folder:`
    /// nothing, L1:110), where v0.6.0 kept the last of each: the first
    /// fills its bucket and the others are terms of the expression, or
    /// `False` for a second, different kind. `dm:` terms all applied
    /// already.
    #[test]
    fn several_filters_all_apply() {
        let p = q("ext:a ext:b size:1 size:2 x");
        assert_eq!(p.filters.ext, Some(vec![b"a".to_vec()]));
        assert_eq!(p.filters.size, Some(SizeRange { min: 1, max: 1 }));
        assert_eq!(
            p.expr,
            Some(and([
                ext(&["b"]),
                Expr::Leaf(Leaf::Size(SizeRange { min: 2, max: 2 }))
            ]))
        );
        let p = q("file: folder: x");
        assert_eq!(p.filters.kind, Some(Kind::File));
        assert_eq!(p.expr, Some(Expr::False), "nothing is both");
        let p = q("file: file: x");
        assert_eq!((p.filters.kind, p.expr), (Some(Kind::File), None));
        assert_eq!(q("<file: x> folder:").expr, Some(Expr::False));
        assert_eq!(
            ex("file:|folder:"),
            or([kind(Kind::File), kind(Kind::Folder)])
        );
    }

    /// A text leaf on `target` with `needle` (folded unless `case`),
    /// anchored and skipping as given.
    fn text(target: Target, needle: &str, case: bool, anchors: Anchors, skip: Skip) -> Expr {
        let needle = if case {
            needle.as_bytes().to_vec()
        } else {
            fold(needle.as_bytes())
        };
        Expr::Leaf(Leaf::Text {
            target,
            needle,
            case,
            diacritics: false,
            anchors,
            skip,
        })
    }

    const FREE: Anchors = Anchors {
        start: false,
        end: false,
        word_start: false,
        word_end: false,
    };
    const NO_SKIP: Skip = Skip {
        punct: false,
        space: false,
    };

    /// `case:` and `nocase:` per term, over the query's default; the
    /// outermost of two that disagree wins (L1:85-97).
    #[test]
    fn case_per_term_and_the_outermost_wins() {
        let foo = |case| text(Target::Name, "Foo", case, FREE, NO_SKIP);
        assert_eq!(ex("case:Foo"), foo(true));
        assert_eq!(ex("::case:Foo"), foo(true));
        assert_eq!(ex("case:nocase:Foo"), foo(true));
        assert_eq!(
            q("nocase:case:Foo").name_terms,
            [b"foo".to_vec()],
            "the query's own case, so the bucket's"
        );
        let p = q("nocase:Foo case:");
        assert!(p.match_case);
        assert_eq!(p.expr, Some(foo(false)), "nocase: under the switch");
        let p = q("no-case:Foo case:");
        assert_eq!(p.expr, Some(foo(false)));
        assert_eq!(
            q("Foo case:").name_terms,
            [b"Foo".to_vec()],
            "the switch still fills the bucket"
        );
        assert_eq!(
            ex("case:<Foo|BAR>"),
            or([foo(true), text(Target::Name, "BAR", true, FREE, NO_SKIP)])
        );
        assert_eq!(
            ex("!case:Foo"),
            ex("case:!Foo"),
            "a modifier reaches through a NOT"
        );
        // `?case:` holds for the terms after it (L1:93).
        assert_eq!(
            q("Foo ?case: Bar").expr,
            Some(text(Target::Name, "Bar", true, FREE, NO_SKIP))
        );
    }

    /// `path:` sends a term to the full path, `nopath:` keeps one with a
    /// separator on the name (where nothing holds a separator, L2:140).
    #[test]
    fn path_and_nopath_per_term() {
        assert_eq!(q("path:words").path_terms, [b"words".to_vec()]);
        assert_eq!(q(r"\:words").path_terms, [b"words".to_vec()]);
        assert_eq!(
            q(r"nopath:windows\cmd.exe").name_terms,
            [br"windows\cmd.exe".to_vec()]
        );
        let p = q("nopath:x matchpath:");
        assert_eq!(p.name_terms, [b"x".to_vec()], "nopath: over matchpath:");
        assert_eq!(q("?path: words ever").path_terms.len(), 2);
    }

    /// The word and anchor modifiers set where a match must stand; a
    /// wildcard matches the whole name and ignores them (L1:145-146,
    /// L2:158).
    #[test]
    fn anchors_and_skips_per_term() {
        let anchored = |start, end, word_start, word_end| Anchors {
            start,
            end,
            word_start,
            word_end,
        };
        let ever = |a| text(Target::Name, "ever", false, a, NO_SKIP);
        for (query, anchors) in [
            ("ww:ever", anchored(false, false, true, true)),
            ("whole-words:ever", anchored(false, false, true, true)),
            ("prefix:ever", anchored(false, false, true, false)),
            ("suffix:ever", anchored(false, false, false, true)),
            ("whole:ever", anchored(true, true, false, false)),
            ("startwith:ever", anchored(true, false, false, false)),
            ("endwith:ever", anchored(false, true, false, false)),
            ("prefix:suffix:ever", anchored(false, false, true, true)),
            ("ww:noprefix:ever", anchored(false, false, true, true)),
            ("noww:prefix:ever", anchored(false, false, true, false)),
        ] {
            assert_eq!(ex(query), ever(anchors), "{query}");
        }
        assert_eq!(
            ex("nopunc:spiderman"),
            text(
                Target::Name,
                "spiderman",
                false,
                FREE,
                Skip {
                    punct: true,
                    space: false
                }
            )
        );
        assert_eq!(
            ex("nows:nopunc:spiderman"),
            text(
                Target::Name,
                "spiderman",
                false,
                FREE,
                Skip {
                    punct: true,
                    space: true
                }
            )
        );
        let p = q("ww:ev*");
        assert!(p.expr.is_none() && p.wildcards[0].matches(b"ever.txt"));
        let p = q("startwith:*ver");
        assert!(p.wildcards[0].matches(b"forever") && !p.wildcards[0].matches(b"ever.txt"));
        assert_eq!(
            q("path:startwith:c:\\users").expr,
            Some(text(
                Target::FullPath,
                r"c:\users",
                false,
                anchored(true, false, false, false),
                NO_SKIP
            ))
        );
    }

    /// `star-all:` lets a sub-path wildcard's `*` cross folders;
    /// `no-wildcards:` makes `*` and `?` text (L7:476-482).
    #[test]
    fn wildcard_switches() {
        let p = q(r"starall:w\*.exe");
        assert!(p.path_wildcards[0].matches(br"c:\fx\w\windows\sub\a.exe"));
        let p = q(r"w\*.exe");
        assert!(!p.path_wildcards[0].matches(br"c:\fx\w\windows\sub\a.exe"));
        assert_eq!(q("nowildcards:*.exe").name_terms, [b"*.exe".to_vec()]);
        assert_eq!(q("no-filename-wildcards:a?b").name_terms, [b"a?b".to_vec()]);
        assert!(q("filename-wildcards:*.exe").wildcards[0].matches(b"a.exe"));
    }

    /// The kind prefixes and their `no-` forms, as a live Everything reads
    /// them: `no-file:` is `folder:`, `no-folder:` is `file:` (L7:452-454,
    /// L8:98-101); `?file:` holds for the terms after it.
    #[test]
    fn kinds_and_their_no_forms() {
        assert_eq!(q("no-file:x"), q("folder:x"));
        assert_eq!(q("nofolders:x"), q("file:x"));
        assert_eq!(ex("nofile:!dot"), ex("folder:!dot"));
        assert_eq!(q("nofolder:!dot"), q("file:!dot"));
        assert_eq!(q("files-only:"), q("file:"));
        assert_eq!(q("folders-only:"), q("folder:"));
        let p = q("?file: dot");
        assert_eq!((p.filters.kind, p.name_terms.len()), (Some(Kind::File), 1));
        assert_eq!(q("?folder: dot ?file: x").expr, Some(Expr::False));
        assert!(matches!(
            parse_query("no-dir:"),
            Err(QueryError::UnknownFunction(_))
        ));
    }

    /// `d:` alone is a drive; `\` or `/` alone keeps folders, quoted and
    /// under any modifier (L7:419-424, L8:102-110); `\\` is a path term.
    #[test]
    fn drive_terms_and_the_lone_separator() {
        assert_eq!(ex("d:"), Expr::Leaf(Leaf::Drive(b'd')));
        assert_eq!(ex("C:"), Expr::Leaf(Leaf::Drive(b'c')));
        assert_eq!(ex("!d:"), Expr::Leaf(Leaf::Drive(b'd')).not());
        assert_eq!(q(r"c:\").path_terms, [br"c:\".to_vec()]);
        assert!(matches!(
            parse_query("c:spider").map(|p| p.name_terms),
            Ok(t) if t == [b"c:spider".to_vec()]
        ));
        for folders in [
            r"\",
            "/",
            r#""\""#,
            r"path:\",
            r"nopath:\",
            r"case:\",
            r"<\>",
        ] {
            let p = q(folders);
            assert_eq!(p.filters.kind, Some(Kind::Folder), "{folders}");
            assert!(p.is_text_empty(), "{folders}");
        }
        assert_eq!(ex(r"!\"), kind(Kind::Folder).not());
        assert_eq!(q(r"\\").path_terms, [br"\\".to_vec()]);
    }

    /// The functions of a full path's parts, each a text leaf on its target
    /// with the same modifiers as a search term; an empty value is any such
    /// text that exists (L7:425-442).
    #[test]
    fn name_part_functions() {
        let on = |target, needle: &str| text(target, needle, false, FREE, NO_SKIP);
        assert_eq!(q("name:Spider").name_terms, [b"spider".to_vec()]);
        assert_eq!(q("basename:x").name_terms, [b"x".to_vec()]);
        assert_eq!(ex("stem:x"), on(Target::Stem, "x"));
        assert_eq!(ex("extension:t"), on(Target::Extension, "t"));
        assert_eq!(q(r"full-path:w\cmd").path_terms, [br"w\cmd".to_vec()]);
        assert_eq!(ex("location:windows"), on(Target::Location, "windows"));
        assert_eq!(ex("pp:windows"), on(Target::Location, "windows"));
        assert_eq!(ex("parent-name:x"), on(Target::ParentName, "x"));
        assert_eq!(
            ex("ww:stem:x"),
            text(
                Target::Stem,
                "x",
                false,
                Anchors {
                    word_start: true,
                    word_end: true,
                    ..FREE
                },
                NO_SKIP
            ),
            "a modifier before the function reaches its value"
        );
        assert_eq!(ex("name:a;b"), or([name("a"), name("b")]), "a list");
        assert_eq!(ex("stem:"), on(Target::Stem, ""));
        assert!(q("name:").is_text_empty());
        assert!(q("location:").is_text_empty());
        assert_eq!(q("name:!x").name_terms, [b"!x".to_vec()], "the ! is text");
        assert!(matches!(
            ex("stem:x*"),
            Expr::Leaf(Leaf::Glob {
                target: Target::Stem,
                ..
            })
        ));
        assert_eq!(ex("whole:"), Expr::False, "no name is empty (L8:131)");
        assert_eq!(ex("whole:\"\""), Expr::False);
        assert_eq!(q("startwith:").expr, None, "everything (L8:133)");
    }

    /// `ext:` values (L1:240-263, L2:102-107, L7:521-535, L8:111-118).
    #[test]
    fn ext_values_as_everything_reads_them() {
        let e = |query: &str| match q(query) {
            ParsedQuery {
                filters: Filters { ext: Some(v), .. },
                expr: None,
                ..
            } => v
                .iter()
                .map(|v| String::from_utf8_lossy(v).into_owned())
                .collect::<Vec<_>>(),
            p => match p.expr {
                Some(Expr::Leaf(Leaf::Ext { values, .. })) => values
                    .iter()
                    .map(|v| String::from_utf8_lossy(v).into_owned())
                    .collect(),
                other => panic!("{query}: {other:?}"),
            },
        };
        assert_eq!(e("ext:TXT;Exe"), ["txt", "exe"]);
        assert_eq!(
            e("ext:.txt;*.pdf;*jpg"),
            ["txt", "pdf", "*jpg"],
            "*jpg a wildcard"
        );
        assert_eq!(e("ext:"), [""], "no extension");
        assert_eq!(e("ext:;"), [""]);
        assert_eq!(e("ext:txt;;exe"), ["txt", "", "exe"]);
        assert_eq!(e("ext:txt;exe;;"), ["txt", "exe"], "trailing empties go");
        assert_eq!(e("ext:txt;TXT"), ["txt"], "each once");
        assert_eq!(e("ext:mp*;t?t"), ["mp*", "t?t"]);
        assert_eq!(e("ext:*.*"), ["*"]);
        assert_eq!(e("ext:!txt"), ["!txt"], "the ! is text (L7:523)");
        assert_eq!(e("ext:tar.gz"), ["tar.gz"]);
        assert_eq!(
            ex("case:ext:TXT"),
            Expr::Leaf(Leaf::Ext {
                values: vec![b"TXT".to_vec()],
                case: true,
                diacritics: false,
            })
        );
        assert_eq!(q("ext:TXT case:").filters.ext, Some(vec![b"txt".to_vec()]));
        assert_eq!(
            ex("ext:<txt|exe>"),
            or([ext(&["txt"]), ext(&["exe"])]),
            "a value group"
        );
        assert_eq!(ex("ext:&ast:"), Expr::False, "no extension holds a *");
        // A macro is an `ext:` list like any other: alone at the top, it
        // fills the filter.
        let audio = q("audio:x");
        assert_eq!(audio.filters.ext.map(|v| v.len()), Some(40));
        assert_eq!((audio.name_terms, audio.expr), (vec![b"x".to_vec()], None));
        let Expr::Or(items) = ex("audio:|x") else {
            panic!()
        };
        assert!(matches!(&items[0], Expr::Leaf(Leaf::Ext { values, .. }) if values.len() == 40));
        assert_eq!(q("pic:"), q("image:"));
    }

    /// Numeric values: `size:` with granules, units, constants, lists and
    /// groups; the lengths; `metric:`; a value's own `!`, which keeps the
    /// entries that have such a value (`extlen:!3` keeps no folder, L8:90).
    #[test]
    fn numeric_functions() {
        let size = |min, max| Expr::Leaf(Leaf::Size(SizeRange { min, max }));
        assert_eq!(
            ex("size:1kb;2kb"),
            or([size(KB, 2 * KB - 1), size(2 * KB, 3 * KB - 1)])
        );
        assert_eq!(ex("size:<1kb|2kb>"), ex("size:1kb;2kb"));
        assert_eq!(ex("size:!1kb"), size(KB, 2 * KB - 1).not());
        assert_eq!(ex("size:!=1kb"), size(KB, KB).not());
        assert_eq!(ex("size:unknown"), Expr::Leaf(Leaf::SizeUnknown));
        assert_eq!(
            q("size:").filters.size,
            Some(SizeRange {
                min: 1,
                max: u64::MAX
            })
        );
        assert_eq!(
            q("metric:size:1kb").filters.size,
            Some(SizeRange {
                min: 1000,
                max: 1999
            })
        );
        assert!(matches!(
            parse_query("size:>=10kb..1mb"),
            Err(QueryError::Malformed { what: "size:", .. })
        ));
        let len = |of, min, max| {
            Expr::Leaf(Leaf::Len {
                of,
                range: SizeRange { min, max },
            })
        };
        assert_eq!(ex("len:7"), len(LenOf::Name, 7, 7));
        assert_eq!(ex("path:len:>100"), len(LenOf::FullPath, 101, u64::MAX));
        assert_eq!(ex("len:7 matchpath:"), len(LenOf::FullPath, 7, 7));
        assert_eq!(ex("namelen:7"), len(LenOf::Name, 7, 7));
        assert_eq!(ex("fullpathlen:<=9"), len(LenOf::FullPath, 0, 9));
        assert_eq!(ex("path-part-length:3"), len(LenOf::Location, 3, 3));
        assert_eq!(ex("extlen:3"), len(LenOf::Extension, 3, 3));
        assert_eq!(ex("len:!7"), len(LenOf::Name, 7, 7).not());
        assert_eq!(
            ex("extlen:!3"),
            and([
                len(LenOf::Extension, 0, u64::MAX),
                len(LenOf::Extension, 3, 3).not()
            ])
        );
        assert_eq!(ex("!extlen:3"), len(LenOf::Extension, 3, 3).not());
        // The kind goes around the value's own NOT: folders whose name is
        // not empty.
        let p = q("folder:len:!0");
        assert_eq!(p.filters.kind, Some(Kind::Folder));
        assert_eq!(p.expr, Some(len(LenOf::Name, 0, 0).not()));
        assert_eq!(
            ex("x|folder:len:!0"),
            or([
                name("x"),
                and([kind(Kind::Folder), len(LenOf::Name, 0, 0).not()])
            ])
        );
        let p = q("size:!1kb");
        assert_eq!(
            p.filters.size,
            Some(SizeRange {
                min: 0,
                max: u64::MAX
            }),
            "a known size"
        );
        assert_eq!(p.expr, Some(size(KB, 2 * KB - 1).not()));
        assert_eq!(
            ex("len:7;10"),
            or([len(LenOf::Name, 7, 7), len(LenOf::Name, 10, 10)])
        );
        assert!(matches!(
            parse_query("len:1kb"),
            Err(QueryError::Malformed { what: "len:", .. })
        ));
    }

    /// The sub-searches (W7, checked against a live Everything 1.5): the
    /// value is a term on the other entry's name under the term's
    /// modifiers, a `;` list one sub-search of any of its values, a leading
    /// `!` text; each spelling with and without its hyphens; at most 8 in a
    /// query.
    #[test]
    fn sub_searches_take_a_term_on_the_name() {
        let rel = |relation, kind, name: Expr| {
            Expr::Leaf(Leaf::Related {
                relation,
                kind,
                term: Box::new(name),
            })
        };
        assert_eq!(ex("child:CMD"), rel(Relation::Child, None, name("cmd")));
        assert_eq!(
            ex("child-file:a;b"),
            rel(
                Relation::Child,
                Some(Kind::File),
                or([name("a"), name("b")])
            )
        );
        assert_eq!(
            ex("childfolder:x"),
            rel(Relation::Child, Some(Kind::Folder), name("x"))
        );
        assert_eq!(
            ex("descendant-folder:x"),
            rel(Relation::Descendant, Some(Kind::Folder), name("x"))
        );
        assert_eq!(
            ex("siblingfile:x"),
            rel(Relation::Sibling, Some(Kind::File), name("x"))
        );
        assert_eq!(
            ex("sibling:!bang"),
            rel(Relation::Sibling, None, name("!bang"))
        );
        assert_eq!(
            ex("ancestorname:"),
            rel(Relation::Ancestor, Some(Kind::Folder), Expr::True)
        );
        // A separator is text: no name holds one.
        assert_eq!(
            ex(r"child:sub\deep"),
            rel(Relation::Child, None, name(r"sub\deep"))
        );
        assert_eq!(
            ex("child:<a|b>"),
            or([
                rel(Relation::Child, None, name("a")),
                rel(Relation::Child, None, name("b"))
            ])
        );
        let Expr::Leaf(Leaf::Related { term: whole, .. }) = ex("whole:child:a") else {
            panic!()
        };
        assert!(
            matches!(*whole, Expr::Leaf(Leaf::Text { anchors, .. }) if anchors.start && anchors.end)
        );
        let Expr::Leaf(Leaf::Related { term: re, .. }) = ex("regex:child:^c") else {
            panic!()
        };
        assert!(matches!(
            *re,
            Expr::Leaf(Leaf::Regex {
                target: Target::Name,
                ..
            })
        ));
        // `regex:` inside the value is text.
        assert_eq!(
            ex("child:regex:^cmd"),
            rel(Relation::Child, None, name("regex:^cmd"))
        );
        assert!(parse_query(&"child:a ".repeat(8)).is_ok());
        assert!(parse_query("child:a;b;c;d;e;f;g;h;i").is_ok());
        assert!(matches!(
            parse_query(&"sibling:a ".repeat(9)),
            Err(QueryError::Malformed {
                what: "sub-search",
                ..
            })
        ));
        // Its terms count toward the regex cap.
        assert!(matches!(
            parse_query(&"regex:child:a ".repeat(9)),
            Err(QueryError::Malformed { what: "regex:", .. })
        ));
    }

    /// The count functions and `empty:` (W6, checked against a live
    /// Everything 1.5): `parent-count:` under its three names, the child
    /// counts, a value's own `!` keeping folders alone for those, and
    /// `empty:` as zero children ANDed with what follows it.
    #[test]
    fn counts_and_empty() {
        let count = |of, min, max| {
            Expr::Leaf(Leaf::Count {
                of,
                range: SizeRange { min, max },
            })
        };
        assert_eq!(ex("parent-count:2"), count(CountOf::Parents, 2, 2));
        assert_eq!(ex("depth:>1"), count(CountOf::Parents, 2, u64::MAX));
        assert_eq!(ex("parents:1..3"), count(CountOf::Parents, 1, 3));
        assert_eq!(ex("ParentCount:<=4"), count(CountOf::Parents, 0, 4));
        assert_eq!(ex("depth:!2"), count(CountOf::Parents, 2, 2).not());
        assert_eq!(ex("child-count:0"), count(CountOf::Children, 0, 0));
        assert_eq!(ex("childfilecount:3"), count(CountOf::ChildFiles, 3, 3));
        assert_eq!(
            ex("child-folder-count:1;4"),
            or([
                count(CountOf::ChildFolders, 1, 1),
                count(CountOf::ChildFolders, 4, 4)
            ])
        );
        assert_eq!(
            ex("child-count:!2"),
            and([
                count(CountOf::Children, 0, u64::MAX),
                count(CountOf::Children, 2, 2).not()
            ])
        );
        assert_eq!(ex("!child-count:2"), count(CountOf::Children, 2, 2).not());
        assert_eq!(ex("empty:"), count(CountOf::Children, 0, 0));
        let p = q("empty:emp");
        assert_eq!(p.expr, Some(count(CountOf::Children, 0, 0)));
        assert_eq!(p.name_terms, vec![b"emp".to_vec()]);
        assert_eq!(
            ex("x|empty:emp"),
            or([
                name("x"),
                and([count(CountOf::Children, 0, 0), name("emp")])
            ])
        );
        assert!(matches!(
            parse_query("depth:1kb"),
            Err(QueryError::Malformed {
                what: "parent-count:",
                ..
            })
        ));
        assert!(matches!(
            parse_query("?empty: a"),
            Err(QueryError::Malformed { what: "empty:", .. })
        ));
        // Still refused: the volume's root is never a result.
        assert_eq!(
            parse_query("root:"),
            Err(QueryError::UnknownFunction("root:".to_string()))
        );
    }

    /// The functions of related entries of W8, checked against a live
    /// Everything 1.5: the four new sub-searches read their value as
    /// `child:` does; the descendant and sibling counts are counts, a
    /// value's own `!` keeping folders alone for the descendant ones only;
    /// the child size and date tests are a `child:` of a size or date, a
    /// list one test, and a value's own `!` keeps the folders holding no
    /// such entry; `total-child-size:` is a size; the aliases Everything
    /// lists read as their functions; what goz's index cannot answer, and
    /// a `$name:` substitution, is refused with why; and each counts toward
    /// the cap of 8.
    #[test]
    fn related_entry_functions_of_w8() {
        use crate::query::dates::tests::{FixedClock, ft};
        let rel = |relation, kind, term: Expr| {
            Expr::Leaf(Leaf::Related {
                relation,
                kind,
                term: Box::new(term),
            })
        };
        let count = |of, min, max| {
            Expr::Leaf(Leaf::Count {
                of,
                range: SizeRange { min, max },
            })
        };
        let size = |min, max| Expr::Leaf(Leaf::Size(SizeRange { min, max }));
        for (query, relation, kind) in [
            ("parent-child:x", Relation::ParentChild, None),
            ("parentchildfile:x", Relation::ParentChild, Some(Kind::File)),
            (
                "parent-child-folder:x",
                Relation::ParentChild,
                Some(Kind::Folder),
            ),
            ("parent-sibling:x", Relation::ParentSibling, None),
            (
                "parent-sibling-file:x",
                Relation::ParentSibling,
                Some(Kind::File),
            ),
            (
                "parentsiblingfolder:x",
                Relation::ParentSibling,
                Some(Kind::Folder),
            ),
            ("ancestor-child:x", Relation::AncestorChild, None),
            (
                "ancestor-child-file:x",
                Relation::AncestorChild,
                Some(Kind::File),
            ),
            (
                "ancestorchildfolder:x",
                Relation::AncestorChild,
                Some(Kind::Folder),
            ),
            ("ancestor-sibling:x", Relation::AncestorSibling, None),
            (
                "ancestorsiblingfile:x",
                Relation::AncestorSibling,
                Some(Kind::File),
            ),
            (
                "ancestor-sibling-folder:x",
                Relation::AncestorSibling,
                Some(Kind::Folder),
            ),
            ("child-name:x", Relation::Child, None),
            ("child-file-name:x", Relation::Child, Some(Kind::File)),
            ("childfoldername:x", Relation::Child, Some(Kind::Folder)),
            ("siblingname:x", Relation::Sibling, None),
            ("sibling-file-name:x", Relation::Sibling, Some(Kind::File)),
            (
                "sibling-folder-name:x",
                Relation::Sibling,
                Some(Kind::Folder),
            ),
        ] {
            assert_eq!(ex(query), rel(relation, kind, name("x")), "{query}");
        }
        assert_eq!(
            ex("parent-sibling:!docs"),
            rel(Relation::ParentSibling, None, name("!docs"))
        );
        assert_eq!(
            ex("ancestor-child:a;b"),
            rel(Relation::AncestorChild, None, or([name("a"), name("b")]))
        );

        assert_eq!(ex("descendant-count:3"), count(CountOf::Descendants, 3, 3));
        assert_eq!(
            ex("descendantfilecount:>2"),
            count(CountOf::DescendantFiles, 3, u64::MAX)
        );
        assert_eq!(
            ex("descendant-folder-count:0"),
            count(CountOf::DescendantFolders, 0, 0)
        );
        // An empty value is any count but 0, as in Everything.
        assert_eq!(ex("sibling-count:"), count(CountOf::Siblings, 1, u64::MAX));
        assert_eq!(ex("siblingfilecount:1"), count(CountOf::SiblingFiles, 1, 1));
        assert_eq!(
            ex("sibling-folder-count:<2"),
            count(CountOf::SiblingFolders, 0, 1)
        );
        assert_eq!(
            ex("descendant-count:!1"),
            and([
                count(CountOf::Descendants, 0, u64::MAX),
                count(CountOf::Descendants, 1, 1).not()
            ])
        );
        // Every entry has siblings to count, so the value's `!` is a NOT.
        assert_eq!(ex("sibling-count:!1"), count(CountOf::Siblings, 1, 1).not());

        let folder_without = |e: Expr| and([kind(Kind::Folder), e.not()]);
        assert_eq!(
            ex("child-file-size:1025"),
            rel(Relation::Child, Some(Kind::File), size(1025, 1025))
        );
        assert_eq!(
            ex("childfilesize:1kb;5000"),
            rel(
                Relation::Child,
                Some(Kind::File),
                or([size(KB, 2 * KB - 1), size(5000, 5000)])
            )
        );
        // At the top level the folders' kind goes to the kind filter.
        let p = q("child-file-size:!0");
        assert_eq!(p.filters.kind, Some(Kind::Folder));
        assert_eq!(
            p.expr,
            Some(rel(Relation::Child, Some(Kind::File), size(0, 0)).not())
        );
        assert_eq!(
            ex("x|child-file-size:!0"),
            or([
                name("x"),
                folder_without(rel(Relation::Child, Some(Kind::File), size(0, 0)))
            ])
        );
        assert_eq!(
            ex("child-file-size:1;!0"),
            or([
                rel(Relation::Child, Some(Kind::File), size(1, 1)),
                folder_without(rel(Relation::Child, Some(Kind::File), size(0, 0)))
            ])
        );
        assert_eq!(
            ex("child-file-size:unknown"),
            rel(
                Relation::Child,
                Some(Kind::File),
                Expr::Leaf(Leaf::SizeUnknown)
            )
        );
        assert_eq!(
            ex("total-child-size:>2kb"),
            Expr::Leaf(Leaf::TotalChildSize(SizeRange {
                min: 2 * KB + 1,
                max: u64::MAX
            }))
        );
        assert_eq!(
            ex("total-child-size:!0"),
            and([
                Expr::Leaf(Leaf::TotalChildSize(SizeRange {
                    min: 0,
                    max: u64::MAX
                })),
                Expr::Leaf(Leaf::TotalChildSize(SizeRange { min: 0, max: 0 })).not()
            ])
        );

        let clock = FixedClock::utc(ft("2026-11-04T10:00:00"));
        let at = |text| {
            parse_query_at(text, false, &clock)
                .unwrap()
                .expr
                .unwrap_or_else(|| panic!("{text} has no expression"))
        };
        let year = |y: &str, next: &str| {
            Expr::Leaf(Leaf::Date(DateFilter::Within {
                min: ft(&format!("{y}-01-01T00:00:00")),
                max: ft(&format!("{next}-01-01T00:00:00")) - 1,
            }))
        };
        for (query, kind) in [
            ("child-date-modified:2025", None),
            ("childdm:2025", None),
            ("child-dm:2025", None),
            ("child-file-date-modified:2025", Some(Kind::File)),
            ("childfiledm:2025", Some(Kind::File)),
            ("child-folder-date-modified:2025", Some(Kind::Folder)),
            ("childfolderdm:2025", Some(Kind::Folder)),
        ] {
            assert_eq!(
                at(query),
                rel(Relation::Child, kind, year("2025", "2026")),
                "{query}"
            );
        }
        assert_eq!(
            at("x|childdm:!2025"),
            or([
                name("x"),
                folder_without(rel(Relation::Child, None, year("2025", "2026")))
            ])
        );
        assert_eq!(parse_query("childdm:2025"), Err(QueryError::NoClock));

        for (query, why) in [
            ("child-size:>1kb", "folder sizes"),
            ("childfoldersize:1", "folder sizes"),
            ("parent-size:1", "folder sizes"),
            ("child-date-created:today", "dates created"),
            ("childdc:2020", "dates created"),
            ("child-file-attrib:h", "attributes"),
            ("ancestor-attr:h", "attributes"),
            ("childrc:today", "recently changed"),
            ("child-occurrence-count:2", "hits"),
            ("descendant-occurrence-count:2", "hits"),
            ("child-count-from-disk:2", "disk"),
            ("file: child-size:1", "folder sizes"),
        ] {
            match parse_query(query) {
                Err(QueryError::Unsupported { what, why: text }) => {
                    assert!(
                        query.contains(what.trim_end_matches(':')),
                        "{query}: {what}"
                    );
                    assert!(text.contains(why), "{query}: {text}");
                }
                other => panic!("{query}: {other:?}"),
            }
        }
        for (query, sub) in [
            ("*.txt !sibling:$stem:.doc", "$stem:"),
            ("stem:.$extension:", "$extension:"),
            ("regex:^(.*)$ sibling-file:$1:", "$1:"),
            ("regex:siblingfile:^$1:.*mp4$", "$1:"),
            ("a$full-path:", "$full-path:"),
        ] {
            match parse_query(query) {
                Err(QueryError::Unsupported { what, .. }) => {
                    assert_eq!(what, format!("the substitution {sub}"), "{query}")
                }
                other => panic!("{query}: {other:?}"),
            }
        }
        // A `$` that names no substitution is text.
        for text in ["$Recycle.Bin", "a$b", "$:x", "price$", "c$ d:"] {
            assert!(parse_query(text).is_ok(), "{text}");
        }

        assert!(parse_query(&"sibling-count:1 ".repeat(8)).is_ok());
        for query in [
            "descendant-count:1 ".repeat(9),
            "ancestor-sibling:a ".repeat(9),
            "child-file-size:1 ".repeat(9),
            "child:a descendant-count:1 parent-child:a parent-sibling:a ancestor-child:a \
             ancestor-sibling:a sibling-count:1 child-file-size:1 descendant:a x"
                .to_string(),
        ] {
            assert!(
                matches!(
                    parse_query(&query),
                    Err(QueryError::Malformed {
                        what: "sub-search",
                        ..
                    })
                ),
                "{query}"
            );
        }
        // A value's own `!` asks for no second set.
        assert!(parse_query(&"descendant-count:!1 ".repeat(8)).is_ok());
    }

    /// `parent:` and `ancestor:` take absolute folders, case-insensitively,
    /// a separator at the end or not, as a list or a group (L1:370-384,
    /// L7:497-509); a relative one is refused.
    #[test]
    fn folder_functions() {
        let parent = |p: &str| {
            Expr::Leaf(Leaf::Parent(vec![FolderPath::folded(
                p.as_bytes().to_vec(),
            )]))
        };
        assert_eq!(ex(r"parent:C:\W\Windows\"), parent(r"c:\w\windows"));
        assert_eq!(ex(r"infolder:C:\W"), parent(r"c:\w"));
        assert_eq!(ex(r"nosubfolders:C:/W"), parent(r"c:\w"));
        assert_eq!(ex(r#"parent:"C:\First Level""#), parent(r"c:\first level"));
        assert_eq!(
            ex(r"parent:C:\a;C:\b"),
            or([parent(r"c:\a"), parent(r"c:\b")])
        );
        assert_eq!(ex(r"parent:<C:\a|C:\b>"), ex(r"parent:C:\a;C:\b"));
        assert_eq!(
            ex(r"ancestor:C:\W"),
            Expr::Leaf(Leaf::Ancestor(vec![FolderPath::folded(br"c:\w".to_vec())]))
        );
        for relative in [
            r"parent:w\windows",
            "parent:windows",
            r"ancestor:\x",
            r"parent:!C:\x",
        ] {
            assert!(
                matches!(parse_query(relative), Err(QueryError::Malformed { .. })),
                "{relative}"
            );
        }
    }

    /// `dm:` lists and groups, and a value's own `!`, through the parser.
    #[test]
    fn date_lists_groups_and_negation() {
        use crate::query::dates::tests::{FixedClock, ft};
        let clock = FixedClock::utc(ft("2026-11-04T10:00:00"));
        let at = |text| parse_query_at(text, false, &clock).unwrap().expr;
        let year = |y: &str| {
            Expr::Leaf(Leaf::Date(DateFilter::Within {
                min: ft(&format!("{y}-01-01T00:00:00")),
                max: ft(&format!("{}-01-01T00:00:00", y.parse::<i64>().unwrap() + 1)) - 1,
            }))
        };
        assert_eq!(at("dm:2020;2024"), Some(or([year("2020"), year("2024")])));
        assert_eq!(at("dm:<2020|2024>"), at("dm:2020;2024"));
        // A date's own `!`: a known date, not that one (L8:90's rule).
        let known = Expr::Leaf(Leaf::Date(DateFilter::Within {
            min: i64::MIN,
            max: i64::MAX,
        }));
        let p = parse_query_at("dm:!2020", false, &clock).unwrap();
        assert_eq!(
            p.filters.date,
            Some(DateFilter::Within {
                min: i64::MIN,
                max: i64::MAX
            })
        );
        assert_eq!(p.expr, Some(year("2020").not()));
        assert_eq!(
            at("!dm:2020"),
            Some(year("2020").not()),
            "the NOT of the term"
        );
        assert_eq!(
            at("dm:2020;!2024"),
            Some(or([year("2020"), and([known, year("2024").not()])]))
        );
        assert!(matches!(
            parse_query_at("dm:<today|>now>", false, &clock),
            Err(QueryError::GroupExpansion { .. })
        ));
    }

    /// The pure-AND guarantee: a query written only with v0.6.0's syntax,
    /// in the spellings whose meaning did not change, builds exactly the
    /// buckets v0.6.0's parser built and no expression, so it runs the same
    /// prefilters and fast paths. The spellings left out are the ones
    /// DESIGN.md §5.3 changes on purpose: path wildcards (C1), quoted
    /// wildcards (C2), and `|`, `!`, `< >`, keyword operators and entities,
    /// which v0.6.0 refused (C3); `ext:` values with a wildcard, an empty
    /// item, an inner dot or a repeat (C4); a second `ext:`, `size:` or
    /// kind (C5); the size constants (C6); `d:` alone (C7); `case:x` (C8);
    /// `lastweek` and its kin (C9). A function's value never holds another
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
                // `ext:` values with no wildcard, empty item or inner dot,
                // each named once: what S2 reads as v0.6.0 did (C4).
                (
                    prop::collection::btree_set("[a-z]{1,4}", 1..4),
                    any::<bool>()
                )
                    .prop_map(|(values, dot)| {
                        let dot = if dot { "." } else { "" };
                        let values: Vec<String> =
                            values.iter().map(|v| format!("{dot}{v}")).collect();
                        format!("ext:{}", values.join(";"))
                    }),
                "ext:\"[a-z]{1,4}\"",
                "(file|folder|dir|FILE):[a-z]{1,4}",
                // Not `path:\` alone, which keeps folders (C7).
                "path:([a-z][a-z\\\\]{0,5})?",
                prop::sample::select(vec![
                    "size:>1mb",
                    "size:1kb..2mb",
                    "size:<=100",
                    "size:1024",
                    "size:abc",
                    "size:>=10kb",
                    "dm:today",
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
                    "dc:x",
                    "Parent:x",
                    "12:30",
                    ":x",
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
                // A second `ext:`, `size:` or kind now ANDs with the first,
                // where v0.6.0 kept the last (C5): keep the first of each.
                let mut seen = [false; 3];
                let parts: Vec<&String> = parts
                    .iter()
                    .filter(|p| {
                        let lower = p.to_ascii_lowercase();
                        let class = if lower.starts_with("ext:") {
                            0
                        } else if lower.starts_with("size:") {
                            1
                        } else if ["file:", "folder:", "dir:"]
                            .iter()
                            .any(|k| lower.starts_with(k))
                        {
                            2
                        } else {
                            return true;
                        };
                        !std::mem::replace(&mut seen[class], true)
                    })
                    .collect();
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

        /// A few folders and files, dotted, dotless, sized and dated.
        fn tiny_index() -> crate::index::VolumeIndex {
            use crate::index::{FrnMap, NTFS_ROOT_FRN, VolumeIndex};
            use crate::types::Frn;
            use crate::usn::record::{
                FILE_ATTRIBUTE_DIRECTORY, ParsedUsnRecord, USN_REASON_FILE_CREATE,
            };
            let mut index = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
            let frn = |n: u64| Frn(n | (1 << 48));
            for (n, parent, name, dir) in [
                (100, None, "a", true),
                (101, Some(100), "a.txt", false),
                (102, None, "b.tar.gz", false),
                (103, Some(100), "c.d", true),
                (104, Some(103), "noext", false),
            ] {
                index.insert_enum(&ParsedUsnRecord {
                    major_version: 2,
                    frn: frn(n),
                    parent_frn: parent.map_or(NTFS_ROOT_FRN, frn),
                    usn: 0,
                    timestamp_ft: 0,
                    reason: USN_REASON_FILE_CREATE,
                    attributes: if dir { FILE_ATTRIBUTE_DIRECTORY } else { 0 },
                    name: name.as_bytes().to_vec(),
                    name_lossy: false,
                });
                index.set_stat(frn(n), n * 10, i64::try_from(n).unwrap_or(0) << 40);
            }
            index
        }

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
                    "?regex:",
                    "regex*:",
                    "noregex:",
                    "(",
                    ")",
                    "[^",
                    "{9999}",
                    "(?<=",
                    "diacritics:",
                    "?diac:",
                    "é",
                    "nocase:",
                    "nopath:",
                    "prefix:",
                    "whole:",
                    "nopunc:",
                    "nows:",
                    "starall:",
                    "nowildcards:",
                    "metric:",
                    "extension:",
                    "len:",
                    "namelen:",
                    "extlen:",
                    "parent:",
                    "ancestor:",
                    "name:",
                    "stem:",
                    "location:",
                    "parent-name:",
                    "audio:",
                    "exe:",
                    "no-file:",
                    "?folder:",
                    "::",
                    "*:",
                    "C:",
                    "d:",
                    "0x",
                    "mtd",
                    "prev",
                    "next2",
                    "coming",
                    "T10:30",
                    "/26",
                    ".5",
                    "2026-11-",
                    "!",
                    "!=",
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
                // What parses must also run, whatever odd values it holds,
                // on volumes mounted the ways the daemon mounts them.
                if let Ok(parsed) = parse_query_at(&input, match_case, &clock) {
                    let index = tiny_index();
                    for prefix in [r"C:\", r"C:\mnt\x\", r"\\?\Volume{0}\"] {
                        let _ = crate::query::run_query(
                            &index,
                            prefix,
                            &parsed,
                            None,
                            crate::types::SortSpec::default(),
                            0,
                            Some(10),
                        );
                    }
                }
            }
        }
    }

    fn regex(target: Target, pattern: &str, case: bool) -> Expr {
        Expr::Leaf(Leaf::Regex {
            target,
            regex: RegexTerm::new(pattern.as_bytes(), case).unwrap(),
        })
    }

    /// `regex:` on the name, or the full path under `path:`, `matchpath:`
    /// or an escaped separator (L1:422-423, L2:168-169); case-insensitive
    /// unless the term or the query matches case (L1:419); never a bucket.
    #[test]
    fn regex_terms_as_everything_reads_them() {
        assert_eq!(
            ex("regex:^foo|^ever"),
            regex(Target::Name, "^foo|^ever", false)
        );
        assert_eq!(
            ex(r"regex:windows\\cmd"),
            regex(Target::FullPath, r"windows\\cmd", false)
        );
        assert_eq!(ex("path:regex:fx"), regex(Target::FullPath, "fx", false));
        assert_eq!(
            ex("matchpath: regex:fx"),
            regex(Target::FullPath, "fx", false)
        );
        assert_eq!(
            ex(r"nopath:regex:a\\b"),
            regex(Target::Name, r"a\\b", false)
        );
        assert_eq!(ex("case:regex:^Foo"), regex(Target::Name, "^Foo", true));
        assert_eq!(ex("regex:^Foo case:"), regex(Target::Name, "^Foo", true));
        assert_eq!(
            ex("nocase:regex:^Foo case:"),
            regex(Target::Name, "^Foo", false)
        );
        assert_eq!(ex("regex:stem:^a|b"), regex(Target::Stem, "^a|b", false));
        assert_eq!(ex("regex*:^a b$"), regex(Target::Name, "^a b$", false));
        assert_eq!(
            ex("?regex: a|b c"),
            and([
                regex(Target::Name, "a|b", false),
                regex(Target::Name, "c", false),
            ])
        );
        let p = q("file:regex:x");
        assert_eq!(
            (p.filters.kind, p.expr),
            (Some(Kind::File), Some(regex(Target::Name, "x", false)))
        );
        // A function whose value is no text reads its group of values as
        // ever under `regex:`.
        assert_eq!(q("?regex: size:<1kb|2kb>"), q("size:<1kb|2kb>"));
        assert_eq!(q("?regex: ext:<jpg|png>"), q("ext:<jpg|png>"));
        assert_eq!(q("regex:ext:<jpg|png>"), q("ext:<jpg|png>"));
        // No wildcard: `*` and `?` are the pattern's.
        assert_eq!(ex("regex:a*"), regex(Target::Name, "a*", false));
        // An empty pattern matches everything.
        assert_eq!(q("regex:"), q(""));
        assert!(matches!(
            parse_query("regex:(?<=a)b"),
            Err(QueryError::BadRegex { .. })
        ));
        assert!(matches!(
            parse_query("regex:a(b"),
            Err(QueryError::BadRegex { .. })
        ));
        let terms = |n: usize| {
            (0..n)
                .map(|i| format!("regex:a{i}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert!(parse_query(&terms(MAX_REGEX_TERMS)).is_ok());
        assert!(matches!(
            parse_query(&terms(MAX_REGEX_TERMS + 1)),
            Err(QueryError::Malformed { what: "regex:", .. })
        ));
    }

    /// Accents are ignored by default and kept under `diacritics:` (L1:169-
    /// 226); `case:` compares the text as stored, accents and all (DESIGN.md
    /// §3.5).
    #[test]
    fn diacritics_keep_accents_per_term() {
        assert_eq!(q("Café").name_terms, vec![b"cafe".to_vec()]);
        assert_eq!(q("Straße").name_terms, vec![b"strasse".to_vec()]);
        let exact = |target, needle: &str| {
            Expr::Leaf(Leaf::Text {
                target,
                needle: fold_exact(needle.as_bytes()),
                case: false,
                diacritics: true,
                anchors: FREE,
                skip: NO_SKIP,
            })
        };
        assert_eq!(ex("diacritics:Café"), exact(Target::Name, "Café"));
        assert_eq!(ex("diac:Straße"), exact(Target::Name, "Straße"));
        assert_eq!(ex(r"diacritics:é\x"), exact(Target::FullPath, r"é\x"));
        assert_eq!(ex("?diacritics: Café"), exact(Target::Name, "Café"));
        assert_eq!(
            q("?diacritics: nodiacritics:Café").name_terms,
            vec![b"cafe".to_vec()]
        );
        assert_eq!(
            ex("case:diacritics:Café"),
            text(Target::Name, "Café", true, FREE, NO_SKIP)
        );
        assert!(matches!(
            ex("diacritics:café*"),
            Expr::Leaf(Leaf::Glob {
                diacritics: true,
                case: false,
                ..
            })
        ));
        assert_eq!(
            ex("diacritics:ext:Ä"),
            Expr::Leaf(Leaf::Ext {
                values: vec![fold_exact("Ä".as_bytes())],
                case: false,
                diacritics: true,
            })
        );
        assert_eq!(q("ext:Ä").filters.ext, Some(vec![b"a".to_vec()]));
    }
}
