//! The daemon's replies, read frame by frame under a deadline that measures
//! silence.
//!
//! The pipe comes in through [`PipeIo`], so nothing here calls Win32 and the
//! deadline is tested on a fake clock on every OS.

use std::fmt;
use std::io;
use std::time::{Duration, Instant};

use goz_core::proto::{
    FrameDecoder, MAX_SERVER_FRAME, QueryResults, Response, decode_response_frame,
};

/// How long the daemon may stay silent before the client gives up, so a
/// wedged but alive daemon cannot hang it forever. It counts from the start
/// of each wait and again from each read, so a reply that keeps arriving
/// never runs out of time, however long it takes.
const READ_DEADLINE: Duration = Duration::from_secs(30);

/// How long to sleep between polls of an empty pipe.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// The pipe and the clock, as [`Replies`] uses them.
pub(crate) trait PipeIo {
    /// The bytes waiting in the pipe, without blocking.
    fn available(&mut self) -> io::Result<u32>;
    /// Reads waiting bytes into `buf`.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>;
    /// The current time.
    fn now(&self) -> Instant;
    /// Waits `time` before the next poll.
    fn pause(&mut self, time: Duration);
}

/// Why no reply arrived.
#[derive(Debug)]
pub(crate) enum ReplyError {
    /// The daemon sent nothing for [`READ_DEADLINE`].
    Silent,
    /// The daemon closed the connection.
    Closed,
    /// Reading the pipe failed.
    Read(io::Error),
    /// The daemon sent a frame the client cannot decode, or one that does
    /// not answer the request.
    Malformed(String),
    /// The daemon answered a query with an error, such as a query it cannot
    /// parse.
    Refused(String),
}

impl fmt::Display for ReplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Silent => f.write_str("daemon did not reply in time"),
            Self::Closed => f.write_str("daemon closed the connection"),
            Self::Read(e) => e.fmt(f),
            Self::Malformed(m) | Self::Refused(m) => f.write_str(m),
        }
    }
}

/// The replies on one connection: the frame decoder and the read buffer,
/// kept across waits so no byte read early is lost.
pub(crate) struct Replies {
    decoder: FrameDecoder,
    /// On the heap so it can be large without risking the main thread's
    /// 1 MiB stack: a broad query streams hundreds of MB, and fewer, bigger
    /// reads cut syscall overhead.
    buf: Vec<u8>,
}

impl Replies {
    pub(crate) fn new() -> Self {
        Self {
            decoder: FrameDecoder::new(MAX_SERVER_FRAME),
            buf: vec![0; 256 * 1024],
        }
    }

    /// Waits for the next reply frame and decodes it.
    ///
    /// The deadline is armed here and again after each read, never across
    /// calls, so the time a caller spends writing a page out is not counted
    /// as the daemon's silence. It is checked only while the pipe is empty,
    /// so bytes already waiting are always read.
    pub(crate) fn next(&mut self, io: &mut impl PipeIo) -> Result<Response, ReplyError> {
        let mut deadline = io.now() + READ_DEADLINE;
        loop {
            match self.decoder.next_frame() {
                Ok(Some(frame)) => {
                    return decode_response_frame(&frame)
                        .map_err(|e| ReplyError::Malformed(e.to_string()));
                }
                Ok(None) => {}
                Err(e) => return Err(ReplyError::Malformed(e.to_string())),
            }
            // Poll instead of a blocking read, so the deadline holds against
            // a daemon that is alive but stuck.
            let waiting = io.available().map_err(|_| ReplyError::Closed)?;
            if waiting == 0 {
                if io.now() > deadline {
                    return Err(ReplyError::Silent);
                }
                io.pause(POLL_INTERVAL);
                continue;
            }
            let n = io.read(&mut self.buf).map_err(ReplyError::Read)?;
            if n == 0 {
                return Err(ReplyError::Closed);
            }
            self.decoder.feed(&self.buf[..n]);
            deadline = io.now() + READ_DEADLINE;
        }
    }

    /// Reads a query's reply, handing each results page to `page` as it
    /// arrives, until the last. The first error `page` returns stops the
    /// reading there, so a reader that went away costs no further page.
    pub(crate) fn query<E: From<ReplyError>>(
        &mut self,
        io: &mut impl PipeIo,
        mut page: impl FnMut(QueryResults) -> Result<(), E>,
    ) -> Result<(), E> {
        loop {
            match self.next(io)? {
                Response::Results(results) => {
                    let more = results.more;
                    page(results)?;
                    if !more {
                        return Ok(());
                    }
                }
                Response::Error { message, .. } => {
                    return Err(ReplyError::Refused(message).into());
                }
                _ => {
                    return Err(ReplyError::Malformed("unexpected reply from daemon".into()).into());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use goz_core::proto::{ProtoError, encode_response_frame};
    use std::collections::VecDeque;

    /// What the fake pipe does next.
    enum Step {
        /// These bytes become readable.
        Bytes(Vec<u8>),
        /// Nothing arrives for this long, counted from the first poll.
        Silence(Duration),
    }

    /// A pipe that plays `steps` on a fake clock. Time passes only as the
    /// reader pauses or a test moves the clock; past the last step the
    /// daemon has gone.
    struct Scripted {
        start: Instant,
        elapsed: Duration,
        steps: VecDeque<Step>,
        quiet_until: Option<Duration>,
    }

    impl Scripted {
        fn new(steps: Vec<Step>) -> Self {
            Self {
                start: Instant::now(),
                elapsed: Duration::ZERO,
                steps: steps.into(),
                quiet_until: None,
            }
        }
    }

    impl PipeIo for Scripted {
        fn available(&mut self) -> io::Result<u32> {
            loop {
                match self.steps.front() {
                    None => return Err(io::ErrorKind::BrokenPipe.into()),
                    Some(Step::Bytes(bytes)) => return Ok(bytes.len() as u32),
                    Some(Step::Silence(time)) => {
                        let until = *self.quiet_until.get_or_insert(self.elapsed + *time);
                        if self.elapsed < until {
                            return Ok(0);
                        }
                        self.quiet_until = None;
                        self.steps.pop_front();
                    }
                }
            }
        }

        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let Some(Step::Bytes(bytes)) = self.steps.pop_front() else {
                panic!("read with nothing available");
            };
            buf[..bytes.len()].copy_from_slice(&bytes);
            Ok(bytes.len())
        }

        fn now(&self) -> Instant {
            self.start + self.elapsed
        }

        fn pause(&mut self, time: Duration) {
            self.elapsed += time;
        }
    }

    /// Reply `n`, told apart from the others by the server name.
    fn reply(n: usize) -> Response {
        Response::Hello {
            proto: 1,
            server: format!("gozd {n}"),
            ready: true,
            features: Vec::new(),
        }
    }

    fn frame(n: usize) -> Step {
        bytes(&reply(n))
    }

    fn bytes(reply: &Response) -> Step {
        let mut bytes = Vec::new();
        encode_response_frame(reply, &mut bytes);
        Step::Bytes(bytes)
    }

    /// Results page `n` of three, told apart by its generation.
    fn results(n: u64) -> Step {
        bytes(&Response::Results(QueryResults {
            total: 3,
            items: Vec::new(),
            more: n < 2,
            volumes_incomplete: false,
            metadata_pending: false,
            generation: n,
            total_size: None,
        }))
    }

    /// Why a test query stopped.
    #[derive(Debug)]
    enum Stop {
        Reply(ReplyError),
        Page(io::Error),
    }

    impl From<ReplyError> for Stop {
        fn from(e: ReplyError) -> Self {
            Self::Reply(e)
        }
    }

    /// Ten replies, each after half the deadline of silence, take 150 s in
    /// all; every one arrives. A deadline armed once per reply gives up at
    /// about 30 s.
    #[test]
    fn a_reply_that_streams_past_the_deadline_still_arrives() {
        let mut steps = Vec::new();
        for n in 0..10 {
            steps.push(Step::Silence(READ_DEADLINE / 2));
            steps.push(frame(n));
        }
        let mut io = Scripted::new(steps);
        let mut replies = Replies::new();
        for n in 0..10 {
            match replies.next(&mut io) {
                Ok(got) => assert_eq!(got, reply(n)),
                Err(e) => panic!("reply {n} at {:?}: {e}", io.elapsed),
            }
        }
        assert!(io.elapsed >= READ_DEADLINE * 5, "{:?}", io.elapsed);
    }

    /// Silence just past the deadline fails the wait, though a reply would
    /// have followed.
    #[test]
    fn silence_longer_than_the_deadline_fails() {
        let mut io = Scripted::new(vec![
            frame(0),
            Step::Silence(READ_DEADLINE + Duration::from_millis(10)),
            frame(1),
        ]);
        let mut replies = Replies::new();
        assert_eq!(replies.next(&mut io).unwrap(), reply(0));
        let late = replies.next(&mut io);
        assert!(matches!(late, Err(ReplyError::Silent)), "{late:?}");
        assert_eq!(
            late.unwrap_err().to_string(),
            "daemon did not reply in time"
        );
    }

    /// A minute spent writing a page to a slow stdout is not the daemon's
    /// silence: the next reply, 5 ms into the wait, still arrives. Fails if
    /// the deadline is armed once per reply, or only at reads.
    #[test]
    fn time_spent_writing_a_page_is_not_silence() {
        let mut io = Scripted::new(vec![
            frame(0),
            Step::Silence(Duration::from_millis(5)),
            frame(1),
        ]);
        let mut replies = Replies::new();
        assert_eq!(replies.next(&mut io).unwrap(), reply(0));
        io.elapsed += READ_DEADLINE * 2;
        match replies.next(&mut io) {
            Ok(got) => assert_eq!(got, reply(1)),
            Err(e) => panic!("{e}"),
        }
    }

    /// The daemon hanging up and a read failing read as today's messages.
    #[test]
    fn a_closed_pipe_and_a_failed_read_keep_their_messages() {
        let mut replies = Replies::new();
        let closed = replies.next(&mut Scripted::new(vec![]));
        assert!(matches!(closed, Err(ReplyError::Closed)), "{closed:?}");
        assert_eq!(
            closed.unwrap_err().to_string(),
            "daemon closed the connection"
        );
        let failed = ReplyError::Read(io::Error::other("the pipe broke"));
        assert_eq!(failed.to_string(), "the pipe broke");
        let mut garbage = Scripted::new(vec![Step::Bytes(vec![1, 0, 0, 0, 9])]);
        let malformed = Replies::new().next(&mut garbage);
        assert!(
            matches!(&malformed, Err(ReplyError::Malformed(m)) if m.contains("tag 9")),
            "{malformed:?}"
        );
    }

    /// Every page reaches the caller in order, and the last one ends the
    /// reading: what follows it stays unread.
    #[test]
    fn a_query_hands_over_each_page_until_the_last() {
        let mut io = Scripted::new(vec![results(0), results(1), results(2), frame(9)]);
        let mut seen = Vec::new();
        let done = Replies::new().query(&mut io, |page| {
            seen.push(page.generation);
            Ok::<(), Stop>(())
        });
        assert!(done.is_ok(), "{done:?}");
        assert_eq!(seen, [0, 1, 2]);
        assert_eq!(io.steps.len(), 1);
    }

    /// A page that cannot be written out stops the query at once: its
    /// error comes back, and the pages after it stay unread.
    #[test]
    fn a_failed_page_stops_the_query() {
        let mut io = Scripted::new(vec![results(0), results(1), results(2)]);
        let mut calls = 0;
        let stopped = Replies::new().query(&mut io, |_| {
            calls += 1;
            Err(Stop::Page(io::ErrorKind::BrokenPipe.into()))
        });
        assert!(
            matches!(&stopped, Err(Stop::Page(e)) if e.kind() == io::ErrorKind::BrokenPipe),
            "{stopped:?}"
        );
        assert_eq!(calls, 1);
        assert_eq!(io.steps.len(), 2);
    }

    /// An error frame for a query is gozd refusing it, which exits 4; any
    /// other reply that is not a page is malformed, which exits 7.
    #[test]
    fn an_error_frame_is_a_refusal() {
        let refusal = bytes(&Response::Error {
            code: ProtoError::BadQuery,
            message: "unbalanced quote".into(),
        });
        let refused =
            Replies::new().query(&mut Scripted::new(vec![refusal]), |_| Ok::<(), Stop>(()));
        assert!(
            matches!(&refused, Err(Stop::Reply(ReplyError::Refused(m))) if m == "unbalanced quote"),
            "{refused:?}"
        );
        let odd = Replies::new().query(&mut Scripted::new(vec![frame(0)]), |_| Ok::<(), Stop>(()));
        assert!(
            matches!(&odd, Err(Stop::Reply(ReplyError::Malformed(m))) if m == "unexpected reply from daemon"),
            "{odd:?}"
        );
    }
}
