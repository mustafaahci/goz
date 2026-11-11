//! es.exe-compatible command-line parsing: [`parse_command_line`] reads the
//! raw command line as es does, and [`parse_argv`] takes tokens already split.
//!
//! Rules implemented from the voidtools es source:
//!
//! - Search text keeps its quotes, so `"foo bar"` reaches the query as a
//!   phrase. A switch's parameter loses them, and `"""` in it is one literal
//!   quote. A backslash is an ordinary character in both, so `"C:\dir\"` is a
//!   path. `-argv` reads the search text after it as most programs read their
//!   arguments: quotes removed, `\"` and `\\` inside them unescaped.
//! - `-name=value` gives a switch its parameter, and is an unknown switch if
//!   the switch takes none. Only a `-` token splits, and only at an `=` before
//!   any quote.
//! - A token starting with `-` or `/` is a switch candidate, and one more `-`
//!   after that is allowed. Name matching is case-sensitive and every dash
//!   inside the canonical name is optional in the input (`-sort-path` ==
//!   `-sortpath`, `-max-results` == `-maxresults`). An input dash is accepted
//!   only where the canonical name has one, so `-d-m` does not match `-dm`.
//! - An unknown `-` switch is fatal (exit 6). An unknown `/` token falls
//!   through to the search text verbatim (es source comment: "allow
//!   /downloads to search for \downloads"), unless es knows it as a switch or,
//!   for `/a...`, an attribute filter goz lacks: searching for it would answer
//!   a different question without saying so, so it is fatal too.
//! - `-` alone is search text. `--` ends the switches: every later token is
//!   search text, without its own switch rules.
//! - A `--` name is goz's native namespace first, matched exactly, then an es
//!   name, so `--json` is goz's object and `--path` is `-path`. es drops any
//!   other `--x` and reads the rest as search text, hiding a typo; goz exits 6.
//! - A parameter-taking switch consumes the next token; a missing or
//!   unparseable parameter is fatal (exit 4). A number may be hexadecimal, as
//!   in `0x10`.
//! - The help and version switches end parsing where they appear, as in es:
//!   a token before them can still fail, and the tokens after them are not
//!   read. Like every other switch they take `/` too, so `/?`, `/h` and `/v`
//!   are switches, not search text.
//! - Non-switch tokens become the query: joined with single spaces, in
//!   command-line order, each token preserved as received. Flags may
//!   interleave with query tokens freely. The search text is Everything
//!   1.5's syntax, operators included (`|`, `!`, `< >`), so goz passes it on
//!   as es does; a shell that takes `|`, `<` or `>` for itself needs them
//!   escaped (`^|` in cmd) or `-argv` with the text in quotes.
//! - What `-ad`, `-a-d` and `-p` add goes before the search text, where no
//!   operator, open quote or `name*:` of the text can reach it: `foo|` with
//!   `-ad` is `folder: foo|`, every folder, where `foo| folder:` would be
//!   `foo` or any folder. So do the search modifiers es sets for the whole
//!   search (`-ww`, `-a`, `-prefix`, `-suffix`, `-ignore-punctuation`,
//!   `-ignore-white-space`), as `?ww:` and the like, which apply to every
//!   term after them.
//! - `-r PATTERN` (`-regex`) puts es in regex mode, where the whole search
//!   text is one regular expression: PATTERN joins the search text where it
//!   stands, and the text goes to gozd as `regex*:TEXT`, which reads the
//!   rest of the query, verbatim, as one `regex:` term.
//! - `-parent DIR` keeps the entries right in DIR (`parent:`), and
//!   `-parent-path PATH` searches below the folder holding PATH; the CLI
//!   makes DIR and PATH absolute, as it does `-path`.
//! - Search text that is malformed whatever gozd's clock, such as an
//!   unbalanced `<`, is refused here with exit 4 and the parser's message,
//!   whose column counts in the text as typed. es passes such text on and
//!   Everything guesses; gozd refuses it too, so failing first only saves
//!   the round trip and works without a daemon.
//! - A sort's direction is set apart from its key: `-sort-size-ascending
//!   -sort-dm` sorts by date, ascending, and an unset direction is the key's
//!   default.
//!
//! Every switch is one row of [`SWITCHES`] and every sort key one row of
//! [`SORT_KEYS`]. The parser reads nothing else, and the help is rendered
//! from the same rows, so a switch the parser accepts is a switch the help
//! lists.
//!
//! Conservative choices where the research is silent (documented, not
//! observed from es): the `-sort` parameter is matched with the same
//! case-sensitive dash-optional rule as the combined `-sort-*` switches (es
//! uses the same name table for both); an unknown `-sort` value and a
//! non-`u32` `-n` value both map to exit 4 ("bad switch parameter" in modern
//! es terms); when several sort switches appear, the last one wins.

use super::Exit;
use super::es_only::ES_ONLY_SWITCHES;
use crate::proto::{CONFIG_KEYS, ConfigChange};
use crate::query::{ContentQuery, QueryError};
use crate::types::{EsColumn, SortDir, SortKey, SortSpec};

/// What one command line asks goz to do.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// Search, or report on the daemon, as the plan says.
    Run(EsPlan),
    /// Print a help screen, without contacting the daemon.
    Help(HelpScreen),
    /// Print goz's own version, without contacting the daemon.
    Version,
}

/// Which help screen to print.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HelpScreen {
    /// `-h` or `-?`: usage, examples and the common switches.
    Quick,
    /// `-help` or `--help`: every switch, sort key and exit code.
    Full,
}

/// One of es's export formats, from `-csv` or `-export-csv` and their kin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputFormat {
    Csv,
    Tsv,
    Efu,
    Txt,
    M3u,
    M3u8,
    Json,
}

/// The fully-resolved plan for one es-compatible invocation.
///
/// Produced by [`parse_argv`]; consumed by the CLI, which resolves
/// process-level concerns (absolutizing `scope`, creating the export file,
/// talking to the daemon).
#[derive(Clone, Debug, PartialEq)]
pub struct EsPlan {
    /// Search text: what `-ad`, `-a-d` and `-p` add to it, then every
    /// non-switch token, joined with single spaces in command-line order,
    /// each token preserved as received.
    pub query: String,
    /// `-path` value, verbatim. The CLI absolutizes it (GetFullPathName
    /// semantics, no existence check); the core stays path-API-free.
    pub scope: Option<String>,
    /// The scope came from `-parent-path`: it is the folder holding
    /// `scope`, once absolutized.
    pub scope_is_parent_of: bool,
    /// `-parent` value, verbatim: only the entries right in this folder.
    /// The CLI absolutizes it and puts it before the search text as a
    /// `parent:` term (see [`EsPlan::query_with_parent`]).
    pub parent: Option<String>,
    /// Where the search text starts in `query`, after the switches' terms
    /// (`folder:`, `matchpath:`, `?diacritics:`, ...): a `parent:` term
    /// goes there, so the modifiers the switches set for the whole search
    /// reach it too.
    pub text_at: usize,
    /// `-n` / `-max-results` value: return only the top N results after
    /// sorting. `None` means unlimited.
    pub limit: Option<u32>,
    /// `-o` / `-offset` value: how many results to skip, after sorting.
    pub offset: u32,
    /// Optional columns of the plain listing and the CSV, from `-size` /
    /// `-dm` / `-date-modified`, in flag order with es dedupe semantics (a
    /// repeated flag moves its column to the end). `Filename` is always
    /// emitted last and is not listed here.
    pub columns: Vec<EsColumn>,
    /// Result order. Defaults to name-ascending (es with no `-sort` flag);
    /// a sort with no direction uses es's per-key default.
    pub sort: SortSpec,
    /// The export format, from `-csv` and the like or `-export-*`, the last
    /// one given. `None` means the plain listing.
    pub format: Option<OutputFormat>,
    /// The `-export-*` file, verbatim. `None` means write to stdout.
    pub export_file: Option<String>,
    /// `-utf8-bom` or `-no-utf8-bom`. `None` means [`EsPlan::wants_bom`]
    /// decides.
    pub bom: Option<bool>,
    /// `-header` or `-no-header`. `None` means [`EsPlan::wants_header`]
    /// decides.
    pub header: Option<bool>,
    /// Whether CSV paths are always double-quoted (es default).
    /// `-no-double-quote` downgrades to quote-only-when-needed.
    pub double_quote: bool,
    /// Whether the paths of every other format are always double-quoted, from
    /// `-double-quote`. Off by default, as in es: only CSV defaults to it.
    pub text_double_quote: bool,
    /// Case-sensitive matching, from `-case` / `-i` / `-match-case`. Default
    /// off.
    pub match_case: bool,
    /// Native `--json`: emit results as goz's JSON object.
    pub json: bool,
    /// `-get-result-count`: print only the number of results. As in es, no
    /// rows are fetched, and neither `-n` nor the sort changes the number.
    /// Wins over `json`.
    pub result_count: bool,
    /// `-get-total-size`: print only the summed size of the matching files,
    /// in bytes. Fetched as `result_count` is, so neither `-n` nor the sort
    /// changes it. `result_count` wins over it, as in es; it wins over
    /// `json`.
    pub total_size: bool,
    /// `-no-result-error`: exit [`Exit::NoResults`] when nothing matches.
    pub no_result_error: bool,
    /// Native `--status`: report daemon/index status instead of searching.
    pub status: bool,
    /// Native `--server-version`: print the daemon's version instead of
    /// searching. Wins over `status`.
    pub server_version: bool,
    /// Native `--insecure-no-server-check`: skip the pipe-server owner-SID
    /// verification (dev escape hatch).
    pub insecure_no_server_check: bool,
    /// Native `--config get` or `--config set KEY=0|1...`: read or change
    /// gozd's index options instead of searching.
    pub config: Option<ConfigCommand>,
}

/// What `--config` asks of gozd.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigCommand {
    /// Print every option.
    Get,
    /// Change these options (an elevated administrator only).
    Set(ConfigChange),
}

impl Default for EsPlan {
    /// The plan for an empty command line: name-ascending sort and es's
    /// default output shape.
    fn default() -> Self {
        Self {
            query: String::new(),
            scope: None,
            scope_is_parent_of: false,
            parent: None,
            text_at: 0,
            limit: None,
            offset: 0,
            columns: Vec::new(),
            sort: SortSpec::default(),
            format: None,
            export_file: None,
            bom: None,
            header: None,
            double_quote: true,
            text_double_quote: false,
            match_case: false,
            json: false,
            result_count: false,
            total_size: false,
            no_result_error: false,
            status: false,
            server_version: false,
            insecure_no_server_check: false,
            config: None,
        }
    }
}

impl EsPlan {
    /// Whether the output starts with a UTF-8 byte order mark: as asked, or
    /// else only for a CSV export, a documented goz deviation from stock es
    /// (which defaults it off).
    pub fn wants_bom(&self) -> bool {
        self.bom
            .unwrap_or(self.export_file.is_some() && self.format == Some(OutputFormat::Csv))
    }

    /// Whether an export format that can have a header row has one: as asked,
    /// or else as es resolves it per format. M3U and es's JSON never have one.
    pub fn wants_header(&self) -> bool {
        self.header
            .unwrap_or(self.format != Some(OutputFormat::Txt))
    }

    /// The search text with `-parent`'s folder, made absolute (`absolute`),
    /// as a `parent:` term before it, after the switches' own terms (so
    /// `-a -parent DIR` names the folder with its accents). Quoted, as a
    /// folder's path may hold spaces or `;`; no Windows path holds a quote.
    pub fn query_with_parent(&self, absolute: &str) -> String {
        let at = if self.query.is_char_boundary(self.text_at) {
            self.text_at
        } else {
            0
        };
        let (lead, text) = self.query.split_at(at);
        let term = format!("parent:\"{absolute}\"");
        [lead.trim_end(), term.as_str(), text]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// A fatal argv-parse error carrying the es-compatible process exit code.
///
/// Exit-code mapping (from es.exe):
///
/// | Code | es meaning |
/// |------|------------|
/// | 4    | expected an additional parameter after a switch (modern es also uses it for a bad switch parameter); in goz also malformed search text |
/// | 6    | unknown switch |
///
/// The message names the offending token so the CLI can report it on stderr
/// and exit with `exit_code`.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct EsFatal {
    /// The process exit code the CLI must terminate with (4 or 6 here).
    pub exit_code: u8,
    /// Human-readable description, including the offending token.
    pub message: String,
}

impl EsFatal {
    /// Exit 6: the token looked like a switch but matched nothing.
    fn unknown_switch(token: &str) -> Self {
        Self {
            exit_code: Exit::UnknownSwitch.code(),
            message: format!("unknown switch: {token}"),
        }
    }

    /// Exit 6: es has this switch and goz does not, as es exits for a switch
    /// it does not know.
    fn unsupported_switch(token: &str) -> Self {
        Self {
            exit_code: Exit::UnknownSwitch.code(),
            message: format!("goz does not support the es switch {token}"),
        }
    }

    /// Exit 4: the switch requires a parameter and none followed.
    fn missing_param(token: &str) -> Self {
        Self {
            exit_code: Exit::BadParam.code(),
            message: format!("expected an additional parameter after {token}"),
        }
    }

    /// Exit 4: the switch got a parameter it cannot interpret.
    fn bad_param(token: &str, value: &str, expected: &str) -> Self {
        Self {
            exit_code: Exit::BadParam.code(),
            message: format!("expected {expected} after {token}, got: {value}"),
        }
    }

    /// Exit 4: the search text cannot be searched as written; the message
    /// is the query parser's, as gozd would send it.
    fn bad_search(error: &QueryError) -> Self {
        Self {
            exit_code: Exit::BadParam.code(),
            message: error.to_string(),
        }
    }
}

/// What a matched switch does to the plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SwitchAction {
    /// `-n` / `-max-results`: consume the next token as a `u32` limit.
    Limit,
    /// `-o` / `-offset`: consume the next token as a `u32` count to skip.
    Offset,
    /// `-path`: consume the next token as the scope, verbatim.
    Scope,
    /// `-export-*`: consume the next token as the output path, verbatim, and
    /// write that format to it.
    Export(OutputFormat),
    /// `-csv` and its kin: write that format.
    Format(OutputFormat),
    /// `-sort`: consume the next token as a sort-key name with an optional
    /// `-ascending` / `-descending` suffix.
    SortParam,
    /// `-size` / `-dm` / `-date-modified`: append a column (es dedupe).
    Column(EsColumn),
    /// `-on` and its kin, and a combined `-sort-<key>[-ascending|-descending]`
    /// switch: the key, and the direction if it names one.
    SortFixed(SortKey, Option<SortDir>),
    /// `-sort-ascending` / `-sort-descending`: the direction alone.
    SortDirection(SortDir),
    /// `-utf8-bom` / `-no-utf8-bom`.
    Bom(bool),
    /// `-header` / `-no-header`, or `-default-header` for `None`.
    Header(Option<bool>),
    /// `-double-quote` / `-no-double-quote`.
    DoubleQuote(bool),
    /// `-case` / `-i` / `-no-case` / `-no-i`.
    MatchCase(bool),
    /// `-p` / `-no-p`: test the search text against the full path.
    MatchPath(bool),
    /// `-ad` / `-a-d`: add this filter to the search text.
    Filter(&'static str),
    /// `-ww`, `-a`, `-prefix` and the like: switch the search modifier of
    /// this name on or off for the whole search text, as `?ww:` does.
    Modifier(&'static str, bool),
    /// `-r` / `-regex`: consume the next token as search text, and read the
    /// whole search text as one regular expression.
    Regex,
    /// `-parent`: consume the next token as a folder whose entries alone
    /// match.
    Parent,
    /// `-parent-path`: consume the next token as a path; the scope is the
    /// folder holding it.
    ParentScope,
    /// `-q`: consume the next token, its quotes removed, as search text.
    SearchParam,
    /// `-q*`: the rest of the command line is search text, untouched.
    SearchRest,
    /// `-argv` / `-no-argv`: how to read the search text after it.
    Argv(bool),
    /// `--json`: emit results as goz's JSON object.
    Json,
    /// `-get-result-count`: print only the number of results.
    ResultCount,
    /// `-get-total-size`: print only the summed size of the matching files.
    TotalSize,
    /// `-no-result-error`: exit with an error when nothing matches.
    NoResultError,
    /// `--status`: report the daemon's state instead of searching.
    Status,
    /// `--server-version`: print the daemon's version instead of searching.
    ServerVersion,
    /// `--insecure-no-server-check`: skip the pipe server's trust check.
    InsecureNoServerCheck,
    /// Native `--config`: consume `get`, or `set` and every later token as
    /// `KEY=0|1`.
    Config,
    /// A help switch: stop parsing and print that screen.
    Help(HelpScreen),
    /// A version switch: stop parsing and print goz's version.
    Version,
}

impl SwitchAction {
    /// The help's name for the parameter the switch consumes, or `None` for
    /// a switch that takes none. [`apply_switch`] consumes one exactly for
    /// the actions named here.
    pub(super) fn param(self) -> Option<&'static str> {
        match self {
            SwitchAction::Limit | SwitchAction::Offset => Some("N"),
            SwitchAction::Scope | SwitchAction::Parent => Some("DIR"),
            SwitchAction::ParentScope => Some("PATH"),
            SwitchAction::Regex => Some("PATTERN"),
            SwitchAction::Export(_) => Some("FILE"),
            SwitchAction::SortParam => Some("KEY"),
            SwitchAction::SearchParam => Some("TEXT"),
            SwitchAction::Config => Some("get|set"),
            SwitchAction::Format(_)
            | SwitchAction::Column(_)
            | SwitchAction::SortFixed(..)
            | SwitchAction::SortDirection(_)
            | SwitchAction::Bom(_)
            | SwitchAction::Header(_)
            | SwitchAction::DoubleQuote(_)
            | SwitchAction::MatchCase(_)
            | SwitchAction::MatchPath(_)
            | SwitchAction::Filter(_)
            | SwitchAction::Modifier(..)
            | SwitchAction::SearchRest
            | SwitchAction::Argv(_)
            | SwitchAction::Json
            | SwitchAction::ResultCount
            | SwitchAction::TotalSize
            | SwitchAction::NoResultError
            | SwitchAction::Status
            | SwitchAction::ServerVersion
            | SwitchAction::InsecureNoServerCheck
            | SwitchAction::Help(_)
            | SwitchAction::Version => None,
        }
    }
}

/// One row of [`SWITCHES`]: how a switch is spelled, and its help line.
pub(super) struct Switch {
    pub(super) spelling: Spelling,
    /// The section of the full help that lists it.
    pub(super) group: Group,
    /// Whether the quick help lists it, by its first name.
    pub(super) quick: bool,
    /// What the help says it does.
    pub(super) help: &'static str,
}

/// How a [`Switch`] is spelled on the command line.
pub(super) enum Spelling {
    /// Fixed names. Each `es` name takes `-` or `/`, every dash inside it
    /// optional; each `native` name takes `--` and matches exactly. The help
    /// shows the `es` names first, in order.
    Names {
        es: &'static [&'static str],
        native: &'static [&'static str],
        action: SwitchAction,
    },
    /// The `-sort-KEY[-ascending|-descending]` family: one switch for each
    /// name in [`SORT_KEYS`].
    SortFamily,
}

/// The sections of the full help, in the order it prints them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Group {
    Search,
    Output,
    Other,
}

/// Shorthand for a [`Spelling::Names`] row.
const fn names(
    es: &'static [&'static str],
    native: &'static [&'static str],
    action: SwitchAction,
) -> Spelling {
    Spelling::Names { es, native, action }
}

/// Shorthand for a row the quick help leaves out.
const fn row(
    es: &'static [&'static str],
    group: Group,
    action: SwitchAction,
    help: &'static str,
) -> Switch {
    Switch {
        spelling: names(es, &[], action),
        group,
        quick: false,
        help,
    }
}

/// Every switch goz accepts, in the order the help lists them.
pub(super) const SWITCHES: &[Switch] = &[
    Switch {
        spelling: names(&["path"], &[], SwitchAction::Scope),
        group: Group::Search,
        quick: true,
        help: "Search only below DIR",
    },
    Switch {
        spelling: names(
            &["case", "match-case", "i"],
            &[],
            SwitchAction::MatchCase(true),
        ),
        group: Group::Search,
        quick: true,
        help: "Match case",
    },
    row(
        &["no-case", "no-i"],
        Group::Search,
        SwitchAction::MatchCase(false),
        "Ignore case, the default",
    ),
    row(
        &["p", "match-path"],
        Group::Search,
        SwitchAction::MatchPath(true),
        "Match the search text against full paths, not only names",
    ),
    row(
        &["no-p", "no-match-path"],
        Group::Search,
        SwitchAction::MatchPath(false),
        "Match names only, the default",
    ),
    // Before `a-d`, which also matches `ad`, its dash being optional.
    row(
        &["ad"],
        Group::Search,
        SwitchAction::Filter("folder:"),
        "Only folders",
    ),
    row(
        &["a-d"],
        Group::Search,
        SwitchAction::Filter("file:"),
        "Only files",
    ),
    row(
        &["parent"],
        Group::Search,
        SwitchAction::Parent,
        "Only the files and folders right in DIR, not below it",
    ),
    row(
        &["parent-path"],
        Group::Search,
        SwitchAction::ParentScope,
        "Search only below the folder that holds PATH",
    ),
    row(
        &["r", "regex"],
        Group::Search,
        SwitchAction::Regex,
        "Read the search text, PATTERN included, as one regular expression",
    ),
    row(
        &["w", "ww", "whole-word", "whole-words"],
        Group::Search,
        SwitchAction::Modifier("ww", true),
        "Match whole words only, as ?ww: does",
    ),
    row(
        &["no-w", "no-ww", "no-whole-word", "no-whole-words"],
        Group::Search,
        SwitchAction::Modifier("ww", false),
        "Match anywhere in a word, the default",
    ),
    row(
        &["prefix"],
        Group::Search,
        SwitchAction::Modifier("prefix", true),
        "Match at the start of a word, as ?prefix: does",
    ),
    row(
        &["no-prefix"],
        Group::Search,
        SwitchAction::Modifier("prefix", false),
        "Turn -prefix off, the default",
    ),
    row(
        &["suffix"],
        Group::Search,
        SwitchAction::Modifier("suffix", true),
        "Match at the end of a word, as ?suffix: does",
    ),
    row(
        &["no-suffix"],
        Group::Search,
        SwitchAction::Modifier("suffix", false),
        "Turn -suffix off, the default",
    ),
    row(
        &["a", "diacritics"],
        Group::Search,
        SwitchAction::Modifier("diacritics", true),
        "Match accents, as ?diacritics: does",
    ),
    row(
        &["no-a", "no-diacritics"],
        Group::Search,
        SwitchAction::Modifier("diacritics", false),
        "Ignore accents, the default: cafe finds the accented name too",
    ),
    row(
        &["ignore-punctuation", "ignore-punc"],
        Group::Search,
        SwitchAction::Modifier("nopunc", true),
        "Skip punctuation in names, as ?nopunc: does",
    ),
    row(
        &["no-ignore-punctuation", "no-ignore-punc"],
        Group::Search,
        SwitchAction::Modifier("nopunc", false),
        "Turn -ignore-punctuation off, the default",
    ),
    row(
        &["ignore-white-space", "ignore-ws"],
        Group::Search,
        SwitchAction::Modifier("nows", true),
        "Skip white space in names, as ?nows: does",
    ),
    row(
        &["no-ignore-white-space", "no-ignore-ws"],
        Group::Search,
        SwitchAction::Modifier("nows", false),
        "Turn -ignore-white-space off, the default",
    ),
    row(
        &["q", "search"],
        Group::Search,
        SwitchAction::SearchParam,
        "Add TEXT to the search text, even if it looks like a switch",
    ),
    row(
        &["q*", "search*", "s*"],
        Group::Search,
        SwitchAction::SearchRest,
        "Read the rest of the command line as search text, as it is",
    ),
    Switch {
        spelling: names(
            &["n", "count", "max-results", "viewport-count"],
            &[],
            SwitchAction::Limit,
        ),
        group: Group::Search,
        quick: true,
        help: "Show at most N results",
    },
    row(
        &["o", "offset", "viewport-offset"],
        Group::Search,
        SwitchAction::Offset,
        "Skip the first N results",
    ),
    Switch {
        spelling: names(&["sort"], &[], SwitchAction::SortParam),
        group: Group::Search,
        quick: true,
        help: "Sort by KEY; KEY-ascending or KEY-descending sets the order",
    },
    Switch {
        spelling: Spelling::SortFamily,
        group: Group::Search,
        quick: false,
        help: "Same as -sort KEY, as in -sort-size-ascending",
    },
    row(
        &["sort-ascending"],
        Group::Search,
        SwitchAction::SortDirection(SortDir::Asc),
        "Sort in ascending order, whatever the key",
    ),
    row(
        &["sort-descending"],
        Group::Search,
        SwitchAction::SortDirection(SortDir::Desc),
        "Sort in descending order, whatever the key",
    ),
    row(
        &["s"],
        Group::Search,
        SwitchAction::SortFixed(SortKey::Path, Some(SortDir::Asc)),
        "Sort by path, A to Z",
    ),
    row(
        &["on"],
        Group::Search,
        SwitchAction::SortFixed(SortKey::Name, Some(SortDir::Asc)),
        "Sort by name, A to Z",
    ),
    row(
        &["o-n"],
        Group::Search,
        SwitchAction::SortFixed(SortKey::Name, Some(SortDir::Desc)),
        "Sort by name, Z to A",
    ),
    row(
        &["os"],
        Group::Search,
        SwitchAction::SortFixed(SortKey::Size, Some(SortDir::Asc)),
        "Sort by size, smallest first",
    ),
    row(
        &["o-s"],
        Group::Search,
        SwitchAction::SortFixed(SortKey::Size, Some(SortDir::Desc)),
        "Sort by size, largest first",
    ),
    row(
        &["od"],
        Group::Search,
        SwitchAction::SortFixed(SortKey::DateModified, Some(SortDir::Asc)),
        "Sort by date modified, oldest first",
    ),
    row(
        &["o-d"],
        Group::Search,
        SwitchAction::SortFixed(SortKey::DateModified, Some(SortDir::Desc)),
        "Sort by date modified, newest first",
    ),
    row(
        &["oe"],
        Group::Search,
        SwitchAction::SortFixed(SortKey::Extension, Some(SortDir::Asc)),
        "Sort by extension, A to Z",
    ),
    row(
        &["o-e"],
        Group::Search,
        SwitchAction::SortFixed(SortKey::Extension, Some(SortDir::Desc)),
        "Sort by extension, Z to A",
    ),
    Switch {
        spelling: names(
            &["export-csv"],
            &[],
            SwitchAction::Export(OutputFormat::Csv),
        ),
        group: Group::Output,
        quick: true,
        help: "Write the results to FILE as CSV",
    },
    row(
        &["export-tsv"],
        Group::Output,
        SwitchAction::Export(OutputFormat::Tsv),
        "Write the results to FILE as tab-separated values",
    ),
    row(
        &["export-efu"],
        Group::Output,
        SwitchAction::Export(OutputFormat::Efu),
        "Write the results to FILE as an Everything file list",
    ),
    row(
        &["export-txt"],
        Group::Output,
        SwitchAction::Export(OutputFormat::Txt),
        "Write the paths to FILE, one per line",
    ),
    row(
        &["export-m3u"],
        Group::Output,
        SwitchAction::Export(OutputFormat::M3u),
        "Write the paths to FILE as a playlist, in UTF-8 where es uses ANSI",
    ),
    row(
        &["export-m3u8"],
        Group::Output,
        SwitchAction::Export(OutputFormat::M3u8),
        "Write the paths to FILE as a UTF-8 playlist",
    ),
    row(
        &["export-json"],
        Group::Output,
        SwitchAction::Export(OutputFormat::Json),
        "Write the results to FILE as es's JSON array",
    ),
    row(
        &["csv"],
        Group::Output,
        SwitchAction::Format(OutputFormat::Csv),
        "Print CSV",
    ),
    row(
        &["tsv"],
        Group::Output,
        SwitchAction::Format(OutputFormat::Tsv),
        "Print tab-separated values",
    ),
    row(
        &["efu"],
        Group::Output,
        SwitchAction::Format(OutputFormat::Efu),
        "Print an Everything file list",
    ),
    row(
        &["txt"],
        Group::Output,
        SwitchAction::Format(OutputFormat::Txt),
        "Print the paths, one per CRLF line",
    ),
    row(
        &["m3u"],
        Group::Output,
        SwitchAction::Format(OutputFormat::M3u),
        "Print a playlist, in UTF-8 where es uses ANSI",
    ),
    row(
        &["m3u8"],
        Group::Output,
        SwitchAction::Format(OutputFormat::M3u8),
        "Print a UTF-8 playlist",
    ),
    row(
        &["json"],
        Group::Output,
        SwitchAction::Format(OutputFormat::Json),
        "Print es's JSON array; --json is goz's own object",
    ),
    Switch {
        spelling: names(&["size"], &[], SwitchAction::Column(EsColumn::Size)),
        group: Group::Output,
        quick: false,
        help: "Add a Size column",
    },
    Switch {
        spelling: names(
            &["dm", "date-modified"],
            &[],
            SwitchAction::Column(EsColumn::DateModified),
        ),
        group: Group::Output,
        quick: false,
        help: "Add a Date Modified column",
    },
    Switch {
        spelling: names(
            &["dc", "date-created"],
            &[],
            SwitchAction::Column(EsColumn::DateCreated),
        ),
        group: Group::Output,
        quick: false,
        help: "Add a Date Created column (read from disk unless indexed)",
    },
    Switch {
        spelling: names(
            &["da", "date-accessed"],
            &[],
            SwitchAction::Column(EsColumn::DateAccessed),
        ),
        group: Group::Output,
        quick: false,
        help: "Add a Date Accessed column (read from disk unless indexed)",
    },
    Switch {
        spelling: names(
            &["attributes", "attribs", "attrib"],
            &[],
            SwitchAction::Column(EsColumn::Attributes),
        ),
        group: Group::Output,
        quick: false,
        help: "Add an Attributes column (read from disk unless indexed)",
    },
    Switch {
        spelling: names(
            &["rc", "date-recently-changed"],
            &[],
            SwitchAction::Column(EsColumn::DateRecentlyChanged),
        ),
        group: Group::Output,
        quick: false,
        help: "Add a Date Recently Changed column (empty unless recent changes are indexed)",
    },
    row(
        &["utf8-bom"],
        Group::Output,
        SwitchAction::Bom(true),
        "Start the output with a byte order mark, the default for -export-csv",
    ),
    row(
        &["no-utf8-bom"],
        Group::Output,
        SwitchAction::Bom(false),
        "Leave out the byte order mark",
    ),
    row(
        &["header"],
        Group::Output,
        SwitchAction::Header(Some(true)),
        "Add a header row, the default except for -txt",
    ),
    row(
        &["no-header"],
        Group::Output,
        SwitchAction::Header(Some(false)),
        "Leave out the header row",
    ),
    row(
        &["default-header"],
        Group::Output,
        SwitchAction::Header(None),
        "Add a header row if the format has one by default",
    ),
    row(
        &["double-quote"],
        Group::Output,
        SwitchAction::DoubleQuote(true),
        "Always quote paths, the default for CSV",
    ),
    row(
        &["no-double-quote"],
        Group::Output,
        SwitchAction::DoubleQuote(false),
        "Quote a CSV or TSV value only when it needs quotes",
    ),
    Switch {
        spelling: names(&[], &["json"], SwitchAction::Json),
        group: Group::Output,
        quick: true,
        help: "Print the results as goz's JSON object",
    },
    Switch {
        spelling: names(&["get-result-count"], &[], SwitchAction::ResultCount),
        group: Group::Output,
        quick: false,
        help: "Print only the number of results",
    },
    Switch {
        spelling: names(&["get-total-size"], &[], SwitchAction::TotalSize),
        group: Group::Output,
        quick: false,
        help: "Print only the total size of the matching files",
    },
    row(
        &["no-result-error", "no-results-error", "error-on-no-results"],
        Group::Output,
        SwitchAction::NoResultError,
        "Exit with code 9 when nothing matches",
    ),
    Switch {
        spelling: names(&[], &["status"], SwitchAction::Status),
        group: Group::Other,
        quick: true,
        help: "Show the state of gozd and each volume",
    },
    Switch {
        spelling: names(&[], &["server-version"], SwitchAction::ServerVersion),
        group: Group::Other,
        quick: false,
        help: "Print the version of the running gozd",
    },
    Switch {
        spelling: names(&[], &["config"], SwitchAction::Config),
        group: Group::Other,
        quick: false,
        help: "Print gozd's index options; set KEY=0|1 ... changes them, from an elevated \
               prompt: index_date_created, index_attributes ... (see README)",
    },
    row(
        &["argv"],
        Group::Other,
        SwitchAction::Argv(true),
        "Read later search text with its quotes removed, as most programs do",
    ),
    row(
        &["no-argv", "default-argv"],
        Group::Other,
        SwitchAction::Argv(false),
        "Read later search text as es does, the default: quotes make a phrase",
    ),
    Switch {
        spelling: names(&["h", "?"], &[], SwitchAction::Help(HelpScreen::Quick)),
        group: Group::Other,
        quick: false,
        help: "Show the quick help",
    },
    Switch {
        spelling: names(&["help"], &["help"], SwitchAction::Help(HelpScreen::Full)),
        group: Group::Other,
        quick: false,
        help: "Show this reference",
    },
    Switch {
        spelling: names(&["v", "version"], &["version"], SwitchAction::Version),
        group: Group::Other,
        quick: false,
        help: "Print the version of goz",
    },
    Switch {
        spelling: names(
            &[],
            &["insecure-no-server-check"],
            SwitchAction::InsecureNoServerCheck,
        ),
        group: Group::Other,
        quick: false,
        help: "Skip the check that the pipe server is the elevated gozd, for testing",
    },
];

/// One row of [`SORT_KEYS`]: a key's spellings, accepted by both `-sort KEY`
/// and `-sort-KEY`, and its help line.
pub(super) struct SortKeyRow {
    /// The help lists the first name first.
    pub(super) names: &'static [&'static str],
    pub(super) key: SortKey,
    /// What the key orders by, as the help starts its line.
    pub(super) what: &'static str,
    /// The order ascending and descending give, as the help ends its line
    /// with the one [`SortSpec::default_for`] picks.
    pub(super) orders: [&'static str; 2],
}

/// Every sort key, in the order the help lists them.
pub(super) const SORT_KEYS: &[SortKeyRow] = &[
    SortKeyRow {
        names: &["name"],
        key: SortKey::Name,
        what: "Name",
        orders: ["A to Z", "Z to A"],
    },
    SortKeyRow {
        names: &["path"],
        key: SortKey::Path,
        what: "Folder, then name",
        orders: ["A to Z", "Z to A"],
    },
    SortKeyRow {
        names: &["size"],
        key: SortKey::Size,
        what: "Size",
        orders: ["smallest first", "largest first"],
    },
    SortKeyRow {
        names: &["dm", "date-modified"],
        key: SortKey::DateModified,
        what: "Date modified",
        orders: ["oldest first", "newest first"],
    },
    SortKeyRow {
        names: &["extension", "ext"],
        key: SortKey::Extension,
        what: "Extension, folders first, then name",
        orders: ["A to Z", "Z to A"],
    },
    SortKeyRow {
        names: &["dc", "date-created"],
        key: SortKey::DateCreated,
        what: "Date created",
        orders: ["oldest first", "newest first"],
    },
    SortKeyRow {
        names: &["da", "date-accessed"],
        key: SortKey::DateAccessed,
        what: "Date accessed",
        orders: ["oldest first", "newest first"],
    },
    SortKeyRow {
        names: &["attributes", "attribs", "attrib"],
        key: SortKey::Attributes,
        what: "Attributes",
        orders: ["lowest first", "highest first"],
    },
    SortKeyRow {
        names: &["rc", "date-recently-changed"],
        key: SortKey::DateRecentlyChanged,
        what: "Date recently changed",
        orders: ["oldest first", "newest first"],
    },
];

/// Where a command line's tokens come from, read as the switch before them
/// asks: es reads search text and a switch's parameter by different rules.
trait Tokens {
    /// The next search text or switch, or `None` at the end. Fails when the
    /// last token was `-name=value` and its switch took no parameter.
    fn next_token(&mut self) -> Result<Option<String>, EsFatal>;
    /// The next token as a switch's parameter, or `None` at the end.
    fn next_param(&mut self) -> Option<String>;
    /// Everything left, as it is.
    fn rest(&mut self) -> String;
    /// `-argv`: how to read the search text from here on.
    fn set_argv(&mut self, argv: bool);
}

/// The tokens of an argv slice, already split and unquoted.
struct ArgTokens<'a> {
    args: core::slice::Iter<'a, String>,
    /// The name and value of a `-name=value` just read, until a switch takes
    /// the value.
    after_eq: Option<(String, String)>,
}

impl Tokens for ArgTokens<'_> {
    fn next_token(&mut self) -> Result<Option<String>, EsFatal> {
        if let Some((name, _)) = self.after_eq.take() {
            return Err(EsFatal::unknown_switch(&format!("{name}=")));
        }
        let Some(token) = self.args.next() else {
            return Ok(None);
        };
        if token.starts_with('-')
            && let Some((name, value)) = token.split_once('=')
        {
            self.after_eq = Some((name.to_owned(), value.to_owned()));
            return Ok(Some(name.to_owned()));
        }
        Ok(Some(token.clone()))
    }

    fn next_param(&mut self) -> Option<String> {
        if let Some((_, value)) = self.after_eq.take() {
            return Some(value);
        }
        self.args.next().cloned()
    }

    fn rest(&mut self) -> String {
        let tokens: Vec<&str> = self.args.by_ref().map(String::as_str).collect();
        tokens.join(" ")
    }

    /// The tokens are split already.
    fn set_argv(&mut self, _argv: bool) {}
}

/// The tokens of a raw command line, read as es's `_es_get_argv` and
/// `_es_get_command_argv` read them.
struct LineTokens<'a> {
    rest: &'a str,
    /// The name of a `-name=value` just read, until a switch takes the value.
    after_eq: Option<String>,
    argv: bool,
}

impl LineTokens<'_> {
    /// `rest` from where its next token starts.
    fn skip_space(&mut self) {
        self.rest = self
            .rest
            .trim_start_matches(|c: char| c.is_ascii_whitespace());
    }
}

impl Tokens for LineTokens<'_> {
    fn next_token(&mut self) -> Result<Option<String>, EsFatal> {
        if let Some(name) = self.after_eq.take() {
            return Err(EsFatal::unknown_switch(&format!("{name}=")));
        }
        self.skip_space();
        let line = self.rest;
        let dashed = line.starts_with('-');
        let mut token = String::new();
        let (mut in_quote, mut was_quote, mut was_eq) = (false, false, false);
        let mut at = 0;
        while let Some(c) = line[at..].chars().next() {
            let tail = &line[at..];
            if !in_quote && c.is_ascii_whitespace() {
                break;
            }
            if !was_quote && c == '=' && dashed {
                was_eq = true;
                at += 1;
                break;
            }
            if self.argv {
                if in_quote && (tail.starts_with("\\\"") || tail.starts_with("\\\\")) {
                    token.push(if tail.starts_with("\\\"") { '"' } else { '\\' });
                    at += 2;
                    continue;
                }
                if c == '"' {
                    in_quote = !in_quote;
                    was_quote = true;
                    at += 1;
                    continue;
                }
            } else {
                // Everything's own spelling of a literal quote.
                if tail.starts_with("\"\"\"") {
                    token.push_str("&quot:");
                    at += 3;
                    continue;
                }
                if c == '"' {
                    token.push('"');
                    in_quote = !in_quote;
                    was_quote = true;
                    at += 1;
                    continue;
                }
            }
            token.push(c);
            at += c.len_utf8();
        }
        self.rest = &line[at..];
        if token.is_empty() && !was_eq && !was_quote {
            return Ok(None);
        }
        if was_eq {
            self.after_eq = Some(token.clone());
        }
        Ok(Some(token))
    }

    fn next_param(&mut self) -> Option<String> {
        // A value after `=` starts at once and may be empty.
        if self.after_eq.take().is_none() {
            self.skip_space();
            if self.rest.is_empty() {
                return None;
            }
        }
        let line = self.rest;
        let mut param = String::new();
        let mut in_quote = false;
        let mut at = 0;
        while let Some(c) = line[at..].chars().next() {
            if !in_quote && c.is_ascii_whitespace() {
                break;
            }
            at += c.len_utf8();
            if c != '"' {
                param.push(c);
            } else if line[at..].starts_with("\"\"") {
                param.push('"');
                at += 2;
            } else {
                in_quote = !in_quote;
            }
        }
        self.rest = &line[at..];
        Some(param)
    }

    fn rest(&mut self) -> String {
        self.after_eq = None;
        self.skip_space();
        core::mem::take(&mut self.rest).to_owned()
    }

    fn set_argv(&mut self, argv: bool) {
        self.argv = argv;
    }
}

/// What a token is.
enum Token {
    SearchText,
    /// `--`: every later token is search text.
    EndOfSwitches,
    Switch(SwitchAction),
    /// `/a<attributes>`: these `attrib:` terms join the search.
    Attrib(String),
}

/// Parses an argv slice (excluding `argv[0]`) into a [`Command`].
///
/// Pure and total: no I/O, no environment access, no panics on any input.
/// Fatal parse errors return [`EsFatal`] with the es exit code (4 or 6).
///
/// ```
/// use goz_core::escompat::{Command, parse_argv};
///
/// let args: Vec<String> = ["report", "-sortpath", "-n", "10"]
///     .iter()
///     .map(ToString::to_string)
///     .collect();
/// let Ok(Command::Run(plan)) = parse_argv(&args) else {
///     panic!("expected a search");
/// };
/// assert_eq!(plan.query, "report");
/// assert_eq!(plan.limit, Some(10));
/// ```
pub fn parse_argv(args: &[String]) -> Result<Command, EsFatal> {
    parse_tokens(ArgTokens {
        args: args.iter(),
        after_eq: None,
    })
}

/// Parses a raw command line, program name included, into a [`Command`], as
/// es reads its own: `GetCommandLineW`, not the split `argv`, since splitting
/// loses the quotes that make a phrase and takes `"C:\dir\"` for an escaped
/// quote. Pure and total, as [`parse_argv`] is.
///
/// ```
/// use goz_core::escompat::{Command, parse_command_line};
///
/// let Ok(Command::Run(plan)) = parse_command_line(r#"goz "a b" -path "C:\my dir\""#) else {
///     panic!("expected a search");
/// };
/// assert_eq!(plan.query, r#""a b""#);
/// assert_eq!(plan.scope.as_deref(), Some(r"C:\my dir\"));
/// ```
pub fn parse_command_line(line: &str) -> Result<Command, EsFatal> {
    let mut tokens = LineTokens {
        rest: line,
        after_eq: None,
        argv: false,
    };
    // The program's name.
    tokens.next_token()?;
    tokens.after_eq = None;
    parse_tokens(tokens)
}

/// What the switches say about the search, until the plan can be finished.
#[derive(Default)]
struct Pending {
    terms: Vec<String>,
    filters: Vec<String>,
    match_path: bool,
    /// The search modifiers set for the whole search, each once, the last
    /// setting winning.
    modifiers: Vec<(&'static str, bool)>,
    /// `-r`: the whole search text is one regular expression.
    regex: bool,
    sort_key: Option<SortKey>,
    sort_dir: Option<SortDir>,
}

fn parse_tokens(mut tokens: impl Tokens) -> Result<Command, EsFatal> {
    let mut plan = EsPlan::default();
    let mut pending = Pending::default();

    while let Some(token) = tokens.next_token()? {
        match classify(&token)? {
            Token::SearchText => pending.terms.push(token),
            Token::EndOfSwitches => {
                while let Some(term) = tokens.next_token()? {
                    pending.terms.push(term);
                }
            }
            Token::Attrib(terms) => pending.filters.push(terms),
            Token::Switch(SwitchAction::Help(screen)) => return Ok(Command::Help(screen)),
            Token::Switch(SwitchAction::Version) => return Ok(Command::Version),
            Token::Switch(action) => {
                apply_switch(action, &token, &mut tokens, &mut plan, &mut pending)?;
            }
        }
    }

    let mut text = pending.terms.join(" ");
    if pending.regex {
        text.insert_str(0, "regex*:");
    }
    check_search_text(&text, plan.match_case)?;
    // The switches' terms go first: after the text, an operator, an open
    // quote or a `name*:` there would take them in (see the module docs).
    let mut query: Vec<String> = pending.filters.iter().map(|f| f.to_string()).collect();
    if pending.match_path {
        query.push("matchpath:".to_string());
    }
    for (name, on) in &pending.modifiers {
        if *on {
            query.push(format!("?{name}:"));
        }
    }
    let text_len = text.len();
    if !text.is_empty() {
        query.push(text);
    }
    plan.query = query.join(" ");
    plan.text_at = plan.query.len() - text_len;
    let key = pending.sort_key.unwrap_or(plan.sort.key);
    plan.sort = SortSpec {
        key,
        dir: pending
            .sort_dir
            .unwrap_or_else(|| SortSpec::default_for(key).dir),
    };
    Ok(Command::Run(plan))
}

/// Refuses search text that gozd would refuse whatever its clock: an
/// unbalanced `<` or `>`, group expansion, an unknown function, a malformed
/// size, nesting too deep, too many terms; and more `content:` terms than
/// the client searches in one query. A `dm:` value is left to gozd, which
/// reads it against its own clock and regional date order. A content term
/// is the client's to search (see `query::content`), so it passes.
fn check_search_text(text: &str, match_case: bool) -> Result<(), EsFatal> {
    match ContentQuery::new(text, match_case) {
        Ok(_) => Ok(()),
        Err(error) => Err(EsFatal::bad_search(&error)),
    }
}

/// Tells search text from a switch, by es's rules (see the module docs).
fn classify(token: &str) -> Result<Token, EsFatal> {
    if token == "-" {
        return Ok(Token::SearchText);
    }
    if let Some(name) = token.strip_prefix("--") {
        if name.bytes().all(|b| b == b'-') {
            return Ok(Token::EndOfSwitches);
        }
        return match_native(name)
            .or_else(|| match_switch(name))
            .map(Token::Switch)
            .ok_or_else(|| refuse(token, name));
    }
    if let Some(name) = token.strip_prefix('-') {
        if let Some(action) = match_switch(name) {
            return Ok(Token::Switch(action));
        }
        return attribute_token(token, name).unwrap_or_else(|| Err(refuse(token, name)));
    }
    if let Some(name) = token.strip_prefix('/') {
        let name = name.strip_prefix('-').unwrap_or(name);
        if let Some(action) = match_switch(name) {
            return Ok(Token::Switch(action));
        }
        if is_es_only(name) {
            return Err(EsFatal::unsupported_switch(token));
        }
        if let Some(attrib) = attribute_token(token, name) {
            return attrib;
        }
    }
    Ok(Token::SearchText)
}

/// es's `DIR`-style attribute filter, `/ah`, `/a-h`, `/ar-hs`, as `attrib:`
/// terms (see [`attribute_terms`]); an error for one naming a letter es
/// does not have; `None` for a name that is no attribute filter, or one es
/// has as another switch.
fn attribute_token(token: &str, name: &str) -> Option<Result<Token, EsFatal>> {
    if !is_attribute_filter(name) || is_es_only(name) {
        return None;
    }
    Some(
        attribute_terms(&name[1..])
            .map(Token::Attrib)
            .ok_or_else(|| EsFatal::unknown_switch(token)),
    )
}

/// The `attrib:` terms of an es attribute filter's letters: those the
/// entry has together (`attrib:RH`), then a `!attrib:` for each one after a
/// `-`, which it lacks. es's V is Device (0x40), which Everything's `attrib:`
/// letters name otherwise, so it goes as its number. `None` for any other
/// character, or a `-` naming nothing.
pub(super) fn attribute_terms(letters: &str) -> Option<String> {
    let mut have = String::new();
    let mut device = false;
    let mut lack = Vec::new();
    let mut not = false;
    for c in letters.chars() {
        if c == '-' && !not {
            not = true;
            continue;
        }
        let up = c.to_ascii_uppercase();
        if !"RHSDAVXNTLCOIEUPM".contains(up) || c == '-' {
            return None;
        }
        let term = if up == 'V' {
            "0x40".to_string()
        } else {
            up.to_string()
        };
        if not {
            lack.push(format!("!attrib:{term}"));
            not = false;
        } else if up == 'V' {
            device = true;
        } else if !have.contains(up) {
            have.push(up);
        }
    }
    if not {
        return None;
    }
    let mut terms = Vec::new();
    if !have.is_empty() {
        terms.push(format!("attrib:{have}"));
    }
    if device {
        terms.push("attrib:0x40".to_string());
    }
    terms.extend(lack);
    (!terms.is_empty()).then(|| terms.join(" "))
}

/// The error for a `-` switch goz does not know.
fn refuse(token: &str, name: &str) -> EsFatal {
    if is_es_only(name) {
        EsFatal::unsupported_switch(token)
    } else {
        EsFatal::unknown_switch(token)
    }
}

/// Whether es has a switch of this name and goz does not.
fn is_es_only(name: &str) -> bool {
    ES_ONLY_SWITCHES
        .iter()
        .any(|canonical| switch_name_matches(name, canonical))
}

/// Whether es reads this name as an attribute filter, the `DIR`-style
/// `/ah` and `/a-h`. `/ad` and `/a-d` are switches of their own.
fn is_attribute_filter(name: &str) -> bool {
    name.len() > 1 && name.starts_with('a')
}

/// Resolves a name without its `--` against the native names in
/// [`SWITCHES`].
fn match_native(name: &str) -> Option<SwitchAction> {
    SWITCHES.iter().find_map(|switch| match switch.spelling {
        Spelling::Names { native, action, .. } => native.contains(&name).then_some(action),
        Spelling::SortFamily => None,
    })
}

/// Resolves a name without its `-` or `/` against the es names and the sort
/// family in [`SWITCHES`]. Returns `None` when nothing matches (caller
/// decides fatal vs fallthrough).
fn match_switch(name: &str) -> Option<SwitchAction> {
    SWITCHES.iter().find_map(|switch| match switch.spelling {
        Spelling::Names { es, action, .. } => es
            .iter()
            .any(|canonical| switch_name_matches(name, canonical))
            .then_some(action),
        Spelling::SortFamily => SORT_KEYS.iter().find_map(|row| {
            row.names.iter().find_map(|alias| {
                match_sort_form(name, &format!("sort-{alias}"), row.key)
                    .map(|(key, dir)| SwitchAction::SortFixed(key, dir))
            })
        }),
    })
}

/// Applies one matched switch, consuming its parameter from `tokens` if it
/// takes one. `token` is the switch as the user typed it (for messages).
/// The help and version actions end parsing in [`parse_tokens`] and never
/// reach here.
fn apply_switch(
    action: SwitchAction,
    token: &str,
    tokens: &mut impl Tokens,
    plan: &mut EsPlan,
    pending: &mut Pending,
) -> Result<(), EsFatal> {
    match action {
        SwitchAction::Limit => plan.limit = Some(take_count(tokens, token)?),
        SwitchAction::Offset => plan.offset = take_count(tokens, token)?,
        SwitchAction::Scope => {
            plan.scope = Some(take_param(tokens, token)?);
            plan.scope_is_parent_of = false;
        }
        SwitchAction::ParentScope => {
            plan.scope = Some(take_param(tokens, token)?);
            plan.scope_is_parent_of = true;
        }
        SwitchAction::Parent => plan.parent = Some(take_param(tokens, token)?),
        SwitchAction::Regex => {
            pending.terms.push(take_param(tokens, token)?);
            pending.regex = true;
        }
        SwitchAction::Modifier(name, on) => {
            pending.modifiers.retain(|(other, _)| *other != name);
            pending.modifiers.push((name, on));
        }
        SwitchAction::Export(format) => {
            plan.export_file = Some(take_param(tokens, token)?);
            plan.format = Some(format);
        }
        SwitchAction::Format(format) => plan.format = Some(format),
        SwitchAction::SortParam => {
            let value = take_param(tokens, token)?;
            let (key, dir) = parse_sort_param(&value)
                .ok_or_else(|| EsFatal::bad_param(token, &value, "a sort key"))?;
            set_sort(pending, key, dir);
        }
        SwitchAction::Column(column) => {
            // es `es_add_column`: remove an existing instance, then append,
            // so the last occurrence determines the column's position.
            plan.columns.retain(|existing| *existing != column);
            plan.columns.push(column);
        }
        SwitchAction::SortFixed(key, dir) => set_sort(pending, key, dir),
        SwitchAction::SortDirection(dir) => pending.sort_dir = Some(dir),
        SwitchAction::Bom(on) => plan.bom = Some(on),
        SwitchAction::Header(on) => plan.header = on,
        SwitchAction::DoubleQuote(on) => {
            plan.double_quote = on;
            plan.text_double_quote = on;
        }
        SwitchAction::MatchCase(on) => plan.match_case = on,
        SwitchAction::MatchPath(on) => pending.match_path = on,
        SwitchAction::Filter(filter) => pending.filters.push(filter.to_string()),
        SwitchAction::Config => plan.config = Some(take_config(tokens, token)?),
        SwitchAction::SearchParam => pending.terms.push(take_param(tokens, token)?),
        SwitchAction::SearchRest => {
            let rest = tokens.rest();
            if !rest.is_empty() {
                pending.terms.push(rest);
            }
        }
        SwitchAction::Argv(on) => tokens.set_argv(on),
        SwitchAction::Json => plan.json = true,
        SwitchAction::ResultCount => plan.result_count = true,
        SwitchAction::TotalSize => plan.total_size = true,
        SwitchAction::NoResultError => plan.no_result_error = true,
        SwitchAction::Status => plan.status = true,
        SwitchAction::ServerVersion => plan.server_version = true,
        SwitchAction::InsecureNoServerCheck => plan.insecure_no_server_check = true,
        // Handled by the caller; nothing to apply.
        SwitchAction::Help(_) | SwitchAction::Version => {}
    }
    Ok(())
}

/// Sets the sort's key, and its direction if one is named. An unnamed one
/// stays as it was, as es's `_es_check_sorts` leaves it.
fn set_sort(pending: &mut Pending, key: SortKey, dir: Option<SortDir>) {
    pending.sort_key = Some(key);
    if dir.is_some() {
        pending.sort_dir = dir;
    }
}

/// Consumes the next token as a switch parameter, or fails with exit 4
/// naming the switch as the user typed it.
fn take_param(tokens: &mut impl Tokens, token: &str) -> Result<String, EsFatal> {
    tokens
        .next_param()
        .ok_or_else(|| EsFatal::missing_param(token))
}

/// Consumes the next token as a count: decimal, or hexadecimal after `0x`,
/// as es reads it.
fn take_count(tokens: &mut impl Tokens, token: &str) -> Result<u32, EsFatal> {
    let value = take_param(tokens, token)?;
    let parsed = match value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        Some(hex) => u32::from_str_radix(hex, 16),
        None => value.parse(),
    };
    parsed.map_err(|_| EsFatal::bad_param(token, &value, "a number"))
}

/// Resolves a `-sort` parameter value (`name`, `dm`, `date-modified`, ...
/// optionally suffixed `-ascending` / `-descending`) into a key and the
/// direction it names, if any. Uses the same dash-optional matching as switch
/// names.
fn parse_sort_param(value: &str) -> Option<(SortKey, Option<SortDir>)> {
    SORT_KEYS.iter().find_map(|row| {
        row.names
            .iter()
            .find_map(|alias| match_sort_form(value, alias, row.key))
    })
}

/// Matches `input` against `base`, `base-ascending`, or `base-descending`
/// (interior dashes optional), giving the direction a suffix names.
fn match_sort_form(input: &str, base: &str, key: SortKey) -> Option<(SortKey, Option<SortDir>)> {
    if switch_name_matches(input, base) {
        return Some((key, None));
    }
    if switch_name_matches(input, &format!("{base}-ascending")) {
        return Some((key, Some(SortDir::Asc)));
    }
    if switch_name_matches(input, &format!("{base}-descending")) {
        return Some((key, Some(SortDir::Desc)));
    }
    None
}

/// `--config`'s parameter: `get`, or `set` and every later token as
/// `KEY=0|1` (Everything's option names; `KEY=on|off|true|false` too).
fn take_config(tokens: &mut impl Tokens, token: &str) -> Result<ConfigCommand, EsFatal> {
    let what = take_param(tokens, token)?;
    match what.as_str() {
        "get" => Ok(ConfigCommand::Get),
        "set" => {
            let mut change = ConfigChange::default();
            let mut any = false;
            while let Some(pair) = tokens.next_token()? {
                let bad = || {
                    let keys = CONFIG_KEYS.join(", ");
                    EsFatal::bad_param(token, &pair, &format!("KEY=0|1 with KEY one of {keys}"))
                };
                let (key, value) = pair.split_once('=').ok_or_else(bad)?;
                let on = match value.to_ascii_lowercase().as_str() {
                    "1" | "on" | "true" | "yes" => true,
                    "0" | "off" | "false" | "no" => false,
                    _ => return Err(bad()),
                };
                *change.slot(key).ok_or_else(bad)? = Some(on);
                any = true;
            }
            if any {
                Ok(ConfigCommand::Set(change))
            } else {
                Err(EsFatal::missing_param(token))
            }
        }
        other => Err(EsFatal::bad_param(token, other, "get or set")),
    }
}

/// es switch-name comparison (`es_check_param`): case-sensitive, and every
/// dash in the canonical name optionally consumes one dash from the input.
/// Input dashes anywhere else fail the match.
fn switch_name_matches(input: &str, canonical: &str) -> bool {
    let mut input_chars = input.chars().peekable();
    for expected in canonical.chars() {
        if expected == '-' {
            if input_chars.peek() == Some(&'-') {
                input_chars.next();
            }
        } else if input_chars.next() != Some(expected) {
            return false;
        }
    }
    input_chars.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(tokens: &[&str]) -> Result<Command, EsFatal> {
        let args: Vec<String> = tokens.iter().map(ToString::to_string).collect();
        parse_argv(&args)
    }

    fn plan(tokens: &[&str]) -> EsPlan {
        match parse(tokens).expect("expected a successful parse") {
            Command::Run(plan) => plan,
            other => panic!("expected a search, got {other:?}"),
        }
    }

    fn fatal(tokens: &[&str]) -> EsFatal {
        parse(tokens).expect_err("expected a fatal parse error")
    }

    use proptest::prelude::*;

    proptest! {
        /// The argv parser is total: for ANY input it returns Ok or a fatal
        /// `EsFatal`, never panicking (its documented contract).
        #[test]
        fn parse_argv_never_panics_on_arbitrary_input(
            args in proptest::collection::vec(".*", 0..12),
        ) {
            let _ = parse_argv(&args);
        }

        /// Flag-shaped tokens (leading `-`/`/` plus arbitrary text, missing
        /// switch parameters, empty tokens) also never panic.
        #[test]
        fn parse_argv_never_panics_on_flaglike_input(
            args in proptest::collection::vec(
                prop_oneof!["-[a-zA-Z-]{0,16}", "/[a-zA-Z-]{0,16}", "[^ ]{0,16}"],
                0..12,
            ),
        ) {
            let _ = parse_argv(&args);
        }
    }

    #[test]
    fn empty_argv_yields_default_plan() {
        // Bare `goz` stays es's list-everything search, not the help.
        assert_eq!(parse(&[]), Ok(Command::Run(EsPlan::default())));
        let parsed = plan(&[]);
        assert_eq!(parsed, EsPlan::default());
        assert_eq!(parsed.query, "");
        assert_eq!(parsed.sort, SortSpec::default());
        assert!(!parsed.wants_bom(), "off without a CSV export");
        assert!(parsed.wants_header());
        assert!(parsed.double_quote);
        assert!(!parsed.text_double_quote);
        assert!(!parsed.match_case);
    }

    #[test]
    fn app_reference_invocation_full_plan() {
        let parsed = plan(&[
            "-path",
            "C:\\some dir",
            "report",
            "-n",
            "200",
            "-size",
            "-dm",
            "-sort-path",
            "-export-csv",
            "C:\\tmp\\out.csv",
        ]);
        assert_eq!(
            parsed,
            EsPlan {
                query: "report".to_string(),
                scope: Some("C:\\some dir".to_string()),
                limit: Some(200),
                columns: vec![EsColumn::Size, EsColumn::DateModified],
                sort: SortSpec {
                    key: SortKey::Path,
                    dir: SortDir::Asc,
                },
                format: Some(OutputFormat::Csv),
                export_file: Some("C:\\tmp\\out.csv".to_string()),
                ..EsPlan::default()
            }
        );
    }

    #[test]
    fn get_result_count_asks_for_the_number_only() {
        for spelling in ["-get-result-count", "-getresultcount", "/get-result-count"] {
            let parsed = plan(&["report", spelling, "-n", "5"]);
            assert!(parsed.result_count, "{spelling}");
            assert_eq!(parsed.query, "report");
        }
        assert!(!plan(&["report"]).result_count);
    }

    #[test]
    fn get_total_size_asks_for_the_size_only() {
        for spelling in ["-get-total-size", "-gettotalsize", "/get-total-size"] {
            let parsed = plan(&["report", spelling, "-n", "5"]);
            assert!(parsed.total_size, "{spelling}");
            assert!(!parsed.result_count, "{spelling}");
            assert_eq!(parsed.query, "report");
        }
        assert!(!plan(&["report"]).total_size);
    }

    #[test]
    fn interior_dashes_are_optional_everywhere() {
        assert_eq!(
            plan(&["-sortpath"]).sort,
            SortSpec::default_for(SortKey::Path)
        );
        assert_eq!(plan(&["-maxresults", "7"]).limit, Some(7));
        assert_eq!(plan(&["-noutf8bom"]).bom, Some(false));
        assert!(!plan(&["-nodoublequote"]).double_quote);
        assert_eq!(
            plan(&["-datemodified"]).columns,
            vec![EsColumn::DateModified]
        );
        assert_eq!(
            plan(&["-sortpathascending"]).sort,
            SortSpec {
                key: SortKey::Path,
                dir: SortDir::Asc,
            }
        );
        assert_eq!(
            plan(&["-sortdatemodifieddescending"]).sort,
            SortSpec {
                key: SortKey::DateModified,
                dir: SortDir::Desc,
            }
        );
        // Partial omission is fine too: any subset of dashes may be dropped.
        assert_eq!(plan(&["-no-utf8bom"]).bom, Some(false));
        assert_eq!(plan(&["-max-results", "3"]).limit, Some(3));
    }

    #[test]
    fn switch_matching_is_case_sensitive() {
        assert_eq!(fatal(&["-SORT-PATH"]).exit_code, 6);
        assert_eq!(fatal(&["-N", "5"]).exit_code, 6);
    }

    #[test]
    fn input_dash_where_canonical_has_none_is_unknown() {
        assert_eq!(fatal(&["-d-m"]).exit_code, 6);
        assert_eq!(fatal(&["-si-ze"]).exit_code, 6);
        assert_eq!(fatal(&["-no--header"]).exit_code, 6);
    }

    #[test]
    fn unknown_dash_switch_is_exit_6_with_token() {
        let err = fatal(&["-zzz"]);
        assert_eq!(err.exit_code, 6);
        assert!(err.message.contains("-zzz"), "message was: {}", err.message);
    }

    #[test]
    fn slash_token_without_match_falls_through_to_query() {
        assert_eq!(plan(&["/nomatch"]).query, "/nomatch");
        assert_eq!(plan(&["report", "/downloads"]).query, "report /downloads");
    }

    #[test]
    fn slash_token_with_match_is_a_switch() {
        // Research: es_check_param treats `-` and `/` prefixes identically,
        // so /n is the -n switch, not search text.
        let parsed = plan(&["/n", "50"]);
        assert_eq!(parsed.limit, Some(50));
        assert_eq!(parsed.query, "");
        // A matched `/` switch with a missing parameter is exit 4 like `-`.
        assert_eq!(fatal(&["/path"]).exit_code, 4);
    }

    #[test]
    fn missing_parameter_is_exit_4_naming_the_switch() {
        for switch in ["-n", "-path", "-export-csv", "-sort", "-maxresults"] {
            let err = fatal(&[switch]);
            assert_eq!(err.exit_code, 4, "switch: {switch}");
            assert!(
                err.message.contains(switch),
                "message for {switch} was: {}",
                err.message
            );
        }
    }

    #[test]
    fn non_numeric_count_is_exit_4() {
        assert_eq!(fatal(&["-n", "abc"]).exit_code, 4);
        assert_eq!(fatal(&["-n", "-5"]).exit_code, 4);
        // Overflowing u32 is a bad parameter, not a silent wrap.
        assert_eq!(fatal(&["-n", "4294967296"]).exit_code, 4);
    }

    #[test]
    fn sort_param_defaults_direction_per_key() {
        assert_eq!(
            plan(&["-sort", "dm"]).sort,
            SortSpec {
                key: SortKey::DateModified,
                dir: SortDir::Desc,
            }
        );
        assert_eq!(
            plan(&["-sort", "size"]).sort,
            SortSpec {
                key: SortKey::Size,
                dir: SortDir::Desc,
            }
        );
        assert_eq!(
            plan(&["-sort", "name"]).sort,
            SortSpec {
                key: SortKey::Name,
                dir: SortDir::Asc,
            }
        );
        assert_eq!(
            plan(&["-sort", "path"]).sort,
            SortSpec {
                key: SortKey::Path,
                dir: SortDir::Asc,
            }
        );
    }

    #[test]
    fn sort_param_with_direction_suffix() {
        assert_eq!(
            plan(&["-sort", "name-descending"]).sort,
            SortSpec {
                key: SortKey::Name,
                dir: SortDir::Desc,
            }
        );
        assert_eq!(
            plan(&["-sort", "date-modified-ascending"]).sort,
            SortSpec {
                key: SortKey::DateModified,
                dir: SortDir::Asc,
            }
        );
        // The parameter uses the same dash-optional matching as switch names.
        assert_eq!(
            plan(&["-sort", "namedescending"]).sort,
            SortSpec {
                key: SortKey::Name,
                dir: SortDir::Desc,
            }
        );
        assert_eq!(
            plan(&["-sort", "datemodified"]).sort,
            SortSpec {
                key: SortKey::DateModified,
                dir: SortDir::Desc,
            }
        );
    }

    #[test]
    fn sort_param_unknown_key_is_exit_4() {
        let err = fatal(&["-sort", "bogus"]);
        assert_eq!(err.exit_code, 4);
        assert!(
            err.message.contains("bogus"),
            "message was: {}",
            err.message
        );
    }

    #[test]
    fn combined_sort_switches() {
        assert_eq!(
            plan(&["-sort-name"]).sort,
            SortSpec::default_for(SortKey::Name)
        );
        assert_eq!(
            plan(&["-sort-size"]).sort,
            SortSpec {
                key: SortKey::Size,
                dir: SortDir::Desc,
            }
        );
        assert_eq!(
            plan(&["-sort-dm"]).sort,
            SortSpec {
                key: SortKey::DateModified,
                dir: SortDir::Desc,
            }
        );
        assert_eq!(
            plan(&["-sort-date-modified"]).sort,
            SortSpec {
                key: SortKey::DateModified,
                dir: SortDir::Desc,
            }
        );
        assert_eq!(
            plan(&["-sort-dm-descending"]).sort,
            SortSpec {
                key: SortKey::DateModified,
                dir: SortDir::Desc,
            }
        );
        assert_eq!(
            plan(&["-sort-size-ascending"]).sort,
            SortSpec {
                key: SortKey::Size,
                dir: SortDir::Asc,
            }
        );
    }

    #[test]
    fn last_sort_switch_wins() {
        assert_eq!(
            plan(&["-sort-path", "-sort", "size"]).sort,
            SortSpec {
                key: SortKey::Size,
                dir: SortDir::Desc,
            }
        );
        assert_eq!(
            plan(&["-sort", "size", "-sortpath"]).sort,
            SortSpec {
                key: SortKey::Path,
                dir: SortDir::Asc,
            }
        );
    }

    #[test]
    fn query_tokens_join_with_single_spaces_around_interleaved_flags() {
        let parsed = plan(&["foo", "-size", "bar", "-n", "5", "baz qux"]);
        assert_eq!(parsed.query, "foo bar baz qux");
        assert_eq!(parsed.limit, Some(5));
        assert_eq!(parsed.columns, vec![EsColumn::Size]);
    }

    #[test]
    fn columns_dedupe_and_reorder_like_es() {
        assert_eq!(plan(&["-size", "-size"]).columns, vec![EsColumn::Size]);
        // es removes the existing instance then appends: last occurrence
        // determines position.
        assert_eq!(
            plan(&["-size", "-dm", "-size"]).columns,
            vec![EsColumn::DateModified, EsColumn::Size]
        );
    }

    #[test]
    fn native_namespace_flags() {
        assert!(plan(&["--json"]).json);
        assert!(plan(&["--status"]).status);
        assert!(plan(&["--insecure-no-server-check"]).insecure_no_server_check);
        assert!(plan(&["--server-version"]).server_version);
        let all = plan(&["--json", "--status"]);
        assert!(all.json && all.status);
    }

    #[test]
    fn help_and_version_switches() {
        for quick in ["-h", "/h", "-?", "/?"] {
            assert_eq!(
                parse(&[quick]),
                Ok(Command::Help(HelpScreen::Quick)),
                "{quick}"
            );
        }
        for full in ["-help", "/help", "--help"] {
            assert_eq!(
                parse(&[full]),
                Ok(Command::Help(HelpScreen::Full)),
                "{full}"
            );
        }
        for version in ["-v", "/v", "-version", "/version", "--version"] {
            assert_eq!(parse(&[version]), Ok(Command::Version), "{version}");
        }
        // Names stay case-sensitive, as every other switch.
        assert_eq!(fatal(&["-H"]).exit_code, 6);
        assert_eq!(fatal(&["--Help"]).exit_code, 6);
        // A second dash is allowed on any es name.
        assert_eq!(parse(&["--h"]), Ok(Command::Help(HelpScreen::Quick)));
        assert_eq!(parse(&["--v"]), Ok(Command::Version));
    }

    /// As in es, the help and version switches end parsing where they
    /// appear: a bad token before them still fails, one after them is never
    /// read.
    #[test]
    fn help_and_version_end_parsing_where_they_appear() {
        assert_eq!(
            parse(&["report", "-help", "-bogus"]),
            Ok(Command::Help(HelpScreen::Full))
        );
        assert_eq!(parse(&["/?", "-n"]), Ok(Command::Help(HelpScreen::Quick)));
        assert_eq!(parse(&["-version", "-path"]), Ok(Command::Version));
        assert_eq!(fatal(&["-bogus", "-h"]).exit_code, 6);
        // A parameter-taking switch still consumes its parameter first.
        assert_eq!(fatal(&["-n", "-h"]).exit_code, 4);
        assert_eq!(plan(&["-path", "-h"]).scope.as_deref(), Some("-h"));
    }

    /// Every spelling of a row resolves to that row's own action, so no
    /// earlier row shadows a later one.
    #[test]
    fn every_spelling_resolves_to_its_own_row() {
        for switch in SWITCHES {
            let Spelling::Names { es, native, action } = switch.spelling else {
                continue;
            };
            for name in es {
                assert_eq!(match_switch(name), Some(action), "-{name}");
            }
            for name in native {
                assert_eq!(match_native(name), Some(action), "--{name}");
            }
        }
    }

    #[test]
    fn native_namespace_unknown_is_exit_6() {
        let err = fatal(&["--bogus"]);
        assert_eq!(err.exit_code, 6);
        assert!(
            err.message.contains("--bogus"),
            "message was: {}",
            err.message
        );
        // Native names are exact: no dash-optionality in the -- namespace.
        assert_eq!(fatal(&["--insecureno-server-check"]).exit_code, 6);
    }

    /// `-` alone is search text. `--` ends the switches, and is not itself
    /// search text.
    #[test]
    fn two_dashes_end_the_switches() {
        assert_eq!(plan(&["-"]).query, "-");
        assert_eq!(plan(&["a", "--", "-n", "/ad", "b"]).query, "a -n /ad b");
        assert_eq!(plan(&["---", "-n"]).query, "-n");
        assert_eq!(plan(&["-n", "5", "--"]).limit, Some(5));
        assert_eq!(plan(&["--"]).query, "");
    }

    #[test]
    fn utf8_bom_switches() {
        assert!(plan(&["-utf8-bom"]).wants_bom());
        assert!(!plan(&["-no-utf8-bom", "-export-csv", "f"]).wants_bom());
        assert!(
            plan(&["-no-utf8-bom", "-utf8-bom"]).wants_bom(),
            "last flag wins"
        );
        // By default a CSV export has one, and nothing else does.
        assert!(plan(&["-export-csv", "f"]).wants_bom());
        assert!(!plan(&["-csv"]).wants_bom());
        assert!(!plan(&["-export-txt", "f"]).wants_bom());
    }

    #[test]
    fn header_and_double_quote_suppression() {
        assert!(!plan(&["-no-header"]).wants_header());
        assert!(!plan(&["-no-double-quote"]).double_quote);
        assert!(plan(&["-no-header", "-header"]).wants_header());
        assert!(plan(&["-no-header", "-default-header"]).wants_header());
        // -txt has none unless asked.
        assert!(!plan(&["-txt"]).wants_header());
        assert!(plan(&["-txt", "-header"]).wants_header());
        let quoted = plan(&["-double-quote"]);
        assert!(quoted.double_quote && quoted.text_double_quote);
    }

    #[test]
    fn match_case_aliases() {
        for spelling in ["-case", "-match-case", "-matchcase", "-i", "/i"] {
            assert!(plan(&[spelling]).match_case, "{spelling}");
        }
        for off in ["-no-case", "-no-i", "/noi"] {
            assert!(!plan(&["-i", off]).match_case, "{off}");
        }
    }

    /// `-p` applies to the whole search, wherever it stands, by a directive
    /// a gozd too old to know it refuses rather than ignores.
    #[test]
    fn match_path_is_a_directive_on_the_query() {
        assert_eq!(plan(&["-p", "a", "b"]).query, "matchpath: a b");
        assert_eq!(plan(&["a", "/match-path"]).query, "matchpath: a");
        assert_eq!(plan(&["-p", "-no-p", "a"]).query, "a");
    }

    #[test]
    fn es_filters_join_the_query() {
        assert_eq!(plan(&["/ad", "a"]).query, "folder: a");
        assert_eq!(plan(&["a", "-a-d"]).query, "file: a");
        assert_eq!(plan(&["-ad"]).query, "folder:");
        assert_eq!(plan(&["-p", "-ad", "a"]).query, "folder: matchpath: a");
    }

    /// The switches' terms go before the search text, where its operators,
    /// an open quote or a `name*:` cannot take them in.
    #[test]
    fn es_filters_stand_apart_from_the_search_texts_syntax() {
        use crate::query::{Kind, parse_query};
        let parsed = |tokens: &[&str]| parse_query(&plan(tokens).query).unwrap();
        // `foo|` is everything, so with -ad every folder: the filter is
        // ANDed, not ORed with `foo`.
        let all = parsed(&["-ad", "foo|"]);
        assert_eq!(all.filters.kind, Some(Kind::Folder));
        assert!(all.is_text_empty(), "{all:?}");
        // `name*:` takes the rest of the text, and the filter is not in it.
        let rest = parsed(&["name*:foo", "/a-d"]);
        assert_eq!(rest.filters.kind, Some(Kind::File));
        assert_eq!(rest.name_terms, [b"foo".to_vec()]);
        // An open quote ends with the text.
        let open = parsed(&["\"foo", "-ad"]);
        assert_eq!(open.filters.kind, Some(Kind::Folder));
        // An OR stays inside the text: (a|b) AND folders.
        let or = parsed(&["a|b", "-ad"]);
        assert_eq!(or.filters.kind, Some(Kind::Folder));
        assert!(or.expr.is_some());
    }

    /// The search text is Everything 1.5's: `|`, `!`, `< >` and the keyword
    /// operators reach gozd as typed, from split arguments, from a raw
    /// command line and through `-argv` alike.
    #[test]
    fn operators_pass_through_as_typed() {
        for text in [
            "a|b",
            "a | b",
            "!a",
            "a !b",
            "<a b>|c",
            "!<a|b> c",
            "a or: b not: c and: d",
            "*.dll|*.txt",
            r"**\bin\*.dll",
            "ext:txt;md|size:>1mb",
            "foo|",
            "<>",
        ] {
            assert_eq!(plan(&[text]).query, text, "split: {text}");
            assert_eq!(line(&format!("goz {text}")).query, text, "raw: {text}");
            assert_eq!(
                line(&format!("goz -argv \"{text}\"")).query,
                text,
                "-argv: {text}"
            );
        }
        // In es's reading the quotes reach the parser, so a quoted operator
        // is literal text, and a quoted value is still a value.
        assert_eq!(line(r#"goz "a|b""#).query, r#""a|b""#);
        assert_eq!(line(r#"goz size:">1mb""#).query, r#"size:">1mb""#);
    }

    /// Search text that is malformed whatever the clock exits 4 with the
    /// parser's message, its column counted in the text as typed.
    #[test]
    fn malformed_search_text_is_exit_4_with_its_column() {
        let refused = |tokens: &[&str]| {
            let err = fatal(tokens);
            assert_eq!(err.exit_code, Exit::BadParam.code(), "{tokens:?}");
            err.message
        };
        let msg = refused(&["<a", "b"]);
        assert!(msg.contains("unbalanced '<' at column 1"), "{msg}");
        let msg = refused(&["-ad", "a", "b>"]);
        assert!(msg.contains("unbalanced '>' at column 4"), "{msg}");
        let msg = refused(&["gr<a|e>y"]);
        assert!(msg.contains("group expansion"), "{msg}");
        let msg = refused(&["date-run:today"]);
        assert!(
            msg.contains("unknown search function or modifier 'date-run:'"),
            "{msg}"
        );
        // Content terms are the client's to search, three at most.
        assert_eq!(plan(&["content:foo"]).query, "content:foo");
        let msg = refused(&["content:a|content:b|content:c|content:d"]);
        assert!(msg.contains("4 content: terms"), "{msg}");
        let msg = refused(&["regex:(?<=a)b"]);
        assert!(msg.contains("look-around"), "{msg}");
        let msg = refused(&["-r", "a(b"]);
        assert!(msg.contains("unclosed group"), "{msg}");
        let msg = refused(&["size:huge2"]);
        assert!(msg.contains("size:"), "{msg}");
        // What the index cannot answer exactly, and a substitution (W8).
        let msg = refused(&["child-size:>1gb"]);
        assert!(
            msg.contains("child-size: is not supported") && msg.contains("folder sizes"),
            "{msg}"
        );
        let msg = refused(&["*.txt", "!sibling:$stem:.doc"]);
        assert!(msg.contains("the substitution $stem:"), "{msg}");
        assert_eq!(
            parse_command_line("goz -argv \"<a b\"")
                .unwrap_err()
                .exit_code,
            4
        );
        // es's reading keeps the quotes, so a quoted `<` is literal text.
        assert_eq!(line(r#"goz "<a b""#).query, r#""<a b""#);
        // A date is gozd's to judge, against its own clock.
        assert_eq!(plan(&["dm:today|dm:2024-05"]).query, "dm:today|dm:2024-05");
        assert_eq!(plan(&["dm:notadate"]).query, "dm:notadate");
    }

    #[test]
    fn es_sort_shorthands_name_key_and_direction() {
        let spec = |key, dir| SortSpec { key, dir };
        for (switch, want) in [
            ("/s", spec(SortKey::Path, SortDir::Asc)),
            ("/on", spec(SortKey::Name, SortDir::Asc)),
            ("/o-n", spec(SortKey::Name, SortDir::Desc)),
            ("/os", spec(SortKey::Size, SortDir::Asc)),
            ("/o-s", spec(SortKey::Size, SortDir::Desc)),
            ("/od", spec(SortKey::DateModified, SortDir::Asc)),
            ("/o-d", spec(SortKey::DateModified, SortDir::Desc)),
            ("/oe", spec(SortKey::Extension, SortDir::Asc)),
            ("/o-e", spec(SortKey::Extension, SortDir::Desc)),
            ("-oe", spec(SortKey::Extension, SortDir::Asc)),
            ("-sort-extension", spec(SortKey::Extension, SortDir::Asc)),
            (
                "-sort-ext-descending",
                spec(SortKey::Extension, SortDir::Desc),
            ),
        ] {
            assert_eq!(plan(&[switch]).sort, want, "{switch}");
        }
    }

    /// The direction stands apart from the key, whichever comes first.
    #[test]
    fn a_sort_direction_outlives_a_later_key() {
        let asc_size = SortSpec {
            key: SortKey::Size,
            dir: SortDir::Asc,
        };
        assert_eq!(plan(&["-sort", "size", "-sort-ascending"]).sort, asc_size);
        assert_eq!(plan(&["-sort-ascending", "-sort", "size"]).sort, asc_size);
        assert_eq!(plan(&["-sort-dm-ascending", "-sort-size"]).sort, asc_size);
        assert_eq!(
            plan(&["-sort-descending"]).sort,
            SortSpec {
                key: SortKey::Name,
                dir: SortDir::Desc,
            }
        );
        assert_eq!(plan(&["-sort-ascending", "-o-s"]).sort.dir, SortDir::Desc);
    }

    #[test]
    fn counts_take_every_es_spelling_and_hexadecimal() {
        for spelling in ["-n", "-count", "-max-results", "-viewport-count"] {
            assert_eq!(plan(&[spelling, "7"]).limit, Some(7), "{spelling}");
        }
        for spelling in ["-o", "-offset", "-viewport-offset", "/o"] {
            assert_eq!(plan(&[spelling, "7"]).offset, 7, "{spelling}");
        }
        assert_eq!(plan(&["-n", "0x10"]).limit, Some(16));
        assert_eq!(plan(&["-o", "0X1f"]).offset, 31);
        assert_eq!(fatal(&["-n", "0xzz"]).exit_code, 4);
        assert_eq!(fatal(&["-o"]).exit_code, 4);
    }

    #[test]
    fn export_formats_and_their_files() {
        for (flag, format) in [
            ("-csv", OutputFormat::Csv),
            ("-tsv", OutputFormat::Tsv),
            ("-efu", OutputFormat::Efu),
            ("-txt", OutputFormat::Txt),
            ("-m3u", OutputFormat::M3u),
            ("-m3u8", OutputFormat::M3u8),
            ("-json", OutputFormat::Json),
        ] {
            let printed = plan(&[flag]);
            assert_eq!(printed.format, Some(format), "{flag}");
            assert_eq!(printed.export_file, None);
            assert!(!printed.json, "-json is es's array, not goz's object");
            let export = format!("-export{flag}");
            let written = plan(&[&export, "f"]);
            assert_eq!(written.format, Some(format), "{export}");
            assert_eq!(written.export_file.as_deref(), Some("f"));
        }
        assert!(plan(&["--json"]).json);
        assert_eq!(plan(&["--json"]).format, None);
        // The last format wins, and the file stays.
        let both = plan(&["-export-txt", "f", "-csv"]);
        assert_eq!(both.format, Some(OutputFormat::Csv));
        assert_eq!(both.export_file.as_deref(), Some("f"));
    }

    #[test]
    fn no_result_error_spellings() {
        for spelling in [
            "-no-result-error",
            "-no-results-error",
            "-error-on-no-results",
        ] {
            assert!(plan(&[spelling]).no_result_error, "{spelling}");
        }
    }

    #[test]
    fn q_adds_search_text_that_looks_like_a_switch() {
        assert_eq!(plan(&["-q", "-exit", "a"]).query, "-exit a");
        assert_eq!(plan(&["/search", "-n"]).query, "-n");
        assert_eq!(plan(&["a", "-q*", "-n", "/ad"]).query, "a -n /ad");
        assert_eq!(plan(&["-s*"]).query, "");
        assert_eq!(fatal(&["-q"]).exit_code, 4);
    }

    /// es has these and goz does not, so a `/` form must not become search
    /// text: `/w` would search for the text `/w` and find something else.
    #[test]
    fn es_switches_goz_lacks_are_refused_in_either_form() {
        for name in ["lf", "name", "punc", "ws", "no-punc", "white-space"] {
            for token in [format!("-{name}"), format!("/{name}"), format!("--{name}")] {
                let err = fatal(&[&token]);
                assert_eq!(err.exit_code, 6, "{token}");
                assert!(err.message.contains("does not support"), "{}", err.message);
            }
        }
        // An attribute filter with a letter es lacks.
        for token in ["/ab", "-a-", "/ah-"] {
            assert_eq!(fatal(&[token]).exit_code, 6, "{token}");
        }
        // Dashes stay optional in them.
        assert_eq!(fatal(&["/nopunctuation"]).exit_code, 6);
        // A slash on anything else is still search text.
        assert_eq!(plan(&["/"]).query, "/");
        assert_eq!(plan(&["/downloads"]).query, "/downloads");
    }

    /// es's attribute filters become `attrib:` terms before the search
    /// text; the columns and sorts of the optional columns parse; and
    /// `--config` reads `get`, or `set` and its `KEY=0|1` pairs.
    #[test]
    fn attribute_filters_columns_sorts_and_config() {
        assert_eq!(plan(&["/ah"]).query, "attrib:H");
        assert_eq!(plan(&["/a-h"]).query, "!attrib:H");
        assert_eq!(plan(&["-arh-s", "x"]).query, "attrib:RH !attrib:S x");
        assert_eq!(plan(&["/av", "/a-v"]).query, "attrib:0x40 !attrib:0x40");
        assert_eq!(plan(&["/ad"]).query, "folder:", "a switch of its own");
        assert_eq!(attribute_terms("hh"), Some("attrib:H".to_string()));
        assert_eq!(attribute_terms(""), None);
        assert_eq!(
            plan(&["-dc", "-da", "-attrib", "-rc", "-dc"]).columns,
            vec![
                EsColumn::DateAccessed,
                EsColumn::Attributes,
                EsColumn::DateRecentlyChanged,
                EsColumn::DateCreated,
            ]
        );
        let spec = |key, dir| SortSpec { key, dir };
        for (args, want) in [
            (
                &["-sort", "date-created"][..],
                spec(SortKey::DateCreated, SortDir::Desc),
            ),
            (&["-sort", "dc"], spec(SortKey::DateCreated, SortDir::Desc)),
            (
                &["-sort-date-accessed-ascending"],
                spec(SortKey::DateAccessed, SortDir::Asc),
            ),
            (
                &["-sort", "attributes"],
                spec(SortKey::Attributes, SortDir::Asc),
            ),
            (
                &["-sort-rc-descending"],
                spec(SortKey::DateRecentlyChanged, SortDir::Desc),
            ),
        ] {
            assert_eq!(plan(args).sort, want, "{args:?}");
        }
        assert_eq!(plan(&["--config", "get"]).config, Some(ConfigCommand::Get));
        let Some(ConfigCommand::Set(change)) = plan(&[
            "--config",
            "set",
            "index_attributes=1",
            "fast_size_sort=off",
        ])
        .config
        else {
            panic!("expected a set");
        };
        assert_eq!(change.index_attributes, Some(true));
        assert_eq!(change.fast_size_sort, Some(false));
        assert_eq!(change.index_date_created, None);
        for bad in [
            &["--config", "set"][..],
            &["--config", "set", "bogus=1"],
            &["--config", "set", "index_attributes=2"],
            &["--config", "toggle"],
            &["--config"],
        ] {
            assert_eq!(fatal(bad).exit_code, 4, "{bad:?}");
        }
    }

    /// The search options es sets for the whole search become the
    /// modifiers that do so, before the search text; `-r` reads the whole
    /// search text as one regular expression; `-parent` and `-parent-path`
    /// keep their paths for the CLI to make absolute.
    #[test]
    fn es_search_options_map_to_the_search_syntax() {
        assert_eq!(plan(&["-ww", "foo"]).query, "?ww: foo");
        assert_eq!(plan(&["/w", "foo"]).query, "?ww: foo");
        assert_eq!(plan(&["-whole-words", "-no-ww", "foo"]).query, "foo");
        assert_eq!(
            plan(&["-ad", "-p", "-a", "-prefix", "x"]).query,
            "folder: matchpath: ?diacritics: ?prefix: x"
        );
        assert_eq!(plan(&["-diacritics", "-no-a"]).query, "");
        assert_eq!(
            plan(&["-suffix", "-no-suffix", "-suffix", "y"]).query,
            "?suffix: y"
        );
        assert_eq!(
            plan(&["-ignore-punctuation", "-ignorews", "a"]).query,
            "?nopunc: ?nows: a"
        );
        assert_eq!(plan(&["-no-ignore-punc", "a"]).query, "a");
        // `-r`: es's regex mode, where the whole search text is the pattern.
        assert_eq!(plan(&["-r", "^foo.*bar$"]).query, "regex*:^foo.*bar$");
        assert_eq!(plan(&["foo", "-regex", "b|c"]).query, "regex*:foo b|c");
        assert_eq!(plan(&["-ad", "-r", "x y"]).query, "folder: regex*:x y");
        assert_eq!(fatal(&["-r"]).exit_code, 4, "-r needs its pattern");
        let p = plan(&["-parent", r"C:\Program Files", "x"]);
        assert_eq!(p.parent.as_deref(), Some(r"C:\Program Files"));
        assert_eq!(
            p.query_with_parent(r"C:\Program Files"),
            r#"parent:"C:\Program Files" x"#
        );
        assert_eq!(
            plan(&["-parent", r"C:\"]).query_with_parent(r"C:\"),
            r#"parent:"C:\""#
        );
        crate::query::parse_query(&p.query_with_parent(r"C:\a;b c")).unwrap();
        // After the switches' terms, which reach it: `-a -parent` names the
        // folder with its accents.
        assert_eq!(
            plan(&["-a", "-ad", "-parent", r"C:\café", "x y"]).query_with_parent(r"C:\café"),
            r#"folder: ?diacritics: parent:"C:\café" x y"#
        );
        assert_eq!(
            plan(&["-a", "-parent", r"C:\café"]).query_with_parent(r"C:\café"),
            r#"?diacritics: parent:"C:\café""#
        );
        assert_eq!(
            plan(&["-r", "^a b", "-parent", r"C:\d"]).query_with_parent(r"C:\d"),
            r#"parent:"C:\d" regex*:^a b"#
        );
        let p = plan(&["-parent-path", r"sub\a.txt"]);
        assert_eq!(
            (p.scope.as_deref(), p.scope_is_parent_of),
            (Some(r"sub\a.txt"), true)
        );
        let p = plan(&["-parent-path", "a.txt", "-path", "d"]);
        assert_eq!(
            (p.scope.as_deref(), p.scope_is_parent_of),
            (Some("d"), false)
        );
    }

    /// No name is both accepted and refused.
    #[test]
    fn the_es_only_table_holds_nothing_goz_accepts() {
        for name in ES_ONLY_SWITCHES {
            assert_eq!(match_switch(name), None, "{name}");
        }
    }

    #[test]
    fn a_dash_switch_takes_its_value_after_an_equals_sign() {
        assert_eq!(plan(&["-n=5"]).limit, Some(5));
        assert_eq!(plan(&["--n=5"]).limit, Some(5));
        assert_eq!(plan(&[r"-path=C:\a=b"]).scope.as_deref(), Some(r"C:\a=b"));
        assert_eq!(fatal(&["-size=5"]).exit_code, 6, "takes no value");
        assert_eq!(fatal(&["-size=5", "a"]).exit_code, 6, "takes no value");
        assert_eq!(plan(&["/n=5"]).query, "/n=5", "only a dash splits");
    }

    fn line(command_line: &str) -> EsPlan {
        match parse_command_line(command_line) {
            Ok(Command::Run(plan)) => plan,
            other => panic!("expected a search, got {other:?}"),
        }
    }

    /// Quotes stay in search text, so they reach the query parser as a
    /// phrase, and a quoted switch is search text.
    #[test]
    fn a_raw_command_line_keeps_the_quotes_of_search_text() {
        assert_eq!(line(r#"goz "foo bar""#).query, r#""foo bar""#);
        assert_eq!(line(r#"goz a "b c" d"#).query, r#"a "b c" d"#);
        assert_eq!(line(r#"goz ab"c d"e"#).query, r#"ab"c d"e"#);
        assert_eq!(line(r#"goz "-n""#).query, r#""-n""#);
        assert_eq!(line(r#"goz """x"#).query, "&quot:x");
        assert_eq!(line(r#""C:\Program Files\goz.exe" a"#).query, "a");
        assert_eq!(line(r#"goz """#).query, r#""""#);
        assert_eq!(line("goz").query, "");
        assert_eq!(line("goz   ").query, "");
        assert_eq!(line("").query, "");
    }

    /// A parameter loses its quotes, and a backslash is ordinary, which
    /// `CommandLineToArgvW` gets wrong for a folder ending in one.
    #[test]
    fn a_raw_command_line_unquotes_parameters_only() {
        let parsed = line(r#"goz -path "C:\my dir\" -n 5 x"#);
        assert_eq!(parsed.scope.as_deref(), Some(r"C:\my dir\"));
        assert_eq!(parsed.limit, Some(5));
        assert_eq!(parsed.query, "x");
        assert_eq!(
            line(r#"goz -path C:\a"b c"d"#).scope.as_deref(),
            Some(r"C:\ab cd")
        );
        assert_eq!(line(r#"goz -q """a""""#).query, r#""a""#);
    }

    #[test]
    fn a_raw_command_line_splits_at_an_equals_sign_before_any_quote() {
        assert_eq!(line("goz -n=5").limit, Some(5));
        assert_eq!(
            line(r#"goz -path="C:\a b" x"#).scope.as_deref(),
            Some(r"C:\a b")
        );
        assert_eq!(line("goz -path= x").scope.as_deref(), Some(""));
        assert_eq!(line("goz -n=5 x").query, "x");
        // After a quote, or without a leading dash, it is ordinary text.
        assert_eq!(line(r#"goz a=b"#).query, "a=b");
        assert_eq!(line(r#"goz "-a"=b"#).query, r#""-a"=b"#);
        let err = parse_command_line("goz -size=5 x").unwrap_err();
        assert_eq!(err.exit_code, 6);
        assert!(err.message.contains("-size="), "{}", err.message);
        assert_eq!(parse_command_line("goz -n").unwrap_err().exit_code, 4);
        assert_eq!(parse_command_line("goz -n   ").unwrap_err().exit_code, 4);
    }

    /// `-argv` reads what follows as most programs read their arguments,
    /// so a caller that quotes one argument per word group, as Node does,
    /// gets its words back.
    #[test]
    fn argv_mode_removes_quotes_from_later_search_text() {
        assert_eq!(line(r#"goz -argv "a b" c"#).query, "a b c");
        assert_eq!(line(r#"goz "a b" -argv "c d""#).query, r#""a b" c d"#);
        assert_eq!(
            line(r#"goz -argv "a b" -no-argv "c d""#).query,
            r#"a b "c d""#
        );
        assert_eq!(line(r#"goz -argv "\"-exit\"""#).query, r#""-exit""#);
        assert_eq!(line(r#"goz -argv "a\\b""#).query, r"a\b");
        // A parameter reads the same in both.
        assert_eq!(
            line(r#"goz -argv -path "C:\my dir\\" x"#).scope.as_deref(),
            Some(r"C:\my dir\\")
        );
    }

    #[test]
    fn q_star_keeps_the_rest_of_a_raw_command_line_as_it_is() {
        assert_eq!(line(r#"goz a -q* "b  c" -n 5"#).query, r#"a "b  c" -n 5"#);
        assert_eq!(line(r#"goz -- "b c" -n"#).query, r#""b c" -n"#);
        assert_eq!(line("goz -q*").query, "");
    }

    proptest! {
        /// The raw reader is total as well, and agrees with itself on any
        /// text cut anywhere.
        #[test]
        fn parse_command_line_never_panics(line in r#"[ a-z"=\\/*?-]{0,40}|.{0,40}"#) {
            let _ = parse_command_line(&line);
        }
    }

    #[test]
    fn scope_is_taken_verbatim() {
        // Absolutization is the CLI's job; the core keeps the raw value.
        assert_eq!(
            plan(&["-path", "rel\\dir"]).scope,
            Some("rel\\dir".to_string())
        );
    }

    #[test]
    fn fatal_display_is_the_message() {
        let err = fatal(&["-zzz"]);
        assert_eq!(format!("{err}"), err.message);
    }
}
