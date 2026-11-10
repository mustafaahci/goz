//! Wildcard patterns, matched as Everything 1.5 matches them.
//!
//! A pattern with no separator and no `**` is a *name* pattern, matched
//! against the whole name: `*` takes any run of code points, `?` exactly one.
//!
//! A pattern with a separator, a `**`, or from `path:` is a *sub-path*
//! pattern, matched against the end of the full path, part by part: its `*`
//! and `?` never take a `\`, its `**` takes anything, and the match starts at
//! any part boundary and runs to the end. So `windows\*.exe` finds
//! `C:\Windows\notepad.exe` and `D:\old\windows\a.exe`, but not
//! `C:\mywindows\a.exe` (the part must be `windows`, whole) nor
//! `C:\Windows\System32\cmd.exe` (the `*` cannot take `System32\`), and
//! `windows\**\*.exe` finds the second. A leading `\` adds nothing, as a
//! match already starts at a part boundary, and is dropped; a trailing `\`
//! means "below it" and becomes `\**`. Everything's documentation and its
//! author's forum answers state these rules, and a live Everything 1.5
//! confirmed each of them (DESIGN.md §2.2 of the query-language work).
//!
//! A sub-path pattern without `**` covers exactly as many parts as it has, so
//! it is matched by splitting the last parts of the path off and matching
//! each against its piece of the pattern with the name matcher. One with
//! `**` cannot be: a `*` that meets a `\` must hand the text back to an
//! earlier `**`, which the name matcher's single backtrack point cannot do,
//! so it runs a bit-parallel automaton over the pattern instead, whose cost
//! is linear in the text whatever the pattern.

use super::budget::Budget;
use crate::wtf8::encode_code_point;
use smallvec::SmallVec;

/// Scratch buffer of decoded code points, reused across candidates so a
/// wildcard scan doesn't heap-allocate per candidate. NTFS names are <= 255
/// code points, so a name fits inline; a full path may spill it once, and
/// `clear` keeps that capacity for the rest of the scan.
pub type CodePointBuf = SmallVec<[u32; 256]>;

/// Decodes `text` (WTF-8) into `buf`, cleared first, for
/// [`Wildcard::matches_decoded`]: once per candidate, however many
/// wildcards test it.
pub(crate) fn decode_into(text: &[u8], buf: &mut CodePointBuf) {
    buf.clear();
    buf.extend(crate::wtf8::CodePoints::new(text));
}

/// The path separator as a code point. A pattern's `/` is stored as `\`.
const SEP: u32 = b'\\' as u32;

/// About how many steps a match takes between checks of its budget: well
/// under a millisecond's worth.
const GLOB_STEPS_PER_CHECK: usize = 1 << 16;

/// A match that may take this many steps checks its budget before it starts.
/// That costs a 24 ns clock read against a few microseconds of steps, and it
/// holds what 256 terms can do to one candidate between checks to about a
/// millisecond.
const GLOB_COSTLY_STEPS: usize = 1 << 12;

/// A compiled wildcard pattern: a name pattern, matched against a whole
/// name, or a sub-path pattern, matched against the end of a full path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wildcard {
    /// The pattern as compiled, WTF-8, folded iff the query is
    /// case-insensitive: for a sub-path pattern, `/` written as `\`, a leading
    /// `\` dropped and a trailing one followed by `**`.
    source: Vec<u8>,
    /// The pattern's tokens, which the literal helpers read.
    toks: Vec<Tok>,
    shape: Shape,
}

/// One token of a pattern.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tok {
    /// A code point that matches itself; in a sub-path pattern, `\` is one.
    Lit(u32),
    /// `?`: one code point, never a `\` in a sub-path pattern.
    One,
    /// `*`: any run of code points, never holding a `\` in a sub-path
    /// pattern.
    Run,
    /// `**` in a sub-path pattern: any run, `\` included.
    RunAll,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Shape {
    /// A name pattern.
    Name(Glob),
    /// A sub-path pattern without `**`: one piece per path part, last part
    /// last.
    Parts(Vec<Glob>),
    /// A sub-path pattern with `**`.
    Nfa(Nfa),
}

impl Wildcard {
    /// A name pattern, from bytes already folded iff the query is
    /// case-insensitive: `*` any run, `?` one code point, matched against
    /// the whole name.
    pub(crate) fn name(pattern: &[u8]) -> Self {
        let mut toks = Vec::new();
        for cp in crate::wtf8::CodePoints::new(pattern) {
            match cp {
                // '*': collapse runs of '*' into one.
                0x2A => {
                    if toks.last() != Some(&Tok::Run) {
                        toks.push(Tok::Run);
                    }
                }
                0x3F => toks.push(Tok::One),
                other => toks.push(Tok::Lit(other)),
            }
        }
        Wildcard {
            source: pattern.to_vec(),
            shape: Shape::Name(Glob::new(&toks)),
            toks,
        }
    }

    /// A sub-path pattern, from bytes already folded iff the query is
    /// case-insensitive, with `/` already written as `\`.
    pub(crate) fn sub_path(pattern: &[u8]) -> Self {
        let pattern = pattern.strip_prefix(b"\\").unwrap_or(pattern);
        let mut source = pattern.to_vec();
        if source.last() == Some(&b'\\') {
            source.extend_from_slice(b"**");
        }
        let mut toks = Vec::new();
        let mut stars = 0usize;
        let flush = |stars: &mut usize, toks: &mut Vec<Tok>| {
            match *stars {
                0 => {}
                1 => toks.push(Tok::Run),
                _ => toks.push(Tok::RunAll),
            }
            *stars = 0;
        };
        for cp in crate::wtf8::CodePoints::new(&source) {
            if cp == 0x2A {
                stars += 1;
                continue;
            }
            flush(&mut stars, &mut toks);
            toks.push(match cp {
                0x3F => Tok::One,
                other => Tok::Lit(other),
            });
        }
        flush(&mut stars, &mut toks);
        let shape = if toks.contains(&Tok::RunAll) {
            Shape::Nfa(Nfa::new(&toks))
        } else {
            Shape::Parts(toks.split(|t| *t == Tok::Lit(SEP)).map(Glob::new).collect())
        };
        Wildcard {
            source,
            toks,
            shape,
        }
    }

    /// Whether this is a name pattern, matched against the whole name.
    pub fn is_name(&self) -> bool {
        matches!(self.shape, Shape::Name(_))
    }

    /// The pattern as compiled (see [`Wildcard`]'s `source`).
    pub fn pattern(&self) -> &[u8] {
        &self.source
    }

    /// Matches `text`, which [`decode_into`] decoded: the whole of a name for
    /// a name pattern, the end of a full path for a sub-path pattern. Answers
    /// `false` once `budget` is spent, as the query is then refused whatever
    /// this answers.
    pub(crate) fn matches_decoded(&self, text: &[u32], budget: &Budget) -> bool {
        match &self.shape {
            Shape::Name(glob) => glob.matches_decoded(text, budget),
            Shape::Parts(parts) => parts_match(parts, text, budget),
            Shape::Nfa(nfa) => nfa.matches(text, budget),
        }
    }

    /// Matches `text` (WTF-8, folded iff the query is case-insensitive): a
    /// name for a name pattern, a full path for a sub-path pattern.
    /// Convenience wrapper that allocates a one-off buffer; the search
    /// decodes each candidate once for all its wildcards instead.
    pub fn matches(&self, text: &[u8]) -> bool {
        let mut buf = CodePointBuf::new();
        decode_into(text, &mut buf);
        self.matches_decoded(&buf, &Budget::unlimited())
    }

    /// The longest run of literal characters in the pattern, as WTF-8 bytes
    /// (already folded iff the query is case-insensitive). A name matching a
    /// name pattern must contain this substring, so it can drive a prefilter
    /// scan. `None` if the pattern is all wildcards (e.g. `*`, `?`).
    pub fn longest_literal(&self) -> Option<Vec<u8>> {
        longest_run(&self.toks)
    }

    /// Each maximal run of literal characters holding no `\`, as WTF-8 bytes
    /// (already folded iff the query is case-insensitive). A full path the
    /// pattern matches holds each run inside one of its names or inside the
    /// mount prefix, since a literal matches only itself.
    pub(crate) fn separator_free_literals(&self) -> Vec<Vec<u8>> {
        let mut runs = Vec::new();
        let mut run = Vec::new();
        for tok in &self.toks {
            match tok {
                Tok::Lit(cp) if *cp != SEP => encode_code_point(*cp, &mut run),
                _ if !run.is_empty() => runs.push(std::mem::take(&mut run)),
                _ => {}
            }
        }
        if !run.is_empty() {
            runs.push(run);
        }
        runs
    }

    /// The literal characters before the pattern's first wildcard, as WTF-8
    /// bytes: what a name pattern's match starts with, and what a sub-path
    /// pattern's match starts with, wherever that start lies.
    pub fn leading_literal(&self) -> Vec<u8> {
        let mut lit = Vec::new();
        for tok in &self.toks {
            let Tok::Lit(cp) = tok else { break };
            encode_code_point(*cp, &mut lit);
        }
        lit
    }

    /// The literal characters after the pattern's last wildcard, as WTF-8
    /// bytes: what every match ends with, a name or a full path.
    pub(crate) fn trailing_literal(&self) -> Vec<u8> {
        let start = self
            .toks
            .iter()
            .rposition(|t| !matches!(t, Tok::Lit(_)))
            .map_or(0, |i| i + 1);
        let mut lit = Vec::new();
        for tok in &self.toks[start..] {
            if let Tok::Lit(cp) = tok {
                encode_code_point(*cp, &mut lit);
            }
        }
        lit
    }
}

/// The longest run of literal tokens in `toks`, as WTF-8 bytes.
fn longest_run(toks: &[Tok]) -> Option<Vec<u8>> {
    let mut best: Vec<u8> = Vec::new();
    let mut run: Vec<u8> = Vec::new();
    for tok in toks {
        match tok {
            Tok::Lit(cp) => encode_code_point(*cp, &mut run),
            _ => {
                if run.len() > best.len() {
                    best = std::mem::take(&mut run);
                }
                run.clear();
            }
        }
    }
    if run.len() > best.len() {
        best = run;
    }
    (!best.is_empty()).then_some(best)
}

/// Whether the last `parts.len()` parts of `text` match `parts`, one by
/// one: a sub-path pattern of `k` separators covers exactly the last `k + 1`
/// parts of a full path, which fixes where its match starts. The name, the
/// last part and usually the most selective, goes first.
fn parts_match(parts: &[Glob], text: &[u32], budget: &Budget) -> bool {
    let mut end = text.len();
    for (i, part) in parts.iter().enumerate().rev() {
        let start = match text[..end].iter().rposition(|&c| c == SEP) {
            Some(sep) => sep + 1,
            // The first part of the pattern may be the first of the path.
            None if i == 0 => 0,
            None => return false,
        };
        if !part.matches_decoded(&text[start..end], budget) {
            return false;
        }
        // The separator before this part ends the one before it.
        end = start.saturating_sub(1);
    }
    true
}

/// A pattern of `*`, `?` and literals with no separator to respect: a name
/// pattern, or one part of a sub-path pattern without `**`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Glob {
    tokens: Vec<GlobTok>,
    /// The code points a match has at least: one per literal and `?`.
    min_len: usize,
    /// Whether the pattern holds a `*`. Without one, a match has exactly
    /// `min_len` code points.
    any_run: bool,
    /// [`Glob::glob_match`] checks its budget each time the text position
    /// it backtracks to has these low bits clear: about every
    /// [`GLOB_STEPS_PER_CHECK`] steps, as a backtrack starts a pass of at most
    /// one step per token.
    check_mask: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum GlobTok {
    Lit(u32),
    /// `?`: exactly one code point.
    AnyOne,
    /// `*`: any run of code points (including empty).
    AnyRun,
}

impl Glob {
    /// The matcher of `toks`, which hold no `**` and, for a part of a
    /// sub-path pattern, no separator.
    fn new(toks: &[Tok]) -> Self {
        let tokens: Vec<GlobTok> = toks
            .iter()
            .map(|t| match *t {
                Tok::Lit(cp) => GlobTok::Lit(cp),
                Tok::One => GlobTok::AnyOne,
                Tok::Run | Tok::RunAll => GlobTok::AnyRun,
            })
            .collect();
        let runs = tokens.iter().filter(|t| **t == GlobTok::AnyRun).count();
        Glob {
            min_len: tokens.len() - runs,
            any_run: runs > 0,
            check_mask: (GLOB_STEPS_PER_CHECK / (tokens.len() + 1)).next_power_of_two() - 1,
            tokens,
        }
    }

    /// Matches the whole of `text`. Answers `false` once `budget` is spent.
    ///
    /// A text that cannot fit is turned away without a walk. A match that may
    /// take many steps checks the budget first: [`Glob::glob_match`] checks
    /// only every so many, so a search of many terms over texts too short to
    /// reach a check of its own ran seconds past its budget.
    pub(crate) fn matches_decoded(&self, text: &[u32], budget: &Budget) -> bool {
        let fits = if self.any_run {
            text.len() >= self.min_len
        } else {
            text.len() == self.min_len
        };
        // Each code point the `*`s can take is a backtrack, and a pass of at
        // most one step per token.
        let costly = || (text.len() - self.min_len) * self.tokens.len() >= GLOB_COSTLY_STEPS;
        fits && !(costly() && budget.check()) && self.glob_match(text, budget)
    }

    /// Iterative glob match with backtracking on the last `*`. Each backtrack
    /// starts a pass of at most one step per pattern token, so a pattern of
    /// `k` tokens takes up to `k` steps per code point of the text: fast for
    /// real patterns, but one built to backtrack against a long text can take
    /// minutes, so `budget` is checked as the backtracks add up, and a spent
    /// one ends the match unmatched.
    fn glob_match(&self, text: &[u32], budget: &Budget) -> bool {
        let pat = self.tokens.as_slice();
        let (mut ti, mut pi) = (0usize, 0usize);
        let mut star: Option<(usize, usize)> = None; // (pattern idx after '*', text idx)
        while ti < text.len() {
            match pat.get(pi) {
                Some(GlobTok::Lit(c)) if *c == text[ti] => {
                    pi += 1;
                    ti += 1;
                }
                Some(GlobTok::AnyOne) => {
                    pi += 1;
                    ti += 1;
                }
                Some(GlobTok::AnyRun) => {
                    // A last `*` takes all that is left.
                    if pi + 1 == pat.len() {
                        return true;
                    }
                    star = Some((pi + 1, ti));
                    pi += 1;
                }
                _ => {
                    // Mismatch: backtrack to the last '*' and consume one more char.
                    if let Some((sp, st)) = star {
                        let next = st + 1;
                        if next & self.check_mask == 0 && budget.check() {
                            return false;
                        }
                        pi = sp;
                        ti = next;
                        star = Some((sp, next));
                    } else {
                        return false;
                    }
                }
            }
        }
        while let Some(GlobTok::AnyRun) = pat.get(pi) {
            pi += 1;
        }
        pi == pat.len()
    }
}

/// A sub-path pattern with `**`, as a nondeterministic automaton run on all
/// its states at once: state `i` means "the first `i` tokens matched", one
/// bit each, 64 to a word. Each code point of the text moves every live state
/// in a few word operations, so a match costs the text's length times the
/// words, whatever the pattern, with no backtracking to blow up.
///
/// A match may start at any part boundary, so the start state is switched on
/// again after every separator: one pass tries every start.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Nfa {
    /// The tokens; the accepting state is `len`.
    len: usize,
    words: usize,
    /// States whose token is `*` or `**`: they stay live on a code point.
    star: Vec<u64>,
    /// States whose token is `**`: they stay live on a separator too.
    star_all: Vec<u64>,
    /// States whose token is `?`.
    one: Vec<u64>,
    /// For each ASCII code point, the states whose token is that literal:
    /// `words` words per code point.
    ascii: Vec<u64>,
    /// The same for the other code points the pattern holds, sorted.
    other: Vec<(u32, Vec<u64>)>,
    /// What no literal matches.
    none: Vec<u64>,
    /// The start state, and the state after it when the first token is a
    /// star, which may take nothing.
    start: Vec<u64>,
    /// [`Nfa::matches`] checks its budget each time the text position has
    /// these low bits all set.
    check_mask: usize,
}

impl Nfa {
    fn new(toks: &[Tok]) -> Self {
        let len = toks.len();
        let words = (len + 1).div_ceil(64);
        let zero = vec![0u64; words];
        let set = |mask: &mut [u64], i: usize| mask[i / 64] |= 1 << (i % 64);
        let (mut star, mut star_all, mut one) = (zero.clone(), zero.clone(), zero.clone());
        let mut ascii = vec![0u64; 128 * words];
        let mut other: Vec<(u32, Vec<u64>)> = Vec::new();
        for (i, tok) in toks.iter().enumerate() {
            match *tok {
                Tok::Lit(cp) if cp < 128 => set(&mut ascii[cp as usize * words..][..words], i),
                Tok::Lit(cp) => match other.binary_search_by_key(&cp, |(c, _)| *c) {
                    Ok(at) => set(&mut other[at].1, i),
                    Err(at) => {
                        let mut mask = zero.clone();
                        set(&mut mask, i);
                        other.insert(at, (cp, mask));
                    }
                },
                Tok::One => set(&mut one, i),
                Tok::Run => set(&mut star, i),
                Tok::RunAll => {
                    set(&mut star, i);
                    set(&mut star_all, i);
                }
            }
        }
        let mut start = zero.clone();
        set(&mut start, 0);
        close(&mut start, &star);
        Nfa {
            len,
            words,
            star,
            star_all,
            one,
            ascii,
            other,
            none: zero,
            start,
            check_mask: (GLOB_STEPS_PER_CHECK / words).next_power_of_two() - 1,
        }
    }

    /// The states whose token is the literal `c`.
    fn literal(&self, c: u32) -> &[u64] {
        if c < 128 {
            &self.ascii[c as usize * self.words..][..self.words]
        } else {
            match self.other.binary_search_by_key(&c, |(cp, _)| *cp) {
                Ok(at) => &self.other[at].1,
                Err(_) => &self.none,
            }
        }
    }

    /// Whether the pattern matches `text` from some part boundary to its
    /// end. Answers `false` once `budget` is spent.
    fn matches(&self, text: &[u32], budget: &Budget) -> bool {
        let w = self.words;
        if text.len() * w >= GLOB_COSTLY_STEPS && budget.check() {
            return false;
        }
        let mut cur: SmallVec<[u64; 4]> = SmallVec::from_slice(&self.start);
        let mut next: SmallVec<[u64; 4]> = SmallVec::from_elem(0, w);
        for (at, &c) in text.iter().enumerate() {
            if at & self.check_mask == self.check_mask && budget.check() {
                return false;
            }
            let lit = self.literal(c);
            let sep = c == SEP;
            let stay = if sep { &self.star_all } else { &self.star };
            let mut carry = 0u64;
            for k in 0..w {
                let mut advance = cur[k] & lit[k];
                if !sep {
                    advance |= cur[k] & self.one[k];
                }
                next[k] = (advance << 1) | carry | (cur[k] & stay[k]);
                carry = advance >> 63;
            }
            close(&mut next, &self.star);
            if sep {
                // A part starts after the separator: try the pattern there.
                for k in 0..w {
                    next[k] |= self.start[k];
                }
            }
            std::mem::swap(&mut cur, &mut next);
        }
        cur[self.len / 64] & (1 << (self.len % 64)) != 0
    }
}

/// Adds to `states` the state after each live star, which may take nothing.
/// Two stars never stand side by side (a run of `*` is one token), so one
/// pass reaches every state a chain of empty steps can.
fn close(states: &mut [u64], star: &[u64]) {
    let mut carry = 0u64;
    for k in 0..states.len() {
        let live = states[k] & star[k];
        states[k] |= (live << 1) | carry;
        carry = live >> 63;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::fold::fold;

    /// A budget already spent, for the tests of what checks one.
    fn with_spent_budget(test: impl FnOnce(&Budget)) {
        use std::sync::atomic::AtomicBool;
        use std::time::{Duration, Instant};
        let cancelled = AtomicBool::new(true);
        test(&Budget::until(
            Instant::now() + Duration::from_secs(3600),
            &cancelled,
        ));
    }

    /// `n` code points of `x`.
    fn xs(n: usize) -> CodePointBuf {
        let mut text = CodePointBuf::new();
        decode_into("x".repeat(n).as_bytes(), &mut text);
        text
    }

    fn glob(pattern: &str) -> Glob {
        match Wildcard::name(pattern.as_bytes()).shape {
            Shape::Name(glob) => glob,
            _ => unreachable!(),
        }
    }

    #[test]
    fn a_name_pattern_matches_the_whole_name() {
        let w = Wildcard::name(b"*.rs");
        assert!(w.is_name());
        assert!(w.matches(&fold(b"main.rs")));
        assert!(!w.matches(&fold(b"main.rs.bak")));
        let star = Wildcard::name(b"re*rt");
        assert!(star.matches(b"report"));
        assert!(star.matches(b"rert"), "re + empty + rt");
        assert!(!star.matches(b"rt"), "missing the re prefix");
        assert!(!star.matches(b"reprtx"));
        let one = Wildcard::name(b"f?o");
        assert!(one.matches(b"foo") && one.matches(b"fao"));
        assert!(!one.matches(b"fo") && !one.matches(b"fooo"));
        assert!(Wildcard::name("caf?".as_bytes()).matches("café".as_bytes()));
        assert!(
            Wildcard::name("?.txt".as_bytes()).matches("😀.txt".as_bytes()),
            "an astral code point is one"
        );
    }

    /// The live answers of Everything 1.5 for its fixture's `w` folder,
    /// against full paths under `C:\fx` (DESIGN.md §2.2, L1:38–84, L2:125–131).
    #[test]
    fn a_sub_path_pattern_matches_whole_parts_at_the_end() {
        let paths = [
            r"c:\fx\w\windows",
            r"c:\fx\w\windows\cmd.exe",
            r"c:\fx\w\windows\notepad.exe",
            r"c:\fx\w\windows\sub",
            r"c:\fx\w\windows\sub\deep\a.exe",
            r"c:\fx\w\windows\system32\cmd.exe",
            r"c:\fx\w\windows\system32\drivers\x.sys",
            r"c:\fx\w\mywindows\c.exe",
            r"c:\fx\w\windows2\b.exe",
            r"c:\fx\words\never.txt",
        ];
        let hits = |pattern: &str| -> Vec<&str> {
            let w = Wildcard::sub_path(pattern.as_bytes());
            assert!(!w.is_name(), "{pattern}");
            paths
                .iter()
                .copied()
                .filter(|p| w.matches(p.as_bytes()))
                .collect()
        };
        assert_eq!(
            hits(r"windows\*.exe"),
            [r"c:\fx\w\windows\cmd.exe", r"c:\fx\w\windows\notepad.exe"]
        );
        assert_eq!(hits(r"\windows\*.exe"), hits(r"windows\*.exe"));
        assert_eq!(
            hits(r"windows\*"),
            [
                r"c:\fx\w\windows\cmd.exe",
                r"c:\fx\w\windows\notepad.exe",
                r"c:\fx\w\windows\sub"
            ]
        );
        assert_eq!(
            hits(r"windows\**.exe"),
            [
                r"c:\fx\w\windows\cmd.exe",
                r"c:\fx\w\windows\notepad.exe",
                r"c:\fx\w\windows\sub\deep\a.exe",
                r"c:\fx\w\windows\system32\cmd.exe"
            ]
        );
        assert_eq!(
            hits(r"windows\**\*.exe"),
            [
                r"c:\fx\w\windows\sub\deep\a.exe",
                r"c:\fx\w\windows\system32\cmd.exe"
            ],
            "two separators around **"
        );
        assert_eq!(
            hits(r"w*\n*"),
            [r"c:\fx\w\windows\notepad.exe", r"c:\fx\words\never.txt"]
        );
        assert_eq!(
            hits(r"windows\*\"),
            [
                r"c:\fx\w\windows\sub\deep\a.exe",
                r"c:\fx\w\windows\system32\cmd.exe",
                r"c:\fx\w\windows\system32\drivers\x.sys"
            ],
            "a trailing separator: below"
        );
        assert_eq!(
            hits(r"**\cmd.exe"),
            [
                r"c:\fx\w\windows\cmd.exe",
                r"c:\fx\w\windows\system32\cmd.exe"
            ]
        );
        assert_eq!(hits("**cmd*"), hits(r"**\cmd.exe"));
        assert_eq!(
            hits(r"c:\**\x.sys"),
            [r"c:\fx\w\windows\system32\drivers\x.sys"]
        );
        assert_eq!(hits(r"?\cmd.exe"), Vec::<&str>::new());
        assert_eq!(
            hits(r"windows?cmd.exe"),
            Vec::<&str>::new(),
            "? takes no separator"
        );
        assert_eq!(
            hits(r"w\*\*\*\*"),
            [
                r"c:\fx\w\windows\sub\deep\a.exe",
                r"c:\fx\w\windows\system32\drivers\x.sys"
            ]
        );
        assert_eq!(hits("**").len(), paths.len());
        assert_eq!(hits(r"*\").len(), paths.len());
    }

    #[test]
    fn a_sub_path_pattern_compiles_its_ends() {
        let w = Wildcard::sub_path(br"\windows\*\");
        assert_eq!(w.pattern(), br"windows\*\**");
        assert_eq!(w.leading_literal(), br"windows\");
        assert_eq!(w.trailing_literal(), b"");
        let w = Wildcard::sub_path(br"*\bin_x64_05\*.dll");
        assert_eq!(w.trailing_literal(), b".dll");
        assert_eq!(
            w.separator_free_literals(),
            [b"bin_x64_05".to_vec(), b".dll".to_vec()]
        );
        assert!(!Wildcard::sub_path(b"*.md").is_name());
    }

    /// A match that backtracks checks its budget as it goes, and a spent one
    /// ends it unmatched: a pattern built to backtrack against a long full
    /// path could otherwise take minutes over one text.
    #[test]
    fn a_spent_budget_ends_a_match_as_it_backtracks() {
        // 101 tokens check every 1,024 code points.
        let g = glob(&format!("*{}", "?".repeat(100)));
        let (long, short) = (xs(5000), xs(500));
        with_spent_budget(|spent| {
            assert!(!g.glob_match(&long, spent), "past a check");
            assert!(g.glob_match(&short, spent), "short of one");
        });
        assert!(g.glob_match(&long, &Budget::unlimited()));
    }

    /// A match that may take many steps checks first, even over a text too
    /// short to reach a check of its own, and one that cannot does not.
    #[test]
    fn a_spent_budget_turns_away_a_costly_match() {
        let costly = Wildcard::name(format!("*{}", "?".repeat(100)).as_bytes());
        let text = xs(500);
        assert!(costly.matches_decoded(&text, &Budget::unlimited()));
        with_spent_budget(|spent| assert!(!costly.matches_decoded(&text, spent)));
        let cheap = Wildcard::name(b"*x");
        with_spent_budget(|spent| assert!(cheap.matches_decoded(&xs(20), spent)));
    }

    /// The automaton checks its budget before a long text and as it goes.
    #[test]
    fn a_spent_budget_stops_the_automaton() {
        let w = Wildcard::sub_path(b"**x");
        let long = xs(10_000);
        assert!(w.matches_decoded(&long, &Budget::unlimited()));
        with_spent_budget(|spent| assert!(!w.matches_decoded(&long, spent)));
        with_spent_budget(|spent| assert!(w.matches_decoded(&xs(20), spent)));
    }

    /// A pattern of more than 64 tokens spans several words of states.
    #[test]
    fn a_long_pattern_carries_its_states_across_words() {
        let parts: Vec<String> = (0..60).map(|i| format!("p{i}")).collect();
        let path = format!(r"c:\{}\end.txt", parts.join(r"\"));
        let pattern = format!(r"**\{}\**.txt", parts[5..55].join(r"\"));
        let w = Wildcard::sub_path(pattern.as_bytes());
        assert!(w.toks.len() > 128, "three words of states");
        assert!(w.matches(path.as_bytes()));
        assert!(!w.matches(path.replace("p20", "q20").as_bytes()));
    }

    /// Whether a pattern of `*`, `**`, `?` and literals matches all of
    /// `text`, worked out for every pair of positions: slow, but plainly
    /// right. `sep_aware`: `*` and `?` take no `\` (a sub-path pattern).
    pub(crate) fn reference_match(pat: &[char], text: &[char], sep_aware: bool) -> bool {
        // `rest[i][j]`: whether `pat[i..]` matches `text[j..]`.
        let mut rest = vec![vec![false; text.len() + 1]; pat.len() + 1];
        rest[pat.len()][text.len()] = true;
        for i in (0..pat.len()).rev() {
            for j in (0..=text.len()).rev() {
                let here = text.get(j).copied();
                let takes = |c: char| !sep_aware || c != '\\';
                rest[i][j] = match pat[i] {
                    // `**`, written as one char by the caller.
                    '\u{1}' => rest[i + 1][j] || (here.is_some() && rest[i][j + 1]),
                    '*' => rest[i + 1][j] || (here.is_some_and(takes) && rest[i][j + 1]),
                    '?' => here.is_some_and(takes) && rest[i + 1][j + 1],
                    c => here == Some(c) && rest[i + 1][j + 1],
                };
            }
        }
        rest[0][0]
    }

    /// Everything 1.5's sub-path rule, from the pattern as typed: a leading
    /// `\` dropped, a trailing one followed by `**`, runs of two or more `*`
    /// a `**`, and a match of the whole of some part-boundary suffix of
    /// `text`.
    pub(crate) fn reference_sub_path(pattern: &str, text: &str) -> bool {
        let pattern = pattern.replace('/', "\\");
        let mut pattern = pattern.strip_prefix('\\').unwrap_or(&pattern).to_string();
        if pattern.ends_with('\\') {
            pattern.push_str("**");
        }
        let mut pat: Vec<char> = Vec::new();
        let chars: Vec<char> = pattern.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '*' {
                let mut j = i;
                while j < chars.len() && chars[j] == '*' {
                    j += 1;
                }
                pat.push(if j - i >= 2 { '\u{1}' } else { '*' });
                i = j;
            } else {
                pat.push(chars[i]);
                i += 1;
            }
        }
        let text: Vec<char> = text.chars().collect();
        std::iter::once(0)
            .chain(
                text.iter()
                    .enumerate()
                    .filter(|(_, c)| **c == '\\')
                    .map(|(i, _)| i + 1),
            )
            .any(|start| reference_match(&pat, &text[start..], true))
    }

    mod arbitrary {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(2048))]

            /// The name matcher, and the length check that turns texts away
            /// before it, answer as the reference does.
            #[test]
            fn wildcards_match_as_the_reference_does(
                pat in "[ab*?]{0,8}",
                text in "[ab]{0,10}",
            ) {
                let w = Wildcard::name(pat.as_bytes());
                let pat: Vec<char> = pat.chars().collect();
                let text: Vec<char> = text.chars().collect();
                prop_assert_eq!(
                    w.matches(text.iter().collect::<String>().as_bytes()),
                    reference_match(&pat, &text, false)
                );
            }

            /// Both sub-path matchers, by parts and by automaton, answer as
            /// the reference does, for patterns with separators, `*`, `**`
            /// and `?` at either end, over paths whose parts repeat.
            #[test]
            fn sub_path_wildcards_match_as_the_reference_does(
                pat in "[ab\\\\*?]{0,9}",
                text in "[ab\\\\é]{0,12}",
            ) {
                let w = Wildcard::sub_path(pat.as_bytes());
                prop_assert_eq!(
                    w.matches(text.as_bytes()),
                    reference_sub_path(&pat, &text),
                    "{:?} on {:?}", pat, text
                );
            }

            /// A pattern long enough for several words of states.
            #[test]
            fn long_sub_path_wildcards_match_as_the_reference_does(
                head in "[ab\\\\]{60,70}",
                pat in "[ab\\\\*?]{0,8}",
                text in "[ab\\\\]{60,90}",
            ) {
                let pattern = format!("**{head}{pat}");
                let w = Wildcard::sub_path(pattern.as_bytes());
                prop_assert_eq!(
                    w.matches(text.as_bytes()),
                    reference_sub_path(&pattern, &text)
                );
                let matching = format!("{text}{head}");
                prop_assert_eq!(
                    w.matches(matching.as_bytes()),
                    reference_sub_path(&pattern, &matching)
                );
            }
        }
    }
}
