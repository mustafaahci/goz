//! The values of `size:` and the length functions, as Everything 1.5 reads
//! them: a number with an optional unit, compared by `<`, `<=`, `>`, `>=`,
//! `=` (`==`), or bounding a range `a..b` (`a-b`), or alone, when it names
//! every number its last written digit rounds to (`size:1kb` is
//! 1024..2047, `size:1.5kb` is 1536..1638); a size may also be one of
//! Everything's constants (`tiny`, `huge`, ...), `unknown`, and an empty
//! value is any number but zero. And the paths `parent:` and `ancestor:`
//! take.
//!
//! Every bound is exact: a value is the rational number its digits and unit
//! write, and each bound is the first or last whole number on its side, so
//! `size:>1.56kb` starts at 1598 bytes and `size:=1.56kb` matches nothing.

use super::parse::{QueryError, SizeRange};
use crate::fold::fold;

const KB: u64 = 1024;
const MB: u64 = 1024 * KB;

/// What one numeric value names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Number {
    /// The numbers in this range, which may be empty (`min > max`).
    Range(SizeRange),
    /// An unknown number: `size:unknown`.
    Unknown,
}

/// A number as written: `num / den`, and the unit of its last written digit,
/// `unit / den`.
#[derive(Clone, Copy)]
struct Scalar {
    num: u128,
    den: u128,
    unit: u128,
}

impl Scalar {
    fn ceil(self) -> u64 {
        clamp(self.num.div_ceil(self.den))
    }

    fn floor(self) -> u64 {
        clamp(self.num / self.den)
    }

    /// `self < other`, exactly.
    fn less(self, other: Scalar) -> bool {
        self.num.saturating_mul(other.den) < other.num.saturating_mul(self.den)
    }
}

fn clamp(n: u128) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// An empty range: matches no number.
const NONE: SizeRange = SizeRange { min: 1, max: 0 };

/// Reads one value of a numeric function (`what`, for errors). `sizes`:
/// the value is a size, which takes units and constants; `metric`: its
/// units are 1000 bytes, not 1024 (`metric:`).
pub(super) fn number(
    value: &str,
    what: &'static str,
    sizes: bool,
    metric: bool,
) -> Result<Number, QueryError> {
    let v = value.trim();
    let lower = v.to_ascii_lowercase();
    let malformed = |detail: String| QueryError::Malformed { what, detail };
    let range = |min, max| Ok(Number::Range(SizeRange { min, max }));
    match lower.as_str() {
        // An empty value is any number but zero, as in Everything.
        "" => return range(1, u64::MAX),
        "unknown" => return Ok(Number::Unknown),
        _ => {}
    }
    if sizes {
        // Everything's constants, bounds as its own syntax help states them.
        let constant = match lower.as_str() {
            "empty" => Some((0, 0)),
            "tiny" => Some((1, 10 * KB)),
            "small" => Some((10 * KB + 1, 100 * KB)),
            "medium" => Some((100 * KB + 1, MB)),
            "large" => Some((MB + 1, 16 * MB)),
            "huge" => Some((16 * MB + 1, 128 * MB)),
            "gigantic" => Some((128 * MB + 1, u64::MAX)),
            _ => None,
        };
        if let Some((min, max)) = constant {
            return range(min, max);
        }
    }
    let scalar = |text: &str| {
        scalar(text.trim(), sizes, metric).ok_or_else(|| {
            malformed(if sizes {
                format!(
                    "{text} is not a size; use a number with an optional unit (b, kb, mb, gb, tb), \
                     a comparison such as >1mb, a range such as 1mb..2mb, empty, tiny, small, \
                     medium, large, huge, gigantic or unknown"
                )
            } else {
                format!(
                    "{text} is not a length; use a whole number, a comparison such as >20 or a \
                     range such as 5..10"
                )
            })
        })
    };
    // Two-character operators before their one-character prefixes.
    for op in ["<=", ">=", "==", "<", ">", "="] {
        if let Some(rest) = v.strip_prefix(op) {
            let s = scalar(rest)?;
            return Ok(Number::Range(match op {
                "<=" => SizeRange {
                    min: 0,
                    max: s.floor(),
                },
                ">=" => SizeRange {
                    min: s.ceil(),
                    max: u64::MAX,
                },
                "<" => match s.ceil().checked_sub(1) {
                    Some(max) => SizeRange { min: 0, max },
                    None => NONE,
                },
                ">" => match s.floor().checked_add(1) {
                    Some(min) => SizeRange { min, max: u64::MAX },
                    None => NONE,
                },
                // `=` and `==`: exactly the value, which a fraction never is.
                _ if s.num % s.den == 0 => SizeRange {
                    min: s.floor(),
                    max: s.floor(),
                },
                _ => NONE,
            }));
        }
    }
    let bounds = v.split_once("..").or_else(|| {
        // `a-b` is a range too: a number never holds a `-`.
        v.split_once('-')
            .filter(|(a, b)| !a.trim().is_empty() && !b.trim().is_empty())
    });
    if let Some((lower, upper)) = bounds {
        let lower = (!lower.trim().is_empty())
            .then(|| scalar(lower))
            .transpose()?;
        let upper = (!upper.trim().is_empty())
            .then(|| scalar(upper))
            .transpose()?;
        // A reversed range is read the right way round, as Everything does
        // with dates.
        let (lower, upper) = match (lower, upper) {
            (Some(a), Some(b)) if b.less(a) => (Some(b), Some(a)),
            other => other,
        };
        let min = lower.map_or(0, Scalar::ceil);
        let max = upper.map_or(u64::MAX, Scalar::floor);
        return range(min, max);
    }
    // A value alone names every number its last written digit rounds to:
    // [value, value + one unit of that digit).
    let s = scalar(v)?;
    let end = Scalar {
        num: s.num.saturating_add(s.unit),
        ..s
    };
    match end.ceil().checked_sub(1) {
        Some(max) if s.ceil() <= max => range(s.ceil(), max),
        _ => Ok(Number::Range(NONE)),
    }
}

/// Reads `text`, digits with at most one `.` and, for a size, a unit.
fn scalar(text: &str, sizes: bool, metric: bool) -> Option<Scalar> {
    let lower = text.to_ascii_lowercase();
    let digits_end = lower
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(lower.len());
    let (number, unit) = lower.split_at(digits_end);
    let k: u128 = if metric { 1000 } else { 1024 };
    let unit: u128 = match unit.trim() {
        "" => 1,
        _ if !sizes => return None,
        "b" => 1,
        "k" | "kb" => k,
        "kib" => 1024,
        "m" | "mb" => k.pow(2),
        "mib" => 1024u128.pow(2),
        "g" | "gb" => k.pow(3),
        "gib" => 1024u128.pow(3),
        "t" | "tb" => k.pow(4),
        "tib" => 1024u128.pow(4),
        _ => return None,
    };
    let (whole, fraction) = match number.split_once('.') {
        Some((whole, fraction)) if sizes => (whole, fraction),
        Some(_) => return None,
        None => (number, ""),
    };
    if whole.is_empty() && fraction.is_empty() {
        return None;
    }
    // More digits than any size needs only pile up zeros: 39 fit a u128.
    if whole.len() + fraction.len() > 30 {
        return None;
    }
    let mantissa: u128 = format!("{whole}{fraction}").parse().ok()?;
    let den = 10u128.pow(fraction.len() as u32);
    Some(Scalar {
        num: mantissa.checked_mul(unit)?,
        den,
        unit,
    })
}

/// An `ext:` value as Everything reads it: a leading `*.` or `.` dropped
/// (`ext:.txt` and `ext:*.txt` are `ext:txt`, and `ext:*.*` and `ext:.*`
/// are `ext:*`, every file, L7:529, L8:115); what is left may be a
/// wildcard (`ext:*txt`, L1:244).
pub(super) fn ext_value(item: &[u8]) -> &[u8] {
    item.strip_prefix(b"*.")
        .or_else(|| item.strip_prefix(b"."))
        .unwrap_or(item)
}

/// A `parent:` or `ancestor:` value: the absolute path of a folder, folded,
/// `/` written as `\`, with no separator at its end. A relative path names
/// no folder, so it is refused.
pub(super) fn folder_path(item: &[u8], what: &'static str) -> Result<Vec<u8>, QueryError> {
    let mut path: Vec<u8> = item
        .iter()
        .map(|&b| if b == b'/' { b'\\' } else { b })
        .collect();
    while path.len() > 2 && path.last() == Some(&b'\\') {
        path.pop();
    }
    if path.len() == 3 && path.ends_with(b":\\") {
        path.pop();
    }
    let drive = path.len() >= 2
        && path[0].is_ascii_alphabetic()
        && path[1] == b':'
        && (path.len() == 2 || path[2] == b'\\');
    if !drive && !path.starts_with(b"\\\\") {
        return Err(QueryError::Malformed {
            what,
            detail: format!(
                "{what} takes a folder's absolute path, such as {what}C:\\Windows; got {}",
                String::from_utf8_lossy(item)
            ),
        });
    }
    Ok(fold(&path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(v: &str) -> (u64, u64) {
        match number(v, "size:", true, false).unwrap() {
            Number::Range(r) => (r.min, r.max),
            Number::Unknown => panic!("{v}"),
        }
    }

    /// Everything's own answers for its fixture's sizes (DESIGN.md §2.4:
    /// L1:264–303, L3:35–37), and its syntax help's constants.
    #[test]
    fn sizes_read_as_everything_reads_them() {
        assert_eq!(size("1kb"), (1024, 2047), "the granule of the last digit");
        assert_eq!(size("1KB"), (1024, 2047));
        assert_eq!(size("1k"), (1024, 2047));
        assert_eq!(size("1kib"), (1024, 2047));
        assert_eq!(size("1.5kb"), (1536, 1638));
        assert_eq!(size("1.56kb"), (1598, 1607));
        assert_eq!(size("1.6kb"), (1639, 1740));
        assert_eq!(size("1.5mb"), (1_572_864, 1_677_721));
        assert_eq!(size("1mb"), (1_048_576, 2_097_151));
        assert_eq!(size("2mb"), (2_097_152, 3_145_727));
        assert_eq!(size("1024"), (1024, 1024), "bytes are exact");
        assert_eq!(size("0"), (0, 0));
        assert_eq!(size("1b"), (1, 1));
        assert_eq!(size("=1kb"), (1024, 1024));
        assert_eq!(size("==1kb"), (1024, 1024));
        assert_eq!(size("=1.5kb"), (1536, 1536));
        assert_eq!(size("=1.56kb"), (1, 0), "no whole size");
        assert_eq!(size(">1kb"), (1025, u64::MAX));
        assert_eq!(size(">=1kb"), (1024, u64::MAX));
        assert_eq!(size("<1kb"), (0, 1023));
        assert_eq!(size("<=1kb"), (0, 1024));
        assert_eq!(size(">1.56kb"), (1598, u64::MAX));
        assert_eq!(size("<1.56kb"), (0, 1597));
        assert_eq!(size("<0"), (1, 0));
        assert_eq!(size("1kb..2kb"), (1024, 2048), "a range's bounds are exact");
        assert_eq!(size("1kb-2kb"), (1024, 2048));
        assert_eq!(size("2kb..1kb"), (1024, 2048), "reversed");
        assert_eq!(size("2kb.."), (2048, u64::MAX));
        assert_eq!(size("..1kb"), (0, 1024));
        assert_eq!(size("10kb..1mb"), (10_240, 1_048_576));
        assert_eq!(size(""), (1, u64::MAX), "an empty value is not zero");
        assert_eq!(size(".."), (0, u64::MAX), "any known size");
        assert_eq!(size("empty"), (0, 0));
        assert_eq!(size("tiny"), (1, 10_240), "tiny leaves out empty files");
        assert_eq!(size("small"), (10_241, 102_400));
        assert_eq!(size("medium"), (102_401, 1_048_576));
        assert_eq!(size("large"), (1_048_577, 16_777_216));
        assert_eq!(size("huge"), (16_777_217, 134_217_728));
        assert_eq!(size("GIGANTIC"), (134_217_729, u64::MAX));
        assert_eq!(size("1gb"), (1 << 30, (2 << 30) - 1));
        assert_eq!(size("1tb"), (1 << 40, (2 << 40) - 1));
        assert_eq!(size("99999999999999999999tb"), (1, 0), "past every size");
        assert_eq!(number("unknown", "size:", true, false), Ok(Number::Unknown));
        let metric = |v| match number(v, "size:", true, true).unwrap() {
            Number::Range(r) => (r.min, r.max),
            Number::Unknown => panic!(),
        };
        assert_eq!(metric("=1kb"), (1000, 1000));
        assert_eq!(metric("1kb"), (1000, 1999));
        assert_eq!(metric("=1kib"), (1024, 1024));
        for bad in [
            "abc", "1xb", "1..x", ">", "1.2.3", "1-", "kb", ".", "1e5", "-5",
        ] {
            assert!(
                matches!(
                    number(bad, "size:", true, false),
                    Err(QueryError::Malformed { what: "size:", .. })
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn lengths_are_whole_numbers() {
        let len = |v| match number(v, "len:", false, false).unwrap() {
            Number::Range(r) => (r.min, r.max),
            Number::Unknown => panic!(),
        };
        assert_eq!(len("7"), (7, 7));
        assert_eq!(len(">20"), (21, u64::MAX));
        assert_eq!(len("7..10"), (7, 10));
        assert_eq!(len(""), (1, u64::MAX));
        for bad in ["1kb", "7.5", "abc", "tiny"] {
            assert!(number(bad, "len:", false, false).is_err(), "{bad}");
        }
    }

    #[test]
    fn ext_values_drop_a_leading_dot() {
        assert_eq!(ext_value(b".txt"), b"txt");
        assert_eq!(ext_value(b"*.txt"), b"txt");
        assert_eq!(ext_value(b"*txt"), b"*txt", "a wildcard ending in txt");
        assert_eq!(ext_value(b"*.*"), b"*");
        assert_eq!(ext_value(b".*"), b"*");
        assert_eq!(ext_value(b"*"), b"*");
        assert_eq!(ext_value(b"tar.gz"), b"tar.gz");
        assert_eq!(ext_value(b"mp*"), b"mp*");
        assert_eq!(ext_value(b""), b"");
    }

    #[test]
    fn folder_paths_are_absolute_and_folded() {
        let ok =
            |v: &str| String::from_utf8(folder_path(v.as_bytes(), "parent:").unwrap()).unwrap();
        assert_eq!(ok(r"C:\Windows"), r"c:\windows");
        assert_eq!(ok(r"C:\Windows\"), r"c:\windows");
        assert_eq!(ok("C:/Windows/"), r"c:\windows");
        assert_eq!(ok(r"C:\"), "c:");
        assert_eq!(ok("C:"), "c:");
        assert_eq!(ok(r"\\server\share\x"), r"\\server\share\x");
        for bad in ["windows", r"w\windows", "", r"\windows", "C", "1:", r"C:x"] {
            assert!(
                matches!(
                    folder_path(bad.as_bytes(), "parent:"),
                    Err(QueryError::Malformed {
                        what: "parent:",
                        ..
                    })
                ),
                "{bad}"
            );
        }
    }
}
