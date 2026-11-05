//! The quick help and the full reference.
//!
//! Every switch line is rendered from [`SWITCHES`], every sort key line from
//! [`SORT_KEYS`], and every exit code line from [`Exit::ALL`]: the rows the
//! parser and the CLI read, so a row added there is a line of the help. The
//! examples and the search text forms are written here, and the tests run
//! each one through the parsers.

use super::argv::{Group, SORT_KEYS, SWITCHES, SortKeyRow, Spelling, Switch};
use super::{Exit, HelpScreen};
use crate::types::{SortDir, SortSpec};

/// What `-version` prints: goz and its version.
pub const VERSION_LINE: &str = concat!("goz ", env!("CARGO_PKG_VERSION"));

/// The help screen `screen` names, ending in a newline.
pub fn help(screen: HelpScreen) -> String {
    let mut out = format!("{VERSION_LINE}: instant file search for Windows NTFS\n{USAGE}\n");
    let examples: Vec<Row> = EXAMPLES
        .iter()
        .map(|(usage, text)| Row::new((*usage).to_owned(), text))
        .collect();
    match screen {
        HelpScreen::Quick => {
            let common: Vec<Row> = SWITCHES
                .iter()
                .filter(|switch| switch.quick)
                .map(|switch| Row::new(first_usage(switch), switch.help))
                .collect();
            section(
                &mut out,
                "Examples",
                &examples,
                column(&[&examples], EXAMPLE_COLUMN),
            );
            section(
                &mut out,
                "Common switches",
                &common,
                column(&[&common], USAGE_COLUMN),
            );
            let keys: Vec<&str> = SORT_KEYS.iter().map(|row| row.names[0]).collect();
            out.push_str(&format!("\nSort keys: {}\n", keys.join(", ")));
            wrap(&mut out, "", QUICK_FOOTER, 0);
        }
        HelpScreen::Full => {
            out.push('\n');
            wrap(&mut out, "", INTRO, 0);
            let terms: Vec<Row> = SEARCH_TEXT
                .iter()
                .map(|(usage, text)| Row::new((*usage).to_owned(), text))
                .collect();
            let groups: Vec<(&str, Vec<Row>)> = [
                (Group::Search, "Search switches"),
                (Group::Output, "Output switches"),
                (Group::Other, "Other switches"),
            ]
            .into_iter()
            .map(|(group, title)| {
                let rows = SWITCHES
                    .iter()
                    .filter(|switch| switch.group == group)
                    .map(|switch| Row::new(all_usages(switch), switch.help))
                    .collect();
                (title, rows)
            })
            .collect();
            let keys: Vec<Row> = SORT_KEYS
                .iter()
                .map(|row| Row::new(row.names.join(", "), &sort_key_text(row)))
                .collect();
            let codes: Vec<Row> = Exit::ALL
                .iter()
                .map(|exit| Row::new(exit.code().to_string(), exit.meaning()))
                .collect();

            // One text column for every table but the examples, so the
            // reference reads as one table.
            let mut tables: Vec<&[Row]> = vec![&terms, &keys];
            tables.extend(groups.iter().map(|(_, rows)| rows.as_slice()));
            let shared = column(&tables, USAGE_COLUMN);
            section(
                &mut out,
                "Examples",
                &examples,
                column(&[&examples], EXAMPLE_COLUMN),
            );
            section(&mut out, "Search text", &terms, shared);
            for (title, rows) in &groups {
                section(&mut out, title, rows, shared);
            }
            section(&mut out, "Sort keys", &keys, shared);
            section(
                &mut out,
                "Exit codes",
                &codes,
                column(&[&codes], USAGE_COLUMN),
            );
        }
    }
    out
}

const USAGE: &str = "Usage: goz [switches] [search text]";

const INTRO: &str = "goz prints the full path of every indexed file and folder that \
     matches the search text, one per line, sorted by name. With no search text it \
     lists everything. A switch starts with a dash or a slash, and any dash inside \
     its name may be left out, so -sortpath is -sort-path. A second leading dash \
     changes nothing, except for goz's own switches such as --json, which must be \
     typed exactly. A value can follow an equals sign, as in -n=5. Two dashes alone \
     end the switches: the rest is search text. Quote search text that holds < or > \
     so the shell passes it on.";

const QUICK_FOOTER: &str = "Run goz --help for every switch, the search syntax and the exit codes.";

/// Complete command lines, as typed in cmd or PowerShell, with what each does.
const EXAMPLES: &[(&str, &str)] = &[
    ("goz report.pdf", "Find names that contain report.pdf"),
    (
        r"goz -path C:\Users\me invoice",
        r"Find invoice anywhere below C:\Users\me",
    ),
    (
        "goz -sort size -n 10 *.iso",
        "Show the 10 largest .iso files",
    ),
    (
        "goz -export-csv out.csv -size ext:cs",
        "Save the .cs files with sizes as CSV",
    ),
    ("goz --status", "Check whether every volume is ready"),
];

/// Search text as goz receives it.
const SEARCH_TEXT: &[(&str, &str)] = &[
    ("report", "Names that contain report"),
    ("report pdf", "Names that contain both words, in any order"),
    ("\"report pdf\"", "Names that contain that exact phrase"),
    ("*.cs", "Whole names that match a wildcard, * or ?"),
    (r"docs\api", r"Full paths that contain docs\api"),
    (r"C:\repo\", "Everything below that folder"),
    ("ext:cs;txt", "Extension is cs or txt"),
    (
        "size:>1mb",
        "Larger than 1 MB; also size:<=4kb, size:1kb..2mb or size:huge",
    ),
    (
        "dm:today",
        "Modified today; also dm:lastweek, dm:2024-05, dm:<30days, \
         dm:2024-01-01..2024-03-31 or dm:unknown",
    ),
    ("path:TEXT", "Full paths that contain TEXT"),
    (
        "file:, folder:, dir:",
        "Only files, or only folders; file:TEXT also needs TEXT in the name",
    ),
    ("case:", "Match case for this search"),
    (
        "matchpath:",
        "Match all search text against full paths, as -p does",
    ),
];

/// The widest usage that keeps its text beside it in a switch section; a
/// wider one puts its text on the next line.
const USAGE_COLUMN: usize = 24;
/// The same for the examples, whose command lines run longer.
const EXAMPLE_COLUMN: usize = 36;
/// The width the help wraps to, so it fits an 80 column console.
const LINE_WIDTH: usize = 79;

/// One help line: what to type, and what it does.
struct Row {
    usage: String,
    text: String,
}

impl Row {
    fn new(usage: String, text: &str) -> Self {
        Self {
            usage,
            text: text.to_owned(),
        }
    }
}

/// A switch's first spelling with its parameter, as the quick help lists it.
fn first_usage(switch: &Switch) -> String {
    usages(switch).swap_remove(0)
}

/// Every spelling of a switch, as the full reference lists it, such as
/// `-n N, -max-results N`.
fn all_usages(switch: &Switch) -> String {
    usages(switch).join(", ")
}

fn usages(switch: &Switch) -> Vec<String> {
    match switch.spelling {
        Spelling::Names { es, native, action } => {
            let param = action.param().map(|p| format!(" {p}")).unwrap_or_default();
            es.iter()
                .map(|name| format!("-{name}{param}"))
                .chain(native.iter().map(|name| format!("--{name}{param}")))
                .collect()
        }
        Spelling::SortFamily => vec!["-sort-KEY".to_owned()],
    }
}

/// A sort key's line: what it orders by, in the order it takes without a
/// suffix.
fn sort_key_text(row: &SortKeyRow) -> String {
    let order = match SortSpec::default_for(row.key).dir {
        SortDir::Asc => row.orders[0],
        SortDir::Desc => row.orders[1],
    };
    format!("{}, {order}", row.what)
}

/// The column where the text of `tables` starts: two spaces of indent, the
/// widest usage no wider than `max_usage`, and two spaces before the text.
fn column(tables: &[&[Row]], max_usage: usize) -> usize {
    let widest = tables
        .iter()
        .flat_map(|rows| rows.iter())
        .map(|row| row.usage.len())
        .filter(|&len| len <= max_usage)
        .max()
        .unwrap_or(0);
    2 + widest + 2
}

/// Writes a blank line, `title`, and `rows` as two columns: the usage, and
/// its text wrapped beside it from `column`. A usage too wide for that puts
/// its text on the next line.
fn section(out: &mut String, title: &str, rows: &[Row], column: usize) {
    out.push('\n');
    out.push_str(title);
    out.push('\n');
    for row in rows {
        let usage = format!("  {}", row.usage);
        if usage.len() + 2 <= column {
            wrap(out, &format!("{usage:<column$}"), &row.text, column);
        } else {
            out.push_str(&usage);
            out.push('\n');
            wrap(out, &" ".repeat(column), &row.text, column);
        }
    }
}

/// Appends `start`, then `text` wrapped at [`LINE_WIDTH`] with every later
/// line indented `indent` spaces, then a newline.
fn wrap(out: &mut String, start: &str, text: &str, indent: usize) {
    out.push_str(start);
    let mut column = start.len();
    let mut line_empty = true;
    for word in text.split(' ').filter(|word| !word.is_empty()) {
        if !line_empty && column + 1 + word.len() > LINE_WIDTH {
            out.push('\n');
            out.push_str(&" ".repeat(indent));
            column = indent;
            line_empty = true;
        }
        if !line_empty {
            out.push(' ');
            column += 1;
        }
        out.push_str(word);
        column += word.len();
        line_empty = false;
    }
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::escompat::{Command, parse_argv};
    use crate::query::{parse_query, parse_query_at};

    fn argv(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(ToString::to_string).collect()
    }

    /// The lines of the full reference's section `title`, without the title.
    fn section_lines(title: &str) -> Vec<String> {
        let full = help(HelpScreen::Full);
        let start = full
            .find(&format!("\n{title}\n"))
            .unwrap_or_else(|| panic!("the full help has a {title} section"));
        full[start + title.len() + 2..]
            .lines()
            .take_while(|line| !line.is_empty())
            .map(str::to_owned)
            .collect()
    }

    /// The usage column of a section: each row's text before its two space
    /// gap, or the whole line for a usage whose text sits below it.
    fn usages_in(title: &str) -> Vec<String> {
        section_lines(title)
            .iter()
            .filter(|line| line.starts_with("  ") && !line.starts_with("   "))
            .map(|line| line[2..].split("  ").next().unwrap_or_default().to_owned())
            .collect()
    }

    /// The sort key names the full reference lists.
    fn listed_sort_keys() -> Vec<String> {
        usages_in("Sort keys")
            .iter()
            .flat_map(|names| names.split(", ").map(str::to_owned).collect::<Vec<_>>())
            .collect()
    }

    /// Every concrete switch spelling the full reference lists, without
    /// parameters, with `KEY` in `-sort-KEY` expanded to each listed sort key
    /// and direction.
    fn listed_switches() -> Vec<String> {
        let keys = listed_sort_keys();
        let mut listed = Vec::new();
        for title in ["Search switches", "Output switches", "Other switches"] {
            for usage in usages_in(title) {
                for form in usage.split(", ") {
                    let name = form.split(' ').next().unwrap_or_default();
                    if name == "-sort-KEY" {
                        for key in &keys {
                            for suffix in ["", "-ascending", "-descending"] {
                                listed.push(format!("-sort-{key}{suffix}"));
                            }
                        }
                    } else {
                        listed.push(name.to_owned());
                    }
                }
            }
        }
        listed
    }

    /// Whether a query holds only what a switch adds to it, or nothing: no
    /// token was taken as search text. `text` is the sample for `-q TEXT`.
    fn holds_no_search_text(query: &str) -> bool {
        ["", "folder:", "file:", "matchpath:", "text"].contains(&query)
    }

    /// Whether `parse_argv` takes `token` alone as a switch rather than as
    /// search text or an unknown switch.
    fn parses_as_switch(token: &str) -> bool {
        match parse_argv(&argv(&[token])) {
            Ok(Command::Run(plan)) => holds_no_search_text(&plan.query),
            Ok(Command::Help(_) | Command::Version) => true,
            // Exit 4 here can only be a switch missing its parameter.
            Err(fatal) => fatal.exit_code == Exit::BadParam.code(),
        }
    }

    /// Whether the help lists the switch the parser took `token` as.
    fn is_listed(token: &str, listed: &[String]) -> bool {
        let strip = |s: &str| s.replace('-', "");
        if listed.iter().any(|form| form == token) {
            return true;
        }
        let name = token.strip_prefix("--").unwrap_or(&token[1..]);
        listed
            .iter()
            .filter(|form| !form.starts_with("--"))
            .any(|form| strip(&form[1..]) == strip(name))
    }

    /// Every switch the full reference lists parses as the switch it
    /// describes, with a sample value for its parameter, so the help cannot
    /// list a switch goz refuses or searches as text.
    #[test]
    fn every_listed_switch_parses() {
        let keys = listed_sort_keys();
        assert!(!keys.is_empty());
        for key in &keys {
            for suffix in ["", "-ascending", "-descending"] {
                let value = format!("{key}{suffix}");
                match parse_argv(&argv(&["-sort", &value])) {
                    Ok(Command::Run(plan)) => assert_eq!(plan.query, "", "-sort {value}"),
                    other => panic!("-sort {value}: {other:?}"),
                }
            }
        }
        for title in ["Search switches", "Output switches", "Other switches"] {
            for usage in usages_in(title) {
                for form in usage.split(", ") {
                    let mut tokens: Vec<&str> = form.split(' ').collect();
                    if let Some(param) = tokens.get_mut(1) {
                        *param = match *param {
                            "N" => "5",
                            "DIR" => r"C:\repo",
                            "FILE" => "out.csv",
                            "KEY" => "size",
                            "TEXT" => "text",
                            other => panic!("{form}: no sample value for {other}"),
                        };
                    }
                    if tokens[0] == "-sort-KEY" {
                        continue; // expanded by listed_switches below
                    }
                    match parse_argv(&argv(&tokens)) {
                        Ok(Command::Run(plan)) => {
                            assert!(holds_no_search_text(&plan.query), "{form}: {}", plan.query);
                        }
                        Ok(Command::Help(_) | Command::Version) => {}
                        Err(fatal) => panic!("{form}: {fatal}"),
                    }
                    // A listed parameter must be one the switch needs.
                    if tokens.len() == 2 {
                        let alone = parse_argv(&argv(&tokens[..1]));
                        assert!(alone.is_err(), "{form} parses without its parameter");
                    }
                }
            }
        }
        for form in listed_switches() {
            assert!(parses_as_switch(&form), "{form} is listed but not a switch");
        }
    }

    /// Every switch the parser accepts is listed. The parser reads only
    /// `SWITCHES` and `SORT_KEYS`, so walking their spellings covers its
    /// vocabulary; every short name is tried too, so a switch matched
    /// outside the tables cannot hide.
    #[test]
    fn every_accepted_switch_is_listed() {
        let listed = listed_switches();
        for switch in SWITCHES {
            let Spelling::Names { es, native, .. } = switch.spelling else {
                continue;
            };
            for name in es {
                for prefix in ["-", "/"] {
                    let token = format!("{prefix}{name}");
                    assert!(parses_as_switch(&token), "{token}");
                    assert!(is_listed(&token, &listed), "{token} is not in the help");
                }
            }
            for name in native {
                let token = format!("--{name}");
                assert!(parses_as_switch(&token), "{token}");
                assert!(is_listed(&token, &listed), "{token} is not in the help");
            }
        }
        for row in SORT_KEYS {
            for alias in row.names {
                for suffix in ["", "-ascending", "-descending"] {
                    let token = format!("-sort-{alias}{suffix}");
                    assert!(parses_as_switch(&token), "{token}");
                    assert!(is_listed(&token, &listed), "{token} is not in the help");
                }
            }
        }

        // Every name of up to three letters or `?`, under every prefix.
        let alphabet: Vec<char> = ('a'..='z').chain(['?']).collect();
        let mut names = vec![String::new()];
        for _ in 0..3 {
            names = names
                .iter()
                .flat_map(|name| alphabet.iter().map(move |c| format!("{name}{c}")))
                .collect();
            for name in &names {
                for prefix in ["-", "/", "--"] {
                    let token = format!("{prefix}{name}");
                    if parses_as_switch(&token) {
                        assert!(is_listed(&token, &listed), "{token} is not in the help");
                    }
                }
            }
        }
    }

    /// Each example is a command line goz accepts as the search it describes.
    #[test]
    fn every_example_parses() {
        for (example, _) in EXAMPLES {
            let tokens: Vec<&str> = example.split(' ').collect();
            assert_eq!(tokens[0], "goz", "{example}");
            match parse_argv(&argv(&tokens[1..])) {
                Ok(Command::Run(plan)) => {
                    assert!(
                        !plan.query.is_empty() || plan.status,
                        "{example} searches for nothing"
                    );
                    parse_query(&plan.query).unwrap_or_else(|e| panic!("{example}: {e}"));
                }
                other => panic!("{example}: {other:?}"),
            }
        }
    }

    /// Each search text form parses, and a function form is read as the
    /// function, not as text.
    #[test]
    fn every_search_text_form_parses() {
        for (usage, _) in SEARCH_TEXT {
            for form in usage.split(", ") {
                let form = form.replace("TEXT", "x");
                // dm: resolves against a clock; a fixed one in 2022 will do.
                let clock = crate::query::dates::tests::FixedClock::utc(133_000_000_000_000_000);
                let parsed =
                    parse_query_at(&form, false, &clock).unwrap_or_else(|e| panic!("{form}: {e}"));
                assert!(
                    !parsed.name_terms.iter().any(|term| term.contains(&b':')),
                    "{form} is searched as text"
                );
            }
        }
    }

    #[test]
    fn full_reference_lists_every_exit_code_and_sort_key() {
        let codes = usages_in("Exit codes");
        let expected: Vec<String> = Exit::ALL.iter().map(|e| e.code().to_string()).collect();
        assert_eq!(codes, expected);
        let names: Vec<String> = SORT_KEYS.iter().map(|row| row.names.join(", ")).collect();
        assert_eq!(usages_in("Sort keys"), names);
        // Each key's line names the order it takes without a suffix.
        let full = help(HelpScreen::Full);
        assert!(full.contains("Name, A to Z"), "{full}");
        assert!(full.contains("Size, largest first"), "{full}");
        assert!(full.contains("Date modified, newest first"), "{full}");
    }

    #[test]
    fn quick_help_lists_the_common_switches_and_points_to_the_reference() {
        let quick = help(HelpScreen::Quick);
        for switch in SWITCHES.iter().filter(|switch| switch.quick) {
            assert!(quick.contains(&first_usage(switch)), "{quick}");
        }
        assert!(quick.contains("goz --help"), "{quick}");
        assert!(quick.starts_with(VERSION_LINE), "{quick}");
    }

    /// Both screens fit an 80 column console, the quick help fits one 80 by
    /// 25 screen, and neither uses a dash as punctuation: every `-` starts
    /// or continues a word, as in a switch name.
    #[test]
    fn help_fits_the_console_and_has_no_dash_punctuation() {
        for screen in [HelpScreen::Quick, HelpScreen::Full] {
            let text = help(screen);
            assert!(text.ends_with('\n'));
            for line in text.lines() {
                assert!(line.len() <= LINE_WIDTH, "too wide: {line}");
                assert!(!line.ends_with(' '), "trailing space: {line:?}");
                assert!(!line.contains(['\u{2013}', '\u{2014}']), "{line}");
                let bytes = line.as_bytes();
                for (at, _) in line.match_indices('-') {
                    let next = bytes.get(at + 1).copied();
                    assert!(
                        next.is_some_and(|b| b != b' '),
                        "dash as punctuation: {line}"
                    );
                }
            }
        }
        assert!(help(HelpScreen::Quick).lines().count() <= 24);
    }
}
