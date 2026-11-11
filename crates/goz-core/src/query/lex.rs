//! The tokens of a query, read as Everything 1.5 reads them: search text,
//! the operators `|` (or `or:`), `!` (or `not:`) and `and:`, groups `< >`,
//! and the prefixes of search modifiers (`case:`, `path:`, `ww:`, ...),
//! search functions (`ext:`, `size:`, `parent:`, ...) and macros (`audio:`,
//! ...).
//!
//! White space separates terms, except inside quotes; a quote toggles
//! quoting anywhere in a term, and quoted text is literal. Outside quotes,
//! `|` ends a term and is an operator wherever it stands; `!` is the NOT
//! operator only where a term starts (`wow!.txt` is a name); `<` opens a
//! group only where a term starts, and inside a word it is an error, since
//! Everything's group expansion (`gr<a|e>y`) is not supported; `>` closes
//! the innermost group wherever it stands. A character entity (`&vert:`,
//! `&#x21:`, ...) stands for one literal character, in quotes too, as
//! Everything replaces them there as well.
//!
//! A prefix's name is read lower-cased with its hyphens dropped, as
//! Everything's are optional (`no-case:` is `nocase:`), and `no` in front of
//! a modifier switches it off. `::name:` names the same prefix (Everything's
//! way to bypass a user's macro of that name); `?name:` applies a modifier
//! to every term after it and leaves no token of its own; `name*:` takes
//! the rest of the query, verbatim, as its text.
//!
//! After a function's prefix comes its value: for a number or a date, any
//! `!`s of its own, as NOT tokens (`size:!1kb`); elsewhere a `!` is text,
//! as Everything reads `ext:!txt` and `name:!x`. Then either text, whose
//! unquoted `;`s separate a list of values, or a value group `<...>`, lexed
//! as any group is. After a
//! numeric function (`size:`, `dm:`, `len:`, ...) a `<` is a comparison
//! (`size:<1kb`), unless a `>` closes it within the same white-space-free
//! run, as in Everything, around values a `|` separates (`size:<1kb|2kb>`).
//!
//! A `regex:` term's value is read as Everything's parser eats it: up to
//! unquoted white space, `|`, `;` and `<` included, and `>` too unless it
//! closes an open group; `?regex:` reads every later term's text so.
//!
//! Three directives are read here and leave no token: `case:` and
//! `matchpath:` with no text, or with `0` or `1`, whole-query switches as
//! goz has always had them (`case:Foo` is the per-term modifier), and
//! `?and-precedence:` / `?or-precedence:`, which set how tightly AND binds
//! for the operators after them. `no-!:` makes the `!` of the term it
//! prefixes literal, and `?no-!:` of every term after it.

use super::parse::{Kind, QueryError};
use crate::wtf8::encode_code_point;

/// One token, with where it stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Token {
    pub(super) kind: Tok,
    /// The 1-based character column it starts at, for errors.
    pub(super) column: usize,
    /// Whether white space separates it from the token before it.
    pub(super) spaced: bool,
    /// Whether AND binds tighter than OR where it stands: a
    /// `?and-precedence:` came before it, and no `?or-precedence:` since.
    pub(super) and_first: bool,
    /// The modifiers `?mod:` prefixes before it set for the rest of the
    /// search.
    pub(super) global: ModSet,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Tok {
    /// `|` or `or:`.
    Or,
    /// `and:`; white space between terms is an AND too, with no token.
    And,
    /// `!` where a term starts, or `not:`.
    Not,
    /// `<` where a term starts.
    Open,
    /// `>` outside quotes.
    Close,
    /// A modifier, function or macro prefix, its colon included. A
    /// function's value follows, after any `!`s of its own (`ext:!pdf`).
    Prefix(Prefix),
    /// Search text, or a function's value.
    Text(Text),
}

/// A search modifier: how a term's text is matched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mod {
    /// `case:`: match case.
    Case,
    /// `path:` (`\:`): match the full path, not the name.
    Path,
    /// `whole-words:` (`ww:`): the match is whole words.
    WholeWords,
    /// `prefix:`: the match starts a word.
    WordStart,
    /// `suffix:`: the match ends a word.
    WordEnd,
    /// `whole:` (`exact:`, `wfn:`, `entire:`): the match is the whole text.
    Whole,
    /// `start-with:` (`^:`): the match starts the text.
    StartWith,
    /// `end-with:` (`$:`): the match ends the text.
    EndWith,
    /// `ignore-punctuation:` (`nopunc:`): punctuation in the text may be
    /// skipped.
    SkipPunct,
    /// `ignore-whitespace:` (`nows:`): white space in the text may be
    /// skipped.
    SkipSpace,
    /// `star-all:`: a wildcard's `*` crosses a separator too.
    StarAll,
    /// Filename wildcards; `no-wildcards:` makes `*` and `?` literal.
    Wildcards,
    /// `metric:`: size units of 1000.
    Metric,
    /// `diacritics:` (`diac:`): match accents; by default they are
    /// ignored.
    Diacritics,
    /// `regex:`: the text is a regular expression.
    Regex,
}

/// The modifiers a term may have, each switched on or off, or left to the
/// default: what the prefixes around a term set, and what `?mod:` sets for
/// the rest of a search.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct ModSet {
    pub(super) case: Option<bool>,
    pub(super) path: Option<bool>,
    pub(super) whole_words: Option<bool>,
    pub(super) word_start: Option<bool>,
    pub(super) word_end: Option<bool>,
    pub(super) whole: Option<bool>,
    pub(super) start_with: Option<bool>,
    pub(super) end_with: Option<bool>,
    pub(super) skip_punct: Option<bool>,
    pub(super) skip_space: Option<bool>,
    pub(super) star_all: Option<bool>,
    pub(super) wildcards: Option<bool>,
    pub(super) metric: Option<bool>,
    pub(super) diacritics: Option<bool>,
    pub(super) regex: Option<bool>,
    /// `?file:` or `?folder:`: the kind of every term after it.
    pub(super) kind: Option<Kind>,
}

impl ModSet {
    fn slot(&mut self, m: Mod) -> &mut Option<bool> {
        match m {
            Mod::Case => &mut self.case,
            Mod::Path => &mut self.path,
            Mod::WholeWords => &mut self.whole_words,
            Mod::WordStart => &mut self.word_start,
            Mod::WordEnd => &mut self.word_end,
            Mod::Whole => &mut self.whole,
            Mod::StartWith => &mut self.start_with,
            Mod::EndWith => &mut self.end_with,
            Mod::SkipPunct => &mut self.skip_punct,
            Mod::SkipSpace => &mut self.skip_space,
            Mod::StarAll => &mut self.star_all,
            Mod::Wildcards => &mut self.wildcards,
            Mod::Metric => &mut self.metric,
            Mod::Diacritics => &mut self.diacritics,
            Mod::Regex => &mut self.regex,
        }
    }

    /// Switches `m` on or off, unless a prefix further out did already:
    /// Everything applies a term's modifiers right to left, so the outermost
    /// wins (`case:nocase:Foo` matches case).
    pub(super) fn set_outer(&mut self, m: Mod, on: bool) {
        let slot = self.slot(m);
        if slot.is_none() {
            *slot = Some(on);
        }
    }

    /// `self`, the modifiers further out, over `inner`: each of `self`'s
    /// that is set wins.
    pub(super) fn over(self, inner: ModSet) -> ModSet {
        ModSet {
            case: self.case.or(inner.case),
            path: self.path.or(inner.path),
            whole_words: self.whole_words.or(inner.whole_words),
            word_start: self.word_start.or(inner.word_start),
            word_end: self.word_end.or(inner.word_end),
            whole: self.whole.or(inner.whole),
            start_with: self.start_with.or(inner.start_with),
            end_with: self.end_with.or(inner.end_with),
            skip_punct: self.skip_punct.or(inner.skip_punct),
            skip_space: self.skip_space.or(inner.skip_space),
            star_all: self.star_all.or(inner.star_all),
            wildcards: self.wildcards.or(inner.wildcards),
            metric: self.metric.or(inner.metric),
            diacritics: self.diacritics.or(inner.diacritics),
            regex: self.regex.or(inner.regex),
            kind: self.kind.or(inner.kind),
        }
    }
}

/// A search function: its value follows its prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Func {
    /// `ext:`: the extension is one of a list.
    Ext,
    /// `extension:`: the extension holds the text.
    Extension,
    /// `size:`.
    Size,
    /// `dm:` (`date:`, `date-modified:`).
    Date,
    /// `len:`: the name's length, or the full path's under `path:`.
    Len,
    /// `name-length:`.
    NameLen,
    /// `full-path-length:`.
    PathLen,
    /// `path-part-length:`: the location's length.
    LocationLen,
    /// `extension-length:`.
    ExtLen,
    /// `parent:` (`infolder:`, `nosubfolders:`): direct children of a folder.
    Parent,
    /// `ancestor:`: everything below a folder.
    Ancestor,
    /// `name:` (`basename:`): the name, even under `path:`.
    Name,
    /// `stem:`: the name without its extension.
    Stem,
    /// `full-path:` (`path-and-name:`, `path-name:`).
    FullPath,
    /// `location:` (`loc:`, `path-part:`, `pp:`): the parent folder's path.
    Location,
    /// `parent-name:`: the parent folder's name.
    ParentName,
    /// `parent-count:` (`parents:`, `depth:`): how many folders are above
    /// the entry.
    ParentCount,
    /// `child-count:`: how many files and folders a folder holds.
    ChildCount,
    /// `child-file-count:`.
    ChildFileCount,
    /// `child-folder-count:`.
    ChildFolderCount,
    /// `child:`: a folder holds a file or folder whose name matches.
    Child,
    /// `child-file:`.
    ChildFile,
    /// `child-folder:`.
    ChildFolder,
    /// `descendant:`: a folder holds, at any depth, an entry whose name
    /// matches.
    Descendant,
    /// `descendant-file:`.
    DescendantFile,
    /// `descendant-folder:`.
    DescendantFolder,
    /// `sibling:`: another entry of the same folder has a name that matches.
    Sibling,
    /// `sibling-file:`.
    SiblingFile,
    /// `sibling-folder:`.
    SiblingFolder,
    /// `ancestor-name:`: a folder above the entry has a name that matches.
    AncestorName,
    /// `parent-child:`: the entry's folder holds an entry whose name
    /// matches, the entry itself included.
    ParentChild,
    /// `parent-child-file:`.
    ParentChildFile,
    /// `parent-child-folder:`.
    ParentChildFolder,
    /// `parent-sibling:`: the folder above the entry's folder holds an
    /// entry whose name matches, the entry's folder itself included.
    ParentSibling,
    /// `parent-sibling-file:`.
    ParentSiblingFile,
    /// `parent-sibling-folder:`.
    ParentSiblingFolder,
    /// `ancestor-child:`: a folder above the entry holds an entry whose
    /// name matches.
    AncestorChild,
    /// `ancestor-child-file:`.
    AncestorChildFile,
    /// `ancestor-child-folder:`.
    AncestorChildFolder,
    /// `ancestor-sibling:`: a folder above the entry's folder holds an
    /// entry whose name matches.
    AncestorSibling,
    /// `ancestor-sibling-file:`.
    AncestorSiblingFile,
    /// `ancestor-sibling-folder:`.
    AncestorSiblingFolder,
    /// `descendant-count:`: how many files and folders are below a folder.
    DescendantCount,
    /// `descendant-file-count:`.
    DescendantFileCount,
    /// `descendant-folder-count:`.
    DescendantFolderCount,
    /// `sibling-count:`: how many other files and folders the entry's
    /// folder holds.
    SiblingCount,
    /// `sibling-file-count:`.
    SiblingFileCount,
    /// `sibling-folder-count:`.
    SiblingFolderCount,
    /// `child-file-size:`: a folder holds a file of that size.
    ChildFileSize,
    /// `child-date-modified:` (`childdm:`): a folder holds a file or folder
    /// modified then.
    ChildDate,
    /// `child-file-date-modified:` (`childfiledm:`).
    ChildFileDate,
    /// `child-folder-date-modified:` (`childfolderdm:`).
    ChildFolderDate,
    /// `total-child-size:`: the sizes of a folder's files, summed.
    TotalChildSize,
}

impl Func {
    /// Whether the value is a number or a date, which may start with a
    /// comparison: `size:>1mb`, `dm:<2020`.
    pub(super) fn compares(self) -> bool {
        matches!(
            self,
            Func::Size
                | Func::Date
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
                | Func::ChildFileSize
                | Func::ChildDate
                | Func::ChildFileDate
                | Func::ChildFolderDate
                | Func::TotalChildSize
        )
    }

    /// Whether the value is text a term matches against a part of the
    /// path, which `regex:` makes a regular expression (`regex:stem:^a`).
    pub(super) fn matches_text(self) -> bool {
        matches!(
            self,
            Func::Extension
                | Func::Name
                | Func::Stem
                | Func::FullPath
                | Func::Location
                | Func::ParentName
                | Func::Child
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
                | Func::AncestorSiblingFolder
        )
    }

    /// The prefix as an error names it.
    pub(super) fn spelling(self) -> &'static str {
        match self {
            Func::Ext => "ext:",
            Func::Extension => "extension:",
            Func::Size => "size:",
            Func::Date => "dm:",
            Func::Len => "len:",
            Func::NameLen => "name-length:",
            Func::PathLen => "full-path-length:",
            Func::LocationLen => "path-part-length:",
            Func::ExtLen => "extension-length:",
            Func::Parent => "parent:",
            Func::Ancestor => "ancestor:",
            Func::Name => "name:",
            Func::Stem => "stem:",
            Func::FullPath => "full-path:",
            Func::Location => "location:",
            Func::ParentName => "parent-name:",
            Func::ParentCount => "parent-count:",
            Func::ChildCount => "child-count:",
            Func::ChildFileCount => "child-file-count:",
            Func::ChildFolderCount => "child-folder-count:",
            Func::Child => "child:",
            Func::ChildFile => "child-file:",
            Func::ChildFolder => "child-folder:",
            Func::Descendant => "descendant:",
            Func::DescendantFile => "descendant-file:",
            Func::DescendantFolder => "descendant-folder:",
            Func::Sibling => "sibling:",
            Func::SiblingFile => "sibling-file:",
            Func::SiblingFolder => "sibling-folder:",
            Func::AncestorName => "ancestor-name:",
            Func::ParentChild => "parent-child:",
            Func::ParentChildFile => "parent-child-file:",
            Func::ParentChildFolder => "parent-child-folder:",
            Func::ParentSibling => "parent-sibling:",
            Func::ParentSiblingFile => "parent-sibling-file:",
            Func::ParentSiblingFolder => "parent-sibling-folder:",
            Func::AncestorChild => "ancestor-child:",
            Func::AncestorChildFile => "ancestor-child-file:",
            Func::AncestorChildFolder => "ancestor-child-folder:",
            Func::AncestorSibling => "ancestor-sibling:",
            Func::AncestorSiblingFile => "ancestor-sibling-file:",
            Func::AncestorSiblingFolder => "ancestor-sibling-folder:",
            Func::DescendantCount => "descendant-count:",
            Func::DescendantFileCount => "descendant-file-count:",
            Func::DescendantFolderCount => "descendant-folder-count:",
            Func::SiblingCount => "sibling-count:",
            Func::SiblingFileCount => "sibling-file-count:",
            Func::SiblingFolderCount => "sibling-folder-count:",
            Func::ChildFileSize => "child-file-size:",
            Func::ChildDate => "child-date-modified:",
            Func::ChildFileDate => "child-file-date-modified:",
            Func::ChildFolderDate => "child-folder-date-modified:",
            Func::TotalChildSize => "total-child-size:",
        }
    }
}

/// One of Everything's filter macros: the files of a list of extensions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Macro {
    Audio,
    Zip,
    Doc,
    Exe,
    Image,
    Video,
}

impl Macro {
    /// The macro's extensions: Everything 1.5.0.1424b's own filters,
    /// extracted from `Everything.exe` (goz-gui's `src/filter.rs` holds the
    /// same lists for its Search menu).
    pub(super) fn extensions(self) -> &'static str {
        match self {
            Macro::Audio => {
                "aac;ac3;adt;adts;aif;aifc;aiff;amr;ape;au;cda;dts;ec3;fla;flac;lpcm;m1a;m2a;\
                 m3u;m3u8;m4a;m4b;m4p;mid;midi;mka;mp2;mp3;mpa;mpc;oga;ogg;opus;ra;rmi;snd;\
                 wav;wax;weba;wma"
            }
            Macro::Zip => {
                "7z;ace;arj;bz2;cab;gz;gzip;jar;r00;r01;r02;r03;r04;r05;r06;r07;r08;\
                 r09;r10;r11;r12;r13;r14;r15;r16;r17;r18;r19;r20;r21;r22;r23;r24;r25;\
                 r26;r27;r28;r29;rar;tar;tgz;z;zip"
            }
            Macro::Doc => {
                "asm;c;cc;chm;cpp;cs;css;csv;cxx;doc;docm;docx;dot;dotm;dotx;efu;epub;h;\
                 hpp;htm;html;hxx;ini;java;js;json;lua;md;mht;mhtml;mobi;odp;ods;odt;ofd;\
                 pdf;php;pl;potm;potx;ppam;pps;ppsm;ppsx;ppt;pptm;pptx;ps1xml;pssc;pub;py;\
                 rtf;sldm;sldx;sql;tsv;txt;vb;vsd;wpd;wps;wri;xlam;xls;xlsb;xlsm;xlsx;\
                 xltm;xltx;xml;xsl"
            }
            Macro::Exe => "bat;cmd;exe;msi;msp;msu;ps1;scr;vbs",
            Macro::Image => {
                "ani;apng;avif;avifs;bmp;bpg;cur;dds;gif;heic;heics;heif;heifs;hif;ico;jfi;\
                 jfif;jif;jpe;jpeg;jpg;jxl;jxr;pcx;png;psb;psd;svg;tga;tif;tiff;wdp;webp;wmf"
            }
            Macro::Video => {
                "3g2;3gp;3gp2;3gpp;amv;asf;asx;avi;bdmv;bik;d2v;divx;drc;dsa;dsm;dss;dsv;evo;\
                 f4v;flc;fli;flic;flv;hdmov;ifo;ivf;m1v;m2p;m2t;m2ts;m2v;m4v;mkv;mod;mov;mp2v;\
                 mp4;mp4v;mpe;mpeg;mpg;mpls;mpv2;mpv4;mts;ogm;ogv;ogx;pss;pva;qt;ram;ratdvd;\
                 rm;rmm;rmvb;roq;rpm;smil;smk;swf;tod;tp;tpr;ts;tts;uvu;vob;vp6;webm;wm;wmp;\
                 wmv;wmx;wvx"
            }
        }
    }
}

/// The prefixes goz knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Prefix {
    /// A modifier switched on or off.
    Mod(Mod, bool),
    /// `file:` (`files:`, `files-only:`, and `no-folder:`): files only.
    File,
    /// `folder:` (`folders:`, `folders-only:`, and `no-file:`): folders
    /// only.
    Folder,
    /// `dir:`, and `no-folder:`: folders only, as `file:` is files only (see
    /// the parser for how the two differ from `folder:` under a `!`).
    Dir,
    /// A filter macro.
    Macro(Macro),
    /// `empty:`: folders that hold nothing, ANDed with what follows as
    /// `dir:` is.
    Empty,
    /// A function, whose value follows.
    Func(Func),
}

/// What a prefix name stands for in the table.
enum Named {
    Prefix(Prefix),
    /// A name Everything knows that goz refuses, with why.
    Refused(&'static str),
    /// One of Everything's functions of related entries that needs what
    /// goz's index does not hold, with why.
    Unsupported(&'static str),
}

/// Why `child-size:` and `child-folder-size:` are refused.
const NO_CHILD_FOLDER_SIZES: &str = "goz's index holds no folder sizes, which Everything \
                                     compares for a child folder (M7); child-file-size: \
                                     tests the files alone";
/// Why `parent-size:` is refused.
const NO_FOLDER_SIZES: &str = "goz's index holds no folder sizes (M7)";
/// Why the date created, accessed, changed and run, run count and
/// attribute functions of related entries are refused.
const NO_SUCH_DATA: &str = "goz's index holds no dates created, accessed, recently changed \
                            or run, no run counts and no attributes (M7); of a file it \
                            holds the size and the date modified";
/// Why the occurrence counts are refused.
const NO_OCCURRENCES: &str = "it counts the hits of the query's child: or descendant: term \
                              in each folder, which goz does not keep; child-count:, \
                              descendant-count: and child: answer the other questions";
/// Why the counts read from disk are refused.
const NO_DISK: &str = "goz answers from its index and never reads a folder from disk; \
                       child-count: counts what the index holds";

impl Prefix {
    /// The prefix `name` stands for, lower-cased with its hyphens dropped,
    /// as Everything's are optional; `no` in front of a modifier's name
    /// switches it off. The table is consulted first, so `nosubfolders:` is
    /// `parent:`, not "no subfolders".
    fn named(name: &str) -> Option<Named> {
        if let Some(named) = Prefix::table(name) {
            return Some(named);
        }
        let rest = name.strip_prefix("no")?;
        Some(match Prefix::table(rest)? {
            Named::Prefix(Prefix::Mod(m, on)) => Named::Prefix(Prefix::Mod(m, !on)),
            // Not files is folders, and not folders files, as Everything
            // reads them, under a `!` too (L7:452-454, L8:98-101): `no-file:`
            // reaches the terms below it as `folder:` does, and `no-folder:`
            // is ANDed with them as `file:` is. `dir:` is a macro in
            // Everything, which takes no `no-` (`no-dir:` finds nothing,
            // L8:96-97), so goz refuses it.
            Named::Prefix(Prefix::File) => Named::Prefix(Prefix::Folder),
            Named::Prefix(Prefix::Folder) => Named::Prefix(Prefix::File),
            // `no-wildcards:` switches filename wildcards off.
            Named::Refused(_) if rest.starts_with("wildcard") => {
                Named::Prefix(Prefix::Mod(Mod::Wildcards, false))
            }
            _ => return None,
        })
    }

    fn table(name: &str) -> Option<Named> {
        use Func as F;
        use Mod as M;
        let on = |m: Mod| Named::Prefix(Prefix::Mod(m, true));
        let off = |m: Mod| Named::Prefix(Prefix::Mod(m, false));
        let func = |f: Func| Named::Prefix(Prefix::Func(f));
        let macro_ = |m: Macro| Named::Prefix(Prefix::Macro(m));
        let unsupported = Named::Unsupported;
        Some(match name {
            "case" => on(M::Case),
            "path" | "\\" => on(M::Path),
            "wholeword" | "wholewords" | "ww" => on(M::WholeWords),
            "prefix" => on(M::WordStart),
            "suffix" => on(M::WordEnd),
            "whole" | "exact" | "wfn" | "wholefilename" | "entire" => on(M::Whole),
            "startwith" | "^" => on(M::StartWith),
            "endwith" | "$" => on(M::EndWith),
            "ignorepunctuation" | "ignorepunc" => on(M::SkipPunct),
            "punc" | "punctuation" => off(M::SkipPunct),
            "ignorewhitespace" | "ignorews" => on(M::SkipSpace),
            "ws" | "whitespace" => off(M::SkipSpace),
            "starall" => on(M::StarAll),
            "filenamewildcards" | "filenamewildcard" => on(M::Wildcards),
            "metric" => on(M::Metric),
            "diacritics" | "diac" => on(M::Diacritics),
            "regex" => on(M::Regex),
            "wildcards" | "wildcard" => Named::Refused(
                "Everything's extended wildcards (wildcards:) are not supported; \
                 * and ? work without it",
            ),
            "file" | "files" | "fileonly" | "filesonly" => Named::Prefix(Prefix::File),
            "folder" | "folders" | "folderonly" | "foldersonly" => Named::Prefix(Prefix::Folder),
            "dir" => Named::Prefix(Prefix::Dir),
            "ext" => func(F::Ext),
            "extension" => func(F::Extension),
            "size" => func(F::Size),
            "dm" | "date" | "datemodified" => func(F::Date),
            "len" => func(F::Len),
            "namelength" | "namelen" => func(F::NameLen),
            "fullpathlength" | "fullpathlen" | "pathlength" | "pathlen" => func(F::PathLen),
            "pathpartlength" | "pathpartlen" => func(F::LocationLen),
            "extensionlength" | "extensionlen" | "extlength" | "extlen" => func(F::ExtLen),
            "parent" | "infolder" | "nosubfolders" => func(F::Parent),
            "ancestor" => func(F::Ancestor),
            "name" | "basename" => func(F::Name),
            "stem" => func(F::Stem),
            "fullpath" | "pathandname" | "pathname" => func(F::FullPath),
            "location" | "loc" | "pathpart" | "pp" => func(F::Location),
            "parentname" => func(F::ParentName),
            "parentcount" | "parents" | "depth" => func(F::ParentCount),
            "childcount" => func(F::ChildCount),
            "childfilecount" => func(F::ChildFileCount),
            "childfoldercount" => func(F::ChildFolderCount),
            "child" | "childname" => func(F::Child),
            "childfile" | "childfilename" => func(F::ChildFile),
            "childfolder" | "childfoldername" => func(F::ChildFolder),
            "descendant" => func(F::Descendant),
            "descendantfile" => func(F::DescendantFile),
            "descendantfolder" => func(F::DescendantFolder),
            "sibling" | "siblingname" => func(F::Sibling),
            "siblingfile" | "siblingfilename" => func(F::SiblingFile),
            "siblingfolder" | "siblingfoldername" => func(F::SiblingFolder),
            "ancestorname" => func(F::AncestorName),
            "parentchild" => func(F::ParentChild),
            "parentchildfile" => func(F::ParentChildFile),
            "parentchildfolder" => func(F::ParentChildFolder),
            "parentsibling" => func(F::ParentSibling),
            "parentsiblingfile" => func(F::ParentSiblingFile),
            "parentsiblingfolder" => func(F::ParentSiblingFolder),
            "ancestorchild" => func(F::AncestorChild),
            "ancestorchildfile" => func(F::AncestorChildFile),
            "ancestorchildfolder" => func(F::AncestorChildFolder),
            "ancestorsibling" => func(F::AncestorSibling),
            "ancestorsiblingfile" => func(F::AncestorSiblingFile),
            "ancestorsiblingfolder" => func(F::AncestorSiblingFolder),
            "descendantcount" => func(F::DescendantCount),
            "descendantfilecount" => func(F::DescendantFileCount),
            "descendantfoldercount" => func(F::DescendantFolderCount),
            "siblingcount" => func(F::SiblingCount),
            "siblingfilecount" => func(F::SiblingFileCount),
            "siblingfoldercount" => func(F::SiblingFolderCount),
            "childfilesize" => func(F::ChildFileSize),
            "childdatemodified" | "childdm" => func(F::ChildDate),
            "childfiledatemodified" | "childfiledm" => func(F::ChildFileDate),
            "childfolderdatemodified" | "childfolderdm" => func(F::ChildFolderDate),
            "totalchildsize" => func(F::TotalChildSize),
            "childsize" => unsupported(NO_CHILD_FOLDER_SIZES),
            "childfoldersize" => unsupported(NO_CHILD_FOLDER_SIZES),
            "parentsize" => unsupported(NO_FOLDER_SIZES),
            "childdatecreated"
            | "childdc"
            | "childfiledatecreated"
            | "childfiledc"
            | "childfolderdatecreated"
            | "childfolderdc"
            | "childdateaccessed"
            | "childda"
            | "childfiledateaccessed"
            | "childfileda"
            | "childfolderdateaccessed"
            | "childfolderda"
            | "childdaterecentlychanged"
            | "childrc"
            | "childfiledaterecentlychanged"
            | "childfilerc"
            | "childfolderdaterecentlychanged"
            | "childfolderrc"
            | "childdaterun"
            | "childfiledaterun"
            | "childfolderdaterun"
            | "childruncount"
            | "childfileruncount"
            | "childfolderruncount"
            | "childattr"
            | "childattrib"
            | "childattribute"
            | "childattributes"
            | "childfileattr"
            | "childfileattrib"
            | "childfileattribute"
            | "childfileattributes"
            | "childfolderattr"
            | "childfolderattrib"
            | "childfolderattribute"
            | "childfolderattributes"
            | "ancestorattr"
            | "ancestorattrib"
            | "ancestorattribute"
            | "ancestorattributes"
            | "parentdatecreated"
            | "parentdc" => unsupported(NO_SUCH_DATA),
            "childoccurrencecount" => unsupported(NO_OCCURRENCES),
            "descendantoccurrencecount" => unsupported(NO_OCCURRENCES),
            "childcountfromdisk" | "childfilecountfromdisk" | "childfoldercountfromdisk" => {
                unsupported(NO_DISK)
            }
            "empty" => Named::Prefix(Prefix::Empty),
            "audio" => macro_(Macro::Audio),
            "zip" => macro_(Macro::Zip),
            "doc" => macro_(Macro::Doc),
            "exe" => macro_(Macro::Exe),
            "image" | "pic" => macro_(Macro::Image),
            "video" => macro_(Macro::Video),
            _ => return None,
        })
    }

    /// The prefix as an error or a test names it.
    pub(super) fn spelling(self) -> &'static str {
        match self {
            Prefix::Mod(m, on) => match (m, on) {
                (Mod::Case, true) => "case:",
                (Mod::Case, false) => "nocase:",
                (Mod::Path, true) => "path:",
                (Mod::Path, false) => "nopath:",
                (Mod::WholeWords, true) => "ww:",
                (Mod::WholeWords, false) => "noww:",
                (Mod::WordStart, true) => "prefix:",
                (Mod::WordStart, false) => "noprefix:",
                (Mod::WordEnd, true) => "suffix:",
                (Mod::WordEnd, false) => "nosuffix:",
                (Mod::Whole, true) => "whole:",
                (Mod::Whole, false) => "nowhole:",
                (Mod::StartWith, true) => "startwith:",
                (Mod::StartWith, false) => "nostartwith:",
                (Mod::EndWith, true) => "endwith:",
                (Mod::EndWith, false) => "noendwith:",
                (Mod::SkipPunct, true) => "nopunc:",
                (Mod::SkipPunct, false) => "punc:",
                (Mod::SkipSpace, true) => "nows:",
                (Mod::SkipSpace, false) => "ws:",
                (Mod::StarAll, true) => "starall:",
                (Mod::StarAll, false) => "nostarall:",
                (Mod::Wildcards, true) => "filenamewildcards:",
                (Mod::Wildcards, false) => "nowildcards:",
                (Mod::Metric, true) => "metric:",
                (Mod::Metric, false) => "nometric:",
                (Mod::Diacritics, true) => "diacritics:",
                (Mod::Diacritics, false) => "nodiacritics:",
                (Mod::Regex, true) => "regex:",
                (Mod::Regex, false) => "noregex:",
            },
            Prefix::File => "file:",
            Prefix::Folder => "folder:",
            Prefix::Dir => "dir:",
            Prefix::Empty => "empty:",
            Prefix::Macro(m) => match m {
                Macro::Audio => "audio:",
                Macro::Zip => "zip:",
                Macro::Doc => "doc:",
                Macro::Exe => "exe:",
                Macro::Image => "image:",
                Macro::Video => "video:",
            },
            Prefix::Func(f) => f.spelling(),
        }
    }
}

/// Search text or a function's value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Text {
    /// WTF-8, quotes removed and entities replaced: an entity can name a
    /// lone surrogate, which a name can hold.
    pub(super) text: Vec<u8>,
    /// `&ast:` or `&quest:` stood in it, which makes every `*` and `?` of
    /// the term literal, as in Everything.
    pub(super) literal_wild: bool,
    /// Where in `text` an unquoted `;` stands: a function's list of values
    /// splits there (`ext:txt;"a;b"` is `txt` and `a;b`).
    pub(super) splits: Vec<usize>,
}

impl Text {
    /// The values of a function's list: `text` split at its unquoted `;`s.
    pub(super) fn items(&self) -> impl Iterator<Item = &[u8]> {
        let mut start = 0;
        let ends = self.splits.iter().copied().chain([self.text.len()]);
        ends.map(move |end| {
            let item = &self.text[start..end];
            start = end + 1;
            item
        })
    }
}

/// A lexed query: its tokens, and the whole-query directives it holds, the
/// last of each winning.
#[derive(Debug, Default)]
pub(super) struct Lexed {
    pub(super) tokens: Vec<Token>,
    pub(super) match_case: Option<bool>,
    pub(super) match_path: Option<bool>,
}

/// Reads `input` into tokens.
pub(super) fn lex(input: &str) -> Result<Lexed, QueryError> {
    let mut lexer = Lexer {
        chars: input.chars().collect(),
        at: 0,
        out: Lexed::default(),
        spaced: false,
        and_first: false,
        literal_bang: false,
        after_close: false,
        global: ModSet::default(),
        depth: 0,
    };
    lexer.run()?;
    Ok(lexer.out)
}

struct Lexer {
    chars: Vec<char>,
    at: usize,
    out: Lexed,
    /// White space came since the last token.
    spaced: bool,
    /// AND binds tighter than OR from here on.
    and_first: bool,
    /// `?no-!:` came: `!` is literal from here on.
    literal_bang: bool,
    /// The last token was a `>` and no white space has come since: text
    /// here would join the group, which is group expansion.
    after_close: bool,
    /// The modifiers `?mod:` set for the rest of the search.
    global: ModSet,
    /// The groups open here, so that a `>` in a regular expression closes
    /// one only when one is open.
    depth: usize,
}

/// What a `NAME:` at a term start is.
enum Head {
    Keyword(Tok),
    /// A prefix; `rest`: written `name*:`, it takes the rest of the query
    /// as its text.
    Prefix {
        prefix: Prefix,
        rest: bool,
    },
    /// `?name:`: a modifier for the rest of the search.
    Global(Prefix),
    /// `case:` or `matchpath:`, whose value follows.
    Directive(Directive),
    /// `?and-precedence:` (true) or `?or-precedence:` (false).
    Precedence(bool),
    /// `no-!:`, or with `global`, `?no-!:`.
    NoBang {
        global: bool,
    },
}

#[derive(Clone, Copy)]
enum Directive {
    Case,
    MatchPath,
}

impl Lexer {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn emit(&mut self, kind: Tok, column: usize) {
        self.after_close = kind == Tok::Close;
        match kind {
            Tok::Open => self.depth += 1,
            Tok::Close => self.depth = self.depth.saturating_sub(1),
            _ => {}
        }
        self.out.tokens.push(Token {
            kind,
            column,
            spaced: std::mem::take(&mut self.spaced),
            and_first: self.and_first,
            global: self.global,
        });
    }

    fn run(&mut self) -> Result<(), QueryError> {
        while let Some(c) = self.peek() {
            if c.is_whitespace() {
                self.at += 1;
                self.spaced = true;
                self.after_close = false;
            } else {
                self.word()?;
            }
        }
        Ok(())
    }

    /// Reads one white-space-free word: its operators, prefixes, groups and
    /// text.
    fn word(&mut self) -> Result<(), QueryError> {
        // A term starts here: after white space, `|`, `<`, `!` or a prefix.
        // `first`: no prefix yet in this term, so a name here may be a
        // keyword, a directive, or a function goz does not know.
        let mut first = true;
        let mut literal_bang = self.literal_bang;
        // The term's own `regex:` or `no-regex:`, the outermost winning.
        let mut regex: Option<bool> = None;
        // The term's function, whose value a regular expression is only
        // when it is text (`Func::matches_text`).
        let mut func: Option<Func> = None;
        // A group of values of a function whose value is no text
        // (`size:<1kb|2kb>`, `ext:<jpg|png>`) is open from this depth: its
        // values keep their own reading under `regex:` too.
        let mut values_from: Option<usize> = None;
        while let Some(c) = self.peek() {
            if c.is_whitespace() {
                return Ok(());
            }
            let column = self.at + 1;
            if self.after_close && !matches!(c, '|' | '>') {
                return Err(QueryError::GroupExpansion { column });
            }
            // In a group of a function's values, which keep their reading.
            let in_values = values_from.is_some();
            // After a prefix of a term that is a regular expression, its
            // value starts: there `|` and `<` are the expression's own, and
            // a `>` closes a group only when one is open. A function whose
            // value is no text keeps its own reading.
            let in_regex = !first
                && !in_values
                && regex.or(self.global.regex) == Some(true)
                && func.is_none_or(Func::matches_text);
            match c {
                '|' if !in_regex => {
                    self.at += 1;
                    self.emit(Tok::Or, column);
                    first = true;
                    literal_bang = self.literal_bang;
                    regex = None;
                    func = None;
                    continue;
                }
                '>' if !in_regex || self.depth > 0 => {
                    self.at += 1;
                    self.emit(Tok::Close, column);
                    if values_from.is_some_and(|d| self.depth < d) {
                        values_from = None;
                    }
                    continue;
                }
                '<' if !in_regex => {
                    // Right after a function whose value is no text: its
                    // group of values.
                    let values = !first && func.is_some_and(|f| !f.matches_text());
                    self.at += 1;
                    self.emit(Tok::Open, column);
                    if values && values_from.is_none() {
                        values_from = Some(self.depth);
                    }
                    first = true;
                    regex = None;
                    func = None;
                    continue;
                }
                '!' if !literal_bang => {
                    self.at += 1;
                    self.emit(Tok::Not, column);
                    continue;
                }
                _ => {}
            }
            if let Some((head, after)) = self.head(first)? {
                match head {
                    Head::Keyword(Tok::Not) => {
                        self.at = after;
                        self.emit(Tok::Not, column);
                    }
                    Head::Keyword(kind) => {
                        self.at = after;
                        self.emit(kind, column);
                        first = true;
                        literal_bang = self.literal_bang;
                        regex = None;
                        func = None;
                    }
                    Head::Precedence(and_first) => {
                        self.at = after;
                        self.and_first = and_first;
                    }
                    Head::NoBang { global } => {
                        self.at = after;
                        self.literal_bang |= global;
                        literal_bang = true;
                    }
                    Head::Directive(directive) => {
                        self.at = after;
                        self.directive(directive)?;
                    }
                    Head::Global(prefix) => {
                        self.at = after;
                        self.set_global(prefix);
                    }
                    Head::Prefix { prefix, rest } => {
                        self.at = after;
                        self.emit(Tok::Prefix(prefix), column);
                        first = false;
                        if let Prefix::Mod(Mod::Regex, on) = prefix {
                            regex.get_or_insert(on);
                        }
                        if rest {
                            // `name*:`: the rest of the query, verbatim.
                            let column = self.at + 1;
                            let mut text = Text::default();
                            let rest: String = self.chars[self.at..].iter().collect();
                            text.text = rest.into_bytes();
                            self.at = self.chars.len();
                            self.emit(Tok::Text(text), column);
                            return Ok(());
                        }
                        if let Prefix::Func(f) = prefix {
                            func = Some(f);
                            let regex = !in_values && regex.or(self.global.regex) == Some(true);
                            self.value(f, literal_bang, regex)?;
                        }
                    }
                }
                continue;
            }
            let text = if !in_values
                && regex.or(self.global.regex) == Some(true)
                && func.is_none_or(Func::matches_text)
            {
                self.regex_text()
            } else {
                self.text(false)?
            };
            self.emit(Tok::Text(text), column);
        }
        Ok(())
    }

    /// Reads a regular expression's text, as Everything's parser eats it:
    /// up to unquoted white space, `|`, `;`, `<` and `>` included, but for a
    /// `>` that closes an open group. Quotes are removed (`regex:"a b"`) and
    /// entities replaced, as everywhere.
    fn regex_text(&mut self) -> Text {
        let mut out = Text::default();
        let mut quoted = false;
        while let Some(c) = self.peek() {
            if c == '"' {
                quoted = !quoted;
                self.at += 1;
                continue;
            }
            if !quoted && (c.is_whitespace() || (c == '>' && self.depth > 0)) {
                break;
            }
            if c == '&'
                && let Some((cp, len, _)) = self.entity()
            {
                encode_code_point(cp, &mut out.text);
                self.at += len;
                continue;
            }
            let mut buf = [0u8; 4];
            out.text
                .extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            self.at += 1;
        }
        out
    }

    /// `?name:`: the modifier holds for every term after it that does not
    /// set it itself (`?case: foo nocase:FOO` matches `foo` case and `FOO`
    /// not, L7:473); a later `?name:` of the same modifier replaces it.
    fn set_global(&mut self, prefix: Prefix) {
        match prefix {
            Prefix::Mod(m, on) => *self.global.slot(m) = Some(on),
            Prefix::File => self.global.kind = Some(Kind::File),
            Prefix::Folder | Prefix::Dir => self.global.kind = Some(Kind::Folder),
            // `head` turns these away.
            Prefix::Macro(_) | Prefix::Empty | Prefix::Func(_) => {}
        }
    }

    /// The `NAME:` at the cursor, when it is one goz reads here, and where
    /// what follows its colon starts. `first`: no prefix came before it in
    /// the term. There, a name of two or more letters, hyphens allowed as in
    /// Everything's own (`date-created:`, `no-dir:`), that names nothing is
    /// refused: searched as text it could match nothing, since no name holds
    /// `:`. After a prefix, such text is the prefix's value (`file:a:b`), as
    /// it always was.
    fn head(&self, first: bool) -> Result<Option<(Head, usize)>, QueryError> {
        let start = self.at;
        let mut end = start;
        let (global, builtin) = match (self.chars.get(end), self.chars.get(end + 1)) {
            (Some('?'), _) => (true, false),
            (Some(':'), Some(':')) => (false, true),
            _ => (false, false),
        };
        end += usize::from(global) + 2 * usize::from(builtin);
        let name_start = end;
        if matches!(self.chars.get(end), Some('^' | '$' | '\\'))
            && self.chars.get(end + 1) == Some(&':')
        {
            // The symbol aliases `^:`, `$:` and `\:`.
            end += 1;
        } else {
            while self
                .chars
                .get(end)
                .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '!'))
            {
                end += 1;
            }
        }
        let rest = self.chars.get(end) == Some(&'*') && end > name_start;
        if rest {
            end += 1;
        }
        if self.chars.get(end) != Some(&':') {
            return Ok(None);
        }
        let raw: String = self.chars[start..end].iter().collect();
        let name: String = self.chars[name_start..end - usize::from(rest)]
            .iter()
            .filter(|&&c| c != '-')
            .map(|c| c.to_ascii_lowercase())
            .collect();
        let after = end + 1;
        let unknown = || {
            Err(QueryError::UnknownFunction(format!(
                "{}:",
                raw.trim_end_matches('*')
            )))
        };
        let head = match (global, name.as_str()) {
            (_, "andprecedence") if first && !rest => Head::Precedence(true),
            (_, "orprecedence") if first && !rest => Head::Precedence(false),
            (global, "no!") if !rest => Head::NoBang { global },
            (false, "and") if first && !builtin && !rest => Head::Keyword(Tok::And),
            (false, "or") if first && !builtin && !rest => Head::Keyword(Tok::Or),
            (false, "not") if !builtin && !rest => Head::Keyword(Tok::Not),
            (false, "case") if first && !rest && self.directive_follows(after) => {
                Head::Directive(Directive::Case)
            }
            (false, "matchpath") if first && !rest => Head::Directive(Directive::MatchPath),
            (global, name) => match Prefix::named(name) {
                Some(Named::Prefix(prefix)) if global => match prefix {
                    Prefix::Macro(_) | Prefix::Empty | Prefix::Func(_) if !rest => {
                        return Err(QueryError::Malformed {
                            what: prefix.spelling(),
                            detail: format!(
                                "only a search modifier applies to the rest of the search; \
                                 {raw}: is a function"
                            ),
                        });
                    }
                    Prefix::Macro(_) | Prefix::Empty | Prefix::Func(_) => return Ok(None),
                    _ if rest => return Ok(None),
                    _ => Head::Global(prefix),
                },
                Some(Named::Prefix(prefix)) => Head::Prefix { prefix, rest },
                Some(Named::Refused(why)) => {
                    return Err(QueryError::Malformed {
                        what: "wildcards:",
                        detail: why.to_string(),
                    });
                }
                Some(Named::Unsupported(why)) => {
                    return Err(QueryError::Unsupported {
                        what: format!("{}:", raw.trim_end_matches('*')),
                        why,
                    });
                }
                None if global => return Ok(None),
                None if first
                    && name.len() > 1
                    && self.chars[name_start..end - usize::from(rest)]
                        .iter()
                        .all(|&c| c.is_ascii_alphabetic() || c == '-') =>
                {
                    return unknown();
                }
                None => return Ok(None),
            },
        };
        Ok(Some((head, after)))
    }

    /// Whether what follows a `case:` at `at` makes it the whole-query
    /// directive: nothing, `0` or `1` (quoted or not), up to the next
    /// white space, `|` or `>`. Anything else is the text of the per-term
    /// modifier (`case:Foo`).
    fn directive_follows(&self, at: usize) -> bool {
        let mut value = String::new();
        let mut quoted = false;
        for &c in &self.chars[at.min(self.chars.len())..] {
            if c == '"' {
                quoted = !quoted;
                continue;
            }
            if !quoted && (c.is_whitespace() || c == '|' || c == '>') {
                break;
            }
            value.push(c);
            if value.len() > 1 {
                return false;
            }
        }
        matches!(value.as_str(), "" | "0" | "1")
    }

    /// Reads a `case:` or `matchpath:` directive's value, which must be
    /// empty, `0` or `1`, and records it. A directive is a switch for the
    /// whole query, so it leaves no token, wherever it stands.
    fn directive(&mut self, directive: Directive) -> Result<(), QueryError> {
        let value = self.text(false)?.text;
        let on = match value.as_slice() {
            b"" | b"1" => true,
            b"0" => false,
            _ => {
                let (what, name) = match directive {
                    Directive::Case => ("case:", "case"),
                    Directive::MatchPath => ("matchpath:", "matchpath"),
                };
                return Err(QueryError::Malformed {
                    what,
                    detail: format!(
                        "expected {name}:, {name}:0 or {name}:1, got {name}:{}",
                        String::from_utf8_lossy(&value)
                    ),
                });
            }
        };
        match directive {
            Directive::Case => self.out.match_case = Some(on),
            Directive::MatchPath => self.out.match_path = Some(on),
        }
        Ok(())
    }

    /// Reads a function's value: for a number or a date, its own `!`s
    /// first, as NOT tokens (`size:!1kb` is `!size:1kb`, L2:112); any other
    /// function's value takes a `!` as text, as Everything does (`ext:!txt`
    /// finds nothing, L7:523, and so do `name:!spider` and
    /// `parent:!C:\Windows`, L8:81-88). Then the value as text, or nothing when a value group
    /// follows, which the word's loop reads as a group. A number or a date
    /// may start with its comparison (`size:>1mb`), whose `<` or `>` is part
    /// of the value; a `<` is a value group there only when it is one (see
    /// [`Lexer::value_group_follows`]). Under `regex:` (`regex`), a text
    /// function's value is a regular expression, read as one is.
    fn value(&mut self, func: Func, literal_bang: bool, regex: bool) -> Result<(), QueryError> {
        while !literal_bang && func.compares() && self.peek() == Some('!') {
            let column = self.at + 1;
            self.at += 1;
            self.emit(Tok::Not, column);
        }
        if regex && func.matches_text() {
            let column = self.at + 1;
            let text = self.regex_text();
            self.emit(Tok::Text(text), column);
            return Ok(());
        }
        if self.peek() == Some('<') && (!func.compares() || self.value_group_follows()) {
            return Ok(());
        }
        let column = self.at + 1;
        let text = self.text(func.compares())?;
        self.emit(Tok::Text(text), column);
        Ok(())
    }

    /// Whether the `<` at the cursor, after a numeric function's prefix,
    /// opens a group of values (`size:<1kb|2048>`) rather than starting a
    /// comparison (`size:<1kb`): a `>` closes it before the next unquoted
    /// white space, as in Everything, and a `|` stands inside it at its own
    /// level. Everything asks only for the `>`, which makes
    /// `<a|size:<1kb>` an unclosed group around a value group; a group of
    /// one value is no group anyone writes, so a `<` with no `|` inside is
    /// the comparison, and that `>` closes the outer group.
    fn value_group_follows(&self) -> bool {
        let mut depth = 0usize;
        let mut quoted = false;
        let mut alternatives = false;
        for &c in &self.chars[self.at..] {
            match c {
                '"' => quoted = !quoted,
                _ if quoted => {}
                c if c.is_whitespace() => return false,
                '|' if depth == 1 => alternatives = true,
                '<' => depth += 1,
                '>' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return alternatives;
                    }
                }
                _ => {}
            }
        }
        false
    }

    /// Reads text up to unquoted white space, `|` or `>`. `comparison`: a
    /// run of `<`, `>` and `=` that starts the value, or an item of its list
    /// (`size:>1mb;<10`), is text too, but for a run of `>` that nothing
    /// follows, which closes groups (`<x size:>` is `x` and an empty size
    /// value in a group).
    fn text(&mut self, comparison: bool) -> Result<Text, QueryError> {
        let mut out = Text::default();
        let take_comparison = |this: &mut Self, out: &mut Text| {
            let run = this.chars[this.at..]
                .iter()
                .take_while(|c| matches!(c, '<' | '>' | '='))
                .count();
            let closes = this.chars[this.at..this.at + run].iter().all(|&c| c == '>')
                && this
                    .chars
                    .get(this.at + run)
                    .is_none_or(|&c| c.is_whitespace() || c == '|');
            if closes {
                return;
            }
            for _ in 0..run {
                out.text.push(this.chars[this.at] as u8);
                this.at += 1;
            }
        };
        if comparison {
            take_comparison(self, &mut out);
        }
        let mut quoted = false;
        while let Some(c) = self.peek() {
            if c == '"' {
                quoted = !quoted;
                self.at += 1;
                continue;
            }
            if !quoted {
                if c.is_whitespace() || c == '|' || c == '>' {
                    break;
                }
                if c == '<' {
                    return Err(QueryError::GroupExpansion {
                        column: self.at + 1,
                    });
                }
                if c == ';' {
                    out.splits.push(out.text.len());
                    out.text.push(b';');
                    self.at += 1;
                    if comparison {
                        take_comparison(self, &mut out);
                    }
                    continue;
                }
            }
            if c == '&'
                && let Some((cp, len, wild)) = self.entity()
            {
                encode_code_point(cp, &mut out.text);
                out.literal_wild |= wild;
                self.at += len;
                continue;
            }
            let mut buf = [0u8; 4];
            out.text
                .extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            self.at += 1;
        }
        Ok(out)
    }

    /// The character entity at the cursor (an `&`): its code point, its
    /// length in characters, and whether it is `&ast:` or `&quest:`. `None`
    /// when the text is no entity, which leaves the `&` literal.
    fn entity(&self) -> Option<(u32, usize, bool)> {
        // The longest entity name is `#x10FFFF` with leading zeros allowed;
        // a dozen characters cover every real one.
        let rest = &self.chars[self.at + 1..];
        let colon = rest.iter().take(16).position(|&c| c == ':')?;
        let name: String = rest[..colon]
            .iter()
            .collect::<String>()
            .to_ascii_lowercase();
        let len = colon + 2;
        let named = |cp: char| Some((cp as u32, len, false));
        match name.as_str() {
            "sp" => named(' '),
            "vert" => named('|'),
            "excl" => named('!'),
            "lt" => named('<'),
            "gt" => named('>'),
            "quot" => named('"'),
            "amp" => named('&'),
            "ast" => Some(('*' as u32, len, true)),
            "quest" => Some(('?' as u32, len, true)),
            _ => {
                let number = name.strip_prefix('#')?;
                let cp = match number.strip_prefix('x') {
                    Some(hex) if !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()) => {
                        u32::from_str_radix(hex, 16).ok()?
                    }
                    Some(_) => return None,
                    None if !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()) => {
                        number.parse::<u32>().ok()?
                    }
                    None => return None,
                };
                (cp <= 0x10FFFF).then_some((cp, len, false))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tokens of `input`, written compactly: `|`, `&`, `!`, `<`, `>`,
    /// `P(name)`, `T(text)`, each after a space when spaced.
    fn shape(input: &str) -> String {
        let lexed = lex(input).unwrap();
        let mut out = String::new();
        for t in &lexed.tokens {
            if t.spaced {
                out.push(' ');
            }
            match &t.kind {
                Tok::Or => out.push('|'),
                Tok::And => out.push('&'),
                Tok::Not => out.push('!'),
                Tok::Open => out.push('<'),
                Tok::Close => out.push('>'),
                Tok::Prefix(p) => out.push_str(&format!("P({})", p.spelling())),
                Tok::Text(t) => out.push_str(&format!(
                    "T({}{})",
                    String::from_utf8_lossy(&t.text),
                    if t.literal_wild { ",lit" } else { "" }
                )),
            }
        }
        out
    }

    #[test]
    fn operators_groups_and_text() {
        assert_eq!(shape("foo bar|ever"), "T(foo) T(bar)|T(ever)");
        assert_eq!(shape("foo | ever"), "T(foo) | T(ever)");
        assert_eq!(shape("<foo bar>|ever"), "<T(foo) T(bar)>|T(ever)");
        assert_eq!(shape("!foo|!<a b>"), "!T(foo)|!<T(a) T(b)>");
        assert_eq!(shape("foo ! ever"), "T(foo) ! T(ever)");
        assert_eq!(shape("<<spider>>"), "<<T(spider)>>");
        assert_eq!(shape("<>"), "<>");
        assert_eq!(shape("|"), "|");
    }

    /// `!` negates only where a term starts; elsewhere a name holds it.
    #[test]
    fn a_bang_inside_a_word_is_text() {
        assert_eq!(shape("wow!.txt"), "T(wow!.txt)");
        assert_eq!(shape("wow!"), "T(wow!)");
        assert_eq!(shape("!!foo"), "!!T(foo)");
        assert_eq!(shape("\"!bang\""), "T(!bang)");
    }

    #[test]
    fn quotes_keep_operators_literal_and_concatenate() {
        assert_eq!(shape("\"a|b\""), "T(a|b)");
        assert_eq!(shape("spider \"|\" man"), "T(spider) T(|) T(man)");
        assert_eq!(shape("\"spider\"man"), "T(spiderman)");
        assert_eq!(shape("spider\"man\""), "T(spiderman)");
        assert_eq!(shape("<\"foo|bar\">"), "<T(foo|bar)>");
        assert_eq!(shape("\"<a>\""), "T(<a>)");
        assert_eq!(shape("\"*.exe\""), "T(*.exe)");
    }

    #[test]
    fn keywords_are_operators_in_any_case() {
        assert_eq!(shape("foo and: bar"), "T(foo) & T(bar)");
        assert_eq!(shape("foo AND: bar"), "T(foo) & T(bar)");
        assert_eq!(shape("foo or:ever"), "T(foo) |T(ever)");
        assert_eq!(shape("NOT:foo"), "!T(foo)");
        assert_eq!(shape("not:not:foo"), "!!T(foo)");
        assert_eq!(shape("not: foo"), "! T(foo)");
        assert_eq!(shape("foo AND bar"), "T(foo) T(AND) T(bar)");
    }

    #[test]
    fn prefixes_and_function_values() {
        assert_eq!(shape("ext:pdf|txt"), "P(ext:)T(pdf)|T(txt)");
        assert_eq!(shape("size:>1mb"), "P(size:)T(>1mb)");
        assert_eq!(shape("<size:>1mb>"), "<P(size:)T(>1mb)>");
        assert_eq!(shape("dm:<2020"), "P(dm:)T(<2020)");
        assert_eq!(shape("size:>>bogus"), "P(size:)T(>>bogus)");
        assert_eq!(shape("Date-Modified:today"), "P(dm:)T(today)");
        assert_eq!(
            shape("ext:!pdf"),
            "P(ext:)T(!pdf)",
            "an extension's ! is text"
        );
        assert_eq!(shape("size:!=1kb"), "P(size:)!T(=1kb)");
        assert_eq!(shape("ext:"), "P(ext:)T()");
        assert_eq!(shape("path:<words ever>"), "P(path:)<T(words) T(ever)>");
        assert_eq!(shape("file:!a"), "P(file:)!T(a)");
        assert_eq!(shape("file:path:words"), "P(file:)P(path:)T(words)");
        assert_eq!(shape("file:not:a"), "P(file:)!T(a)");
        assert_eq!(
            shape(r#"path:"C:\Program Files""#),
            r"P(path:)T(C:\Program Files)"
        );
        assert_eq!(shape("ext:path:x"), "P(ext:)T(path:x)");
        assert_eq!(
            shape("file:foo:bar"),
            "P(file:)T(foo:bar)",
            "a value is not checked"
        );
        assert_eq!(shape(r"C:\Windows 12:30"), r"T(C:\Windows) T(12:30)");
        assert_eq!(
            shape("\"a:b\" ?case:"),
            "T(a:b)",
            "?case: is a global modifier"
        );
    }

    #[test]
    fn directives_leave_no_token() {
        let lexed = lex("Report case: matchpath:1 x case:0").unwrap();
        assert_eq!(lexed.match_case, Some(false), "the last wins");
        assert_eq!(lexed.match_path, Some(true));
        assert_eq!(lexed.tokens.len(), 2);
        assert_eq!(lex("case:\"1\"").unwrap().match_case, Some(true));
        assert_eq!(shape("case:2"), "P(case:)T(2)", "the per-term modifier");
        assert!(matches!(
            lex("matchpath:maybe"),
            Err(QueryError::Malformed {
                what: "matchpath:",
                ..
            })
        ));
        assert_eq!(
            shape("file:case:"),
            "P(file:)P(case:)",
            "a directive only as a term's first name"
        );
    }

    #[test]
    fn precedence_switches_mark_the_tokens_after_them() {
        let lexed = lex("a b|c ?and-precedence: d|e and-precedence: f ?OR-precedence: g").unwrap();
        let marks: Vec<bool> = lexed.tokens.iter().map(|t| t.and_first).collect();
        // a b | c d | e f g
        assert_eq!(
            marks,
            [false, false, false, false, true, true, true, true, false]
        );
    }

    #[test]
    fn no_bang_makes_a_terms_bang_literal() {
        assert_eq!(shape("no-!:!bang"), "T(!bang)");
        assert_eq!(shape("no!:!bang !x"), "T(!bang) !T(x)");
        assert_eq!(shape("no-!: !bang"), " !T(bang)");
        assert_eq!(shape("?no-!: !bang !x|!y"), " T(!bang) T(!x)|T(!y)");
        assert_eq!(shape("!no-!:!bang"), "!T(!bang)");
        assert_eq!(shape("no-!:size:!1kb"), "P(size:)T(!1kb)");
    }

    #[test]
    fn entities_are_literal_characters() {
        assert_eq!(shape("&excl:bang"), "T(!bang)");
        assert_eq!(shape("\"&excl:bang\""), "T(!bang)", "in quotes too");
        assert_eq!(
            shape("&#33:bang &#x21:bang &#X0021:bang"),
            "T(!bang) T(!bang) T(!bang)"
        );
        assert_eq!(shape("foo&sp:bar"), "T(foo bar)");
        assert_eq!(shape("spider&vert:man"), "T(spider|man)");
        assert_eq!(shape("w&lt:x&gt:"), "T(w<x>)");
        assert_eq!(shape("&quot:a&amp:b"), "T(\"a&b)");
        assert_eq!(shape("&ast:.exe"), "T(*.exe,lit)");
        assert_eq!(shape("*.exe&quest:"), "T(*.exe?,lit)");
        assert_eq!(
            shape("a&b &foo:bar &#: &#x: &#1114112:"),
            "T(a&b) T(&foo:bar) T(&#:) T(&#x:) T(&#1114112:)"
        );
        let surrogate = lex("&#xD800:").unwrap();
        let Tok::Text(t) = &surrogate.tokens[0].kind else {
            panic!()
        };
        assert_eq!(t.text, [0xED, 0xA0, 0x80], "a lone surrogate, as WTF-8");
    }

    /// A group never joins text: Everything's group expansion is refused
    /// with the column.
    #[test]
    fn group_expansion_is_refused_with_its_column() {
        let at = |input: &str| match lex(input) {
            Err(QueryError::GroupExpansion { column }) => column,
            other => panic!("{input}: {other:?}"),
        };
        assert_eq!(at("gr<a|e>y"), 3);
        assert_eq!(at("a<b"), 2);
        assert_eq!(at("<spider>man"), 9);
        assert_eq!(at("<a><b>"), 4);
        assert_eq!(at("<a>!b"), 4);
        assert_eq!(at("x size:1<2"), 9);
        assert!(lex("<a>|<b> <c>").is_ok());
        assert_eq!(at("size:<1kb|2kb>x"), 15, "text after a value group");
        assert_eq!(
            at("dm:<today|>now>"),
            12,
            "Everything finds nothing (L7:585)"
        );
    }

    #[test]
    fn columns_count_characters() {
        let lexed = lex("é <x").unwrap();
        let columns: Vec<usize> = lexed.tokens.iter().map(|t| t.column).collect();
        assert_eq!(columns, [1, 3, 4]);
    }

    /// Prefix names are read lower-cased with their hyphens dropped, as
    /// Everything's are optional, with each alias Everything knows
    /// (DESIGN.md §2.3-§2.4; `no-file:` and `no-folder:` as L7:452-454 and
    /// L8:98-101 answer).
    #[test]
    fn prefixes_have_everythings_names_and_aliases() {
        for (input, want) in [
            ("case:x", "P(case:)T(x)"),
            ("CASE:x", "P(case:)T(x)"),
            ("no-case:x", "P(nocase:)T(x)"),
            ("nocase:x", "P(nocase:)T(x)"),
            ("path:x", "P(path:)T(x)"),
            (r"\:x", "P(path:)T(x)"),
            ("nopath:x", "P(nopath:)T(x)"),
            ("no-path:x", "P(nopath:)T(x)"),
            ("ww:x", "P(ww:)T(x)"),
            ("whole-word:x", "P(ww:)T(x)"),
            ("Whole-Words:x", "P(ww:)T(x)"),
            ("wholeword:x", "P(ww:)T(x)"),
            ("no-ww:x", "P(noww:)T(x)"),
            ("prefix:x", "P(prefix:)T(x)"),
            ("suffix:x", "P(suffix:)T(x)"),
            ("whole:x", "P(whole:)T(x)"),
            ("exact:x", "P(whole:)T(x)"),
            ("wfn:x", "P(whole:)T(x)"),
            ("whole-filename:x", "P(whole:)T(x)"),
            ("entire:x", "P(whole:)T(x)"),
            ("startwith:x", "P(startwith:)T(x)"),
            ("start-with:x", "P(startwith:)T(x)"),
            ("^:x", "P(startwith:)T(x)"),
            ("endwith:x", "P(endwith:)T(x)"),
            ("end-with:x", "P(endwith:)T(x)"),
            ("$:x", "P(endwith:)T(x)"),
            ("nopunc:x", "P(nopunc:)T(x)"),
            ("no-punctuation:x", "P(nopunc:)T(x)"),
            ("ignore-punctuation:x", "P(nopunc:)T(x)"),
            ("ignore-punc:x", "P(nopunc:)T(x)"),
            ("punc:x", "P(punc:)T(x)"),
            ("no-ignore-punc:x", "P(punc:)T(x)"),
            ("nows:x", "P(nows:)T(x)"),
            ("no-whitespace:x", "P(nows:)T(x)"),
            ("ignore-whitespace:x", "P(nows:)T(x)"),
            ("ignore-ws:x", "P(nows:)T(x)"),
            ("ws:x", "P(ws:)T(x)"),
            ("star-all:x", "P(starall:)T(x)"),
            ("no-star-all:x", "P(nostarall:)T(x)"),
            ("no-wildcards:x", "P(nowildcards:)T(x)"),
            ("filename-wildcards:x", "P(filenamewildcards:)T(x)"),
            ("no-filename-wildcards:x", "P(nowildcards:)T(x)"),
            ("metric:x", "P(metric:)T(x)"),
            ("file:x", "P(file:)T(x)"),
            ("files:x", "P(file:)T(x)"),
            ("files-only:x", "P(file:)T(x)"),
            ("no-folder:x", "P(file:)T(x)"),
            ("no-folders-only:x", "P(file:)T(x)"),
            ("folder:x", "P(folder:)T(x)"),
            ("folders:x", "P(folder:)T(x)"),
            ("folders-only:x", "P(folder:)T(x)"),
            ("no-file:x", "P(folder:)T(x)"),
            ("nofiles:x", "P(folder:)T(x)"),
            ("dir:x", "P(dir:)T(x)"),
            ("ext:x", "P(ext:)T(x)"),
            ("extension:x", "P(extension:)T(x)"),
            ("size:x", "P(size:)T(x)"),
            ("dm:x", "P(dm:)T(x)"),
            ("date:x", "P(dm:)T(x)"),
            ("date-modified:x", "P(dm:)T(x)"),
            ("len:x", "P(len:)T(x)"),
            ("name-length:x", "P(name-length:)T(x)"),
            ("namelen:x", "P(name-length:)T(x)"),
            ("full-path-length:x", "P(full-path-length:)T(x)"),
            ("fullpathlen:x", "P(full-path-length:)T(x)"),
            ("path-len:x", "P(full-path-length:)T(x)"),
            ("path-part-length:x", "P(path-part-length:)T(x)"),
            ("extension-length:x", "P(extension-length:)T(x)"),
            ("extlen:x", "P(extension-length:)T(x)"),
            ("parent:x", "P(parent:)T(x)"),
            ("infolder:x", "P(parent:)T(x)"),
            ("in-folder:x", "P(parent:)T(x)"),
            ("nosubfolders:x", "P(parent:)T(x)"),
            ("no-sub-folders:x", "P(parent:)T(x)"),
            ("ancestor:x", "P(ancestor:)T(x)"),
            ("name:x", "P(name:)T(x)"),
            ("basename:x", "P(name:)T(x)"),
            ("stem:x", "P(stem:)T(x)"),
            ("full-path:x", "P(full-path:)T(x)"),
            ("path-and-name:x", "P(full-path:)T(x)"),
            ("path-name:x", "P(full-path:)T(x)"),
            ("location:x", "P(location:)T(x)"),
            ("loc:x", "P(location:)T(x)"),
            ("path-part:x", "P(location:)T(x)"),
            ("pp:x", "P(location:)T(x)"),
            ("parent-name:x", "P(parent-name:)T(x)"),
            ("audio:x", "P(audio:)T(x)"),
            ("zip:x", "P(zip:)T(x)"),
            ("doc:x", "P(doc:)T(x)"),
            ("exe:x", "P(exe:)T(x)"),
            ("image:x", "P(image:)T(x)"),
            ("pic:x", "P(image:)T(x)"),
            ("video:x", "P(video:)T(x)"),
            ("::case:x", "P(case:)T(x)"),
            ("::ext:x", "P(ext:)T(x)"),
        ] {
            assert_eq!(shape(input), want, "{input}");
        }
        for unknown in ["nodir:", "no-dir:", "noext:", "bogus:", "dc:", "length:"] {
            assert!(
                matches!(lex(unknown), Err(QueryError::UnknownFunction(_))),
                "{unknown}"
            );
        }
        assert!(
            matches!(lex("wildcards:x"), Err(QueryError::Malformed { .. })),
            "extended wildcards are refused with why"
        );
    }

    /// Modifiers stack and reach groups; `?mod:` reaches every term after
    /// it and leaves no token; `name*:` takes the rest of the query as text.
    #[test]
    fn prefixes_stack_and_global_and_rest_forms() {
        assert_eq!(
            shape("folder:case:ww:path:FOO"),
            "P(folder:)P(case:)P(ww:)P(path:)T(FOO)"
        );
        assert_eq!(shape("case:<FOO BAR>"), "P(case:)<T(FOO) T(BAR)>");
        let lexed = lex("a ?case: b ?nocase: c").unwrap();
        let cases: Vec<Option<bool>> = lexed.tokens.iter().map(|t| t.global.case).collect();
        assert_eq!(cases, [None, Some(true), Some(false)]);
        let lexed = lex("?file: a ?no-file: b").unwrap();
        let kinds: Vec<Option<Kind>> = lexed.tokens.iter().map(|t| t.global.kind).collect();
        assert_eq!(kinds, [Some(Kind::File), Some(Kind::Folder)]);
        assert!(matches!(
            lex("?ext:x"),
            Err(QueryError::Malformed { what: "ext:", .. })
        ));
        assert_eq!(shape("name*:a b|c"), "P(name:)T(a b|c)");
        assert_eq!(shape("x case*:A b"), "T(x) P(case:)T(A b)");
    }

    /// After a function, a `;` splits values outside quotes; after a numeric
    /// one, a `<` compares unless a value group follows, and a `!` negates,
    /// which after any other function is text.
    #[test]
    fn function_values_lists_groups_and_comparisons() {
        let items = |input: &str| -> Vec<String> {
            let lexed = lex(input).unwrap();
            let Tok::Text(text) = &lexed.tokens[1].kind else {
                panic!("{input}")
            };
            text.items()
                .map(|i| String::from_utf8_lossy(i).into_owned())
                .collect()
        };
        assert_eq!(items("ext:txt;exe"), ["txt", "exe"]);
        assert_eq!(items("ext:txt;\"a;b\""), ["txt", "a;b"]);
        assert_eq!(items("ext:;"), ["", ""]);
        assert_eq!(items("size:1kb;2kb"), ["1kb", "2kb"]);
        assert_eq!(items("size:>1mb;<10"), [">1mb", "<10"]);
        assert_eq!(items("dm:<2020;>=2024"), ["<2020", ">=2024"]);
        assert_eq!(shape("<size:<1;>9>"), "<P(size:)T(<1;>9)>");
        assert_eq!(shape("<x size:>"), "<T(x) P(size:)T()>", "an empty value");
        assert_eq!(shape("<<x size:>>|y"), "<<T(x) P(size:)T()>>|T(y)");
        assert_eq!(shape("size:>9>"), "P(size:)T(>9)>");
        assert_eq!(shape("size:<1kb"), "P(size:)T(<1kb)");
        assert_eq!(shape("size:<1kb|2kb>"), "P(size:)<T(1kb)|T(2kb)>");
        assert_eq!(
            shape("<a|size:<1kb>"),
            "<T(a)|P(size:)T(<1kb)>",
            "a comparison closing an outer group"
        );
        assert_eq!(
            shape("size:<1kb dm:>2020"),
            "P(size:)T(<1kb) P(dm:)T(>2020)"
        );
        assert_eq!(shape("dm:<2021|2024>"), "P(dm:)<T(2021)|T(2024)>");
        assert_eq!(shape("ext:<pdf|txt>"), "P(ext:)<T(pdf)|T(txt)>");
        assert_eq!(shape("size:!=1kb"), "P(size:)!T(=1kb)");
        assert_eq!(shape("dm:!today"), "P(dm:)!T(today)");
        assert_eq!(shape("len:!7"), "P(len:)!T(7)");
        assert_eq!(shape("name:!x"), "P(name:)T(!x)");
        assert_eq!(shape(r"parent:!C:\x"), r"P(parent:)T(!C:\x)");
    }

    /// A regular expression's value runs to white space, `|`, `;`, `<` and
    /// `>` included, as Everything's parser eats them (L1:413-417), but for
    /// a `>` that closes an open group; the prefixes before it are read as
    /// ever, and a `!` before the value negates the term.
    #[test]
    fn a_regex_value_runs_to_white_space() {
        assert_eq!(shape("regex:^foo|^ever"), "P(regex:)T(^foo|^ever)");
        assert_eq!(shape("regex:^foo |ever"), "P(regex:)T(^foo) |T(ever)");
        assert_eq!(shape(r#"regex:"^foo bar""#), "P(regex:)T(^foo bar)");
        assert_eq!(shape("regex:a;b<c>d"), "P(regex:)T(a;b<c>d)");
        assert_eq!(shape("regex:<a|b>"), "P(regex:)T(<a|b>)");
        assert_eq!(shape("<regex:a|b> c"), "<P(regex:)T(a|b)> T(c)");
        assert_eq!(shape("case:regex:^Foo"), "P(case:)P(regex:)T(^Foo)");
        assert_eq!(shape("regex:!x|y"), "P(regex:)!T(x|y)");
        assert_eq!(shape("regex:a&vert:b"), "P(regex:)T(a|b)");
        assert_eq!(shape("regex*:^a b|c$"), "P(regex:)T(^a b|c$)");
        assert_eq!(shape("regex:stem:^a|b"), "P(regex:)P(stem:)T(^a|b)");
        // A function whose value is no text keeps its own reading, its
        // group of values included, under the term's or the search's
        // `regex:`.
        assert_eq!(shape("regex:ext:a;b"), "P(regex:)P(ext:)T(a;b)");
        assert_eq!(
            shape("regex:ext:<jpg|png>"),
            "P(regex:)P(ext:)<T(jpg)|T(png)>"
        );
        assert_eq!(shape("?regex: ext:<jpg|png>"), " P(ext:)<T(jpg)|T(png)>");
        assert_eq!(shape("?regex: size:<1kb|2kb>"), " P(size:)<T(1kb)|T(2kb)>");
        assert_eq!(
            shape("?regex: dm:<2021|2024> a|b"),
            " P(dm:)<T(2021)|T(2024)> T(a|b)"
        );
        assert_eq!(shape("?regex: size:<1kb x|y"), " P(size:)T(<1kb) T(x|y)");
        assert_eq!(shape("?regex: name:<a|b>"), " P(name:)T(<a|b>)");
        // `?regex:` makes every later term's text a regular expression; a
        // term starts as ever, and `noregex:` turns it off for one term.
        assert_eq!(shape("?regex: a|b <c|d>"), " T(a|b) <T(c|d)>");
        assert_eq!(shape("?regex: a noregex:b|c"), " T(a) P(noregex:)T(b)|T(c)");
    }

    #[test]
    fn diacritics_is_a_modifier() {
        assert_eq!(shape("diac:café"), "P(diacritics:)T(café)");
        assert_eq!(shape("diacritics:<a b>"), "P(diacritics:)<T(a) T(b)>");
        assert_eq!(shape("no-diacritics:x"), "P(nodiacritics:)T(x)");
        assert_eq!(shape("?diacritics: x"), " T(x)");
        assert_eq!(
            lex("?diacritics: x").unwrap().tokens[0].global.diacritics,
            Some(true)
        );
    }
}
