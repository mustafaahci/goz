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
    CheckTokenMembership, CreateWellKnownSid, DuplicateToken, GetTokenInformation, RevertToSelf,
    SECURITY_ATTRIBUTES, SecurityIdentification, TOKEN_DUPLICATE, TOKEN_ELEVATION, TOKEN_QUERY,
    TokenElevation, WELL_KNOWN_SID_TYPE, WinBuiltinAdministratorsSid,
};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateFileW, DeleteFileW, FILE_ATTRIBUTE_NORMAL, FlushFileBuffers,
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW, WriteFile,
};
use windows_sys::Win32::System::Pipes::ImpersonateNamedPipeClient;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, OpenProcessToken, OpenThreadToken,
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

/// Whether the client of `pipe` connected with an elevated administrator's
/// token: elevated (`TokenElevation`) and holding the enabled
/// BUILTIN\Administrators group, as an elevated administrator or SYSTEM
/// does. The token is the one the connection carries, which the server
/// reads by impersonating the client for as long as it takes to open it;
/// not the client process's, whose ID can be freed while the connection
/// stays open and given to an elevated process. Call it after a request
/// has been read from the pipe (Windows lets no server impersonate before).
/// Fails closed: an anonymous connection, or any failure, is an error,
/// which callers treat as "not an administrator".
pub fn pipe_client_is_elevated_admin(pipe: BorrowedHandle<'_>) -> Result<bool, WinError> {
    let token = pipe_client_token(pipe)?;
    token_is_elevated_admin(token.0)
}

/// The token the connection at the other end of `pipe` carries, opened for
/// query and duplication. This thread impersonates the client only while
/// it opens the token.
fn pipe_client_token(pipe: BorrowedHandle<'_>) -> Result<Owned, WinError> {
    // SAFETY: `pipe` is a live, borrowed server end.
    if unsafe { ImpersonateNamedPipeClient(pipe.as_raw_handle()) } == 0 {
        return Err(last_error("ImpersonateNamedPipeClient"));
    }
    let revert = Revert;
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: GetCurrentThread returns a pseudo-handle that needs no close;
    // `token` is a live out param. OpenAsSelf: the token is opened with the
    // process's own rights, not the client's.
    let ok = unsafe {
        OpenThreadToken(
            GetCurrentThread(),
            TOKEN_QUERY | TOKEN_DUPLICATE,
            1,
            &mut token,
        )
    };
    // Read before RevertToSelf can change the last error.
    let failed = (ok == 0).then(|| last_error("OpenThreadToken"));
    drop(revert);
    match failed {
        Some(e) => Err(e),
        None => Ok(Owned(token)),
    }
}

/// Ends this thread's impersonation of a pipe client when dropped. A thread
/// that could not stop impersonating would go on serving other clients as
/// that one, so a failure to revert ends the process.
struct Revert;

impl Drop for Revert {
    fn drop(&mut self) {
        // SAFETY: plain FFI on this thread's own token.
        if unsafe { RevertToSelf() } == 0 {
            std::process::abort();
        }
    }
}

/// [`pipe_client_is_elevated_admin`] for the current process, the same test
/// of its own token.
pub fn current_process_is_elevated_admin() -> Result<bool, WinError> {
    let token = own_token()?;
    token_is_elevated_admin(token.0)
}

/// The current process's token, opened for query and duplication.
fn own_token() -> Result<Owned, WinError> {
    // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no close.
    let me = unsafe { GetCurrentProcess() };
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: `me` is a live process handle; `token` is a live out param.
    if unsafe { OpenProcessToken(me, TOKEN_QUERY | TOKEN_DUPLICATE, &mut token) } == 0 {
        return Err(last_error("OpenProcessToken"));
    }
    Ok(Owned(token))
}

/// Whether `token` (opened for query and duplication) is elevated and holds
/// the enabled BUILTIN\Administrators group.
fn token_is_elevated_admin(token: HANDLE) -> Result<bool, WinError> {
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut len = 0u32;
    // SAFETY: `token` is live; `elevation` is a correctly sized out struct.
    let ok = unsafe {
        GetTokenInformation(
            token,
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
    token_in_group(token, WinBuiltinAdministratorsSid)
}

/// Whether `token` (opened for query and duplication) holds the well-known
/// group `group` enabled: a group the token holds deny-only does not count.
fn token_in_group(token: HANDLE, group: WELL_KNOWN_SID_TYPE) -> Result<bool, WinError> {
    // CheckTokenMembership takes an impersonation token.
    let mut imp: HANDLE = ptr::null_mut();
    // SAFETY: `token` is live and opened with TOKEN_DUPLICATE; `imp` is a
    // live out param.
    if unsafe { DuplicateToken(token, SecurityIdentification, &mut imp) } == 0 {
        return Err(last_error("DuplicateToken"));
    }
    let imp = Owned(imp);
    let mut sid = [0u8; MAX_SID_SIZE];
    let mut sid_len = MAX_SID_SIZE as u32;
    // SAFETY: `sid` is a MAX_SID_SIZE buffer and `sid_len` its capacity; the
    // BUILTIN groups need no domain SID.
    if unsafe {
        CreateWellKnownSid(
            group,
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
    use windows_sys::Win32::Foundation::{ERROR_NO_TOKEN, GENERIC_READ};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, GetNamedSecurityInfoW, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        CreateRestrictedToken, DACL_SECURITY_INFORMATION, ImpersonateLoggedOnUser,
        SID_AND_ATTRIBUTES, TOKEN_ASSIGN_PRIMARY, TOKEN_IMPERSONATE, WinBuiltinUsersSid,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, SECURITY_ANONYMOUS, SECURITY_IDENTIFICATION,
        SECURITY_SQOS_PRESENT,
    };
    use windows_sys::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
    };

    /// Both ends of a pipe after the client sent one byte and the server read
    /// it: the state the daemon is in when it judges a request's sender.
    struct Connected {
        server: Owned,
        _client: Owned,
    }

    impl Connected {
        fn server(&self) -> BorrowedHandle<'_> {
            // SAFETY: `server` stays open for the borrow.
            unsafe { BorrowedHandle::borrow_raw(self.server.0) }
        }
    }

    /// A pipe everyone may open, and a client that opens it with `flags`
    /// from a thread impersonating `as_token` (when given) while it opens
    /// the pipe and writes, so the connection carries that token, not the
    /// process's.
    fn connected(tag: &str, flags: u32, as_token: Option<&Owned>) -> Connected {
        let name = format!(r"\\.\pipe\goz-admin-test-{}-{tag}", std::process::id());
        let wide = to_wide_nul(&name);
        let sddl = to_wide_nul("D:(A;;GA;;;WD)");
        let mut descriptor: *mut c_void = ptr::null_mut();
        // SAFETY: `sddl` is NUL-terminated; `descriptor` is freed below.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                ptr::null_mut(),
            )
        };
        assert_ne!(ok, 0);
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        // SAFETY: `wide` is NUL-terminated; `attributes` points at the live
        // descriptor for the call.
        let server = Owned(unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                4096,
                4096,
                0,
                &attributes,
            )
        });
        // SAFETY: LocalAlloc'd by the conversion above; freed once.
        unsafe { LocalFree(descriptor) };
        assert_ne!(server.0, INVALID_HANDLE_VALUE);
        let token = as_token.map(|t| t.0 as usize);
        let client = std::thread::spawn(move || {
            if let Some(t) = token {
                // SAFETY: `t` is a live primary token the caller keeps open.
                assert_ne!(unsafe { ImpersonateLoggedOnUser(t as HANDLE) }, 0);
            }
            // SAFETY: `wide` names the pipe made above; optional pointers null.
            let client = unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    ptr::null(),
                    OPEN_EXISTING,
                    flags,
                    ptr::null_mut(),
                )
            };
            assert_ne!(client, INVALID_HANDLE_VALUE);
            let mut written = 0u32;
            // SAFETY: `client` is the live pipe end just opened.
            let ok = unsafe { WriteFile(client, b"x".as_ptr(), 1, &mut written, ptr::null_mut()) };
            assert_ne!(ok, 0);
            if token.is_some() {
                // SAFETY: plain FFI on this thread's own token.
                assert_ne!(unsafe { RevertToSelf() }, 0);
            }
            client as usize
        })
        .join()
        .expect("client thread");
        let client = Owned(client as HANDLE);
        let mut byte = [0u8; 1];
        let mut read = 0u32;
        // SAFETY: `server` is live; `byte` is writable for one byte.
        let ok = unsafe { ReadFile(server.0, byte.as_mut_ptr(), 1, &mut read, ptr::null_mut()) };
        assert!(ok != 0 && read == 1);
        Connected {
            server,
            _client: client,
        }
    }

    /// This process's token with `groups` made deny-only.
    fn restricted_token(groups: &[WELL_KNOWN_SID_TYPE]) -> Owned {
        let mut process: HANDLE = ptr::null_mut();
        // SAFETY: the pseudo-handle needs no close; `process` is a live out
        // param.
        let ok = unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_IMPERSONATE | TOKEN_ASSIGN_PRIMARY,
                &mut process,
            )
        };
        assert_ne!(ok, 0);
        let process = Owned(process);
        let mut sids = vec![[0u8; MAX_SID_SIZE]; groups.len()];
        for (sid, &group) in sids.iter_mut().zip(groups) {
            let mut len = MAX_SID_SIZE as u32;
            // SAFETY: `sid` is a MAX_SID_SIZE buffer and `len` its capacity.
            let ok = unsafe {
                CreateWellKnownSid(group, ptr::null_mut(), sid.as_mut_ptr().cast(), &mut len)
            };
            assert_ne!(ok, 0);
        }
        let disable: Vec<SID_AND_ATTRIBUTES> = sids
            .iter_mut()
            .map(|sid| SID_AND_ATTRIBUTES {
                Sid: sid.as_mut_ptr().cast(),
                Attributes: 0,
            })
            .collect();
        let mut restricted: HANDLE = ptr::null_mut();
        // SAFETY: `process` is live with TOKEN_DUPLICATE; `disable` holds
        // valid SIDs for the call; `restricted` is a live out param.
        let ok = unsafe {
            CreateRestrictedToken(
                process.0,
                0,
                disable.len() as u32,
                disable.as_ptr(),
                0,
                ptr::null(),
                0,
                ptr::null(),
                &mut restricted,
            )
        };
        assert_ne!(ok, 0);
        Owned(restricted)
    }

    /// The verdict on a pipe's client is the verdict on its token: this test
    /// process at both ends, so an unelevated run proves a non-administrator
    /// is refused, and an elevated one that an elevated administrator is
    /// accepted.
    /// Both at the default impersonation level and at identification, the
    /// level `goz --config set` opens the pipe with.
    #[test]
    fn the_pipe_clients_token_decides() {
        let me = current_process_is_elevated_admin().expect("own token");
        for (tag, flags) in [
            ("own", 0),
            (
                "identified",
                SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
            ),
        ] {
            let pipe = connected(tag, flags, None);
            let verdict = pipe_client_is_elevated_admin(pipe.server()).expect("the client's token");
            println!(
                "admin check {tag}: elevated={} verdict={verdict}",
                is_elevated()
            );
            assert_eq!(verdict, me);
            if !is_elevated() {
                assert!(!verdict, "an unelevated client is no administrator");
            }
        }
    }

    /// W9R-1: the token the connection carries decides, not its process's,
    /// whose ID can be freed and reused by an elevated process while the
    /// connection stays open. A client connecting with BUILTIN\Users and
    /// Administrators deny-only is judged by that token, though its process
    /// holds both; and the server thread stops impersonating it.
    #[test]
    fn the_connections_token_decides_not_its_process() {
        let own = own_token().expect("own token");
        assert!(
            token_in_group(own.0, WinBuiltinUsersSid).expect("own groups"),
            r"this process holds BUILTIN\Users"
        );
        let restricted = restricted_token(&[WinBuiltinUsersSid, WinBuiltinAdministratorsSid]);
        for (tag, flags) in [
            ("restricted", 0),
            (
                "restricted-identified",
                SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
            ),
        ] {
            let pipe = connected(tag, flags, Some(&restricted));
            let token = pipe_client_token(pipe.server()).expect("the connection's token");
            assert!(
                !token_in_group(token.0, WinBuiltinUsersSid).expect("its groups"),
                "{tag}: the connection's token, not the process's"
            );
            assert!(
                !pipe_client_is_elevated_admin(pipe.server()).expect("its verdict"),
                "{tag}: Administrators deny-only is no administrator, even when this process is one"
            );
        }
        let mut left: HANDLE = ptr::null_mut();
        // SAFETY: the pseudo-handle needs no close; `left` is a live out param.
        let ok = unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut left) };
        let code = last_error("OpenThreadToken").code;
        drop(Owned(left));
        assert!(ok == 0 && code == ERROR_NO_TOKEN, "the thread reverted");
    }

    /// A client that lets the server see no token at all is no administrator:
    /// the check fails closed.
    #[test]
    fn an_anonymous_connection_is_refused() {
        let pipe = connected(
            "anonymous",
            SECURITY_SQOS_PRESENT | SECURITY_ANONYMOUS,
            None,
        );
        let verdict = pipe_client_is_elevated_admin(pipe.server());
        println!("anonymous: {verdict:?}");
        assert!(verdict.is_err(), "{verdict:?}");
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
