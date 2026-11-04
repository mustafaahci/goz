//! Blocking named-pipe client. Opens `\\.\pipe\goz-v1` and exchanges framed
//! messages with the daemon using the shared codec from `goz-core::proto`:
//! JSON requests, and tagged responses whose results pages carry a binary body.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::io::AsHandle;
use std::time::{Duration, Instant};

use goz_core::proto::{PIPE_NAME, QueryResults, Request, Response, encode_request};

use crate::reply::{PipeIo, Replies};

/// Opening the pipe with an anonymous impersonation level (`SECURITY_ANONYMOUS`,
/// value 0, so only the SQOS-present bit is set) prevents a rogue server that
/// squatted the name from impersonating this client's token. It does NOT
/// authenticate the server: see [`Client::connect`], which verifies the pipe's
/// owner is the elevated daemon before any request is sent.
const SECURITY_SQOS_PRESENT: u32 = 0x0010_0000;

#[derive(Debug)]
pub(crate) enum ClientError {
    /// The daemon is not running / the pipe does not exist.
    Unreachable,
    /// A server is listening on the pipe but is not the trusted elevated daemon
    /// (owner is not SYSTEM / Administrators), a possible squatter.
    Untrusted,
    /// The daemon replied with something malformed or timed out.
    Protocol(String),
}

pub(crate) struct Client {
    pipe: File,
    replies: Replies,
}

impl Client {
    /// Connects to the running daemon and, unless `insecure`, verifies the pipe
    /// server's identity before returning.
    ///
    /// Returns [`ClientError::Unreachable`] if no pipe exists (daemon absent),
    /// [`ClientError::Untrusted`] if a server is present but not owned by the
    /// elevated daemon, or [`ClientError::Protocol`] for other open errors.
    pub(crate) fn connect(insecure: bool) -> Result<Self, ClientError> {
        use std::os::windows::fs::OpenOptionsExt;

        // Win32 error codes (goz-cli intentionally has no windows-sys dep).
        const ERROR_FILE_NOT_FOUND: i32 = 2;
        const ERROR_PATH_NOT_FOUND: i32 = 3;
        const ERROR_PIPE_BUSY: i32 = 231;

        // The daemon keeps a single listening instance and recreates it only
        // after accepting a client, so a burst of concurrent clients can
        // momentarily find no free instance (ERROR_PIPE_BUSY). That is transient
        // and retryable: the daemon IS running, so it must not be reported as
        // "unreachable".
        let mut attempts = 0u32;
        let pipe = loop {
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(SECURITY_SQOS_PRESENT)
                .open(PIPE_NAME)
            {
                Ok(pipe) => break pipe,
                Err(e) => match e.raw_os_error() {
                    // Pipe does not exist: the daemon really is not running.
                    Some(ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) => {
                        return Err(ClientError::Unreachable);
                    }
                    // All instances momentarily busy: brief backoff and retry
                    // (~500 ms budget) before giving up.
                    Some(ERROR_PIPE_BUSY) if attempts < 20 => {
                        attempts += 1;
                        std::thread::sleep(Duration::from_millis(25));
                    }
                    // Busy past the retry budget, ERROR_ACCESS_DENIED, or anything
                    // unexpected: the pipe is present but we could not open it,
                    // so surface the real OS error rather than "not running".
                    _ => {
                        return Err(ClientError::Protocol(format!(
                            "cannot open {PIPE_NAME}: {e}"
                        )));
                    }
                },
            }
        };

        // Authenticate the server BEFORE sending any request: a non-elevated
        // local user can pre-create `\\.\pipe\goz-v1` before the daemon starts
        // and serve spoofed results. Require the pipe be owned by SYSTEM /
        // Administrators (created by the elevated daemon). Fail closed on error.
        if !insecure {
            match goz_winfs::pipe_server_is_trusted(pipe.as_handle()) {
                Ok(true) => {}
                Ok(false) | Err(_) => return Err(ClientError::Untrusted),
            }
        }

        Ok(Self {
            pipe,
            replies: Replies::new(),
        })
    }

    /// Sends one request and reads the framed response.
    ///
    /// A `Results` response may span multiple frames (the daemon pages large
    /// result sets, setting `more` on all but the last); these are accumulated
    /// transparently into a single [`Response::Results`]. Each wait gives up
    /// once the daemon has been silent for the deadline [`Replies`] keeps.
    pub(crate) fn request(&mut self, req: &Request) -> Result<Response, ClientError> {
        let mut out = Vec::new();
        encode_request(req, &mut out);
        self.pipe
            .write_all(&out)
            .map_err(|e| ClientError::Protocol(e.to_string()))?;

        let mut merged: Option<QueryResults> = None;
        loop {
            let reply = self
                .replies
                .next(&mut Pipe(&self.pipe))
                .map_err(|e| ClientError::Protocol(e.to_string()))?;
            match reply {
                Response::Results(page) => {
                    let more = page.more;
                    match &mut merged {
                        None => merged = Some(page),
                        Some(acc) => acc.items.extend(page.items),
                    }
                    if !more {
                        let mut done = merged.take().expect("at least one Results page");
                        done.more = false;
                        return Ok(Response::Results(done));
                    }
                }
                other => return Ok(other),
            }
        }
    }
}

/// The daemon's pipe, polled with `PeekNamedPipe` so that no read blocks
/// past the deadline.
struct Pipe<'a>(&'a File);

impl PipeIo for Pipe<'_> {
    fn available(&mut self) -> io::Result<u32> {
        goz_winfs::pipe_bytes_available(self.0.as_handle()).map_err(io::Error::other)
    }

    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }

    fn now(&self) -> Instant {
        Instant::now()
    }

    fn pause(&mut self, time: Duration) {
        std::thread::sleep(time);
    }
}
