//! Volume discovery and owned volume handles.
//!
//! `enumerate_volumes` classifies every volume on the machine; the daemon
//! keeps the ones that are `is_fixed && is_ntfs`. `open_volume` turns a volume
//! GUID path into a `\\.\`-style device handle usable with the journal/MFT
//! FSCTLs.

use core::ptr;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_MORE_DATA, ERROR_NO_MORE_FILES, GENERIC_READ, GetLastError, HANDLE,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_READ, FILE_SHARE_WRITE, FindFirstVolumeW,
    FindNextVolumeW, FindVolumeClose, GetDiskFreeSpaceExW, GetDriveTypeW, GetVolumeInformationW,
    GetVolumePathNamesForVolumeNameW, OPEN_EXISTING,
};

use crate::error::{WinError, last_error};
use crate::util::{from_wide_nul, to_wide_nul};

/// `GetDriveType` return value for a fixed disk (winbase.h `DRIVE_FIXED`).
/// Defined locally so we needn't pull in `Win32_System_WindowsProgramming`.
const DRIVE_FIXED: u32 = 3;

/// One classified volume. The daemon filters on `is_fixed && is_ntfs`.
#[derive(Debug, Clone)]
pub struct VolumeInfo {
    /// `\\?\Volume{GUID}\` with the trailing backslash, exactly as returned.
    pub guid_path: String,
    /// Mount points, e.g. `["C:\\"]`; may be empty (letterless volume).
    pub mounts: Vec<String>,
    /// `GetDriveType == DRIVE_FIXED`.
    pub is_fixed: bool,
    /// `GetVolumeInformation` filesystem name is `"NTFS"`.
    pub is_ntfs: bool,
}

/// Enumerates every volume via `FindFirstVolumeW`/`FindNextVolumeW`,
/// classifying each. Ordering is not guaranteed by the OS.
pub fn enumerate_volumes() -> Result<Vec<VolumeInfo>, WinError> {
    let mut buf = [0u16; 260];
    // SAFETY: `buf` provides 260 wide slots; we pass that as the capacity.
    let find = unsafe { FindFirstVolumeW(buf.as_mut_ptr(), buf.len() as u32) };
    if find == INVALID_HANDLE_VALUE {
        return Err(last_error("FindFirstVolumeW"));
    }

    let mut volumes = Vec::new();
    let mut error: Option<WinError> = None;
    loop {
        // `buf` currently holds a NUL-terminated volume GUID path.
        volumes.push(classify_volume(&buf));

        // SAFETY: `find` is a live volume-search handle; `buf` is writable for
        // its capacity.
        if unsafe { FindNextVolumeW(find, buf.as_mut_ptr(), buf.len() as u32) } == 0 {
            // SAFETY: GetLastError has no preconditions; captured before any
            // other call can clobber it.
            let code = unsafe { GetLastError() };
            if code != ERROR_NO_MORE_FILES {
                error = Some(WinError {
                    code,
                    context: "FindNextVolumeW",
                });
            }
            break;
        }
    }

    // SAFETY: `find` is a live search handle owned here; closed exactly once.
    unsafe { FindVolumeClose(find) };

    match error {
        Some(e) => Err(e),
        None => Ok(volumes),
    }
}

/// Classifies a single volume from its NUL-terminated GUID path buffer. All
/// probing calls are best-effort: on failure the flag is `false`.
fn classify_volume(guid_wide: &[u16]) -> VolumeInfo {
    let guid_path = from_wide_nul(guid_wide);
    let guid_ptr = guid_wide.as_ptr();

    // SAFETY: `guid_ptr` points to the NUL-terminated volume GUID path.
    let drive_type = unsafe { GetDriveTypeW(guid_ptr) };
    let is_fixed = drive_type == DRIVE_FIXED;

    let mut fs_name = [0u16; 16];
    // SAFETY: `guid_ptr` is NUL-terminated; the filesystem-name buffer holds
    // `fs_name.len()` wide chars; the other out params are null (not wanted).
    let ok = unsafe {
        GetVolumeInformationW(
            guid_ptr,
            ptr::null_mut(),
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            fs_name.as_mut_ptr(),
            fs_name.len() as u32,
        )
    };
    let is_ntfs = ok != 0 && from_wide_nul(&fs_name) == "NTFS";

    let mounts = query_mount_points(guid_ptr);

    VolumeInfo {
        guid_path,
        mounts,
        is_fixed,
        is_ntfs,
    }
}

/// Resolves a volume's mount points (drive letters + mounted folders). The API
/// returns a double-NUL-terminated `MULTI_SZ`; on any failure we return empty.
fn query_mount_points(guid_ptr: *const u16) -> Vec<String> {
    let mut buf = vec![0u16; 260];
    let mut needed: u32 = 0;
    // SAFETY: `guid_ptr` is NUL-terminated; `buf` is writable for its length;
    // `needed` is a live out param.
    let ok = unsafe {
        GetVolumePathNamesForVolumeNameW(guid_ptr, buf.as_mut_ptr(), buf.len() as u32, &mut needed)
    };
    if ok == 0 {
        // SAFETY: GetLastError has no preconditions.
        if unsafe { GetLastError() } != ERROR_MORE_DATA {
            return Vec::new();
        }
        buf = vec![0u16; needed as usize];
        // SAFETY: as above, with the buffer grown to the required size.
        let ok = unsafe {
            GetVolumePathNamesForVolumeNameW(
                guid_ptr,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut needed,
            )
        };
        if ok == 0 {
            return Vec::new();
        }
    }
    split_multi_sz(&buf)
}

/// Splits a `MULTI_SZ` (NUL-separated strings, terminated by an empty string).
fn split_multi_sz(buf: &[u16]) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, &c) in buf.iter().enumerate() {
        if c == 0 {
            if i == start {
                break; // empty string terminates the list
            }
            out.push(String::from_utf16_lossy(&buf[start..i]));
            start = i + 1;
        }
    }
    out
}

/// An owned volume handle; `CloseHandle` on drop.
pub struct VolumeHandle(HANDLE);

impl VolumeHandle {
    /// The underlying Win32 `HANDLE` as an integer (diagnostics / FFI).
    pub fn raw(&self) -> isize {
        self.0 as isize
    }

    /// The raw `HANDLE`, for this crate's own FSCTL calls.
    pub(crate) fn as_handle(&self) -> HANDLE {
        self.0
    }
}

impl Drop for VolumeHandle {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from a successful CreateFileW and is owned by
        // this value; it is closed exactly once, here.
        unsafe { CloseHandle(self.0) };
    }
}

// SAFETY: a Win32 HANDLE is a process-wide value with no thread affinity, so a
// VolumeHandle can be moved to another thread (e.g. the daemon's journal-tail
// thread). It is intentionally not `Sync`: concurrent synchronous
// DeviceIoControl on one handle is not supported.
unsafe impl Send for VolumeHandle {}

#[cfg(test)]
impl VolumeHandle {
    /// Takes over `file` as an `OpenFileById` volume hint.
    pub(crate) fn from_file(file: std::fs::File) -> Self {
        use std::os::windows::io::IntoRawHandle;
        VolumeHandle(file.into_raw_handle() as HANDLE)
    }
}

/// Opens a volume for FSCTL use from its GUID path (as returned by
/// [`enumerate_volumes`]). The trailing backslash is stripped first: opening
/// `\\?\Volume{GUID}\` yields the root directory, not the volume device.
///
/// `ERROR_ACCESS_DENIED` surfaces as a [`WinError`] so the daemon can map it to
/// a "run elevated" message.
pub fn open_volume(guid_path: &str) -> Result<VolumeHandle, WinError> {
    let trimmed = guid_path.strip_suffix('\\').unwrap_or(guid_path);
    let wide = to_wide_nul(trimmed);
    // SAFETY: `wide` is a NUL-terminated path; no security attributes and no
    // template handle are supplied.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(last_error("CreateFileW (open_volume)"));
    }
    Ok(VolumeHandle(handle))
}

/// Total size of the volume in bytes (`GetDiskFreeSpaceExW` on its GUID path,
/// which works for letterless volumes too). Used to size the USN journal in
/// proportion to the volume.
pub fn volume_total_bytes(guid_path: &str) -> Result<u64, WinError> {
    let wide = to_wide_nul(guid_path);
    let mut total: u64 = 0;
    // SAFETY: `wide` is a NUL-terminated root path; the two unused out-params
    // are null, which the API permits, and `total` outlives the call.
    let ok =
        unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), ptr::null_mut(), &mut total, ptr::null_mut()) };
    if ok == 0 {
        return Err(last_error("GetDiskFreeSpaceExW"));
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::{enumerate_volumes, split_multi_sz};

    #[test]
    fn enumerate_volumes_succeeds() {
        let volumes = enumerate_volumes().expect("enumerate_volumes should succeed");
        for v in &volumes {
            assert!(!v.guid_path.is_empty());
        }
    }

    #[test]
    fn split_multi_sz_parses_double_null_list() {
        // "C:\\\0X:\\\0\0"
        let wide: Vec<u16> = "C:\\\u{0}X:\\\u{0}\u{0}".encode_utf16().collect();
        assert_eq!(
            split_multi_sz(&wide),
            vec!["C:\\".to_string(), "X:\\".to_string()]
        );
    }
}
