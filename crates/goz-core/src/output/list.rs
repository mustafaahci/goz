//! es's other export formats, written page by page, from the es source:
//!
//! - `-txt`, `-m3u` and `-m3u8` hold only the paths, each on a CRLF line. es
//!   writes `-m3u` in the ANSI code page; goz has no good answer for a path
//!   that does not fit one, so it writes UTF-8, as for `-m3u8`.
//! - `-efu` is an Everything file list, its filename always quoted, with
//!   `Size`, `Date Modified` as a raw FILETIME and `Attributes`. Of the
//!   attributes goz knows only the directory bit, the one es too says is
//!   always valid. goz has no creation dates, so it leaves that column out.
//! - `-json` is one array of an object per result, each on a CRLF line, sizes
//!   and dates as numbers or `null`. es prints nothing at all for no results,
//!   which no JSON parser accepts; goz prints `[]`.
//!
//! Like the CSV, these keep a folder's path without the trailing backslash es
//! adds to its exports, so that every format of one search lists the same paths.

use super::csv::write_quoted;
use crate::proto::{QueryResults, ResultItem};
use crate::types::EsColumn;

/// The attribute bit of a directory.
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

/// Which of these formats to write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListKind {
    /// The paths, one per line.
    Txt,
    /// An Everything file list.
    Efu,
    /// es's JSON array.
    Json,
}

/// Options for one of these formats.
#[derive(Clone, Debug)]
pub struct ListOptions {
    pub kind: ListKind,
    /// Start with a UTF-8 byte order mark.
    pub bom: bool,
    /// Start with a header row, which `Json` never has.
    pub header: bool,
    /// Wrap each `Txt` path in double quotes.
    pub quote: bool,
    /// The cells of each `Json` object before its path.
    pub columns: Vec<EsColumn>,
}

/// Where one of these formats stands between pages.
#[derive(Debug)]
pub(super) struct List {
    opts: ListOptions,
    /// Whether what comes before the first row is written.
    started: bool,
    /// Whether a row is written, so a JSON row needs a separator first.
    any_row: bool,
}

impl List {
    pub(super) fn new(opts: ListOptions) -> Self {
        Self {
            opts,
            started: false,
            any_row: false,
        }
    }

    pub(super) fn page(&mut self, page: &QueryResults, out: &mut Vec<u8>) {
        self.start(out);
        for item in &page.items {
            match self.opts.kind {
                ListKind::Txt => self.txt_row(item, out),
                ListKind::Efu => efu_row(item, out),
                ListKind::Json => self.json_row(item, out),
            }
            self.any_row = true;
        }
    }

    /// Appends what follows the last row, and what comes before the first
    /// when no page came.
    pub(super) fn finish(&mut self, out: &mut Vec<u8>) {
        self.start(out);
        if self.opts.kind == ListKind::Json {
            out.extend_from_slice(if self.any_row { b"]\r\n" } else { b"[]\r\n" });
        }
    }

    fn start(&mut self, out: &mut Vec<u8>) {
        if self.started {
            return;
        }
        self.started = true;
        if self.opts.bom {
            out.extend_from_slice(b"\xEF\xBB\xBF");
        }
        if self.opts.header {
            match self.opts.kind {
                ListKind::Txt => out.extend_from_slice(b"Filename\r\n"),
                ListKind::Efu => {
                    out.extend_from_slice(b"Filename,Size,Date Modified,Attributes\r\n");
                }
                ListKind::Json => {}
            }
        }
    }

    fn txt_row(&self, item: &ResultItem, out: &mut Vec<u8>) {
        if self.opts.quote {
            out.push(b'"');
        }
        out.extend_from_slice(item.path.as_bytes());
        if self.opts.quote {
            out.push(b'"');
        }
        out.extend_from_slice(b"\r\n");
    }

    fn json_row(&self, item: &ResultItem, out: &mut Vec<u8>) {
        out.extend_from_slice(if self.any_row { b",\r\n{" } else { b"[{" });
        for &column in &self.opts.columns {
            match column {
                EsColumn::Size => json_number(b"size", file_size(item), out),
                EsColumn::DateModified => {
                    json_number(b"date_modified", item.mtime_ft.map(|ft| ft as u64), out);
                }
                EsColumn::DateCreated => {
                    json_number(b"date_created", item.ctime_ft.map(|ft| ft as u64), out);
                }
                EsColumn::DateAccessed => {
                    json_number(b"date_accessed", item.atime_ft.map(|ft| ft as u64), out);
                }
                EsColumn::DateRecentlyChanged => {
                    let rtime = item.rtime_ft.map(|ft| ft as u64);
                    json_number(b"date_recently_changed", rtime, out);
                }
                EsColumn::Attributes => {
                    json_number(b"attributes", item.attrib.map(u64::from), out);
                }
            }
            out.push(b',');
        }
        out.extend_from_slice(b"\"filename\":");
        serde_json::to_writer(&mut *out, &item.path)
            .expect("writing a JSON string to a buffer is infallible");
        out.push(b'}');
    }
}

/// One line of an Everything file list.
fn efu_row(item: &ResultItem, out: &mut Vec<u8>) {
    write_quoted(&item.path, out);
    out.push(b',');
    if let Some(size) = file_size(item) {
        out.extend_from_slice(size.to_string().as_bytes());
    }
    out.push(b',');
    if let Some(mtime) = item.mtime_ft {
        out.extend_from_slice(mtime.to_string().as_bytes());
    }
    let attributes = if item.is_dir {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        0
    };
    out.extend_from_slice(format!(",{attributes}\r\n").as_bytes());
}

/// The size a cell shows: none for a folder, whose cell es's output leaves
/// empty, folder sizes or not.
fn file_size(item: &ResultItem) -> Option<u64> {
    if item.is_dir { None } else { item.size }
}

fn json_number(name: &[u8], value: Option<u64>, out: &mut Vec<u8>) {
    out.push(b'"');
    out.extend_from_slice(name);
    out.extend_from_slice(b"\":");
    match value {
        Some(value) => out.extend_from_slice(value.to_string().as_bytes()),
        None => out.extend_from_slice(b"null"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(path: &str, is_dir: bool, size: Option<u64>, mtime_ft: Option<i64>) -> ResultItem {
        ResultItem {
            path: path.to_owned(),
            path_u16: None,
            is_dir,
            size,
            mtime_ft,
            ..Default::default()
        }
    }

    fn page(items: Vec<ResultItem>) -> QueryResults {
        QueryResults {
            total: items.len() as u64,
            items,
            more: false,
            volumes_incomplete: false,
            metadata_pending: false,
            generation: 0,
            total_size: None,
        }
    }

    fn opts(kind: ListKind) -> ListOptions {
        ListOptions {
            kind,
            bom: false,
            header: false,
            quote: false,
            columns: Vec::new(),
        }
    }

    /// Writes `pages`, one [`List::page`] call each, as the client does.
    fn written(opts: ListOptions, pages: Vec<Vec<ResultItem>>) -> String {
        let mut list = List::new(opts);
        let mut out = Vec::new();
        for items in pages {
            list.page(&page(items), &mut out);
        }
        list.finish(&mut out);
        String::from_utf8(out).unwrap()
    }

    fn sample() -> Vec<ResultItem> {
        vec![
            item(r"C:\a b.txt", false, Some(12), Some(133_000)),
            item(r"C:\dir", true, Some(4096), None),
            item(r"C:\unread", false, None, None),
        ]
    }

    #[test]
    fn txt_is_the_paths_on_crlf_lines() {
        assert_eq!(
            written(opts(ListKind::Txt), vec![sample()]),
            "C:\\a b.txt\r\nC:\\dir\r\nC:\\unread\r\n"
        );
        let quoted = ListOptions {
            quote: true,
            ..opts(ListKind::Txt)
        };
        assert_eq!(
            written(quoted, vec![vec![item(r"C:\a", false, None, None)]]),
            "\"C:\\a\"\r\n"
        );
        let with_header = ListOptions {
            header: true,
            ..opts(ListKind::Txt)
        };
        assert_eq!(written(with_header, vec![]), "Filename\r\n");
    }

    #[test]
    fn efu_quotes_the_path_and_keeps_raw_dates_and_the_directory_bit() {
        let efu = ListOptions {
            header: true,
            ..opts(ListKind::Efu)
        };
        assert_eq!(
            written(efu, vec![sample()]),
            "Filename,Size,Date Modified,Attributes\r\n\
             \"C:\\a b.txt\",12,133000,0\r\n\
             \"C:\\dir\",,,16\r\n\
             \"C:\\unread\",,,0\r\n"
        );
    }

    /// Valid JSON across pages, with a row per line, and an empty array for
    /// no rows.
    #[test]
    fn json_is_one_array_of_an_object_per_result() {
        let json = ListOptions {
            columns: vec![EsColumn::Size, EsColumn::DateModified],
            ..opts(ListKind::Json)
        };
        let mut rows = sample();
        let tail = rows.split_off(2);
        let text = written(json, vec![rows, vec![], tail]);
        assert_eq!(
            text,
            "[{\"size\":12,\"date_modified\":133000,\"filename\":\"C:\\\\a b.txt\"},\r\n\
             {\"size\":null,\"date_modified\":null,\"filename\":\"C:\\\\dir\"},\r\n\
             {\"size\":null,\"date_modified\":null,\"filename\":\"C:\\\\unread\"}]\r\n"
        );
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 3);
        assert_eq!(written(opts(ListKind::Json), vec![]), "[]\r\n");
        assert_eq!(written(opts(ListKind::Json), vec![vec![]]), "[]\r\n");
    }

    #[test]
    fn a_byte_order_mark_comes_first_once() {
        let bom = ListOptions {
            bom: true,
            ..opts(ListKind::Txt)
        };
        assert_eq!(
            written(bom, vec![vec![item("a", false, None, None)], vec![]]).as_bytes(),
            b"\xEF\xBB\xBFa\r\n"
        );
    }
}
