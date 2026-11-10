//! goz v0.6.0's query parser, kept verbatim but for the types it fills,
//! for the test that holds the new parser to it: a query written only with
//! v0.6.0's syntax, in the spellings whose meaning did not change, must
//! build the same buckets and no expression, so it runs exactly as before.

use super::dates::{LocalClock, date_filter};
use super::parse::{Filters, Kind, QueryError, SizeRange};
use crate::fold::fold;

/// v0.6.0's `ParsedQuery`, each wildcard kept as the bytes it compiled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct OldQuery {
    pub(super) name_terms: Vec<Vec<u8>>,
    pub(super) path_terms: Vec<Vec<u8>>,
    pub(super) wildcards: Vec<Vec<u8>>,
    pub(super) path_wildcards: Vec<Vec<u8>>,
    pub(super) filters: Filters,
    pub(super) match_case: bool,
}

const MAX_QUERY_TERMS: usize = 256;

/// v0.6.0's `parse_query_at`.
pub(super) fn parse_v060(
    input: &str,
    match_case: bool,
    clock: &dyn LocalClock,
) -> Result<OldQuery, QueryError> {
    parse(input, match_case, Some(clock))
}

fn term_count(q: &OldQuery) -> usize {
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

fn parse(
    input: &str,
    match_case: bool,
    clock: Option<&dyn LocalClock>,
) -> Result<OldQuery, QueryError> {
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

    let mut q = OldQuery {
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
                    push_text_term(&mut q, value, tok.quoted, true, &fold_term);
                    continue;
                }
                "file" => {
                    q.filters.kind = Some(Kind::File);
                    push_text_term(&mut q, value, tok.quoted, match_path, &fold_term);
                    continue;
                }
                "folder" | "dir" => {
                    q.filters.kind = Some(Kind::Folder);
                    push_text_term(&mut q, value, tok.quoted, match_path, &fold_term);
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

        push_text_term(&mut q, text, tok.quoted, match_path, &fold_term);
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
/// `*` and `?` make a wildcard, matched against the whole name or, for a path,
/// the whole full path. Quotes keep them literal in a name, but in a path they
/// stay wildcards: the quotes are there for the spaces in a folder name.
fn push_text_term(
    q: &mut OldQuery,
    text: &str,
    quoted: bool,
    as_path: bool,
    fold_term: &impl Fn(&[u8]) -> Vec<u8>,
) {
    if text.is_empty() {
        return;
    }
    let is_path = as_path || text.contains(['\\', '/']);
    let wildcard = text.contains(['*', '?']) && (is_path || !quoted);
    if is_path {
        let path = fold_term(text.replace('/', "\\").as_bytes());
        if wildcard {
            q.path_wildcards.push(path);
        } else {
            q.path_terms.push(path);
        }
    } else if wildcard {
        q.wildcards.push(fold_term(text.as_bytes()));
    } else {
        q.name_terms.push(fold_term(text.as_bytes()));
    }
}

struct Token {
    /// The token with its quotes removed.
    text: String,
    /// Whether any part of it was quoted.
    quoted: bool,
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
            '|' | '!' => {
                return Err(QueryError::Malformed {
                    what: "operator",
                    detail: ch.to_string(),
                });
            }
            c if c.is_whitespace() => {
                if has_content {
                    tokens.push(Token {
                        text: std::mem::take(&mut cur),
                        quoted: token_quoted,
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
            quoted: token_quoted,
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
