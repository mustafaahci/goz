//! es.exe-compatible argv parsing: `parse_argv(args) -> Result<Command, EsFatal>`.
//!
//! Rules implemented from the voidtools es source:
//!
//! - A token starting with `-` or `/` is a switch candidate. Name matching is
//!   case-sensitive and every dash inside the canonical name is
//!   optional in the input (`-sort-path` == `-sortpath`, `-max-results` ==
//!   `-maxresults`). An input dash is accepted only where the canonical name
//!   has one, so `-d-m` does not match `-dm`.
//! - An unknown `-` switch is fatal (exit 6). An unknown `/` token falls
//!   through to the search text verbatim (es source comment: "allow
//!   /downloads to search for \downloads").
//! - A token consisting solely of dashes (`-`, `--`, `---`, ...) is search
//!   text, never a switch (es `es_is_literal_switch`).
//! - Other `--`-prefixed tokens are goz's native namespace, matched exactly
//!   (case-sensitive, dashes required). Anything else is fatal (exit 6).
//! - A parameter-taking switch consumes the next token; a missing or
//!   unparseable parameter is fatal (exit 4).
//! - The help and version switches end parsing where they appear, as in es:
//!   a token before them can still fail, and the tokens after them are not
//!   read. Like every other switch they take `/` too, so `/?`, `/h` and `/v`
//!   are switches, not search text.
//! - Non-switch tokens become the query: joined with single spaces, in
//!   command-line order, each token preserved as received. Flags may
//!   interleave with query tokens freely.
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

/// The fully-resolved plan for one es-compatible invocation.
///
/// Produced by [`parse_argv`]; consumed by the CLI, which resolves
/// process-level concerns (absolutizing `scope`, creating the export file,
/// talking to the daemon).
#[derive(Clone, Debug, PartialEq)]
pub struct EsPlan {
    /// Search text: every non-switch token, joined with single spaces in
    /// command-line order, each token preserved as received.
    pub query: String,
    /// `-path` value, verbatim. The CLI absolutizes it (GetFullPathName
    /// semantics, no existence check); the core stays path-API-free.
    pub scope: Option<String>,
    /// `-n` / `-max-results` value: return only the top N results after
    /// sorting. `None` means unlimited.
    pub limit: Option<u32>,
    /// Optional CSV columns from `-size` / `-dm` / `-date-modified`, in flag
    /// order with es dedupe semantics (a repeated flag moves its column to
    /// the end). `Filename` is always emitted last and is not listed here.
    pub columns: Vec<EsColumn>,
    /// Result order. Defaults to name-ascending (es with no `-sort` flag);
    /// unsuffixed sort switches use es's per-key default direction.
    pub sort: SortSpec,
    /// `-export-csv` value, verbatim. `None` means write results to stdout.
    pub export_csv: Option<String>,
    /// Whether the CSV starts with a UTF-8 BOM. Default true: a
    /// documented goz deviation from stock es (which defaults BOM off).
    /// `-no-utf8-bom` clears it; `-utf8-bom` is accepted and sets it.
    pub bom: bool,
    /// Whether the CSV includes the header row. `-no-header` clears it.
    pub header: bool,
    /// Whether string columns are always double-quoted (es default).
    /// `-no-double-quote` downgrades to quote-only-when-needed.
    pub double_quote: bool,
    /// Case-sensitive matching, from `-case` / `-match-case`. Default off.
    pub match_case: bool,
    /// Native `--json`: emit results as JSON instead of CSV/plain text.
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
    /// Native `--status`: report daemon/index status instead of searching.
    pub status: bool,
    /// Native `--server-version`: print the daemon's version instead of
    /// searching. Wins over `status`.
    pub server_version: bool,
    /// Native `--insecure-no-server-check`: skip the pipe-server owner-SID
    /// verification (dev escape hatch).
    pub insecure_no_server_check: bool,
}

impl Default for EsPlan {
    /// The plan for an empty command line: name-ascending sort and es's
    /// default output shape (header on, always-quote on, BOM on per goz's
    /// deviation).
    fn default() -> Self {
        Self {
            query: String::new(),
            scope: None,
            limit: None,
            columns: Vec::new(),
            sort: SortSpec::default(),
            export_csv: None,
            bom: true,
            header: true,
            double_quote: true,
            match_case: false,
            json: false,
            result_count: false,
            total_size: false,
            status: false,
            server_version: false,
            insecure_no_server_check: false,
        }
    }
}

/// A fatal argv-parse error carrying the es-compatible process exit code.
///
/// Exit-code mapping (from es.exe):
///
/// | Code | es meaning |
/// |------|------------|
/// | 4    | expected an additional parameter after a switch (modern es also uses it for a bad switch parameter) |
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
}

/// What a matched switch does to the plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SwitchAction {
    /// `-n` / `-max-results`: consume the next token as a `u32` limit.
    Limit,
    /// `-path`: consume the next token as the scope, verbatim.
    Scope,
    /// `-export-csv`: consume the next token as the output path, verbatim.
    ExportCsv,
    /// `-sort`: consume the next token as a sort-key name with an optional
    /// `-ascending` / `-descending` suffix.
    SortParam,
    /// `-size` / `-dm` / `-date-modified`: append a column (es dedupe).
    Column(EsColumn),
    /// A combined `-sort-<key>[-ascending|-descending]` switch.
    SortFixed(SortSpec),
    /// `-utf8-bom`: set BOM on (a no-op against goz's default).
    Utf8Bom,
    /// `-no-utf8-bom`: set BOM off.
    NoUtf8Bom,
    /// `-no-header`: suppress the CSV header row.
    NoHeader,
    /// `-no-double-quote`: quote string columns only when needed.
    NoDoubleQuote,
    /// `-case` / `-match-case`: case-sensitive matching.
    MatchCase,
    /// `--json`: emit results as JSON.
    Json,
    /// `-get-result-count`: print only the number of results.
    ResultCount,
    /// `-get-total-size`: print only the summed size of the matching files.
    TotalSize,
    /// `--status`: report the daemon's state instead of searching.
    Status,
    /// `--server-version`: print the daemon's version instead of searching.
    ServerVersion,
    /// `--insecure-no-server-check`: skip the pipe server's trust check.
    InsecureNoServerCheck,
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
            SwitchAction::Limit => Some("N"),
            SwitchAction::Scope => Some("DIR"),
            SwitchAction::ExportCsv => Some("FILE"),
            SwitchAction::SortParam => Some("KEY"),
            SwitchAction::Column(_)
            | SwitchAction::SortFixed(_)
            | SwitchAction::Utf8Bom
            | SwitchAction::NoUtf8Bom
            | SwitchAction::NoHeader
            | SwitchAction::NoDoubleQuote
            | SwitchAction::MatchCase
            | SwitchAction::Json
            | SwitchAction::ResultCount
            | SwitchAction::TotalSize
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

/// Every switch goz accepts, in the order the help lists them.
pub(super) const SWITCHES: &[Switch] = &[
    Switch {
        spelling: names(&["path"], &[], SwitchAction::Scope),
        group: Group::Search,
        quick: true,
        help: "Search only below DIR",
    },
    Switch {
        spelling: names(&["case", "match-case"], &[], SwitchAction::MatchCase),
        group: Group::Search,
        quick: true,
        help: "Match case",
    },
    Switch {
        spelling: names(&["n", "max-results"], &[], SwitchAction::Limit),
        group: Group::Search,
        quick: true,
        help: "Show at most N results",
    },
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
    Switch {
        spelling: names(&["export-csv"], &[], SwitchAction::ExportCsv),
        group: Group::Output,
        quick: true,
        help: "Write the results to FILE as CSV",
    },
    Switch {
        spelling: names(&["size"], &[], SwitchAction::Column(EsColumn::Size)),
        group: Group::Output,
        quick: false,
        help: "Add a Size column to the CSV",
    },
    Switch {
        spelling: names(
            &["dm", "date-modified"],
            &[],
            SwitchAction::Column(EsColumn::DateModified),
        ),
        group: Group::Output,
        quick: false,
        help: "Add a Date Modified column to the CSV",
    },
    Switch {
        spelling: names(&["utf8-bom"], &[], SwitchAction::Utf8Bom),
        group: Group::Output,
        quick: false,
        help: "Start the CSV with a byte order mark (the default)",
    },
    Switch {
        spelling: names(&["no-utf8-bom"], &[], SwitchAction::NoUtf8Bom),
        group: Group::Output,
        quick: false,
        help: "Leave out the byte order mark",
    },
    Switch {
        spelling: names(&["no-header"], &[], SwitchAction::NoHeader),
        group: Group::Output,
        quick: false,
        help: "Leave out the CSV header row",
    },
    Switch {
        spelling: names(&["no-double-quote"], &[], SwitchAction::NoDoubleQuote),
        group: Group::Output,
        quick: false,
        help: "Quote a CSV value only when it needs quotes",
    },
    Switch {
        spelling: names(&[], &["json"], SwitchAction::Json),
        group: Group::Output,
        quick: true,
        help: "Print the results as JSON",
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
];

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
    let mut plan = EsPlan::default();
    let mut query: Vec<&str> = Vec::new();
    let mut rest = args.iter();

    while let Some(token) = rest.next() {
        let token = token.as_str();

        // es literal-switch rule: all-dash tokens are search text.
        if !token.is_empty() && token.bytes().all(|b| b == b'-') {
            query.push(token);
            continue;
        }

        let action = if let Some(native) = token.strip_prefix("--") {
            // Native namespace: exact match only, no dash-optionality.
            match_native(native).ok_or_else(|| EsFatal::unknown_switch(token))?
        } else if let Some(name) = token.strip_prefix('-') {
            match_switch(name).ok_or_else(|| EsFatal::unknown_switch(token))?
        } else if let Some(name) = token.strip_prefix('/') {
            match match_switch(name) {
                Some(action) => action,
                // es: unknown `/` tokens search for the text verbatim.
                None => {
                    query.push(token);
                    continue;
                }
            }
        } else {
            query.push(token);
            continue;
        };

        match action {
            SwitchAction::Help(screen) => return Ok(Command::Help(screen)),
            SwitchAction::Version => return Ok(Command::Version),
            action => apply_switch(action, token, &mut rest, &mut plan)?,
        }
    }

    plan.query = query.join(" ");
    Ok(Command::Run(plan))
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
                    .map(SwitchAction::SortFixed)
            })
        }),
    })
}

/// Applies one matched switch, consuming its parameter from `rest` if it
/// takes one. `token` is the switch as the user typed it (for messages).
/// The help and version actions end parsing in [`parse_argv`] and never
/// reach here.
fn apply_switch(
    action: SwitchAction,
    token: &str,
    rest: &mut core::slice::Iter<'_, String>,
    plan: &mut EsPlan,
) -> Result<(), EsFatal> {
    match action {
        SwitchAction::Limit => {
            let value = take_param(rest, token)?;
            let limit: u32 = value
                .parse()
                .map_err(|_| EsFatal::bad_param(token, value, "a number"))?;
            plan.limit = Some(limit);
        }
        SwitchAction::Scope => plan.scope = Some(take_param(rest, token)?.to_owned()),
        SwitchAction::ExportCsv => plan.export_csv = Some(take_param(rest, token)?.to_owned()),
        SwitchAction::SortParam => {
            let value = take_param(rest, token)?;
            plan.sort = parse_sort_param(value)
                .ok_or_else(|| EsFatal::bad_param(token, value, "a sort key"))?;
        }
        SwitchAction::Column(column) => {
            // es `es_add_column`: remove an existing instance, then append,
            // so the last occurrence determines the column's position.
            plan.columns.retain(|existing| *existing != column);
            plan.columns.push(column);
        }
        SwitchAction::SortFixed(spec) => plan.sort = spec,
        SwitchAction::Utf8Bom => plan.bom = true,
        SwitchAction::NoUtf8Bom => plan.bom = false,
        SwitchAction::NoHeader => plan.header = false,
        SwitchAction::NoDoubleQuote => plan.double_quote = false,
        SwitchAction::MatchCase => plan.match_case = true,
        SwitchAction::Json => plan.json = true,
        SwitchAction::ResultCount => plan.result_count = true,
        SwitchAction::TotalSize => plan.total_size = true,
        SwitchAction::Status => plan.status = true,
        SwitchAction::ServerVersion => plan.server_version = true,
        SwitchAction::InsecureNoServerCheck => plan.insecure_no_server_check = true,
        // Handled by the caller; nothing to apply.
        SwitchAction::Help(_) | SwitchAction::Version => {}
    }
    Ok(())
}

/// Consumes the next token as a switch parameter, or fails with exit 4
/// naming the switch as the user typed it.
fn take_param<'a>(
    rest: &mut core::slice::Iter<'a, String>,
    token: &str,
) -> Result<&'a str, EsFatal> {
    rest.next()
        .map(String::as_str)
        .ok_or_else(|| EsFatal::missing_param(token))
}

/// Resolves a `-sort` parameter value (`name`, `dm`, `date-modified`, ...
/// optionally suffixed `-ascending` / `-descending`) into a [`SortSpec`].
/// Uses the same dash-optional matching as switch names.
fn parse_sort_param(value: &str) -> Option<SortSpec> {
    SORT_KEYS.iter().find_map(|row| {
        row.names
            .iter()
            .find_map(|alias| match_sort_form(value, alias, row.key))
    })
}

/// Matches `input` against `base`, `base-ascending`, or `base-descending`
/// (interior dashes optional). Unsuffixed forms take the key's es default
/// direction ([`SortSpec::default_for`]).
fn match_sort_form(input: &str, base: &str, key: SortKey) -> Option<SortSpec> {
    if switch_name_matches(input, base) {
        return Some(SortSpec::default_for(key));
    }
    if switch_name_matches(input, &format!("{base}-ascending")) {
        return Some(SortSpec {
            key,
            dir: SortDir::Asc,
        });
    }
    if switch_name_matches(input, &format!("{base}-descending")) {
        return Some(SortSpec {
            key,
            dir: SortDir::Desc,
        });
    }
    None
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
        assert!(parsed.bom);
        assert!(parsed.header);
        assert!(parsed.double_quote);
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
                export_csv: Some("C:\\tmp\\out.csv".to_string()),
                bom: true,
                header: true,
                double_quote: true,
                match_case: false,
                json: false,
                result_count: false,
                total_size: false,
                status: false,
                server_version: false,
                insecure_no_server_check: false,
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
        assert_eq!(fatal(&["--get-result-count"]).exit_code, 6);
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
        assert_eq!(fatal(&["--get-total-size"]).exit_code, 6);
    }

    #[test]
    fn interior_dashes_are_optional_everywhere() {
        assert_eq!(
            plan(&["-sortpath"]).sort,
            SortSpec::default_for(SortKey::Path)
        );
        assert_eq!(plan(&["-maxresults", "7"]).limit, Some(7));
        assert!(!plan(&["-noutf8bom"]).bom);
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
        assert!(!plan(&["-no-utf8bom"]).bom);
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
        // Names stay case-sensitive and exact, as every other switch.
        assert_eq!(fatal(&["-H"]).exit_code, 6);
        assert_eq!(fatal(&["--h"]).exit_code, 6);
        assert_eq!(fatal(&["--v"]).exit_code, 6);
        assert_eq!(fatal(&["--Help"]).exit_code, 6);
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

    #[test]
    fn all_dash_tokens_are_search_text() {
        assert_eq!(plan(&["-", "--", "---"]).query, "- -- ---");
    }

    #[test]
    fn utf8_bom_switches() {
        assert!(plan(&["-utf8-bom"]).bom, "accepted no-op vs goz default");
        assert!(!plan(&["-no-utf8-bom"]).bom);
        assert!(plan(&["-no-utf8-bom", "-utf8-bom"]).bom, "last flag wins");
    }

    #[test]
    fn header_and_double_quote_suppression() {
        assert!(!plan(&["-no-header"]).header);
        assert!(!plan(&["-no-double-quote"]).double_quote);
    }

    #[test]
    fn match_case_aliases() {
        assert!(plan(&["-case"]).match_case);
        assert!(plan(&["-match-case"]).match_case);
        assert!(plan(&["-matchcase"]).match_case);
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
