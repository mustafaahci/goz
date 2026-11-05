//! The named-pipe query server.
//!
//! A tokio multi-thread runtime accepts connections on `\\.\pipe\goz-v1`,
//! frames requests/responses with the shared sans-io codec from
//! `goz-core::proto`, and answers each `Query` by locking every volume's index
//! for reads, running the query, and merging the results. Indexing itself
//! (bootstrap + tail) stays on dedicated OS threads; tokio only serves the pipe.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use goz_core::fold::fold;
use goz_core::index::{PathStatus, VolumeIndex};
use goz_core::proto::{
    DaemonStatus, MemPair, PIPE_NAME, PROTO_VERSION, ProtoError, Request, Response, VolumeMemory,
    VolumeStatus,
};
use goz_core::proto::{
    MAX_CLIENT_FRAME, MAX_SERVER_FRAME, PAGE_ROWS, QueryRequest, encode_response_frame,
    patch_results_header, push_item, push_results_header,
};
use goz_core::query::{HitRef, mtime_key, parse_query_at, resolve_scope, run_query_refs, size_key};
use goz_core::types::{EntryIdx, SortDir, SortKey, SortSpec};
use goz_winfs::{PipeSecurity, build_pipe_security};
use rustc_hash::FxHashMap;
use std::cmp::Ordering;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::{Semaphore, watch};

use crate::local_clock::MachineClock;
use crate::volume_state::{VolumeRegistry, VolumeSet, VolumeState};

/// Cap on concurrently-served pipe connections. A silent/hostile local client
/// cannot pin more than this many tasks + read buffers + pipe instances.
const MAX_CONNECTIONS: usize = 64;
/// Pages in flight between the query encoder and the pipe writer. Each page
/// is ~[`PAGE_ROWS`] rows (~450 KB), so the backlog bounds a full export's
/// buffered bytes at ~2 MB where it used to buffer the entire response.
const QUERY_PAGE_BACKLOG: usize = 4;
/// Idle/half-open connections are dropped after this long with no request, so a
/// connected-but-silent client cannot block a task forever. The one-shot CLI
/// client always writes immediately after connecting, well within this window.
/// A connection with a reply out waits [`WRITE_TIMEOUT`] instead (see
/// [`handle_connection`]).
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// A client that leaves a reply page untaken this long is dropped, so one that
/// stops reading cannot hold its result set and connection slot forever. A
/// write returns once the pipe has buffered the page, so this bounds the wait
/// for the page before it, and the wait for the client's next request covers
/// the last one. Long, so a client that streams pages to a console its user
/// has paused is not cut off. goz's own CLI streams pages to its output, so a
/// paused console or reader holds the reply here.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Pause after a failed pipe connect before listening again. A connect fails
/// only on an unexpected `ConnectNamedPipe` error (a client that closed before
/// the server saw it counts as connected), and such an error tends to repeat,
/// so this holds the accept loop to about 100 turns a second instead of a busy
/// loop. A client that connects during the pause waits for it, not longer: the
/// next instance already exists.
const CONNECT_RETRY_PAUSE: Duration = Duration::from_millis(10);
/// At most one log line per this period for failed connects. It counts the
/// failures since the last line, so the service log cannot grow with them.
const CONNECT_WARN_EVERY: Duration = Duration::from_secs(60);
/// A results page ends once its frame passes this many bytes, half the
/// client's [`MAX_SERVER_FRAME`]. One row is at most `MAX_PATH_HOPS` names of
/// up to 255 UTF-16 units each, about 3 MiB, so the row that crosses the budget
/// still fits under the cap. Cutting by row count alone let [`PAGE_ROWS`] rows
/// of 4 KB paths, which any local user can create, build a page the client
/// refuses.
const PAGE_BYTE_BUDGET: usize = MAX_SERVER_FRAME as usize / 2;
/// How often to try the pipe name again while another process holds it.
const CLAIM_RETRY_EVERY: Duration = Duration::from_secs(2);
/// `CreateNamedPipe` errors that mean another process already holds the name:
/// `FIRST_PIPE_INSTANCE` is refused with access denied, and a holder that
/// allowed one instance reports every instance busy.
const ERROR_ACCESS_DENIED: i32 = 5;
const ERROR_PIPE_BUSY: i32 = 231;

/// Runs the pipe server until stopped. Blocks on a tokio runtime. Each request
/// answers from the volumes indexed when it arrives.
///
/// `ready` runs once, as soon as the pipe is claimed, or as soon as the claim
/// has to wait for another process to release the name. Service mode reports
/// `Running` from it: the SCM delivers no control to a service that is still
/// `StartPending`, so one held there while a squatter keeps the name could
/// not be stopped. Waiting for the name itself is unchanged.
///
/// Stops on either Ctrl+C (console mode) or `shutdown` flipping to `true` (the
/// service-control handler's Stop/Shutdown path). Keep at least one
/// `watch::Sender` alive for the duration in console mode: if every sender
/// drops, `changed()` resolves and would end the loop prematurely.
pub(crate) fn run(
    volumes: Arc<VolumeRegistry>,
    shutdown: watch::Receiver<bool>,
    ready: impl FnOnce(),
) -> Result<()> {
    let security = build_pipe_security().context("building pipe security descriptor")?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;
    rt.block_on(accept_loop(volumes, security, shutdown, ready))
}

async fn accept_loop(
    volumes: Arc<VolumeRegistry>,
    security: PipeSecurity,
    mut shutdown: watch::Receiver<bool>,
    ready: impl FnOnce(),
) -> Result<()> {
    let mut ready = Some(ready);
    let Some(mut server) = claim_first_instance(&security, &mut shutdown, &mut ready).await? else {
        return Ok(());
    };
    if let Some(ready) = ready.take() {
        ready();
    }
    let conn_limit = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let mut connect_failures = 0u64;
    let mut last_connect_warning: Option<Instant> = None;
    tracing::info!(pipe = PIPE_NAME, "query server listening");
    loop {
        tokio::select! {
            r = server.connect() => {
                if let Err(e) = r {
                    // A failed connect ends only that connection. Returning
                    // here stopped the server for every user until the next
                    // boot. The new instance is created before the old one
                    // drops, so the pipe always has a listener.
                    connect_failures += 1;
                    if last_connect_warning.is_none_or(|t| t.elapsed() >= CONNECT_WARN_EVERY) {
                        tracing::warn!(
                            error = %e,
                            failures = connect_failures,
                            "pipe connect failed; still listening"
                        );
                        connect_failures = 0;
                        last_connect_warning = Some(Instant::now());
                    }
                    server = create_instance(&security, false)?;
                    tokio::time::sleep(CONNECT_RETRY_PAUSE).await;
                    continue;
                }
                let connected = server;
                // Create the next instance before handling, so there is never
                // a window with no listener for a client to race into.
                server = create_instance(&security, false)?;
                // Bound concurrent connections. try_acquire (non-blocking) keeps
                // the accept loop responsive to shutdown under load: at the cap
                // we drop the newest connection rather than pin an extra one.
                match Arc::clone(&conn_limit).try_acquire_owned() {
                    Ok(permit) => {
                        let vols = volumes.clone();
                        tokio::spawn(async move {
                            let _permit = permit; // held for the connection's lifetime
                            let mut connected = connected;
                            if let Err(e) = handle_connection(&mut connected, vols).await {
                                tracing::debug!(error = %e, "connection ended");
                            }
                            // Dropping the instance cancels only reads and
                            // connects, so a page the client never took keeps
                            // the handle and its buffer alive until the client
                            // reads or closes, outside MAX_CONNECTIONS.
                            // Disconnecting fails that write and frees both.
                            let _ = connected.disconnect();
                        });
                    }
                    Err(_) => {
                        tracing::warn!(max = MAX_CONNECTIONS, "connection limit reached; dropping client");
                        drop(connected); // closes/disconnects this pipe instance
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("Ctrl+C received; shutting down");
                return Ok(());
            }
            changed = shutdown.changed() => {
                // Err = all senders dropped; either way, honour the stop.
                if changed.is_err() || *shutdown.borrow() {
                    tracing::info!("stop requested; shutting down");
                    return Ok(());
                }
            }
        }
    }
}

fn create_instance(security: &PipeSecurity, first: bool) -> Result<NamedPipeServer> {
    let mut opts = ServerOptions::new();
    opts.first_pipe_instance(first);
    opts.reject_remote_clients(true);
    // SAFETY: `security.attributes_ptr()` points at a live SECURITY_ATTRIBUTES
    // owned by `security`, which outlives this call and every returned instance.
    let server =
        unsafe { opts.create_with_security_attributes_raw(PIPE_NAME, security.attributes_ptr()) }
            .with_context(|| format!("creating pipe instance {PIPE_NAME}"))?;
    Ok(server)
}

/// Creates the first pipe instance, waiting while another process holds the
/// name.
///
/// Any local user can create [`PIPE_NAME`] while gozd builds its indexes (see
/// `goz_winfs::pipe_trust`), and the first instance is then refused. Failing
/// here would make the SCM restart the service and read every MFT again about
/// once a minute for as long as the name is held, so the daemon waits instead,
/// indexing meanwhile, and the server starts once the name is free. `ready`
/// runs when the wait begins. Any other error is returned. `None` when a stop
/// arrives first.
async fn claim_first_instance(
    security: &PipeSecurity,
    shutdown: &mut watch::Receiver<bool>,
    ready: &mut Option<impl FnOnce()>,
) -> Result<Option<NamedPipeServer>> {
    let mut waiting = false;
    loop {
        match create_instance(security, true) {
            Ok(server) => {
                if waiting {
                    tracing::info!(pipe = PIPE_NAME, "pipe name is free again");
                }
                return Ok(Some(server));
            }
            Err(e) if name_taken(&e) => {
                if !waiting {
                    tracing::warn!(
                        pipe = PIPE_NAME,
                        error = %e,
                        "another process holds the pipe name; waiting for it"
                    );
                    waiting = true;
                    if let Some(ready) = ready.take() {
                        ready();
                    }
                }
            }
            Err(e) => return Err(e),
        }
        tokio::select! {
            () = tokio::time::sleep(CLAIM_RETRY_EVERY) => {}
            _ = tokio::signal::ctrl_c() => return Ok(None),
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Ok(None);
                }
            }
        }
    }
}

/// Whether creating an instance failed because another process holds the name.
fn name_taken(e: &anyhow::Error) -> bool {
    e.downcast_ref::<std::io::Error>()
        .and_then(std::io::Error::raw_os_error)
        .is_some_and(|code| code == ERROR_ACCESS_DENIED || code == ERROR_PIPE_BUSY)
}

async fn handle_connection(
    server: &mut NamedPipeServer,
    volumes: Arc<VolumeRegistry>,
) -> Result<()> {
    let mut decoder = goz_core::proto::FrameDecoder::new(MAX_CLIENT_FRAME);
    let mut read_buf = vec![0u8; 16 * 1024];
    // Reap idle/half-open connections: a connected client that never sends a
    // request is dropped after IDLE_TIMEOUT, freeing its task/buffer/pipe. Once
    // a reply is out the wait is WRITE_TIMEOUT: the pipe can still hold the
    // reply's last page, which a client streaming to a paused console takes
    // late, and the disconnect that follows a timeout discards it.
    let mut wait = IDLE_TIMEOUT;
    loop {
        let n = match tokio::time::timeout(wait, server.read(&mut read_buf)).await {
            Ok(r) => r?,
            Err(_) => return Ok(()),
        };
        if n == 0 {
            return Ok(());
        }
        decoder.feed(&read_buf[..n]);
        loop {
            let frame = match decoder.next_frame() {
                Ok(Some(f)) => f,
                Ok(None) => break,
                Err(e) => return Err(anyhow::anyhow!("frame error: {e}")),
            };
            let out = match goz_core::proto::decode_request(&frame) {
                // Query execution plus result serialization is the heaviest CPU
                // work in the process and holds a read lock, so run it on the
                // blocking pool rather than stalling an async worker (and thus
                // the accept loop). Pages STREAM through a bounded channel: a
                // multi-million-row export never exists as one buffer (it
                // peaked at ~600 MB of response bytes), at most
                // QUERY_PAGE_BACKLOG pages are in flight, and encoding
                // overlaps the pipe writes instead of preceding them.
                Ok(Request::Query(q)) => {
                    let received = Instant::now();
                    let shape = QueryShape::of(&q);
                    let vols = volumes.current();
                    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(QUERY_PAGE_BACKLOG);
                    let encoder = tokio::task::spawn_blocking(move || {
                        let queued = received.elapsed();
                        // A failed send means the receiver is gone (client
                        // vanished or write error): stop encoding.
                        let timing = stream_query_binary(&vols, q, &mut |page| {
                            tx.blocking_send(page).is_ok()
                        });
                        (queued, timing)
                    });
                    let mut write_err: Option<std::io::Error> = None;
                    let mut writing = Duration::ZERO;
                    let mut first_written = None;
                    while let Some(page) = rx.recv().await {
                        let write = Instant::now();
                        if let Err(e) = write_reply(server, &page).await {
                            write_err = Some(e);
                            break;
                        }
                        writing += write.elapsed();
                        first_written.get_or_insert_with(|| received.elapsed());
                    }
                    // Closing the channel unblocks a still-encoding task; it
                    // aborts at its next emit.
                    drop(rx);
                    let (queued, timing) = encoder.await.context("query task panicked")?;
                    log_query(
                        &shape,
                        &timing,
                        queued,
                        first_written,
                        writing,
                        received.elapsed(),
                    );
                    if let Some(e) = write_err {
                        return Err(e.into());
                    }
                    wait = WRITE_TIMEOUT;
                    continue;
                }
                // Hello / Status are trivial; encode inline as a tagged JSON frame.
                Ok(req) => {
                    let mut out = Vec::new();
                    encode_response_frame(&handle_request(&volumes.current(), req), &mut out);
                    out
                }
                Err(e) => {
                    let mut out = Vec::new();
                    encode_response_frame(
                        &Response::Error {
                            code: ProtoError::BadQuery,
                            message: format!("malformed request: {e}"),
                        },
                        &mut out,
                    );
                    out
                }
            };
            write_reply(server, &out).await?;
            wait = WRITE_TIMEOUT;
        }
    }
}

/// Writes reply bytes to the client, failing with `TimedOut` when the client
/// has not taken them within [`WRITE_TIMEOUT`].
async fn write_reply(server: &mut NamedPipeServer, bytes: &[u8]) -> std::io::Result<()> {
    tokio::time::timeout(WRITE_TIMEOUT, server.write_all(bytes))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "client stopped reading"))?
}

/// Collects every streamed page into one buffer: the tests' view of
/// [`stream_query_binary`] (production writes each page to the pipe as it is
/// encoded and never holds the whole response).
#[cfg(test)]
fn encode_query_binary(volumes: &VolumeSet, q: QueryRequest) -> Vec<u8> {
    let mut out = Vec::new();
    let _ = stream_query_binary(volumes, q, &mut |page| {
        out.extend_from_slice(&page);
        true
    });
    out
}

/// Runs a query and emits the framed response as it is produced: one or more
/// binary results-page frames (each ends at [`PAGE_ROWS`] rows or once it
/// passes [`PAGE_BYTE_BUDGET`] bytes, `more` set on all but the last), or a
/// single tagged JSON error frame if the query failed to parse. An
/// empty result set still emits one `more: false` page so the client always sees
/// a terminal frame. Each framed page is handed to `emit` as soon as it is
/// encoded. `emit` returning `false` means the consumer is gone; encoding stops.
///
/// A result set is held as 16-byte [`HitRef`]s per volume, in sort order. A
/// page's paths, sizes and dates are read from the index while it is encoded,
/// under a read lock per page, and each row's slot is checked against its FRN
/// first (a journal apply since the scan may have reused it; such rows are
/// dropped). Holding every hit's full path instead peaked at ~1 GB on a 3M-row
/// export and grew with folder depth; now the set costs ~50 MB and the first
/// page leaves after the scan and sort instead of after every path.
fn stream_query_binary(
    volumes: &VolumeSet,
    q: QueryRequest,
    emit: &mut dyn FnMut(Vec<u8>) -> bool,
) -> QueryTiming {
    let mut timing = QueryTiming::default();
    let parsing = Instant::now();
    // Read as the query arrives, so its `dm:` dates count from now.
    let parsed = parse_query_at(&q.query, q.match_case, &MachineClock::read());
    timing.parse = parsing.elapsed();
    let parsed = match parsed {
        Ok(p) => p,
        Err(e) => {
            let mut out = Vec::new();
            encode_response_frame(
                &Response::Error {
                    code: ProtoError::BadQuery,
                    message: e.to_string(),
                },
                &mut out,
            );
            let _ = emit(out);
            return timing;
        }
    };

    let per_vol_limit = q.limit.map(|l| q.offset.saturating_add(l));
    let targets = targets(volumes, &q.scope);

    // One sorted run of references per volume with hits, plus that volume.
    let mut runs: Vec<Run> = Vec::new();
    let mut vols: Vec<Arc<VolumeState>> = Vec::new();
    let mut total: u64 = 0;
    let mut total_size: Option<u64> = q.want_total_size.then_some(0);
    let mut volumes_incomplete = false;
    let mut metadata_pending = false;

    for (vol, scope) in &targets {
        if !vol.phase.read().is_complete() {
            volumes_incomplete = true;
        }
        if *vol.metadata_pending.read() {
            metadata_pending = true;
        }
        // Resolve the scope and run the query under ONE read guard, so a
        // concurrent rescan cannot swap the index between the two steps.
        let waiting = Instant::now();
        let index = vol.index.read();
        timing.lock_wait += waiting.elapsed();
        let matching = Instant::now();
        let scope_entry = match scope {
            VolScope::All => None,
            VolScope::Rel(rel) => match resolve_scope_cached(vol, &index, rel, parsed.match_case) {
                Some(e) => Some(e),
                None => continue, // scope folder absent on this volume: no hits
            },
        };
        // Ordered under the lock: the comparators read folded names straight
        // from the index and allocate nothing, so a 3M-row set orders in
        // ~160 ms across cores, against ~1 s the lock used to be held while
        // every row's path was built here.
        let outcome = run_query_refs(
            &index,
            vol.mount_prefix(),
            &parsed,
            scope_entry,
            q.sort,
            0,
            per_vol_limit,
            q.want_total_size,
        );
        drop(index);
        timing.matching += matching.elapsed();
        timing.volumes += 1;
        total += outcome.total;
        if let (Some(sum), Some(size)) = (total_size.as_mut(), outcome.total_size) {
            *sum = sum.saturating_add(size);
        }
        if !outcome.refs.is_empty() {
            runs.push(Run::new(outcome.refs));
            vols.push(vol.clone());
        }
    }

    let prefixes: Vec<&str> = vols.iter().map(|v| v.mount_prefix()).collect();
    let turn = match q.sort.key {
        SortKey::Path => volumes_in_turn(&prefixes, q.sort.dir),
        _ => None,
    };
    let n_total: usize = runs.iter().map(|r| r.refs.len()).sum();
    let start = (q.offset as usize).min(n_total);
    let end = q
        .limit
        .map_or(n_total, |l| (start + l as usize).min(n_total));

    let page = PAGE_ROWS as usize;
    let mut consumed = 0usize; // merged rows taken so far, emitted or not
    let mut path = Vec::new();
    loop {
        // Every run's volume is read-locked for the page, in volume-set order
        // (the order every query uses, so readers never wait on each other in
        // a cycle), and released before the page is handed off, since `emit`
        // may block on the pipe.
        let waiting = Instant::now();
        let guards: Vec<IndexGuard<'_>> = vols.iter().map(|v| v.index.read()).collect();
        timing.page_lock_wait += waiting.elapsed();
        let building = Instant::now();
        // Frame = 4-byte LE length placeholder + payload, both backfilled once
        // the rows are in (the count is known only then). ~96 bytes/row covers
        // a typical path + fields without a mid-page regrow.
        let mut out = Vec::with_capacity(page * 96 + 64);
        out.extend_from_slice(&[0u8; 4]);
        let header_at = out.len();
        push_results_header(
            &mut out,
            total,
            0, // generation: unused by the CLI, matches the prior wire
            false,
            volumes_incomplete,
            metadata_pending,
            total_size,
            0,
        );
        let mut count = 0u32;
        let mut taken = 0usize;
        while taken < page && consumed < end && out.len() < PAGE_BYTE_BUDGET {
            let Some(ri) = next_head(
                &mut runs,
                &guards,
                &vols,
                q.sort,
                turn.as_deref(),
                &mut consumed,
            ) else {
                break;
            };
            let href = runs[ri].take_head();
            consumed += 1;
            taken += 1;
            if consumed <= start {
                continue; // an offset row: merged in order, then skipped
            }
            let index = &*guards[ri];
            // `next_head` proved the head still is this file.
            let entry = index.entry(href.idx);
            let presentable = runs[ri].row_path(
                index,
                entry.parent(),
                entry.name(),
                vols[ri].mount_prefix(),
                &mut path,
            );
            if !presentable {
                continue;
            }
            let size = q.want_size.then(|| entry.size()).flatten();
            let mtime = q.want_mtime.then(|| entry.mtime()).flatten();
            push_item(&mut out, &path, entry.is_dir(), size, mtime);
            count += 1;
        }
        drop(guards);
        let more = consumed < end;
        patch_results_header(&mut out[header_at..], more, count);
        let payload_len = (out.len() - 4) as u32;
        out[0..4].copy_from_slice(&payload_len.to_le_bytes());
        timing.rows += building.elapsed();
        timing.pages += 1;
        timing.rows_sent += u64::from(count);
        timing.total = total;
        let handing = Instant::now();
        let handed = emit(out);
        timing.handoff += handing.elapsed();
        if !handed || !more {
            return timing;
        }
    }
}

/// Where one query's time went inside [`stream_query_binary`]: an `Instant`
/// read per step, logged by [`log_query`].
#[derive(Debug, Default)]
struct QueryTiming {
    /// Parsing the query text.
    parse: Duration,
    /// Waiting for each searched volume's read lock before matching.
    lock_wait: Duration,
    /// Resolving the scope, matching and ordering, under those locks.
    matching: Duration,
    /// Waiting for the read locks each page is built under.
    page_lock_wait: Duration,
    /// Building and encoding the rows of every page.
    rows: Duration,
    /// Handing pages to the pipe writer, which waits when the client reads
    /// slower than gozd encodes.
    handoff: Duration,
    /// Volumes searched.
    volumes: u32,
    pages: u32,
    rows_sent: u64,
    /// Matches, whatever the limit.
    total: u64,
}

/// What a query asked for, as far as its log line says: not the query text,
/// since the service's log is readable by every user of the machine.
struct QueryShape {
    query_len: usize,
    scoped: bool,
    sort: SortSpec,
    limit: Option<u32>,
}

impl QueryShape {
    fn of(q: &QueryRequest) -> Self {
        Self {
            query_len: q.query.len(),
            scoped: q.scope.is_some(),
            sort: q.sort,
            limit: q.limit,
        }
    }
}

/// Logs where one query's time went, under the `gozd::query` target at
/// debug level, which `RUST_LOG=info,gozd::query=debug` turns on. `queued`
/// is the wait for a blocking-pool thread, `first_written` when the first
/// page reached the pipe, `writing` the time spent writing pages to it and
/// `all` the whole answer, each counted from when the request was read.
fn log_query(
    shape: &QueryShape,
    timing: &QueryTiming,
    queued: Duration,
    first_written: Option<Duration>,
    writing: Duration,
    all: Duration,
) {
    let ms = |d: Duration| (d.as_secs_f64() * 100_000.0).round() / 100.0;
    tracing::debug!(
        target: "gozd::query",
        query_len = shape.query_len,
        scoped = shape.scoped,
        sort = ?shape.sort,
        limit = ?shape.limit,
        volumes = timing.volumes,
        total = timing.total,
        rows = timing.rows_sent,
        pages = timing.pages,
        all_ms = ms(all),
        first_page_ms = first_written.map(ms),
        queued_ms = ms(queued),
        parse_ms = ms(timing.parse),
        lock_wait_ms = ms(timing.lock_wait),
        match_ms = ms(timing.matching),
        page_lock_ms = ms(timing.page_lock_wait),
        rows_ms = ms(timing.rows),
        handoff_ms = ms(timing.handoff),
        write_ms = ms(writing),
        "query answered"
    );
}

fn handle_request(volumes: &VolumeSet, req: Request) -> Response {
    match req {
        Request::Hello {
            proto_min,
            proto_max,
            ..
        } => hello(volumes, proto_min, proto_max),
        Request::Status => Response::Status(build_status(volumes)),
        // Queries are streamed directly to binary frames by stream_query_binary
        // and never routed through here.
        Request::Query(_) => unreachable!("Query is handled by stream_query_binary"),
    }
}

/// Answers version negotiation.
///
/// This daemon speaks exactly [`PROTO_VERSION`], so a client whose supported
/// range does not cover it is refused. Reporting `PROTO_VERSION.min(proto_max)`
/// without the range check would answer `proto: 0` to a `proto_max: 0` client,
/// agreeing on a version neither side implements.
fn hello(volumes: &VolumeSet, proto_min: u16, proto_max: u16) -> Response {
    if proto_min > PROTO_VERSION || proto_max < PROTO_VERSION {
        return Response::Error {
            code: ProtoError::Unsupported,
            message: format!(
                "client speaks protocol {proto_min}..={proto_max}; this daemon speaks {PROTO_VERSION}"
            ),
        };
    }
    Response::Hello {
        proto: PROTO_VERSION,
        server: concat!("gozd ", env!("CARGO_PKG_VERSION")).to_string(),
        ready: all_volumes_live(volumes),
    }
}

/// Whether every volume's index is as complete as goz can make it, via
/// [`goz_core::types::VolumePhase::is_complete`]. The same bar
/// [`DaemonStatus::volumes_incomplete`] applies, so `Hello.ready` and a `Status`
/// reply can never disagree about whether the index is whole.
fn all_volumes_live(volumes: &VolumeSet) -> bool {
    volumes.iter().all(|v| v.phase.read().is_complete())
}

fn build_status(volumes: &VolumeSet) -> DaemonStatus {
    let mut incomplete = false;
    let vols = volumes
        .iter()
        .map(|v| {
            let phase = v.phase.read().clone();
            if !phase.is_complete() {
                incomplete = true;
            }
            let index = v.index.read();
            let stats = index.stats();
            let mem = index.memory();
            let pair = |c: goz_core::index::ComponentBytes| MemPair {
                used: c.used,
                alloc: c.allocated,
            };
            VolumeStatus {
                guid: v.guid.clone(),
                mounts: v.mounts.clone(),
                phase,
                entries: index.len() as u64,
                generation: index.generation(),
                metadata_pending: *v.metadata_pending.read(),
                journal_max_bytes: *v.journal_max_bytes.read(),
                has_orphans: index.has_orphans(),
                has_parent_cycle: index.has_parent_cycle(),
                names_waiting: index.name_order_stats().1,
                placeholders_created: stats.placeholders_created,
                delete_of_unknown: stats.delete_of_unknown,
                stale_slots: stats.stale_slots,
                link_reconciles_dropped: stats.link_reconciles_dropped,
                memory: Some(VolumeMemory {
                    entries: pair(mem.entries),
                    arena_raw: pair(mem.arena_raw),
                    arena_folded: pair(mem.arena_folded),
                    frn_map: pair(mem.frn_map),
                    name_tables: pair(mem.name_tables),
                    dir_children: pair(mem.dir_children),
                    frn_map_kind: mem.frn_map_kind.to_string(),
                }),
            }
        })
        .collect();
    let self_mem = goz_winfs::self_memory().unwrap_or_default();
    DaemonStatus {
        volumes: vols,
        volumes_incomplete: incomplete,
        process_working_set: self_mem.working_set,
        process_private_bytes: self_mem.private_bytes,
    }
}

/// A query's per-volume scope: the whole volume, or a relative folder path not
/// yet resolved to an `EntryIdx`. Resolution is deferred into
/// `stream_query_binary`'s per-volume loop so it shares the SAME index read
/// guard as `run_query`. Otherwise a
/// concurrent rescan between resolving the scope and running the query could
/// swap the index and leave the resolved `EntryIdx` stale (wrong/empty results
/// or an out-of-bounds panic).
enum VolScope {
    All,
    Rel(Vec<u8>),
}

/// Resolves which volumes (and per-volume unresolved scope) a query targets.
/// A scope belongs to the volume with the longest mount prefix it starts with,
/// so a volume mounted in a folder of another (`C:\mnt\usb\`) gets the scopes
/// under that folder wherever it sits in the volume set.
fn targets(volumes: &VolumeSet, scope: &Option<String>) -> Vec<(Arc<VolumeState>, VolScope)> {
    match scope {
        None => volumes.iter().map(|v| (v.clone(), VolScope::All)).collect(),
        Some(path) => {
            let Some((vol, at)) = volumes
                .iter()
                .filter_map(|v| scope_under(path, v.mount_prefix()).map(|at| (v, at)))
                .max_by_key(|&(_, at)| at)
            else {
                return Vec::new();
            };
            // Trim trailing separators. `VolumeIndex::path_of` emits `dir\name`
            // with no trailing separator, so an untrimmed `Windows\` (what shell
            // tab-completion produces, and what the CLI's `std::path::absolute`
            // preserves) resolves fine but can never equal the reconstructed
            // path: the scope cache would fail validation and re-walk the tree
            // on every keystroke, with correct results and no other symptom.
            // Trimming also lets both spellings share one cache entry.
            let raw = &path.as_bytes()[at..];
            let start = raw
                .iter()
                .position(|&b| b != b'\\' && b != b'/')
                .unwrap_or(raw.len());
            let end = raw
                .iter()
                .rposition(|&b| b != b'\\' && b != b'/')
                .map_or(start, |i| i + 1);
            vec![(vol.clone(), VolScope::Rel(raw[start..end].to_vec()))]
        }
    }
}

/// Where the part of `path` below the volume mounted at `prefix` starts, or
/// `None` when `path` is not on that volume. The prefix carries its closing
/// separator (`C:\mnt\usb\`), which a path that names the mount folder itself
/// lacks (`C:\mnt\usb`), so the comparison drops it and asks for a folder
/// boundary instead: `C:\mnt\usbstick` is not below `C:\mnt\usb\`.
fn scope_under(path: &str, prefix: &str) -> Option<usize> {
    let folder = prefix.trim_end_matches(['\\', '/']);
    if !starts_with_ci(path, folder) {
        return None;
    }
    match path.as_bytes().get(folder.len()) {
        None | Some(b'\\' | b'/') => Some(folder.len()),
        Some(_) => None,
    }
}

/// Resolves a `-path` scope, reusing the volume's cached resolution when the
/// same folder is searched again (every keystroke). The cache is validated
/// against the entry's current path, so a moved/deleted folder transparently
/// falls back to a fresh directory walk.
fn resolve_scope_cached(
    vol: &Arc<VolumeState>,
    index: &goz_core::index::VolumeIndex,
    rel: &[u8],
    match_case: bool,
) -> Option<EntryIdx> {
    // Case-sensitive scopes are rare; skip the cache for them.
    if !match_case {
        let cached = vol.scope_cache.lock();
        if let Some(sc) = cached.as_ref()
            && sc.rel == rel
            && scope_entry_valid(index, sc.entry, rel)
        {
            return Some(sc.entry);
        }
    }
    let entry = resolve_scope(index, rel, match_case)?;
    if !match_case {
        *vol.scope_cache.lock() = Some(crate::volume_state::ScopeCache {
            rel: rel.to_vec(),
            entry,
        });
    }
    Some(entry)
}

/// Confirms a cached scope entry still reconstructs to the same folded path
/// (cheap O(depth) walk), so a stale cache after a rename never mis-scopes.
fn scope_entry_valid(index: &goz_core::index::VolumeIndex, entry: EntryIdx, rel: &[u8]) -> bool {
    let mut buf = Vec::new();
    if !matches!(
        index.path_of(entry, &mut buf),
        goz_core::index::PathStatus::Ok
    ) {
        return false;
    }
    goz_core::fold::fold(&buf) == goz_core::fold::fold(rel)
}

/// Read guard over one volume's index, held for the span of one page.
type IndexGuard<'a> = parking_lot::RwLockReadGuard<'a, VolumeIndex>;

/// Parent paths memoized across the pages of one query stay valid only while
/// the index generation they were read under is current, and are capped so a
/// result set spread over very many directories cannot regrow the memory this
/// design exists to remove.
const PATH_MEMO_CAP_BYTES: usize = 32 << 20;

/// One volume's share of a query: its hits in sort order as 16-byte
/// references, consumed from the front as pages are built.
struct Run {
    refs: Vec<HitRef>,
    pos: usize,
    /// Cross-volume path sort only: the current head's
    /// `fold(prefix + parent path) + NUL + fold(name)`, built once per head
    /// instead of once per comparison.
    head_key: Option<Vec<u8>>,
    /// Parent directory to its path (`None`: not presentable), read under
    /// `memo_generation`.
    memo: FxHashMap<EntryIdx, Option<Vec<u8>>>,
    memo_generation: u64,
    memo_bytes: usize,
}

impl Run {
    fn new(refs: Vec<HitRef>) -> Self {
        Self {
            refs,
            pos: 0,
            head_key: None,
            memo: FxHashMap::default(),
            memo_generation: u64::MAX,
            memo_bytes: 0,
        }
    }

    fn head(&self) -> Option<HitRef> {
        self.refs.get(self.pos).copied()
    }

    fn take_head(&mut self) -> HitRef {
        let h = self.refs[self.pos];
        self.pos += 1;
        self.head_key = None;
        h
    }

    /// Builds the head's cross-volume path-sort key unless cached. The key is
    /// the engine's per-volume order, `(parent path, name)`, with the mount
    /// prefix in front so volumes mounted under one another compare right.
    fn ensure_head_key(&mut self, index: &VolumeIndex, prefix: &str) {
        if self.head_key.is_some() {
            return;
        }
        let entry = index.entry(self.refs[self.pos].idx);
        let mut parent = Vec::new();
        index.path_of(entry.parent(), &mut parent);
        let mut raw = Vec::with_capacity(prefix.len() + parent.len());
        raw.extend_from_slice(prefix.as_bytes());
        raw.extend_from_slice(&parent);
        let mut key = fold(&raw);
        key.push(0);
        key.extend_from_slice(&fold(entry.name()));
        self.head_key = Some(key);
    }

    /// Writes `prefix + parent path + '\' + name` into `path`, memoizing the
    /// parent's path. `false` when the parent is not presentable (its chain
    /// ends in lost+found or a cycle): the row is dropped, as the engine's page
    /// builder drops it.
    fn row_path(
        &mut self,
        index: &VolumeIndex,
        parent: EntryIdx,
        name: &[u8],
        prefix: &str,
        path: &mut Vec<u8>,
    ) -> bool {
        if self.memo_generation != index.generation() || self.memo_bytes > PATH_MEMO_CAP_BYTES {
            self.memo.clear();
            self.memo_generation = index.generation();
            self.memo_bytes = 0;
        }
        let memo_bytes = &mut self.memo_bytes;
        let parent_path = self.memo.entry(parent).or_insert_with(|| {
            let mut buf = Vec::new();
            let built = match index.path_of(parent, &mut buf) {
                PathStatus::Ok => Some(buf),
                _ => None,
            };
            *memo_bytes += built.as_ref().map_or(0, Vec::len);
            built
        });
        let Some(parent_path) = parent_path.as_deref() else {
            return false;
        };
        path.clear();
        path.extend_from_slice(prefix.as_bytes());
        if !parent_path.is_empty() {
            path.extend_from_slice(parent_path);
            path.push(b'\\');
        }
        path.extend_from_slice(name);
        true
    }
}

/// Picks the run whose head comes first under `sort`, stepping every run past
/// heads whose entry no longer exists (they count as consumed, like any other
/// merged row). `None` when every run is exhausted. Ties go to the earlier
/// run, so a multi-volume page equals a stable merge of the per-volume runs.
/// With a `turn` (see [`volumes_in_turn`]) the first run in it that has a head
/// comes first, and no heads are compared.
fn next_head(
    runs: &mut [Run],
    guards: &[IndexGuard<'_>],
    vols: &[Arc<VolumeState>],
    sort: SortSpec,
    turn: Option<&[usize]>,
    consumed: &mut usize,
) -> Option<usize> {
    for ri in 0..runs.len() {
        while let Some(h) = runs[ri].head() {
            if guards[ri].entry_if_frn(h.idx, h.frn).is_some() {
                break;
            }
            runs[ri].take_head();
            *consumed += 1;
        }
    }
    if let Some(turn) = turn {
        return turn.iter().copied().find(|&ri| runs[ri].head().is_some());
    }
    let mut best: Option<usize> = None;
    for ri in 0..runs.len() {
        if runs[ri].head().is_none() {
            continue;
        }
        best = Some(match best {
            None => ri,
            Some(bi) if cmp_heads(runs, ri, bi, guards, vols, sort) == Ordering::Less => ri,
            Some(bi) => bi,
        });
    }
    best
}

/// Orders the heads of runs `a` and `b` under `sort`, mirroring the engine's
/// per-volume comparator so the merged pages form one global order.
fn cmp_heads(
    runs: &mut [Run],
    a: usize,
    b: usize,
    guards: &[IndexGuard<'_>],
    vols: &[Arc<VolumeState>],
    sort: SortSpec,
) -> Ordering {
    let (ia, ib) = (&*guards[a], &*guards[b]);
    let (ha, hb) = (runs[a].refs[runs[a].pos], runs[b].refs[runs[b].pos]);
    let by_name = || ia.folded_name(ha.idx).cmp(ib.folded_name(hb.idx));
    let base = match sort.key {
        SortKey::Name => by_name(),
        SortKey::Size => size_key(ia.entry(ha.idx).size(), sort.dir)
            .cmp(&size_key(ib.entry(hb.idx).size(), sort.dir))
            .then_with(by_name),
        SortKey::DateModified => mtime_key(ia.entry(ha.idx).mtime(), sort.dir)
            .cmp(&mtime_key(ib.entry(hb.idx).mtime(), sort.dir))
            .then_with(by_name),
        SortKey::Path => {
            runs[a].ensure_head_key(ia, vols[a].mount_prefix());
            runs[b].ensure_head_key(ib, vols[b].mount_prefix());
            runs[a]
                .head_key
                .as_deref()
                .cmp(&runs[b].head_key.as_deref())
        }
    };
    if sort.dir == SortDir::Desc {
        base.reverse()
    } else {
        base
    }
}

/// The order a path sort takes volumes in, one whole run after another, when
/// their mount `prefixes` allow it: by folded prefix, reversed for a
/// descending sort. Every head key of a volume (see `Run::ensure_head_key`)
/// starts with its folded prefix, since the closing `\` keeps folding from
/// joining the prefix to the path after it, so when no prefix starts another
/// each volume's rows all sort before the next one's. `None` when one does,
/// as `C:\` starts `C:\mnt\data\`: those rows interleave and the heads are
/// compared.
fn volumes_in_turn(prefixes: &[&str], dir: SortDir) -> Option<Vec<usize>> {
    let folded: Vec<Vec<u8>> = prefixes.iter().map(|p| fold(p.as_bytes())).collect();
    let nested = folded.iter().enumerate().any(|(i, a)| {
        !a.ends_with(b"\\")
            || folded
                .iter()
                .enumerate()
                .any(|(j, b)| i != j && b.starts_with(a))
    });
    if nested {
        return None;
    }
    let mut order: Vec<usize> = (0..folded.len()).collect();
    order.sort_by(|&a, &b| folded[a].cmp(&folded[b]));
    if dir == SortDir::Desc {
        order.reverse();
    }
    Some(order)
}

/// Case-insensitive ASCII prefix test (drive letters and `\` are ASCII).
fn starts_with_ci(haystack: &str, prefix: &str) -> bool {
    haystack.len() >= prefix.len()
        && haystack.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::{
        VolScope, build_status, encode_query_binary, handle_request, resolve_scope_cached,
        scope_entry_valid, starts_with_ci, targets,
    };
    use crate::volume_state::{VolumeSet, VolumeState};
    use goz_core::index::{FrnMap, NTFS_ROOT_FRN, VolumeIndex};
    use goz_core::proto::{
        FrameDecoder, MAX_SERVER_FRAME, PAGE_ROWS, PROTO_VERSION, ProtoError, Request, Response,
        decode_response_frame,
    };
    use goz_core::query::resolve_scope;
    use goz_core::types::{Frn, SortKey, SortSpec, VolumePhase};
    use goz_core::usn::record::{
        FILE_ATTRIBUTE_DIRECTORY, ParsedUsnRecord, USN_REASON_FILE_CREATE,
    };
    use parking_lot::{Mutex, RwLock};
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    fn dir_record(frn: u64, parent: u64, name: &str) -> ParsedUsnRecord {
        ParsedUsnRecord {
            major_version: 2,
            frn: Frn(frn),
            parent_frn: Frn(parent),
            usn: 0,
            timestamp_ft: 0,
            reason: USN_REASON_FILE_CREATE,
            attributes: FILE_ATTRIBUTE_DIRECTORY,
            name: name.as_bytes().to_vec(),
            name_lossy: false,
        }
    }

    /// `C:\Windows\System32` as an index, built through the public API only.
    fn windows_index() -> VolumeIndex {
        let mut index = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
        index.insert_enum(&dir_record(100, NTFS_ROOT_FRN.0, "Windows"));
        index.insert_enum(&dir_record(101, 100, "System32"));
        index
    }

    fn volume(mount: &str, index: VolumeIndex, phase: VolumePhase) -> Arc<VolumeState> {
        Arc::new(VolumeState {
            guid: r"\\?\Volume{00000000-0000-0000-0000-000000000000}\".to_string(),
            mounts: vec![mount.to_string()],
            index: RwLock::new(index),
            phase: RwLock::new(phase),
            metadata_pending: RwLock::new(false),
            journal_max_bytes: RwLock::new(None),
            indexed: AtomicBool::new(true),
            scope_cache: Mutex::new(None),
        })
    }

    fn live_volume(mount: &str) -> Arc<VolumeState> {
        volume(mount, windows_index(), VolumePhase::Live)
    }

    /// An index whose name order is the REVERSE of its path order: `a\z.log`
    /// sorts after `z\a.log` by name but before it by path. A fixture where the
    /// two agree would pass even with the name key computed wrongly.
    fn merge_index() -> VolumeIndex {
        let mut index = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
        index.insert_enum(&dir_record(100, NTFS_ROOT_FRN.0, "a"));
        index.insert_enum(&dir_record(101, NTFS_ROOT_FRN.0, "z"));
        for (frn, parent, name) in [(102, 100, "z.log"), (103, 101, "a.log")] {
            let mut rec = dir_record(frn, parent, name);
            rec.attributes = 0;
            index.insert_enum(&rec);
        }
        index
    }

    /// The scope cache only pays off when a resolved entry validates on the next
    /// keystroke, and `scope_entry_valid` is the predicate that decides that.
    #[test]
    fn resolved_scope_validates_against_the_path_it_came_from() {
        let index = windows_index();
        for rel in [&b"Windows"[..], b"Windows\\System32"] {
            let entry = resolve_scope(&index, rel, false).expect("scope resolves");
            assert!(
                scope_entry_valid(&index, entry, rel),
                "{} resolved but did not validate",
                String::from_utf8_lossy(rel)
            );
        }
    }

    /// A trailing separator is the common spelling: shell tab-completion emits
    /// `C:\Windows\`, and `std::path::absolute` in the CLI preserves it.
    ///
    /// `resolve_scope` filters empty components so it resolves fine, but
    /// `path_of` never emits a trailing separator, so an untrimmed `Windows\`
    /// could never match it: the entry failed validation on every query, the
    /// cache missed forever, and the tree walk silently ran every keystroke.
    /// Results stayed correct, which is exactly why nothing caught it.
    #[test]
    fn trailing_separator_scope_still_validates() {
        let index = windows_index();
        let entry = resolve_scope(&index, b"Windows\\", false).expect("resolves today");
        let plain = resolve_scope(&index, b"Windows", false).unwrap();
        assert_eq!(entry, plain, "both spellings name the same directory");

        let vol = live_volume(r"C:\");
        let (_, scope) = targets(&set(&[vol]), &Some(r"C:\Windows\".to_string()))
            .pop()
            .expect("C: matches");
        let VolScope::Rel(rel) = scope else {
            panic!("a -path scope must produce Rel")
        };
        assert!(
            scope_entry_valid(&index, entry, &rel),
            "scope {:?} never validates, so the cache misses on every query",
            String::from_utf8_lossy(&rel)
        );
    }

    /// A second identical query must reuse the cached entry, and a cached entry
    /// must survive only while it still names the same path.
    #[test]
    fn scope_cache_stores_then_reuses() {
        let vol = live_volume(r"C:\");
        let index = vol.index.read();

        let first = resolve_scope_cached(&vol, &index, b"Windows", false).expect("resolves");
        assert!(vol.scope_cache.lock().is_some(), "first call must populate");

        let second = resolve_scope_cached(&vol, &index, b"Windows", false).expect("resolves");
        assert_eq!(first, second);

        // A cached entry whose path no longer matches must not be handed back.
        assert!(!scope_entry_valid(&index, first, b"Somewhere\\Else"));
    }

    /// Case-sensitive scopes are documented as bypassing the cache entirely.
    #[test]
    fn case_sensitive_scope_never_touches_the_cache() {
        let vol = live_volume(r"C:\");
        let index = vol.index.read();
        resolve_scope_cached(&vol, &index, b"Windows", true).expect("resolves");
        assert!(
            vol.scope_cache.lock().is_none(),
            "match_case must neither read nor write the cache"
        );
    }

    fn set(vols: &[Arc<VolumeState>]) -> VolumeSet {
        Arc::new(vols.to_vec())
    }

    /// Routing a `-path` to the right volume. An unmatched scope returns no
    /// targets, which the caller renders as zero hits.
    #[test]
    fn targets_route_scope_to_one_volume() {
        let c = live_volume(r"C:\");
        let d = live_volume(r"D:\");
        let vols = set(&[c, d]);

        assert_eq!(
            targets(&vols, &None).len(),
            2,
            "no scope means every volume"
        );

        let picked = targets(&vols, &Some(r"c:\windows".to_string()));
        assert_eq!(picked.len(), 1, "scope selects exactly one volume");
        assert_eq!(picked[0].0.mount_prefix(), r"C:\", "case-insensitive match");

        assert!(
            targets(&vols, &Some(r"Z:\nope".to_string())).is_empty(),
            "an unindexed drive yields no targets"
        );
    }

    /// A volume mounted in a folder of another gets the scopes under that
    /// folder, also when it comes after the volume it sits in, as a volume
    /// found later does. The first matching prefix sent them to the outer
    /// volume, whose index holds only the mount folder, never its contents.
    #[test]
    fn a_scope_goes_to_the_volume_mounted_deepest() {
        let mounted = live_volume(r"C:\mnt\usb\");
        let vols = set(&[live_volume(r"C:\"), mounted.clone()]);

        let picked = targets(&vols, &Some(r"c:\MNT\usb\Windows\".to_string()));
        assert_eq!(picked.len(), 1);
        assert!(Arc::ptr_eq(&picked[0].0, &mounted));
        let VolScope::Rel(rel) = &picked[0].1 else {
            panic!("a -path scope must produce Rel")
        };
        assert_eq!(rel, b"Windows");

        let outer = targets(&vols, &Some(r"C:\mnt\other".to_string()));
        assert_eq!(outer[0].0.mount_prefix(), r"C:\");
        assert_eq!(
            result_paths(&vols, scoped("System32", r"C:\mnt\usb\Windows")),
            [r"C:\mnt\usb\Windows\System32"]
        );

        // The mount folder itself, as `std::path::absolute` spells it, with no
        // closing separator: the whole mounted volume, not the outer one's
        // empty mount point. A longer name beside it is still the outer volume.
        for scope in [r"C:\mnt\usb", r"C:\mnt\usb\", r"c:\MNT\USB"] {
            let picked = targets(&vols, &Some(scope.to_string()));
            assert!(Arc::ptr_eq(&picked[0].0, &mounted), "{scope}");
            assert!(
                matches!(&picked[0].1, VolScope::Rel(rel) if rel.is_empty()),
                "{scope}"
            );
        }
        assert_eq!(
            result_paths(&vols, scoped("System32", r"C:\mnt\usb")),
            [r"C:\mnt\usb\Windows\System32"]
        );
        let beside = targets(&vols, &Some(r"C:\mnt\usbstick".to_string()));
        assert_eq!(beside[0].0.mount_prefix(), r"C:\");
    }

    /// A volume is served from the moment it is found, before its first build:
    /// its empty index adds no hits, every page and the status say results may
    /// be incomplete, and the handshake is not ready. The daemon used to open
    /// the pipe only after every volume was built, so a client asking in the
    /// first half minute was told the daemon was not running.
    #[test]
    fn a_volume_still_bootstrapping_is_served_flagged_incomplete() {
        let found = Arc::new(VolumeState::found(
            r"\\?\Volume{0000000d-0000-0000-0000-000000000000}\".into(),
            vec![r"D:\".into()],
        ));
        let vols = set(&[live_volume(r"C:\"), found]);

        let pages = results(&encode_query_binary(&vols, query("System32")));
        assert_eq!(pages[0].total, 1, "the built volume still answers");
        assert!(pages.iter().all(|p| p.volumes_incomplete));

        let status = build_status(&vols);
        assert!(status.volumes_incomplete);
        assert_eq!(status.volumes[1].phase, VolumePhase::Bootstrapping);
        let Response::Hello { ready, .. } = handle_request(&vols, hello_request(1, 1)) else {
            panic!("must negotiate")
        };
        assert!(!ready);
    }

    /// A Snapshot volume must NOT taint results.
    ///
    /// It has no USN journal and never can (Windows refuses to create one on a
    /// recovery partition), so its index is finished: no waiting improves it and
    /// no action fixes it. Counting it as incomplete printed "results may be
    /// incomplete" on every query forever on any machine with a recovery
    /// partition, which is nearly all of them, and an always-on warning is one
    /// nobody reads when it finally matters.
    #[test]
    fn snapshot_volumes_do_not_taint_results() {
        let vols = set(&[
            live_volume(r"C:\"),
            volume(r"D:\", windows_index(), VolumePhase::Snapshot),
        ]);
        assert!(
            !build_status(&vols).volumes_incomplete,
            "a fully indexed journal-less volume must not flag results incomplete"
        );
        assert!(
            !results(&encode_query_binary(&vols, query("System32")))[0].volumes_incomplete,
            "and no query page may flag it either"
        );
        let Response::Hello { ready, .. } = handle_request(&vols, hello_request(1, 1)) else {
            panic!("must negotiate")
        };
        assert!(ready, "a snapshot volume is ready to serve");
    }

    /// The honesty bit: any volume whose index is NOT as complete as goz can make
    /// it must mark results incomplete, or the user trusts a partial index.
    #[test]
    fn status_is_incomplete_unless_every_volume_is_live() {
        assert!(!build_status(&set(&[live_volume(r"C:\")])).volumes_incomplete);

        for phase in [
            VolumePhase::Bootstrapping,
            VolumePhase::Rescanning,
            VolumePhase::Offline,
            VolumePhase::Failed {
                reason: "test".into(),
            },
        ] {
            let vols = set(&[live_volume(r"C:\"), volume(r"D:\", windows_index(), phase)]);
            assert!(
                build_status(&vols).volumes_incomplete,
                "one non-Live volume must taint the whole status"
            );
        }
    }

    fn query(q: &str) -> goz_core::proto::QueryRequest {
        goz_core::proto::QueryRequest {
            query: q.to_string(),
            scope: None,
            sort: SortSpec::default_for(SortKey::Name),
            offset: 0,
            limit: None,
            want_size: true,
            want_mtime: true,
            match_case: false,
            want_total_size: false,
        }
    }

    /// Splits the daemon's own output back into responses. The wire is
    /// `[len:u32-le][payload]` repeated, and every decoder used here is public
    /// goz-core API, so the test reads exactly what the CLI would.
    fn decode_frames(bytes: &[u8]) -> Vec<goz_core::proto::Response> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            let len = u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap()) as usize;
            i += 4;
            out.push(goz_core::proto::decode_response_frame(&bytes[i..i + len]).expect("decodes"));
            i += len;
        }
        out
    }

    fn results(bytes: &[u8]) -> Vec<goz_core::proto::QueryResults> {
        decode_frames(bytes)
            .into_iter()
            .map(|r| match r {
                goz_core::proto::Response::Results(r) => r,
                other => panic!("expected Results, got {other:?}"),
            })
            .collect()
    }

    /// The documented terminal-frame invariant. A client reads until it sees
    /// `more: false`, so a zero-match query that emitted no frame at all would
    /// hang it rather than return nothing.
    #[test]
    fn empty_result_set_still_emits_one_terminal_frame() {
        let vols = set(&[live_volume(r"C:\")]);
        let pages = results(&encode_query_binary(&vols, query("no-such-file-anywhere")));
        assert_eq!(pages.len(), 1, "exactly one frame");
        assert!(!pages[0].more, "the only frame must be terminal");
        assert_eq!(pages[0].items.len(), 0);
        assert_eq!(pages[0].total, 0);
    }

    /// A query that fails to parse must come back as a single tagged JSON error,
    /// never as an empty results page (which would read as "no matches").
    #[test]
    fn unparseable_query_returns_an_error_frame_not_an_empty_page() {
        let vols = set(&[live_volume(r"C:\")]);
        let frames = decode_frames(&encode_query_binary(&vols, query("size:>>bogus")));
        assert_eq!(frames.len(), 1);
        assert!(
            matches!(frames[0], goz_core::proto::Response::Error { .. }),
            "got {:?}",
            frames[0]
        );
    }

    /// Paging must set `more` on every frame but the last, and `total` must be
    /// the whole match count on every page, not the page's own length.
    #[test]
    fn pages_carry_more_until_the_last_and_a_stable_total() {
        // One directory holding PAGE_ROWS + 1 matches, so the encoder pages.
        let mut index = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
        index.insert_enum(&dir_record(100, NTFS_ROOT_FRN.0, "many"));
        let n = PAGE_ROWS as u64 + 1;
        for i in 0..n {
            let mut rec = dir_record(1000 + i, 100, &format!("hit{i:05}.log"));
            rec.attributes = 0;
            index.insert_enum(&rec);
        }
        let vols = set(&[volume(r"C:\", index, VolumePhase::Live)]);

        let pages = results(&encode_query_binary(&vols, query("hit")));
        assert_eq!(pages.len(), 2, "PAGE_ROWS + 1 matches must span two frames");
        assert!(pages[0].more, "first page must signal more");
        assert!(!pages[1].more, "last page must be terminal");
        assert_eq!(pages[0].items.len(), PAGE_ROWS as usize);
        assert_eq!(pages[1].items.len(), 1);
        assert_eq!(pages[0].total, n, "total is the whole match count");
        assert_eq!(pages[1].total, n, "total must not drift across pages");
    }

    /// Every page must fit the 16 MiB frame cap the client decodes with, or
    /// the client refuses it and the search fails. [`PAGE_ROWS`] paths of
    /// about 4 KB pass that cap, and any local user can create them.
    #[test]
    fn pages_of_long_paths_fit_the_clients_frame_cap() {
        let mut index = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
        // 17 nested folders with 250-character names make every full path
        // below them about 4.3 KB, so PAGE_ROWS rows would need 17.6 MB.
        let mut parent = NTFS_ROOT_FRN.0;
        for level in 0..17 {
            index.insert_enum(&dir_record(100 + level, parent, &"d".repeat(250)));
            parent = 100 + level;
        }
        for i in 0..PAGE_ROWS as u64 {
            let mut rec = dir_record(1000 + i, parent, &format!("hit{i:05}.log"));
            rec.attributes = 0;
            index.insert_enum(&rec);
        }
        let vols = set(&[volume(r"C:\", index, VolumePhase::Live)]);

        let mut decoder = FrameDecoder::new(MAX_SERVER_FRAME);
        decoder.feed(&encode_query_binary(&vols, query("hit")));
        let mut pages = Vec::new();
        while let Some(frame) = decoder
            .next_frame()
            .expect("every frame fits the client's cap")
        {
            match decode_response_frame(&frame).expect("decodes") {
                Response::Results(page) => pages.push(page),
                other => panic!("expected Results, got {other:?}"),
            }
        }
        assert!(pages.len() > 1, "the byte budget splits the rows");
        let (last, rest) = pages.split_last().expect("at least one page");
        assert!(
            rest.iter().all(|p| p.more),
            "every page but the last says more"
        );
        assert!(!last.more, "the last page is terminal");
        let names: Vec<String> = pages
            .iter()
            .flat_map(|page| &page.items)
            .map(|item| item.path.rsplit('\\').next().unwrap().to_string())
            .collect();
        let expected: Vec<String> = (0..PAGE_ROWS).map(|i| format!("hit{i:05}.log")).collect();
        assert_eq!(names, expected, "every row arrives once, in order");
    }

    /// A connection that asks nothing is dropped after the idle timeout, but
    /// one with a reply out waits as long as a page write may take: its last
    /// page can sit unread in the pipe while the client writes the one before
    /// to a paused console, and the disconnect that ends the connection would
    /// discard it.
    #[tokio::test(start_paused = true)]
    async fn a_reply_left_unread_outlasts_the_idle_timeout() {
        use super::{IDLE_TIMEOUT, WRITE_TIMEOUT, handle_connection};
        use crate::volume_state::VolumeRegistry;
        use tokio::io::AsyncWriteExt;
        use tokio::net::windows::named_pipe::{ClientOptions, ServerOptions};
        for asks in [false, true] {
            let name = format!(r"\\.\pipe\goz-test-{}-{asks}", std::process::id());
            let mut server = ServerOptions::new()
                .first_pipe_instance(true)
                .create(&name)
                .unwrap();
            let mut client = ClientOptions::new().open(&name).unwrap();
            server.connect().await.unwrap();
            if asks {
                let mut frame = Vec::new();
                goz_core::proto::encode_request(&Request::Status, &mut frame);
                client.write_all(&frame).await.unwrap();
            }
            let started = tokio::time::Instant::now();
            handle_connection(&mut server, Arc::new(VolumeRegistry::default()))
                .await
                .unwrap();
            let waited = started.elapsed();
            if asks {
                assert!(waited >= WRITE_TIMEOUT, "dropped after {waited:?}");
            } else {
                assert!(
                    waited >= IDLE_TIMEOUT && waited < WRITE_TIMEOUT,
                    "{waited:?}"
                );
            }
            // Open until here, so the server waited on a live client.
            drop(client);
        }
    }

    /// A taken pipe name must read as taken through the context `create_instance`
    /// adds, or `claim_first_instance` returns the error and the SCM rebuilds
    /// every index once a minute for as long as a squatter holds the name.
    #[test]
    fn a_held_pipe_name_is_recognized_through_its_context() {
        use anyhow::Context;
        let held = |code| {
            Err::<(), _>(std::io::Error::from_raw_os_error(code))
                .with_context(|| "creating pipe instance")
                .unwrap_err()
        };
        assert!(super::name_taken(&held(super::ERROR_ACCESS_DENIED)));
        assert!(super::name_taken(&held(super::ERROR_PIPE_BUSY)));
        assert!(
            !super::name_taken(&held(2)),
            "file not found is a real failure"
        );
    }

    /// When the page consumer reports it is gone (`emit` returns false, i.e.
    /// the client vanished or the pipe write failed), the encoder must stop
    /// instead of encoding every remaining page into the void.
    #[test]
    fn streaming_stops_when_the_consumer_goes_away() {
        let mut index = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
        index.insert_enum(&dir_record(100, NTFS_ROOT_FRN.0, "many"));
        for i in 0..(PAGE_ROWS as u64 * 3) {
            let mut rec = dir_record(1000 + i, 100, &format!("hit{i:05}.log"));
            rec.attributes = 0;
            index.insert_enum(&rec);
        }
        let vols = set(&[volume(r"C:\", index, VolumePhase::Live)]);

        let mut emitted = 0usize;
        super::stream_query_binary(&vols, query("hit"), &mut |_page| {
            emitted += 1;
            false // consumer gone after the first page
        });
        assert_eq!(
            emitted, 1,
            "a dead consumer must stop the encoder after the rejected page"
        );
    }

    /// `want_size: false` must suppress the value even when the index holds one,
    /// and the mount prefix must be prepended to every emitted path.
    #[test]
    fn flags_suppress_metadata_and_paths_carry_the_mount_prefix() {
        let vols = set(&[live_volume(r"C:\")]);
        let mut q = query("System32");
        q.want_size = false;
        q.want_mtime = false;

        let pages = results(&encode_query_binary(&vols, q));
        let item = &pages[0].items[0];
        assert!(item.size.is_none(), "want_size: false must suppress size");
        assert!(
            item.mtime_ft.is_none(),
            "want_mtime: false must suppress mtime"
        );
        assert!(
            item.path.starts_with(r"C:\"),
            "path {:?} must carry the mount prefix",
            item.path
        );
    }

    /// The paths of every row `q` returns.
    fn result_paths(vols: &VolumeSet, q: goz_core::proto::QueryRequest) -> Vec<String> {
        results(&encode_query_binary(vols, q))
            .into_iter()
            .flat_map(|page| page.items)
            .map(|item| item.path)
            .collect()
    }

    /// The request's `match_case` (the `-case` switch) decides matching, not
    /// only the `-path` lookup.
    #[test]
    fn the_case_switch_reaches_matching() {
        let vols = set(&[live_volume(r"C:\")]);
        let mut q = query("system32");
        q.match_case = true;
        assert!(result_paths(&vols, q.clone()).is_empty());
        q.match_case = false;
        assert_eq!(result_paths(&vols, q), [r"C:\Windows\System32"]);
    }

    /// The `-path` folder is looked up with the case the query ends up with, so
    /// a `case:` directive decides it as it decides matching.
    #[test]
    fn the_scope_lookup_follows_the_case_directive() {
        let vols = set(&[live_volume(r"C:\")]);
        let scoped = |text: &str, scope: &str, match_case| {
            let mut q = query(text);
            q.scope = Some(scope.to_string());
            q.match_case = match_case;
            result_paths(&vols, q)
        };
        assert!(scoped("case: System32", r"C:\windows", false).is_empty());
        assert_eq!(
            scoped("case: System32", r"C:\Windows", false),
            [r"C:\Windows\System32"]
        );
        assert_eq!(
            scoped("case:0 system32", r"C:\windows", true),
            [r"C:\Windows\System32"]
        );
    }

    /// The daemon resolves `dm:` against its own clock and time zone as the
    /// query arrives: a folder touched now is newer than 2001 and in the last
    /// hour, one dated mid 2001 is in 2001 wherever the machine is.
    #[test]
    fn dm_dates_resolve_against_the_machine_clock() {
        use goz_core::query::LocalClock;
        let mut index = windows_index();
        let now = crate::local_clock::MachineClock::read().now();
        index.set_stat(Frn(100), 0, now);
        index.set_stat(
            Frn(101),
            0,
            goz_core::output::unix_ms_to_filetime(993_945_600_000), // 2001-07-01T00:00Z
        );
        let vols = set(&[volume(r"C:\", index, VolumePhase::Live)]);
        assert_eq!(result_paths(&vols, query("dm:>2001")), [r"C:\Windows"]);
        assert_eq!(result_paths(&vols, query("dm:last1hours")), [r"C:\Windows"]);
        assert_eq!(
            result_paths(&vols, query("datemodified:2001")),
            [r"C:\Windows\System32"]
        );
        assert!(result_paths(&vols, query("dm:unknown")).is_empty());
    }

    /// An absolute path finds what is on that volume and nothing on another.
    #[test]
    fn an_absolute_path_term_matches_only_its_drive() {
        let vols = set(&[live_volume(r"C:\"), live_volume(r"D:\")]);
        assert_eq!(
            result_paths(&vols, query(r"C:\Windows\System32")),
            [r"C:\Windows\System32"]
        );
        assert_eq!(
            result_paths(&vols, query(r"d:/windows/")),
            [r"D:\Windows\System32"]
        );
    }

    /// A volume that is not Live must taint every page, not just the first, so a
    /// client that only reads the last frame still learns the set is partial.
    #[test]
    fn incomplete_volumes_taint_every_page() {
        let vols = set(&[
            live_volume(r"C:\"),
            volume(r"D:\", windows_index(), VolumePhase::Rescanning),
        ]);
        let pages = results(&encode_query_binary(&vols, query("System32")));
        assert!(
            pages.iter().all(|p| p.volumes_incomplete),
            "every page must report the index is incomplete"
        );
    }

    /// Offset past the end is a legal request and must terminate cleanly rather
    /// than emit nothing.
    #[test]
    fn offset_and_limit_slice_the_result_set() {
        let vols = set(&[live_volume(r"C:\")]);

        let mut q = query("System32");
        q.offset = 500;
        let pages = results(&encode_query_binary(&vols, q));
        assert_eq!(pages.len(), 1);
        assert!(!pages[0].more);
        assert_eq!(
            pages[0].items.len(),
            0,
            "offset past the end yields no items"
        );

        let mut q = query("Windows");
        q.limit = Some(1);
        let pages = results(&encode_query_binary(&vols, q));
        assert_eq!(pages[0].items.len(), 1);
    }

    /// The payoff the key agreement buys: two pre-sorted runs merged into one
    /// global name order without a re-sort. The fixture's name order is the
    /// reverse of its path order, so a merge keyed on the wrong thing cannot
    /// coincidentally produce the right answer.
    #[test]
    fn name_sort_merges_two_volumes_into_one_global_order() {
        let vols = set(&[
            volume(r"C:\", merge_index(), VolumePhase::Live),
            volume(r"D:\", merge_index(), VolumePhase::Live),
        ]);
        let pages = results(&encode_query_binary(&vols, query("log")));
        let names: Vec<&str> = pages[0]
            .items
            .iter()
            .map(|i| i.path.rsplit('\\').next().unwrap())
            .collect();

        assert_eq!(
            names,
            ["a.log", "a.log", "z.log", "z.log"],
            "merged pages must be globally ordered by name, not by path or by volume"
        );
    }

    /// A volume mounted under another volume's folder interleaves with it in a
    /// path sort: `C:\m\z.log` belongs between `C:\z.log` and
    /// `C:\z.log`. Each run is ordered within its volume; only the merge
    /// knows the prefixes, so it must compare the heads' full `(parent path,
    /// name)` keys rather than concatenate the runs.
    #[test]
    fn path_sort_merges_a_volume_mounted_inside_another() {
        use goz_core::types::SortDir;
        let vols = set(&[
            volume(r"C:\", merge_index(), VolumePhase::Live),
            volume(r"C:\m\", merge_index(), VolumePhase::Live),
        ]);
        let ordered = |dir| {
            let mut q = query("log");
            q.sort = SortSpec {
                key: SortKey::Path,
                dir,
            };
            paths(&encode_query_binary(&vols, q))
        };
        let asc = ordered(SortDir::Asc);
        assert_eq!(
            asc,
            [
                r"C:\a\z.log",
                r"C:\m\a\z.log",
                r"C:\m\z\a.log",
                r"C:\z\a.log"
            ]
        );
        let mut desc = ordered(SortDir::Desc);
        desc.reverse();
        assert_eq!(desc, asc, "descending is the exact reverse");
    }

    /// Volumes whose folded mount prefixes do not start one another go in
    /// prefix order, a GUID path (`\` sorts below the drive letters) first;
    /// a volume mounted in another, a repeated prefix, or one without its
    /// closing `\` needs the heads compared.
    #[test]
    fn path_sort_takes_volumes_in_turn_unless_one_is_mounted_in_another() {
        use super::volumes_in_turn;
        use goz_core::types::SortDir;
        let guid = r"\\?\Volume{00000000-0000-0000-0000-000000000000}\";
        let vols = [r"D:\", r"C:\", guid];
        assert_eq!(volumes_in_turn(&vols, SortDir::Asc), Some(vec![2, 1, 0]));
        assert_eq!(volumes_in_turn(&vols, SortDir::Desc), Some(vec![0, 1, 2]));
        assert_eq!(
            volumes_in_turn(&[r"C:\data2\", r"c:\DATA\"], SortDir::Asc),
            Some(vec![0, 1]),
            r"`2` sorts below `\`"
        );
        assert_eq!(
            volumes_in_turn(&[r"C:\", r"c:\mnt\data\"], SortDir::Asc),
            None
        );
        assert_eq!(volumes_in_turn(&[r"C:\", r"C:\"], SortDir::Asc), None);
        assert_eq!(volumes_in_turn(&["C:", r"D:\"], SortDir::Asc), None);
    }

    /// Volumes taken in turn give the pages a merge by comparing gave: every
    /// row of one before every row of the next, across a page cut too.
    #[test]
    fn path_sort_over_separate_volumes_pages_like_the_merge() {
        use goz_core::types::SortDir;
        let guid = r"\\?\Volume{00000000-0000-0000-0000-000000000000}\";
        let vols = set(&[
            volume(r"D:\", merge_index(), VolumePhase::Live),
            volume(r"C:\", merge_index(), VolumePhase::Live),
            volume(guid, merge_index(), VolumePhase::Live),
        ]);
        let page = |dir, offset, limit| {
            let mut q = query("log");
            q.sort = SortSpec {
                key: SortKey::Path,
                dir,
            };
            q.offset = offset;
            q.limit = limit;
            paths(&encode_query_binary(&vols, q))
        };
        let asc = page(SortDir::Asc, 0, None);
        let want: Vec<String> = [guid, r"C:\", r"D:\"]
            .iter()
            .flat_map(|p| [format!(r"{p}a\z.log"), format!(r"{p}z\a.log")])
            .collect();
        assert_eq!(asc, want);
        let mut desc = page(SortDir::Desc, 0, None);
        desc.reverse();
        assert_eq!(desc, asc, "descending is the exact reverse");
        assert_eq!(page(SortDir::Asc, 1, Some(2)), asc[1..3]);
    }

    /// Taken in turn, a merge compares no heads, so it builds no head key: the
    /// per-row path a compared merge builds is the cost this skips.
    #[test]
    fn a_merge_in_turn_builds_no_head_key() {
        use super::{IndexGuard, Run, next_head};
        use goz_core::query::{parse_query, run_query_refs};
        let vols = [
            volume(r"C:\", merge_index(), VolumePhase::Live),
            volume(r"D:\", merge_index(), VolumePhase::Live),
        ];
        let parsed = parse_query("log").unwrap();
        let sort = SortSpec::default_for(SortKey::Path);
        let mut runs: Vec<Run> = vols
            .iter()
            .map(|v| {
                let index = v.index.read();
                let out = run_query_refs(
                    &index,
                    v.mount_prefix(),
                    &parsed,
                    None,
                    sort,
                    0,
                    None,
                    false,
                );
                Run::new(out.refs)
            })
            .collect();
        let guards: Vec<IndexGuard<'_>> = vols.iter().map(|v| v.index.read()).collect();
        let mut consumed = 0;
        let mut taken = Vec::new();
        while let Some(ri) = next_head(
            &mut runs,
            &guards,
            &vols,
            sort,
            Some(&[0, 1]),
            &mut consumed,
        ) {
            assert!(runs.iter().all(|r| r.head_key.is_none()));
            runs[ri].take_head();
            taken.push(ri);
        }
        assert_eq!(taken, [0, 0, 1, 1]);
    }

    /// Every emitted path, in wire order, across all pages.
    fn paths(bytes: &[u8]) -> Vec<String> {
        results(bytes)
            .into_iter()
            .flat_map(|p| p.items.into_iter().map(|i| i.path))
            .collect()
    }

    /// A limit at or above the match count means "everything", so it must return
    /// exactly what no limit returns, in the same order, under every sort key
    /// and direction.
    ///
    /// The deferral that makes the covering-limit case cheap changes WHERE the
    /// ordering happens (off the index lock instead of under it). This pins that
    /// it does not change WHAT comes back. Before the fix the two spellings took
    /// different code paths entirely, the engine's comparator versus the
    /// server's, which is exactly how their tie-ordering could drift apart
    /// unnoticed.
    #[test]
    fn a_covering_limit_returns_the_same_page_as_no_limit() {
        use goz_core::types::SortDir;
        let vols = set(&[volume(r"C:\", merge_index(), VolumePhase::Live)]);
        for key in [
            SortKey::Name,
            SortKey::Path,
            SortKey::Size,
            SortKey::DateModified,
        ] {
            for dir in [SortDir::Asc, SortDir::Desc] {
                let page = |limit| {
                    let mut q = query("log");
                    q.sort = SortSpec { key, dir };
                    q.limit = limit;
                    paths(&encode_query_binary(&vols, q))
                };
                let unlimited = page(None);
                assert!(!unlimited.is_empty(), "fixture must produce hits");
                for limit in [u32::MAX, unlimited.len() as u32, unlimited.len() as u32 + 1] {
                    assert_eq!(
                        page(Some(limit)),
                        unlimited,
                        "{key:?}/{dir:?}: -n {limit} covers the set and must equal no limit"
                    );
                }
            }
        }
    }

    /// Files at the volume root, each with its size and date, both `value`,
    /// or neither when `value` is `None`, as a file not read yet.
    fn dated_files(files: &[(u64, &str, Option<u64>)]) -> VolumeIndex {
        let mut index = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
        for &(frn, name, value) in files {
            let mut rec = dir_record(frn, NTFS_ROOT_FRN.0, name);
            rec.attributes = 0;
            index.insert_enum(&rec);
            if let Some(v) = value {
                index.set_stat(Frn(frn), v, v as i64);
            }
        }
        index
    }

    /// The merge keys a size or date sort as the engine does: a value not
    /// read yet goes after every volume's known ones, in either direction,
    /// instead of meeting the other volume's rows as a 0. One volume's values
    /// lie between the other's, so the merge compares every head.
    #[test]
    fn size_and_date_sorts_merge_volumes_with_unknowns_last() {
        use goz_core::types::SortDir;
        let vols = set(&[
            volume(
                r"C:\",
                dated_files(&[(200, "c10.log", Some(10)), (201, "cnone.log", None)]),
                VolumePhase::Live,
            ),
            volume(
                r"D:\",
                dated_files(&[(200, "d5.log", Some(5)), (201, "d20.log", Some(20))]),
                VolumePhase::Live,
            ),
        ]);
        for key in [SortKey::Size, SortKey::DateModified] {
            for (dir, want) in [
                (
                    SortDir::Asc,
                    [r"D:\d5.log", r"C:\c10.log", r"D:\d20.log", r"C:\cnone.log"],
                ),
                (
                    SortDir::Desc,
                    [r"D:\d20.log", r"C:\c10.log", r"D:\d5.log", r"C:\cnone.log"],
                ),
            ] {
                let mut q = query("log");
                q.sort = SortSpec { key, dir };
                assert_eq!(
                    paths(&encode_query_binary(&vols, q)),
                    want,
                    "{key:?} {dir:?}"
                );
            }
        }
    }

    fn hello_request(proto_min: u16, proto_max: u16) -> Request {
        Request::Hello {
            proto_min,
            proto_max,
            client: "test".to_string(),
        }
    }

    /// Negotiation must refuse a client it cannot actually serve rather than
    /// answer a cheerful `ready: true` and let the mismatch surface as garbled
    /// frames later.
    #[test]
    fn hello_refuses_clients_whose_range_excludes_this_version() {
        let vols = set(&[live_volume(r"C:\")]);

        match handle_request(&vols, hello_request(1, 3)) {
            Response::Hello { proto, .. } => assert_eq!(proto, PROTO_VERSION),
            other => panic!("a range covering v1 must negotiate, got {other:?}"),
        }

        // Floor above us: the client needs a protocol this build cannot speak.
        match handle_request(&vols, hello_request(2, 4)) {
            Response::Error { code, .. } => assert_eq!(code, ProtoError::Unsupported),
            other => panic!("proto_min: 2 must be refused, got {other:?}"),
        }

        // Ceiling below us: no overlap either. Without the check this answered
        // `proto: 0`, a version neither side implements.
        match handle_request(&vols, hello_request(0, 0)) {
            Response::Error { code, .. } => assert_eq!(code, ProtoError::Unsupported),
            other => panic!("proto_max: 0 must be refused, got {other:?}"),
        }
    }

    /// `Hello.ready` and `Status.volumes_incomplete` answer the same question,
    /// so they must never contradict each other.
    #[test]
    fn hello_ready_never_contradicts_status() {
        for vols in [
            set(&[live_volume(r"C:\")]),
            set(&[
                live_volume(r"C:\"),
                volume(r"D:\", windows_index(), VolumePhase::Bootstrapping),
            ]),
        ] {
            let Response::Hello { ready, .. } = handle_request(&vols, hello_request(1, 1)) else {
                panic!("a v1 client must negotiate")
            };
            assert_eq!(
                ready,
                !build_status(&vols).volumes_incomplete,
                "ready must track the same bar as volumes_incomplete"
            );
        }
    }

    #[test]
    fn starts_with_ci_respects_length_and_case() {
        assert!(starts_with_ci(r"c:\x", r"C:\"));
        assert!(starts_with_ci(r"C:\", r"C:\"));
        // Shorter than the prefix: must not panic or match.
        assert!(!starts_with_ci("C:", r"C:\"));
        assert!(!starts_with_ci(r"D:\x", r"C:\"));
    }

    fn file_record(frn: u64, parent: u64, name: &str) -> ParsedUsnRecord {
        let mut rec = dir_record(frn, parent, name);
        rec.attributes = 0;
        rec
    }

    fn delete_record(frn: u64) -> ParsedUsnRecord {
        let mut rec = file_record(frn, NTFS_ROOT_FRN.0, "");
        rec.reason = goz_core::usn::record::USN_REASON_FILE_DELETE;
        rec
    }

    /// `C:\Windows\System32` holding `a.log` (100 bytes), `b.log` (no size
    /// read yet) and the folder `logs`, with `c.log` (7 bytes) at the root.
    fn sized_index() -> VolumeIndex {
        let mut index = windows_index();
        index.insert_enum(&file_record(200, 101, "a.log"));
        index.insert_enum(&file_record(201, 101, "b.log"));
        index.insert_enum(&dir_record(202, 101, "logs"));
        index.insert_enum(&file_record(203, NTFS_ROOT_FRN.0, "c.log"));
        index.set_stat(Frn(200), 100, 0);
        index.set_stat(Frn(203), 7, 0);
        index
    }

    fn total_size_of(q: goz_core::proto::QueryRequest) -> goz_core::proto::QueryRequest {
        goz_core::proto::QueryRequest {
            limit: Some(0),
            want_total_size: true,
            ..q
        }
    }

    /// `-get-total-size` gets one empty page whose total size adds every
    /// volume's files: no folder, and no file whose size is unknown.
    #[test]
    fn the_total_size_adds_the_files_of_every_volume() {
        let vols = set(&[
            volume(r"C:\", sized_index(), VolumePhase::Live),
            volume(r"D:\", sized_index(), VolumePhase::Live),
        ]);
        let pages = results(&encode_query_binary(&vols, total_size_of(query("log"))));
        assert_eq!(pages.len(), 1);
        assert!(pages[0].items.is_empty());
        assert_eq!(pages[0].total, 8, "four matches a volume, the folder too");
        assert_eq!(pages[0].total_size, Some(2 * 107));

        let below = total_size_of(scoped("log", r"D:\Windows\System32"));
        assert_eq!(
            results(&encode_query_binary(&vols, below))[0].total_size,
            Some(100)
        );
        let nowhere = total_size_of(scoped("log", r"Q:\"));
        assert_eq!(
            results(&encode_query_binary(&vols, nowhere))[0].total_size,
            Some(0)
        );
    }

    /// Two volumes whose sums pass `u64::MAX` together: the total stops there
    /// rather than wrapping, or panicking the query task in a debug build.
    #[test]
    fn the_total_size_of_every_volume_stops_at_u64_max() {
        let huge = || {
            let mut index = windows_index();
            index.insert_enum(&file_record(200, 101, "huge.bin"));
            index.set_stat(Frn(200), u64::MAX / 2 + 1, 0);
            index
        };
        let vols = set(&[
            volume(r"C:\", huge(), VolumePhase::Live),
            volume(r"D:\", huge(), VolumePhase::Live),
        ]);
        let pages = results(&encode_query_binary(&vols, total_size_of(query("huge"))));
        assert_eq!(pages[0].total_size, Some(u64::MAX));
    }

    /// Unasked, a page carries no total size, so its bytes are the ones a
    /// client built before the field decodes. Asked with rows, every page
    /// repeats the whole set's sum, as it repeats `total`.
    #[test]
    fn the_total_size_comes_only_when_asked_and_on_every_page() {
        let vols = set(&[volume(r"C:\", sized_index(), VolumePhase::Live)]);
        let plain = results(&encode_query_binary(&vols, query("log")));
        assert_eq!(plain[0].total_size, None);

        let mut index = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
        index.insert_enum(&dir_record(100, NTFS_ROOT_FRN.0, "many"));
        let n = PAGE_ROWS as u64 + 1;
        for i in 0..n {
            index.insert_enum(&file_record(1000 + i, 100, &format!("hit{i:05}.log")));
            index.set_stat(Frn(1000 + i), 3, 0);
        }
        let vols = set(&[volume(r"C:\", index, VolumePhase::Live)]);
        let q = goz_core::proto::QueryRequest {
            want_total_size: true,
            ..query("hit")
        };
        let pages = results(&encode_query_binary(&vols, q));
        assert_eq!(pages.len(), 2);
        for page in &pages {
            assert_eq!(page.total_size, Some(3 * n));
        }
    }

    /// A query's timing counts what it did, so its log line can be read
    /// against the durations: every volume searched, each page and row sent,
    /// and every match, a limit or not.
    #[test]
    fn a_query_timing_counts_its_volumes_pages_and_rows() {
        let mut index = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
        index.insert_enum(&dir_record(100, NTFS_ROOT_FRN.0, "many"));
        let n = PAGE_ROWS as u64 + 1;
        for i in 0..n {
            index.insert_enum(&file_record(1000 + i, 100, &format!("hit{i:05}.log")));
        }
        let vols = set(&[
            volume(r"C:\", index, VolumePhase::Live),
            live_volume(r"D:\"),
        ]);
        let mut emitted = 0;
        let timing = super::stream_query_binary(&vols, query("hit"), &mut |_| {
            emitted += 1;
            true
        });
        assert_eq!(timing.volumes, 2);
        assert_eq!(timing.pages, 2);
        assert_eq!(emitted, 2);
        assert_eq!(timing.rows_sent, n);
        assert_eq!(timing.total, n);

        let limited = goz_core::proto::QueryRequest {
            limit: Some(10),
            ..query("hit")
        };
        let timing = super::stream_query_binary(&vols, limited, &mut |_| true);
        assert_eq!((timing.pages, timing.rows_sent, timing.total), (1, 10, n));
    }

    /// A parent cycle no record has broken makes every query pay the
    /// presentability walk, so `--status` must say so, apart from orphans.
    #[test]
    fn status_reports_a_parent_cycle() {
        let mut index = windows_index();
        index.insert_enum(&dir_record(100, 101, "Windows"));
        index.optimize_storage();
        assert!(index.has_parent_cycle());
        let vols = set(&[
            volume(r"C:\", index, VolumePhase::Live),
            live_volume(r"D:\"),
        ]);
        let status = build_status(&vols);
        assert!(status.volumes[0].has_parent_cycle);
        assert!(!status.volumes[0].has_orphans);
        assert!(!status.volumes[1].has_parent_cycle);
    }

    fn scoped(q: &str, scope: &str) -> goz_core::proto::QueryRequest {
        goz_core::proto::QueryRequest {
            scope: Some(scope.to_string()),
            ..query(q)
        }
    }

    /// The scope cache keeps the entry of the last `-path` folder across
    /// journal batches and compactions. Once that folder was removed, its slot
    /// still held the old name id: after a compaction the id was past the end
    /// of the name table and the next search of the folder panicked, or it
    /// named the folder's own new copy, so the dead slot validated and the
    /// search returned nothing.
    #[test]
    fn a_removed_scope_folder_is_resolved_again_after_a_compaction() {
        // Panic shape: "proj" is the newest name, then every name but the
        // volume's own dies.
        let mut index = windows_index();
        let mut batch: Vec<_> = (0..400)
            .map(|i| file_record(1000 + i, NTFS_ROOT_FRN.0, &format!("{i:040}")))
            .collect();
        batch.push(dir_record(200, NTFS_ROOT_FRN.0, "proj"));
        index.apply_batch(&batch);
        let vol = volume(r"C:\", index, VolumePhase::Live);
        let vols = set(std::slice::from_ref(&vol));
        encode_query_binary(&vols, scoped("inside", r"C:\proj"));
        assert!(vol.scope_cache.lock().is_some());
        {
            let mut index = vol.index.write();
            let dels: Vec<_> = (0..400)
                .map(|i| delete_record(1000 + i))
                .chain([delete_record(200)])
                .collect();
            index.apply_batch(&dels);
            let plan = index.plan_compaction().expect("garbage");
            index.apply_compaction(plan);
        }
        let pages = results(&encode_query_binary(&vols, scoped("inside", r"C:\proj")));
        assert_eq!(pages[0].total, 0, "the folder is gone");

        // Wrong-result shape: proj is removed and made again in another
        // slot, and the compaction gives the new "proj" the dead slot's id.
        let mut index = windows_index(); // ids 0 "", 1 $LostFound, 2-3 the tree
        let mut batch = vec![
            file_record(150, NTFS_ROOT_FRN.0, "keep.txt"), // id 4
            dir_record(200, NTFS_ROOT_FRN.0, "proj"),      // id 5
        ];
        batch.extend((0..400).map(|i| file_record(1000 + i, NTFS_ROOT_FRN.0, &format!("{i:040}"))));
        index.apply_batch(&batch);
        let vol = volume(r"C:\", index, VolumePhase::Live);
        let vols = set(std::slice::from_ref(&vol));
        encode_query_binary(&vols, scoped("inside", r"C:\proj"));
        let cached = vol.scope_cache.lock().as_ref().unwrap().entry;
        {
            let mut index = vol.index.write();
            let mut recs = vec![delete_record(200)];
            recs.extend((0..400).map(|i| delete_record(1000 + i)));
            recs.push(dir_record(300, NTFS_ROOT_FRN.0, "proj"));
            recs.push(file_record(301, 300, "inside.txt"));
            index.apply_batch(&recs);
            assert_ne!(index.head_of(Frn(300)), Some(cached), "fixture: a new slot");
            let plan = index.plan_compaction().expect("garbage");
            index.apply_compaction(plan);
            assert!(!scope_entry_valid(&index, cached, b"proj"));
        }
        let pages = results(&encode_query_binary(&vols, scoped("inside", r"C:\proj")));
        assert_eq!(pages[0].total, 1, "the new proj's file is found");
    }
}
