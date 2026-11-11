//! `regex:` terms: a regular expression, compiled once at parse time,
//! matched against a name or a full path as it is stored.
//!
//! Everything 1.5 matches a regex against the raw name (no accent folding:
//! `regex:é` does not find a decomposed `é`), case-insensitively unless the
//! term matches case, and against the full path when it is a `path:` term or
//! its text holds an escaped separator (`\\`). goz uses the `regex` crate:
//! its matching is linear in the text, so no pattern can backtrack for
//! ever, but it has no backreferences or look-around, and a pattern using
//! them is refused with the crate's reason. A pattern whose compiled form
//! would pass [`SIZE_LIMIT`] is refused too, which bounds the work of one
//! match on one name; the engine checks the query's budget as the text it
//! has matched grows (see `cexpr`).
//!
//! Memory: a compiled pattern is shared, but every thread that runs it at
//! the same moment keeps caches of its own: the lazy DFA's, one for each
//! direction and each at most [`DFA_LIMIT`], and, where the lazy DFA cannot
//! decide (a Unicode `\b` on a non-ASCII name, say), the PikeVM's or the
//! backtracker's, which grow with the compiled size. The pass over the
//! names runs on a thread per CPU, so a query costs about its regex terms
//! (at most `MAX_REGEX_TERMS`, 8) times the threads times those caches,
//! plus the compiled patterns: 8 of the largest hostile patterns the limits
//! let through, on 16 threads over a mix of ASCII and accented names, peaked
//! at 18 MiB (4 MiB of it compiled), where 32 terms with 1 MiB caches had
//! reached 596 MiB.

use super::parse::QueryError;
use regex::bytes::{Regex, RegexBuilder};
use std::sync::Arc;

/// The most memory one compiled pattern may take: 512 KiB. A big Unicode
/// class repeated is what reaches it: `\w{10}` (ten word characters) is
/// about the largest such repetition that fits, while `(?-u:\w)` and
/// `[a-z0-9_]` are small.
const SIZE_LIMIT: usize = 512 << 10;

/// The most memory each of a pattern's two lazy DFAs (forward and reverse)
/// may keep in its cache, per thread that runs it: 128 KiB. A pattern whose
/// DFA outgrows it is matched by the PikeVM instead, which is slower but
/// linear, and the query's budget bounds the time.
const DFA_LIMIT: usize = 128 << 10;

/// A `regex:` term's pattern, compiled. Two terms are equal when their
/// patterns and case are.
#[derive(Clone)]
pub struct RegexTerm {
    source: String,
    case: bool,
    regex: Arc<Regex>,
}

impl RegexTerm {
    /// Compiles `source` (WTF-8), case-insensitive unless `case`.
    pub(super) fn new(source: &[u8], case: bool) -> Result<RegexTerm, QueryError> {
        let bad = |detail: String| QueryError::BadRegex {
            pattern: crate::wtf8::to_string_lossy(source),
            detail,
        };
        let text = std::str::from_utf8(source)
            .map_err(|_| bad("a regular expression must be valid Unicode".to_string()))?;
        let regex = RegexBuilder::new(text)
            .case_insensitive(!case)
            .unicode(true)
            .size_limit(SIZE_LIMIT)
            .dfa_size_limit(DFA_LIMIT)
            .build()
            .map_err(|e| bad(reason(&e)))?;
        Ok(RegexTerm {
            source: text.to_string(),
            case,
            regex: Arc::new(regex),
        })
    }

    /// The pattern as written.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Whether the pattern matches case.
    pub fn matches_case(&self) -> bool {
        self.case
    }

    /// Whether `text`, as stored, holds a match.
    #[inline]
    pub(super) fn is_match(&self, text: &[u8]) -> bool {
        self.regex.is_match(text)
    }
}

/// The crate's reason for refusing a pattern, on one line: its message's
/// last line, which names the problem (the lines before it repeat the
/// pattern and point at the spot).
fn reason(e: &regex::Error) -> String {
    let message = e.to_string();
    let last = message
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or(&message);
    last.trim().trim_start_matches("error: ").to_string()
}

impl PartialEq for RegexTerm {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source && self.case == other.case
    }
}

impl Eq for RegexTerm {}

impl std::fmt::Debug for RegexTerm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegexTerm")
            .field("source", &self.source)
            .field("case", &self.case)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bad(pattern: &str) -> String {
        match RegexTerm::new(pattern.as_bytes(), false) {
            Err(QueryError::BadRegex { detail, .. }) => detail,
            other => panic!("{pattern}: {other:?}"),
        }
    }

    #[test]
    fn patterns_match_case_insensitively_unless_case() {
        let r = RegexTerm::new(br"^foo\d", false).unwrap();
        assert!(r.is_match(b"FOO1.txt") && !r.is_match(b"afoo1"));
        let r = RegexTerm::new(b"^Foo", true).unwrap();
        assert!(r.is_match(b"Foo.txt") && !r.is_match(b"foo.txt"));
        // Inline flags work, as in Everything (L2:164-165).
        let r = RegexTerm::new(b"(?-i)^A", false).unwrap();
        assert!(!r.is_match(b"a.txt"));
        // Unicode simple case folding, on the raw name: `é` is not `e`.
        let r = RegexTerm::new("é".as_bytes(), false).unwrap();
        assert!(r.is_match("CAFÉ2.txt".as_bytes()) && !r.is_match("cafe\u{301}".as_bytes()));
    }

    #[test]
    fn what_the_crate_cannot_run_is_refused_with_its_reason() {
        assert!(bad("(?<=a)b").contains("look-around"), "{}", bad("(?<=a)b"));
        assert!(
            bad(r"(a)\1").contains("backreferences"),
            "{}",
            bad(r"(a)\1")
        );
        assert!(bad("(").contains("unclosed group"), "{}", bad("("));
        assert!(
            bad(r"\w{1000}{1000}").contains("size limit"),
            "{}",
            bad(r"\w{1000}{1000}")
        );
        // The compiled size is held to 512 KiB (each thread that runs a
        // pattern keeps caches that grow with it): a big Unicode class
        // repeated a few times fits, twenty times does not; small classes
        // fit far longer repetitions.
        assert!(RegexTerm::new(br"\w{8}", false).is_ok());
        assert!(bad(r"\w{20}").contains("size limit"));
        assert!(RegexTerm::new(br"(?-u:\w){200}[a-z0-9_]{200}", false).is_ok());
        let mut lone = Vec::new();
        crate::wtf8::from_utf16(&[0xD800], &mut lone);
        assert!(matches!(
            RegexTerm::new(&lone, false),
            Err(QueryError::BadRegex { .. })
        ));
    }

    #[test]
    fn terms_are_equal_by_pattern_and_case() {
        let a = RegexTerm::new(b"a.c", false).unwrap();
        assert_eq!(a, RegexTerm::new(b"a.c", false).unwrap());
        assert_ne!(a, RegexTerm::new(b"a.c", true).unwrap());
        assert_ne!(a, RegexTerm::new(b"a.d", false).unwrap());
    }
}
