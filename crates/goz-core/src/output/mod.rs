//! Output formatting: the byte-exact es-compatible CSV writer, the page by
//! page result writer, and FILETIME conversion/formatting helpers.
//!
//! [`csv`] reproduces voidtools es.exe's `-export-csv` byte stream (spec
//! extracted from the es source), with goz's one documented deviation: the
//! UTF-8 BOM is on by default.
//! [`plain`] writes the default listing: a path per line, after es's console
//! cells for `-size` and `-dm`.
//! [`writer`] turns each results page into the bytes of the CLI's output as
//! the page arrives: plain lines, CSV rows, or the `--json` object.
//! [`filetime`] converts between FILETIME ticks and Unix milliseconds and
//! renders civil UTC timestamps without a timezone database (the CLI formats
//! local time itself).

pub mod csv;
pub mod filetime;
mod json;
pub mod plain;
pub mod writer;

pub use csv::{CsvOptions, write_csv, write_csv_preamble, write_csv_row};
pub use filetime::{
    FILETIME_UNIX_EPOCH, filetime_to_unix_ms, format_filetime_utc, unix_ms_to_filetime,
};
pub use plain::{DigitGrouping, PlainOptions, write_plain_row};
pub use writer::{Format, ResultWriter};

/// One result row, borrowed from the caller.
#[derive(Clone, Debug)]
pub struct Row<'a> {
    /// Full path and name; folders must carry no trailing backslash (the
    /// caller's responsibility: the writer never adds or strips one).
    pub path: &'a str,
    /// Size in bytes; `None` = unknown → empty field.
    pub size: Option<u64>,
    /// Preformatted modification time (the CLI formats local time); `None`
    /// → empty field. Written verbatim, never quoted.
    pub mtime: Option<&'a str>,
    /// Directories always get an empty `Size` field (empty size ⇒ directory
    /// in the es contract), regardless of `size`.
    pub is_dir: bool,
}

impl Row<'_> {
    /// The size a `Size` cell shows: none for a folder, as es leaves it
    /// empty, and none when unknown.
    fn file_size(&self) -> Option<u64> {
        if self.is_dir { None } else { self.size }
    }
}
