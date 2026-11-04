//! Hard-link set enumeration: every name a file record currently has.
//!
//! A USN `HARD_LINK_CHANGE` record names exactly one link and may name a DEAD
//! one, so the index can never learn a file's real link set from the journal
//! alone. This asks the filesystem instead, and the daemon feeds the answer to
//! `goz_core::index::VolumeIndex::reconcile_links`.
//!
//! Cost is paid only when a hard-link change actually arrives, which is rare,
//! and the caller runs it with the index write lock RELEASED: like `stat_file`,
//! these are blocking file opens and must never sit inside the lock every query
//! contends on.
//!
//! Returns raw UTF-16 exactly as Win32 gave it. NTFS names can contain unpaired
//! surrogates, so decoding is left to `goz-core`'s WTF-8 layer rather than being
//! lossily flattened here.

use core::mem::{size_of, zeroed};
use core::ptr;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_HANDLE_EOF, ERROR_MORE_DATA, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_DESCRIPTOR,
    FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FindClose, FindFirstFileNameW, FindNextFileNameW, GetFinalPathNameByHandleW,
    OpenFileById, VOLUME_NAME_GUID,
};

use crate::error::WinError;
use crate::volume::VolumeHandle;

/// `FileIdType`: a 64-bit NTFS file reference number in `FILE_ID_DESCRIPTOR`.
const FILE_ID_TYPE_FILE: i32 = 0;
/// Starting buffer for a link name, in UTF-16 units. Grown on `ERROR_MORE_DATA`.
const NAME_BUF_UNITS: usize = 512;
/// Refuse to walk a pathological link set rather than spin. NTFS caps hard links
/// at 1024 per file.
const MAX_LINKS: usize = 4096;

/// Every current hard-link name of `frn`, each as a volume-relative UTF-16 path
/// (`\dir\name`, no drive letter), exactly as `FindNextFileNameW` reports them.
///
/// `Ok(None)` means the file could not be opened or walked: already deleted, or
/// transiently locked. That is a "nothing to reconcile", not an error, and the
/// caller must NOT treat it as an empty link set (which would delete the file).
///
/// # Errors
/// Returns [`WinError`] only when the walk fails for a reason that is not
/// "the file is gone".
pub fn link_paths(volume: &VolumeHandle, frn: u64) -> Result<Option<Vec<Vec<u16>>>, WinError> {
    // SAFETY: `desc` is fully initialized before use; OpenFileById takes the
    // live volume-hint handle; the returned handle is closed on every path
    // below, including the early returns.
    let file = unsafe {
        let mut desc: FILE_ID_DESCRIPTOR = zeroed();
        desc.dwSize = size_of::<FILE_ID_DESCRIPTOR>() as u32;
        desc.Type = FILE_ID_TYPE_FILE;
        desc.Anonymous.FileId = frn as i64;

        OpenFileById(
            volume.as_handle(),
            &desc,
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            // A link opens as itself, so the anchor below is its own name.
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
        )
    };
    if file == INVALID_HANDLE_VALUE {
        return Ok(None); // deleted or locked between apply and reconcile
    }

    // FindFirstFileNameW needs an openable path, and the index's own path for
    // this FRN is exactly what may be stale (it is why we are reconciling), so
    // ask the filesystem. The GUID form works on a volume with no drive letter.
    let anchor = final_path(file, VOLUME_NAME_GUID | FILE_NAME_NORMALIZED);
    // SAFETY: `file` is a live handle this function opened and has not closed.
    unsafe {
        CloseHandle(file);
    }
    let Some(anchor) = anchor else {
        return Ok(None);
    };

    walk_links(&anchor)
}

/// `GetFinalPathNameByHandleW` into an owned NUL-terminated UTF-16 buffer.
fn final_path(file: HANDLE, flags: u32) -> Option<Vec<u16>> {
    // SAFETY: `file` is live for this call. The first call passes a zero-length
    // buffer, which the API documents as "return the required length".
    let needed = unsafe { GetFinalPathNameByHandleW(file, ptr::null_mut(), 0, flags) };
    if needed == 0 {
        return None;
    }
    let mut buf = vec![0u16; needed as usize];
    // SAFETY: `buf` holds `needed` units, which is the length the call above
    // asked for; the API writes at most that many including the NUL.
    let written = unsafe { GetFinalPathNameByHandleW(file, buf.as_mut_ptr(), needed, flags) };
    if written == 0 || written >= needed {
        return None;
    }
    buf.truncate(written as usize + 1); // keep the NUL for PCWSTR
    Some(buf)
}

/// Walks `FindFirstFileNameW`/`FindNextFileNameW` over an anchor path.
fn walk_links(anchor: &[u16]) -> Result<Option<Vec<Vec<u16>>>, WinError> {
    let mut out: Vec<Vec<u16>> = Vec::new();
    let mut buf = vec![0u16; NAME_BUF_UNITS];
    let mut len = buf.len() as u32;

    // SAFETY: `anchor` is NUL-terminated; `len` names `buf`'s capacity and the
    // API writes at most that many units, updating `len` to what it needs.
    let find = unsafe { FindFirstFileNameW(anchor.as_ptr(), 0, &mut len, buf.as_mut_ptr()) };
    let find = if find == INVALID_HANDLE_VALUE {
        // SAFETY: plain FFI read of the calling thread's last error.
        let code = unsafe { GetLastError() };
        if code == ERROR_MORE_DATA {
            buf = vec![0u16; len as usize];
            // SAFETY: `buf` is now the size the API just asked for.
            let retry =
                unsafe { FindFirstFileNameW(anchor.as_ptr(), 0, &mut len, buf.as_mut_ptr()) };
            if retry == INVALID_HANDLE_VALUE {
                return Ok(None);
            }
            retry
        } else {
            return Ok(None); // gone or unreadable: nothing to reconcile
        }
    } else {
        find
    };

    out.push(take_name(&buf));

    loop {
        if out.len() >= MAX_LINKS {
            break;
        }
        len = buf.len() as u32;
        // SAFETY: `find` is the live search handle; `len` names `buf`'s capacity.
        let ok = unsafe { FindNextFileNameW(find, &mut len, buf.as_mut_ptr()) };
        if ok == 0 {
            // SAFETY: plain FFI read of the calling thread's last error.
            let code = unsafe { GetLastError() };
            if code == ERROR_MORE_DATA {
                buf = vec![0u16; len as usize];
                // SAFETY: `buf` is now the size the API just asked for.
                let ok = unsafe { FindNextFileNameW(find, &mut len, buf.as_mut_ptr()) };
                if ok == 0 {
                    break;
                }
            } else if code == ERROR_HANDLE_EOF {
                break; // walked every link
            } else {
                break; // raced with a delete: keep what we have
            }
        }
        out.push(take_name(&buf));
    }

    // SAFETY: `find` came from FindFirstFileNameW and is closed exactly once.
    unsafe {
        FindClose(find);
    }
    Ok(Some(out))
}

/// Copies the NUL-terminated name out of the scratch buffer.
fn take_name(buf: &[u16]) -> Vec<u16> {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    buf[..end].to_vec()
}

#[cfg(test)]
mod tests {
    use super::link_paths;
    use crate::test_support::{Scratch, frn_of, junction, own_info, volume_hint};

    /// A junction walks its own name, not its target's. Opened by ID without
    /// `FILE_FLAG_OPEN_REPARSE_POINT` it fails and the reconcile is skipped.
    #[test]
    fn a_junction_walks_its_own_name() {
        let scratch = Scratch::new("links");
        let target = scratch.0.join("target");
        std::fs::create_dir(&target).expect("create target");
        let link = scratch.0.join("link");
        junction(&link, &target);

        let names = link_paths(&volume_hint(&scratch.0), frn_of(&own_info(&link)))
            .expect("walk")
            .expect("a junction opens by ID as itself");
        let names: Vec<String> = names.iter().map(|n| String::from_utf16_lossy(n)).collect();
        let folder = scratch
            .0
            .file_name()
            .and_then(|n| n.to_str())
            .expect("folder name");
        assert_eq!(names.len(), 1, "{names:?}");
        assert!(names[0].ends_with(&format!(r"\{folder}\link")), "{names:?}");
    }

    /// The control: a plain file with two hard links walks both.
    #[test]
    fn a_hard_linked_file_walks_every_name() {
        let scratch = Scratch::new("links-plain");
        let first = scratch.0.join("a.txt");
        std::fs::write(&first, b"x").expect("write");
        std::fs::hard_link(&first, scratch.0.join("b.txt")).expect("hard link");

        let mut names: Vec<String> =
            link_paths(&volume_hint(&scratch.0), frn_of(&own_info(&first)))
                .expect("walk")
                .expect("a plain file opens by ID")
                .iter()
                .map(|n| String::from_utf16_lossy(n))
                .collect();
        names.sort();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(
            names[0].ends_with(r"\a.txt") && names[1].ends_with(r"\b.txt"),
            "{names:?}"
        );
    }
}
