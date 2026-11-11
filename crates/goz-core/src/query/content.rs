//! `content:`, searched by the goz client, never by gozd.
//!
//! gozd answers from its index and never opens a file, so a query with
//! content terms is split here. For a query holding n content terms
//! (`content:`, `utf8content:`, `utf16content:`, `utf16becontent:`,
//! `ansicontent:` and Everything's aliases of them), n at most
//! [`MAX_CONTENT_TERMS`], [`ContentQuery::new`] gives the 2^n queries gozd
//! runs: query `a` has term `i` replaced by a constant that is true when
//! bit `i` of `a` is set and false otherwise (a name length any name has,
//! or one none has, which gozd folds away). An entry's verdict under the
//! original query is the verdict of the query whose constants are the
//! entry's own content answers, whatever the query's boolean structure
//! (AND, OR, NOT, groups), since the term's place in the query is kept and
//! only its value changes: [`ContentQuery::verdict`] reads that off which
//! queries matched the entry, asking for content answers only while they
//! can change it. An entry that matched no query matches nothing; one that
//! matched all of them matches without its content being read.
//!
//! A term's own modifiers (`case:`, `diacritics:`, `ww:`, `prefix:`,
//! `suffix:`, `startwith:`, `endwith:`, `whole:`, `nopunc:`, `nows:`,
//! `regex:`), those of a group around it and the `?mod:`s before it apply
//! to its value as to a name term's, read by the same parser, and its value
//! may be a group (`content:<abc 123>`, `content:<abc|123>`). A kind around
//! it (`file:`, `folder:`) stays in the query around the constant. As in
//! Everything, `*` and `?` in a content value are text, and so is `;`.
//!
//! How a file's bytes become text, as a live Everything 1.5.0.1424b reads
//! them (wave 9, `w9\m8\ev`):
//!
//! - `content:` on a file whose extension is one of Everything's
//!   `text_plain_extensions`: a byte order mark (UTF-8, UTF-16LE,
//!   UTF-16BE), else UTF-16LE when the zero bytes say so, else UTF-8 when
//!   the text is valid UTF-8, else the ANSI code page. A UTF-16BE file
//!   without a mark is not read as such.
//! - `content:` on another file: no text when Windows reads its extension
//!   through the null filter (`.bin`, `.exe`, `.dll`, `.zip`: Everything
//!   finds nothing in them), else the bytes searched as ANSI, as UTF-8 and
//!   as UTF-16LE and UTF-16BE from either byte, any of them matching.
//!   Everything runs a file's real IFilter (PDF, Office) first; goz does
//!   not, and searches those files as bytes too, which finds text stored
//!   plainly and not text a format compresses.
//! - `utf8content:`, `utf16content:` (LE, from the first byte),
//!   `utf16becontent:` and `ansicontent:` read every file so, a mark at
//!   the start dropped.
//!
//! Matching is case- and accent-insensitive unless `case:` or
//! `diacritics:` says otherwise, folded as names are. A folder has no
//! content. A file is read in windows of [`WINDOW`] bytes that overlap by
//! [`OVERLAP`], so its size costs no memory: a match that ends a window
//! counts in the next, whose start is no start of the text and whose end
//! no end of it, so a text term's match is found exactly when it is
//! shorter than the overlap. A `regex:` value is matched in each window as
//! it stands, so a match crossing a window's edge, or `^` and `$` there,
//! can differ from Everything's.

use std::io::{self, Read};

use super::budget::Budget;
use super::engine::{extension_key, mtime_key, size_key, sort_extension};
use super::expr::{Anchors, Expr, Leaf, Skip};
use super::glob::CodePointBuf;
use super::parse::{Kind, QueryError, parse_content_terms};
use super::pattern::RegexTerm;
use super::text::{Open, TextMatcher};
use crate::fold::{fold, fold_exact};
use crate::proto::ResultItem;
use crate::types::{SortDir, SortKey, SortSpec};

/// The most content terms one query may hold: it costs gozd 2^n queries.
pub const MAX_CONTENT_TERMS: usize = 3;

/// What a content term becomes in the query that takes it as true: a name
/// length every name has.
const ALWAYS: &str = "name-length:>=0";
/// What a content term becomes in the query that takes it as false: a name
/// length no name has. Not `<0`: after a numeric function a `<` that a `|`
/// and a `>` follow is a group of values.
const NEVER: &str = "name-length:>18446744073709551615";

/// How a content function reads a file's bytes as text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ContentEncoding {
    /// `content:`: as Everything picks for the file (see the module docs).
    Auto,
    /// `utf8content:`.
    Utf8,
    /// `utf16content:` (`unicode-content:`, `utf16le-content:`,
    /// `wchar-content:`, `contentw:`, `wcontent:`).
    Utf16Le,
    /// `utf16becontent:`.
    Utf16Be,
    /// `ansicontent:` (`contenta:`): the system's ANSI code page.
    Ansi,
}

impl ContentEncoding {
    /// The function as an error names it.
    pub(super) fn spelling(self) -> &'static str {
        match self {
            ContentEncoding::Auto => "content:",
            ContentEncoding::Utf8 => "utf8content:",
            ContentEncoding::Utf16Le => "utf16content:",
            ContentEncoding::Utf16Be => "utf16becontent:",
            ContentEncoding::Ansi => "ansicontent:",
        }
    }
}

/// A query with its content terms split out: the queries gozd runs, and
/// the terms the client searches files for.
#[derive(Debug)]
pub struct ContentQuery {
    queries: Vec<String>,
    terms: Vec<ContentTerm>,
}

impl ContentQuery {
    /// Splits `query`, whose text matches case unless `match_case` is off
    /// and no `case:` says otherwise, as gozd would read it. Fails as gozd
    /// would on a malformed query, and with
    /// [`QueryError::TooManyContentTerms`] for more than
    /// [`MAX_CONTENT_TERMS`] content terms. A query with none gives itself.
    pub fn new(query: &str, match_case: bool) -> Result<ContentQuery, QueryError> {
        let found = parse_content_terms(query, match_case)?;
        if found.len() > MAX_CONTENT_TERMS {
            return Err(QueryError::TooManyContentTerms(found.len()));
        }
        let mut terms = Vec::with_capacity(found.len());
        for f in &found {
            terms.push(ContentTerm::new(f.encoding, &f.value)?);
        }
        // Byte offsets of the spans, which the parser counts in characters.
        let offsets: Vec<usize> = query
            .char_indices()
            .map(|(at, _)| at)
            .chain([query.len()])
            .collect();
        let spans: Vec<(usize, usize)> = found
            .iter()
            .map(|f| (offsets[f.span.0], offsets[f.span.1]))
            .collect();
        let queries = (0..1usize << found.len())
            .map(|a| {
                let mut out = String::with_capacity(query.len() + 16 * spans.len());
                let mut from = 0;
                for (i, &(start, end)) in spans.iter().enumerate() {
                    out.push_str(&query[from..start]);
                    out.push_str(if a >> i & 1 == 1 { ALWAYS } else { NEVER });
                    from = end;
                }
                out.push_str(&query[from..]);
                out
            })
            .collect();
        Ok(ContentQuery { queries, terms })
    }

    /// The queries for gozd, 2^n of them: query `a` takes term `i` as true
    /// when bit `i` of `a` is set.
    pub fn queries(&self) -> &[String] {
        &self.queries
    }

    /// The content terms, in query order.
    pub fn terms(&self) -> &[ContentTerm] {
        &self.terms
    }

    /// Whether the query holds no content term, so its one query is itself.
    pub fn is_plain(&self) -> bool {
        self.terms.is_empty()
    }

    /// The verdict for an entry that matched the queries whose bits are set
    /// in `matched` (bit `a` for [`Self::queries`]`[a]`), when it needs no
    /// content answer: `false` when it matched none, `true` when it matched
    /// all. `None` when it depends on the entry's content.
    pub fn decided(&self, matched: u8) -> Option<bool> {
        let all = all_bits(self.queries.len());
        let m = u16::from(matched) & all;
        if m == 0 {
            Some(false)
        } else if m == all {
            Some(true)
        } else {
            None
        }
    }

    /// The entry's verdict under the original query, exactly: the bit of
    /// `matched` for the query whose constants are the entry's content
    /// answers. `answer(i)` gives term `i`'s answer for the entry, and is
    /// asked only while the queries still left disagree, in term order, so
    /// a file is read only for a term that can change the verdict.
    pub fn verdict(&self, matched: u8, mut answer: impl FnMut(usize) -> bool) -> bool {
        let n = self.terms.len();
        let mut known = 0usize;
        let mut value = 0usize;
        for i in 0..=n {
            let mut seen = [false; 2];
            for a in 0..1usize << n {
                if a & known == value {
                    seen[usize::from(matched >> a & 1 == 1)] = true;
                }
            }
            if !(seen[0] && seen[1]) {
                return seen[1];
            }
            if i == n {
                break;
            }
            known |= 1 << i;
            if answer(i) {
                value |= 1 << i;
            }
        }
        matched >> value & 1 == 1
    }
}

/// The low `count` bits set: a mask over `count` queries.
fn all_bits(count: usize) -> u16 {
    ((1u32 << count) - 1) as u16
}

/// How a probe's needle and text are folded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fold {
    /// `case:`: as written.
    Raw,
    /// The default: case and accents ignored.
    Folded,
    /// `diacritics:`: case ignored, accents kept.
    Exact,
}

/// One text or `regex:` value of a content term.
#[derive(Debug)]
enum Probe {
    Text {
        needle: Vec<u8>,
        fold: Fold,
        anchors: Anchors,
        skip: Skip,
    },
    Regex(RegexTerm),
}

/// A content term's value as a formula over its probes.
#[derive(Clone, Debug)]
enum Node {
    Const(bool),
    And(Vec<Node>),
    Or(Vec<Node>),
    Not(Box<Node>),
    Probe(usize),
}

impl Node {
    /// The value, a probe found or not as `found` says.
    fn eval(&self, found: &dyn Fn(usize) -> bool) -> bool {
        match self {
            Node::Const(b) => *b,
            Node::And(items) => items.iter().all(|n| n.eval(found)),
            Node::Or(items) => items.iter().any(|n| n.eval(found)),
            Node::Not(inner) => !inner.eval(found),
            Node::Probe(i) => found(*i),
        }
    }

    /// The value while the probes not `found` yet may still be found:
    /// `None` until reading on can no longer change it.
    fn settled(&self, found: &[bool]) -> Option<bool> {
        match self {
            Node::Const(b) => Some(*b),
            Node::And(items) => {
                let mut all = Some(true);
                for n in items {
                    match n.settled(found) {
                        Some(false) => return Some(false),
                        Some(true) => {}
                        None => all = None,
                    }
                }
                all
            }
            Node::Or(items) => {
                let mut any = Some(false);
                for n in items {
                    match n.settled(found) {
                        Some(true) => return Some(true),
                        Some(false) => {}
                        None => any = None,
                    }
                }
                any
            }
            Node::Not(inner) => inner.settled(found).map(|v| !v),
            Node::Probe(i) => found[*i].then_some(true),
        }
    }
}

/// One content term: how it reads a file, and what its value asks.
#[derive(Debug)]
pub struct ContentTerm {
    encoding: ContentEncoding,
    node: Node,
    probes: Vec<Probe>,
}

/// How Windows reads text out of files of one extension, which decides
/// what `content:` reads in a file that is no plain text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextFilter {
    /// No filter is registered: the bytes are searched.
    None,
    /// The null filter, which gives no text (`.bin`, `.exe`, `.zip`).
    Null,
    /// A real filter (PDF, Office), which goz does not run: the bytes are
    /// searched.
    Other,
}

/// What a content search needs from the system.
pub trait ContentEnv {
    /// Appends `bytes` read in the system's ANSI code page to `out`.
    fn ansi(&self, bytes: &[u8], out: &mut String);
    /// How Windows reads files of extension `ext` (lower case, no dot).
    fn filter(&self, ext: &str) -> TextFilter;
    /// Whether to stop reading.
    fn cancelled(&self) -> bool;
}

/// Bytes read at a time, as Everything's `content_buf_size`.
pub const WINDOW: usize = 8 << 20;
/// Bytes each window repeats of the one before. Even, so UTF-16 keeps its
/// alignment from window to window.
pub const OVERLAP: usize = 256 << 10;

/// Everything 1.5.0.1424b's `text_plain_extensions`: the files `content:`
/// reads as plain text.
const TEXT_PLAIN_EXTENSIONS: &str = "a;ahk;ans;asc;ascx;asm;asmx;asp;aspx;asx;bas;bat;bcp;btm;c;\
    cc;cd;cls;cmd;cod;contact;cpp;cs;csa;csh;cshader;csproj;css;csv;cxx;dbs;def;dic;diz;dos;dsh;\
    dshader;dsp;dsw;efu;ext;faq;filters;fky;fx;gsh;gshader;h;hh;hhc;hlsl;hlsli;hpp;hsh;hshader;\
    hta;htm;html;htt;htw;htx;hxx;i;ibq;ics;idl;idq;inc;inf;ini;inl;inx;ipp;jav;java;js;json;\
    jsproj;kci;lgn;log;lst;lua;m3u;mak;map;md;mdp;mk;nfo;nvr;odc;odh;odl;php;php3;pkgdef;\
    pkgundef;pl;plg;prc;ps1xml;psh;pshader;pssc;py;rc;rc2;rct;reg;res;rgs;rul;s;scc;sed;shtm;\
    shtml;sln;slnf;sol;sql;srf;stm;tab;tdl;text;tlh;tli;trg;tsv;tt;txt;udf;udt;url;user;usr;vb;\
    vbproj;vbs;vcp;vcproj;vcw;vcxproj;viw;vsh;vshader;vspscc;vsscc;vssscc;wri;wtx;x;xml;xsd;xsl;\
    xslt;yaml";

/// The extension of a file name, lower case, for [`TextFilter`] and the
/// plain-text list: what follows its last dot, empty for none.
pub fn extension_of(name: &str) -> String {
    name.rsplit_once('.')
        .map_or_else(String::new, |(_, ext)| ext.to_lowercase())
}

/// Whether `content:` reads files of extension `ext` (lower case) as plain
/// text.
pub fn is_text_plain(ext: &str) -> bool {
    !ext.is_empty() && TEXT_PLAIN_EXTENSIONS.split(';').any(|e| e == ext)
}

/// One way of reading a window's bytes as text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Decode {
    Utf8,
    Ansi,
    /// UTF-16, big-endian or not, from the window's first or second byte.
    Utf16 {
        big: bool,
        from: usize,
    },
}

impl ContentTerm {
    fn new(encoding: ContentEncoding, value: &Expr) -> Result<ContentTerm, QueryError> {
        let mut probes = Vec::new();
        let node = node_of(value, &mut probes)?;
        Ok(ContentTerm {
            encoding,
            node,
            probes,
        })
    }

    /// How the term reads a file.
    pub fn encoding(&self) -> ContentEncoding {
        self.encoding
    }

    /// Whether the content of the file named `name`, read from `file`,
    /// matches the term. `Ok(false)` once `env` is cancelled; an error only
    /// from reading.
    pub fn search(
        &self,
        file: &mut dyn Read,
        name: &str,
        env: &dyn ContentEnv,
    ) -> io::Result<bool> {
        self.search_in(file, name, env, WINDOW, OVERLAP)
    }

    /// [`Self::search`] in windows of `window` bytes that overlap by
    /// `overlap` (even, and less than `window`).
    fn search_in(
        &self,
        file: &mut dyn Read,
        name: &str,
        env: &dyn ContentEnv,
        window: usize,
        overlap: usize,
    ) -> io::Result<bool> {
        // Each window moves on, and keeps UTF-16 aligned.
        let overlap = overlap.min(window / 2) & !1;
        let ext = extension_of(name);
        let mut found = vec![false; self.probes.len()];
        if let Some(v) = self.node.settled(&found) {
            return Ok(v);
        }
        let mut plan: Option<Vec<Decode>> = None;
        if self.encoding == ContentEncoding::Auto
            && !is_text_plain(&ext)
            && env.filter(&ext) == TextFilter::Null
        {
            // No text: every probe stays unfound.
            return Ok(self.node.eval(&|_| false));
        }
        let mut buf: Vec<u8> = Vec::with_capacity(window.min(1 << 20));
        let mut start = 0u64;
        let matchers: Vec<Option<TextMatcher<'_>>> = self
            .probes
            .iter()
            .map(|p| match p {
                Probe::Text {
                    needle,
                    anchors,
                    skip,
                    ..
                } => Some(TextMatcher::new(needle, *anchors, *skip)),
                Probe::Regex(_) => None,
            })
            .collect();
        let mut cps = CodePointBuf::new();
        let budget = Budget::unlimited();
        loop {
            if env.cancelled() {
                return Ok(false);
            }
            // Fill the window.
            // `read_to_end` grows the buffer as the bytes come, so a small
            // file costs a small one.
            let want = (window - buf.len()) as u64;
            let got = (&mut *file).take(want).read_to_end(&mut buf)?;
            let eof = (got as u64) < want;
            let first = start == 0;
            let decodes = plan.get_or_insert_with(|| self.decodes(&buf, &ext));
            let mut window = &buf[..];
            if first {
                window = &window[bom_len(self.encoding, &ext, decodes, window)..];
            }
            let open = Open {
                start: !first,
                end: !eof,
            };
            for &decode in decodes.iter() {
                // An all-ASCII window reads in ANSI as in UTF-8.
                if decode == Decode::Ansi && decodes.len() > 1 && window.is_ascii() {
                    continue;
                }
                let text = decode_window(decode, window, open.start, env);
                self.probe_text(&text, open, &matchers, &mut found, &mut cps, &budget);
                if let Some(v) = self.node.settled(&found) {
                    return Ok(v);
                }
            }
            if eof {
                return Ok(self.node.eval(&|i| found[i]));
            }
            // Keep the overlap and read on.
            let keep = buf.len().min(overlap);
            let drop = buf.len() - keep;
            buf.drain(..drop);
            start += drop as u64;
        }
    }

    /// The decodings of a file whose first window is `head`.
    fn decodes(&self, head: &[u8], ext: &str) -> Vec<Decode> {
        match self.encoding {
            ContentEncoding::Utf8 => vec![Decode::Utf8],
            ContentEncoding::Ansi => vec![Decode::Ansi],
            ContentEncoding::Utf16Le => vec![Decode::Utf16 {
                big: false,
                from: 0,
            }],
            ContentEncoding::Utf16Be => vec![Decode::Utf16 { big: true, from: 0 }],
            ContentEncoding::Auto if is_text_plain(ext) => vec![text_plain_decode(head)],
            ContentEncoding::Auto => {
                let mut all = vec![Decode::Utf8, Decode::Ansi];
                for big in [false, true] {
                    for from in [0, 1] {
                        all.push(Decode::Utf16 { big, from });
                    }
                }
                all
            }
        }
    }

    /// Tests the probes not found yet against one window's `text`.
    fn probe_text(
        &self,
        text: &str,
        open: Open,
        matchers: &[Option<TextMatcher<'_>>],
        found: &mut [bool],
        cps: &mut CodePointBuf,
        budget: &Budget,
    ) {
        let mut folded: Option<Vec<u8>> = None;
        let mut exact: Option<Vec<u8>> = None;
        for (i, probe) in self.probes.iter().enumerate() {
            if found[i] {
                continue;
            }
            found[i] = match probe {
                Probe::Regex(regex) => regex.is_match(text.as_bytes()),
                Probe::Text { fold: how, .. } => {
                    let hay: &[u8] = match how {
                        Fold::Raw => text.as_bytes(),
                        Fold::Folded => folded.get_or_insert_with(|| fold(text.as_bytes())),
                        Fold::Exact => exact.get_or_insert_with(|| fold_exact(text.as_bytes())),
                    };
                    matchers[i]
                        .as_ref()
                        .is_some_and(|m| m.matches_within(hay, cps, budget, open))
                }
            };
        }
    }

    /// Whether `bytes`, the whole content of the file `name`, matches: the
    /// tests' and small callers' form of [`Self::search`].
    pub fn search_bytes(&self, bytes: &[u8], name: &str, env: &dyn ContentEnv) -> bool {
        self.search(&mut io::Cursor::new(bytes), name, env)
            .unwrap_or(false)
    }
}

/// How many bytes of a byte order mark start the file's first window, for
/// the decodings that drop one.
fn bom_len(encoding: ContentEncoding, ext: &str, decodes: &[Decode], head: &[u8]) -> usize {
    // The bytes of a file that is no plain text are searched as they are.
    if encoding == ContentEncoding::Auto && !is_text_plain(ext) {
        return 0;
    }
    match decodes.first() {
        Some(Decode::Utf8) if head.starts_with(b"\xEF\xBB\xBF") => 3,
        Some(Decode::Utf16 { big: false, .. }) if head.starts_with(b"\xFF\xFE") => 2,
        Some(Decode::Utf16 { big: true, .. }) if head.starts_with(b"\xFE\xFF") => 2,
        _ => 0,
    }
}

/// How `content:` reads a plain-text file that starts with `head`: by its
/// byte order mark, else UTF-16LE when every other byte is mostly zero,
/// else UTF-8 when it is valid UTF-8, else ANSI.
fn text_plain_decode(head: &[u8]) -> Decode {
    if head.starts_with(b"\xEF\xBB\xBF") {
        return Decode::Utf8;
    }
    if head.starts_with(b"\xFF\xFE") {
        return Decode::Utf16 {
            big: false,
            from: 0,
        };
    }
    if head.starts_with(b"\xFE\xFF") {
        return Decode::Utf16 { big: true, from: 0 };
    }
    if looks_utf16le(head) {
        return Decode::Utf16 {
            big: false,
            from: 0,
        };
    }
    if valid_utf8_prefix(head) {
        Decode::Utf8
    } else {
        Decode::Ansi
    }
}

/// Whether text without a mark reads as UTF-16LE: in its first 4 KiB, at
/// least half the odd bytes are zero, and at most a tenth of the even ones.
fn looks_utf16le(head: &[u8]) -> bool {
    let sample = &head[..head.len().min(4096) & !1];
    let pairs = sample.len() / 2;
    if pairs == 0 {
        return false;
    }
    let (pairs_of, _) = sample.as_chunks::<2>();
    let odd_zero = pairs_of.iter().filter(|p| p[1] == 0).count();
    let even_zero = pairs_of.iter().filter(|p| p[0] == 0).count();
    odd_zero * 2 >= pairs && even_zero * 10 <= pairs
}

/// Whether `head` is valid UTF-8, but for a sequence the window's end cut.
fn valid_utf8_prefix(head: &[u8]) -> bool {
    match std::str::from_utf8(head) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none() && head.len() - e.valid_up_to() < 4,
    }
}

/// Reads `window` as text the way `decode` says. A window that starts in
/// the middle of the file (`open_start`) first passes over a character it
/// starts in the middle of, so no broken character stands before a match.
fn decode_window(decode: Decode, window: &[u8], open_start: bool, env: &dyn ContentEnv) -> String {
    match decode {
        Decode::Utf8 => {
            let mut w = window;
            if open_start {
                let skip = w.iter().take(3).take_while(|&&b| b & 0xC0 == 0x80).count();
                w = &w[skip..];
            }
            String::from_utf8_lossy(w).into_owned()
        }
        Decode::Ansi => {
            let mut out = String::with_capacity(window.len());
            env.ansi(window, &mut out);
            out
        }
        Decode::Utf16 { big, from } => {
            let w = window.get(from..).unwrap_or_default();
            let mut units = w.as_chunks::<2>().0.iter().map(|p| {
                if big {
                    u16::from_be_bytes([p[0], p[1]])
                } else {
                    u16::from_le_bytes([p[0], p[1]])
                }
            });
            let mut out = String::with_capacity(w.len() / 2);
            let mut pending: Option<u16> = None;
            if open_start {
                let first = units.next();
                match first {
                    Some(u) if (0xDC00..0xE000).contains(&u) => {}
                    other => pending = other,
                }
            }
            let rest = pending.into_iter().chain(units);
            for c in char::decode_utf16(rest) {
                out.push(c.unwrap_or(char::REPLACEMENT_CHARACTER));
            }
            out
        }
    }
}

/// The formula of a content term's value, its text and `regex:` leaves
/// pushed to `probes`.
fn node_of(e: &Expr, probes: &mut Vec<Probe>) -> Result<Node, QueryError> {
    Ok(match e {
        Expr::True => Node::Const(true),
        Expr::False => Node::Const(false),
        Expr::And(items) => Node::And(
            items
                .iter()
                .map(|i| node_of(i, probes))
                .collect::<Result<_, _>>()?,
        ),
        Expr::Or(items) => Node::Or(
            items
                .iter()
                .map(|i| node_of(i, probes))
                .collect::<Result<_, _>>()?,
        ),
        Expr::Not(inner) => Node::Not(Box::new(node_of(inner, probes)?)),
        // Content is only ever read from a file.
        Expr::Leaf(Leaf::Kind(kind)) => Node::Const(*kind == Kind::File),
        Expr::Leaf(Leaf::Text {
            needle,
            case,
            diacritics,
            anchors,
            skip,
            ..
        }) => {
            probes.push(Probe::Text {
                needle: needle.clone(),
                fold: if *case {
                    Fold::Raw
                } else if *diacritics {
                    Fold::Exact
                } else {
                    Fold::Folded
                },
                anchors: *anchors,
                skip: *skip,
            });
            Node::Probe(probes.len() - 1)
        }
        Expr::Leaf(Leaf::Regex { regex, .. }) => {
            probes.push(Probe::Regex(regex.clone()));
            Node::Probe(probes.len() - 1)
        }
        Expr::Leaf(_) => {
            return Err(QueryError::Malformed {
                what: "content:",
                detail: "a content: value is text: only text, regex: and their modifiers \
                         stand in it"
                    .to_string(),
            });
        }
    })
}

/// Orders the rows of a content search as gozd orders a query's: by the
/// key `sort` names, then the folded name, then the folded path, the whole
/// order reversed for a descending sort. A path sort is by folded folder
/// path, then name. The client sorts these rows itself, since gozd never
/// sees which of its candidates matched.
pub fn sort_items(items: &mut [ResultItem], sort: SortSpec) {
    /// What a row sorts by: a number, then three byte strings.
    type Key = (u64, Vec<u8>, Vec<u8>, Vec<u8>);
    let key = |item: &ResultItem| -> Key {
        let path = fold(item.path.as_bytes());
        let cut = path.iter().rposition(|&b| b == b'\\');
        let name = cut.map_or(&path[..], |i| &path[i + 1..]).to_vec();
        match sort.key {
            SortKey::Name => (0, Vec::new(), name, path),
            SortKey::Path => {
                let parent = cut.map_or(Vec::new(), |i| path[..i].to_vec());
                (0, parent, name, path)
            }
            SortKey::Size => {
                let size = if item.is_dir { None } else { item.size };
                (size_key(size, sort.dir), Vec::new(), name, path)
            }
            SortKey::DateModified => (mtime_key(item.mtime_ft, sort.dir), Vec::new(), name, path),
            SortKey::Extension => {
                let ext = sort_extension(&name, item.is_dir).to_vec();
                (extension_key(&name, item.is_dir), ext, name, path)
            }
        }
    };
    let mut keyed: Vec<(Key, usize)> = items
        .iter()
        .enumerate()
        .map(|(i, it)| (key(it), i))
        .collect();
    keyed.sort();
    if sort.dir == SortDir::Desc {
        keyed.reverse();
    }
    let order: Vec<usize> = keyed.into_iter().map(|(_, i)| i).collect();
    let sorted: Vec<ResultItem> = order.iter().map(|&i| items[i].clone()).collect();
    for (slot, item) in items.iter_mut().zip(sorted) {
        *slot = item;
    }
}

#[cfg(test)]
mod tests;
