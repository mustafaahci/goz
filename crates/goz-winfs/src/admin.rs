//! What gozd's index options need of Windows: whether the client of a pipe
//! is an elevated administrator (only one may change them), and a file
//! everyone may read and only administrators and SYSTEM may write (where
//! gozd keeps them).

use core::ffi::c_void;
use core::ptr;
use std::os::windows::io::{AsRawHandle, BorrowedHandle};
use std::path::Path;

use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::{
    CheckTokenMembership, CreateWellKnownSid, DuplicateToken, GetTokenInformation,
    SECURITY_ATTRIBUTES, SecurityIdentification, TOKEN_DUPLICATE, TOKEN_ELEVATION, TOKEN_QUERY,
    TokenElevation, WinBuiltinAdministratorsSid,
};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateFileW, DeleteFileW, FILE_ATTRIBUTE_NORMAL, FlushFileBuffers,
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW, WriteFile,
};
use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;
use windows_sys::Win32::System::Threading::{
    OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

use crate::error::{WinError, last_error};
use crate::util::to_wide_nul;

/// Maximum size of a SID, in bytes (`SECURITY_MAX_SID_SIZE`).
const MAX_SID_SIZE: usize = 68;

/// A handle closed on drop.
struct Owned(HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the handle was opened by this module and is owned here;
            // closed exactly once.
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// Whether the process at the other end of `pipe` runs with an elevated
/// administrator's token: elevated (`TokenElevation`) and holding the
/// enabled BUILTIN\Administrators group, as an elevated administrator or
/// SYSTEM does. Read from the client process's token, so it needs nothing
/// of how the client opened the pipe. Fails closed: callers should treat an
/// error as "not an administrator".
pub fn pipe_client_is_elevated_admin(pipe: BorrowedHandle<'_>) -> Result<bool, WinError> {
    let mut pid: u32 = 0;
    // SAFETY: `pipe` is a live, borrowed server end; `pid` is a live out param.
    if unsafe { GetNamedPipeClientProcessId(pipe.as_raw_handle(), &mut pid) } == 0 {
        return Err(last_error("GetNamedPipeClientProcessId"));
    }
    // SAFETY: plain FFI; a null result is checked below.
    let process = Owned(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) });
    if process.0.is_null() {
        return Err(last_error("OpenProcess"));
    }
    token_is_elevated_admin(process.0)
}

/// [`pipe_client_is_elevated_admin`] for the current process, the same test
/// of its own token.
pub fn current_process_is_elevated_admin() -> Result<bool, WinError> {
    // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no close.
    let me = unsafe { windows_sys::Win32::System::Threading::GetCurrentProcess() };
    token_is_elevated_admin(me)
}

fn token_is_elevated_admin(process: HANDLE) -> Result<bool, WinError> {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: `process` is a live process handle; `token` is a live out param.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY | TOKEN_DUPLICATE, &mut token) } == 0 {
        return Err(last_error("OpenProcessToken"));
    }
    let token = Owned(token);
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut len = 0u32;
    // SAFETY: `token` is live; `elevation` is a correctly sized out struct.
    let ok = unsafe {
        GetTokenInformation(
            token.0,
            TokenElevation,
            ptr::from_mut(&mut elevation).cast::<c_void>(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
    };
    if ok == 0 {
        return Err(last_error("GetTokenInformation(TokenElevation)"));
    }
    if elevation.TokenIsElevated == 0 {
        return Ok(false);
    }
    // CheckTokenMembership takes an impersonation token.
    let mut imp: HANDLE = ptr::null_mut();
    // SAFETY: `token` is live and opened with TOKEN_DUPLICATE; `imp` is a
    // live out param.
    if unsafe { DuplicateToken(token.0, SecurityIdentification, &mut imp) } == 0 {
        return Err(last_error("DuplicateToken"));
    }
    let imp = Owned(imp);
    let mut sid = [0u8; MAX_SID_SIZE];
    let mut sid_len = MAX_SID_SIZE as u32;
    // SAFETY: `sid` is a MAX_SID_SIZE buffer and `sid_len` its capacity; the
    // BUILTIN\Administrators SID needs no domain SID.
    if unsafe {
        CreateWellKnownSid(
            WinBuiltinAdministratorsSid,
            ptr::null_mut(),
            sid.as_mut_ptr().cast(),
            &mut sid_len,
        )
    } == 0
    {
        return Err(last_error("CreateWellKnownSid"));
    }
    let mut member = 0;
    // SAFETY: `imp` is a live impersonation token and `sid` a valid SID.
    if unsafe { CheckTokenMembership(imp.0, sid.as_mut_ptr().cast(), &mut member) } == 0 {
        return Err(last_error("CheckTokenMembership"));
    }
    Ok(member != 0)
}

/// The DACL of a file everyone may read and only administrators and SYSTEM
/// may change: protected, so nothing inherited widens it.
const ADMIN_WRITE_SDDL: &str = "D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FR;;;WD)";

/// Writes `bytes` to `path` as a file everyone may read and only
/// administrators and SYSTEM may write: a new file beside it, created with
/// that DACL and flushed, then moved over `path`, which keeps the DACL. A
/// reader sees the old file or the new one, never part of one.
pub fn write_admin_only(path: &Path, bytes: &[u8]) -> Result<(), WinError> {
    let tmp = path.with_extension("new");
    let wide_tmp = to_wide_nul(&tmp.to_string_lossy());
    let wide_path = to_wide_nul(&path.to_string_lossy());
    let sddl = to_wide_nul(ADMIN_WRITE_SDDL);
    let mut descriptor: *mut c_void = ptr::null_mut();
    // SAFETY: `sddl` is NUL-terminated; on success `descriptor` is a
    // LocalAlloc'd self-relative descriptor, freed below.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(last_error(
            "ConvertStringSecurityDescriptorToSecurityDescriptorW",
        ));
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let result = (|| {
        // A file left by an interrupted write would keep its own DACL under
        // CREATE_ALWAYS; this one is made anew with the DACL above.
        // SAFETY: `wide_tmp` is NUL-terminated.
        unsafe { DeleteFileW(wide_tmp.as_ptr()) };
        // SAFETY: `wide_tmp` is NUL-terminated and `attributes` points at the
        // live descriptor for the call.
        let file = Owned(unsafe {
            CreateFileW(
                wide_tmp.as_ptr(),
                GENERIC_WRITE,
                0,
                &attributes,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                ptr::null_mut(),
            )
        });
        if file.0 == INVALID_HANDLE_VALUE {
            let e = last_error("CreateFileW (config)");
            core::mem::forget(file);
            return Err(e);
        }
        let mut written = 0u32;
        // SAFETY: `file` is a live handle opened for writing; `bytes` is
        // readable for its length.
        if unsafe {
            WriteFile(
                file.0,
                bytes.as_ptr(),
                bytes.len() as u32,
                &mut written,
                ptr::null_mut(),
            )
        } == 0
            || written as usize != bytes.len()
        {
            return Err(last_error("WriteFile (config)"));
        }
        // SAFETY: `file` is live.
        if unsafe { FlushFileBuffers(file.0) } == 0 {
            return Err(last_error("FlushFileBuffers (config)"));
        }
        drop(file);
        // SAFETY: both paths are NUL-terminated.
        if unsafe {
            MoveFileExW(
                wide_tmp.as_ptr(),
                wide_path.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(last_error("MoveFileExW (config)"));
        }
        Ok(())
    })();
    // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW,
    // which documents LocalFree; freed exactly once.
    unsafe { LocalFree(descriptor) };
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::elevation::is_elevated;
    use crate::test_support::Scratch;
    use windows_sys::Win32::Foundation::GENERIC_READ;
    use windows_sys::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, GetNamedSecurityInfoW, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    };
    use windows_sys::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
    };

    /// The verdict on a pipe's client is the verdict on its process's token:
    /// this test process at both ends, so an unelevated run proves a
    /// non-administrator is refused, and an elevated one that an elevated
    /// administrator is accepted.
    #[test]
    fn the_pipe_clients_token_decides() {
        let name = format!(r"\\.\pipe\goz-admin-test-{}", std::process::id());
        let wide = to_wide_nul(&name);
        // SAFETY: `wide` is NUL-terminated; default security.
        let server = Owned(unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                4096,
                4096,
                0,
                ptr::null(),
            )
        });
        assert_ne!(server.0, INVALID_HANDLE_VALUE);
        // SAFETY: `wide` names the pipe just made; optional pointers null.
        let client = Owned(unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                OPEN_EXISTING,
                0,
                ptr::null_mut(),
            )
        });
        assert_ne!(client.0, INVALID_HANDLE_VALUE);
        // SAFETY: `server` stays open for the borrow.
        let borrowed = unsafe { BorrowedHandle::borrow_raw(server.0) };
        let verdict = pipe_client_is_elevated_admin(borrowed).expect("the client's token");
        let me = current_process_is_elevated_admin().expect("own token");
        println!("admin check: elevated={} verdict={verdict}", is_elevated());
        assert_eq!(verdict, me);
        if !is_elevated() {
            assert!(!verdict, "an unelevated client is no administrator");
        }
    }

    /// The written file holds the bytes, and its DACL is exactly the
    /// protected one: SYSTEM and Administrators full, Everyone read. A
    /// second write replaces the first.
    #[test]
    fn the_config_file_is_admin_write_only() {
        let scratch = Scratch::new("admin-file");
        let path = scratch.0.join("gozd.ini");
        write_admin_only(&path, b"first\n").expect("write");
        write_admin_only(&path, b"index_attributes=1\n").expect("rewrite");
        assert_eq!(std::fs::read(&path).expect("read"), b"index_attributes=1\n");
        assert!(!path.with_extension("new").exists());
        let wide = to_wide_nul(&path.to_string_lossy());
        let mut sd: *mut c_void = ptr::null_mut();
        // SAFETY: `wide` is NUL-terminated; only the descriptor out param is
        // asked for, and it is freed below.
        let rc = unsafe {
            GetNamedSecurityInfoW(
                wide.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut sd,
            )
        };
        assert_eq!(rc, 0);
        let mut text: *mut u16 = ptr::null_mut();
        // SAFETY: `sd` is the descriptor just read; `text` is LocalAlloc'd.
        let ok = unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                sd,
                1,
                DACL_SECURITY_INFORMATION,
                &mut text,
                ptr::null_mut(),
            )
        };
        assert_ne!(ok, 0);
        // SAFETY: `text` is a NUL-terminated string from the call above.
        let sddl = unsafe {
            let mut n = 0;
            while *text.add(n) != 0 {
                n += 1;
            }
            String::from_utf16_lossy(std::slice::from_raw_parts(text, n))
        };
        // SAFETY: both were LocalAlloc'd by the calls above; freed once.
        unsafe {
            LocalFree(text.cast());
            LocalFree(sd);
        }
        assert_eq!(sddl, "D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FR;;;WD)");
    }
}
