//! goz: the CLI client.
//!
//! Parses arguments with the es.exe-compatible rules from
//! `goz-core::escompat`, talks to the running daemon over `\\.\pipe\goz-v1`,
//! and writes results as an es-compatible CSV (`-export-csv`), JSON (`--json`),
//! or a plain path list (default), or only their number
//! (`-get-result-count`) or the summed size of their files
//! (`-get-total-size`). The help and version switches print to stdout
//! without contacting the daemon.
//!
//! Exit codes (es-compatible where they overlap): 0 ok · 4 missing switch
//! parameter / bad query / argument not valid Unicode · 5 export-file
//! create failure / output write failure · 6 unknown switch · 7 protocol
//! failure (malformed daemon reply, 30 s of silence from the daemon, or
//! pipe open error) · 8 daemon unreachable · 9 pipe server not trusted.

// Thread-caching allocator: decoding a broad result set allocates a String per
// path (millions of them), which the Windows default heap serializes.
#[cfg(windows)]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(windows)]
mod client;
#[cfg(windows)]
mod not_running;
#[cfg(any(windows, test))]
mod reply;
#[cfg(any(windows, test))]
mod status;

use goz_core::escompat::{Command, Exit, VERSION_LINE, help, parse_argv};
use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

fn main() -> ExitCode {
    // `std::env::args` panics on an argument that is not valid Unicode, such
    // as a lone surrogate on Windows; goz cannot use one, so it is a bad
    // parameter.
    let args = match std::env::args_os()
        .skip(1)
        .map(OsString::into_string)
        .collect::<Result<Vec<String>, OsString>>()
    {
        Ok(args) => args,
        Err(arg) => {
            eprintln!(
                "goz: an argument is not valid Unicode: {}",
                arg.to_string_lossy()
            );
            return Exit::BadParam.into();
        }
    };
    match parse_argv(&args) {
        Ok(Command::Run(plan)) => execute(plan),
        Ok(Command::Help(screen)) => print(&help(screen)),
        Ok(Command::Version) => print(&format!("{VERSION_LINE}\n")),
        Err(fatal) => {
            eprintln!("goz: {}", fatal.message);
            if fatal.exit_code == Exit::UnknownSwitch.code() {
                eprintln!("goz: run goz --help for every switch");
            }
            ExitCode::from(fatal.exit_code)
        }
    }
}

/// Writes `text` to stdout, with the exit code [`output_failed`] gives a
/// failed write.
fn print(text: &str) -> ExitCode {
    let mut out = std::io::stdout().lock();
    match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => Exit::Success.into(),
        Err(e) => output_failed(&e, "to stdout"),
    }
}

/// The exit code for output that could not be written `to` a destination
/// such as `to stdout`. A reader closing early (`goz --help | more`) is a
/// clean stop; any other failure is reported and exits 5.
fn output_failed(error: &std::io::Error, to: &str) -> ExitCode {
    if error.kind() == std::io::ErrorKind::BrokenPipe {
        return Exit::Success.into();
    }
    eprintln!("goz: writing {to} failed: {error}");
    Exit::WriteFailed.into()
}

#[cfg(not(windows))]
fn execute(_plan: goz_core::escompat::EsPlan) -> ExitCode {
    eprintln!("goz: this client requires Windows (it talks to the gozd daemon)");
    Exit::NotRunning.into()
}

#[cfg(windows)]
use win::execute;

#[cfg(windows)]
mod win {
    use super::client::{Client, ClientError};
    use goz_core::escompat::{EsPlan, Exit, VERSION_LINE};
    use goz_core::output::{CsvOptions, Format, ResultWriter, filetime_to_unix_ms};
    use goz_core::proto::{PROTO_VERSION, QueryRequest, QueryResults, Request, Response};
    use goz_core::types::{EsColumn, SortSpec};
    use std::io::Write;
    use std::process::ExitCode;

    /// es's line for a missing Everything, kept verbatim for consumers that
    /// grep stderr.
    const ES_NOT_RUNNING: &str =
        "Error 8: Everything IPC window not found. Please make sure Everything is running.";

    pub(crate) fn execute(plan: EsPlan) -> ExitCode {
        match run(plan) {
            Ok(code) => code,
            Err(code) => code,
        }
    }

    fn run(plan: EsPlan) -> Result<ExitCode, ExitCode> {
        // Create the export file up front (es semantics): a later daemon-unreachable
        // failure then leaves the observable artifact, a 0-byte file + exit 8.
        let export_file = match &plan.export_csv {
            Some(path) => match std::fs::File::create(path) {
                Ok(f) => Some(f),
                Err(e) => {
                    eprintln!("goz: cannot create {path}: {e}");
                    return Err(Exit::WriteFailed.into());
                }
            },
            None => None,
        };

        let mut connection = match Client::connect(plan.insecure_no_server_check) {
            Ok(c) => c,
            Err(ClientError::Unreachable) => {
                // es's line, then the goz service's state and its fix.
                eprintln!("{ES_NOT_RUNNING}");
                eprint!("{}", super::not_running::advice());
                return Err(Exit::NotRunning.into());
            }
            Err(ClientError::Untrusted) => {
                eprintln!(
                    "goz: the pipe server is not the trusted gozd daemon (owner is not SYSTEM/Administrators)."
                );
                eprintln!(
                    "goz: refusing to send the query. If this is expected, pass --insecure-no-server-check."
                );
                return Err(Exit::Untrusted.into());
            }
            Err(error) => return Err(request_failed(error)),
        };

        if plan.server_version {
            return server_version(&mut connection);
        }
        if plan.status {
            return status(&mut connection);
        }

        let query = build_query(&plan);
        if !plan.result_count && !plan.total_size {
            return stream(&plan, &mut connection, query, export_file);
        }

        // A count or a total size comes in one page with no rows.
        let mut warning = IncompleteWarning::default();
        let mut head = None;
        connection
            .query(query, |page| {
                warning.check(&page);
                head.get_or_insert(page);
                Ok::<(), ClientError>(())
            })
            .map_err(request_failed)?;
        let results = head.expect("a query that succeeds delivers a page");
        if plan.result_count {
            return number_only(&plan, results.total, export_file);
        }
        // A gozd predating the field ignores the ask; 0 would be a lie.
        let Some(size) = results.total_size else {
            eprintln!(
                "goz: the running gozd does not report total sizes; restart the goz service to run the installed version"
            );
            return Err(Exit::Protocol.into());
        };
        if results.metadata_pending {
            eprintln!("goz: warning: some file sizes are not read yet; the total leaves them out");
        }
        if size == u64::MAX {
            eprintln!("goz: warning: the total is at least {size} bytes, the most goz reports");
        }
        number_only(&plan, size, export_file)
    }

    /// Why a streamed search stopped before its last page.
    enum Stop {
        /// The daemon's reply failed.
        Client(ClientError),
        /// A page could not be written out.
        Output(std::io::Error),
    }

    impl From<ClientError> for Stop {
        fn from(e: ClientError) -> Self {
            Self::Client(e)
        }
    }

    /// The warning that volumes are still indexing, printed once, when the
    /// first page that says so arrives and before its rows.
    #[derive(Default)]
    struct IncompleteWarning {
        printed: bool,
    }

    impl IncompleteWarning {
        fn check(&mut self, page: &QueryResults) {
            if page.volumes_incomplete && !self.printed {
                self.printed = true;
                eprintln!(
                    "goz: warning: some volumes are still indexing or unavailable; results may be incomplete"
                );
            }
        }
    }

    /// Writes the results to the export file as CSV, or to stdout as JSON or
    /// one path per line, a page at a time as the pages arrive, so the
    /// client's memory stays at one page and the first rows show at once. A
    /// failure partway leaves what was written, and the exit code says the
    /// output is incomplete.
    fn stream(
        plan: &EsPlan,
        connection: &mut Client,
        query: QueryRequest,
        export_file: Option<std::fs::File>,
    ) -> Result<ExitCode, ExitCode> {
        let format = match &export_file {
            Some(_) => Format::Csv(CsvOptions {
                bom: plan.bom,
                header: plan.header,
                double_quote: plan.double_quote,
                columns: plan.columns.clone(),
            }),
            None if plan.json => Format::Json { limit: plan.limit },
            None => Format::Plain,
        };
        // Local time, ISO 8601, so the JavaScript consumer's `new Date()`
        // parses it regardless of locale. The time zone is resolved once,
        // and only when a date is shown.
        let want_date = export_file.is_some() && plan.columns.contains(&EsColumn::DateModified);
        let tz = want_date.then(jiff::tz::TimeZone::system);
        let date = |ft| {
            tz.as_ref()
                .map_or_else(String::new, |tz| format_date_local(ft, tz))
        };
        let mut writer = ResultWriter::new(format, date);

        // A plain page ends in LF, so stdout's LineWriter hands it on in one
        // write; of a JSON page it keeps back only the part after the last LF.
        let (mut out, to): (Box<dyn Write>, &str) = match export_file {
            Some(file) => (Box::new(file), "export CSV"),
            None => (Box::new(std::io::stdout().lock()), "results"),
        };
        let mut warning = IncompleteWarning::default();
        let mut buf = Vec::new();
        let fetched = connection.query(query, |page| {
            warning.check(&page);
            buf.clear();
            writer.page(&page, &mut buf);
            out.write_all(&buf).map_err(Stop::Output)
        });
        match fetched {
            Ok(()) => {}
            Err(Stop::Output(e)) => return Err(super::output_failed(&e, to)),
            Err(Stop::Client(e)) => {
                // Keep the pages already written: the exit code marks them
                // incomplete.
                let _ = out.flush();
                return Err(request_failed(e));
            }
        }
        buf.clear();
        writer.finish(&mut buf);
        if let Err(e) = out.write_all(&buf).and_then(|()| out.flush()) {
            return Err(super::output_failed(&e, to));
        }
        Ok(ExitCode::SUCCESS)
    }

    fn build_query(plan: &EsPlan) -> QueryRequest {
        // A count or a total size fetches no rows, in name order as es
        // resets it to, which the daemon can answer from the distinct names
        // alone.
        let (sort, limit) = if plan.result_count || plan.total_size {
            (SortSpec::default(), Some(0))
        } else {
            (plan.sort, plan.limit)
        };
        QueryRequest {
            query: plan.query.clone(),
            scope: plan.scope.as_ref().map(|s| absolutize(s)),
            sort,
            offset: 0,
            limit,
            // Always fetch metadata; output decides what to show.
            want_size: true,
            want_mtime: true,
            match_case: plan.match_case,
            want_total_size: plan.total_size && !plan.result_count,
        }
    }

    /// Resolves a `-path` value against the CWD without touching the filesystem, so
    /// the daemon receives an absolute `X:\...` path to match a mount prefix. On
    /// failure the original value is forwarded, but with a warning: a non-absolute
    /// scope will not match any mount prefix and yields no results, so a silent
    /// fallback would look like "the folder is empty".
    fn absolutize(path: &str) -> String {
        match std::path::absolute(path)
            .ok()
            .and_then(|p| p.to_str().map(str::to_owned))
        {
            Some(abs) => abs,
            None => {
                eprintln!(
                    "goz: warning: could not resolve '{path}' to an absolute path; scope may not match"
                );
                path.to_owned()
            }
        }
    }

    /// `-get-result-count` and `-get-total-size`: the number alone, on stdout
    /// or, as es writes it, in the export file after its byte order mark.
    fn number_only(
        plan: &EsPlan,
        number: impl std::fmt::Display,
        export_file: Option<std::fs::File>,
    ) -> Result<ExitCode, ExitCode> {
        let Some(mut file) = export_file else {
            return Ok(super::print(&format!("{number}\n")));
        };
        let mut bytes = Vec::new();
        if plan.bom {
            bytes.extend_from_slice(b"\xEF\xBB\xBF");
        }
        bytes.extend_from_slice(format!("{number}\r\n").as_bytes());
        if let Err(e) = file.write_all(&bytes) {
            return Err(super::output_failed(&e, "export CSV"));
        }
        Ok(ExitCode::SUCCESS)
    }

    /// Formats a Windows FILETIME as local `YYYY-MM-DDTHH:MM:SS` in `tz` (resolved
    /// once by the caller). The `TimeZone` handle is cheap (Arc-backed) to clone.
    fn format_date_local(filetime: i64, tz: &jiff::tz::TimeZone) -> String {
        let ms = filetime_to_unix_ms(filetime);
        match jiff::Timestamp::from_millisecond(ms) {
            Ok(ts) => ts
                .to_zoned(tz.clone())
                .strftime("%Y-%m-%dT%H:%M:%S")
                .to_string(),
            Err(_) => String::new(),
        }
    }

    fn status(connection: &mut Client) -> Result<ExitCode, ExitCode> {
        match connection.request(&Request::Status) {
            Ok(Response::Status(s)) => {
                let code = super::print(&super::status::render(&s));
                if s.volumes_incomplete {
                    eprintln!("goz: some volumes are not fully live");
                }
                Ok(code)
            }
            Ok(_) => {
                eprintln!("goz: unexpected reply to status");
                Err(Exit::Protocol.into())
            }
            Err(error) => Err(request_failed(error)),
        }
    }

    /// `--server-version`: asks gozd for its version with a `Hello` and
    /// prints the name and version it answers with, such as `gozd 0.4.4`.
    fn server_version(connection: &mut Client) -> Result<ExitCode, ExitCode> {
        let hello = Request::Hello {
            proto_min: PROTO_VERSION,
            proto_max: PROTO_VERSION,
            client: VERSION_LINE.to_owned(),
        };
        match connection.request(&hello) {
            Ok(Response::Hello { server, .. }) => Ok(super::print(&format!("{server}\n"))),
            // A daemon that speaks another protocol refuses the Hello.
            Ok(Response::Error { message, .. }) => {
                eprintln!("goz: {message}");
                Err(Exit::Protocol.into())
            }
            Ok(_) => {
                eprintln!("goz: unexpected reply to hello");
                Err(Exit::Protocol.into())
            }
            Err(error) => Err(request_failed(error)),
        }
    }

    /// Reports a request that got no usable reply, and its exit code.
    fn request_failed(error: ClientError) -> ExitCode {
        match error {
            ClientError::Protocol(m) => {
                eprintln!("goz: {m}");
                Exit::Protocol.into()
            }
            ClientError::Refused(m) => {
                eprintln!("goz: {m}");
                Exit::BadParam.into()
            }
            ClientError::Unreachable => {
                eprintln!("{ES_NOT_RUNNING}");
                Exit::NotRunning.into()
            }
            ClientError::Untrusted => {
                eprintln!("goz: pipe server not trusted");
                Exit::Untrusted.into()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind};

    /// `goz ... | head` closing the pipe early is not a failure.
    #[test]
    fn a_closed_reader_is_a_clean_stop() {
        let closed = Error::from(ErrorKind::BrokenPipe);
        assert_eq!(output_failed(&closed, "results"), ExitCode::SUCCESS);
    }

    /// Any other failed write, a full disk or a handle opened read only,
    /// exits 5 rather than 0 with output missing.
    #[test]
    fn a_failed_write_exits_5() {
        let denied = Error::from(ErrorKind::PermissionDenied);
        assert_eq!(
            output_failed(&denied, "to stdout"),
            ExitCode::from(Exit::WriteFailed.code())
        );
    }
}
