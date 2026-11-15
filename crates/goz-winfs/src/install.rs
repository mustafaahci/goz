//! Where `gozd install` puts the service's binary, and removing a binary
//! that is still running.
//!
//! The service runs as LocalSystem, so its binary must sit where only
//! administrators can write. Anywhere a user can write, a program the user
//! runs could swap in its own binary, which the service manager would then
//! start as SYSTEM.

use core::ptr;
use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_DELAY_UNTIL_REBOOT, MoveFileExW};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::UI::Shell::{FOLDERID_ProgramFiles, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
use windows_sys::core::PWSTR;

use crate::error::{WinError, last_error};

/// The Program Files folder, as Windows records it.
///
/// It is read from the known-folder store, not from `%ProgramFiles%`: an
/// elevated `gozd install` runs with the user's environment, and a user can
/// set their own `ProgramFiles` variable to a folder they can write.
pub fn program_files_dir() -> Result<PathBuf, WinError> {
    let mut path: PWSTR = ptr::null_mut();
    // SAFETY: `FOLDERID_ProgramFiles` is a static GUID and `path` is a valid
    // out-pointer; a null token asks for the folder as every user sees it.
    let hr = unsafe {
        SHGetKnownFolderPath(
            &FOLDERID_ProgramFiles,
            KF_FLAG_DEFAULT as u32,
            ptr::null_mut(),
            &mut path,
        )
    };
    let result = if hr < 0 || path.is_null() {
        Err(WinError {
            code: hr as u32,
            context: "SHGetKnownFolderPath (Program Files)",
        })
    } else {
        let mut len = 0;
        // SAFETY: on success `path` is a NUL-terminated wide string, so every
        // unit up to and including the NUL is readable.
        while unsafe { *path.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: the `len` units before the NUL were just read above.
        let wide = unsafe { core::slice::from_raw_parts(path, len) };
        Ok(PathBuf::from(OsString::from_wide(wide)))
    };
    // SAFETY: SHGetKnownFolderPath allocates `path` with CoTaskMemAlloc and
    // documents freeing it whether or not it succeeds; null is allowed.
    unsafe { CoTaskMemFree(path.cast()) };
    result
}

/// Asks Windows to delete `path`, a file or an empty folder, at the next
/// restart. Needs elevation.
pub fn remove_at_reboot(path: &Path) -> Result<(), WinError> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: `wide` is NUL-terminated; a null destination means delete.
    if unsafe { MoveFileExW(wide.as_ptr(), ptr::null(), MOVEFILE_DELAY_UNTIL_REBOOT) } == 0 {
        return Err(last_error("MoveFileExW (delete at restart)"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::program_files_dir;
    use std::process::Command;

    /// Set in the child process of the test below.
    const CHILD: &str = "GOZ_TEST_PROGRAM_FILES_CHILD";

    #[test]
    fn program_files_is_an_existing_absolute_folder() {
        let dir = program_files_dir().expect("Program Files");
        assert!(dir.is_absolute() && dir.is_dir(), "{}", dir.display());
    }

    /// A `ProgramFiles` variable the user sets does not move the folder.
    #[test]
    fn a_users_program_files_variable_does_not_move_it() {
        if std::env::var_os(CHILD).is_some() {
            println!(
                "dir={}",
                program_files_dir().expect("Program Files").display()
            );
            return;
        }
        let real = program_files_dir().expect("Program Files");
        let fake = std::env::temp_dir();
        let out = Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "install::tests::a_users_program_files_variable_does_not_move_it",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .env("ProgramFiles", &fake)
            .env("ProgramW6432", &fake)
            .output()
            .expect("run the child");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{stdout}");
        assert!(
            stdout.contains(&format!("dir={}", real.display())),
            "expected {}; the child said: {stdout}",
            real.display()
        );
    }
}
