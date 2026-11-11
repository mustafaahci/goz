//! Small facts the goz programs ask of Windows outside the volume layer:
//! the client process of a pipe (for gozd's per-process watch cap), Ctrl+C
//! as a flag, the ANSI code page, and which text filter Windows has for a
//! file extension (for the CLI's `content:`).

use std::os::windows::io::RawHandle;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::Foundation::{ERROR_SUCCESS, HANDLE};
use windows_sys::Win32::Globalization::{CP_ACP, MultiByteToWideChar};
use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler};
use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;
use windows_sys::Win32::System::Registry::{HKEY_CLASSES_ROOT, RRF_RT_REG_SZ, RegGetValueW};

/// The process at the client end of the server pipe `pipe`, or `None` when
/// Windows cannot say.
pub fn pipe_client_process_id(pipe: RawHandle) -> Option<u32> {
    let mut pid = 0u32;
    // SAFETY: `pipe` is a live pipe handle the caller owns for the call;
    // `pid` is a live local the call writes.
    let ok = unsafe { GetNamedPipeClientProcessId(pipe as HANDLE, &mut pid) };
    (ok != 0 && pid != 0).then_some(pid)
}

/// Set once Ctrl+C or Ctrl+Break reaches the console.
static CTRL_C: AtomicBool = AtomicBool::new(false);
static HANDLER: OnceLock<bool> = OnceLock::new();

unsafe extern "system" fn on_ctrl(kind: u32) -> i32 {
    if kind == CTRL_C_EVENT || kind == CTRL_BREAK_EVENT {
        CTRL_C.store(true, Ordering::SeqCst);
        1
    } else {
        0
    }
}

/// A flag Ctrl+C (or Ctrl+Break) sets, instead of ending the process, from
/// the first call on: a long task polls it and stops cleanly. `None` when
/// the handler cannot be installed, and Ctrl+C then ends the process as
/// before.
pub fn ctrl_c_flag() -> Option<&'static AtomicBool> {
    let installed = *HANDLER.get_or_init(|| {
        // SAFETY: `on_ctrl` is a `'static` function of the signature
        // `PHANDLER_ROUTINE` asks for; it only stores to a static atomic.
        unsafe { SetConsoleCtrlHandler(Some(on_ctrl), 1) != 0 }
    });
    installed.then_some(&CTRL_C)
}

/// Appends `bytes` read in the system's ANSI code page to `out`.
pub fn ansi_to_string(bytes: &[u8], out: &mut String) {
    // A chunk at a time, so no length passes `i32`; a code page's character
    // is at most two bytes, and a chunk cut through one costs one character.
    for chunk in bytes.chunks(1 << 20) {
        let len = chunk.len() as i32;
        // SAFETY: `chunk` is a live slice of `len` bytes; with a null
        // output the call only measures.
        let need =
            unsafe { MultiByteToWideChar(CP_ACP, 0, chunk.as_ptr(), len, std::ptr::null_mut(), 0) };
        if need <= 0 {
            continue;
        }
        let mut wide = vec![0u16; need as usize];
        // SAFETY: `wide` holds `need` units, as the call measured.
        let got =
            unsafe { MultiByteToWideChar(CP_ACP, 0, chunk.as_ptr(), len, wide.as_mut_ptr(), need) };
        if got > 0 {
            wide.truncate(got as usize);
            out.extend(char::decode_utf16(wide).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)));
        }
    }
}

/// The class id of the persistent handler (the IFilter host) Windows has
/// registered for files of extension `ext` (no dot), as
/// `HKEY_CLASSES_ROOT\.ext\PersistentHandler` names it, upper case and
/// braced; `None` when there is none.
pub fn persistent_handler(ext: &str) -> Option<String> {
    if ext.is_empty() || ext.contains(['\\', '\0']) {
        return None;
    }
    let key: Vec<u16> = format!(".{ext}\\PersistentHandler")
        .encode_utf16()
        .chain([0])
        .collect();
    let mut buf = [0u16; 64];
    let mut size = std::mem::size_of_val(&buf) as u32;
    // SAFETY: `key` is NUL-terminated; `buf` and `size` are live locals,
    // and `size` gives `buf`'s length in bytes.
    let status = unsafe {
        RegGetValueW(
            HKEY_CLASSES_ROOT,
            key.as_ptr(),
            std::ptr::null(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    let units = (size as usize / 2).min(buf.len());
    let text = String::from_utf16_lossy(&buf[..units]);
    let text = text.trim_end_matches('\0').trim().to_ascii_uppercase();
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `.bin` has the null persistent handler on every Windows, and an
    /// extension no one registers has none.
    #[test]
    fn persistent_handlers_are_read() {
        assert_eq!(
            persistent_handler("bin").as_deref(),
            Some("{098F2470-BAE0-11CD-B579-08002B30BFEB}")
        );
        assert_eq!(persistent_handler("goz-no-such-extension"), None);
        assert_eq!(persistent_handler(""), None);
    }

    /// ASCII reads as itself in every ANSI code page.
    #[test]
    fn ansi_reads_ascii_as_itself() {
        let mut out = String::new();
        ansi_to_string(b"plain text 123", &mut out);
        assert_eq!(out, "plain text 123");
    }
}
