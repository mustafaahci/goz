//! The plain listing on stdout: one path per line, or with `-size` and
//! `-dm` the cells es prints on its console before each path.
//!
//! What es.c settles, and goz follows:
//! - the cells come in flag order and the path last (es.c:9375-9398), each
//!   cell followed by one space (es.c:3797-3823);
//! - Size is right-aligned, its digits grouped as the user's locale says
//!   (es.c:2243-2251, 10253-10256, 10437-10475, 6416-6471), and Date
//!   Modified is left-aligned;
//! - an unknown value is an empty padded cell (es.c:2234-2240, 2295-2300),
//!   and the path is never padded (es.c:1395, 1653);
//! - a cell wider than its column takes the excess from the padding of the
//!   cells after it on the same line (es.c:1405-1418, 1664-1685, 4148).
//!
//! goz's deviations: LF line ends, UTF-8, and the date as local
//! `YYYY-MM-DDTHH:MM:SS`, es's `-date-format 1`, where es's default is the
//! locale's short date and time. A folder has no size, since goz indexes
//! none.

use super::Row;
use crate::types::EsColumn;

/// The width of the Size column, in characters. es keeps its default
/// column widths outside es.c, so this is gozler's value, to be confirmed
/// against a capture of es.exe's console output.
pub const SIZE_WIDTH: usize = 14;

/// The width of the Date Modified column, in characters: the length of
/// `YYYY-MM-DDTHH:MM:SS`, so a known date never pads. To be confirmed
/// against a capture of es.exe's console output, as [`SIZE_WIDTH`] is.
pub const DATE_WIDTH: usize = 19;

/// Options for the plain listing.
#[derive(Clone, Debug)]
pub struct PlainOptions {
    /// The cells before each path, in flag order. None gives one path per
    /// line.
    pub columns: Vec<EsColumn>,
    /// How a Size cell groups its digits.
    pub grouping: DigitGrouping,
}

impl Default for PlainOptions {
    fn default() -> Self {
        Self {
            columns: Vec::new(),
            grouping: DigitGrouping::es_default(),
        }
    }
}

/// How a Size cell groups its digits, as the Windows `NUMBERFMT` does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DigitGrouping {
    separator: String,
    /// `NUMBERFMT::Grouping`: 0 for none, a digit for groups of that size,
    /// or two digits for a first group and then groups of the second size,
    /// as 32 for a group of 3 and then groups of 2.
    groups: u8,
}

impl DigitGrouping {
    /// es's grouping when the locale cannot be read: `,` every three digits
    /// (es.c:362, 6467-6471).
    pub fn es_default() -> Self {
        Self {
            separator: ",".into(),
            groups: 3,
        }
    }

    /// The grouping a locale's `LOCALE_STHOUSAND` and `LOCALE_SGROUPING`
    /// values give, read as es reads them (es.c:6418-6445): the grouping's
    /// digits up to its first `0`, so `3;0` is 3 and `3;2;0` is 32. A value
    /// that is missing is es's default.
    pub fn from_locale(thousand_separator: Option<&str>, grouping: Option<&str>) -> Self {
        let default = Self::es_default();
        Self {
            separator: thousand_separator.map_or(default.separator, str::to_owned),
            groups: grouping.map_or(default.groups, es_groups),
        }
    }

    /// `n` with its digits grouped: the first group from the right, then
    /// groups of the last size.
    pub fn format(&self, n: u64) -> String {
        let digits = n.to_string();
        let (first, rest) = match usize::from(self.groups) {
            0 => return digits,
            g @ 1..=9 => (g, g),
            g => (g / 10, g % 10),
        };
        // Where a separator goes, counted from the left.
        let mut cuts = Vec::new();
        let mut from_right = first;
        while from_right < digits.len() {
            cuts.push(digits.len() - from_right);
            from_right += rest;
        }
        let mut out = String::with_capacity(digits.len() + cuts.len() * self.separator.len());
        let mut start = 0;
        for &cut in cuts.iter().rev() {
            out.push_str(&digits[start..cut]);
            out.push_str(&self.separator);
            start = cut;
        }
        out.push_str(&digits[start..]);
        out
    }
}

/// es's reading of `LOCALE_SGROUPING`: its digits up to the first `0`, as
/// one number, which es hands to `NUMBERFMT` as it is. One or two digits
/// group as Windows groups them; a longer value, which no regional format
/// has, groups by 3.
fn es_groups(grouping: &str) -> u8 {
    let mut value: u32 = 0;
    for b in grouping.bytes() {
        match b {
            b'0' => break,
            b'1'..=b'9' => value = value.saturating_mul(10).saturating_add(u32::from(b - b'0')),
            _ => {}
        }
    }
    u8::try_from(value).ok().filter(|&g| g < 100).unwrap_or(3)
}

/// Appends one line of the listing: the cells `opts` asks for, then the
/// path and LF.
pub fn write_plain_row(opts: &PlainOptions, row: &Row<'_>, out: &mut Vec<u8>) {
    // How far the cells so far ran past their widths, which the padding of
    // the cells after them pays first, as es does.
    let mut overflow = 0;
    for &col in &opts.columns {
        let size;
        let (text, width, right_aligned) = match col {
            EsColumn::Size => {
                size = row.file_size().map(|s| opts.grouping.format(s));
                (size.as_deref().unwrap_or(""), SIZE_WIDTH, true)
            }
            EsColumn::DateModified => (row.mtime.unwrap_or(""), DATE_WIDTH, false),
        };
        // Characters, which for the separators locales use is es's count
        // of UTF-16 units.
        let len = text.chars().count();
        let mut fill = width.saturating_sub(len);
        let paid = fill.min(overflow);
        fill -= paid;
        overflow = overflow - paid + len.saturating_sub(width);
        if right_aligned {
            pad(fill, out);
            out.extend_from_slice(text.as_bytes());
        } else {
            out.extend_from_slice(text.as_bytes());
            pad(fill, out);
        }
        out.push(b' ');
    }
    out.extend_from_slice(row.path.as_bytes());
    out.push(b'\n');
}

fn pad(spaces: usize, out: &mut Vec<u8>) {
    out.resize(out.len() + spaces, b' ');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(columns: &[EsColumn], row: &Row<'_>) -> String {
        let opts = PlainOptions {
            columns: columns.to_vec(),
            ..PlainOptions::default()
        };
        let mut out = Vec::new();
        write_plain_row(&opts, row, &mut out);
        String::from_utf8(out).unwrap()
    }

    fn file<'a>(path: &'a str, size: Option<u64>, mtime: Option<&'a str>) -> Row<'a> {
        Row {
            path,
            size,
            mtime,
            is_dir: false,
        }
    }

    const DATE: &str = "2026-07-13T15:42:00";

    /// Without `-size` or `-dm` the listing is one path per line, as before
    /// it had columns.
    #[test]
    fn without_columns_one_path_per_line() {
        let mut out = Vec::new();
        for path in [r"C:\a", r"C:\b"] {
            write_plain_row(
                &PlainOptions::default(),
                &file(path, Some(1), Some(DATE)),
                &mut out,
            );
        }
        assert_eq!(out, b"C:\\a\nC:\\b\n");
    }

    /// es's console cells with en-US grouping, for a page through the
    /// result writer: the size right-aligned in 14, the date left-aligned
    /// in 19, a space after each, a folder's size and unknown values as
    /// empty padded cells, and the cells in flag order.
    #[test]
    fn columns_follow_the_es_console_layout() {
        use crate::output::{Format, ResultWriter, format_filetime_utc, unix_ms_to_filetime};
        use crate::proto::{QueryResults, ResultItem};
        use EsColumn::{DateModified, Size};

        let item = |path: &str, size, unix_secs: Option<i64>, is_dir| ResultItem {
            path: path.into(),
            path_u16: None,
            size,
            mtime_ft: unix_secs.map(|secs| unix_ms_to_filetime(secs * 1000)),
            is_dir,
        };
        let page = QueryResults {
            total: 3,
            items: vec![
                item(
                    r"C:\proj\report.txt",
                    Some(4096),
                    Some(1_783_957_320),
                    false,
                ),
                item(r"C:\proj\sub", Some(4096), Some(1_783_846_860), true),
                item(r"C:\proj\unknown.bin", None, None, false),
            ],
            more: false,
            volumes_incomplete: false,
            metadata_pending: false,
            generation: 0,
            total_size: None,
        };
        let listing = |columns: &[EsColumn]| {
            let format = Format::Plain(PlainOptions {
                columns: columns.to_vec(),
                grouping: DigitGrouping::from_locale(Some(","), Some("3;0")),
            });
            let mut writer = ResultWriter::new(format, format_filetime_utc);
            let mut out = Vec::new();
            writer.page(&page, &mut out);
            writer.finish(&mut out);
            String::from_utf8(out).unwrap()
        };
        let s = |n| " ".repeat(n);
        assert_eq!(
            listing(&[Size, DateModified]),
            format!(
                "{}4,096 2026-07-13T15:42:00 C:\\proj\\report.txt\n\
                 {}2026-07-12T09:01:00 C:\\proj\\sub\n\
                 {}C:\\proj\\unknown.bin\n",
                s(9),
                s(15),
                s(35)
            )
        );
        assert_eq!(
            listing(&[Size]),
            format!(
                "{}4,096 C:\\proj\\report.txt\n{}C:\\proj\\sub\n{}C:\\proj\\unknown.bin\n",
                s(9),
                s(15),
                s(15)
            )
        );
        assert_eq!(
            listing(&[DateModified]),
            format!(
                "2026-07-13T15:42:00 C:\\proj\\report.txt\n\
                 2026-07-12T09:01:00 C:\\proj\\sub\n\
                 {}C:\\proj\\unknown.bin\n",
                s(20)
            )
        );
        assert_eq!(
            listing(&[DateModified, Size]),
            format!(
                "2026-07-13T15:42:00 {}4,096 C:\\proj\\report.txt\n\
                 2026-07-12T09:01:00 {}C:\\proj\\sub\n\
                 {}C:\\proj\\unknown.bin\n",
                s(9),
                s(15),
                s(35)
            )
        );
    }

    /// The overflow of a cell wider than its column is paid by the next
    /// cell's padding: u64::MAX is 12 characters too wide for Size, so the
    /// empty date after it pads 7 instead of 19.
    #[test]
    fn a_cell_wider_than_its_column_takes_from_the_next_padding() {
        use EsColumn::{DateModified, Size};
        let widest = format!("18,446,744,073,709,551,615{}C:\\x\n", " ".repeat(9));
        assert_eq!(
            line(&[Size, DateModified], &file(r"C:\x", Some(u64::MAX), None)),
            widest
        );
        // A cell that overflows last has nothing after it to pay.
        assert_eq!(
            line(
                &[Size, DateModified],
                &file(r"C:\x", Some(100_000_000_000), Some(DATE))
            ),
            format!("100,000,000,000 {DATE} C:\\x\n")
        );
        // The overflow is per line.
        let opts = PlainOptions {
            columns: vec![Size, DateModified],
            ..PlainOptions::default()
        };
        let mut out = Vec::new();
        write_plain_row(&opts, &file(r"C:\x", Some(u64::MAX), None), &mut out);
        out.clear();
        write_plain_row(&opts, &file(r"C:\y", Some(4096), None), &mut out);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("{}4,096 {} C:\\y\n", " ".repeat(9), " ".repeat(19))
        );
    }

    /// es's reading of the locale: the grouping's digits up to its first
    /// `0`, the separator as given, and `,` every three digits for a value
    /// that is missing.
    #[test]
    fn digit_grouping_reads_the_locale_as_es_does() {
        let grouped =
            |separator, grouping, n| DigitGrouping::from_locale(separator, grouping).format(n);
        assert_eq!(grouped(Some("."), Some("3;0"), 1_234_567), "1.234.567");
        assert_eq!(grouped(Some(","), Some("3;2;0"), 1_234_567), "12,34,567");
        assert_eq!(
            grouped(Some(","), Some("3;2;0"), 1_234_567_890),
            "1,23,45,67,890"
        );
        assert_eq!(grouped(Some(","), Some("0;0"), 1_234_567), "1234567");
        assert_eq!(grouped(None, None, 1_234_567), "1,234,567");
        // Any two digits are a first group and then groups of the second,
        // as Windows formats them for es; a longer value groups by 3.
        assert_eq!(grouped(Some("."), Some("4;5;0"), 1_234_567), "123.4567");
        assert_eq!(grouped(Some("."), Some("1;2;0"), 1_234_567), "12.34.56.7");
        assert_eq!(
            grouped(Some(","), Some("2;3;0"), 1_234_567_890),
            "12,345,678,90"
        );
        assert_eq!(grouped(Some(","), Some("3;2;1;0"), 1_234_567), "1,234,567");
        assert_eq!(grouped(Some(","), Some("4;0"), 123_456_789), "1,2345,6789");
        assert_eq!(DigitGrouping::es_default().format(999), "999");
        assert_eq!(DigitGrouping::es_default().format(0), "0");
        assert_eq!(DigitGrouping::es_default().format(1000), "1,000");

        // A separator of more than one byte pads by characters, as es pads
        // by UTF-16 units.
        let opts = PlainOptions {
            columns: vec![EsColumn::Size],
            grouping: DigitGrouping::from_locale(Some("\u{a0}"), Some("3;0")),
        };
        let mut out = Vec::new();
        write_plain_row(&opts, &file(r"C:\x", Some(1_234_567), None), &mut out);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "     1\u{a0}234\u{a0}567 C:\\x\n"
        );
    }
}
