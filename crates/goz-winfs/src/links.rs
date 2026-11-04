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
    CloseHandle, ERROR_HANDLE_EOF, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_PARAMETER,
    ERROR_MORE_DATA, ERROR_TOO_MANY_LINKS, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_DESCRIPTOR,
    FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FindClose, FindFirstFileNameW, FindNextFileNameW, GetFinalPathNameByHandleW,
    OpenFileById, VOLUME_NAME_GUID,
};

use crate::error::{WinError, last_error};
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
/// A symbolic link or junction reports its own names.
///
/// `Ok(None)` means the file is gone: its file ID no longer opens. Its delete
/// record removes it, so there is nothing to reconcile, and the caller must not
/// read this as an empty link set (which would delete the file).
///
/// # Errors
/// Every other failure, including a walk that fails part way or finds more than
/// 4096 links. The caller makes the index hold exactly the links returned, so a
/// partial set would delete the links the walk never reached.
pub fn link_paths(volume: &VolumeHandle, frn: u64) -> Result<Option<Vec<Vec<u16>>>, WinError> {
    // SAFETY: `desc` is fully initialized before use; OpenFileById takes the
    // live volume-hint handle; the returned handle is closed below on every
    // path that got one.
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
        let error = last_error("OpenFileById");
        // Only the file ID proves a file gone: its record is free, or reused
        // under a newer sequence number. A pending delete answers
        // ERROR_ACCESS_DENIED like a real denial, so it stays an error.
        return if error.code == ERROR_INVALID_PARAMETER {
            Ok(None)
        } else {
            Err(error)
        };
    }

    // FindFirstFileNameW needs an openable path, and the index's own path for
    // this FRN is exactly what may be stale (it is why we are reconciling), so
    // ask the filesystem. The GUID form works on a volume with no drive letter.
    let anchor = final_path(file, VOLUME_NAME_GUID | FILE_NAME_NORMALIZED);
    // SAFETY: `file` is a live handle this function opened and has not closed.
    unsafe {
        CloseHandle(file);
    }
    walk_links(&anchor?).map(Some)
}

/// `GetFinalPathNameByHandleW` into an owned NUL-terminated UTF-16 buffer.
fn final_path(file: HANDLE, flags: u32) -> Result<Vec<u16>, WinError> {
    // SAFETY: `file` is live for this call. The first call passes a zero-length
    // buffer, which the API documents as "return the required length".
    let needed = unsafe { GetFinalPathNameByHandleW(file, ptr::null_mut(), 0, flags) };
    if needed == 0 {
        return Err(last_error("GetFinalPathNameByHandleW"));
    }
    let mut buf = vec![0u16; needed as usize];
    // SAFETY: `buf` holds `needed` units, which is the length the call above
    // asked for; the API writes at most that many including the NUL.
    let written = unsafe { GetFinalPathNameByHandleW(file, buf.as_mut_ptr(), needed, flags) };
    if written == 0 {
        return Err(last_error("GetFinalPathNameByHandleW"));
    }
    if written >= needed {
        // The path grew between the two calls: a rename.
        return Err(WinError {
            code: ERROR_INSUFFICIENT_BUFFER,
            context: "GetFinalPathNameByHandleW",
        });
    }
    buf.truncate(written as usize + 1); // keep the NUL for PCWSTR
    Ok(buf)
}

/// Walks `FindFirstFileNameW`/`FindNextFileNameW` over an anchor path. Returns
/// the whole link set or an error, never part of it.
fn walk_links(anchor: &[u16]) -> Result<Vec<Vec<u16>>, WinError> {
    let mut buf = vec![0u16; NAME_BUF_UNITS];
    let find = loop {
        let mut len = buf.len() as u32;
        // SAFETY: `anchor` is NUL-terminated; `len` names `buf`'s capacity and
        // the API writes at most that many units, updating `len` to what it
        // needs.
        let find = unsafe { FindFirstFileNameW(anchor.as_ptr(), 0, &mut len, buf.as_mut_ptr()) };
        if find != INVALID_HANDLE_VALUE {
            break find;
        }
        let error = last_error("FindFirstFileNameW");
        if error.code == ERROR_MORE_DATA && len as usize > buf.len() {
            buf = vec![0u16; len as usize];
        } else {
            // Also a file renamed or deleted since it was opened: a path that
            // stopped resolving does not prove the file gone.
            return Err(error);
        }
    };
    let first = take_name(&buf);
    let walked = drain_links(first, &mut buf, |buf, len| {
        // SAFETY: `find` is the live search handle; `len` names `buf`'s capacity.
        if unsafe { FindNextFileNameW(find, len, buf.as_mut_ptr()) } != 0 {
            Ok(())
        } else {
            Err(last_error("FindNextFileNameW"))
        }
    });
    // SAFETY: `find` came from FindFirstFileNameW and is closed exactly once.
    unsafe {
        FindClose(find);
    }
    walked
}

/// Collects the links after `first`, one `next` (a `FindNextFileNameW`) at a
/// time. Anything but the end of the walk fails the whole walk: a link folder
/// that cannot be opened, or one deleted mid-walk, leaves later links unread.
fn drain_links(
    first: Vec<u16>,
    buf: &mut Vec<u16>,
    mut next: impl FnMut(&mut [u16], &mut u32) -> Result<(), WinError>,
) -> Result<Vec<Vec<u16>>, WinError> {
    let mut links = vec![first];
    loop {
        let mut len = buf.len() as u32;
        match next(buf, &mut len) {
            Ok(()) if links.len() == MAX_LINKS => {
                return Err(WinError {
                    code: ERROR_TOO_MANY_LINKS,
                    context: "FindNextFileNameW",
                });
            }
            Ok(()) => links.push(take_name(buf)),
            Err(e) if e.code == ERROR_HANDLE_EOF => return Ok(links),
            // The same link again: ERROR_MORE_DATA does not advance the walk.
            Err(e) if e.code == ERROR_MORE_DATA && len as usize > buf.len() => {
                *buf = vec![0; len as usize];
            }
            Err(e) => return Err(e),
        }
    }
}

/// Copies the NUL-terminated name out of the scratch buffer.
fn take_name(buf: &[u16]) -> Vec<u16> {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    buf[..end].to_vec()
}

#[cfg(test)]
mod tests {
    use super::{MAX_LINKS, NAME_BUF_UNITS, drain_links, link_paths};
    use crate::error::WinError;
    use crate::test_support::{Scratch, frn_of, junction, own_info, volume_hint};
    use std::path::Path;
    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_HANDLE_EOF, ERROR_MORE_DATA, ERROR_TOO_MANY_LINKS,
    };

    fn names(links: &[Vec<u16>]) -> Vec<String> {
        links.iter().map(|n| String::from_utf16_lossy(n)).collect()
    }

    /// One file with a link in each of folders a, b and c.
    fn three_links(scratch: &Scratch) -> u64 {
        for dir in ["a", "b", "c"] {
            std::fs::create_dir(scratch.0.join(dir)).expect("create folder");
        }
        let first = scratch.0.join(r"a\f");
        std::fs::write(&first, b"x").expect("write");
        std::fs::hard_link(&first, scratch.0.join(r"b\f")).expect("hard link");
        std::fs::hard_link(&first, scratch.0.join(r"c\f")).expect("hard link");
        frn_of(&own_info(&first))
    }

    /// Sets the DACL of `path` from SDDL.
    fn set_dacl(path: &Path, sddl: &str) {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
        use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, SetFileSecurityW};

        let sddl: Vec<u16> = sddl.encode_utf16().chain([0]).collect();
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        let mut sd = core::ptr::null_mut();
        // SAFETY: `sddl` is NUL-terminated, `sd` receives a LocalAlloc'd
        // descriptor that is freed below, and `wide` is a NUL-terminated path.
        unsafe {
            assert_ne!(
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    1,
                    &mut sd,
                    core::ptr::null_mut()
                ),
                0,
                "ConvertStringSecurityDescriptorToSecurityDescriptorW"
            );
            let ok = SetFileSecurityW(wide.as_ptr(), DACL_SECURITY_INFORMATION, sd);
            LocalFree(sd);
            assert_ne!(ok, 0, "SetFileSecurityW {}", path.display());
        }
    }

    /// A junction walks its own name, not its target's. Opened by ID without
    /// `FILE_FLAG_OPEN_REPARSE_POINT` it fails and the reconcile is skipped.
    #[test]
    fn a_junction_walks_its_own_name() {
        let scratch = Scratch::new("links");
        let target = scratch.0.join("target");
        std::fs::create_dir(&target).expect("create target");
        let link = scratch.0.join("link");
        junction(&link, &target);

        let names = names(
            &link_paths(&volume_hint(&scratch.0), frn_of(&own_info(&link)))
                .expect("walk")
                .expect("a junction opens by ID as itself"),
        );
        let folder = scratch
            .0
            .file_name()
            .and_then(|n| n.to_str())
            .expect("folder name");
        assert_eq!(names.len(), 1, "{names:?}");
        assert!(names[0].ends_with(&format!(r"\{folder}\link")), "{names:?}");
    }

    /// The control: a file with links in three folders walks all of them.
    #[test]
    fn a_hard_linked_file_walks_every_name() {
        let scratch = Scratch::new("links-plain");
        let frn = three_links(&scratch);
        let mut names = names(
            &link_paths(&volume_hint(&scratch.0), frn)
                .expect("walk")
                .expect("a plain file opens by ID"),
        );
        names.sort();
        let tails: Vec<&str> = names.iter().map(|n| &n[n.len() - 4..]).collect();
        assert_eq!(tails, [r"\a\f", r"\b\f", r"\c\f"], "{names:?}");
    }

    /// Only a file ID that no longer opens proves a file gone: deleted, or
    /// its record now holds a newer file.
    #[test]
    fn a_deleted_file_reads_as_gone() {
        let scratch = Scratch::new("links-gone");
        let hint = volume_hint(&scratch.0);
        let path = scratch.0.join("f");
        std::fs::write(&path, b"x").expect("write");
        let frn = frn_of(&own_info(&path));
        std::fs::remove_file(&path).expect("delete");
        assert!(matches!(link_paths(&hint, frn), Ok(None)));

        std::fs::write(&path, b"x").expect("write");
        let live = frn_of(&own_info(&path));
        let other_sequence = live ^ (0x7F << 48);
        assert!(matches!(link_paths(&hint, other_sequence), Ok(None)));
    }

    /// A link folder the walk cannot open stops `FindNextFileNameW` part way.
    /// The set read so far must not come back: the index would make it the
    /// file's whole set and delete the links the walk never reached.
    #[test]
    fn an_unreadable_link_folder_fails_the_whole_walk() {
        let scratch = Scratch::new("links-denied");
        let frn = three_links(&scratch);
        let hint = volume_hint(&scratch.0);
        // Links come back in folder-ID order, so deny the second one returned.
        let order = names(&link_paths(&hint, frn).expect("walk").expect("opens"));
        let second = &order[1];
        let folder = scratch.0.join(&second[second.len() - 3..second.len() - 2]);
        // Everyone may do anything here except read the folder's attributes,
        // which FindNextFileNameW needs to name the link inside it.
        set_dacl(&folder, "D:P(D;;0x80;;;WD)(A;OICI;FA;;;WD)");
        let walked = link_paths(&hint, frn);
        set_dacl(&folder, "D:(A;OICI;FA;;;WD)");
        assert!(
            matches!(walked, Err(ref e) if e.code == ERROR_ACCESS_DENIED),
            "{walked:?}"
        );
    }

    /// A file whose delete is pending refuses to open with
    /// ERROR_ACCESS_DENIED. It is not gone yet, so it is an error the caller
    /// counts, not a silent skip.
    #[test]
    fn a_pending_delete_is_an_error_not_gone() {
        use std::os::windows::fs::OpenOptionsExt;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            DELETE, FILE_DISPOSITION_INFO, FileDispositionInfo, SetFileInformationByHandle,
        };

        let scratch = Scratch::new("links-pending");
        let path = scratch.0.join("f");
        std::fs::write(&path, b"x").expect("write");
        let frn = frn_of(&own_info(&path));
        let doomed = std::fs::OpenOptions::new()
            .access_mode(DELETE)
            .share_mode(7)
            .open(&path)
            .expect("open for delete");
        let info = FILE_DISPOSITION_INFO { DeleteFile: true };
        // SAFETY: `doomed` is a live handle opened with DELETE access and
        // `info` is a FILE_DISPOSITION_INFO of the size passed.
        let ok = unsafe {
            SetFileInformationByHandle(
                doomed.as_raw_handle(),
                FileDispositionInfo,
                (&raw const info).cast(),
                size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        };
        assert_ne!(ok, 0, "SetFileInformationByHandle");
        let walked = link_paths(&volume_hint(&scratch.0), frn);
        drop(doomed);
        assert!(
            matches!(walked, Err(ref e) if e.code == ERROR_ACCESS_DENIED),
            "{walked:?}"
        );
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    /// A fake `FindNextFileNameW` over `links`, then `end`. Sized like the real
    /// call: a name that does not fit fails with ERROR_MORE_DATA and the length
    /// it needs, and comes back on the next call.
    fn fake_next(
        links: Vec<Vec<u16>>,
        end: u32,
    ) -> impl FnMut(&mut [u16], &mut u32) -> Result<(), WinError> {
        let mut at = 0;
        move |buf, len| {
            let Some(name) = links.get(at) else {
                return Err(WinError {
                    code: end,
                    context: "fake",
                });
            };
            if name.len() + 1 > buf.len() {
                *len = (name.len() + 1) as u32;
                return Err(WinError {
                    code: ERROR_MORE_DATA,
                    context: "fake",
                });
            }
            buf[..name.len()].copy_from_slice(name);
            buf[name.len()] = 0;
            at += 1;
            Ok(())
        }
    }

    #[test]
    fn the_end_of_the_walk_returns_every_link() {
        let mut buf = vec![0; NAME_BUF_UNITS];
        let links = fake_next(vec![wide(r"\b\f"), wide(r"\c\f")], ERROR_HANDLE_EOF);
        let walked = drain_links(wide(r"\a\f"), &mut buf, links).expect("walk");
        assert_eq!(walked, [wide(r"\a\f"), wide(r"\b\f"), wide(r"\c\f")]);
    }

    #[test]
    fn a_failure_part_way_fails_the_walk() {
        let mut buf = vec![0; NAME_BUF_UNITS];
        let links = fake_next(vec![wide(r"\b\f")], ERROR_ACCESS_DENIED);
        let walked = drain_links(wide(r"\a\f"), &mut buf, links);
        assert!(
            matches!(walked, Err(ref e) if e.code == ERROR_ACCESS_DENIED),
            "{walked:?}"
        );
    }

    #[test]
    fn a_long_name_grows_the_buffer_and_keeps_the_link() {
        let long = wide(&format!(r"\{}", "n".repeat(700)));
        let mut buf = vec![0; NAME_BUF_UNITS];
        let links = fake_next(vec![long.clone()], ERROR_HANDLE_EOF);
        let walked = drain_links(wide(r"\a\f"), &mut buf, links).expect("walk");
        assert_eq!(walked, [wide(r"\a\f"), long]);
    }

    /// ERROR_MORE_DATA without a larger length would ask for the same link
    /// forever.
    #[test]
    fn more_data_that_does_not_grow_is_an_error_not_a_spin() {
        let mut buf = vec![0; NAME_BUF_UNITS];
        let mut calls = 0;
        let walked = drain_links(wide(r"\a\f"), &mut buf, |_, _| {
            calls += 1;
            Err(WinError {
                code: ERROR_MORE_DATA,
                context: "fake",
            })
        });
        assert!(matches!(walked, Err(ref e) if e.code == ERROR_MORE_DATA));
        assert_eq!(calls, 1);
    }

    #[test]
    fn more_links_than_the_cap_is_an_error() {
        let links = |n: usize| (1..n).map(|i| wide(&format!(r"\{i}"))).collect();
        let mut buf = vec![0; NAME_BUF_UNITS];
        let full = drain_links(
            wide(r"\0"),
            &mut buf,
            fake_next(links(MAX_LINKS), ERROR_HANDLE_EOF),
        )
        .expect("a walk at the cap");
        assert_eq!(full.len(), MAX_LINKS);
        let over = drain_links(
            wide(r"\0"),
            &mut buf,
            fake_next(links(MAX_LINKS + 1), ERROR_HANDLE_EOF),
        );
        assert!(matches!(over, Err(ref e) if e.code == ERROR_TOO_MANY_LINKS));
    }
}
