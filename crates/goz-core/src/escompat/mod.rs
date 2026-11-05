//! es.exe-compatible command-line parsing, goz's exit codes and its help.
//!
//! A pure translation of a command line into a [`Command`] (what to search,
//! how to sort, what to output, or which help to print) or an [`EsFatal`]
//! carrying the es-compatible process exit code. The rules are lifted from
//! the voidtools es source: how it reads quotes, case-sensitive names with
//! every interior dash optional, `/`-prefixed fallthrough to search text,
//! and es exit codes 4 (missing/bad switch parameter) and 6 (unknown switch).
//!
//! goz-native extensions live behind a `--` prefix (`--json`, `--status`,
//! `--insecure-no-server-check`), which es reads as the same switch with one
//! dash, so goz's take precedence only where es has none or, for `--json`,
//! where goz's shape differs.
//!
//! The help is rendered from the switch and sort key tables the parser
//! reads and from [`Exit::ALL`], so it lists exactly what goz accepts.

mod argv;
mod es_only;
mod exit;
mod help;

pub use argv::{
    Command, EsFatal, EsPlan, HelpScreen, OutputFormat, parse_argv, parse_command_line,
};
pub use exit::Exit;
pub use help::{VERSION_LINE, help};
