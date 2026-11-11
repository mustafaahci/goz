//! Output formatting: the byte-exact es-compatible CSV writer, the page by
//! page result writer, and FILETIME conversion/formatting helpers.
//!
//! [`csv`] reproduces voidtools es.exe's `-export-csv` byte stream (spec
//! extracted from the es source), with goz's one documented deviation: the
//! UTF-8 BOM is on by default.
//! [`plain`] writes the default listing: a path per line, after es's console
//! cells for `-size` and `-dm`.
//! [`list`] writes es's other export formats: `-txt`, `-efu` and `-json`.
//! [`writer`] turns each results page into the bytes of the CLI's output as
//! the page arrives: plain lines, CSV rows, those formats, or the `--json`
//! object.
//! [`filetime`] converts between FILETIME ticks and Unix milliseconds and
//! renders civil UTC timestamps without a timezone database (the CLI formats
//! local time itself).

pub mod csv;
pub mod filetime;
mod json;
pub mod list;
pub mod plain;
pub mod writer;

pub use csv::{CsvOptions, write_csv, write_csv_preamble, write_csv_row};
pub use filetime::{
    FILETIME_UNIX_EPOCH, filetime_to_unix_ms, format_filetime_utc, unix_ms_to_filetime,
};
pub use list::{ListKind, ListOptions};
pub use plain::{DigitGrouping, PlainOptions, write_plain_row};
pub use writer::{Format, ResultWriter};

/// One result row, borrowed from the caller.
#[derive(Clone, Debug, Default)]
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
    /// Preformatted date created, as `mtime`.
    pub ctime: Option<&'a str>,
    /// Preformatted date accessed, as `mtime`.
    pub atime: Option<&'a str>,
    /// Preformatted date recently changed, as `mtime`.
    pub rtime: Option<&'a str>,
    /// `FILE_ATTRIBUTE_*` bits; `None` → empty field.
    pub attrib: Option<u32>,
}

/// Everything's attribute letters, each with its `FILE_ATTRIBUTE_*` bit, in
/// the order of the bits, which is the order its Attributes column shows
/// them in (`RHSA`, `HD`, `DL`; a live Everything 1.5 sorts that column by
/// the letters, W9). Sparse (0x200) has no letter.
pub const ATTRIBUTE_LETTERS: [(char, u32); 17] = [
    ('R', 0x1),
    ('H', 0x2),
    ('S', 0x4),
    ('D', 0x10),
    ('A', 0x20),
    ('N', 0x80),
    ('T', 0x100),
    ('L', 0x400),
    ('C', 0x800),
    ('O', 0x1000),
    ('I', 0x2000),
    ('E', 0x4000),
    ('V', 0x8000),
    ('X', 0x2_0000),
    ('P', 0x8_0000),
    ('U', 0x10_0000),
    ('M', 0x40_0000),
];

/// Attributes as Everything's column shows them: the letters of the bits
/// set.
pub fn attribute_letters(attrib: u32) -> String {
    ATTRIBUTE_LETTERS
        .iter()
        .filter(|(_, bit)| attrib & bit != 0)
        .map(|(c, _)| *c)
        .collect()
}

/// A key that orders attributes as their letters (see
/// [`attribute_letters`]) order as text, which is how Everything sorts its
/// Attributes column: each letter 5 bits from the top, its place in the
/// alphabet, so a shorter text that starts a longer one comes first. Twelve
/// letters fit; no file has more.
pub fn attribute_sort_key(attrib: u32) -> u64 {
    let mut key = 0u64;
    let mut shift = 59;
    for (c, bit) in ATTRIBUTE_LETTERS {
        if attrib & bit != 0 && shift >= 0 {
            key |= u64::from(c as u8 - b'A' + 1) << shift;
            shift -= 5;
        }
    }
    key
}

impl Row<'_> {
    /// The size a `Size` cell shows: none for a folder, as es leaves it
    /// empty, and none when unknown.
    fn file_size(&self) -> Option<u64> {
        if self.is_dir { None } else { self.size }
    }
}
