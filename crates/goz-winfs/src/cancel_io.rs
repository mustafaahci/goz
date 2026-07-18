//! Cross-thread cancellation of a stuck synchronous I/O.
//!
//! A synchronous `DeviceIoControl` can block forever when a device pends the
//! IRP and never completes it; observed live on `FSCTL_READ_USN_JOURNAL`
//! against a spinning-down HDD, which froze that volume's tail thread for
//! good (no error, no return, phase stuck at Offline). `CancelSynchronousIo`
//! exists for exactly this: it aborts the blocked call from another thread,
//! making it return `ERROR_OPERATION_ABORTED`, and the caller's normal retry
//! path takes over.

use windows_sys::Win32::Foundation::{
    CloseHandle, DUPLICATE_SAME_ACCESS, ERROR_NOT_FOUND, GetLastError, HANDLE,
};
use windows_sys::Win32::System::IO::CancelSynchronousIo;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, GetCurrentThreadId,
};

use crate::error::WinError;

/// A real (non-pseudo) handle to a thread, held so a supervisor can cancel
/// that thread's blocked synchronous I/O. Closes the handle on drop.
#[derive(Debug)]
pub struct ThreadIoHandle {
    handle: HANDLE,
    /// The owning thread's id, for log lines.
    pub thread_id: u32,
}

// SAFETY: a duplicated thread handle is a kernel object reference; using it
// from another thread (the watchdog) is its entire purpose, and Windows
// permits handle use across threads.
unsafe impl Send for ThreadIoHandle {}
// SAFETY: the only operations are CancelSynchronousIo and CloseHandle-on-drop;
// both are safe against concurrent use of the same handle value, so shared
// references across threads are sound.
unsafe impl Sync for ThreadIoHandle {}

impl Drop for ThreadIoHandle {
    fn drop(&mut self) {
        // SAFETY: `handle` is a real handle owned by this struct, closed once.
        unsafe { CloseHandle(self.handle) };
    }
}

/// Duplicates a REAL handle to the CALLING thread (the pseudo-handle from
/// `GetCurrentThread` is only meaningful on the thread itself, so this must
/// run on the thread that will later be cancelled).
pub fn current_thread_io_handle() -> Result<ThreadIoHandle, WinError> {
    // SAFETY: plain FFI; the pseudo-handles are valid by definition, and the
    // out-param is a live stack slot. DUPLICATE_SAME_ACCESS on a
    // GetCurrentThread pseudo-handle yields THREAD_ALL_ACCESS, which covers
    // the THREAD_TERMINATE right CancelSynchronousIo requires.
    unsafe {
        let mut real: HANDLE = core::ptr::null_mut();
        let ok = windows_sys::Win32::Foundation::DuplicateHandle(
            GetCurrentProcess(),
            GetCurrentThread(),
            GetCurrentProcess(),
            &mut real,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        );
        if ok == 0 {
            return Err(WinError {
                code: GetLastError(),
                context: "DuplicateHandle(GetCurrentThread)",
            });
        }
        Ok(ThreadIoHandle {
            handle: real,
            thread_id: GetCurrentThreadId(),
        })
    }
}

/// Cancels the synchronous I/O the target thread is currently blocked in.
/// `Ok(true)` = an operation was cancelled (it returns
/// `ERROR_OPERATION_ABORTED` to its caller); `Ok(false)` = the thread was not
/// in a cancellable wait (nothing to do, not an error).
pub fn cancel_synchronous_io(thread: &ThreadIoHandle) -> Result<bool, WinError> {
    // SAFETY: plain FFI on a live duplicated thread handle.
    unsafe {
        if CancelSynchronousIo(thread.handle) != 0 {
            return Ok(true);
        }
        let code = GetLastError();
        if code == ERROR_NOT_FOUND {
            return Ok(false);
        }
        Err(WinError {
            code,
            context: "CancelSynchronousIo",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// End to end against a genuinely blocked synchronous read: a thread
    /// blocks reading an anonymous pipe nobody writes to, the "supervisor"
    /// (this test) cancels it, and the read returns ERROR_OPERATION_ABORTED
    /// instead of blocking forever. This is the exact rescue the tail
    /// supervisor performs on a stuck FSCTL_READ_USN_JOURNAL.
    #[test]
    fn cancels_a_blocked_synchronous_read() {
        use windows_sys::Win32::Foundation::ERROR_OPERATION_ABORTED;
        use windows_sys::Win32::Storage::FileSystem::ReadFile;
        use windows_sys::Win32::System::Pipes::CreatePipe;

        let mut read_end: HANDLE = core::ptr::null_mut();
        let mut write_end: HANDLE = core::ptr::null_mut();
        // SAFETY: out-params are live; default security, default buffer.
        let ok = unsafe { CreatePipe(&mut read_end, &mut write_end, core::ptr::null(), 0) };
        assert_ne!(ok, 0, "CreatePipe failed");
        let read_end_addr = read_end as usize;

        let (tx, rx) = mpsc::channel();
        let blocked = std::thread::spawn(move || {
            let io = current_thread_io_handle().expect("duplicating own thread handle");
            tx.send(io).expect("send handle");
            let mut buf = [0u8; 16];
            let mut got: u32 = 0;
            // SAFETY: blocking read on the live pipe read end; buffer and
            // out-param outlive the call.
            let ok = unsafe {
                ReadFile(
                    read_end_addr as HANDLE,
                    buf.as_mut_ptr(),
                    buf.len() as u32,
                    &mut got,
                    core::ptr::null_mut(),
                )
            };
            // SAFETY: last error read immediately after the failed call on
            // this thread.
            let err = unsafe { GetLastError() };
            (ok, err)
        });

        let io = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("handle arrives");
        // The reader may not have entered ReadFile yet; retry until the
        // cancel actually lands on a blocked operation.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match cancel_synchronous_io(&io) {
                Ok(true) => break,
                Ok(false) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "reader never entered a cancellable wait"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("CancelSynchronousIo errored: {e}"),
            }
        }

        let (ok, err) = blocked.join().expect("blocked thread joins");
        assert_eq!(ok, 0, "the cancelled read must fail, not succeed");
        assert_eq!(
            err, ERROR_OPERATION_ABORTED,
            "the cancelled read must surface ERROR_OPERATION_ABORTED"
        );

        // SAFETY: both pipe ends are live and closed exactly once.
        unsafe {
            CloseHandle(read_end_addr as HANDLE);
            CloseHandle(write_end);
        }
    }
}
