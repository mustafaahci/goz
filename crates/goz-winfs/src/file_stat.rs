//! Read one file's current size + last-write time by its FRN, for the live
//! enricher. USN change records carry neither size nor timestamps, so a file
//! created or modified after bootstrap needs an out-of-band stat to refresh
//! them (otherwise its size stays "unknown", which downstream renders as a
//! directory).

use core::mem::{size_of, zeroed};

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_ID_DESCRIPTOR, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    GetFileInformationByHandle, OpenFileById,
};

use crate::error::WinError;
use crate::volume::VolumeHandle;

/// `FILE_ATTRIBUTE_DIRECTORY`, defined locally per this crate's convention.
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
/// `FileIdType`: a 64-bit NTFS file reference number in `FILE_ID_DESCRIPTOR`.
const FILE_ID_TYPE_FILE: i32 = 0;

/// The live size/mtime of one file.
pub struct FileStat {
    pub size: u64,
    /// Last-write time as a Windows `FILETIME` packed into `i64` (100 ns ticks
    /// since 1601), the same representation the FILE_LAYOUT path produces.
    pub mtime_ft: i64,
    pub is_dir: bool,
}

/// Opens the file identified by `frn` on `volume` and reads its size and
/// last-write time. Returns `Ok(None)` when the file cannot be opened (already
/// deleted, or transiently locked): a non-fatal "nothing to refresh", since
/// the structural create/delete was already applied from the journal.
pub fn stat_file(volume: &VolumeHandle, frn: u64) -> Result<Option<FileStat>, WinError> {
    // SAFETY: `desc`/`info` are fully initialized before use; OpenFileById gets
    // the live volume-hint handle; the opened handle is always closed before the
    // block returns. Only the FFI + union access needs unsafe. The size/mtime
    // arithmetic below reads a plain (non-union) struct and stays in safe code.
    let info = unsafe {
        let mut desc: FILE_ID_DESCRIPTOR = zeroed();
        desc.dwSize = size_of::<FILE_ID_DESCRIPTOR>() as u32;
        desc.Type = FILE_ID_TYPE_FILE;
        desc.Anonymous.FileId = frn as i64;

        let handle = OpenFileById(
            volume.as_handle(),
            &desc,
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            core::ptr::null(),
            // Backup semantics also open directories. A symbolic link or a
            // junction opens as itself, the record FILE_LAYOUT read at
            // bootstrap; without the flag its open by ID fails with
            // ERROR_INVALID_PARAMETER, which reads as gone.
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
        );
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            return Ok(None);
        }

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
    let mtime_ft = ((info.ftLastWriteTime.dwHighDateTime as i64) << 32)
        | info.ftLastWriteTime.dwLowDateTime as i64;
    let is_dir = info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    Ok(Some(FileStat {
        size,
        mtime_ft,
        is_dir,
    }))
}

#[cfg(test)]
mod tests {
    use super::stat_file;
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
        }
        assert_ne!(mtime_of(&own_info(&link)), mtime_of(&own_info(&target)));
    }

    /// The control: a plain file still stats its size and date.
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
    }
}
