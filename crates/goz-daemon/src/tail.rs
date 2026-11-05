//! Per-volume USN journal tail: one dedicated OS thread that builds the
//! volume's index, then reads the change journal from the build's cursor
//! forward and applies each batch to the shared index, keeping it live.
//!
//! v1 polls with a non-blocking drain and a short sleep when caught up, which
//! gives sub-100 ms freshness and a clean, `CancelSynchronousIo`-free shutdown
//! (the blocking `BytesToWaitFor=1` push variant is a post-v1 refinement). It
//! applies structural changes (create / rename / delete) immediately and
//! collects `NeedsStat` side effects for the enricher, which refreshes the
//! size/date a USN record does not carry.
//!
//! Hard links are reconciled out of band: a `HARD_LINK_CHANGE` record names one
//! link and may name a dead one, so the real set comes from a Win32 link walk
//! (`goz_winfs::link_paths`) fed to `VolumeIndex::reconcile_links`. A walk that
//! fails, even part way, or names a folder the index does not hold yet, is
//! skipped, never applied as a partial set (which would delete the links it
//! missed), and counted as `IndexStats::link_reconciles_dropped` so `--status`
//! shows it rather than hiding it. A file that is gone is left to its delete
//! record.

use std::collections::hash_map::Entry;
use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use goz_core::index::{LinkTarget, VolumeIndex, WtfName};
use goz_core::query::resolve_scope;
use goz_core::types::{Frn, VolumePhase};
use goz_core::usn::record::{
    USN_REASON_BASIC_INFO_CHANGE, USN_REASON_CLOSE, USN_REASON_DATA_EXTEND,
    USN_REASON_DATA_OVERWRITE, USN_REASON_DATA_TRUNCATION, USN_REASON_FILE_CREATE,
    USN_REASON_FILE_DELETE, USN_REASON_HARD_LINK_CHANGE, USN_REASON_RENAME_NEW_NAME,
    USN_REASON_RENAME_OLD_NAME,
};
use goz_core::usn::{RescanReason, walk_journal_buffer};
use goz_winfs::{
    ERROR_JOURNAL_DELETE_IN_PROGRESS, ERROR_JOURNAL_ENTRY_DELETED, ERROR_JOURNAL_NOT_ACTIVE,
    JournalInfo, VolumeHandle, read_usn_journal, stat_file,
};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::bootstrap::{self, JournalSetup};
use crate::pulse::TailPulse;
use crate::volume_state::VolumeState;

/// Reason bits we ask the kernel to report: everything that changes a name,
/// a hard link, or a file's size/timestamps.
const REASON_MASK: u32 = USN_REASON_FILE_CREATE
    | USN_REASON_FILE_DELETE
    | USN_REASON_RENAME_OLD_NAME
    | USN_REASON_RENAME_NEW_NAME
    | USN_REASON_HARD_LINK_CHANGE
    | USN_REASON_BASIC_INFO_CHANGE
    | USN_REASON_DATA_OVERWRITE
    | USN_REASON_DATA_EXTEND
    | USN_REASON_DATA_TRUNCATION
    | USN_REASON_CLOSE;

/// 512 KiB journal read buffer (~5k records per fill).
const READ_BUFFER_BYTES: usize = 512 * 1024;
/// Poll interval when the journal is caught up.
const IDLE_POLL: Duration = Duration::from_millis(50);

/// How long journal reads may keep failing before the volume stops claiming to
/// be `Live`. One failure is not news (a drive spinning up answers
/// `ERROR_NOT_READY` for a moment); several seconds of them means the index has
/// stopped tracking the volume, which every consumer must be told.
const DEGRADE_AFTER: Duration = Duration::from_secs(5);
/// How long journal reads may keep failing before retrying stops and a
/// bounded rescan takes over. Long enough that a spin-up's `ERROR_NOT_READY`
/// window (~15 s worst observed) never triggers a needless rebuild, short
/// enough that an unmappable error (see the escalation comment in the read
/// loop) costs seconds of zombie time, not forever.
const ESCALATE_AFTER: Duration = Duration::from_secs(30);
/// Read-retry backoff bounds. The ceiling is what keeps a permanently sick
/// volume from writing a log line every 500 ms for the life of the service.
/// The ceiling is `pub(crate)` because the supervisor's stall threshold must
/// comfortably exceed it, and a test over there pins that coupling.
const RETRY_BACKOFF_MIN: Duration = Duration::from_millis(500);
pub(crate) const RETRY_BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Longest wait between journal asks while a rescan waits for an unavailable
/// journal. Every ask that fails logs a line, so a disk that never comes back
/// costs a dozen lines an hour rather than one every 30 s.
const JOURNAL_WAIT_MAX: Duration = Duration::from_secs(5 * 60);
/// Consecutive rescans before giving up and marking the volume Failed: a storm
/// guard so a persistently broken journal cannot spin on rebuilds forever. Reset
/// after any healthy batch.
const MAX_RESCANS: u32 = 3;

/// A running tail thread and the flag that stops it.
pub(crate) struct TailHandle {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    /// Volume this tail serves, for supervisor log lines.
    pub(crate) mount: String,
    /// The tail's self-reported liveness: beating, busy in a rebuild, or
    /// retired. The supervisor's stall decisions read only this, never
    /// `state.phase`, which is user-facing honesty state; the phase is
    /// consulted only for what it says about the volume: whether it is served
    /// (the supervisor's `accrue_health`) and whether a retired tail left a
    /// snapshot, which needs no revival.
    pub(crate) pulse: Arc<TailPulse>,
    /// Real handle to the tail thread, target of `cancel_synchronous_io`.
    pub(crate) io_handle: Arc<goz_winfs::ThreadIoHandle>,
}

impl TailHandle {
    /// Signals the tail to stop and joins it. Only safe against a thread
    /// that will actually observe the flag; teardown of a possibly-wedged
    /// tail goes through the supervisor's cancel-and-poll loop instead.
    pub(crate) fn shutdown(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }

    /// Signals the tail to stop without joining, so teardown can flag every
    /// tail before waiting on any single one of them.
    pub(crate) fn signal_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Whether the tail thread has exited, i.e. a join would not block.
    pub(crate) fn is_finished(&self) -> bool {
        self.join
            .as_ref()
            .is_none_or(std::thread::JoinHandle::is_finished)
    }
}

/// Spawns the tail thread for one volume. `handle` is moved into the thread
/// (it is not shared). A tail starts without a cursor: its first act is a
/// full build, which builds the index and acquires a fresh cursor, whether the
/// volume was just found or its previous tail exited and left it untailed for
/// a while.
pub(crate) fn spawn(state: Arc<VolumeState>, handle: VolumeHandle) -> std::io::Result<TailHandle> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let pulse = Arc::new(TailPulse::new());
    let pulse_thread = pulse.clone();
    let mount = state.mount_prefix().to_string();
    // The supervisor needs a REAL handle to this thread, and duplicating one
    // must happen ON the thread (the pseudo-handle is per-thread), so the
    // thread sends it back before entering the loop.
    let (io_tx, io_rx) = std::sync::mpsc::sync_channel(1);
    // OS thread creation can fail on a machine near its thread/handle limit.
    // Propagate it so the supervisor retries this one volume rather than
    // panic the whole daemon.
    let join = std::thread::Builder::new()
        .name(format!("goz-tail-{mount}"))
        .spawn(move || {
            let io = goz_winfs::current_thread_io_handle();
            let usable = io.is_ok();
            let _ = io_tx.send(io);
            if !usable {
                // `spawn` is about to report failure and drop this thread's
                // stop flag and join handle. Running on anyway would leak a
                // live index writer that nobody can stop, join, supervise,
                // or ever rescue from a wedge; and a caller that retries the
                // spawn (the supervisor's revive) would then stack a second
                // writer on the same volume. No tail beats a ghost tail.
                return;
            }
            // Publishes `Retired` on ANY exit: the stop flag, a give-up
            // return, or a panic unwind. This is what lets the supervisor
            // trust silence: the only way to stop stamping without retiring
            // is to be genuinely wedged in a syscall. The handle moves INTO
            // tail_loop so its CloseHandle runs before this guard fires;
            // `Retired` is a promise that joining is instant, and a close is
            // itself an IRP a sick device can pend.
            let _retired = pulse_thread.retire_on_drop();
            tail_loop(&state, handle, &stop_thread, &pulse_thread)
        })?;
    let io_handle = io_rx
        .recv()
        .map_err(|_| std::io::Error::other("tail thread died before sending its handle"))?
        .map_err(|e| std::io::Error::other(format!("duplicating tail thread handle: {e}")))?;
    Ok(TailHandle {
        stop,
        join: Some(join),
        mount,
        pulse,
        io_handle: Arc::new(io_handle),
    })
}

fn tail_loop(state: &VolumeState, handle: VolumeHandle, stop: &AtomicBool, pulse: &TailPulse) {
    // Owned so it drops in THIS frame, on return and on unwind alike, before
    // the spawning closure's retire guard publishes `Retired`. A thread that
    // claimed Retired while still wedged in its handle's CloseHandle would
    // draw an unbounded join from the supervisor's give-up or revive path.
    let handle = &handle;
    let mut rescans: u32 = 0;
    // A new tail has no cursor: a volume just found has no index yet, and a
    // revived tail's volume kept changing while nothing read its journal.
    // Build the index and pick up a fresh cursor before tailing a single
    // record. Deliberately NOT routed through `bounded_rescan`: this build is
    // the price of a start, not a journal fault, and charging it would leave
    // every incarnation one incident short of the documented budget (the
    // counter only resets on a non-empty batch, so on a quiet volume the
    // charge would never clear). Start storms are bounded by the supervisor's
    // own budget instead.
    let first = if state.indexed.load(Ordering::Acquire) {
        rescan(state, handle, pulse, RescanReason::NoCursor, stop)
    } else {
        first_build(state, handle, pulse, stop)
    };
    let Some(cursor) = first else {
        return;
    };
    let mut start_usn = cursor.next_usn;
    let mut journal_id = cursor.journal_id;
    let mut buf = vec![0u8; READ_BUFFER_BYTES];
    // Journal-read failure streak. `degraded` records that WE moved the volume
    // to Offline, so only we move it back and we never stomp a phase someone
    // else owns (Rescanning, Failed).
    let mut failing_since: Option<Instant> = None;
    let mut backoff = RETRY_BACKOFF_MIN;
    let mut degraded = false;

    while !stop.load(Ordering::Relaxed) {
        // Liveness stamp for the supervisor: as long as this loop is turning,
        // however unhealthily, no one cancels our I/O.
        pulse.beat();
        match read_usn_journal(handle, start_usn, journal_id, REASON_MASK, 0, &mut buf) {
            Ok(bytes) => {
                if degraded {
                    tracing::info!(
                        volume = %state.mount_prefix(),
                        "journal reads recovered; volume is tracking again"
                    );
                    *state.phase.write() = VolumePhase::Live;
                    degraded = false;
                }
                failing_since = None;
                backoff = RETRY_BACKOFF_MIN;
                let walk = match walk_journal_buffer(&buf[..bytes]) {
                    Ok(w) => w,
                    Err(e) => {
                        // A corrupt buffer means we cannot prove what we missed,
                        // so rebuild the index from the MFT rather than drift.
                        match bounded_rescan(
                            state,
                            handle,
                            pulse,
                            RescanReason::ParserAnomaly,
                            &format!("{e:?}"),
                            &mut rescans,
                            stop,
                        ) {
                            Some(c) => {
                                start_usn = c.next_usn;
                                journal_id = c.journal_id;
                                continue;
                            }
                            None => return,
                        }
                    }
                };
                if walk.records.is_empty() {
                    // Caught up (or only filtered-out records): advance and idle.
                    start_usn = walk.next_usn;
                    std::thread::sleep(IDLE_POLL);
                    continue;
                }
                // Apply structural changes under the write lock, then collect
                // the FRNs whose size/date must be refreshed (USN records carry
                // neither). Hard-link-dirty FRNs are stat'd too, which refreshes
                // their size/date but does NOT reconcile their names.
                //
                // Hold the write lock only for the mutation; the owned
                // `ApplyOutcome` needs no lock, so build the dirty list after
                // releasing it (keeps the query-blocking window minimal).
                //
                // The lock covers only the mutation. Arena compaction, which is
                // the one thing here that used to run long (~184 ms on a large
                // index), is no longer inline: `apply_batch` reports it and
                // `compact_index` does the copying off the exclusive lock.
                let outcome = {
                    let mut index = state.index.write();
                    index.apply_batch(&walk.records)
                };
                for anomaly in &outcome.anomalies {
                    tracing::debug!(volume = %state.mount_prefix(), ?anomaly, "index anomaly");
                }

                let mut dirty: Vec<u64> = outcome
                    .needs_stat
                    .iter()
                    .chain(&outcome.needs_link_reconcile)
                    .map(|f| f.0)
                    .collect();
                dirty.sort_unstable();
                dirty.dedup();
                enrich_stats(state, handle, &dirty, stop, pulse);
                // A hard-link change also alters the file's NAMES, which no stat
                // can refresh and which the record itself cannot be trusted for
                // (it may name a link that is already dead).
                reconcile_links(state, handle, &outcome.needs_link_reconcile, stop, pulse);
                if outcome.wants_compact {
                    compact_index(state);
                }
                start_usn = walk.next_usn;
                rescans = 0; // a healthy batch resets the rescan storm guard
            }
            Err(e) if e.code == ERROR_JOURNAL_ENTRY_DELETED => {
                // Our cursor fell off the end of the journal (wrap / deletion):
                // the index can no longer prove it is in sync, so rebuild it in
                // place and resume tailing from a fresh cursor.
                match bounded_rescan(
                    state,
                    handle,
                    pulse,
                    RescanReason::EntryDeletedError,
                    &e.to_string(),
                    &mut rescans,
                    stop,
                ) {
                    Some(c) => {
                        start_usn = c.next_usn;
                        journal_id = c.journal_id;
                        failing_since = None;
                        backoff = RETRY_BACKOFF_MIN;
                        degraded = false;
                    }
                    None => return,
                }
            }
            Err(e)
                if e.code == ERROR_JOURNAL_NOT_ACTIVE
                    || e.code == ERROR_JOURNAL_DELETE_IN_PROGRESS =>
            {
                // The journal we were tailing is gone or being deleted. Retrying
                // the same cursor against it can never succeed, so rebuild from
                // the MFT and pick up a fresh one.
                let reason = if e.code == ERROR_JOURNAL_NOT_ACTIVE {
                    RescanReason::JournalNotActive
                } else {
                    RescanReason::JournalDeleteInProgress
                };
                match bounded_rescan(
                    state,
                    handle,
                    pulse,
                    reason,
                    &e.to_string(),
                    &mut rescans,
                    stop,
                ) {
                    Some(c) => {
                        start_usn = c.next_usn;
                        journal_id = c.journal_id;
                        failing_since = None;
                        backoff = RETRY_BACKOFF_MIN;
                        degraded = false;
                    }
                    None => return,
                }
            }
            Err(e) => {
                // Some other read failure. It may be transient (a drive spinning
                // up answers ERROR_NOT_READY for a moment), so keep retrying
                // rather than abandoning the volume.
                //
                // What we must never do is retry silently while still claiming
                // `Live`: that is the third state the design forbids, where the
                // index has stopped tracking the volume and every query still
                // reports it complete. Past `DEGRADE_AFTER` the drift is real, so
                // say so and let `--status`, `Hello.ready`, and every result page
                // carry it.
                let since = *failing_since.get_or_insert_with(Instant::now);
                // Nor may we retry FOREVER: deleting the journal mid-read was
                // observed to answer the stale cursor with
                // ERROR_INVALID_PARAMETER (87), a code no arm maps, and no
                // retry of that can ever succeed, so the volume sat Offline
                // permanently while its tail beat away and the supervisor
                // (correctly) saw nothing to revive. Past `ESCALATE_AFTER`
                // the code no longer matters: reads that will not stop
                // failing mean the journal cannot be trusted, so rebuild.
                // Bounded: a rescan that fails retires the tail into the
                // supervisor's revival budget instead of looping here.
                if since.elapsed() >= ESCALATE_AFTER {
                    match bounded_rescan(
                        state,
                        handle,
                        pulse,
                        RescanReason::ReadsKeptFailing,
                        &e.to_string(),
                        &mut rescans,
                        stop,
                    ) {
                        Some(c) => {
                            start_usn = c.next_usn;
                            journal_id = c.journal_id;
                            failing_since = None;
                            backoff = RETRY_BACKOFF_MIN;
                            degraded = false;
                        }
                        None => return,
                    }
                    continue;
                }
                if !degraded && since.elapsed() >= DEGRADE_AFTER {
                    tracing::error!(
                        volume = %state.mount_prefix(),
                        error = %e,
                        failing_for_secs = since.elapsed().as_secs(),
                        "journal reads keep failing; marking volume Offline (its index is no longer current)"
                    );
                    *state.phase.write() = VolumePhase::Offline;
                    degraded = true;
                } else {
                    tracing::warn!(volume = %state.mount_prefix(), error = %e, "journal read failed; retrying");
                }
                // Growing backoff, so a permanently sick volume costs one line
                // per RETRY_BACKOFF_MAX instead of one every 500 ms forever.
                // Interruptible: a stop arriving mid-backoff must not wait out
                // up to 30 s, or teardown would mistake this tail for wedged.
                sleep_unless_stopped(stop, backoff);
                backoff = (backoff * 2).min(RETRY_BACKOFF_MAX);
            }
        }
    }
}

/// Compacts the name arenas, blocking queries only for the install.
///
/// The expensive half copies every live name into fresh arenas (hundreds of MB
/// on a large volume, measured at ~184 ms), and it used to run inline in
/// `apply_batch` under the write lock, so a single journal batch froze every
/// query for that long.
///
/// The plan is only valid while the index does not change, which is safe here
/// for a specific reason: a volume has exactly one writer, this tail thread. An
/// upgradable read lets queries keep reading throughout while excluding any
/// other writer, and the upgrade to exclusive covers only the column writes.
fn compact_index(state: &VolumeState) {
    let started = Instant::now();
    let guard = state.index.upgradable_read();
    let Some(plan) = guard.plan_compaction() else {
        return; // raced with another compaction, or no longer worth it
    };
    let planned = started.elapsed();

    let install_started = Instant::now();
    let mut index = parking_lot::RwLockUpgradableReadGuard::upgrade(guard);
    index.apply_compaction(plan);
    drop(index);

    // A ~184 ms query stall used to be completely invisible. `blocked_ms` is the
    // only part that stops queries; `planned_ms` runs alongside them.
    tracing::debug!(
        volume = %state.mount_prefix(),
        planned_ms = planned.as_millis() as u64,
        blocked_ms = install_started.elapsed().as_millis() as u64,
        "compacted name arenas"
    );
}

/// Reconciles the hard-link set of each FRN in `frns` to what the filesystem
/// actually reports.
///
/// A `HARD_LINK_CHANGE` record names one link and may name a dead one, so the
/// index cannot learn the real set from the journal. Without this a rename of a
/// file with several hard links was dropped entirely: the old name kept matching
/// and the new one never appeared.
///
/// Mirrors [`enrich_stats`]'s lock discipline, and for the same reason: the
/// Win32 walk is a blocking file open and must never run under the index write
/// lock that every query contends on. Only resolving the links' folders and
/// the apply take the lock, see [`apply_link_walks`].
///
/// An FRN whose walk fails is SKIPPED, never reconciled to an empty set:
/// `reconcile_links` treats empty as a delete, so a failed walk would
/// otherwise erase a live file from the index.
fn reconcile_links(
    state: &VolumeState,
    handle: &VolumeHandle,
    frns: &[Frn],
    stop: &AtomicBool,
    pulse: &TailPulse,
) {
    if frns.is_empty() {
        return;
    }
    let mut dropped = 0u64;
    // Reason bits accumulate over an open-close window, so one hard-link
    // change is listed once per record of its window: walk each file once.
    let walks = walk_each_once(frns, |frn| {
        // Teardown must not wait behind a long walk batch: each walk is a
        // blocking open, and dozens on a slow disk outlast the teardown
        // deadline. The daemon is exiting; the results would never be served.
        if stop.load(Ordering::Relaxed) {
            return ControlFlow::Break(());
        }
        // Per-file progress IS liveness: without the stamp, a long batch on
        // a slow disk reads as a stale beat, which delays the supervisor's
        // budget refund and inches toward a spurious stall verdict.
        pulse.beat();
        ControlFlow::Continue(match goz_winfs::link_paths(handle, frn.0) {
            Ok(Some(l)) if !l.is_empty() => Some(l),
            // Gone: its delete record removes it. (A walk always holds the
            // name it started from, so an empty set never reaches here.)
            Ok(_) => None,
            Err(e) => {
                tracing::debug!(volume = %state.mount_prefix(), frn = frn.0, error = %e, "link walk failed");
                dropped += 1;
                None
            }
        })
    });
    let ControlFlow::Continue(walks) = walks else {
        return;
    };
    let walked = in_batch_order(frns, &walks);

    if walked.is_empty() && dropped == 0 {
        return;
    }
    let mut index = state.index.write();
    dropped += apply_link_walks(&mut index, &walked);
    if dropped > 0 {
        // Not silent: a reconcile we could not perform means names on this
        // volume may be stale until the next rescan, and `--status` says so.
        index.note_link_reconciles_dropped(dropped);
    }
}

/// A file's hard links, as volume-relative UTF-16 paths.
type LinkPaths = Vec<Vec<u16>>;

/// Calls `walk` once for each distinct FRN in `frns`, in the order first
/// seen, and keeps what each walk found: its links, or `None` when the file
/// is gone or the walk failed. Stops at the first `Break`.
fn walk_each_once(
    frns: &[Frn],
    mut walk: impl FnMut(Frn) -> ControlFlow<(), Option<LinkPaths>>,
) -> ControlFlow<(), FxHashMap<Frn, Option<LinkPaths>>> {
    let mut walks = FxHashMap::default();
    for &frn in frns {
        if let Entry::Vacant(slot) = walks.entry(frn) {
            slot.insert(walk(frn)?);
        }
    }
    ControlFlow::Continue(walks)
}

/// The links found for `frns`, in the batch's order with its repeats, leaving
/// out files that are gone or whose walk failed. A repeat resolves its folders
/// again after the reconciles before it, see [`apply_link_walks`].
fn in_batch_order<'a>(
    frns: &[Frn],
    walks: &'a FxHashMap<Frn, Option<LinkPaths>>,
) -> Vec<(Frn, &'a [Vec<u16>])> {
    frns.iter()
        .filter_map(|&frn| Some((frn, walks.get(&frn)?.as_deref()?)))
        .collect()
}

/// Reconciles each walked FRN to its link set, resolving the links' folders
/// by path just before that FRN's own reconcile. One reconcile can reclaim a
/// record's previous tenant, and a folder resolved before it ran could be that
/// tenant, freed and handed to another entry by the time a later reconcile
/// used it; an FRN listed again after such a reconcile resolves its folders
/// anew, which puts its links back in the folder that is really there.
/// Returns how many FRNs were left unreconciled because a folder is not
/// indexed, each counted once: reconciling to a partial set would delete the
/// links it could not resolve.
fn apply_link_walks(index: &mut VolumeIndex, walked: &[(Frn, &[Vec<u16>])]) -> u64 {
    let mut unreconciled = FxHashSet::default();
    'walks: for &(frn, links) in walked {
        let mut targets = Vec::with_capacity(links.len());
        for wide in links {
            let mut bytes = Vec::new();
            let lossy = !goz_core::wtf8::from_utf16(wide, &mut bytes);
            let (dir, name) = split_parent(&bytes);
            let Some(parent) = resolve_scope(index, dir, false) else {
                unreconciled.insert(frn);
                continue 'walks;
            };
            targets.push(LinkTarget {
                parent,
                name: WtfName::new(name.to_vec(), lossy),
            });
        }
        if targets.is_empty() {
            unreconciled.insert(frn);
            continue;
        }
        index.reconcile_links(frn, &targets);
    }
    unreconciled.len() as u64
}

/// Splits a volume-relative path into (parent dir, file name) at the last
/// separator, so a link path becomes something `resolve_scope` can look up. A
/// name at the volume root yields an empty dir, which resolves to the root
/// entry.
fn split_parent(path: &[u8]) -> (&[u8], &[u8]) {
    match path.iter().rposition(|&b| b == b'\\' || b == b'/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => (&[], path),
    }
}

/// Refreshes size/mtime for `frns` (a file created or modified after bootstrap
/// has no size/date in the USN record; without this it stays "unknown", which
/// the CSV output renders as a directory). Stats every file with the write lock
/// RELEASED, since `OpenFileById` must never run under it, then applies the whole
/// batch under one short lock.
fn enrich_stats(
    state: &VolumeState,
    handle: &VolumeHandle,
    frns: &[u64],
    stop: &AtomicBool,
    pulse: &TailPulse,
) {
    if frns.is_empty() {
        return;
    }
    let mut stats: Vec<(Frn, u64, i64)> = Vec::with_capacity(frns.len());
    for &frn in frns {
        // Same teardown escape as `reconcile_links`: a large dirty batch of
        // blocking opens must not make a healthy tail look wedged.
        if stop.load(Ordering::Relaxed) {
            break; // apply what was already gathered
        }
        // Per-file liveness stamp, same reason as `reconcile_links`.
        pulse.beat();
        match stat_file(handle, frn) {
            Ok(Some(s)) => stats.push((Frn(frn), s.size, s.mtime_ft)),
            Ok(None) => {} // gone or locked: nothing to refresh
            Err(e) => {
                tracing::debug!(volume = %state.mount_prefix(), frn, error = %e, "live stat failed")
            }
        }
    }
    if stats.is_empty() {
        return;
    }
    let mut index = state.index.write();
    for (frn, size, mtime) in stats {
        index.set_stat(frn, size, mtime);
    }
}

/// Rescan with a storm guard: rebuilds the index, capping consecutive attempts
/// so a persistently broken journal cannot loop forever. Returns the fresh
/// cursor to resume from, or `None` to stop the tail (rebuild failed, the volume
/// refused a new journal, or the cap was hit; the volume is left Failed in the
/// first and last cases and a Snapshot in the second).
fn bounded_rescan(
    state: &VolumeState,
    handle: &VolumeHandle,
    pulse: &TailPulse,
    reason: RescanReason,
    detail: &str,
    rescans: &mut u32,
    stop: &AtomicBool,
) -> Option<JournalInfo> {
    *rescans += 1;
    if *rescans > MAX_RESCANS {
        mark_failed(
            state,
            reason,
            &format!("{detail}: too many rescans, giving up"),
        );
        return None;
    }
    rescan(state, handle, pulse, reason, stop)
}

/// Builds the index of a volume that has none yet and returns the cursor to
/// tail from. The volume reports `Bootstrapping` until the index is published,
/// also when a failed first build is tried again.
///
/// Unlike [`rescan`], it does not wait for a journal it cannot read yet: an
/// index flagged Offline answers more than an empty one, so it is built and
/// published at once, and [`rescan`] then waits for the journal and rebuilds
/// with a cursor, as the daemon did when it built every volume before serving.
/// A volume that refuses a journal is left a Snapshot and the tail stops.
fn first_build(
    state: &VolumeState,
    handle: &VolumeHandle,
    pulse: &TailPulse,
    stop: &AtomicBool,
) -> Option<JournalInfo> {
    // Before the build, for the reason `rescan` gives.
    let journal = bootstrap::ensure_journal(handle, &state.guid);
    let built = {
        let _busy = pulse.busy_scope();
        *state.phase.write() = VolumePhase::Bootstrapping;
        tracing::info!(volume = %state.mount_prefix(), "indexing volume");
        match bootstrap::build_index(handle, Some(stop)) {
            Ok(Some(built)) => built,
            Ok(None) => return None,
            Err(e) => {
                mark_failed(
                    state,
                    RescanReason::NoCursor,
                    &format!("first build failed: {e:#}"),
                );
                return None;
            }
        }
    };
    let cursor = install(state, built, journal);
    // mimalloc heaps are per thread, and this one lives on as the tail, so
    // return the build's freed transients (the 1 MiB buffer, the sparse FRN
    // map swapped for dense, the pre-shrink slack) now.
    // SAFETY: mi_collect takes no pointers and is safe from any thread.
    unsafe { libmimalloc_sys::mi_collect(true) };

    match journal {
        JournalSetup::Active(_) => tracing::info!(
            volume = %state.mount_prefix(),
            entries = state.index.read().len(),
            "volume indexed; tailing journal"
        ),
        // Snapshot, not Offline: nothing is wrong and nothing will change, so
        // the volume must not flag every result incomplete. The tail stops,
        // and the supervisor sees the phase and does not revive it.
        JournalSetup::Refused => tracing::info!(
            volume = %state.mount_prefix(),
            "no USN journal; volume is indexed but its index is a snapshot and cannot track changes"
        ),
        // Offline: changes made during and after the build may be missing.
        JournalSetup::Unavailable => {
            tracing::warn!(
                volume = %state.mount_prefix(),
                "USN journal unavailable; the index is served flagged incomplete and rebuilt once the journal can be read"
            );
            return rescan(state, handle, pulse, RescanReason::NoCursor, stop);
        }
    }
    cursor
}

/// Rebuilds this volume's index in place after a journal loss (wrap, deletion,
/// or parse anomaly). Re-enumerates the MFT on the handle the tail already owns,
/// swaps the fresh index in under the write lock, invalidates the scope cache
/// (its `EntryIdx`s belonged to the old index), and re-acquires a journal
/// cursor. Queries keep serving (flagged incomplete) during the rebuild. A
/// volume that refuses a new journal is left a Snapshot, as at a first build,
/// and the tail stops.
fn rescan(
    state: &VolumeState,
    handle: &VolumeHandle,
    pulse: &TailPulse,
    reason: RescanReason,
    stop: &AtomicBool,
) -> Option<JournalInfo> {
    // Capture the cursor BEFORE enumerating, exactly as a first build does. A
    // change landing after its MFT region was enumerated but before
    // a late cursor capture would be in neither the snapshot nor the replay,
    // and nothing would ever notice; captured first, such changes replay
    // from the journal. The other race is safe: a journal deleted or
    // recreated DURING the rebuild invalidates this cursor, the first read
    // from it fails, and the ordinary error arms trigger another rescan. A
    // stale cursor is loud, a late cursor is silent.
    let journal = wait_for_journal(
        state,
        || bootstrap::ensure_journal(handle, &state.guid),
        pulse,
        stop,
    )?;

    tracing::warn!(volume = %state.mount_prefix(), ?reason, "rebuilding index");
    // The rebuild re-enumerates the whole MFT without stamping the pulse.
    // `Busy` tells the supervisor this silence is legitimate; the guard
    // restores a fresh beat on every way out of this function.
    let _busy = pulse.busy_scope();
    *state.phase.write() = VolumePhase::Rescanning;

    // Pass the stop flag so a shutdown arriving mid-rebuild aborts promptly
    // instead of blocking `TailHandle::shutdown`'s join for the full MFT scan.
    let built = match bootstrap::build_index(handle, Some(stop)) {
        Ok(Some(built)) => built,
        // Aborted by shutdown: leave the old index in place and stop cleanly;
        // this is NOT a failure, so do not mark the volume Failed.
        Ok(None) => return None,
        Err(e) => {
            mark_failed(state, reason, &format!("rescan rebuild failed: {e:#}"));
            return None;
        }
    };

    let cursor = install(state, built, journal);

    match journal {
        JournalSetup::Active(_) => tracing::info!(
            volume = %state.mount_prefix(),
            entries = state.index.read().len(),
            "rescan complete; tailing resumed"
        ),
        // Snapshot, not Offline, as at bootstrap: the index is complete as of
        // now and no journal will ever update it. The tail stops, and the
        // supervisor sees the Snapshot phase and does not revive it. Offline
        // flagged every query incomplete for good once its revivals, each a
        // full rebuild refused again, ran out.
        JournalSetup::Refused => tracing::info!(
            volume = %state.mount_prefix(),
            "the volume refused a new USN journal; its index is a snapshot and cannot track changes"
        ),
        // `wait_for_journal` waits this out, so it is not reached. If it ever
        // is, the volume is Offline and the supervisor revives the tail.
        JournalSetup::Unavailable => tracing::warn!(
            volume = %state.mount_prefix(),
            "rescan rebuilt index but no journal is available; live updates paused"
        ),
    }
    cursor
}

/// Asks for the volume's journal until it is usable or refused.
///
/// A journal that cannot be read yet (a deletion still running, which can take
/// minutes and continues across restarts, or a disk still waking) would only
/// give a rebuild with no cursor: the volume would go Offline, the tail would
/// retire, and each revival would read the whole MFT again until the
/// supervisor gave up for good. Waiting here means one rebuild, with a cursor,
/// once the journal answers. Meanwhile the volume reports Offline and the
/// pulse beats, so the supervisor sees a live tail rather than a wedged one.
/// `None` when `stop` flips first.
fn wait_for_journal(
    state: &VolumeState,
    mut ask: impl FnMut() -> JournalSetup,
    pulse: &TailPulse,
    stop: &AtomicBool,
) -> Option<JournalSetup> {
    let mut wait = RETRY_BACKOFF_MIN;
    let mut waited = false;
    loop {
        let journal = ask();
        if journal != JournalSetup::Unavailable {
            if waited {
                tracing::info!(volume = %state.mount_prefix(), "USN journal readable again");
            }
            return Some(journal);
        }
        if !waited {
            tracing::warn!(
                volume = %state.mount_prefix(),
                "USN journal unavailable; waiting for it before rebuilding the index"
            );
            *state.phase.write() = VolumePhase::Offline;
            waited = true;
        }
        // Beat at least every RETRY_BACKOFF_MAX, well inside the
        // supervisor's stall window, however long the wait between asks.
        let mut left = wait;
        while !left.is_zero() {
            pulse.beat();
            let step = left.min(RETRY_BACKOFF_MAX);
            sleep_unless_stopped(stop, step);
            if stop.load(Ordering::Relaxed) {
                return None;
            }
            left -= step;
        }
        wait = (wait * 2).min(JOURNAL_WAIT_MAX);
    }
}

/// Publishes a built index with the flags that describe it, under the index
/// write lock, then the phase its journal gives it. Returns the cursor to tail
/// from, `None` when there is no journal to tail.
fn install(
    state: &VolumeState,
    built: bootstrap::BuiltIndex,
    journal: JournalSetup,
) -> Option<JournalInfo> {
    {
        let mut index = state.index.write();
        *index = built.index;
        *state.metadata_pending.write() = built.metadata_pending;
        *state.journal_max_bytes.write() = journal.cursor().map(|c| c.maximum_size);
        state.indexed.store(true, Ordering::Release);
        // Invalidate the scope cache while the index write lock is held: its
        // EntryIdx values belong to the old generation. Clearing it here
        // (rather than after the lock drops) guarantees a reader never sees
        // the new, possibly smaller index alongside a stale EntryIdx, which
        // would index out of bounds in VolumeIndex::path_of during scope
        // validation.
        *state.scope_cache.lock() = None;
    }
    *state.phase.write() = journal.built_phase();
    journal.cursor()
}

/// Sleeps up to `total`, waking early if `stop` flips. Plain `sleep` here
/// would hold a stopping tail hostage to its own backoff (up to 30 s), which
/// teardown's cancel-and-poll rescue would misread as a wedged thread.
fn sleep_unless_stopped(stop: &AtomicBool, total: Duration) {
    let deadline = Instant::now() + total;
    while !stop.load(Ordering::Relaxed) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        std::thread::sleep(left.min(Duration::from_millis(50)));
    }
}

fn mark_failed(state: &VolumeState, reason: RescanReason, detail: &str) {
    // The tail is about to exit for good. Without this line the volume died
    // silently: nothing else announces the exit, and the supervisor's later
    // revival attempts were the first evidence anything happened.
    tracing::error!(
        volume = %state.mount_prefix(),
        ?reason,
        detail,
        "volume failed; its tail is stopping and live updates have ended"
    );
    *state.phase.write() = VolumePhase::Failed {
        reason: format!("{reason:?}: {detail}"),
    };
}

#[cfg(test)]
mod tests {
    use super::{
        DEGRADE_AFTER, ESCALATE_AFTER, JOURNAL_WAIT_MAX, REASON_MASK, RETRY_BACKOFF_MAX,
        RETRY_BACKOFF_MIN, apply_link_walks, in_batch_order, install, wait_for_journal,
        walk_each_once,
    };
    use crate::bootstrap::{BuiltIndex, JournalSetup};
    use crate::pulse::TailPulse;
    use crate::volume_state::{ScopeCache, VolumeState};
    use goz_core::index::{FrnMap, NTFS_ROOT_FRN, VolumeIndex};
    use goz_core::types::Frn;
    use goz_core::types::VolumePhase;
    use goz_core::usn::ops_for;
    use goz_core::usn::record::ParsedUsnRecord;
    use parking_lot::{Mutex, RwLock};
    use std::ops::ControlFlow;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    fn record_with(reason: u32) -> ParsedUsnRecord {
        ParsedUsnRecord {
            major_version: 2,
            frn: Frn(42),
            parent_frn: Frn(5),
            usn: 0,
            timestamp_ft: 0,
            reason,
            attributes: 0,
            name: b"f.txt".to_vec(),
            name_lossy: false,
        }
    }

    /// `REASON_MASK` is what we ask the kernel to report; `ops_for` (over in
    /// goz-core) decides what an arriving record does to the index. Nothing
    /// links the two, and they live in different crates.
    ///
    /// If a bit ever drives an op but is absent from the mask, the kernel never
    /// reports it, `apply_batch` never sees it, and that class of update is
    /// dropped forever. No crash, no log, no other failing test. So sweep the
    /// whole 32-bit reason space rather than a hand-listed set of constants: a
    /// bit added to `ops_for` later is caught even if nobody updates this test.
    #[test]
    fn reason_mask_covers_every_bit_that_drives_an_index_op() {
        for pos in 0..32 {
            let bit = 1u32 << pos;
            let ops = ops_for(&record_with(bit));
            if !ops.is_empty() {
                assert_ne!(
                    REASON_MASK & bit,
                    0,
                    "reason bit {bit:#010x} yields {ops:?}, but REASON_MASK omits it: \
                     the kernel would never report it and the update would vanish silently"
                );
            }
        }
    }

    /// The read-retry policy exists to stop the one state the design forbids: a
    /// volume whose journal reads keep failing while it still reports `Live`, so
    /// `--status`, `Hello.ready`, and every result page call the index current
    /// when it has stopped tracking the volume entirely.
    ///
    /// The loop itself needs a real `VolumeHandle` and cannot be unit-tested, so
    /// this pins the constants that bound it. The behaviour they drive is
    /// asserted by the gated Windows test that stops a volume mid-tail.
    #[test]
    fn retry_policy_is_bounded_and_degrades() {
        // Must tolerate a transient failure: a drive spinning up answers
        // ERROR_NOT_READY for a moment and must not flip the volume Offline.
        assert!(
            DEGRADE_AFTER >= Duration::from_secs(1),
            "degrading on the first failure would flap on every spin-up"
        );
        // Must not be so patient that a dead volume looks healthy for minutes.
        assert!(
            DEGRADE_AFTER <= Duration::from_secs(30),
            "a volume that stopped tracking must be reported promptly"
        );
        // Backoff must actually grow, or a permanently sick volume writes a log
        // line every RETRY_BACKOFF_MIN forever (this is what produced ~96k
        // identical lines in one run).
        assert!(RETRY_BACKOFF_MAX > RETRY_BACKOFF_MIN);
        // The first retry must be prompt enough that a blip costs no freshness.
        assert!(RETRY_BACKOFF_MIN <= Duration::from_secs(1));
        // Escalation must announce Offline first (honesty precedes surgery)
        // and outlast a spin-up's NOT_READY window (~15 s worst observed), or
        // every sleepy drive would eat a full MFT rebuild on wake.
        assert!(ESCALATE_AFTER > DEGRADE_AFTER);
        assert!(ESCALATE_AFTER >= Duration::from_secs(20));
        // But not so patient that an unmappable read error (the observed
        // ERROR_INVALID_PARAMETER zombie) parks a volume for minutes.
        assert!(ESCALATE_AFTER <= Duration::from_secs(120));
        // A journal wait must ask again well inside an hour, and its beats
        // (every RETRY_BACKOFF_MAX) must stay inside the supervisor's stall
        // window while it waits that long between asks.
        assert!(JOURNAL_WAIT_MAX >= RETRY_BACKOFF_MAX);
        assert!(JOURNAL_WAIT_MAX <= Duration::from_secs(15 * 60));
    }

    fn built(metadata_pending: bool) -> BuiltIndex {
        BuiltIndex {
            index: VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse()),
            layout_secs: 0.0,
            enum_secs: None,
            metadata_pending,
        }
    }

    fn volume(phase: VolumePhase) -> VolumeState {
        VolumeState {
            guid: r"\\?\Volume{00000000-0000-0000-0000-000000000000}\".to_string(),
            mounts: vec![r"C:\".to_string()],
            index: RwLock::new(VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse())),
            phase: RwLock::new(phase),
            metadata_pending: RwLock::new(true),
            journal_max_bytes: RwLock::new(None),
            indexed: AtomicBool::new(false),
            scope_cache: Mutex::new(Some(ScopeCache {
                rel: b"windows".to_vec(),
                entry: 1,
            })),
        }
    }

    fn journal() -> goz_winfs::JournalInfo {
        goz_winfs::JournalInfo {
            journal_id: 7,
            first_usn: 0,
            next_usn: 4096,
            lowest_valid_usn: 0,
            max_usn: i64::MAX,
            maximum_size: 64 << 20,
        }
    }

    /// A rescan waits out a journal it cannot read yet and then rebuilds once,
    /// with its cursor, instead of rebuilding without one and retiring. The
    /// volume reports Offline while it waits.
    #[test]
    fn a_rescan_waits_for_an_unavailable_journal() {
        let state = volume(VolumePhase::Live);
        let mut answers =
            vec![JournalSetup::Unavailable, JournalSetup::Active(journal())].into_iter();
        let mut asks = 0;
        let got = wait_for_journal(
            &state,
            || {
                asks += 1;
                assert_eq!(
                    *state.phase.read(),
                    if asks == 1 {
                        VolumePhase::Live
                    } else {
                        VolumePhase::Offline
                    }
                );
                answers.next().expect("one ask too many")
            },
            &TailPulse::new(),
            &AtomicBool::new(false),
        );
        assert_eq!(got, Some(JournalSetup::Active(journal())));
        assert_eq!(asks, 2);
    }

    /// A usable or refused journal ends the wait at once and leaves the phase
    /// alone, and a stop ends it without an answer.
    #[test]
    fn a_journal_wait_ends_on_an_answer_or_a_stop() {
        for answer in [JournalSetup::Active(journal()), JournalSetup::Refused] {
            let state = volume(VolumePhase::Rescanning);
            let got = wait_for_journal(
                &state,
                || answer,
                &TailPulse::new(),
                &AtomicBool::new(false),
            );
            assert_eq!(got, Some(answer));
            assert_eq!(*state.phase.read(), VolumePhase::Rescanning);
        }
        let state = volume(VolumePhase::Live);
        let stop = AtomicBool::new(true);
        let got = wait_for_journal(
            &state,
            || JournalSetup::Unavailable,
            &TailPulse::new(),
            &stop,
        );
        assert_eq!(got, None);
    }

    /// A file or folder created under `parent`, as the journal reports it.
    fn created(frn: Frn, parent: Frn, name: &str, is_dir: bool) -> ParsedUsnRecord {
        ParsedUsnRecord {
            frn,
            parent_frn: parent,
            reason: goz_core::usn::record::USN_REASON_FILE_CREATE,
            attributes: if is_dir {
                goz_core::usn::record::FILE_ATTRIBUTE_DIRECTORY
            } else {
                0
            },
            name: name.as_bytes().to_vec(),
            ..record_with(0)
        }
    }

    /// A reconcile can reclaim a record's previous tenant, which may be a
    /// folder another walk in the same batch names by path: here a folder `d`
    /// whose delete was missed, beside the `d` that replaced it. Each FRN's
    /// folders are resolved just before its own reconcile, so the later file
    /// lands in the `d` that is there, not in whatever took the freed slot.
    #[test]
    fn a_link_walk_resolves_its_folders_after_the_reconciles_before_it() {
        let root = NTFS_ROOT_FRN;
        let frn = |record: u64, seq: u64| Frn((seq << 48) | record);
        let (d_old, d_new) = (frn(100, 3), frn(300, 1));
        let (f1, f2) = (frn(100, 4), frn(400, 1));
        let mut index = VolumeIndex::new(root, FrnMap::sparse());
        index.apply_batch(&[
            created(d_old, root, "d", true),
            created(d_new, root, "d", true),
            created(f2, d_new, "x.txt", false),
        ]);
        let wide = |p: &str| p.encode_utf16().collect::<Vec<u16>>();

        let (f1_links, f2_links) = (vec![wide(r"\f1.txt")], vec![wide(r"\d\x.txt")]);
        let dropped = apply_link_walks(
            &mut index,
            &[(f1, f1_links.as_slice()), (f2, f2_links.as_slice())],
        );

        assert_eq!(dropped, 0);
        let f2_entry = index.entry(index.head_of(f2).expect("f2 is indexed"));
        assert_eq!(index.entry(f2_entry.parent()).frn(), d_new);
        let mut path = Vec::new();
        index.path_of(index.head_of(f2).unwrap(), &mut path);
        assert_eq!(path, br"d\x.txt");
    }

    /// One hard-link change is listed once per record of its open-close
    /// window. Each file is walked once, in the order first seen, and a stop
    /// ends the walks.
    #[test]
    fn each_file_is_walked_once() {
        let (a, b) = (Frn(400 | 1 << 48), Frn(401 | 1 << 48));
        let mut asked = Vec::new();
        let walks = walk_each_once(&[a, b, a, b, a], |frn| {
            asked.push(frn);
            ControlFlow::Continue(None)
        });
        assert_eq!(asked, [a, b]);
        assert!(matches!(walks, ControlFlow::Continue(w) if w.len() == 2));

        let mut asked = Vec::new();
        let walks = walk_each_once(&[a, b, a], |frn| {
            asked.push(frn);
            ControlFlow::Break(())
        });
        assert_eq!(asked, [a]);
        assert!(walks.is_break());
    }

    /// The walked links keep the batch's order and repeats, without the files
    /// that are gone.
    #[test]
    fn walked_links_keep_the_batch_order() {
        let (a, b, c) = (Frn(400 | 1 << 48), Frn(401 | 1 << 48), Frn(402 | 1 << 48));
        let a_links = vec![vec![b'a' as u16]];
        let c_links = vec![vec![b'c' as u16]];
        let walks = rustc_hash::FxHashMap::from_iter([
            (a, Some(a_links.clone())),
            (b, None),
            (c, Some(c_links.clone())),
        ]);
        assert_eq!(
            in_batch_order(&[a, b, c, a], &walks),
            [
                (a, a_links.as_slice()),
                (c, c_links.as_slice()),
                (a, a_links.as_slice())
            ]
        );
    }

    /// A file listed again after another file's reconcile resolves its folders
    /// anew. Here the first pass puts the file's links in a stale `d` whose
    /// record the other file then takes over, which sends them to lost+found,
    /// and only the second pass puts them back in the `d` that is there.
    #[test]
    fn a_file_listed_again_is_applied_again() {
        let root = NTFS_ROOT_FRN;
        let frn = |record: u64, seq: u64| Frn((seq << 48) | record);
        let (d_old, d_new) = (frn(100, 3), frn(300, 1));
        let (taker, file) = (frn(100, 4), frn(400, 1));
        let mut index = VolumeIndex::new(root, FrnMap::sparse());
        index.apply_batch(&[
            created(d_old, root, "d", true),
            created(d_new, root, "d", true),
            created(file, d_new, "x.txt", false),
        ]);
        let wide = |p: &str| p.encode_utf16().collect::<Vec<u16>>();
        let file_links = vec![wide(r"\d\x.txt"), wide(r"\d\y.txt")];
        let taker_links = vec![wide(r"\f1.txt")];

        let dropped = apply_link_walks(
            &mut index,
            &[
                (file, file_links.as_slice()),
                (taker, taker_links.as_slice()),
                (file, file_links.as_slice()),
            ],
        );

        assert_eq!(dropped, 0);
        let mut link = index.head_of(file).expect("the file is indexed");
        let mut paths = Vec::new();
        while link != goz_core::types::NIL {
            let mut path = Vec::new();
            index.path_of(link, &mut path);
            paths.push(String::from_utf8(path).unwrap());
            link = index.entry(link).next_link();
        }
        paths.sort();
        assert_eq!(paths, [r"d\x.txt", r"d\y.txt"]);
    }

    /// A file whose links name a folder the index lacks is counted once as
    /// not reconciled, however often the batch lists it.
    #[test]
    fn an_unreconciled_file_is_counted_once() {
        let file = Frn(400 | 1 << 48);
        let mut index = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
        let links: Vec<Vec<u16>> = vec![r"\missing\x.txt".encode_utf16().collect()];
        let dropped = apply_link_walks(
            &mut index,
            &[(file, links.as_slice()), (file, links.as_slice())],
        );
        assert_eq!(dropped, 1);
    }

    /// A rescan publishes the flags of the index it built, not the ones its
    /// volume had at bootstrap.
    #[test]
    fn a_rebuilt_index_brings_its_own_flags() {
        let state = volume(VolumePhase::Rescanning);
        assert!(!state.indexed.load(Ordering::Acquire));
        install(&state, built(false), JournalSetup::Active(journal()));
        assert!(!*state.metadata_pending.read());
        assert_eq!(*state.journal_max_bytes.read(), Some(64 << 20));
        assert!(state.scope_cache.lock().is_none());
        assert!(
            state.indexed.load(Ordering::Acquire),
            "a later tail must rebuild this index, not build it as new"
        );

        install(&state, built(true), JournalSetup::Refused);
        assert!(*state.metadata_pending.read());
        assert_eq!(*state.journal_max_bytes.read(), None);
    }

    /// A rescan the volume refuses a new journal for leaves a Snapshot, as
    /// bootstrap does, so its stopped tail flags no query incomplete and the
    /// supervisor does not revive it. A journal goes Live with its cursor.
    #[test]
    fn a_rescan_refused_a_journal_leaves_a_snapshot() {
        let state = volume(VolumePhase::Rescanning);
        assert_eq!(install(&state, built(false), JournalSetup::Refused), None);
        assert_eq!(*state.phase.read(), VolumePhase::Snapshot);
        assert!(state.phase.read().is_complete());

        let state = volume(VolumePhase::Rescanning);
        assert_eq!(
            install(&state, built(false), JournalSetup::Unavailable),
            None
        );
        assert_eq!(*state.phase.read(), VolumePhase::Offline);

        let state = volume(VolumePhase::Rescanning);
        assert_eq!(
            install(&state, built(false), JournalSetup::Active(journal())),
            Some(journal())
        );
        assert_eq!(*state.phase.read(), VolumePhase::Live);
    }

    /// The converse direction is deliberately NOT asserted: the mask may name
    /// bits that yield no op on their own (`CLOSE` is exactly that, it only
    /// tells us a window ended), so requesting a superset is correct. This pins
    /// that the mask stays a superset and never becomes an equality by accident.
    #[test]
    fn close_is_requested_even_though_it_drives_no_op_alone() {
        use goz_core::usn::record::USN_REASON_CLOSE;
        assert!(ops_for(&record_with(USN_REASON_CLOSE)).is_empty());
        assert_ne!(REASON_MASK & USN_REASON_CLOSE, 0);
    }
}
