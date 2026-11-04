//! Scratch folders, junctions and an unelevated volume hint, for tests that
//! open files by ID.

use core::mem::zeroed;
use std::fs::OpenOptions;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_READ_ATTRIBUTES, FILE_WRITE_ATTRIBUTES, GetFileInformationByHandle,
};

use crate::volume::VolumeHandle;

/// A fresh folder under the temp folder, removed on drop. `remove_dir_all`
/// removes a junction without entering it.
pub(crate) struct Scratch(pub(crate) PathBuf);

impl Scratch {
    pub(crate) fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after 1970")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("goz-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir(&dir).expect("create scratch folder");
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Makes `link` a junction to `target`. Unlike a symbolic link, a junction
/// needs no privilege, so this runs on any developer box and CI runner.
pub(crate) fn junction(link: &Path, target: &Path) {
    let status = Command::new("cmd")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(link)
        .arg(target)
        .stdout(Stdio::null())
        .status()
        .expect("run mklink");
    assert!(status.success(), "mklink /J {} failed", link.display());
}

/// Sets the last-write time of the file or folder at `path`.
pub(crate) fn set_mtime(path: &Path, unix_secs: u64) {
    OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .expect("open for FILE_WRITE_ATTRIBUTES")
        .set_modified(UNIX_EPOCH + Duration::from_secs(unix_secs))
        .expect("set_modified");
}

/// The record at `path` itself: a reparse point is not followed.
pub(crate) fn own_info(path: &Path) -> BY_HANDLE_FILE_INFORMATION {
    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .expect("open the record itself");
    // SAFETY: `info` is a plain out struct and `file` stays open for the call.
    unsafe {
        let mut info: BY_HANDLE_FILE_INFORMATION = zeroed();
        assert_ne!(
            GetFileInformationByHandle(file.as_raw_handle(), &mut info),
            0,
            "GetFileInformationByHandle"
        );
        info
    }
}

/// The 64-bit file reference number in `info`.
pub(crate) fn frn_of(info: &BY_HANDLE_FILE_INFORMATION) -> u64 {
    ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64
}

/// The last-write time in `info`, packed as `FileStat::mtime_ft` packs it.
pub(crate) fn mtime_of(info: &BY_HANDLE_FILE_INFORMATION) -> i64 {
    ((info.ftLastWriteTime.dwHighDateTime as i64) << 32) | info.ftLastWriteTime.dwLowDateTime as i64
}

/// A handle on `dir` as the volume hint. `OpenFileById` takes a handle to any
/// file on the volume, so tests need no elevated volume open.
pub(crate) fn volume_hint(dir: &Path) -> VolumeHandle {
    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(dir)
        .expect("open the hint folder");
    VolumeHandle::from_file(file)
}
