use super::*;
use crate::index::store::FrnMap;
use crate::index::{NTFS_ROOT_FRN, VolumeIndex};
use crate::proto::ResultItem;
use crate::query::engine::run_query;
use crate::query::parse::parse_query;
use crate::types::{Frn, SortKey, SortSpec};
use crate::usn::record::{FILE_ATTRIBUTE_DIRECTORY, ParsedUsnRecord, USN_REASON_FILE_CREATE};
use proptest::prelude::*;
use std::cell::Cell;
use std::collections::BTreeMap;

/// The system a test searches in: Latin-1 for the ANSI code page (the
/// fixtures' accents are where Windows-1252 has them), and the null filter
/// for `.bin`, `.exe` and `.zip`, as on the machine the answers came from.
struct Env {
    cancel: Cell<bool>,
}

impl Env {
    fn new() -> Self {
        Env {
            cancel: Cell::new(false),
        }
    }
}

impl ContentEnv for Env {
    fn ansi(&self, bytes: &[u8], out: &mut String) {
        out.extend(bytes.iter().map(|&b| char::from(b)));
    }
    fn filter(&self, ext: &str) -> TextFilter {
        match ext {
            "bin" | "exe" | "zip" => TextFilter::Null,
            "pdf" => TextFilter::Other,
            _ => TextFilter::None,
        }
    }
    fn cancelled(&self) -> bool {
        self.cancel.get()
    }
}

fn utf16le(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

fn utf16be(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_be_bytes).collect()
}

fn latin1(s: &str) -> Vec<u8> {
    s.chars()
        .map(|c| u8::try_from(u32::from(c)).unwrap())
        .collect()
}

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

/// The files of wave 9's fixtures `c9` and `c9b` (`w9\m8\ev\mkfx.py`,
/// `mkfx2.py`), byte for byte.
fn fixtures() -> Vec<(&'static str, Vec<u8>)> {
    let blob = cat(&[
        b"\x00\x01\x02needle\x00\x00",
        &utf16le("wneedle"),
        b"\xff\xfe\x00",
    ]);
    vec![
        (
            "u8.txt",
            "Hello World caf\u{e9} na\u{ef}ve TODO_item foo-bar\nline2 abc123 x;y\n"
                .as_bytes()
                .to_vec(),
        ),
        (
            "u8bom.txt",
            cat(&[
                b"\xef\xbb\xbf",
                "Gr\u{fc}\u{df}e aus K\u{f6}ln\n".as_bytes(),
            ]),
        ),
        (
            "u16le.txt",
            cat(&[b"\xff\xfe", &utf16le("Unicode sixteen Stra\u{df}e\n")]),
        ),
        ("u16le_nobom.dat", utf16le("widechar plain text\n")),
        (
            "u16be.txt",
            cat(&[b"\xfe\xff", &utf16be("big endian words\n")]),
        ),
        ("u16be_nobom.dat", utf16be("bigend nobom\n")),
        ("ansi.txt", latin1("r\u{e9}sum\u{e9} ANSI fa\u{e7}ade\n")),
        ("bin.bin", blob.clone()),
        ("upper.txt", b"CamelCase WORD wordy words\n".to_vec()),
        ("empty.txt", Vec::new()),
        ("cafe_name.txt", b"nothing here\n".to_vec()),
        ("b_needle.dat", blob.clone()),
        ("b_needle.xyz", blob.clone()),
        ("b_needle.bin", blob),
        ("w16.txt", utf16le("widechar text\n")),
        ("be16.txt", utf16be("bigend text\n")),
        ("cafe8.dat", "caf\u{e9} au lait\n".as_bytes().to_vec()),
        ("ansi.dat", latin1("r\u{e9}sum\u{e9} here\n")),
        ("nul.txt", b"hello\x00world needle3\n".to_vec()),
        ("nul.dat", b"hello\x00world needle4\n".to_vec()),
        ("odd16.dat", cat(&[b"x", &utf16le("oddwide")])),
        (
            "mixed.dat",
            cat(&[b"plain ascii start ", &utf16le("wideafter")]),
        ),
    ]
}

/// The files of the fixtures whose content `query`'s one term matches.
fn matching(query: &str) -> Vec<&'static str> {
    let split = ContentQuery::new(query, false).unwrap_or_else(|e| panic!("{query}: {e}"));
    assert_eq!(split.terms().len(), 1, "{query}");
    let env = Env::new();
    let mut out: Vec<&str> = fixtures()
        .into_iter()
        .filter(|(name, bytes)| split.terms()[0].search_bytes(bytes, name, &env))
        .map(|(name, _)| name)
        .collect();
    out.sort_unstable();
    out
}

/// A live Everything 1.5.0.1424b's answers for the fixtures (`w9\m8\ev\r.txt`
/// and `r2.txt`; folders left out, since the client never reads one): the
/// encodings it reads, its case, accent, word and punctuation modifiers,
/// `*` and `;` as text, and no text in a file of the null filter.
#[test]
fn content_matches_as_everything_reads_files() {
    let rows: &[(&str, &[&str])] = &[
        ("content:hello", &["nul.dat", "nul.txt", "u8.txt"]),
        ("case:content:hello", &["nul.dat", "nul.txt"]),
        ("case:content:Hello", &["u8.txt"]),
        ("content:cafe", &["cafe8.dat", "u8.txt"]),
        ("diacritics:content:cafe", &[]),
        ("content:caf\u{e9}", &["cafe8.dat", "u8.txt"]),
        ("content:CAF\u{c9}", &["cafe8.dat", "u8.txt"]),
        ("content:naive", &["u8.txt"]),
        ("content:strasse", &["u16le.txt"]),
        ("content:stra\u{df}e", &["u16le.txt"]),
        ("content:STRASSE", &["u16le.txt"]),
        ("content:gr\u{fc}\u{df}e", &["u8bom.txt"]),
        ("content:grusse", &["u8bom.txt"]),
        ("content:r\u{e9}sum\u{e9}", &["ansi.dat", "ansi.txt"]),
        ("content:resume", &["ansi.dat", "ansi.txt"]),
        ("content:widechar", &["u16le_nobom.dat", "w16.txt"]),
        ("content:bigend", &["u16be_nobom.dat"]),
        ("content:endian", &["u16be.txt"]),
        (
            "content:needle",
            &["b_needle.dat", "b_needle.xyz", "nul.dat", "nul.txt"],
        ),
        ("content:wneedle", &["b_needle.dat", "b_needle.xyz"]),
        ("utf8content:hello", &["nul.dat", "nul.txt", "u8.txt"]),
        ("utf8content:sixteen", &[]),
        ("utf8content:gr\u{fc}\u{df}e", &["u8bom.txt"]),
        ("utf8content:r\u{e9}sum\u{e9}", &[]),
        ("utf16content:sixteen", &["u16le.txt"]),
        ("utf16content:widechar", &["u16le_nobom.dat", "w16.txt"]),
        ("utf16content:hello", &[]),
        ("utf16content:wneedle", &[]),
        ("utf16content:wideafter", &["mixed.dat"]),
        ("utf16becontent:endian", &["u16be.txt"]),
        ("utf16becontent:bigend", &["be16.txt", "u16be_nobom.dat"]),
        ("utf16becontent:sixteen", &[]),
        ("ansicontent:r\u{e9}sum\u{e9}", &["ansi.dat", "ansi.txt"]),
        ("ansicontent:hello", &["nul.dat", "nul.txt", "u8.txt"]),
        ("ansicontent:caf\u{e9}", &[]),
        ("ansicontent:sixteen", &[]),
        ("ansicontent:gr\u{fc}\u{df}e", &[]),
        (
            "ansicontent:needle",
            &[
                "b_needle.bin",
                "b_needle.dat",
                "b_needle.xyz",
                "bin.bin",
                "nul.dat",
                "nul.txt",
            ],
        ),
        ("ansicontent:needle3", &["nul.txt"]),
        ("utf8content:needle4", &["nul.dat"]),
        ("content:word", &["u16be.txt", "upper.txt"]),
        ("ww:content:word", &["upper.txt"]),
        ("ww:content:wor", &[]),
        ("ww:content:words", &["u16be.txt", "upper.txt"]),
        (
            "prefix:content:wor",
            &["nul.dat", "nul.txt", "u16be.txt", "u8.txt", "upper.txt"],
        ),
        ("suffix:content:ords", &["u16be.txt", "upper.txt"]),
        ("content:\"world caf\u{e9}\"", &["u8.txt"]),
        ("content:\"hello  world\"", &[]),
        ("content:<hello abc123>", &["u8.txt"]),
        (
            "content:<hello|endian>",
            &["nul.dat", "nul.txt", "u16be.txt", "u8.txt"],
        ),
        (
            "content:",
            &[
                "ansi.dat",
                "ansi.txt",
                "b_needle.dat",
                "b_needle.xyz",
                "be16.txt",
                "cafe8.dat",
                "cafe_name.txt",
                "mixed.dat",
                "nul.dat",
                "nul.txt",
                "odd16.dat",
                "u16be.txt",
                "u16be_nobom.dat",
                "u16le.txt",
                "u16le_nobom.dat",
                "u8.txt",
                "u8bom.txt",
                "upper.txt",
                "w16.txt",
            ],
        ),
        ("content:hel*", &[]),
        ("content:h?llo", &[]),
        ("content:*", &[]),
        ("regex:content:^Hello", &["nul.dat", "nul.txt", "u8.txt"]),
        ("regex:content:abc\\d+", &["u8.txt"]),
        ("content:x;y", &["u8.txt"]),
        ("content:line2", &["u8.txt"]),
        ("content:\"world\\nline2\"", &[]),
        ("content:foo-bar", &["u8.txt"]),
        ("content:foobar", &[]),
        ("nopunc:content:foobar", &["u8.txt"]),
        ("content:camelcase", &["upper.txt"]),
        ("case:content:camelCase", &[]),
        ("startwith:content:hello", &["nul.dat", "nul.txt", "u8.txt"]),
        ("startwith:content:world", &[]),
        ("endwith:content:x;y", &[]),
        ("whole:content:hello", &[]),
        ("content:sixteen", &["u16le.txt"]),
        ("content:\"Unicode sixteen\"", &["u16le.txt"]),
        ("content:oddwide", &["odd16.dat"]),
        ("content:wideafter", &["mixed.dat"]),
        ("content:\"plain ascii\"", &["mixed.dat"]),
        ("content:needle3", &["nul.txt"]),
        ("content:needle4", &["nul.dat"]),
        ("content:world", &["nul.dat", "nul.txt", "u8.txt"]),
    ];
    for (query, want) in rows {
        assert_eq!(matching(query), *want, "{query}");
    }
}

/// Everything's aliases name the same functions, `-` optional.
#[test]
fn content_function_aliases() {
    for (alias, encoding) in [
        ("content:", ContentEncoding::Auto),
        ("ansi-content:", ContentEncoding::Ansi),
        ("contenta:", ContentEncoding::Ansi),
        ("utf8-content:", ContentEncoding::Utf8),
        ("utf16content:", ContentEncoding::Utf16Le),
        ("unicode-content:", ContentEncoding::Utf16Le),
        ("utf16le-content:", ContentEncoding::Utf16Le),
        ("wchar-content:", ContentEncoding::Utf16Le),
        ("contentw:", ContentEncoding::Utf16Le),
        ("wcontent:", ContentEncoding::Utf16Le),
        ("utf16be-content:", ContentEncoding::Utf16Be),
        ("UTF16BECONTENT:", ContentEncoding::Utf16Be),
    ] {
        let split = ContentQuery::new(&format!("{alias}abc"), false).unwrap();
        assert_eq!(split.terms().len(), 1, "{alias}");
        assert_eq!(split.terms()[0].encoding(), encoding, "{alias}");
    }
}

/// gozd refuses a content term, with why; the client's split never sends
/// one, and each query it sends parses.
#[test]
fn gozd_refuses_content_and_the_split_queries_parse() {
    let err = parse_query("*.txt content:abc").unwrap_err();
    assert!(
        matches!(&err, QueryError::Unsupported { what, .. } if what == "content:"),
        "{err}"
    );
    for (query, n) in [
        ("*.txt", 0),
        ("*.txt content:abc", 1),
        ("!content:a | case:content:<b c> folder:utf8content:d", 3),
        ("content*:rest of <the> query | x", 1),
        ("regex:content:^a|b", 1),
        ("<x content:y>", 1),
        ("?regex: content:^a <b>", 1),
        ("whole:<content:a b>", 1),
        ("ext:txt dm:today content:a", 1),
        ("childdm:2024 content:a", 1),
    ] {
        let split = ContentQuery::new(query, false).unwrap_or_else(|e| panic!("{query}: {e}"));
        assert_eq!(split.terms().len(), n, "{query}");
        assert_eq!(split.queries().len(), 1 << n, "{query}");
        assert_eq!(split.is_plain(), n == 0);
        for q in split.queries() {
            assert!(!q.contains("content:"), "{q}");
            crate::query::parse::parse_query_at(q, false, &Clock)
                .unwrap_or_else(|e| panic!("{query} gives {q}: {e}"));
        }
    }
    assert_eq!(
        ContentQuery::new("*.txt content:abc", false)
            .unwrap()
            .queries(),
        [
            "*.txt name-length:>18446744073709551615",
            "*.txt name-length:>=0"
        ]
    );
    assert_eq!(
        ContentQuery::new("a content:\"x y\" | é content:<p|q>", false)
            .unwrap()
            .queries(),
        [
            "a name-length:>18446744073709551615 | é name-length:>18446744073709551615",
            "a name-length:>=0 | é name-length:>18446744073709551615",
            "a name-length:>18446744073709551615 | é name-length:>=0",
            "a name-length:>=0 | é name-length:>=0",
        ]
    );
}

/// More than three content terms are refused, precisely there; a content
/// term inside another function's value too.
#[test]
fn more_than_three_content_terms_are_refused() {
    for n in 0..=6 {
        let query = vec!["content:a"; n].join(" | ");
        match ContentQuery::new(&query, false) {
            Ok(split) => {
                assert!(n <= MAX_CONTENT_TERMS, "{query}");
                assert_eq!(split.terms().len(), n);
            }
            Err(QueryError::TooManyContentTerms(k)) => {
                assert!(n > MAX_CONTENT_TERMS, "{query}");
                assert_eq!(k, n);
            }
            Err(e) => panic!("{query}: {e}"),
        }
    }
    for query in [
        "content:<content:a>",
        "child:<content:a>",
        "content:<size:1>",
        "?content:a",
    ] {
        assert!(ContentQuery::new(query, false).is_err(), "{query}");
    }
}

/// `match_case` (es `-case`) reaches content terms as it does name terms,
/// and `nocase:` turns it off for one.
#[test]
fn the_case_switch_reaches_content_terms() {
    let env = Env::new();
    let hit = |q: &str, case| {
        ContentQuery::new(q, case).unwrap().terms()[0].search_bytes(b"Hello", "a.txt", &env)
    };
    assert!(hit("content:hello", false));
    assert!(!hit("content:hello", true));
    assert!(hit("nocase:content:hello", true));
    assert!(hit("content:Hello", true));
}

/// A clock for the queries with a `dm:` term.
struct Clock;

impl crate::query::LocalClock for Clock {
    fn now(&self) -> i64 {
        134_115_840_000_000_000
    }
    fn to_local(&self, utc: i64) -> Option<i64> {
        Some(utc)
    }
    fn to_utc(&self, local: i64) -> Option<i64> {
        Some(local)
    }
    fn week_start(&self) -> u8 {
        0
    }
    fn date_order(&self) -> crate::query::DateOrder {
        crate::query::DateOrder::YearMonthDay
    }
}

/// A query in the shape the proptest builds: name terms and content terms
/// under AND, OR, NOT, groups and kinds.
#[derive(Clone, Debug)]
enum Q {
    Name(usize),
    Content,
    Not(Box<Q>),
    And(Vec<Q>),
    Or(Vec<Q>),
    File(Box<Q>),
    Folder(Box<Q>),
}

const NAMES: [&str; 3] = ["aa", "bb", "cc"];

impl Q {
    fn render(&self, out: &mut String) {
        match self {
            Q::Name(i) => out.push_str(NAMES[*i]),
            Q::Content => out.push_str("content:x"),
            Q::Not(q) => {
                out.push('!');
                q.render_atom(out);
            }
            Q::And(items) => {
                for (i, q) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(' ');
                    }
                    q.render_atom(out);
                }
            }
            Q::Or(items) => {
                for (i, q) in items.iter().enumerate() {
                    if i > 0 {
                        out.push('|');
                    }
                    q.render_atom(out);
                }
            }
            Q::File(q) => {
                out.push_str("file:");
                q.render_atom(out);
            }
            Q::Folder(q) => {
                out.push_str("folder:");
                q.render_atom(out);
            }
        }
    }

    /// Rendered so that it reads as one operand wherever it stands.
    fn render_atom(&self, out: &mut String) {
        if matches!(self, Q::And(_) | Q::Or(_)) {
            out.push('<');
            self.render(out);
            out.push('>');
        } else {
            self.render(out);
        }
    }

    fn contents(&self) -> usize {
        match self {
            Q::Name(_) => 0,
            Q::Content => 1,
            Q::Not(q) | Q::File(q) | Q::Folder(q) => q.contents(),
            Q::And(items) | Q::Or(items) => items.iter().map(Q::contents).sum(),
        }
    }

    /// The verdict for an entry named `name`, a folder or not, whose
    /// content terms answer as `answers` says, in query order: goz's
    /// meaning, where `folder:` reaches each term below it (through NOTs)
    /// and `file:` is ANDed with what follows it.
    fn eval(
        &self,
        name: &str,
        dir: bool,
        answers: &mut impl Iterator<Item = bool>,
        folder: bool,
    ) -> bool {
        let kinded = |v: bool| if folder { dir && v } else { v };
        match self {
            Q::Name(i) => kinded(name.contains(NAMES[*i])),
            Q::Content => {
                let v = answers.next().unwrap();
                kinded(v)
            }
            Q::Not(q) => !q.eval(name, dir, answers, folder),
            Q::And(items) => {
                let vs: Vec<bool> = items
                    .iter()
                    .map(|q| q.eval(name, dir, answers, folder))
                    .collect();
                vs.into_iter().all(|v| v)
            }
            Q::Or(items) => {
                let vs: Vec<bool> = items
                    .iter()
                    .map(|q| q.eval(name, dir, answers, folder))
                    .collect();
                vs.into_iter().any(|v| v)
            }
            Q::File(q) => {
                let v = q.eval(name, dir, answers, folder);
                !dir && v
            }
            Q::Folder(q) => q.eval(name, dir, answers, true),
        }
    }
}

fn query_tree() -> impl Strategy<Value = Q> {
    let leaf = prop_oneof![(0usize..3).prop_map(Q::Name), Just(Q::Content)];
    leaf.prop_recursive(4, 16, 3, |inner| {
        prop_oneof![
            inner.clone().prop_map(|q| Q::Not(Box::new(q))),
            prop::collection::vec(inner.clone(), 2..4).prop_map(Q::And),
            prop::collection::vec(inner.clone(), 2..4).prop_map(Q::Or),
            inner.clone().prop_map(|q| Q::File(Box::new(q))),
            inner.prop_map(|q| Q::Folder(Box::new(q))),
        ]
    })
}

fn enum_rec(frn: u64, name: &str, dir: bool) -> ParsedUsnRecord {
    ParsedUsnRecord {
        major_version: 2,
        frn: Frn(frn | 1 << 48),
        parent_frn: NTFS_ROOT_FRN,
        usn: 0,
        timestamp_ft: 0,
        reason: USN_REASON_FILE_CREATE,
        attributes: if dir { FILE_ATTRIBUTE_DIRECTORY } else { 0 },
        name: name.as_bytes().to_vec(),
        name_lossy: false,
    }
}

/// Every mix of the three name terms, as a file and as a folder.
fn entries() -> Vec<(String, bool)> {
    let mut out = Vec::new();
    for set in 0..8usize {
        for dir in [false, true] {
            let mut name: String = (0..3)
                .filter(|i| set >> i & 1 == 1)
                .map(|i| NAMES[i])
                .collect();
            name.push_str(&format!("_{}{}", set, u8::from(dir)));
            out.push((name, dir));
        }
    }
    out
}

fn entry_index() -> VolumeIndex {
    let mut idx = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
    for (i, (name, dir)) in entries().iter().enumerate() {
        idx.insert_enum(&enum_rec(100 + i as u64, name, *dir));
    }
    idx
}

/// Which of the split's queries each entry matches, run by gozd's own
/// parser and engine.
fn masks(idx: &VolumeIndex, split: &ContentQuery) -> BTreeMap<String, u8> {
    let mut out: BTreeMap<String, u8> = entries().into_iter().map(|(n, _)| (n, 0)).collect();
    for (a, q) in split.queries().iter().enumerate() {
        let parsed = parse_query(q).unwrap_or_else(|e| panic!("{q}: {e}"));
        let hits = run_query(idx, r"C:\", &parsed, None, SortSpec::default(), 0, None);
        for hit in hits.hits {
            let path = String::from_utf8(hit.path).unwrap();
            let name = path.rsplit('\\').next().unwrap().to_string();
            *out.get_mut(&name).unwrap() |= 1 << a;
        }
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// For any boolean structure over at most three content terms, the
    /// verdict read off the 2^n queries gozd runs equals the query's own
    /// meaning, for every entry and every way its content can answer; it
    /// reads no answer it does not need, and none when the entry matched
    /// every query or none. Past three terms the split is refused.
    #[test]
    fn the_verdict_is_exact_for_any_structure(q in query_tree()) {
        let mut text = String::new();
        q.render(&mut text);
        let n = q.contents();
        let split = match ContentQuery::new(&text, false) {
            Ok(split) => split,
            Err(QueryError::TooManyContentTerms(k)) => {
                prop_assert!(n > MAX_CONTENT_TERMS, "{}", text);
                prop_assert_eq!(k, n);
                return Ok(());
            }
            Err(e) => panic!("{text}: {e}"),
        };
        prop_assert!(n <= MAX_CONTENT_TERMS);
        prop_assert_eq!(split.terms().len(), n, "{}", text);
        let idx = entry_index();
        let masks = masks(&idx, &split);
        for (name, dir) in entries() {
            let mask = masks[&name];
            for answers in 0..1usize << n {
                let want = q.eval(&name, dir, &mut (0..n).map(|i| answers >> i & 1 == 1), false);
                let mut asked = Vec::new();
                let got = split.verdict(mask, |i| {
                    asked.push(i);
                    answers >> i & 1 == 1
                });
                prop_assert_eq!(got, want, "{} on {} answering {:b}, mask {:b}", text, name, answers, mask);
                prop_assert!(asked.len() <= n);
                if let Some(v) = split.decided(mask) {
                    prop_assert!(asked.is_empty(), "{} on {}", text, name);
                    prop_assert_eq!(v, want);
                }
            }
        }
    }
}

/// n = 0 to 3 each, by hand: the queries' order and a verdict read off
/// them.
#[test]
fn zero_to_three_terms() {
    let split = ContentQuery::new("aa", false).unwrap();
    assert_eq!(split.queries(), ["aa"]);
    assert!(split.verdict(1, |_| unreachable!()));
    assert!(!split.verdict(0, |_| unreachable!()));
    assert_eq!(split.decided(1), Some(true));

    // `!content:x`: the false query matches, the true one does not.
    let split = ContentQuery::new("!content:x", false).unwrap();
    assert_eq!(
        split.queries(),
        ["!name-length:>18446744073709551615", "!name-length:>=0"]
    );
    assert!(split.verdict(0b01, |_| false));
    assert!(!split.verdict(0b01, |_| true));
    assert_eq!(split.decided(0b01), None);

    // `content:x|content:y`: matched by every query but the all-false one.
    let split = ContentQuery::new("content:x|content:y", false).unwrap();
    assert_eq!(split.queries().len(), 4);
    let mut asked = 0;
    assert!(split.verdict(0b1110, |_| {
        asked += 1;
        true
    }));
    assert_eq!(asked, 1, "the first answer settles it");

    // Three terms, all needed: x and not y, or z.
    let split = ContentQuery::new("<content:x !content:y>|content:z", false).unwrap();
    assert_eq!(split.queries().len(), 8);
    let mask: u8 = (0..8u8)
        .filter(|a| (a & 1 == 1 && a & 2 == 0) || a & 4 != 0)
        .fold(0, |m, a| m | 1 << a);
    for a in 0..8usize {
        let want = (a & 1 == 1 && a & 2 == 0) || a & 4 != 0;
        assert_eq!(split.verdict(mask, |i| a >> i & 1 == 1), want, "{a:b}");
    }
}

/// A window's edges are no ends of the text: a match is found exactly
/// across them, however the windows fall, as in one read of the whole.
#[test]
fn windows_find_what_one_read_finds() {
    let env = Env::new();
    let text = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda";
    let bytes = text.as_bytes();
    for query in [
        "content:gamma",
        "ww:content:eta",
        "ww:content:et",
        "prefix:content:the",
        "suffix:content:ta",
        "startwith:content:alpha",
        "startwith:content:beta",
        "endwith:content:lambda",
        "endwith:content:kappa",
        "whole:content:alpha",
        "nopunc:content:alphabeta",
        "nows:content:alphabeta",
        "content:<alpha lambda>",
        "content:<!zeta>",
        "content:<!omega>",
        "content:<alpha !omega>",
    ] {
        let split = ContentQuery::new(query, false).unwrap();
        let term = &split.terms()[0];
        let whole = term
            .search_in(&mut io::Cursor::new(bytes), "a.txt", &env, 1 << 20, 64)
            .unwrap();
        for window in 24..48 {
            for overlap in [10usize, 12] {
                let got = term
                    .search_in(&mut io::Cursor::new(bytes), "a.txt", &env, window, overlap)
                    .unwrap();
                assert_eq!(got, whole, "{query} in windows of {window} by {overlap}");
            }
        }
    }
    // UTF-16 keeps its alignment from window to window.
    let wide = utf16le(text);
    for query in [
        "utf16content:gamma",
        "content:lambda",
        "ww:utf16content:eta",
    ] {
        let split = ContentQuery::new(query, false).unwrap();
        let term = &split.terms()[0];
        for window in [24, 30, 50] {
            assert!(
                term.search_in(&mut io::Cursor::new(&wide), "w.dat", &env, window, 20)
                    .unwrap(),
                "{query} in {window}"
            );
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Any text, any needle shorter than the overlap, any window: the
    /// windows answer as one read does.
    #[test]
    fn windows_agree_with_one_read(
        words in prop::collection::vec("[ab]{1,3}|[ ,.]", 1..40),
        needle in "[ab]{1,3}",
        modifier in prop::sample::select(vec!["", "ww:", "prefix:", "suffix:", "startwith:", "endwith:", "nopunc:"]),
        window in 14usize..32,
    ) {
        let env = Env::new();
        let text: String = words.concat();
        let query = format!("{modifier}content:{needle}");
        let split = ContentQuery::new(&query, false).unwrap();
        let term = &split.terms()[0];
        let whole = term.search_in(&mut io::Cursor::new(text.as_bytes()), "a.txt", &env, 1 << 20, 64).unwrap();
        let got = term.search_in(&mut io::Cursor::new(text.as_bytes()), "a.txt", &env, window, 6).unwrap();
        prop_assert_eq!(got, whole, "{} in {:?}, window {}", query, text, window);
    }
}

/// A cancelled search stops with no match, and a folder-only kind inside
/// a value is no text.
#[test]
fn a_cancelled_search_matches_nothing() {
    let env = Env::new();
    env.cancel.set(true);
    let split = ContentQuery::new("content:a", false).unwrap();
    let term = &split.terms()[0];
    assert!(!term.search_bytes(b"a", "a.txt", &env));
}

/// The plain-text list and the extension of a name.
#[test]
fn extensions_and_the_plain_text_list() {
    assert_eq!(extension_of("A.TXT"), "txt");
    assert_eq!(extension_of("noext"), "");
    assert_eq!(extension_of(".hidden"), "hidden");
    assert!(is_text_plain("txt") && is_text_plain("yaml") && is_text_plain("a"));
    assert!(!is_text_plain("dat") && !is_text_plain("") && !is_text_plain("pdf"));
}

/// The client orders its rows as gozd does: Everything's extension order,
/// sizes with the unknown last both ways, names, and folders before their
/// contents by path; descending reverses.
#[test]
fn rows_sort_as_gozd_sorts_them() {
    use crate::types::SortDir;
    let item = |path: &str, dir: bool, size: Option<u64>| ResultItem {
        path: path.to_string(),
        path_u16: None,
        size,
        mtime_ft: None,
        is_dir: dir,
    };
    let rows = vec![
        item(r"C:\b.TXT", false, Some(3)),
        item(r"C:\dir.d", true, None),
        item(r"D:\noext", false, Some(1)),
        item(r"C:\z.tar.gz", false, None),
        item(r"D:\a.txt", false, Some(2)),
        item(r"C:\sub\a.txt", false, Some(9)),
    ];
    let order = |key, dir| {
        let mut r = rows.clone();
        sort_items(&mut r, SortSpec { key, dir });
        r.into_iter().map(|i| i.path).collect::<Vec<_>>()
    };
    assert_eq!(
        order(SortKey::Extension, SortDir::Asc),
        [
            r"C:\dir.d",
            r"D:\noext",
            r"C:\z.tar.gz",
            r"C:\sub\a.txt",
            r"D:\a.txt",
            r"C:\b.TXT"
        ]
    );
    let mut rev = order(SortKey::Extension, SortDir::Asc);
    rev.reverse();
    assert_eq!(order(SortKey::Extension, SortDir::Desc), rev);
    assert_eq!(
        order(SortKey::Size, SortDir::Desc),
        [
            r"C:\sub\a.txt",
            r"C:\b.TXT",
            r"D:\a.txt",
            r"D:\noext",
            r"C:\z.tar.gz",
            r"C:\dir.d"
        ]
    );
    assert_eq!(
        order(SortKey::Name, SortDir::Asc),
        [
            r"C:\sub\a.txt",
            r"D:\a.txt",
            r"C:\b.TXT",
            r"C:\dir.d",
            r"D:\noext",
            r"C:\z.tar.gz"
        ]
    );
    assert_eq!(
        order(SortKey::Path, SortDir::Asc),
        [
            r"C:\b.TXT",
            r"C:\dir.d",
            r"C:\z.tar.gz",
            r"C:\sub\a.txt",
            r"D:\a.txt",
            r"D:\noext"
        ]
    );
}

/// A file of bytes reads in ANSI wherever its first non-ASCII byte stands,
/// past its first window too.
#[test]
fn ansi_is_read_past_an_ascii_first_window() {
    let env = Env::new();
    let mut bytes = vec![b'x'; 100];
    bytes.extend_from_slice(&latin1(" r\u{e9}sum\u{e9}"));
    let split = ContentQuery::new("content:resume", false).unwrap();
    let term = &split.terms()[0];
    for window in [40, 64, 1 << 20] {
        assert!(
            term.search_in(&mut io::Cursor::new(&bytes), "a.dat", &env, window, 16)
                .unwrap(),
            "{window}"
        );
    }
}
