//! Query tokenizer and grammar for goz's v1 search syntax.
//!
//! Supported: space-separated terms (implicit AND), `"quoted phrases"`,
//! `*`/`?` wildcards (anchored to the whole name, or for a path wildcard to
//! the end of the full path, part by part, as Everything 1.5 matches them:
//! see the `glob` module), path terms (a term
//! containing `\` or `/`, matched against the full path with its drive), and
//! the functions `ext:`, `size:`, `dm:`, `path:`, `file:`, `folder:`, `case:`,
//! `matchpath:`, whose value may be quoted. The boolean operators `|` and `!` are rejected
//! loudly rather than silently mis-executed: es's own precedence for them is
//! undocumented, so guessing it would be a compatibility trap. An unknown
//! `func:` prefix is refused too: as text it could match nothing, since no name
//! contains `:`.
//!
//! Terms are folded at parse time when the query is case-insensitive, so the
//! engine compares candidate data (folded iff `!match_case`) against
//! ready-to-use needles. A `dm:` value is resolved here too, to FILETIME
//! bounds, against the [`LocalClock`] the query is parsed with.

use super::dates::{DateFilter, LocalClock, date_filter};
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

/// A fully parsed query.
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
}

impl ParsedQuery {
    /// `true` when there are no positive text constraints: the query matches
    /// everything subject to `filters`.
    pub fn is_text_empty(&self) -> bool {
        self.name_terms.is_empty()
            && self.path_terms.is_empty()
            && self.wildcards.is_empty()
            && self.path_wildcards.is_empty()
    }
}

/// A query that cannot be executed as written.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum QueryError {
    #[error(
        "operator '{0}' is not supported yet; quote it to search for it literally (e.g. \"a{0}b\")"
    )]
    UnsupportedOperator(char),
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
}

/// The most terms one query may hold. The engine tests each candidate against
/// every term under the volume's index read lock, so a query's cost grows with
/// its term count. Its [`super::Budget`] bounds how long it holds the lock, and this
/// cap bounds the work on each candidate, so the budget is checked often.
/// Real queries use a few terms.
const MAX_QUERY_TERMS: usize = 256;

/// How many terms `q` holds, as [`MAX_QUERY_TERMS`] counts them: name and path
/// terms, wildcards and `ext:` values, since each costs a candidate one test.
/// `size:`, `dm:` and the kind filters cost the same whatever their value.
fn term_count(q: &ParsedQuery) -> usize {
    q.name_terms.len()
        + q.path_terms.len()
        + q.wildcards.len()
        + q.path_wildcards.len()
        + q.filters.ext.as_ref().map_or(0, Vec::len)
}

/// Recognizes the `case:` directive: `case:`/`case:1` enable case-sensitivity,
/// `case:0` disables it. Returns `None` for anything else (so it is not
/// mistaken for a search term). Case-insensitive on the function name itself.
fn case_directive(text: &str) -> Option<bool> {
    match text.to_ascii_lowercase().as_str() {
        "case:" | "case:1" => Some(true),
        "case:0" => Some(false),
        _ => None,
    }
}

/// Recognizes the `matchpath:` directive, as `case_directive` does `case:`:
/// the search text tests the full path, not only the name. es's `-p` sends it
/// rather than a request field, so a gozd too old to know it refuses the search
/// instead of answering a different one.
fn match_path_directive(text: &str) -> Option<bool> {
    match text.to_ascii_lowercase().as_str() {
        "matchpath:" | "matchpath:1" => Some(true),
        "matchpath:0" => Some(false),
        _ => None,
    }
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
    let tokens = tokenize(input)?;

    // First pass: detect case-sensitivity so text folding is correct. The
    // last `case:` directive wins over the caller's default.
    let match_case = tokens
        .iter()
        .filter(|t| t.func_colon.is_some())
        .filter_map(|t| case_directive(&t.text))
        .next_back()
        .unwrap_or(match_case);
    let match_path = tokens
        .iter()
        .filter(|t| t.func_colon.is_some())
        .filter_map(|t| match_path_directive(&t.text))
        .next_back()
        .unwrap_or(false);

    let fold_term = |s: &[u8]| -> Vec<u8> { if match_case { s.to_vec() } else { fold(s) } };

    let mut q = ParsedQuery {
        name_terms: Vec::new(),
        path_terms: Vec::new(),
        wildcards: Vec::new(),
        path_wildcards: Vec::new(),
        filters: Filters::default(),
        match_case,
    };

    for tok in &tokens {
        let text = tok.text.as_str();
        if tok.func_colon.is_some()
            && (case_directive(text).is_some() || match_path_directive(text).is_some())
        {
            continue; // the directives are consumed, never search terms
        }

        if let Some(colon) = tok.func_colon {
            let func = text[..colon].to_ascii_lowercase();
            let value = &text[colon + 1..];
            match func.as_str() {
                "ext" => {
                    let exts: Vec<Vec<u8>> = value
                        .split(';')
                        .filter(|s| !s.is_empty())
                        .map(|s| fold(s.trim_start_matches('.').as_bytes()))
                        .collect();
                    if !exts.is_empty() {
                        q.filters.ext = Some(exts);
                    }
                    continue;
                }
                "size" => {
                    q.filters.size = Some(parse_size(value)?);
                    continue;
                }
                "dm" | "datemodified" | "date-modified" => {
                    let date = date_filter(value, clock.ok_or(QueryError::NoClock)?)?;
                    q.filters.date = Some(q.filters.date.map_or(date, |d| d.and(date)));
                    continue;
                }
                "path" => {
                    push_text_term(&mut q, value, true, &fold_term);
                    continue;
                }
                "file" => {
                    q.filters.kind = Some(Kind::File);
                    push_text_term(&mut q, value, match_path, &fold_term);
                    continue;
                }
                "folder" | "dir" => {
                    q.filters.kind = Some(Kind::Folder);
                    push_text_term(&mut q, value, match_path, &fold_term);
                    continue;
                }
                // Not one of the values `match_path_directive` consumed.
                "matchpath" => {
                    return Err(QueryError::Malformed {
                        what: "matchpath:",
                        detail: format!(
                            "expected matchpath:, matchpath:0 or matchpath:1, got matchpath:{value}"
                        ),
                    });
                }
                // Not one of the directives `case_directive` consumed.
                "case" => {
                    return Err(QueryError::Malformed {
                        what: "case:",
                        detail: format!("expected case:, case:0 or case:1, got case:{value}"),
                    });
                }
                // A drive letter, as in `C:\Windows`: a path, not a function.
                f if f.len() == 1 => {}
                f if f.len() > 1 && f.bytes().all(|b| b.is_ascii_alphabetic()) => {
                    return Err(QueryError::UnknownFunction(text[..=colon].to_string()));
                }
                // Not a function name (`12:30`): plain text.
                _ => {}
            }
        }

        push_text_term(&mut q, text, match_path, &fold_term);
    }

    let terms = term_count(&q);
    if terms > MAX_QUERY_TERMS {
        return Err(QueryError::TooManyTerms(terms));
    }
    Ok(q)
}

/// Routes a text term to the name, path or wildcard buckets.
///
/// A term with a separator, or any `path:` value (`as_path`), matches the
/// full path; `/` is a separator like `\`, since no name contains either.
/// `*` and `?` make a wildcard, in quotes too, as in Everything: a name
/// wildcard matches the whole name, and a path wildcard, or one with `**`,
/// the end of the full path (see [`super::glob`]).
fn push_text_term(
    q: &mut ParsedQuery,
    text: &str,
    as_path: bool,
    fold_term: &impl Fn(&[u8]) -> Vec<u8>,
) {
    if text.is_empty() {
        return;
    }
    let is_path = as_path || text.contains(['\\', '/']);
    let wildcard = text.contains(['*', '?']);
    if is_path || (wildcard && text.contains("**")) {
        let path = fold_term(text.replace('/', "\\").as_bytes());
        if wildcard {
            q.path_wildcards.push(Wildcard::sub_path(&path));
        } else {
            q.path_terms.push(path);
        }
    } else if wildcard {
        q.wildcards
            .push(Wildcard::name(&fold_term(text.as_bytes())));
    } else {
        q.name_terms.push(fold_term(text.as_bytes()));
    }
}

struct Token {
    /// The token with its quotes removed.
    text: String,
    /// Where `text` has the colon of a `func:` prefix: the token's first `:`,
    /// when no quote came before it. A quoted value keeps its spaces, as in
    /// `path:"C:\Program Files"`, while a quoted `"a:b"` stays plain text.
    func_colon: Option<usize>,
}

/// Splits on unquoted whitespace, honoring `"double quotes"`, and rejects the
/// unsupported boolean operators when they appear unquoted.
fn tokenize(input: &str) -> Result<Vec<Token>, QueryError> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut token_quoted = false;
    let mut func_colon = None;
    let mut has_content = false;

    for ch in input.chars() {
        if in_quotes {
            if ch == '"' {
                in_quotes = false;
            } else {
                cur.push(ch);
            }
            continue;
        }
        match ch {
            '"' => {
                in_quotes = true;
                token_quoted = true;
                has_content = true;
            }
            // Reject the boolean OR / NOT operators (whose es precedence is
            // undocumented). `<`/`>` are NOT rejected: they are comparison
            // operators inside the `size:` function. es grouping via `<…>`
            // is unsupported and would be treated as literal text.
            '|' | '!' => return Err(QueryError::UnsupportedOperator(ch)),
            c if c.is_whitespace() => {
                if has_content {
                    tokens.push(Token {
                        text: std::mem::take(&mut cur),
                        func_colon: func_colon.take(),
                    });
                    token_quoted = false;
                    has_content = false;
                }
            }
            c => {
                if c == ':' && !token_quoted && func_colon.is_none() {
                    func_colon = Some(cur.len());
                }
                cur.push(c);
                has_content = true;
            }
        }
    }
    if has_content {
        tokens.push(Token {
            text: cur,
            func_colon,
        });
    }
    Ok(tokens)
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

    #[test]
    fn space_separated_terms_are_anded_and_folded() {
        let p = q("Report Invoice");
        assert_eq!(p.name_terms, vec![b"report".to_vec(), b"invoice".to_vec()]);
        assert!(!p.match_case);
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

    #[test]
    fn unsupported_operators_rejected_unquoted_but_literal_when_quoted() {
        assert_eq!(
            parse_query("a|b"),
            Err(QueryError::UnsupportedOperator('|'))
        );
        assert_eq!(
            parse_query("!foo"),
            Err(QueryError::UnsupportedOperator('!'))
        );
        // `<`/`>` are allowed (comparison operators); "a<b" is literal text.
        assert_eq!(q("a<b").name_terms, vec![b"a<b".to_vec()]);
        // Quoted → literal, no rejection.
        assert_eq!(q("\"a|b\"").name_terms, vec![b"a|b".to_vec()]);
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
        assert_eq!(q(r"C:\Windows").path_terms, vec![br"c:\windows".to_vec()]);
        assert_eq!(q("12:30").name_terms, vec![b"12:30".to_vec()]);
        assert_eq!(q(r":\docs").path_terms, vec![br":\docs".to_vec()]);
        assert_eq!(q(r#""a:b""#).name_terms, vec![b"a:b".to_vec()]);
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
        let today = Some(DateFilter::Within {
            min: ft("2026-11-04T00:00:00"),
            max: ft("2026-11-05T00:00:00") - 1,
        });
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
    }

    /// Without a clock a `dm:` term cannot be resolved, so it is refused
    /// rather than read as some other day; a bad value is malformed.
    #[test]
    fn dm_needs_a_clock_and_a_date() {
        assert_eq!(parse_query("dm:today"), Err(QueryError::NoClock));
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
        assert!(matches!(
            parse_query("matchpath:maybe"),
            Err(QueryError::Malformed { .. })
        ));
    }

    mod arbitrary {
        use super::*;
        use proptest::prelude::*;

        /// Query text built from the parser's own syntax and arbitrary
        /// characters, so that functions, ranges and wildcards are reached.
        fn arb_query() -> impl Strategy<Value = String> {
            let fragment = prop_oneof![
                prop::sample::select(vec![
                    "size:", "ext:", "path:", "file:", "folder:", "case:", "case:0", "..", ">=",
                    "<", "=", "*", "?", ";", ".", "\"", " ", "\\", "/", "kb", "gb", "tiny", "dm:",
                    "today", "lastweek", "thisyear", "days", "months", "last", "-", "unknown",
                ])
                .prop_map(str::to_string),
                "[0-9]{1,24}",
                "\\PC{0,4}",
            ];
            prop::collection::vec(fragment, 0..12).prop_map(|parts| parts.concat())
        }

        proptest! {
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
