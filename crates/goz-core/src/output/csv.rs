//! Byte-exact es-compatible CSV writer, and TSV, the same with a tab.
//!
//! Reproduces the `-export-csv` output of voidtools es.exe 1.1.0.30, with one
//! documented deviation: goz writes a UTF-8 BOM by default (stock es only with
//! `-utf8-bom`); `-no-utf8-bom` is the escape hatch.
//!
//! Format facts implemented here, all verified against the es source:
//! - header row uses column display names (`Size`, `Date Modified`,
//!   `Filename`), never quoted, CRLF-terminated;
//! - every data row is CRLF-terminated, including the last;
//! - column order = flag order; `Filename` is always last;
//! - `Size` and `Date Modified` are never quoted; `Size` is empty for
//!   directories (empty size ⇒ directory in the es contract) and for unknown
//!   sizes; the date is the caller's preformatted string verbatim, empty
//!   when unknown;
//! - `Filename` is the full path with no trailing backslash for folders.
//!   With `double_quote` (the default) it is always wrapped in `"` with
//!   interior `"` doubled; with `-no-double-quote` es downgrades to
//!   quote-only-when-needed (field contains `,`, `"`, CR, or LF;
//!   `_es_should_quote`, cli.c:1707-1718), which this writer matches.
//!
//! [`write_csv_preamble`] and [`write_csv_row`] are the two halves of
//! [`write_csv`], so a caller can write the rows page by page.

use super::Row;
use crate::types::EsColumn;

/// Options controlling the CSV byte stream, mirroring es.exe's flags.
#[derive(Clone, Debug)]
pub struct CsvOptions {
    /// What separates the cells: `,`, or a tab for TSV.
    pub separator: u8,
    /// Write the UTF-8 BOM (`EF BB BF`) first. goz defaults to `true`, a
    /// documented deviation; stock es writes a BOM only with `-utf8-bom`.
    pub bom: bool,
    /// Write the header row. `false` = `-no-header`.
    pub header: bool,
    /// Always double-quote the filename column (es default). `false` =
    /// `-no-double-quote`: quote only when the field contains `,`, `"`, CR,
    /// or LF (es's `_es_should_quote` downgrade).
    pub double_quote: bool,
    /// Optional columns in flag order (from [`crate::types`]); `Filename` is
    /// implicit and always emitted last.
    pub columns: Vec<EsColumn>,
}

impl Default for CsvOptions {
    /// goz defaults; BOM on is a documented deviation from stock es.
    fn default() -> Self {
        Self {
            separator: b',',
            bom: true,
            header: true,
            double_quote: true,
            columns: Vec::new(),
        }
    }
}

/// Serializes `rows` as es-compatible CSV, appending raw bytes to `out`.
///
/// Infallible: writes to an in-memory buffer and every row shape is
/// representable. See the module docs for the exact byte contract.
pub fn write_csv<'a>(
    opts: &CsvOptions,
    rows: impl IntoIterator<Item = Row<'a>>,
    out: &mut Vec<u8>,
) {
    write_csv_preamble(opts, out);
    for row in rows {
        write_csv_row(opts, &row, out);
    }
}

/// Appends what comes before the first row: the byte order mark and the
/// header row, each when `opts` asks for it.
pub fn write_csv_preamble(opts: &CsvOptions, out: &mut Vec<u8>) {
    if opts.bom {
        out.extend_from_slice(b"\xEF\xBB\xBF");
    }
    if opts.header {
        for &col in &opts.columns {
            out.extend_from_slice(column_display_name(col).as_bytes());
            out.push(opts.separator);
        }
        out.extend_from_slice(b"Filename\r\n");
    }
}

/// Appends one record, CRLF included.
pub fn write_csv_row(opts: &CsvOptions, row: &Row<'_>, out: &mut Vec<u8>) {
    for &col in &opts.columns {
        match col {
            EsColumn::Size => {
                if let Some(size) = row.file_size() {
                    out.extend_from_slice(size.to_string().as_bytes());
                }
            }
            EsColumn::DateModified => {
                if let Some(mtime) = row.mtime {
                    out.extend_from_slice(mtime.as_bytes());
                }
            }
        }
        out.push(opts.separator);
    }
    if opts.double_quote || needs_quoting(row.path, opts.separator) {
        write_quoted(row.path, out);
    } else {
        out.extend_from_slice(row.path.as_bytes());
    }
    out.extend_from_slice(b"\r\n");
}

/// es's CSV header display names (cli.c:303-320) for the columns we emit.
fn column_display_name(col: EsColumn) -> &'static str {
    match col {
        EsColumn::Size => "Size",
        EsColumn::DateModified => "Date Modified",
    }
}

/// es's `_es_should_quote` (cli.c:1707-1718): quote when the field contains
/// the separator, a double quote, CR, or LF.
fn needs_quoting(field: &str, separator: u8) -> bool {
    field
        .bytes()
        .any(|b| b == separator || matches!(b, b'"' | b'\r' | b'\n'))
}

/// Wraps `field` in double quotes, doubling interior `"` (RFC-4180 / es
/// cli.c:1668-1697).
pub(super) fn write_quoted(field: &str, out: &mut Vec<u8>) {
    out.push(b'"');
    for &b in field.as_bytes() {
        if b == b'"' {
            out.push(b'"');
        }
        out.push(b);
    }
    out.push(b'"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn size_dm_opts() -> CsvOptions {
        CsvOptions {
            columns: vec![EsColumn::Size, EsColumn::DateModified],
            ..CsvOptions::default()
        }
    }

    fn file<'a>(path: &'a str, size: u64, mtime: &'static str) -> Row<'a> {
        Row {
            path,
            size: Some(size),
            mtime: Some(mtime),
            is_dir: false,
        }
    }

    fn write(opts: &CsvOptions, rows: Vec<Row<'_>>) -> Vec<u8> {
        let mut out = Vec::new();
        write_csv(opts, rows, &mut out);
        out
    }

    #[test]
    fn golden_zero_rows_bom_and_header_only() {
        let out = write(&size_dm_opts(), vec![]);
        assert_eq!(
            out.as_slice(),
            b"\xEF\xBB\xBFSize,Date Modified,Filename\r\n"
        );
    }

    /// TSV is the same with a tab: a comma needs no quotes, and a tab does.
    #[test]
    fn a_tab_separator_quotes_tabs_and_not_commas() {
        let opts = CsvOptions {
            separator: b'\t',
            bom: false,
            double_quote: false,
            ..size_dm_opts()
        };
        let out = write(
            &opts,
            vec![
                file(r"C:\a,b", 5, "2026-07-13T15:42:00"),
                file("C:\\a\tb", 6, "2026-07-13T15:42:00"),
            ],
        );
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "Size\tDate Modified\tFilename\r\n\
             5\t2026-07-13T15:42:00\tC:\\a,b\r\n\
             6\t2026-07-13T15:42:00\t\"C:\\a\tb\"\r\n"
        );
    }

    #[test]
    fn golden_file_row() {
        let out = write(
            &size_dm_opts(),
            vec![file("C:\\proj\\a.txt", 1024, "2026-07-13T15:42:00")],
        );
        assert_eq!(
            out.as_slice(),
            b"\xEF\xBB\xBFSize,Date Modified,Filename\r\n\
              1024,2026-07-13T15:42:00,\"C:\\proj\\a.txt\"\r\n"
        );
    }

    #[test]
    fn golden_dir_row_empty_size() {
        // Directories get an empty Size even when a size is present, and
        // when it is None; a file with size None is also empty.
        let rows = vec![
            Row {
                path: "C:\\proj\\sub",
                size: None,
                mtime: Some("2026-07-12T09:01:00"),
                is_dir: true,
            },
            Row {
                path: "C:\\proj\\sized-dir",
                size: Some(4096),
                mtime: None,
                is_dir: true,
            },
            Row {
                path: "C:\\proj\\unknown.bin",
                size: None,
                mtime: None,
                is_dir: false,
            },
        ];
        let out = write(&size_dm_opts(), rows);
        assert_eq!(
            out.as_slice(),
            b"\xEF\xBB\xBFSize,Date Modified,Filename\r\n\
              ,2026-07-12T09:01:00,\"C:\\proj\\sub\"\r\n\
              ,,\"C:\\proj\\sized-dir\"\r\n\
              ,,\"C:\\proj\\unknown.bin\"\r\n"
        );
    }

    #[test]
    fn golden_interior_quotes_doubled() {
        let out = write(
            &size_dm_opts(),
            vec![file("C:\\he said \"hi\".txt", 7, "2026-01-02T03:04:05")],
        );
        assert_eq!(
            out.as_slice(),
            b"\xEF\xBB\xBFSize,Date Modified,Filename\r\n\
              7,2026-01-02T03:04:05,\"C:\\he said \"\"hi\"\".txt\"\r\n"
        );
    }

    #[test]
    fn golden_commas_and_spaces_stay_one_field() {
        let out = write(
            &size_dm_opts(),
            vec![file("C:\\a, b\\c d.txt", 53, "2026-07-11T08:00:00")],
        );
        assert_eq!(
            out.as_slice(),
            b"\xEF\xBB\xBFSize,Date Modified,Filename\r\n\
              53,2026-07-11T08:00:00,\"C:\\a, b\\c d.txt\"\r\n"
        );
    }

    #[test]
    fn golden_unicode_path_utf8_verbatim() {
        // "C:\ağaç\文件名.txt": ğ=C4 9F, ç=C3 A7, 文=E6 96 87, 件=E4 BB B6,
        // 名=E5 90 8D.
        let out = write(
            &size_dm_opts(),
            vec![file(
                "C:\\a\u{11F}a\u{E7}\\\u{6587}\u{4EF6}\u{540D}.txt",
                1,
                "2026-06-01T00:00:00",
            )],
        );
        assert_eq!(
            out.as_slice(),
            b"\xEF\xBB\xBFSize,Date Modified,Filename\r\n\
              1,2026-06-01T00:00:00,\"C:\\a\xC4\x9Fa\xC3\xA7\\\xE6\x96\x87\xE4\xBB\xB6\xE5\x90\x8D.txt\"\r\n"
        );
    }

    #[test]
    fn golden_no_header() {
        let opts = CsvOptions {
            header: false,
            ..size_dm_opts()
        };
        let out = write(&opts, vec![file("C:\\x.txt", 9, "2026-05-04T03:02:01")]);
        assert_eq!(
            out.as_slice(),
            b"\xEF\xBB\xBF9,2026-05-04T03:02:01,\"C:\\x.txt\"\r\n"
        );
    }

    #[test]
    fn golden_no_bom() {
        let opts = CsvOptions {
            bom: false,
            ..size_dm_opts()
        };
        let out = write(&opts, vec![file("C:\\x.txt", 9, "2026-05-04T03:02:01")]);
        assert_eq!(
            out.as_slice(),
            b"Size,Date Modified,Filename\r\n\
              9,2026-05-04T03:02:01,\"C:\\x.txt\"\r\n"
        );
    }

    #[test]
    fn golden_no_double_quote_plain_path_unquoted() {
        let opts = CsvOptions {
            double_quote: false,
            ..size_dm_opts()
        };
        let out = write(
            &opts,
            vec![file("C:\\proj\\a.txt", 5, "2026-07-13T15:42:00")],
        );
        assert_eq!(
            out.as_slice(),
            b"\xEF\xBB\xBFSize,Date Modified,Filename\r\n\
              5,2026-07-13T15:42:00,C:\\proj\\a.txt\r\n"
        );
    }

    #[test]
    fn no_double_quote_still_quotes_when_needed() {
        // Research deviation from the naive reading of -no-double-quote: es
        // downgrades to quote-only-when-needed (_es_should_quote), it never
        // emits an unparseable line. A comma or quote in the path still
        // forces quoting.
        let opts = CsvOptions {
            double_quote: false,
            ..size_dm_opts()
        };
        let out = write(
            &opts,
            vec![
                file("C:\\a,b.txt", 1, "2026-01-01T00:00:00"),
                file("C:\\q\"uote.txt", 2, "2026-01-01T00:00:00"),
            ],
        );
        assert_eq!(
            out.as_slice(),
            b"\xEF\xBB\xBFSize,Date Modified,Filename\r\n\
              1,2026-01-01T00:00:00,\"C:\\a,b.txt\"\r\n\
              2,2026-01-01T00:00:00,\"C:\\q\"\"uote.txt\"\r\n"
        );
    }

    #[test]
    fn golden_date_modified_only_column() {
        let opts = CsvOptions {
            columns: vec![EsColumn::DateModified],
            ..CsvOptions::default()
        };
        let out = write(&opts, vec![file("C:\\x.txt", 9, "2026-05-04T03:02:01")]);
        assert_eq!(
            out.as_slice(),
            b"\xEF\xBB\xBFDate Modified,Filename\r\n\
              2026-05-04T03:02:01,\"C:\\x.txt\"\r\n"
        );
    }

    #[test]
    fn golden_filename_only_when_no_columns() {
        let out = write(
            &CsvOptions::default(),
            vec![file("C:\\x.txt", 9, "2026-05-04T03:02:01")],
        );
        assert_eq!(out.as_slice(), b"\xEF\xBB\xBFFilename\r\n\"C:\\x.txt\"\r\n");
    }

    #[test]
    fn golden_column_order_follows_flag_order() {
        let opts = CsvOptions {
            columns: vec![EsColumn::DateModified, EsColumn::Size],
            bom: false,
            ..CsvOptions::default()
        };
        let out = write(&opts, vec![file("C:\\x.txt", 9, "2026-05-04T03:02:01")]);
        assert_eq!(
            out.as_slice(),
            b"Date Modified,Size,Filename\r\n\
              2026-05-04T03:02:01,9,\"C:\\x.txt\"\r\n"
        );
    }

    /// Minimal RFC-4180-style reader for the parse-back property: BOM strip,
    /// CRLF records, `"`-quoting with `""` unescape. Panics on malformed
    /// input: the property is that our writer never produces any.
    fn parse_csv(mut bytes: &[u8]) -> Vec<Vec<String>> {
        if let Some(rest) = bytes.strip_prefix(b"\xEF\xBB\xBF") {
            bytes = rest;
        }
        let mut records = Vec::new();
        let mut fields = Vec::new();
        let mut field = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'"' if field.is_empty() => {
                    i += 1;
                    loop {
                        match bytes[i] {
                            b'"' if bytes.get(i + 1) == Some(&b'"') => {
                                field.push(b'"');
                                i += 2;
                            }
                            b'"' => {
                                i += 1;
                                break;
                            }
                            other => {
                                field.push(other);
                                i += 1;
                            }
                        }
                    }
                }
                b',' => {
                    fields.push(String::from_utf8(std::mem::take(&mut field)).unwrap());
                    i += 1;
                }
                b'\r' if bytes.get(i + 1) == Some(&b'\n') => {
                    fields.push(String::from_utf8(std::mem::take(&mut field)).unwrap());
                    records.push(std::mem::take(&mut fields));
                    i += 2;
                }
                other => {
                    field.push(other);
                    i += 1;
                }
            }
        }
        assert!(field.is_empty() && fields.is_empty(), "unterminated record");
        records
    }

    proptest! {
        /// Arbitrary printable paths (quotes/commas/unicode included) and
        /// sizes survive a write → naive-reader round trip field-exactly, in
        /// both quoting modes, with and without header/BOM.
        #[test]
        fn parse_back_recovers_exact_fields(
            rows in prop::collection::vec(
                (
                    "[ -~çğı文🎉]{1,40}",
                    prop::option::of(proptest::num::u64::ANY),
                    prop::option::of("[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}"),
                    proptest::bool::ANY,
                ),
                0..8,
            ),
            double_quote in proptest::bool::ANY,
            header in proptest::bool::ANY,
            bom in proptest::bool::ANY,
        ) {
            let opts = CsvOptions {
                bom,
                header,
                double_quote,
                columns: vec![EsColumn::Size, EsColumn::DateModified],
                ..CsvOptions::default()
            };
            let mut out = Vec::new();
            write_csv(
                &opts,
                rows.iter().map(|(path, size, mtime, is_dir)| Row {
                    path,
                    size: *size,
                    mtime: mtime.as_deref(),
                    is_dir: *is_dir,
                }),
                &mut out,
            );

            let mut expected: Vec<Vec<String>> = Vec::new();
            if header {
                expected.push(vec![
                    "Size".into(),
                    "Date Modified".into(),
                    "Filename".into(),
                ]);
            }
            for (path, size, mtime, is_dir) in &rows {
                let size_field = match (is_dir, size) {
                    (false, Some(s)) => s.to_string(),
                    _ => String::new(),
                };
                expected.push(vec![
                    size_field,
                    mtime.clone().unwrap_or_default(),
                    path.clone(),
                ]);
            }
            prop_assert_eq!(parse_csv(&out), expected);
        }
    }
}
