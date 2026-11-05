//! Results pages as the bytes of the CLI's output, written as each page
//! arrives, so the client holds one page rather than the whole result set.
//!
//! The writer does no I/O: each call appends to a buffer the caller writes
//! out, so write errors stay with the caller. Dates come in through a
//! closure, since the local time zone is the CLI's to read.

use super::Row;
use super::csv::{CsvOptions, write_csv_preamble, write_csv_row};
use super::json::Json;
use crate::proto::QueryResults;
use crate::types::EsColumn;

/// What the output is.
#[derive(Clone, Debug)]
pub enum Format {
    /// One path per line, each ending in LF.
    Plain,
    /// es's `-export-csv` bytes.
    Csv(CsvOptions),
    /// One object for the whole result set, in the bytes
    /// `serde_json::to_string_pretty` gives a [`QueryResults`] holding every
    /// item, then an LF.
    Json,
}

/// Writes results pages in one [`Format`].
///
/// `date` formats a FILETIME for a `Date Modified` cell, and is called
/// only when that column is shown.
#[derive(Debug)]
pub struct ResultWriter<D> {
    state: State,
    date: D,
}

#[derive(Debug)]
enum State {
    Plain,
    Csv {
        opts: CsvOptions,
        /// Whether the byte order mark and header are written.
        started: bool,
    },
    Json(Json),
}

impl<D: FnMut(i64) -> String> ResultWriter<D> {
    /// A writer of `format` that formats dates with `date`.
    pub fn new(format: Format, date: D) -> Self {
        let state = match format {
            Format::Plain => State::Plain,
            Format::Csv(opts) => State::Csv {
                opts,
                started: false,
            },
            Format::Json => State::Json(Json::default()),
        };
        Self { state, date }
    }

    /// Appends the bytes of `page` to `out`. The first page also writes
    /// what comes before every row: the CSV byte order mark and header, or
    /// the head of the JSON object.
    pub fn page(&mut self, page: &QueryResults, out: &mut Vec<u8>) {
        match &mut self.state {
            State::Plain => {
                for item in &page.items {
                    out.extend_from_slice(item.path.as_bytes());
                    out.push(b'\n');
                }
            }
            State::Csv { opts, started } => {
                if !*started {
                    write_csv_preamble(opts, out);
                    *started = true;
                }
                let wants_date = opts.columns.contains(&EsColumn::DateModified);
                for item in &page.items {
                    let date = if wants_date {
                        item.mtime_ft.map(&mut self.date)
                    } else {
                        None
                    };
                    let row = Row {
                        path: &item.path,
                        size: item.size,
                        mtime: date.as_deref(),
                        is_dir: item.is_dir,
                    };
                    write_csv_row(opts, &row, out);
                }
            }
            State::Json(json) => json.page(page, out),
        }
    }

    /// Appends what follows the last page: the tail of the JSON object, or
    /// the CSV preamble when no page came.
    pub fn finish(self, out: &mut Vec<u8>) {
        match self.state {
            State::Plain => {}
            State::Csv { opts, started } => {
                if !started {
                    write_csv_preamble(&opts, out);
                }
            }
            State::Json(mut json) => json.tail(false, out),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::write_csv;
    use crate::proto::ResultItem;
    use proptest::prelude::*;

    fn item(path: &str, size: Option<u64>, mtime_ft: Option<i64>, is_dir: bool) -> ResultItem {
        ResultItem {
            path: path.into(),
            path_u16: None,
            size,
            mtime_ft,
            is_dir,
        }
    }

    fn page(total: u64, items: Vec<ResultItem>, more: bool) -> QueryResults {
        QueryResults {
            total,
            items,
            more,
            volumes_incomplete: false,
            metadata_pending: false,
            generation: 0,
            total_size: None,
        }
    }

    /// A date that shows which FILETIME it came from.
    fn date(ft: i64) -> String {
        format!("d{ft}")
    }

    /// The bytes `format` writes for `pages` and its finish.
    fn written(format: Format, pages: &[QueryResults]) -> Vec<u8> {
        let mut writer = ResultWriter::new(format, date);
        let mut out = Vec::new();
        for page in pages {
            writer.page(page, &mut out);
        }
        writer.finish(&mut out);
        out
    }

    /// `items` cut into pages after `header` at each of `cuts`, as the
    /// daemon sends them: every page repeats the header, and all but the
    /// last set `more`. A cut may leave a page empty, as the daemon does
    /// when every row of a page is dropped.
    fn paged(header: &QueryResults, items: &[ResultItem], cuts: &[usize]) -> Vec<QueryResults> {
        let mut bounds: Vec<usize> = cuts.iter().map(|&c| c.min(items.len())).collect();
        bounds.sort_unstable();
        bounds.push(items.len());
        let mut start = 0;
        let mut pages: Vec<QueryResults> = bounds
            .into_iter()
            .map(|end| {
                let page = QueryResults {
                    items: items[start..end].to_vec(),
                    more: true,
                    ..header.clone()
                };
                start = end;
                page
            })
            .collect();
        pages.last_mut().expect("one page at least").more = false;
        pages
    }

    /// `serde_json::to_string_pretty` of every item at once, `more` false,
    /// and an LF: the bytes `--json` printed before it streamed.
    fn pretty(header: &QueryResults, items: &[ResultItem]) -> String {
        let whole = QueryResults {
            items: items.to_vec(),
            more: false,
            ..header.clone()
        };
        format!("{}\n", serde_json::to_string_pretty(&whole).unwrap())
    }

    fn result_item() -> impl Strategy<Value = ResultItem> {
        (
            "[ -~\t\n\u{1}\u{7f}\u{e7}\u{11f}\u{2028}\u{6587}\u{1f389}]{0,24}",
            prop::option::of(prop::collection::vec(any::<u16>(), 0..6)),
            prop::option::of(any::<u64>()),
            prop::option::of(any::<i64>()),
            any::<bool>(),
        )
            .prop_map(|(path, path_u16, size, mtime_ft, is_dir)| ResultItem {
                path,
                path_u16,
                size,
                mtime_ft,
                is_dir,
            })
    }

    fn header() -> impl Strategy<Value = QueryResults> {
        (
            any::<u64>(),
            any::<bool>(),
            any::<bool>(),
            any::<u64>(),
            prop::option::of(any::<u64>()),
        )
            .prop_map(
                |(total, volumes_incomplete, metadata_pending, generation, total_size)| {
                    QueryResults {
                        total,
                        items: Vec::new(),
                        more: false,
                        volumes_incomplete,
                        metadata_pending,
                        generation,
                        total_size,
                    }
                },
            )
    }

    proptest! {
        /// CSV over pages is the CSV of all the rows at once: the byte
        /// order mark and header come once, and no row is lost or moved.
        #[test]
        fn csv_pages_equal_one_csv(
            items in prop::collection::vec(result_item(), 0..12),
            cuts in prop::collection::vec(0..12usize, 0..4),
            columns in prop::sample::select(vec![
                vec![],
                vec![EsColumn::Size],
                vec![EsColumn::DateModified],
                vec![EsColumn::Size, EsColumn::DateModified],
                vec![EsColumn::DateModified, EsColumn::Size],
            ]),
            bom in any::<bool>(),
            header in any::<bool>(),
            double_quote in any::<bool>(),
        ) {
            let opts = CsvOptions { bom, header, double_quote, columns };
            let pages = paged(&page(0, Vec::new(), false), &items, &cuts);
            let dates: Vec<Option<String>> =
                items.iter().map(|it| it.mtime_ft.map(date)).collect();
            let rows = items.iter().zip(&dates).map(|(it, d)| Row {
                path: &it.path,
                size: it.size,
                mtime: d.as_deref(),
                is_dir: it.is_dir,
            });
            let mut expected = Vec::new();
            write_csv(&opts, rows, &mut expected);
            prop_assert_eq!(written(Format::Csv(opts), &pages), expected);
        }

        /// `--json` over pages prints the bytes it printed when the client
        /// merged every page first.
        #[test]
        fn json_pages_equal_one_pretty_object(
            header in header(),
            items in prop::collection::vec(result_item(), 0..12),
            cuts in prop::collection::vec(0..12usize, 0..4),
        ) {
            let pages = paged(&header, &items, &cuts);
            let out = String::from_utf8(written(Format::Json, &pages)).unwrap();
            prop_assert_eq!(out, pretty(&header, &items));
        }
    }

    /// A CSV of no rows is the byte order mark and the header, whether the
    /// one page is empty or no page came.
    #[test]
    fn csv_with_no_rows_is_bom_and_header() {
        let opts = CsvOptions {
            columns: vec![EsColumn::Size, EsColumn::DateModified],
            ..CsvOptions::default()
        };
        let expected = b"\xEF\xBB\xBFSize,Date Modified,Filename\r\n";
        let empty = page(0, Vec::new(), false);
        assert_eq!(written(Format::Csv(opts.clone()), &[empty]), expected);
        assert_eq!(written(Format::Csv(opts), &[]), expected);
    }

    /// Each page's rows are out once that page is written, before the next
    /// page exists: nothing waits for the finish.
    #[test]
    fn each_page_is_written_when_it_arrives() {
        let first = page(2, vec![item(r"C:\a", Some(1), Some(7), false)], true);
        let second = page(2, vec![item(r"C:\b", None, None, true)], false);
        let csv = Format::Csv(CsvOptions {
            columns: vec![EsColumn::Size, EsColumn::DateModified],
            ..CsvOptions::default()
        });
        for (format, after_first, after_second) in [
            (Format::Plain, &b"C:\\a\n"[..], &b"C:\\b\n"[..]),
            (
                csv,
                b"\xEF\xBB\xBFSize,Date Modified,Filename\r\n1,d7,\"C:\\a\"\r\n",
                b",,\"C:\\b\"\r\n",
            ),
        ] {
            let mut writer = ResultWriter::new(format, date);
            let mut out = Vec::new();
            writer.page(&first, &mut out);
            assert_eq!(out, after_first);
            out.clear();
            writer.page(&second, &mut out);
            assert_eq!(out, after_second);
        }

        let mut writer = ResultWriter::new(Format::Json, date);
        let mut out = Vec::new();
        writer.page(&first, &mut out);
        let head = String::from_utf8(out).unwrap();
        let whole = pretty(&first, &[first.items[0].clone(), second.items[0].clone()]);
        assert!(whole.starts_with(&head), "{head}");
        assert!(head.ends_with("\"is_dir\": false\n    }"), "{head}");
    }

    /// With no page at all, `--json` still prints a whole object.
    #[test]
    fn json_with_no_page_is_an_empty_object() {
        let out = String::from_utf8(written(Format::Json, &[])).unwrap();
        assert_eq!(out, pretty(&page(0, Vec::new(), false), &[]));
    }
}
