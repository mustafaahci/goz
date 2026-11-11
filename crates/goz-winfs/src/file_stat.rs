//! Read one file's current size, dates and attributes by its FRN, for the
//! live enricher, and its basic information for a query that reads a column
//! the index does not hold. USN change records carry neither size nor
//! timestamps, so a file created or modified after bootstrap needs an
//! out-of-band stat to refresh them (otherwise its size stays "unknown",
//! which downstream renders as a directory).

use core::ffi::c_void;
use core::mem::{size_of, zeroed};

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_DESCRIPTOR, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FileBasicInfo, GetFileInformationByHandle,
    GetFileInformationByHandleEx, OPEN_EXISTING, OpenFileById,
};

use crate::error::{WinError, last_error};
use crate::util::to_wide_nul;
use crate::volume::VolumeHandle;

/// `FILE_ATTRIBUTE_DIRECTORY`, defined locally per this crate's convention.
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
/// `FileIdType`: a 64-bit NTFS file reference number in `FILE_ID_DESCRIPTOR`.
const FILE_ID_TYPE_FILE: i32 = 0;

/// The live size, dates and attributes of one file.
pub struct FileStat {
    pub size: u64,
    /// Last-write time as a Windows `FILETIME` packed into `i64` (100 ns ticks
    /// since 1601), the same representation the FILE_LAYOUT path produces.
    pub mtime_ft: i64,
    pub is_dir: bool,
    /// Creation time, packed as `mtime_ft`.
    pub ctime_ft: i64,
    /// Last-access time, packed as `mtime_ft`.
    pub atime_ft: i64,
    /// `FILE_ATTRIBUTE_*` bits.
    pub attributes: u32,
}

/// Opens the file identified by `frn` on the volume of `hint` for its
/// attributes, without following a reparse point; `None` when it cannot be
/// opened (already deleted, or transiently locked).
///
/// # Safety
///
/// `hint` must be a live handle on a file or folder of the volume.
unsafe fn open_by_id(hint: HANDLE, frn: u64) -> Option<HANDLE> {
    // SAFETY: `desc` is fully initialized before use; the caller keeps
    // `hint` alive for the call.
    let handle = unsafe {
        let mut desc: FILE_ID_DESCRIPTOR = zeroed();
        desc.dwSize = size_of::<FILE_ID_DESCRIPTOR>() as u32;
        desc.Type = FILE_ID_TYPE_FILE;
        desc.Anonymous.FileId = frn as i64;
        OpenFileById(
            hint,
            &desc,
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            core::ptr::null(),
            // Backup semantics also open directories. A symbolic link or a
            // junction opens as itself, the record FILE_LAYOUT read at
            // bootstrap; without the flag its open by ID fails with
            // ERROR_INVALID_PARAMETER, which reads as gone.
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
        )
    };
    (handle != INVALID_HANDLE_VALUE && !handle.is_null()).then_some(handle)
}

fn packed(high: u32, low: u32) -> i64 {
    ((high as i64) << 32) | low as i64
}

/// Opens the file identified by `frn` on `volume` and reads its size, dates
/// and attributes. Returns `Ok(None)` when the file cannot be opened
/// (already deleted, or transiently locked): a non-fatal "nothing to
/// refresh", since the structural create/delete was already applied from
/// the journal.
pub fn stat_file(volume: &VolumeHandle, frn: u64) -> Result<Option<FileStat>, WinError> {
    // SAFETY: the volume handle is live for the call; the opened handle is
    // always closed before the block returns. Only the FFI needs unsafe.
    let info = unsafe {
        let Some(handle) = open_by_id(volume.as_handle(), frn) else {
            return Ok(None);
        };
        let mut info: BY_HANDLE_FILE_INFORMATION = zeroed();
        let ok = GetFileInformationByHandle(handle, &mut info);
        let last_error = if ok == 0 { GetLastError() } else { 0 };
        CloseHandle(handle);
        if ok == 0 {
            return Err(WinError {
                code: last_error,
                context: "GetFileInformationByHandle",
            });
        }
        info
    };

    let size = ((info.nFileSizeHigh as u64) << 32) | info.nFileSizeLow as u64;
    let ft =
        |t: windows_sys::Win32::Foundation::FILETIME| packed(t.dwHighDateTime, t.dwLowDateTime);
    Ok(Some(FileStat {
        size,
        mtime_ft: ft(info.ftLastWriteTime),
        is_dir: info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0,
        ctime_ft: ft(info.ftCreationTime),
        atime_ft: ft(info.ftLastAccessTime),
        attributes: info.dwFileAttributes,
    }))
}

/// A handle on a volume's root folder, to open its files by FRN from any
/// thread: what a query that reads a column the index does not hold opens
/// them with, outside the index lock. `CloseHandle` on drop.
pub struct VolumeHint(HANDLE);

// SAFETY: a Win32 HANDLE is a process-wide value with no thread affinity, and
// `OpenFileById` only reads the hint to find the volume, so threads may share
// one.
unsafe impl Send for VolumeHint {}
// SAFETY: as above: every use is a read of the handle value.
unsafe impl Sync for VolumeHint {}

impl Drop for VolumeHint {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from a successful CreateFileW and is owned by
        // this value; it is closed exactly once, here.
        unsafe { CloseHandle(self.0) };
    }
}

impl VolumeHint {
    /// Opens the root folder of the volume at `guid_path`
    /// (`\\?\Volume{GUID}\`), or any folder path, for its attributes.
    pub fn open(path: &str) -> Result<Self, WinError> {
        let mut with_sep = path.to_owned();
        if !with_sep.ends_with('\\') {
            with_sep.push('\\');
        }
        let wide = to_wide_nul(&with_sep);
        // SAFETY: `wide` is a NUL-terminated path; no security attributes and
        // no template handle are supplied.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                core::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                core::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(last_error("CreateFileW (volume hint)"));
        }
        Ok(Self(handle))
    }
}

/// What `FILE_BASIC_INFO` says of one file: its dates as packed FILETIMEs
/// and its `FILE_ATTRIBUTE_*` bits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileBasic {
    pub ctime_ft: i64,
    pub atime_ft: i64,
    pub mtime_ft: i64,
    pub attributes: u32,
}

/// Opens the file `frn` on the volume of `hint` by its ID and reads its
/// basic information; `None` when it cannot be opened or read.
pub fn basic_info(hint: &VolumeHint, frn: u64) -> Option<FileBasic> {
    // SAFETY: the hint is live for the call; the opened handle is closed
    // before the block returns, and `info` is a plain out struct of the size
    // passed.
    let info = unsafe {
        let handle = open_by_id(hint.0, frn)?;
        let mut info: FILE_BASIC_INFO = zeroed();
        let ok = GetFileInformationByHandleEx(
            handle,
            FileBasicInfo,
            core::ptr::from_mut(&mut info).cast::<c_void>(),
            size_of::<FILE_BASIC_INFO>() as u32,
        );
        CloseHandle(handle);
        if ok == 0 {
            return None;
        }
        info
    };
    Some(FileBasic {
        ctime_ft: info.CreationTime,
        atime_ft: info.LastAccessTime,
        mtime_ft: info.LastWriteTime,
        attributes: info.FileAttributes,
    })
}

#[cfg(test)]
mod tests {
    use super::{VolumeHint, basic_info, stat_file};
    use crate::test_support::{
        Scratch, frn_of, junction, mtime_of, own_info, set_mtime, volume_hint,
    };

    /// A junction made while the tail runs stats as itself, with the date
    /// FILE_LAYOUT gives its record at bootstrap, not its target's. Opened by
    /// ID without `FILE_FLAG_OPEN_REPARSE_POINT` it fails with
    /// `ERROR_INVALID_PARAMETER`, which reads as gone, so it kept no date.
    #[test]
    fn a_junction_stats_as_itself() {
        let scratch = Scratch::new("stat");
        let target = scratch.0.join("target");
        std::fs::create_dir(&target).expect("create target");
        set_mtime(&target, 1_000_000_000); // 2001, far from the junction's own date
        let link = scratch.0.join("link");
        junction(&link, &target);
        let dangling = scratch.0.join("dangling");
        junction(&dangling, &scratch.0.join("missing"));
        let hint = volume_hint(&scratch.0);

        for path in [&link, &dangling] {
            let own = own_info(path);
            let stat = stat_file(&hint, frn_of(&own))
                .expect("stat")
                .unwrap_or_else(|| panic!("{} opens by ID as itself", path.display()));
            assert!(stat.is_dir);
            assert_eq!(stat.mtime_ft, mtime_of(&own), "{}", path.display());
            // A junction is a reparse point, which the attributes column shows.
            assert_eq!(stat.attributes & 0x410, 0x410, "{}", path.display());
        }
        assert_ne!(mtime_of(&own_info(&link)), mtime_of(&own_info(&target)));
    }

    /// The control: a plain file still stats its size and date, and its
    /// creation date and attributes.
    #[test]
    fn a_plain_file_stats_its_size_and_date() {
        let scratch = Scratch::new("stat-plain");
        let file = scratch.0.join("plain.txt");
        std::fs::write(&file, b"hello\n").expect("write");
        let own = own_info(&file);
        let stat = stat_file(&volume_hint(&scratch.0), frn_of(&own))
            .expect("stat")
            .expect("a plain file opens by ID");
        assert_eq!(
            (stat.size, stat.mtime_ft, stat.is_dir),
            (6, mtime_of(&own), false)
        );
        let ctime = ((own.ftCreationTime.dwHighDateTime as i64) << 32)
            | own.ftCreationTime.dwLowDateTime as i64;
        assert_eq!(stat.ctime_ft, ctime);
        assert_eq!(stat.attributes, own.dwFileAttributes);
    }

    /// A query's disk read: the basic information of a hidden read-only file
    /// and of a folder, by FRN, from a hint any thread may share; a FRN no
    /// file has reads as nothing.
    #[test]
    fn basic_info_reads_dates_and_attributes_by_id() {
        let scratch = Scratch::new("basic");
        let file = scratch.0.join("h.txt");
        std::fs::write(&file, b"x").expect("write");
        set_mtime(&file, 1_000_000_000);
        let mut perms = std::fs::metadata(&file).expect("meta").permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&file, perms).expect("read-only");
        let status = std::process::Command::new("attrib")
            .arg("+h")
            .arg(&file)
            .status()
            .expect("attrib");
        assert!(status.success());
        let hint = VolumeHint::open(&scratch.0.to_string_lossy()).expect("open the hint folder");
        let own = own_info(&file);
        let got = basic_info(&hint, frn_of(&own)).expect("opens by ID");
        assert_eq!(got.mtime_ft, mtime_of(&own));
        assert_eq!(got.attributes & 0x3, 0x3, "read-only and hidden");
        let dir = own_info(&scratch.0);
        let got = basic_info(&hint, frn_of(&dir)).expect("a folder opens by ID");
        assert_ne!(got.attributes & 0x10, 0);
        assert_eq!(basic_info(&hint, 0x0000_7FFF_FFFF_FFF0), None);
        // Undo read-only so the scratch folder goes.
        let _ = std::process::Command::new("attrib")
            .arg("-r")
            .arg("-h")
            .arg(&file)
            .status();
    }

    /// The root folder of a volume opens as a hint by its path.
    #[test]
    fn a_volume_hint_opens_by_path() {
        let scratch = Scratch::new("hint");
        let hint = VolumeHint::open(&scratch.0.to_string_lossy()).expect("open a folder");
        let own = own_info(&scratch.0);
        assert!(basic_info(&hint, frn_of(&own)).is_some());
    }
}
