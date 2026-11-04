//! goz: the CLI client.
//!
//! Parses arguments with the es.exe-compatible rules from
//! `goz-core::escompat`, talks to the running daemon over `\\.\pipe\goz-v1`,
//! and writes results as an es-compatible CSV (`-export-csv`), JSON (`--json`),
//! or a plain path list (default). The help and version switches print to
//! stdout without contacting the daemon.
//!
//! Exit codes (es-compatible where they overlap): 0 ok · 4 missing switch
//! parameter / bad query · 5 export-file create failure / output write failure ·
//! 6 unknown switch · 7 protocol failure (malformed daemon reply, reply
//! timeout, or pipe open error) · 8 daemon unreachable · 9 pipe server not
//! trusted.

// Thread-caching allocator: decoding a broad result set allocates a String per
// path (millions of them), which the Windows default heap serializes.
#[cfg(windows)]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(windows)]
mod client;
#[cfg(windows)]
mod not_running;

use goz_core::escompat::{Command, Exit, VERSION_LINE, help, parse_argv};
use std::io::Write;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
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

/// Writes `text` to stdout. A reader closing early (`goz --help | more`) is a
/// clean stop; a genuine write failure reports exit 5.
fn print(text: &str) -> ExitCode {
    let mut out = std::io::stdout().lock();
    match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        Err(e) if e.kind() != std::io::ErrorKind::BrokenPipe => {
            eprintln!("goz: writing to stdout failed: {e}");
            Exit::WriteFailed.into()
        }
        _ => Exit::Success.into(),
    }
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
    use goz_core::output::{CsvOptions, CsvRow, filetime_to_unix_ms, write_csv};
    use goz_core::proto::{PROTO_VERSION, QueryRequest, QueryResults, Request, Response};
    use goz_core::types::EsColumn;
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
            Err(ClientError::Protocol(m)) => {
                eprintln!("goz: {m}");
                return Err(Exit::Protocol.into());
            }
        };

        if plan.server_version {
            return server_version(&mut connection);
        }
        if plan.status {
            return status(&mut connection);
        }

        let request = Request::Query(build_query(&plan));
        let results = match connection.request(&request) {
            Ok(Response::Results(r)) => r,
            Ok(Response::Error { message, .. }) => {
                eprintln!("goz: {message}");
                return Err(Exit::BadParam.into());
            }
            Ok(_) => {
                eprintln!("goz: unexpected reply from daemon");
                return Err(Exit::Protocol.into());
            }
            Err(error) => return Err(request_failed(error)),
        };

        if results.volumes_incomplete {
            eprintln!(
                "goz: warning: some volumes are still indexing or unavailable; results may be incomplete"
            );
        }

        emit(&plan, &results, export_file)
    }

    fn build_query(plan: &EsPlan) -> QueryRequest {
        QueryRequest {
            query: plan.query.clone(),
            scope: plan.scope.as_ref().map(|s| absolutize(s)),
            sort: plan.sort,
            offset: 0,
            limit: plan.limit,
            // Always fetch metadata; output decides what to show.
            want_size: true,
            want_mtime: true,
            match_case: plan.match_case,
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

    fn emit(
        plan: &EsPlan,
        results: &QueryResults,
        export_file: Option<std::fs::File>,
    ) -> Result<ExitCode, ExitCode> {
        if let Some(mut file) = export_file {
            let bytes = render_csv(plan, results);
            if let Err(e) = file.write_all(&bytes) {
                eprintln!("goz: writing export CSV failed: {e}");
                return Err(Exit::WriteFailed.into());
            }
            return Ok(ExitCode::SUCCESS);
        }

        if plan.json {
            match serde_json::to_string_pretty(results) {
                Ok(s) => {
                    // Locked writeln! that tolerates a broken pipe (`goz --json '*' | head`)
                    // instead of println!, which panics on any stdout write error.
                    let stdout = std::io::stdout();
                    let mut out = stdout.lock();
                    let _ = writeln!(out, "{s}");
                }
                Err(e) => {
                    eprintln!("goz: JSON encoding failed: {e}");
                    return Err(Exit::Protocol.into());
                }
            }
            return Ok(ExitCode::SUCCESS);
        }

        // Default: one path per line. StdoutLock is a LineWriter (it flushes on every
        // newline), so wrap it in a BufWriter to batch a large result set into a few
        // writes. A reader closing early (`goz * | head`) is a clean stop; a genuine
        // write failure reports exit 5.
        let stdout = std::io::stdout();
        let mut out = std::io::BufWriter::new(stdout.lock());
        for item in &results.items {
            if let Err(e) = writeln!(out, "{}", item.path) {
                if e.kind() == std::io::ErrorKind::BrokenPipe {
                    return Ok(ExitCode::SUCCESS);
                }
                eprintln!("goz: writing results failed: {e}");
                return Err(Exit::WriteFailed.into());
            }
        }
        if let Err(e) = out.flush()
            && e.kind() != std::io::ErrorKind::BrokenPipe
        {
            eprintln!("goz: writing results failed: {e}");
            return Err(Exit::WriteFailed.into());
        }
        Ok(ExitCode::SUCCESS)
    }

    /// Renders the results as an es-compatible CSV.
    fn render_csv(plan: &EsPlan, results: &QueryResults) -> Vec<u8> {
        let opts = CsvOptions {
            bom: plan.bom,
            header: plan.header,
            double_quote: plan.double_quote,
            columns: plan.columns.clone(),
        };

        // Pre-format dates (CsvRow borrows them). Local time, ISO-8601, so the
        // JavaScript consumer's `new Date()` parses it regardless of locale. Resolve
        // the system time zone ONCE (it is loop-invariant) instead of per row.
        let want_date = plan.columns.contains(&EsColumn::DateModified);
        let tz = want_date.then(jiff::tz::TimeZone::system);
        let dates: Vec<Option<String>> = results
            .items
            .iter()
            .map(|it| {
                tz.as_ref()
                    .and_then(|tz| it.mtime_ft.map(|ft| format_date_local(ft, tz)))
            })
            .collect();

        let rows = results.items.iter().zip(&dates).map(|(it, date)| CsvRow {
            path: &it.path,
            size: it.size,
            mtime: date.as_deref(),
            is_dir: it.is_dir,
        });

        let mut out = Vec::new();
        write_csv(&opts, rows, &mut out);
        out
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
                // Locked writeln! that tolerates a broken pipe (`goz --status | head`)
                // instead of println!, which panics on any stdout write error.
                let stdout = std::io::stdout();
                let mut out = stdout.lock();
                let _ = writeln!(out, "gozd status: {} volume(s)", s.volumes.len());
                for v in &s.volumes {
                    let mount = v.mounts.first().cloned().unwrap_or_else(|| v.guid.clone());
                    let _ = writeln!(
                        out,
                        "  {mount:<8} {:>12} entries  phase={}{}",
                        v.entries,
                        v.phase, // user-facing Display, not Debug
                        if v.metadata_pending {
                            "  (metadata pending)"
                        } else {
                            ""
                        }
                    );
                    // The journal's size is the replay horizon: everything
                    // older than what it holds costs a full rescan on restart.
                    if let Some(bytes) = v.journal_max_bytes {
                        let _ =
                            writeln!(out, "           journal max {} MiB", bytes / (1024 * 1024));
                    }
                    // Orphans make every query walk each candidate's parent
                    // chain; a healthy volume has none, so say when it does.
                    if v.has_orphans {
                        let _ = writeln!(
                            out,
                            "           orphans: entries without a full path (a parent never arrived, or its folder was removed first) sit under lost+found; queries pay a presentability walk"
                        );
                    }
                    if v.names_waiting > 0 {
                        let _ = writeln!(
                            out,
                            "           {} name(s) waiting to join the sort order",
                            v.names_waiting
                        );
                    }
                    // Drift counters, shown only when non-zero. These are the
                    // only outward sign that the index and the volume have
                    // diverged, so reporting them nowhere made a drifting index
                    // indistinguishable from a healthy one.
                    if v.link_reconciles_dropped > 0 {
                        let _ = writeln!(
                            out,
                            "           {} hard-link change(s) not reconciled: some names may be stale",
                            v.link_reconciles_dropped
                        );
                    }
                    if v.stale_slots > 0 || v.delete_of_unknown > 0 || v.placeholders_created > 0 {
                        let _ = writeln!(
                            out,
                            "           drift: {} stale slot(s), {} unknown delete(s), {} placeholder(s)",
                            v.stale_slots, v.delete_of_unknown, v.placeholders_created
                        );
                    }
                    // Index memory, component by component. `alloc` differing
                    // from `used` is capacity slack held by that component.
                    if let Some(m) = &v.memory {
                        let mb = |p: &goz_core::proto::MemPair| {
                            format!("{:.1}/{:.1}", p.used as f64 / 1e6, p.alloc as f64 / 1e6)
                        };
                        let total_used = m.entries.used
                            + m.arena_raw.used
                            + m.arena_folded.used
                            + m.frn_map.used
                            + m.name_tables.used
                            + m.dir_children.used;
                        let total_alloc = m.entries.alloc
                            + m.arena_raw.alloc
                            + m.arena_folded.alloc
                            + m.frn_map.alloc
                            + m.name_tables.alloc
                            + m.dir_children.alloc;
                        let _ = writeln!(
                            out,
                            "           mem MB used/alloc: total {:.1}/{:.1}  entries {}  names {}  folded {}  frn[{}] {}  ntables {}  dirs {}",
                            total_used as f64 / 1e6,
                            total_alloc as f64 / 1e6,
                            mb(&m.entries),
                            mb(&m.arena_raw),
                            mb(&m.arena_folded),
                            m.frn_map_kind,
                            mb(&m.frn_map),
                            mb(&m.name_tables),
                            mb(&m.dir_children),
                        );
                    }
                }
                if s.process_private_bytes > 0 {
                    let _ = writeln!(
                        out,
                        "  daemon: {:.1} MB private, {:.1} MB working set",
                        s.process_private_bytes as f64 / 1e6,
                        s.process_working_set as f64 / 1e6,
                    );
                }
                if s.volumes_incomplete {
                    eprintln!("goz: some volumes are not fully live");
                }
                Ok(ExitCode::SUCCESS)
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
            Ok(Response::Hello { server, .. }) => {
                let stdout = std::io::stdout();
                let mut out = stdout.lock();
                let _ = writeln!(out, "{server}");
                Ok(ExitCode::SUCCESS)
            }
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
