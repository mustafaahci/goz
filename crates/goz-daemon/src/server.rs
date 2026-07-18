//! The named-pipe query server.
//!
//! A tokio multi-thread runtime accepts connections on `\\.\pipe\goz-v1`,
//! frames requests/responses with the shared sans-io codec from
//! `goz-core::proto`, and answers each `Query` by locking every volume's index
//! for reads, running the query, and merging the results. Indexing itself
//! (bootstrap + tail) stays on dedicated OS threads; tokio only serves the pipe.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use goz_core::proto::{
    DaemonStatus, MemPair, PIPE_NAME, PROTO_VERSION, ProtoError, Request, Response, VolumeMemory,
    VolumeStatus,
};
use goz_core::proto::{
    MAX_CLIENT_FRAME, PAGE_ROWS, QueryRequest, encode_response_frame, push_item,
    push_results_header,
};
use goz_core::query::{parse_query, resolve_scope, run_query_deferrable};
use goz_core::types::{EntryIdx, SortKey, SortSpec};
use goz_winfs::{PipeSecurity, build_pipe_security};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::{Semaphore, watch};

use crate::volume_state::{VolumeSet, VolumeState};

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
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Runs the pipe server until stopped. Blocks on a tokio runtime.
///
/// Stops on either Ctrl+C (console mode) or `shutdown` flipping to `true` (the
/// service-control handler's Stop/Shutdown path). Keep at least one
/// `watch::Sender` alive for the duration in console mode: if every sender
/// drops, `changed()` resolves and would end the loop prematurely.
pub(crate) fn run(volumes: VolumeSet, shutdown: watch::Receiver<bool>) -> Result<()> {
    let security = build_pipe_security().context("building pipe security descriptor")?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;
    rt.block_on(accept_loop(volumes, security, shutdown))
}

async fn accept_loop(
    volumes: VolumeSet,
    security: PipeSecurity,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let mut server = create_instance(&security, true)?;
    let conn_limit = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    tracing::info!(pipe = PIPE_NAME, "query server listening");
    loop {
        tokio::select! {
            r = server.connect() => {
                r.context("pipe connect")?;
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
                            if let Err(e) = handle_connection(connected, vols).await {
                                tracing::debug!(error = %e, "connection ended");
                            }
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

async fn handle_connection(mut server: NamedPipeServer, volumes: VolumeSet) -> Result<()> {
    let mut decoder = goz_core::proto::FrameDecoder::new(MAX_CLIENT_FRAME);
    let mut read_buf = vec![0u8; 16 * 1024];
    loop {
        // Reap idle/half-open connections: a connected client that never sends a
        // request is dropped after IDLE_TIMEOUT, freeing its task/buffer/pipe.
        let n = match tokio::time::timeout(IDLE_TIMEOUT, server.read(&mut read_buf)).await {
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
                    let vols = volumes.clone();
                    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(QUERY_PAGE_BACKLOG);
                    let encoder = tokio::task::spawn_blocking(move || {
                        // A failed send means the receiver is gone (client
                        // vanished or write error): stop encoding.
                        stream_query_binary(&vols, q, &mut |page| tx.blocking_send(page).is_ok());
                    });
                    let mut write_err: Option<std::io::Error> = None;
                    while let Some(page) = rx.recv().await {
                        if let Err(e) = server.write_all(&page).await {
                            write_err = Some(e);
                            break;
                        }
                    }
                    // Closing the channel unblocks a still-encoding task; it
                    // aborts at its next emit.
                    drop(rx);
                    encoder.await.context("query task panicked")?;
                    if let Some(e) = write_err {
                        return Err(e.into());
                    }
                    continue;
                }
                // Hello / Status are trivial; encode inline as a tagged JSON frame.
                Ok(req) => {
                    let mut out = Vec::new();
                    encode_response_frame(&handle_request(&volumes, req), &mut out);
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
            server.write_all(&out).await?;
        }
    }
}

/// Collects every streamed page into one buffer: the tests' view of
/// [`stream_query_binary`] (production writes each page to the pipe as it is
/// encoded and never holds the whole response).
#[cfg(test)]
fn encode_query_binary(volumes: &VolumeSet, q: QueryRequest) -> Vec<u8> {
    let mut out = Vec::new();
    stream_query_binary(volumes, q, &mut |page| {
        out.extend_from_slice(&page);
        true
    });
    out
}

/// Runs a query and emits the framed response as it is produced: one or more
/// binary results-page frames (paged at [`PAGE_ROWS`], `more` set on all but the
/// last), or a single tagged JSON error frame if the query failed to parse. An
/// empty result set still emits one `more: false` page so the client always sees
/// a terminal frame. Each framed page is handed to `emit` as soon as it is
/// encoded, and the matched hits are consumed as it goes, so a full export holds
/// neither the whole response nor the already-emitted hits' path bytes. `emit`
/// returning `false` means the consumer is gone; encoding stops.
///
/// Pages are encoded straight from each matched hit's stored path bytes: the
/// mount prefix and the volume-relative path are copied into one reused scratch
/// buffer and handed to [`push_item`], so a match never allocates an
/// intermediate `ResultItem`/`String`, and the set is never cloned for paging or
/// re-serialized as JSON. This is the whole point of the binary wire: on a
/// multi-million-path result set it is ~12x less serialization work than the old
/// JSON path.
fn stream_query_binary(
    volumes: &VolumeSet,
    q: QueryRequest,
    emit: &mut dyn FnMut(Vec<u8>) -> bool,
) {
    let parsed = match parse_query(&q.query) {
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
            return;
        }
    };

    let per_vol_limit = q.limit.map(|l| q.offset.saturating_add(l));
    let targets = targets(volumes, &q.scope);

    // One pre-sorted run per volume that has hits: run_query already ordered each
    // volume's hits by `q.sort`, so the runs are merged (not re-sorted) below.
    let mut runs: Vec<Vec<VolHit>> = Vec::new();
    let mut total: u64 = 0;
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
        let index = vol.index.read();
        let scope_entry = match scope {
            VolScope::All => None,
            VolScope::Rel(rel) => match resolve_scope_cached(vol, &index, rel, q.match_case) {
                Some(e) => Some(e),
                None => continue, // scope folder absent on this volume: no hits
            },
        };
        // A limited query orders INSIDE the lock only when the limit actually
        // slices the match set: picking which `per_vol_limit` matches survive
        // needs the index, and only that many paths are then reconstructed.
        //
        // When the page covers every match there is nothing to pick, so ordering
        // is pure post-processing over owned rows and every sort key comes from
        // the hit itself (`sort_collected` touches no index). Sorting a million
        // rows under the read lock blocked this volume's tail thread from
        // applying a single journal record for the whole sort; doing it after
        // the guard drops blocks nobody.
        //
        // Which case applies depends on the match count, which is not known
        // until the scan has run, so the executor decides and reports back via
        // `outcome.sorted`. Testing `limit.is_none()` here instead meant a
        // client asking for everything as a NUMBER (`-n 4294967295`, or any
        // limit at or above the match count) took the in-lock path and
        // re-introduced the very stall the deferral exists to prevent.
        let outcome = run_query_deferrable(&index, &parsed, scope_entry, q.sort, 0, per_vol_limit);
        total += outcome.total;
        let sorted = outcome.sorted;
        drop(index); // hits own their bytes; release the read lock early

        let mut run: Vec<_> = outcome
            .hits
            .into_iter()
            .map(|hit| (vol.clone(), hit))
            .collect();
        if !sorted {
            // Off the lock. `order_merged` k-way merges runs it assumes are
            // already sorted, so this is not optional.
            sort_collected(&mut run, q.sort);
        }
        if !run.is_empty() {
            runs.push(run);
        }
    }

    // Merge/order the pre-sorted per-volume runs into one global order.
    let collected = order_merged(runs, q.sort);
    let start = (q.offset as usize).min(collected.len());
    let end = q
        .limit
        .map(|l| (start + l as usize).min(collected.len()))
        .unwrap_or(collected.len());
    let n_rows = end - start;

    let page = PAGE_ROWS as usize;
    let n_pages = n_rows.div_ceil(page).max(1);
    // Consuming iteration: each emitted page's hits DROP right after
    // encoding, so the held path bytes drain with export progress instead of
    // peaking all together (skipped offset rows and beyond-limit rows drop
    // with the iterator).
    let mut rows_iter = collected.into_iter().skip(start).take(n_rows);
    let mut path = Vec::new();
    for i in 0..n_pages {
        let rows = if i + 1 < n_pages {
            page
        } else {
            n_rows - i * page
        };
        // Frame = 4-byte LE length placeholder + payload, length backfilled
        // once the payload is written (avoids a second buffer + copy per
        // page). ~96 bytes/row covers a typical path + fields without a
        // mid-page regrow.
        let mut out = Vec::with_capacity(rows * 96 + 64);
        out.extend_from_slice(&[0u8; 4]);
        push_results_header(
            &mut out,
            total,
            0, // generation: unused by the CLI, matches the prior wire
            i + 1 < n_pages,
            volumes_incomplete,
            metadata_pending,
            rows as u32,
        );
        for (vol, hit) in rows_iter.by_ref().take(rows) {
            path.clear();
            path.extend_from_slice(vol.mount_prefix().as_bytes());
            path.extend_from_slice(&hit.path);
            let size = q.want_size.then_some(hit.size).flatten();
            let mtime = q.want_mtime.then_some(hit.mtime).flatten();
            push_item(&mut out, &path, hit.is_dir, size, mtime);
        }
        let payload_len = (out.len() - 4) as u32;
        out[0..4].copy_from_slice(&payload_len.to_le_bytes());
        if !emit(out) {
            return;
        }
    }
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
fn targets(volumes: &VolumeSet, scope: &Option<String>) -> Vec<(Arc<VolumeState>, VolScope)> {
    match scope {
        None => volumes.iter().map(|v| (v.clone(), VolScope::All)).collect(),
        Some(path) => {
            let Some(vol) = volumes
                .iter()
                .find(|v| starts_with_ci(path, v.mount_prefix()))
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
            let raw = &path.as_bytes()[vol.mount_prefix().len()..];
            let end = raw
                .iter()
                .rposition(|&b| b != b'\\' && b != b'/')
                .map_or(0, |i| i + 1);
            vec![(vol.clone(), VolScope::Rel(raw[..end].to_vec()))]
        }
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

/// A query hit paired with the volume it came from (the source of its mount
/// prefix), the unit the server orders and pages.
type VolHit = (Arc<VolumeState>, goz_core::query::QueryHit);

/// Orders the merged cross-volume hits by the sort spec (mirrors the engine's
/// comparator, but on full paths since volumes differ by prefix).
fn sort_collected(hits: &mut [VolHit], sort: SortSpec) {
    use std::cmp::Reverse;
    // Compute each element's folded key ONCE (O(n) folds) instead of re-folding
    // inside every comparison (O(n log n)). Desc is applied via `Reverse` on the
    // key so tie/stable order matches the old `sort_by(..).reverse()` exactly.
    let desc = sort.dir == goz_core::types::SortDir::Desc;
    match sort.key {
        SortKey::Name => {
            if desc {
                hits.sort_by_cached_key(|(_, a)| Reverse(folded_name(&a.path)));
            } else {
                hits.sort_by_cached_key(|(_, a)| folded_name(&a.path));
            }
        }
        SortKey::Path => {
            if desc {
                hits.sort_by_cached_key(|(va, a)| Reverse(full_folded(va, a)));
            } else {
                hits.sort_by_cached_key(|(va, a)| full_folded(va, a));
            }
        }
        SortKey::Size => {
            if desc {
                hits.sort_by_key(|(_, a)| Reverse(a.size.unwrap_or(0)));
            } else {
                hits.sort_by_key(|(_, a)| a.size.unwrap_or(0));
            }
        }
        SortKey::DateModified => {
            if desc {
                hits.sort_by_key(|(_, a)| Reverse(a.mtime.unwrap_or(i64::MIN)));
            } else {
                hits.sort_by_key(|(_, a)| a.mtime.unwrap_or(i64::MIN));
            }
        }
    }
}

fn full_folded(vol: &VolumeState, hit: &goz_core::query::QueryHit) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(vol.mount_prefix().as_bytes());
    v.extend_from_slice(&hit.path);
    goz_core::fold::fold(&v)
}

/// Folded file name (the last `\`-separated component) of a hit path: the
/// cross-volume key for a name sort. Matches `VolumeIndex::folded_name`, so a run
/// the engine ordered by name is already ordered by this key.
fn folded_name(path: &[u8]) -> Vec<u8> {
    let ns = path
        .iter()
        .rposition(|&b| b == b'\\')
        .map(|i| i + 1)
        .unwrap_or(0);
    goz_core::fold::fold(&path[ns..])
}

/// Globally orders the per-volume runs (each already sorted by `sort` inside
/// [`run_query_deferrable`]) into one list.
///
/// A name sort needs no re-sort: the folded file name is volume-independent and
/// each run is already in that order, so the runs are k-way merged in O(n),
/// skipping the O(n log n) re-sort plus the full re-fold that dominated a broad
/// query (~950 ms -> ~150 ms on a 2.8M-hit set). A single run, the common case
/// of all matches on one volume, is already global and returned as-is. Other
/// sort keys, whose cross-volume key differs from the per-volume one (path
/// carries the mount prefix) or which are rare at scale, fall back to
/// flatten-and-[`sort_collected`].
fn order_merged(runs: Vec<Vec<VolHit>>, sort: SortSpec) -> Vec<VolHit> {
    if runs.len() <= 1 {
        return runs.into_iter().next().unwrap_or_default();
    }
    if sort.key != SortKey::Name {
        let mut all: Vec<_> = runs.into_iter().flatten().collect();
        sort_collected(&mut all, sort);
        return all;
    }
    let keyed: Vec<Vec<(Vec<u8>, VolHit)>> = runs
        .into_iter()
        .map(|run| {
            run.into_iter()
                .map(|(v, h)| (folded_name(&h.path), (v, h)))
                .collect()
        })
        .collect();
    let desc = sort.dir == goz_core::types::SortDir::Desc;
    debug_assert!(
        keyed.iter().all(|r| r.windows(2).all(|w| {
            let ord = w[0].0.cmp(&w[1].0);
            if desc {
                ord != std::cmp::Ordering::Less
            } else {
                ord != std::cmp::Ordering::Greater
            }
        })),
        "engine run not ordered by the server name key; k-way merge would misorder"
    );
    kway_merge(keyed, desc)
}

/// Stable k-way merge of pre-sorted `(key, item)` runs. Each run must already be
/// ordered ascending (or descending when `desc`); equal keys resolve to the
/// earliest run, so the result matches a stable sort of the concatenation. `k`
/// (the volume count) is tiny, so a linear min-scan over the run heads is used
/// rather than a heap.
fn kway_merge<T>(runs: Vec<Vec<(Vec<u8>, T)>>, desc: bool) -> Vec<T> {
    use std::collections::VecDeque;
    let total: usize = runs.iter().map(Vec::len).sum();
    let mut dqs: Vec<VecDeque<(Vec<u8>, T)>> = runs.into_iter().map(VecDeque::from).collect();
    let mut out = Vec::with_capacity(total);
    for _ in 0..total {
        let mut best: Option<usize> = None;
        for ri in 0..dqs.len() {
            let Some((k, _)) = dqs[ri].front() else {
                continue;
            };
            best = Some(match best {
                None => ri,
                Some(bi) => {
                    let ord = k.cmp(&dqs[bi][0].0);
                    let take = if desc {
                        ord == std::cmp::Ordering::Greater
                    } else {
                        ord == std::cmp::Ordering::Less
                    };
                    if take { ri } else { bi }
                }
            });
        }
        let bi = best.expect("total counts every remaining item");
        out.push(dqs[bi].pop_front().expect("front present").1);
    }
    out
}

/// Case-insensitive ASCII prefix test (drive letters and `\` are ASCII).
fn starts_with_ci(haystack: &str, prefix: &str) -> bool {
    haystack.len() >= prefix.len()
        && haystack.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::{
        VolScope, build_status, encode_query_binary, handle_request, kway_merge,
        resolve_scope_cached, scope_entry_valid, starts_with_ci, targets,
    };
    use crate::volume_state::{VolumeSet, VolumeState};
    use goz_core::index::{FrnMap, NTFS_ROOT_FRN, VolumeIndex};
    use goz_core::proto::{PAGE_ROWS, PROTO_VERSION, ProtoError, Request, Response};
    use goz_core::query::{parse_query, resolve_scope, run_query};
    use goz_core::types::{Frn, SortKey, SortSpec, VolumePhase};
    use goz_core::usn::record::{
        FILE_ATTRIBUTE_DIRECTORY, ParsedUsnRecord, USN_REASON_FILE_CREATE,
    };
    use parking_lot::{Mutex, RwLock};
    use std::sync::Arc;

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

    /// `folded_name` here and `VolumeIndex::folded_name` in goz-core are two
    /// implementations of the same key, in two crates, with nothing linking
    /// them. [`order_merged`] k-way merges runs the ENGINE sorted, keyed by what
    /// the SERVER computes, so any drift silently misorders a multi-volume name
    /// sort. The only guard is a `debug_assert!` that needs a multi-volume,
    /// multi-hit name sort to fire at all, so drift reaches release unseen.
    #[test]
    fn server_folded_name_agrees_with_the_index_key() {
        let index = merge_index();
        let parsed = parse_query("log").expect("query parses");
        let outcome = run_query(
            &index,
            &parsed,
            None,
            SortSpec::default_for(SortKey::Name),
            0,
            None,
        );
        assert_eq!(
            outcome.hits.len(),
            2,
            "fixture must produce hits to compare"
        );

        for hit in &outcome.hits {
            assert_eq!(
                super::folded_name(&hit.path),
                index.folded_name(hit.idx),
                "server key for {:?} diverged from the index key the engine sorted by",
                String::from_utf8_lossy(&hit.path)
            );
        }
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

    fn run(items: &[(&str, char)]) -> Vec<(Vec<u8>, char)> {
        items
            .iter()
            .map(|(k, v)| (k.as_bytes().to_vec(), *v))
            .collect()
    }

    #[test]
    fn merge_ascending_is_stable_across_and_within_runs() {
        // Two pre-sorted runs. On the tied key "c", the earlier run's items come
        // first and keep their in-run order, matching a stable sort of run0 ++ run1.
        let a = run(&[("a", '1'), ("c", '2'), ("c", '3')]);
        let b = run(&[("b", '4'), ("c", '5')]);
        assert_eq!(kway_merge(vec![a, b], false), vec!['1', '4', '2', '3', '5']);
    }

    #[test]
    fn merge_descending_picks_largest_head() {
        let a = run(&[("c", '1'), ("a", '2')]); // each run already in desc order
        let b = run(&[("b", '3')]);
        assert_eq!(kway_merge(vec![a, b], true), vec!['1', '3', '2']);
    }

    #[test]
    fn merge_handles_empty_and_single() {
        assert_eq!(kway_merge::<char>(vec![], false), Vec::<char>::new());
        assert_eq!(
            kway_merge(vec![run(&[("x", '1'), ("y", '2')])], false),
            vec!['1', '2']
        );
        // An empty run interleaved with a non-empty one is skipped cleanly.
        assert_eq!(
            kway_merge(vec![run(&[]), run(&[("a", '1')]), run(&[])], false),
            vec!['1']
        );
    }
}
