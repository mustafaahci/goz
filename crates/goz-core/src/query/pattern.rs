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
//!
//! A pattern's literal text is worked out once too ([`Need`]): the pieces
//! every match holds, from the parsed pattern itself, so that a text lacking
//! one is turned away by a `memmem` before the pattern runs, and the pieces
//! of a pattern on the name can drive the scan of the folded names. The
//! regex crate finds no literal of its own in the case variants of a long
//! case-insensitive pattern, and a full-path pattern then ran its lazy DFA
//! over every path.

use super::parse::QueryError;
use memchr::memmem;
use regex::bytes::{Regex, RegexBuilder};
use regex_syntax::hir::{Class, Hir, HirKind};
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
    /// What every match holds, if anything: see [`Need`].
    need: Option<Arc<Need>>,
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
            need: Need::of(text, case).map(Arc::new),
        })
    }

    /// The literal text every match holds, if the pattern has any, in the
    /// form [`Need::folded`] says.
    pub(super) fn need(&self) -> Option<&Need> {
        self.need.as_deref()
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

/// The literal pieces of text every match of a pattern holds, taken from
/// the pattern as the regex crate parses it (same flags, so the same
/// case-insensitive classes), so that a text lacking one can be turned away
/// before the pattern runs, with no answer changed.
///
/// A case-insensitive pattern's pieces are folded as [`crate::fold::fold`]
/// folds names (`folded`): a position of the pattern adds text to a piece
/// only when every character it can match folds to the same ASCII text, as
/// a case class does (`[Kk\u{212A}]` all fold to `k`, `[ßẞ]` to `ss`), and
/// the fold of a name holds that text wherever the name holds such a
/// character, since the fold maps each character on its own and no
/// recomposition reaches an ASCII letter. Any other position (`.`, `[a-z]`,
/// a Hangul syllable, which the fold may recompose with what follows) ends
/// the piece. A pattern that matches case keeps its pieces as stored, a
/// position adding text only when it is one character: such a match holds
/// those very bytes.
///
/// Optional parts and repetitions that may match nothing add nothing; an
/// alternation adds one requirement of any of its branches, when each
/// branch has one.
#[derive(Debug)]
pub(super) struct Need {
    /// The pieces are folded with [`crate::fold::fold`]; else as stored.
    pub(super) folded: bool,
    all: Req,
}

/// A requirement of a [`Need`].
#[derive(Debug)]
enum Req {
    /// The text holds this piece.
    Piece(Box<memmem::Finder<'static>>),
    /// The text meets every one.
    All(Vec<Req>),
    /// The text meets one of them.
    Any(Vec<Req>),
}

/// The shortest piece a [`Need`] keeps: a byte or two is in most texts,
/// and its search would cost more than it saves.
const PIECE_MIN: usize = 3;

/// The most branches an alternation's requirement keeps: past that, testing
/// each branch costs about what the pattern does.
const ANY_MAX: usize = 8;

/// The most characters a class may hold to be read as one position (the
/// case variants of a letter are two or three).
const CLASS_MAX: u32 = 8;

impl Need {
    /// The need of `pattern` matched case-insensitively unless `case`, or
    /// `None` when a match need hold no piece (or the pattern does not
    /// parse here, which [`RegexTerm::new`] has refused already).
    fn of(pattern: &str, case: bool) -> Option<Need> {
        let hir = regex_syntax::ParserBuilder::new()
            .case_insensitive(!case)
            .unicode(true)
            .utf8(false)
            .build()
            .parse(pattern)
            .ok()?;
        let folded = !case;
        Some(Need {
            folded,
            all: Req::of(&hir, folded)?,
        })
    }

    /// Whether `text` (folded when [`Need::folded`], else as stored) meets
    /// every requirement: `false` means no match of the pattern is in it.
    #[inline]
    pub(super) fn holds(&self, text: &[u8]) -> bool {
        self.all.holds(text)
    }

    /// The pieces the text holds whatever branch matches: those of the
    /// top-level requirements, longest first.
    pub(super) fn required_pieces(&self) -> Vec<&[u8]> {
        match &self.all {
            Req::Piece(f) => vec![f.needle()],
            Req::All(items) => items
                .iter()
                .filter_map(|item| match item {
                    Req::Piece(f) => Some(f.needle()),
                    _ => None,
                })
                .collect(),
            Req::Any(_) => Vec::new(),
        }
    }

    /// Needles one of which the text holds: a piece every match holds, or
    /// one for each branch of an alternation. `None` when no set of single
    /// pieces covers every match.
    pub(super) fn needles(&self) -> Option<Vec<Vec<u8>>> {
        self.all.needles()
    }
}

impl Req {
    /// The requirement of `hir`, or `None` when it requires nothing.
    fn of(hir: &Hir, folded: bool) -> Option<Req> {
        let mut walk = Walk {
            folded,
            out: Vec::new(),
            run: Vec::new(),
        };
        walk.seq(hir);
        walk.flush();
        let mut items = walk.out;
        // The longest pieces first: usually the rarest, so the first test
        // turns most texts away.
        items.sort_by_key(|r| std::cmp::Reverse(r.weight()));
        match items.len() {
            0 => None,
            1 => items.pop(),
            _ => Some(Req::All(items)),
        }
    }

    fn holds(&self, text: &[u8]) -> bool {
        match self {
            Req::Piece(f) => f.find(text).is_some(),
            Req::All(items) => items.iter().all(|r| r.holds(text)),
            Req::Any(items) => items.iter().any(|r| r.holds(text)),
        }
    }

    /// How telling the requirement is, to order an AND by.
    fn weight(&self) -> usize {
        match self {
            Req::Piece(f) => f.needle().len(),
            Req::All(items) => items.iter().map(Req::weight).max().unwrap_or(0),
            Req::Any(items) => items.iter().map(Req::weight).min().unwrap_or(0),
        }
    }

    fn needles(&self) -> Option<Vec<Vec<u8>>> {
        match self {
            Req::Piece(f) => Some(vec![f.needle().to_vec()]),
            // The first requirement that gives some: the longest piece.
            Req::All(items) => items.iter().find_map(Req::needles),
            Req::Any(items) => {
                let mut all = Vec::new();
                for item in items {
                    all.extend(item.needles()?);
                }
                (all.len() <= ANY_MAX).then_some(all)
            }
        }
    }
}

/// The walk of a pattern that gathers its requirements: `run` is the piece
/// the positions read so far extend, ended by any position that adds no
/// text.
struct Walk {
    folded: bool,
    out: Vec<Req>,
    run: Vec<u8>,
}

impl Walk {
    /// Ends the current piece, keeping it when it is long enough.
    fn flush(&mut self) {
        let run = std::mem::take(&mut self.run);
        if run.len() >= PIECE_MIN {
            self.out
                .push(Req::Piece(Box::new(memmem::Finder::new(&run).into_owned())));
        }
    }

    /// Reads `hir` as the next part of a sequence.
    fn seq(&mut self, hir: &Hir) {
        match hir.kind() {
            HirKind::Empty => {}
            HirKind::Literal(lit) => self.literal(&lit.0),
            HirKind::Class(class) => match self.class(class) {
                Some(text) => self.run.extend_from_slice(&text),
                None => self.flush(),
            },
            // Zero-width, but no piece need run across one.
            HirKind::Look(_) => self.flush(),
            HirKind::Capture(cap) => self.seq(&cap.sub),
            HirKind::Concat(items) => items.iter().for_each(|h| self.seq(h)),
            HirKind::Repetition(rep) if rep.min == 1 && rep.max == Some(1) => self.seq(&rep.sub),
            HirKind::Repetition(rep) => {
                self.flush();
                if rep.min >= 1
                    && let Some(req) = Req::of(&rep.sub, self.folded)
                {
                    self.out.push(req);
                }
            }
            HirKind::Alternation(branches) => {
                self.flush();
                let reqs: Option<Vec<Req>> =
                    branches.iter().map(|b| Req::of(b, self.folded)).collect();
                if let Some(reqs) = reqs.filter(|r| r.len() <= ANY_MAX) {
                    self.out.push(Req::Any(reqs));
                }
            }
        }
    }

    /// A literal's characters, each one position (and each byte that is no
    /// UTF-8, which `(?-u)` allows).
    fn literal(&mut self, bytes: &[u8]) {
        if !self.folded {
            // Matched as stored: the very bytes.
            self.run.extend_from_slice(bytes);
            return;
        }
        let mut add = |c: Option<char>| match c.and_then(|c| fold_alike(std::iter::once(c))) {
            Some(f) => self.run.extend_from_slice(&f),
            None => self.flush(),
        };
        match std::str::from_utf8(bytes) {
            Ok(text) => text.chars().for_each(|c| add(Some(c))),
            Err(_) => bytes
                .iter()
                .for_each(|&b| add(b.is_ascii().then_some(char::from(b)))),
        }
    }

    /// The text a class adds at its position: folded, the one ASCII text
    /// its characters all fold to; as stored, its one character's bytes.
    fn class(&self, class: &Class) -> Option<Vec<u8>> {
        match class {
            Class::Unicode(cls) => {
                let ranges = cls.ranges();
                let n = class_size(
                    ranges
                        .iter()
                        .map(|r| (u32::from(r.start()), u32::from(r.end()))),
                )?;
                if self.folded {
                    fold_alike(ranges.iter().flat_map(|r| r.start()..=r.end()))
                } else if n == 1 {
                    Some(
                        ranges[0]
                            .start()
                            .encode_utf8(&mut [0; 4])
                            .as_bytes()
                            .to_vec(),
                    )
                } else {
                    None
                }
            }
            Class::Bytes(cls) => {
                let ranges = cls.ranges();
                let n = class_size(
                    ranges
                        .iter()
                        .map(|r| (u32::from(r.start()), u32::from(r.end()))),
                )?;
                if self.folded {
                    // A byte past ASCII is no character the fold reads.
                    if ranges.iter().any(|r| !r.end().is_ascii()) {
                        return None;
                    }
                    fold_alike(
                        ranges
                            .iter()
                            .flat_map(|r| (r.start()..=r.end()).map(char::from)),
                    )
                } else if n == 1 {
                    Some(vec![ranges[0].start()])
                } else {
                    None
                }
            }
        }
    }
}

/// How many characters (or bytes) the inclusive `spans` cover, or `None`
/// past [`CLASS_MAX`].
fn class_size(spans: impl Iterator<Item = (u32, u32)>) -> Option<u32> {
    let mut n = 0u32;
    for (a, b) in spans {
        n += b - a + 1;
        if n > CLASS_MAX {
            return None;
        }
    }
    Some(n)
}

/// The ASCII text every one of `chars` folds to, when they all fold to the
/// same text and it is not empty.
fn fold_alike(chars: impl Iterator<Item = char>) -> Option<Vec<u8>> {
    let mut alike: Option<Vec<u8>> = None;
    for c in chars {
        let f = crate::fold::fold(c.encode_utf8(&mut [0; 4]).as_bytes());
        if f.is_empty() || !f.is_ascii() || alike.as_ref().is_some_and(|a| *a != f) {
            return None;
        }
        alike = Some(f);
    }
    alike
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

    /// The pieces a pattern's matches hold, as a list of strings.
    fn pieces(pattern: &str, case: bool) -> Vec<String> {
        let term = RegexTerm::new(pattern.as_bytes(), case).unwrap();
        term.need().map_or_else(Vec::new, |need| {
            need.required_pieces()
                .iter()
                .map(|p| String::from_utf8(p.to_vec()).unwrap())
                .collect()
        })
    }

    #[test]
    fn the_need_holds_the_literal_runs_folded_as_names_are() {
        // A case class whose characters all fold to one letter extends the
        // piece: `k` matches the Kelvin sign too, which folds to `k`.
        assert_eq!(
            pieces(r"bin_x64_05\\kernel32", false),
            [r"bin_x64_05\kernel32"]
        );
        assert_eq!(pieces("Straße", false), ["strasse"]);
        assert_eq!(pieces("CAFÉ", false), ["cafe"]);
        // The longest first; a run under three bytes is dropped.
        assert_eq!(pieces(r"ab\d+report_v\d", false), ["report_v"]);
        assert_eq!(pieces("colou?r", false), ["colo"]);
        assert_eq!(pieces(r"(foo|bar)bazz", false), ["bazz"]);
        assert_eq!(pieces("a.b", false), Vec::<String>::new());
        // `[a-z]` is no single letter; a Hangul syllable may recompose with
        // what follows it in a name, so it ends a piece.
        assert_eq!(pieces("xyz[a-z]uvw", false), ["xyz", "uvw"]);
        assert_eq!(pieces("가나다abcd", false), ["abcd"]);
        // `(?-i)` in a case-insensitive pattern: still folded, as names are.
        assert_eq!(pieces("(?-i)FooBar", false), ["foobar"]);
        // A pattern that matches case keeps the bytes as stored.
        assert_eq!(pieces(r"Foo\d+Bar", true), ["Foo", "Bar"]);
        assert_eq!(pieces("Straße", true), ["Straße"]);
        // A repetition that may match nothing requires nothing.
        assert_eq!(pieces("(abc)*xyz", false), ["xyz"]);
        assert_eq!(pieces("(abc)+", false), ["abc"]);
        // Branches: a requirement of any, which gives needles but no piece
        // every match holds (`32` alone is too short to keep).
        let term = RegexTerm::new(b"^(kernel|user)32", false).unwrap();
        let need = term.need().unwrap();
        assert!(need.required_pieces().is_empty());
        assert_eq!(
            need.needles(),
            Some(vec![b"kernel".to_vec(), b"user".to_vec()])
        );
        assert!(need.holds(b"x\\user32.dll") && !need.holds(b"kern32.dll"));
        // A branch with nothing to require leaves the alternation none.
        assert!(
            RegexTerm::new(b"^(kernel|.)$", false)
                .unwrap()
                .need()
                .is_none()
        );
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(3000))]

        /// No text a pattern matches lacks what its need says every match
        /// holds, so the prefilter changes no answer: random patterns of
        /// literals, classes, alternations, optional and repeated parts,
        /// anchors and word boundaries, over letters with case variants
        /// beyond ASCII (the Kelvin sign, long s, sharp s, dotted and dotless
        /// i), accents precomposed and not, a ligature, Hangul, and a
        /// separator; texts built mostly from what the pattern matches, in
        /// other cases and spellings.
        #[test]
        fn a_match_always_meets_the_need(
            frags in proptest::collection::vec(frag(), 1..6),
            noise in proptest::collection::vec((0usize..ALPHABET.len(), 0usize..8), 0..6),
            picks in proptest::collection::vec(proptest::prelude::any::<u16>(), 32),
            case in proptest::prelude::any::<bool>(),
        ) {
            let pattern: String = frags.iter().map(Frag::pattern).collect();
            let Ok(term) = RegexTerm::new(pattern.as_bytes(), case) else {
                return Ok(());
            };
            let mut at = 0usize;
            let mut pick = || {
                at += 1;
                usize::from(picks[at % picks.len()])
            };
            let mut text = String::new();
            for &(c, _) in noise.iter().take(noise.len() / 2) {
                text.push_str(ALPHABET[c]);
            }
            for f in &frags {
                f.sample(&mut pick, &mut text);
            }
            for &(c, _) in noise.iter().skip(noise.len() / 2) {
                text.push_str(ALPHABET[c]);
            }
            if !term.is_match(text.as_bytes()) {
                return Ok(());
            }
            if let Some(need) = term.need() {
                let hay = if need.folded {
                    crate::fold::fold(text.as_bytes())
                } else {
                    text.as_bytes().to_vec()
                };
                proptest::prop_assert!(need.holds(&hay), "{pattern:?} on {text:?}");
                if let Some(needles) = need.needles().filter(|_| need.folded) {
                    proptest::prop_assert!(
                        needles.iter().any(|n| memmem::find(&hay, n).is_some()),
                        "{pattern:?} on {text:?}: {needles:?}"
                    );
                }
                for piece in need.required_pieces() {
                    proptest::prop_assert!(memmem::find(&hay, piece).is_some());
                }
            }
        }
    }

    /// What the random patterns are made of: letters with case variants in
    /// and beyond ASCII, accents, a ligature, Hangul and a separator.
    const ALPHABET: &[&str] = &[
        "a", "b", "k", "K", "\u{212A}", "s", "S", "\u{17F}", "ß", "ẞ", "é", "É", "e\u{301}", "e",
        "i", "I", "İ", "ı", "ﬁ", "가", "\u{11A8}", "\\", ".", "_", "1",
    ];

    /// The spellings a pattern's character may meet in a name: its case
    /// variants and its accents decomposed.
    fn variants(c: char) -> Vec<String> {
        let mut out = vec![c.to_string()];
        out.extend(c.to_uppercase().map(String::from));
        out.extend(c.to_lowercase().map(String::from));
        match c {
            'k' | 'K' => out.push("\u{212A}".into()),
            's' | 'S' => out.push("\u{17F}".into()),
            'ß' => out.push("ẞ".into()),
            'é' => out.push("e\u{301}".into()),
            _ => {}
        }
        out
    }

    #[derive(Clone, Debug)]
    enum Frag {
        Lit(String),
        Dot,
        Class(String, String),
        Alt(String, String),
        Opt(Box<Frag>),
        Plus(Box<Frag>),
        Star(Box<Frag>),
        Two(Box<Frag>),
        Start,
        End,
        Word,
    }

    fn word() -> impl proptest::strategy::Strategy<Value = String> {
        use proptest::strategy::Strategy;
        proptest::collection::vec(0usize..ALPHABET.len(), 1..6)
            .prop_map(|cs| cs.iter().map(|&c| ALPHABET[c]).collect())
    }

    fn one() -> impl proptest::strategy::Strategy<Value = String> {
        use proptest::strategy::Strategy;
        (0usize..ALPHABET.len()).prop_map(|c| {
            // A class holds characters, not sequences.
            ALPHABET[c].chars().next().unwrap().to_string()
        })
    }

    fn frag() -> impl proptest::strategy::Strategy<Value = Frag> {
        use proptest::prop_oneof;
        use proptest::strategy::Strategy;
        let leaf = prop_oneof![
            4 => word().prop_map(Frag::Lit),
            1 => proptest::strategy::Just(Frag::Dot),
            1 => (one(), one()).prop_map(|(a, b)| Frag::Class(a, b)),
            2 => (word(), word()).prop_map(|(a, b)| Frag::Alt(a, b)),
            1 => proptest::strategy::Just(Frag::Start),
            1 => proptest::strategy::Just(Frag::End),
            1 => proptest::strategy::Just(Frag::Word),
        ];
        leaf.prop_recursive(2, 6, 1, |inner| {
            prop_oneof![
                inner.clone().prop_map(|f| Frag::Opt(Box::new(f))),
                inner.clone().prop_map(|f| Frag::Plus(Box::new(f))),
                inner.clone().prop_map(|f| Frag::Star(Box::new(f))),
                inner.prop_map(|f| Frag::Two(Box::new(f))),
            ]
        })
    }

    impl Frag {
        fn pattern(&self) -> String {
            match self {
                Frag::Lit(s) => regex_syntax::escape(s),
                Frag::Dot => ".".into(),
                Frag::Class(a, b) => {
                    format!("[{}{}]", regex_syntax::escape(a), regex_syntax::escape(b))
                }
                Frag::Alt(a, b) => format!(
                    "(?:{}|{})",
                    regex_syntax::escape(a),
                    regex_syntax::escape(b)
                ),
                Frag::Opt(f) => format!("(?:{})?", f.pattern()),
                Frag::Plus(f) => format!("(?:{})+", f.pattern()),
                Frag::Star(f) => format!("(?:{})*", f.pattern()),
                Frag::Two(f) => format!("(?:{}){{2}}", f.pattern()),
                Frag::Start => "^".into(),
                Frag::End => "$".into(),
                Frag::Word => r"\b".into(),
            }
        }

        /// Appends text this part may match, in a spelling `pick` chooses.
        fn sample(&self, pick: &mut impl FnMut() -> usize, out: &mut String) {
            fn spell(s: &str, pick: &mut impl FnMut() -> usize, out: &mut String) {
                for c in s.chars() {
                    let v = variants(c);
                    out.push_str(&v[pick() % v.len()]);
                }
            }
            match self {
                Frag::Lit(s) => spell(s, pick, out),
                Frag::Dot => out.push_str(ALPHABET[pick() % ALPHABET.len()]),
                Frag::Class(a, b) | Frag::Alt(a, b) => {
                    let which = if pick().is_multiple_of(2) { a } else { b };
                    spell(which, pick, out);
                }
                Frag::Opt(f) => {
                    if pick().is_multiple_of(2) {
                        f.sample(pick, out);
                    }
                }
                Frag::Plus(f) => (0..1 + pick() % 2).for_each(|_| f.sample(pick, out)),
                Frag::Star(f) => (0..pick() % 3).for_each(|_| f.sample(pick, out)),
                Frag::Two(f) => (0..2).for_each(|_| f.sample(pick, out)),
                Frag::Start | Frag::End | Frag::Word => {}
            }
        }
    }

    #[test]
    fn terms_are_equal_by_pattern_and_case() {
        let a = RegexTerm::new(b"a.c", false).unwrap();
        assert_eq!(a, RegexTerm::new(b"a.c", false).unwrap());
        assert_ne!(a, RegexTerm::new(b"a.c", true).unwrap());
        assert_ne!(a, RegexTerm::new(b"a.d", false).unwrap());
    }
}
