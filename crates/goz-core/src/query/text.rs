//! Matching a text term's needle where its modifiers say: at the start or
//! end of its target (`startwith:`, `endwith:`, `whole:`), at a word's
//! start or end (`prefix:`, `suffix:`, `ww:`), and skipping punctuation or
//! white space in the target (`nopunc:`, `nows:`), as Everything 1.5 does.
//!
//! A word is a run of letters, digits and combining marks; every other
//! character ends one, `_` and `'` included (Everything 1.5 changed `_`;
//! DESIGN.md §2.3 of the query-language work, live answers L1:127–160 and
//! L2:78–83). Punctuation is what is neither part of a word nor white
//! space. With a skip, the target's characters of that class may be passed
//! over between the needle's, while the needle's own characters, its
//! punctuation included, must each match one of the target's.

use super::budget::Budget;
use super::expr::{Anchors, Skip};
use super::glob::{CodePointBuf, decode_into};
use crate::wtf8::CodePoints;
use memchr::memmem;
use unicode_normalization::char::is_combining_mark;

/// Whether code point `cp` belongs to a word: a letter, a digit or a
/// combining mark. A lone surrogate does not.
pub(super) fn is_word(cp: u32) -> bool {
    char::from_u32(cp).is_some_and(|c| c.is_alphanumeric() || is_combining_mark(c))
}

/// Whether `cp` is white space (Unicode `White_Space`).
fn is_space(cp: u32) -> bool {
    char::from_u32(cp).is_some_and(char::is_whitespace)
}

/// Whether `cp` is punctuation: neither part of a word nor white space.
fn is_punct(cp: u32) -> bool {
    !is_word(cp) && !is_space(cp)
}

/// How many steps a match that skips takes between checks of its budget:
/// over a full path of tens of thousands of characters, which its own skips
/// can make quadratic (`path:nopunc:-b` over a path of dashes), it must not
/// outlast the budget, as no wildcard does either.
const SKIP_STEPS_PER_CHECK: usize = 1 << 16;

/// The code point that ends at byte `p` of `text` (WTF-8), if any.
fn before(text: &[u8], p: usize) -> Option<u32> {
    let mut start = p.checked_sub(1)?;
    // Back over at most three continuation bytes to the code point's lead.
    while start > 0 && p - start < 4 && text[start] & 0xC0 == 0x80 {
        start -= 1;
    }
    CodePoints::new(&text[start..p]).last()
}

/// The code point that starts at byte `p` of `text`, if any.
fn after(text: &[u8], p: usize) -> Option<u32> {
    CodePoints::new(text.get(p..)?).next()
}

/// Which ends of a text are no ends of the whole: a window of a file's
/// content that more of it comes before or after (see the `content`
/// module). An anchor never holds at an open end, nor a word boundary; a
/// match there is found in the window beyond, which repeats it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Open {
    pub(super) start: bool,
    pub(super) end: bool,
}

/// A needle and where its match must stand.
pub(super) struct TextMatcher<'a> {
    finder: memmem::Finder<'a>,
    /// The needle decoded, for a match that skips characters.
    needle_cps: Vec<u32>,
    anchors: Anchors,
    skip: Skip,
}

impl<'a> TextMatcher<'a> {
    pub(super) fn new(needle: &'a [u8], anchors: Anchors, skip: Skip) -> Self {
        TextMatcher {
            finder: memmem::Finder::new(needle),
            needle_cps: CodePoints::new(needle).collect(),
            anchors,
            skip,
        }
    }

    pub(super) fn needle(&self) -> &[u8] {
        self.finder.needle()
    }

    pub(super) fn skips(&self) -> bool {
        !self.skip.is_none()
    }

    /// Whether `hay` (folded as the needle is) holds the needle where the
    /// anchors say. `cps` is scratch for a match that skips. An empty
    /// needle matches any text but the empty one, as an empty value does in
    /// Everything. `false` once `budget` is found spent, as the query is then
    /// refused whatever this answers.
    pub(super) fn matches(&self, hay: &[u8], cps: &mut CodePointBuf, budget: &Budget) -> bool {
        self.matches_within(hay, cps, budget, Open::default())
    }

    /// [`Self::matches`] on a window of a longer text, whose `open` ends
    /// are none of the text's.
    pub(super) fn matches_within(
        &self,
        hay: &[u8],
        cps: &mut CodePointBuf,
        budget: &Budget,
        open: Open,
    ) -> bool {
        if self.needle_cps.is_empty() {
            return !hay.is_empty();
        }
        if self.skip.is_none() {
            return self.anchored(hay, open);
        }
        decode_into(hay, cps);
        self.skipping(cps, budget, open)
    }

    /// The match without skips: each occurrence of the needle in turn,
    /// overlapping ones too, until one stands where the anchors say.
    fn anchored(&self, hay: &[u8], open: Open) -> bool {
        let needle = self.finder.needle();
        let n = needle.len();
        if n > hay.len() {
            return false;
        }
        let a = self.anchors;
        if (a.start && open.start) || (a.end && open.end) {
            return false;
        }
        let stands = |p: usize| {
            (!a.start || p == 0)
                && (!a.end || p + n == hay.len())
                && (!a.word_start || before(hay, p).map_or(!open.start, |c| !is_word(c)))
                && (!a.word_end || after(hay, p + n).map_or(!open.end, |c| !is_word(c)))
        };
        if a.start {
            return hay.starts_with(needle) && stands(0);
        }
        if a.end {
            let p = hay.len() - n;
            return hay[p..] == *needle && stands(p);
        }
        let mut from = 0;
        while let Some(rel) = hay.get(from..).and_then(|rest| self.finder.find(rest)) {
            let p = from + rel;
            if stands(p) {
                return true;
            }
            from = p + 1;
        }
        false
    }

    /// The match with skips, over code points: from each start where the
    /// needle's first character stands, the needle's characters are taken
    /// in turn, each matching the next of the target's or passing over one
    /// the skip allows. Taking a match when one is there is never worse than
    /// passing over it, since a character passed over could only be matched
    /// later by an equal one, which may then be passed over in its turn. The
    /// target's characters before the start and after the end are passed
    /// over too, for `startwith:` and `endwith:`.
    fn skipping(&self, hay: &[u32], budget: &Budget, open: Open) -> bool {
        let mut steps = 0usize;
        let needle = &self.needle_cps;
        let skippable =
            |c: u32| (self.skip.punct && is_punct(c)) || (self.skip.space && is_space(c));
        let a = self.anchors;
        if (a.start && open.start) || (a.end && open.end) {
            return false;
        }
        // A start anchor allows only starts before the first character that
        // cannot be passed over.
        let last_start = if a.start {
            hay.iter().position(|&c| !skippable(c)).unwrap_or(hay.len())
        } else {
            hay.len()
        };
        for s in 0..hay.len().min(last_start + 1) {
            if hay[s] != needle[0]
                || (a.word_start && s > 0 && is_word(hay[s - 1]))
                || (a.word_start && s == 0 && open.start)
            {
                continue;
            }
            let (mut k, mut p) = (0, s);
            steps += hay.len() - s;
            if steps > SKIP_STEPS_PER_CHECK {
                steps = 0;
                if budget.check() {
                    return false;
                }
            }
            while k < needle.len() && p < hay.len() {
                if hay[p] == needle[k] {
                    k += 1;
                } else if !skippable(hay[p]) {
                    break;
                }
                p += 1;
            }
            if k < needle.len() {
                continue;
            }
            if a.end && !hay[p..].iter().all(|&c| skippable(c)) {
                continue;
            }
            if a.word_end && p < hay.len() && is_word(hay[p]) {
                continue;
            }
            if a.word_end && p == hay.len() && open.end {
                continue;
            }
            return true;
        }
        false
    }
}

/// The length of WTF-8 `text` in UTF-16 units, as Everything counts
/// lengths: one per code point, two for one outside the basic plane.
pub(super) fn utf16_len(text: &[u8]) -> u64 {
    text.iter()
        .map(|&b| u64::from(b & 0xC0 != 0x80) + u64::from(b >= 0xF0))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fold::fold;

    fn m(needle: &str, anchors: Anchors, skip: Skip, hay: &str) -> bool {
        let needle = fold(needle.as_bytes());
        let matcher = TextMatcher::new(&needle, anchors, skip);
        matcher.matches(
            &fold(hay.as_bytes()),
            &mut CodePointBuf::new(),
            &Budget::unlimited(),
        )
    }

    /// The match that skips checks its budget as it goes, so a quadratic
    /// one over a long path stops once the budget is spent.
    #[test]
    fn a_spent_budget_stops_a_long_skipping_match() {
        let needle = fold("-b".as_bytes());
        let matcher = TextMatcher::new(&needle, NO_ANCHOR, PUNCT);
        let dashes = "-".repeat(20_000);
        let mut cps = CodePointBuf::new();
        assert!(!matcher.matches(dashes.as_bytes(), &mut cps, &Budget::unlimited()));
        assert!(matcher.matches(
            format!("{dashes}b").as_bytes(),
            &mut cps,
            &Budget::unlimited()
        ));
        // With no match, each start runs to the end: the budget is checked
        // on the way, found spent, and the search stops.
        let cancel = std::sync::atomic::AtomicBool::new(true);
        let spent = Budget::until(std::time::Instant::now(), &cancel);
        assert!(!matcher.matches(dashes.as_bytes(), &mut cps, &spent));
        assert!(spent.spent(), "the check ran");
    }

    const WW: Anchors = Anchors {
        start: false,
        end: false,
        word_start: true,
        word_end: true,
    };
    const PREFIX: Anchors = Anchors {
        start: false,
        end: false,
        word_start: true,
        word_end: false,
    };
    const SUFFIX: Anchors = Anchors {
        start: false,
        end: false,
        word_start: false,
        word_end: true,
    };
    const WHOLE: Anchors = Anchors {
        start: true,
        end: true,
        word_start: false,
        word_end: false,
    };
    const START: Anchors = Anchors {
        start: true,
        end: false,
        word_start: false,
        word_end: false,
    };
    const END: Anchors = Anchors {
        start: false,
        end: true,
        word_start: false,
        word_end: false,
    };
    const NO: Skip = Skip {
        punct: false,
        space: false,
    };
    const PUNCT: Skip = Skip {
        punct: true,
        space: false,
    };
    const SPACE: Skip = Skip {
        punct: false,
        space: true,
    };

    /// Everything's answers for its fixture's `words` folder (L1:127–160).
    #[test]
    fn words_break_at_everything_but_letters_digits_and_marks() {
        for hay in ["ever.txt", "ever-and-ever.txt", "ever's.txt"] {
            assert!(m("ever", WW, NO, hay), "{hay}");
        }
        for hay in [
            "123ever.txt",
            "camelCaseEver.txt",
            "everest.txt",
            "forever.txt",
        ] {
            assert!(!m("ever", WW, NO, hay), "{hay}");
        }
        for hay in [
            "spider man.txt",
            "spider.man.txt",
            "spider_man.txt",
            "spider-man.txt",
        ] {
            assert!(m("spider", WW, NO, hay) && m("man", WW, NO, hay), "{hay}");
        }
        assert!(!m("spider", WW, NO, "spiderman.txt"));
        assert!(m("ever's", WW, NO, "ever's.txt"));
        assert!(m("spider-man", WW, NO, "spider-man.txt"));
        assert!(!m("ab", WW, NO, "ab12cd.txt") && m("ab12cd", WW, NO, "ab12cd.txt"));
        assert!(m("txt", WW, NO, "foo bar.txt"));
        assert!(!m("a", WW, NO, "a1b.txt") && m("a", WW, NO, "a·b.txt"));
        assert!(!m("a", WW, NO, "áb.txt"), "á is a letter");
        assert!(
            m("a", WW, NO, "a\u{3000}b.txt"),
            "ideographic space breaks a word"
        );
        assert!(m("aa", WW, NO, "aaa aa"), "an overlapping occurrence");
    }

    #[test]
    fn prefixes_and_suffixes_stand_at_word_edges() {
        assert!(m("ever", PREFIX, NO, "everest.txt"));
        assert!(!m("ever", PREFIX, NO, "forever.txt"));
        assert!(m("12", PREFIX, NO, "123ever.txt"));
        assert!(m("case", PREFIX, NO, "case1"));
        assert!(m("ever", SUFFIX, NO, "forever.txt"));
        assert!(m("ever", SUFFIX, NO, "camelCaseEver.txt"));
        assert!(!m("12", SUFFIX, NO, "123ever.txt"));
        assert!(m("cd", SUFFIX, NO, "l7.abcd"));
        assert!(m("man", SUFFIX, NO, "spiderman.txt"));
    }

    #[test]
    fn anchors_hold_the_ends() {
        assert!(m("hosts", WHOLE, NO, "hosts") && m("HOSTS", WHOLE, NO, "hosts"));
        assert!(!m("hosts", WHOLE, NO, "hosts.txt"));
        assert!(m("ever", START, NO, "ever123.txt") && !m("ever", START, NO, "never.txt"));
        assert!(m("hosts", END, NO, "myhosts") && !m("ever", END, NO, "ever.txt"));
        assert!(m(".txt", END, NO, "ever.txt"));
        assert!(m("", WHOLE, NO, "x") && !m("", NO_ANCHOR, NO, ""));
    }

    const NO_ANCHOR: Anchors = Anchors {
        start: false,
        end: false,
        word_start: false,
        word_end: false,
    };

    /// Everything's answers (L1:161–168, L2:75–77, L3:29–31).
    #[test]
    fn a_skip_passes_over_the_targets_punctuation_or_space() {
        for hay in [
            "spider.man.txt",
            "spider_man.txt",
            "spider-man.txt",
            "spiderman.txt",
        ] {
            assert!(m("spiderman", NO_ANCHOR, PUNCT, hay), "{hay}");
        }
        assert!(!m("spiderman", NO_ANCHOR, PUNCT, "spider man.txt"));
        assert!(m("spiderman", NO_ANCHOR, SPACE, "spider man.txt"));
        assert!(!m("spiderman", NO_ANCHOR, SPACE, "spider-man.txt"));
        let both = Skip {
            punct: true,
            space: true,
        };
        assert!(m("spiderman", NO_ANCHOR, both, "spider - man.txt"));
        for hay in [
            "a+b.txt",
            "a#b.txt",
            "a€b.txt",
            "a©b.txt",
            "a·b.txt",
            "a'b.txt",
            "a(b).txt",
        ] {
            assert!(m("ab", NO_ANCHOR, PUNCT, hay), "{hay}");
        }
        assert!(!m("ab", NO_ANCHOR, PUNCT, "a1b.txt"));
        assert!(!m("ab", NO_ANCHOR, PUNCT, "a b.txt"));
        assert!(m("ab", NO_ANCHOR, SPACE, "a\u{3000}b.txt"));
        assert!(
            !m("a.b", NO_ANCHOR, PUNCT, "a#b.txt"),
            "the needle's own must match"
        );
        assert!(m("a.b", NO_ANCHOR, PUNCT, "a.-b.txt"));
        assert!(m("a-b", NO_ANCHOR, PUNCT, "a--b"));
        assert!(m("ab", WHOLE, PUNCT, "-a.b-"));
        assert!(!m("ab", WHOLE, PUNCT, "-a.bc"));
        assert!(m("ab", START, PUNCT, "..a_b x"));
        assert!(!m("ab", START, PUNCT, "x a_b"));
        assert!(m("man", WW, PUNCT, "spider.man.txt"));
    }

    #[test]
    fn utf16_lengths_count_astral_characters_twice() {
        assert_eq!(utf16_len(b"l7.abcd"), 7);
        assert_eq!(utf16_len("😀.txt".as_bytes()), 6);
        assert_eq!(utf16_len("çağ.txt".as_bytes()), 7);
        assert_eq!(utf16_len(&[0xED, 0xA0, 0x80]), 1, "a lone surrogate");
        assert_eq!(utf16_len(b""), 0);
    }
}
