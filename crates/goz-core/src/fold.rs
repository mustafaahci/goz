//! Folding for case- and accent-insensitive matching.
//!
//! [`fold`] is how goz compares text by default, as Everything 1.5 does with
//! Match Case and Match Diacritics off: case and accents ignored. Both the
//! needle and each candidate name are folded with the same function, so
//! comparisons stay consistent, and the index keeps every name folded this way
//! for its scan. [`fold_exact`] keeps accents, for `diacritics:`.
//!
//! Each has an ASCII fast path (`A..=Z` → `+0x20`), where the two agree, so
//! an ASCII name or needle costs what it always did. For non-ASCII text:
//!
//! - [`fold_exact`]: canonical decomposition, a simple 1:1 Unicode lowercase
//!   of each scalar (the first scalar of `char::to_lowercase`), then canonical
//!   RECOMPOSITION. NFD makes the same text fold identically whether it
//!   arrived precomposed (NFC, `é` = U+00E9) or decomposed (NFD, `e` +
//!   U+0301): a filename copied from macOS matches a query typed on Windows.
//!   Combining marks are KEPT, so `é` stays apart from `e` and Cyrillic `й`
//!   from `и`. This was goz's only fold before it read `diacritics:`.
//! - [`fold`]: the same lowercase, after Everything's base-letter folding
//!   (DESIGN.md §10 of the query-language work, checked against a live
//!   Everything 1.5): compatibility decomposition (NFKD: `ﬁ` → `fi`,
//!   full-width `ｆ` → `f`, `²` → `2`, a no-break or ideographic space → a
//!   space), the combining diacritical marks dropped (U+0300–U+036F and the
//!   other combining-mark blocks for Latin, Greek and Cyrillic and for
//!   symbols: `é` → `e`, `й` → `и`, `ǆ` → `dz`), and a table of letters
//!   that carry their accent in the letter itself: `ß` `ẞ` → `ss`, `æ` →
//!   `ae`, `œ` → `oe`, `ø` → `o`, `ł` `ŀ` → `l`, `đ` `ð` → `d`, `þ` → `th`,
//!   `ħ` → `h`, `ŧ` → `t`, `ŋ` → `n`, `ı` → `i`, `ς` → `σ`. Then canonical
//!   recomposition, which puts Hangul syllables and voiced kana back together
//!   so `?` keeps counting each as one character. Two kinds of character keep
//!   their own form (canonical decomposition only) because their
//!   compatibility one would change what the text says: one whose
//!   decomposition holds path or name syntax, `\` `/` `:` `.` (full-width
//!   `＼` `／` `：` `．`, small `﹨` `﹒`, the dot leaders `․` `‥` `…`, ...),
//!   which a folded name or path would read as a separator, a drive or an
//!   extension; and one that decomposes to conjoining Hangul jamo though it
//!   is no syllable (the compatibility and half-width jamo `ㄱ` `ㅏ`, the
//!   circled and parenthesized ones), which recomposition would join into
//!   syllables the name never held (`ㄱㅏ` into `가`). Marks other scripts write
//!   their vowels or points with (Devanagari, Thai, Hebrew, Arabic, ...) are
//!   kept: what Everything does with them is unverified, and dropping a vowel
//!   sign would merge different words.
//!
//! The result is close to, but not identical to, NTFS's own `$UpCase`
//! comparison: `$UpCase` is a per-code-unit table and does not normalize.
//! Matching never affects index integrity.
//!
//! Input and output are WTF-8. Unpaired surrogates (code points `U+D800..=
//! U+DFFF`, which a real text needle can never contain) pass through
//! unchanged.

use crate::wtf8::CodePoints;
use smallvec::SmallVec;
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::{decompose_canonical, decompose_compatible};

/// Folds `bytes` (WTF-8) into `out` (WTF-8), ignoring case and accents: the
/// default way goz compares text.
pub fn fold_into(bytes: &[u8], out: &mut Vec<u8>) {
    // ASCII fast path: no decomposition or combining marks are possible
    // below U+0080, so fold byte-wise with no decode. Written out here, as
    // it was before accents were folded, so the per-candidate extension
    // folding on the query hot path compiles as it did.
    if bytes.is_ascii() {
        out.reserve(bytes.len());
        for &b in bytes {
            out.push(b.to_ascii_lowercase());
        }
        return;
    }
    fold_non_ascii(bytes, out, Marks::Drop);
}

/// Folds `bytes` (WTF-8) into `out` (WTF-8), ignoring case but keeping
/// accents: `diacritics:`.
pub fn fold_exact_into(bytes: &[u8], out: &mut Vec<u8>) {
    if bytes.is_ascii() {
        out.reserve(bytes.len());
        for &b in bytes {
            out.push(b.to_ascii_lowercase());
        }
        return;
    }
    fold_non_ascii(bytes, out, Marks::Keep);
}

/// What the non-ASCII path does with accents.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Marks {
    /// [`fold`]: base letters only.
    Drop,
    /// [`fold_exact`]: accents kept.
    Keep,
}

/// The non-ASCII path, deliberately OUT of line.
///
/// It needs a `SmallVec<[char; 256]>` scratch, which is a 1 KiB stack frame, and
/// Rust reserves a function's whole frame on entry. Inlined into `fold_into`
/// that 1 KiB was paid by the ASCII fast path too, on every call, including the
/// per-candidate extension folding on the query hot path. Splitting it out keeps
/// `fold_into` small enough to inline and leaves the big frame to the rare path
/// that actually needs it.
#[inline(never)]
fn fold_non_ascii(bytes: &[u8], out: &mut Vec<u8>, marks: Marks) {
    // Decompose, simple-lowercase each scalar, then RECOMPOSE. The round
    // trip is what makes NFC and NFD spellings equal: emitting the
    // decomposed form would leave "café" as "cafe" + U+0301 under
    // `fold_exact`, which a substring search for "cafe" matches on the
    // prefix, and which counts as five code points so `????` stops matching
    // a four-character name.
    //
    // Composition spans code points (an NFD name arrives as `e` then U+0301 as
    // two separate ones), so this buffers a run rather than working per input
    // scalar. The run is stack-sized for any NTFS name; only the non-ASCII path
    // pays for it at all.
    let mut run: SmallVec<[char; 256]> = SmallVec::new();
    // Whether a character a mark could sit on came since the start or the
    // last separator (see `base_letters`).
    let mut based = false;
    for cp in CodePoints::new(bytes) {
        match char::from_u32(cp) {
            Some(c) => match marks {
                Marks::Keep => decompose_canonical(c, |d| run.push(lower_char(d))),
                Marks::Drop => base_letters(c, &mut run, &mut based),
            },
            // Surrogate / non-scalar (WTF-8): it can take part in no
            // composition, so flush the run and pass it through unchanged.
            None => {
                compose_into(&mut run, out);
                encode(cp, out);
                based = true;
            }
        }
    }
    compose_into(&mut run, out);
}

/// Pushes `c`'s base letters, lower-cased, onto `run`: its letter-table
/// spelling, or its compatibility decomposition less the diacritical marks.
/// The table is read before the decomposition too, so that `ŀ` is `l`, not
/// the `l·` its decomposition is.
///
/// A mark with nothing before it to sit on, at the start of the text or of
/// a path's part (after a `\`, or the `:` of a mount prefix `C:` that names
/// follow with no separator), is kept: so a name of marks alone (which NTFS
/// allows) does not fold to nothing, which a full path built from folded
/// names could not hold, and a full path folded whole folds as its names
/// do one by one (`based` says whether such a character came).
fn base_letters(c: char, run: &mut SmallVec<[char; 256]>, based: &mut bool) {
    let lower = lower_char(c);
    if let Some(letters) = letter_table(lower) {
        run.extend(letters.chars());
        *based = true;
        return;
    }
    let mut parts: SmallVec<[char; 4]> = SmallVec::new();
    decompose_compatible(lower, |d| parts.push(d));
    if !c.is_ascii() && parts.iter().any(|&d| changes_meaning(d)) {
        // Its own form: see the module's documentation.
        parts.clear();
        decompose_canonical(lower, |d| parts.push(d));
    }
    for d in parts {
        let d = lower_char(d);
        if is_diacritic_mark(d) {
            if !*based {
                run.push(d);
            }
            continue;
        }
        match letter_table(d) {
            Some(letters) => run.extend(letters.chars()),
            None => run.push(d),
        }
        *based = !matches!(d, '\\' | ':');
    }
}

/// Whether `d`, in the compatibility decomposition of a non-ASCII
/// character, would change what the folded text says: path or name syntax,
/// or a conjoining Hangul jamo (only a syllable's canonical decomposition,
/// which is its compatibility one too, may give those).
fn changes_meaning(d: char) -> bool {
    matches!(
        d,
        '\\' | '/' | ':' | '.' | '\u{1100}'..='\u{11FF}' | '\u{A960}'..='\u{A97F}' | '\u{D7B0}'..='\u{D7FF}'
    )
}

/// The letters Everything folds to others though no decomposition does:
/// each carries its accent in the letter itself (lower-case forms; the
/// upper-case ones lower-case to these first).
fn letter_table(c: char) -> Option<&'static str> {
    Some(match c {
        'ß' => "ss",
        'æ' => "ae",
        'œ' => "oe",
        'ø' => "o",
        'ł' | 'ŀ' => "l",
        'đ' | 'ð' => "d",
        'þ' => "th",
        'ħ' => "h",
        'ŧ' => "t",
        'ŋ' => "n",
        'ı' => "i",
        'ς' => "σ",
        _ => return None,
    })
}

/// Whether `c` is a combining diacritical mark [`fold`] drops: the blocks
/// of combining marks for Latin, Greek and Cyrillic letters and for symbols.
fn is_diacritic_mark(c: char) -> bool {
    matches!(
        c,
        '\u{0300}'..='\u{036F}'
            | '\u{1AB0}'..='\u{1AFF}'
            | '\u{1DC0}'..='\u{1DFF}'
            | '\u{20D0}'..='\u{20FF}'
            | '\u{FE20}'..='\u{FE2F}'
    )
}

/// Canonically composes `run` into `out` as WTF-8 and clears it.
fn compose_into(run: &mut SmallVec<[char; 256]>, out: &mut Vec<u8>) {
    if run.is_empty() {
        return;
    }
    for c in run.iter().copied().nfc() {
        encode(c as u32, out);
    }
    run.clear();
}

/// `bytes` folded as goz compares text by default: case and accents ignored.
pub fn fold(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    fold_into(bytes, &mut out);
    out
}

/// `bytes` folded with case ignored and accents kept: `diacritics:`.
pub fn fold_exact(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    fold_exact_into(bytes, &mut out);
    out
}

/// Simple 1:1 lowercase of one scalar: the first scalar of the full lowercase
/// mapping. 1:1 and stable, unlike full case folding.
fn lower_char(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Encodes one code point as WTF-8 (surrogates permitted).
fn encode(cp: u32, out: &mut Vec<u8>) {
    if cp < 0x80 {
        out.push(cp as u8);
    } else if cp < 0x800 {
        out.push(0xC0 | (cp >> 6) as u8);
        out.push(0x80 | (cp & 0x3F) as u8);
    } else if cp < 0x10000 {
        out.push(0xE0 | (cp >> 12) as u8);
        out.push(0x80 | ((cp >> 6) & 0x3F) as u8);
        out.push(0x80 | (cp & 0x3F) as u8);
    } else {
        out.push(0xF0 | (cp >> 18) as u8);
        out.push(0x80 | ((cp >> 12) & 0x3F) as u8);
        out.push(0x80 | ((cp >> 6) & 0x3F) as u8);
        out.push(0x80 | (cp & 0x3F) as u8);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_folds_case() {
        assert_eq!(fold(b"Report.PDF"), b"report.pdf");
        assert_eq!(fold(b"already lower"), b"already lower");
        assert_eq!(fold(b"MiXeD123"), b"mixed123");
        assert_eq!(fold_exact(b"MiXeD123"), b"mixed123");
    }

    #[test]
    fn folds_are_idempotent() {
        for s in [
            "Foo", "ÇAĞAÇ", "Straße", "ФайЛ", "ǅx", "가각", "がガ", "ŀa", "ﬁle",
        ] {
            for f in [fold, fold_exact] {
                let once = f(s.as_bytes());
                let twice = f(&once);
                assert_eq!(once, twice, "fold not idempotent for {s}");
            }
        }
    }

    #[test]
    fn needle_and_haystack_fold_consistently() {
        // A folded needle is found in a folded haystack regardless of the
        // original case of either.
        for f in [fold, fold_exact] {
            let hay = f("MyÇAĞAÇFile".as_bytes());
            let needle = f("çağaç".as_bytes());
            assert!(
                hay.windows(needle.len()).any(|w| w == needle.as_slice()),
                "case-insensitive substring should match across cases"
            );
        }
    }

    #[test]
    fn non_ascii_lowercases() {
        // Turkish/Cyrillic uppercase → lowercase (via NFD + simple lowercase).
        assert!(!fold("İ".as_bytes()).is_empty());
        // Case-insensitive Cyrillic: upper and lower fold to the same thing.
        assert_eq!(fold("ФАЙЛ".as_bytes()), fold("файл".as_bytes()));
        assert_eq!(fold_exact("ФАЙЛ".as_bytes()), fold_exact("файл".as_bytes()));
    }

    #[test]
    fn nfc_and_nfd_fold_identically() {
        // "café": precomposed (NFC, é = U+00E9) vs decomposed (NFD, e + U+0301)
        // must fold to the same bytes so a query matches regardless of the
        // normalization form the filename was stored in.
        let nfc = "caf\u{00E9}"; // café, precomposed
        let nfd = "cafe\u{0301}"; // café, e + combining acute
        assert_ne!(nfc.as_bytes(), nfd.as_bytes(), "inputs differ byte-wise");
        assert_eq!(fold(nfc.as_bytes()), fold(nfd.as_bytes()));
        assert_eq!(fold_exact(nfc.as_bytes()), fold_exact(nfd.as_bytes()));
        // `fold_exact` keeps the accent: "cafe" ≠ "café".
        assert_ne!(fold_exact("cafe".as_bytes()), fold_exact(nfc.as_bytes()));
    }

    /// `fold_exact` must not smuggle in accent-insensitivity, which `fold`
    /// gives on purpose.
    ///
    /// Byte inequality was too weak to catch that: emitting the DECOMPOSED form
    /// left the folded name as "cafe" + U+0301, which is unequal to "cafe" while
    /// still CONTAINING it, so every substring query for "cafe" matched it. The
    /// invariant that matters is that the folded needle does not OCCUR in the
    /// folded haystack.
    #[test]
    fn exact_folding_is_not_accent_insensitive() {
        let needle = fold_exact("cafe".as_bytes());
        for spelling in ["caf\u{00E9}", "cafe\u{0301}"] {
            let hay = fold_exact(spelling.as_bytes());
            assert!(
                !hay.windows(needle.len()).any(|w| w == needle.as_slice()),
                "{spelling:?}: diacritics:cafe must not match an accented name"
            );
            assert_eq!(fold(spelling.as_bytes()), b"cafe", "{spelling:?}");
        }
        // The same rule from the other side: a genuinely distinct letter stays
        // distinct through the decompose/recompose round trip. Cyrillic U+0439
        // decomposes to U+0438 + U+0306.
        assert_ne!(
            fold_exact("\u{0439}".as_bytes()),
            fold_exact("\u{0438}".as_bytes())
        );
        assert_eq!(fold("\u{0439}".as_bytes()), fold("\u{0438}".as_bytes()));
    }

    /// Everything 1.5's own answers (live, L1:169-226 and L3:1-23 of the
    /// query-language work): each pair finds the other, both ways.
    #[test]
    fn base_letters_are_everythings() {
        for (a, b) in [
            ("café", "cafe"),
            ("CAFÉ", "cafe"),
            ("Straße", "strasse"),
            ("ẞ", "ss"),
            ("æble", "aeble"),
            ("Æble", "aeble"),
            ("œuvre", "oeuvre"),
            ("søren", "soren"),
            ("łódź", "lodz"),
            ("đak", "dak"),
            ("kırmızı", "kirmizi"),
            ("İstanbul", "istanbul"),
            ("çağ", "cag"),
            ("ﬁle", "file"),
            ("ｆｕｌｌ", "full"),
            ("x²y", "x2y"),
            ("año", "ano"),
            ("άλφα", "αλφα"),
            ("phở", "pho"),
            ("naïve", "naive"),
            ("q\u{0301}", "q"),
            ("þðr", "thdr"),
            ("ħal", "hal"),
            ("ŧar", "tar"),
            ("ŋa", "na"),
            ("ĳs", "ijs"),
            ("ſun", "sun"),
            ("йод", "иод"),
            ("ёж", "еж"),
            ("ας", "ασ"),
            ("ŀa", "la"),
            ("Ŀa", "la"),
            ("ǆx", "dzx"),
            ("a\u{00A0}b", "a b"),
            ("a\u{3000}b", "a b"),
        ] {
            assert_eq!(
                fold(a.as_bytes()),
                fold(b.as_bytes()),
                "{a:?} and {b:?} fold alike"
            );
        }
        // Unmapped in Everything, and so in goz.
        assert_ne!(fold("əb".as_bytes()), fold("eb".as_bytes()));
        // A mark with no letter before it stays, so that a name of marks
        // alone keeps them, and a path folds part by part as its names do.
        assert_eq!(fold("\u{301}".as_bytes()), "\u{301}".as_bytes());
        assert_eq!(
            fold("\u{301}\u{301}é".as_bytes()),
            "\u{301}\u{301}e".as_bytes()
        );
        for (prefix, name) in [(r"C:\é\", "\u{301}x"), ("C:", "\u{301}x")] {
            let mut parts = fold(prefix.as_bytes());
            parts.extend_from_slice(&fold(name.as_bytes()));
            assert_eq!(parts, fold(format!("{prefix}{name}").as_bytes()));
        }
        // Scripts whose marks are vowels keep them.
        assert_ne!(fold("कु".as_bytes()), fold("क".as_bytes()));
    }

    /// No folding makes path or name syntax (a separator, a drive's `:`, an
    /// extension's dot) of a character that is none, nor joins compatibility
    /// jamo into a syllable: the character keeps its own form. Other
    /// full-width letters still fold.
    #[test]
    fn folding_makes_no_syntax_and_no_syllables() {
        for kept in [
            "\u{FF3C}", // ＼ fullwidth reverse solidus
            "\u{FE68}", // ﹨ small reverse solidus
            "\u{FF0F}", // ／
            "\u{FF1A}", // ：
            "\u{FE55}", // ﹕ small colon
            "\u{FF0E}", // ．
            "\u{FE52}", // ﹒ small full stop
            "\u{2024}", // ․ one dot leader
            "\u{2025}", // ‥
            "\u{2026}", // …
            "\u{2488}", // ⒈ digit one full stop
            "\u{2100}", // ℀ account of
            "\u{3131}", // ㄱ compatibility jamo
            "\u{314F}", // ㅏ
            "\u{FFA1}", // ﾡ half-width jamo
            "\u{3260}", // ㉠ circled jamo
            "\u{326E}", // ㉮ circled syllable
        ] {
            for f in [fold, fold_exact] {
                assert_eq!(f(kept.as_bytes()), kept.as_bytes(), "{kept:?}");
            }
        }
        assert_eq!(fold(r"C:\a＼b".as_bytes()), r"c:\a＼b".as_bytes());
        assert_eq!(fold("report․PDF".as_bytes()), "report․pdf".as_bytes());
        // The jamo stay apart: a name of four keeps four for `????`.
        assert_eq!(fold("ㄱㅏ".as_bytes()), "ㄱㅏ".as_bytes());
        assert_eq!(fold("ㅎㅏㅎㅏ".as_bytes()), "ㅎㅏㅎㅏ".as_bytes());
        // Syllables, decomposed or not, and the rest of the full-width forms
        // fold as before.
        assert_eq!(fold("\u{1100}\u{1161}".as_bytes()), "가".as_bytes());
        assert_eq!(fold("가".as_bytes()), "가".as_bytes());
        assert_eq!(fold("ｆｕｌｌ".as_bytes()), b"full");
        assert_eq!(fold("Ａ＿１".as_bytes()), b"a_1");
    }

    /// A precomposed character folds to ONE code point, so `?` (which matches
    /// exactly one) keeps counting visible characters. The decomposed form made
    /// a 4-character name 5 code points and quietly broke `????`. Hangul
    /// syllables and voiced kana are composed again after the decomposition
    /// the base fold makes.
    #[test]
    fn folding_preserves_code_point_arity() {
        for spelling in ["caf\u{00E9}", "cafe\u{0301}"] {
            for f in [fold, fold_exact] {
                let folded = f(spelling.as_bytes());
                assert_eq!(
                    crate::wtf8::to_utf16(&folded).len(),
                    4,
                    "{spelling:?} must fold to 4 code points, not a decomposed 5"
                );
            }
        }
        assert_eq!(fold("가각".as_bytes()), "가각".as_bytes());
        assert_eq!(fold("ガが".as_bytes()), "ガが".as_bytes());
    }

    #[test]
    fn surrogates_pass_through() {
        // Lone high surrogate embedded in WTF-8 survives folding.
        let mut bytes = Vec::new();
        crate::wtf8::from_utf16(&[0x0041, 0xD800, 0x0042], &mut bytes);
        for f in [fold, fold_exact] {
            let folded = f(&bytes);
            // 'A' → 'a', 'B' → 'b', surrogate unchanged.
            assert_eq!(crate::wtf8::to_utf16(&folded), vec![0x0061, 0xD800, 0x0062]);
        }
    }

    use proptest::prelude::*;

    proptest! {
        /// Both folds are total (never panic) and idempotent over arbitrary
        /// UTF-16 (unpaired surrogates included): they are the basis of
        /// every query match, so folding a folded name must be a fixpoint.
        #[test]
        fn folds_are_total_and_idempotent(units in proptest::collection::vec(any::<u16>(), 0..64)) {
            let mut bytes = Vec::new();
            crate::wtf8::from_utf16(&units, &mut bytes);
            for f in [fold, fold_exact] {
                let once = f(&bytes);
                let twice = f(&once);
                prop_assert_eq!(once, twice);
            }
        }

        /// The decoder path inside the folds is total on arbitrary bytes.
        #[test]
        fn folds_never_panic_on_arbitrary_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..128)) {
            let _ = fold(&bytes);
            let _ = fold_exact(&bytes);
        }
    }
}
