//! FSCTL control codes, ioctl input/output structs, and the `DeviceIoControl`
//! plumbing. All of it is crate-private: the public modules (`journal`,
//! `file_layout`) build the typed inputs and hand `DeviceIoControl` a raw byte
//! slice for METHOD_NEITHER FSCTLs. This crate never interprets the record
//! payloads that come back. That is `goz-core`'s job.
//!
//! We define the FSCTL codes and structs ourselves rather than relying on
//! `windows-sys` so the wire layout is pinned here, next to the docs it
//! mirrors.

use core::ffi::c_void;
use core::ptr;

use windows_sys::Win32::Foundation::{GetLastError, HANDLE};
use windows_sys::Win32::System::IO::DeviceIoControl;

// --- CTL_CODE machinery (winioctl.h) -------------------------------------

const FILE_DEVICE_FILE_SYSTEM: u32 = 0x9;
const METHOD_BUFFERED: u32 = 0;
const METHOD_NEITHER: u32 = 3;
const FILE_ANY_ACCESS: u32 = 0;

/// The `CTL_CODE` macro: `(device << 16) | (access << 14) | (function << 2) | method`.
pub(crate) const fn ctl_code(device: u32, function: u32, method: u32, access: u32) -> u32 {
    (device << 16) | (access << 14) | (function << 2) | method
}

pub(crate) const FSCTL_ENUM_USN_DATA: u32 =
    ctl_code(FILE_DEVICE_FILE_SYSTEM, 44, METHOD_NEITHER, FILE_ANY_ACCESS);
pub(crate) const FSCTL_QUERY_USN_JOURNAL: u32 = ctl_code(
    FILE_DEVICE_FILE_SYSTEM,
    61,
    METHOD_BUFFERED,
    FILE_ANY_ACCESS,
);
/// `METHOD_NEITHER`, like the other journal controls. It was `METHOD_BUFFERED`
/// for months: NTFS does not recognise that code at all and answers
/// `STATUS_INVALID_DEVICE_REQUEST` (Win32 error 1), so every journal create
/// and grow silently failed. The unit test pins the header value.
pub(crate) const FSCTL_CREATE_USN_JOURNAL: u32 =
    ctl_code(FILE_DEVICE_FILE_SYSTEM, 57, METHOD_NEITHER, FILE_ANY_ACCESS);
pub(crate) const FSCTL_READ_USN_JOURNAL: u32 =
    ctl_code(FILE_DEVICE_FILE_SYSTEM, 46, METHOD_NEITHER, FILE_ANY_ACCESS);
pub(crate) const FSCTL_QUERY_FILE_LAYOUT: u32 = ctl_code(
    FILE_DEVICE_FILE_SYSTEM,
    157,
    METHOD_NEITHER,
    FILE_ANY_ACCESS,
);
pub(crate) const FSCTL_GET_NTFS_VOLUME_DATA: u32 = ctl_code(
    FILE_DEVICE_FILE_SYSTEM,
    25,
    METHOD_BUFFERED,
    FILE_ANY_ACCESS,
);

// --- ioctl structs -------------------------------------------------------
//
// These are `#[repr(C)]` mirrors of Win32 structs. The kernel reads (inputs)
// or writes (outputs) their fields through the pointer we hand
// `DeviceIoControl`; Rust never reads most input fields, so `dead_code` would
// otherwise fire on them.

/// Win32 `MFT_ENUM_DATA_V0`, input to `FSCTL_ENUM_USN_DATA`.
#[repr(C)]
#[allow(dead_code)] // fields are consumed by the kernel via the ioctl pointer
pub(crate) struct MftEnumDataV0 {
    pub start_file_reference_number: u64,
    pub low_usn: i64,
    pub high_usn: i64,
}

/// Win32 `READ_USN_JOURNAL_DATA_V0`, input to `FSCTL_READ_USN_JOURNAL`.
#[repr(C)]
#[allow(dead_code)] // fields are consumed by the kernel via the ioctl pointer
pub(crate) struct ReadUsnJournalDataV0 {
    pub start_usn: i64,
    pub reason_mask: u32,
    pub return_only_on_close: u32,
    pub timeout: u64,
    pub bytes_to_wait_for: u64,
    pub usn_journal_id: u64,
}

/// Win32 `CREATE_USN_JOURNAL_DATA`, input to `FSCTL_CREATE_USN_JOURNAL`.
#[repr(C)]
#[allow(dead_code)] // fields are consumed by the kernel via the ioctl pointer
pub(crate) struct CreateUsnJournalData {
    pub maximum_size: u64,
    pub allocation_delta: u64,
}

/// Win32 `USN_JOURNAL_DATA_V0`, output of `FSCTL_QUERY_USN_JOURNAL`.
///
/// `allocation_delta` is populated by the driver but unused by `JournalInfo`.
#[repr(C)]
#[derive(Default)]
#[allow(dead_code)] // some fields are written by the kernel but unused Rust-side
pub(crate) struct UsnJournalDataV0 {
    pub usn_journal_id: u64,
    pub first_usn: i64,
    pub next_usn: i64,
    pub lowest_valid_usn: i64,
    pub max_usn: i64,
    pub maximum_size: u64,
    pub allocation_delta: u64,
}

/// Win32 `NTFS_VOLUME_DATA_BUFFER`, output of `FSCTL_GET_NTFS_VOLUME_DATA`.
#[repr(C)]
#[derive(Default)]
#[allow(dead_code)] // written by the kernel; only the MFT fields are read
pub(crate) struct NtfsVolumeDataBuffer {
    pub volume_serial_number: i64,
    pub number_sectors: i64,
    pub total_clusters: i64,
    pub free_clusters: i64,
    pub total_reserved: i64,
    pub bytes_per_sector: u32,
    pub bytes_per_cluster: u32,
    pub bytes_per_file_record_segment: u32,
    pub clusters_per_file_record_segment: u32,
    pub mft_valid_data_length: i64,
    pub mft_start_lcn: i64,
    pub mft2_start_lcn: i64,
    pub mft_zone_start: i64,
    pub mft_zone_end: i64,
}

/// Win32 `QUERY_FILE_LAYOUT_INPUT`, input to `FSCTL_QUERY_FILE_LAYOUT`.
///
/// Field order is exactly the ntifs.h header: `FilterEntryCount`
/// (a.k.a. `NumberOfPairs`), then `Flags`, then `FilterType`, then
/// `Reserved` (must be 0), then the 16-byte filter union. `Flags` precedes
/// `FilterType`: transposing them makes the driver read the flags as the
/// filter type (and vice versa), which fails every call with
/// `ERROR_INVALID_PARAMETER` (87). Four `u32`s pack with no padding, so the
/// `[u64; 2]` union lands at offset 16 and the struct is 32 bytes.
#[repr(C)]
#[allow(dead_code)] // fields are consumed by the kernel via the ioctl pointer
pub(crate) struct QueryFileLayoutInput {
    pub filter_entry_count: u32,
    pub flags: u32,
    pub filter_type: u32,
    pub reserved: u32,
    /// Unused for a NONE full-volume scan.
    pub filter: [u64; 2],
}

// --- DeviceIoControl plumbing --------------------------------------------

/// Issues one synchronous `DeviceIoControl`, returning bytes-written on success
/// or the raw `GetLastError()` code on failure (captured before it can be
/// clobbered). Callers map the code to the right domain outcome.
///
/// # Safety
/// `handle` must be a live device handle. `in_ptr`/`in_size` and
/// `out_ptr`/`out_size` must describe valid readable and writable regions
/// respectively (a null pointer requires a zero size).
unsafe fn device_io_control(
    handle: HANDLE,
    control_code: u32,
    in_ptr: *const c_void,
    in_size: u32,
    out_ptr: *mut c_void,
    out_size: u32,
) -> Result<u32, u32> {
    let mut bytes_returned: u32 = 0;
    // SAFETY: preconditions are the caller's responsibility (see fn docs). No
    // OVERLAPPED is supplied, so the call is synchronous and `bytes_returned`
    // (a live local) is written on success.
    let ok = unsafe {
        DeviceIoControl(
            handle,
            control_code,
            in_ptr,
            in_size,
            out_ptr,
            out_size,
            &mut bytes_returned,
            ptr::null_mut(),
        )
    };
    if ok != 0 {
        Ok(bytes_returned)
    } else {
        // SAFETY: GetLastError has no preconditions.
        Err(unsafe { GetLastError() })
    }
}

/// Typed input struct, raw byte output buffer (METHOD_NEITHER FSCTLs).
pub(crate) fn ioctl_in_bytes<In>(
    handle: HANDLE,
    control_code: u32,
    input: &In,
    out: &mut [u8],
) -> Result<u32, u32> {
    // SAFETY: `input` points to a live `In` of size_of::<In>() bytes; `out` is
    // a live writable slice of `out.len()` bytes; `handle` is caller-owned.
    unsafe {
        device_io_control(
            handle,
            control_code,
            ptr::from_ref(input).cast::<c_void>(),
            size_of::<In>() as u32,
            out.as_mut_ptr().cast::<c_void>(),
            out.len() as u32,
        )
    }
}

/// Typed input struct, no output buffer (e.g. `FSCTL_CREATE_USN_JOURNAL`).
pub(crate) fn ioctl_in<In>(handle: HANDLE, control_code: u32, input: &In) -> Result<u32, u32> {
    // SAFETY: `input` points to a live `In` of size_of::<In>() bytes; there is
    // no output buffer; `handle` is caller-owned.
    unsafe {
        device_io_control(
            handle,
            control_code,
            ptr::from_ref(input).cast::<c_void>(),
            size_of::<In>() as u32,
            ptr::null_mut(),
            0,
        )
    }
}

/// No input, a typed output struct (e.g. `FSCTL_QUERY_USN_JOURNAL`).
pub(crate) fn ioctl_out<Out>(handle: HANDLE, control_code: u32, out: &mut Out) -> Result<u32, u32> {
    // SAFETY: `out` points to a live `Out` of size_of::<Out>() bytes; there is
    // no input; `handle` is caller-owned.
    unsafe {
        device_io_control(
            handle,
            control_code,
            ptr::null(),
            0,
            ptr::from_mut(out).cast::<c_void>(),
            size_of::<Out>() as u32,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fsctl_codes_match_known_constants() {
        // Well-known winioctl.h values (verified against the SDK header).
        assert_eq!(FSCTL_ENUM_USN_DATA, 0x0009_00B3);
        assert_eq!(FSCTL_QUERY_USN_JOURNAL, 0x0009_00F4);
        assert_eq!(FSCTL_CREATE_USN_JOURNAL, 0x0009_00E7);
        assert_eq!(FSCTL_READ_USN_JOURNAL, 0x0009_00BB);
        assert_eq!(FSCTL_QUERY_FILE_LAYOUT, 0x0009_0277);
        assert_eq!(FSCTL_GET_NTFS_VOLUME_DATA, 0x0009_0064);
        assert_eq!(size_of::<NtfsVolumeDataBuffer>(), 96);
    }

    #[test]
    fn ctl_code_matches_macro_expansion() {
        // CTL_CODE(9, 44, METHOD_NEITHER, FILE_ANY_ACCESS)
        assert_eq!(ctl_code(9, 44, 3, 0), (9 << 16) | (44 << 2) | 3);
        // access bits land at position 14
        assert_eq!(ctl_code(0, 0, 0, 1), 1 << 14);
    }
}
