//! USN journal FSCTLs: query/create the journal, bootstrap-enumerate the MFT,
//! and tail the journal. All record-bearing calls (`enum_usn_data`,
//! `read_usn_journal`) return the raw output bytes untouched: `goz-core`
//! parses them.

use windows_sys::Win32::Foundation::ERROR_HANDLE_EOF;

use crate::error::WinError;
use crate::ioctl::{
    CreateUsnJournalData, FSCTL_CREATE_USN_JOURNAL, FSCTL_ENUM_USN_DATA,
    FSCTL_GET_NTFS_VOLUME_DATA, FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, MftEnumDataV0,
    NtfsVolumeDataBuffer, ReadUsnJournalDataV0, UsnJournalDataV0, ioctl_in, ioctl_in_bytes,
    ioctl_out,
};
use crate::volume::VolumeHandle;

pub const ERROR_JOURNAL_DELETE_IN_PROGRESS: u32 = 1178;
pub const ERROR_JOURNAL_NOT_ACTIVE: u32 = 1179;

/// A `FSCTL_READ_USN_JOURNAL` whose `StartUsn` predates the journal's first
/// record fails with this code. The daemon detects it to trigger a full
/// re-enumeration.
pub const ERROR_JOURNAL_ENTRY_DELETED: u32 = 1181;

/// The live journal's identity and cursor bounds (from `USN_JOURNAL_DATA_V0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalInfo {
    pub journal_id: u64,
    pub first_usn: i64,
    pub next_usn: i64,
    pub lowest_valid_usn: i64,
    pub max_usn: i64,
    pub maximum_size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalQuery {
    Active(JournalInfo),
    NotActive,
    DeleteInProgress,
}

/// `FSCTL_QUERY_USN_JOURNAL`. Maps `ERROR_JOURNAL_NOT_ACTIVE` and
/// `ERROR_JOURNAL_DELETE_IN_PROGRESS` to their variants; other errors are
/// `Err`.
pub fn query_usn_journal(h: &VolumeHandle) -> Result<JournalQuery, WinError> {
    let mut data = UsnJournalDataV0::default();
    match ioctl_out(h.as_handle(), FSCTL_QUERY_USN_JOURNAL, &mut data) {
        Ok(_) => Ok(JournalQuery::Active(JournalInfo {
            journal_id: data.usn_journal_id,
            first_usn: data.first_usn,
            next_usn: data.next_usn,
            lowest_valid_usn: data.lowest_valid_usn,
            max_usn: data.max_usn,
            maximum_size: data.maximum_size,
        })),
        Err(ERROR_JOURNAL_NOT_ACTIVE) => Ok(JournalQuery::NotActive),
        Err(ERROR_JOURNAL_DELETE_IN_PROGRESS) => Ok(JournalQuery::DeleteInProgress),
        Err(code) => Err(WinError {
            code,
            context: "FSCTL_QUERY_USN_JOURNAL",
        }),
    }
}

/// How many file records the volume's MFT holds (`FSCTL_GET_NTFS_VOLUME_DATA`:
/// its valid data length over the record size), in use or free. The index
/// sizes its columns from it before a build.
pub fn mft_record_count(h: &VolumeHandle) -> Result<u64, WinError> {
    let mut data = NtfsVolumeDataBuffer::default();
    ioctl_out(h.as_handle(), FSCTL_GET_NTFS_VOLUME_DATA, &mut data).map_err(|code| WinError {
        code,
        context: "FSCTL_GET_NTFS_VOLUME_DATA",
    })?;
    Ok(match data.bytes_per_file_record_segment {
        0 => 0,
        size => data.mft_valid_data_length.max(0) as u64 / u64::from(size),
    })
}

/// `FSCTL_CREATE_USN_JOURNAL`. Creates the journal, or grows an existing one
/// (it never shrinks).
pub fn create_usn_journal(
    h: &VolumeHandle,
    maximum_size: u64,
    allocation_delta: u64,
) -> Result<(), WinError> {
    let input = CreateUsnJournalData {
        maximum_size,
        allocation_delta,
    };
    ioctl_in(h.as_handle(), FSCTL_CREATE_USN_JOURNAL, &input)
        .map(|_| ())
        .map_err(|code| WinError {
            code,
            context: "FSCTL_CREATE_USN_JOURNAL",
        })
}

/// `FSCTL_ENUM_USN_DATA`, one page. Fills `out` with the raw output (leading
/// u64 next-start-FRN + packed USN records) and returns `Some(bytes_written)`,
/// or `None` at end-of-enumeration (`ERROR_HANDLE_EOF`).
///
/// Pass `start_file_reference_number == 0` on the first call, then the leading
/// u64 of the previous output (extracted by `goz-core`) on each subsequent
/// call.
pub fn enum_usn_data(
    h: &VolumeHandle,
    start_file_reference_number: u64,
    out: &mut [u8],
) -> Result<Option<usize>, WinError> {
    let input = MftEnumDataV0 {
        start_file_reference_number,
        low_usn: 0,
        high_usn: i64::MAX,
    };
    match ioctl_in_bytes(h.as_handle(), FSCTL_ENUM_USN_DATA, &input, out) {
        Ok(bytes) => Ok(Some(bytes as usize)),
        Err(ERROR_HANDLE_EOF) => Ok(None),
        Err(code) => Err(WinError {
            code,
            context: "FSCTL_ENUM_USN_DATA",
        }),
    }
}

/// `FSCTL_READ_USN_JOURNAL`, one read. Fills `out` (a leading i64
/// next-start-USN followed by packed records) and returns the byte count.
/// `bytes_to_wait_for == 0` gives a non-blocking drain. A purged `start_usn`
/// surfaces as a [`WinError`] with `code == ERROR_JOURNAL_ENTRY_DELETED`.
pub fn read_usn_journal(
    h: &VolumeHandle,
    start_usn: i64,
    journal_id: u64,
    reason_mask: u32,
    bytes_to_wait_for: u64,
    out: &mut [u8],
) -> Result<usize, WinError> {
    let input = ReadUsnJournalDataV0 {
        start_usn,
        reason_mask,
        return_only_on_close: 0,
        timeout: 0,
        bytes_to_wait_for,
        usn_journal_id: journal_id,
    };
    ioctl_in_bytes(h.as_handle(), FSCTL_READ_USN_JOURNAL, &input, out)
        .map(|bytes| bytes as usize)
        .map_err(|code| WinError {
            code,
            context: "FSCTL_READ_USN_JOURNAL",
        })
}
