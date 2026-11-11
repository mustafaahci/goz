//! Blocking named-pipe client. Opens `\\.\pipe\goz-v1` and exchanges framed
//! messages with the daemon using the shared codec from `goz-core::proto`:
//! JSON requests, and tagged responses whose results pages carry a binary body.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::io::AsHandle;
use std::time::{Duration, Instant};

use goz_core::proto::{PIPE_NAME, QueryRequest, QueryResults, Request, Response, encode_request};

use crate::reply::{PipeIo, Replies, ReplyError};

/// Opening the pipe with an anonymous impersonation level (`SECURITY_ANONYMOUS`,
/// value 0, so only the SQOS-present bit is set) prevents a rogue server that
/// squatted the name from impersonating this client's token. It does NOT
/// authenticate the server: see [`Client::connect`], which verifies the pipe's
/// owner is the elevated daemon before any request is sent.
const SECURITY_SQOS_PRESENT: u32 = 0x0010_0000;
/// The impersonation level `--config set` offers instead
/// (`SECURITY_IDENTIFICATION`): gozd judges a change by the token the
/// connection carries, which an anonymous connection does not show it. At
/// this level a server learns who the client is but cannot act as it.
const SECURITY_IDENTIFICATION: u32 = 0x0001_0000;

#[derive(Debug)]
pub(crate) enum ClientError {
    /// The daemon is not running / the pipe does not exist.
    Unreachable,
    /// A server is listening on the pipe but is not the trusted elevated daemon
    /// (owner is not SYSTEM / Administrators), a possible squatter.
    Untrusted,
    /// The daemon replied with something malformed or timed out.
    Protocol(String),
    /// The daemon refused the query, with its reason.
    Refused(String),
}

pub(crate) struct Client {
    pipe: File,
    replies: Replies,
}

impl Client {
    /// Connects to the running daemon and, unless `insecure`, verifies the pipe
    /// server's identity before returning. The connection shows the daemon
    /// this client's token only when `identify` (see
    /// [`SECURITY_IDENTIFICATION`]); otherwise it is anonymous.
    ///
    /// Returns [`ClientError::Unreachable`] if no pipe exists (daemon absent),
    /// [`ClientError::Untrusted`] if a server is present but not owned by the
    /// elevated daemon, or [`ClientError::Protocol`] for other open errors.
    pub(crate) fn connect(insecure: bool, identify: bool) -> Result<Self, ClientError> {
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
                .custom_flags(if identify {
                    SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION
                } else {
                    SECURITY_SQOS_PRESENT
                })
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

    /// Sends a request answered in one frame, such as `Hello` or `Status`,
    /// and reads that frame. The wait gives up once the daemon has been
    /// silent for the deadline [`Replies`] keeps.
    pub(crate) fn request(&mut self, req: &Request) -> Result<Response, ClientError> {
        self.send(req)?;
        Ok(self.replies.next(&mut Pipe(&self.pipe))?)
    }

    /// Runs a search, handing each results page to `page` as it arrives, so
    /// the client holds one page at a time however many match. It ends after
    /// the last page, or at the first error, `page`'s own included; a query
    /// gozd refuses ends in [`ClientError::Refused`].
    pub(crate) fn query<E: From<ClientError>>(
        &mut self,
        q: QueryRequest,
        mut page: impl FnMut(QueryResults) -> Result<(), E>,
    ) -> Result<(), E> {
        self.send(&Request::Query(q))?;
        self.replies
            .query(&mut Pipe(&self.pipe), |p| page(p).map_err(Relay))
            .map_err(|Relay(e)| e)
    }

    fn send(&mut self, req: &Request) -> Result<(), ClientError> {
        let mut out = Vec::new();
        encode_request(req, &mut out);
        self.pipe
            .write_all(&out)
            .map_err(|e| ClientError::Protocol(e.to_string()))
    }
}

impl From<ReplyError> for ClientError {
    fn from(e: ReplyError) -> Self {
        match e {
            ReplyError::Refused(message) => Self::Refused(message),
            other => Self::Protocol(other.to_string()),
        }
    }
}

/// The error of a [`Client::query`] page on its way through
/// [`Replies::query`], which carries the reply's own errors the same way.
struct Relay<E>(E);

impl<E: From<ClientError>> From<ReplyError> for Relay<E> {
    fn from(e: ReplyError) -> Self {
        Self(ClientError::from(e).into())
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
