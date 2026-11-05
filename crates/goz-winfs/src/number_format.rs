//! The digit grouping of the calling account's regional format, which es
//! reads for the sizes it prints.

use core::ptr::null;

use windows_sys::Win32::Globalization::{GetLocaleInfoEx, LOCALE_SGROUPING, LOCALE_STHOUSAND};

/// The digit grouping of a regional format, as the raw locale strings, so
/// that their meaning is read where it is used. `None` where a value cannot
/// be read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NumberFormat {
    /// What separates the digit groups (`LOCALE_STHOUSAND`), such as `,`.
    pub thousand_separator: Option<String>,
    /// The sizes of the digit groups from the right (`LOCALE_SGROUPING`),
    /// such as `3;0`.
    pub grouping: Option<String>,
}

/// The digit grouping of the calling account's regional format, the values
/// es reads for its Size column.
pub fn number_format() -> NumberFormat {
    NumberFormat {
        thousand_separator: locale_text(LOCALE_STHOUSAND),
        grouping: locale_text(LOCALE_SGROUPING),
    }
}

/// One text value of the calling account's locale, or `None` when it
/// cannot be read. Both values above fit 32 UTF-16 units with room to
/// spare: Windows caps the separator at 4 and the grouping at 10.
fn locale_text(kind: u32) -> Option<String> {
    let mut buf = [0u16; 32];
    // SAFETY: plain Win32 FFI writing at most `buf.len()` UTF-16 units into
    // the local buffer it is given; a null name is the calling account's
    // default locale.
    let written = unsafe { GetLocaleInfoEx(null(), kind, buf.as_mut_ptr(), buf.len() as i32) };
    // The count includes the terminating null; 0 is a failure.
    let len = usize::try_from(written).ok()?.checked_sub(1)?;
    String::from_utf16(buf.get(..len)?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The account's format reads: a separator of at most three
    /// characters, and the group sizes as single digits split by `;`.
    #[test]
    fn the_account_format_reads() {
        let format = number_format();
        let separator = format.thousand_separator.expect("LOCALE_STHOUSAND");
        assert!(separator.chars().count() <= 3, "{separator:?}");
        let grouping = format.grouping.expect("LOCALE_SGROUPING");
        assert!(
            grouping
                .split(';')
                .all(|size| size.len() == 1 && size.as_bytes()[0].is_ascii_digit()),
            "{grouping:?}"
        );
    }
}
