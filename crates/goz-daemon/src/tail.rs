//! Per-volume USN journal tail: one dedicated OS thread that reads the change
//! journal from the bootstrap cursor forward and applies each batch to the
//! shared index, keeping it live.
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
//! cannot be resolved is skipped, never applied as an empty set (which would
//! delete a live file), and counted as `IndexStats::link_reconciles_dropped` so
//! `--status` shows it rather than hiding it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use goz_core::index::{LinkTarget, WtfName};
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

use crate::bootstrap;
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
    /// retired. The supervisor's stall and retirement decisions read only
    /// this, never `state.phase`, which is user-facing honesty state; the
    /// phase is consulted for exactly one thing, gating the revival budget
    /// refund (see the supervisor's `accrue_health`).
    pub(crate) pulse: Arc<TailPulse>,
    /// Shared volume state, kept so the supervisor can revive a retired tail
    /// (reopen by GUID, respawn) and correct the phase when it gives up.
    pub(crate) state: Arc<VolumeState>,
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
/// (it is not shared). `cursor: None` means the caller has no trustworthy
/// starting point (a supervisor reviving a tail whose predecessor exited, so
/// the volume went untailed for a while) and the thread's first act is a full
/// rescan, which rebuilds the index and acquires a fresh cursor.
pub(crate) fn spawn(
    state: Arc<VolumeState>,
    handle: VolumeHandle,
    cursor: Option<JournalInfo>,
) -> std::io::Result<TailHandle> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let pulse = Arc::new(TailPulse::new());
    let pulse_thread = pulse.clone();
    let mount = state.mount_prefix().to_string();
    let state_thread = state.clone();
    // The supervisor needs a REAL handle to this thread, and duplicating one
    // must happen ON the thread (the pseudo-handle is per-thread), so the
    // thread sends it back before entering the loop.
    let (io_tx, io_rx) = std::sync::mpsc::sync_channel(1);
    // OS thread creation can fail on a machine near its thread/handle limit.
    // Propagate it so the caller can degrade this one volume to Offline rather
    // than panic the whole engine bootstrap.
    let join = std::thread::Builder::new()
        .name(format!("goz-tail-{}", state.mount_prefix()))
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
            tail_loop(&state_thread, handle, cursor, &stop_thread, &pulse_thread)
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
        state,
        io_handle: Arc::new(io_handle),
    })
}

fn tail_loop(
    state: &VolumeState,
    handle: VolumeHandle,
    cursor: Option<JournalInfo>,
    stop: &AtomicBool,
    pulse: &TailPulse,
) {
    // Owned so it drops in THIS frame, on return and on unwind alike, before
    // the spawning closure's retire guard publishes `Retired`. A thread that
    // claimed Retired while still wedged in its handle's CloseHandle would
    // draw an unbounded join from the supervisor's give-up or revive path.
    let handle = &handle;
    let mut rescans: u32 = 0;
    // A revived tail starts with no cursor: changes kept landing while the
    // volume had no tail, so nothing proves the index is in sync. Rebuild it
    // and pick up a fresh cursor before tailing a single record. Deliberately
    // NOT routed through `bounded_rescan`: this rebuild is revival's price,
    // not a journal fault, and charging it would leave every revived
    // incarnation one incident short of the documented budget (the counter
    // only resets on a non-empty batch, so on a quiet volume the charge
    // would never clear). Revival storms are bounded by the supervisor's own
    // budget instead.
    let cursor = match cursor {
        Some(c) => c,
        None => match rescan(state, handle, pulse, RescanReason::TailRevived, stop) {
            Some(c) => c,
            None => return,
        },
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
/// lock that every query contends on. Only the apply takes the lock.
///
/// An FRN whose walk fails is SKIPPED, never reconciled to an empty set:
/// `reconcile_links` treats empty as a delete, so a transient lock would
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
    let mut resolved: Vec<(Frn, Vec<LinkTarget>)> = Vec::new();
    let mut dropped = 0u64;

    for &frn in frns {
        // Teardown must not wait behind a long walk batch: each iteration is
        // a blocking open, and dozens on a slow disk outlast the teardown
        // deadline. The daemon is exiting; the results would never be served.
        if stop.load(Ordering::Relaxed) {
            return;
        }
        // Per-file progress IS liveness: without the stamp, a long batch on
        // a slow disk reads as a stale beat, which delays the supervisor's
        // budget refund and inches toward a spurious stall verdict.
        pulse.beat();
        let links = match goz_winfs::link_paths(handle, frn.0) {
            Ok(Some(l)) if !l.is_empty() => l,
            // Gone, locked, or unwalkable: leave the entry alone. The structural
            // delete, if there was one, already arrived through the journal.
            Ok(_) => continue,
            Err(e) => {
                tracing::debug!(volume = %state.mount_prefix(), frn = frn.0, error = %e, "link walk failed");
                dropped += 1;
                continue;
            }
        };

        // Resolve each link's parent directory under one short read lock.
        let index = state.index.read();
        let mut targets = Vec::with_capacity(links.len());
        let mut unresolved = false;
        for wide in &links {
            let mut bytes = Vec::new();
            let lossy = !goz_core::wtf8::from_utf16(wide, &mut bytes);
            let (dir, name) = split_parent(&bytes);
            match resolve_scope(&index, dir, false) {
                Some(parent) => targets.push(LinkTarget {
                    parent,
                    name: WtfName::new(name.to_vec(), lossy),
                }),
                // A parent we have not indexed yet: reconciling to a partial set
                // would delete the links we could not resolve.
                None => {
                    unresolved = true;
                    break;
                }
            }
        }
        drop(index);

        if unresolved || targets.is_empty() {
            dropped += 1;
            continue;
        }
        resolved.push((frn, targets));
    }

    if dropped > 0 {
        // Not silent: a reconcile we could not perform means names on this
        // volume may be stale until the next rescan, and `--status` says so.
        state.index.write().note_link_reconciles_dropped(dropped);
    }
    if resolved.is_empty() {
        return;
    }
    let mut index = state.index.write();
    for (frn, targets) in &resolved {
        index.reconcile_links(*frn, targets);
    }
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
/// cursor to resume from, or `None` to stop the tail (rebuild failed, no journal
/// after rebuild, or the cap was hit; the volume is left Failed in the first
/// and last cases).
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

/// Rebuilds this volume's index in place after a journal loss (wrap, deletion,
/// or parse anomaly). Re-enumerates the MFT on the handle the tail already owns,
/// swaps the fresh index in under the write lock, invalidates the scope cache
/// (its `EntryIdx`s belonged to the old index), and re-acquires a journal
/// cursor. Queries keep serving (flagged incomplete) during the rebuild.
fn rescan(
    state: &VolumeState,
    handle: &VolumeHandle,
    pulse: &TailPulse,
    reason: RescanReason,
    stop: &AtomicBool,
) -> Option<JournalInfo> {
    tracing::warn!(volume = %state.mount_prefix(), ?reason, "journal lost; rebuilding index");
    // The rebuild re-enumerates the whole MFT without stamping the pulse.
    // `Busy` tells the supervisor this silence is legitimate; the guard
    // restores a fresh beat on every way out of this function.
    let _busy = pulse.busy_scope();
    *state.phase.write() = VolumePhase::Rescanning;

    // Capture the cursor BEFORE enumerating, exactly as first-time bootstrap
    // does. A change landing after its MFT region was enumerated but before
    // a late cursor capture would be in neither the snapshot nor the replay,
    // and nothing would ever notice; captured first, such changes replay
    // from the journal. The other race is safe: a journal deleted or
    // recreated DURING the rebuild invalidates this cursor, the first read
    // from it fails, and the ordinary error arms trigger another rescan. A
    // stale cursor is loud, a late cursor is silent.
    let cursor = bootstrap::ensure_journal(handle);

    // Pass the stop flag so a shutdown arriving mid-rebuild aborts promptly
    // instead of blocking `TailHandle::shutdown`'s join for the full MFT scan.
    let new_index = match bootstrap::build_index(handle, Some(stop)) {
        Ok(Some((index, _enum_secs, _layout_secs))) => index,
        // Aborted by shutdown: leave the old index in place and stop cleanly;
        // this is NOT a failure, so do not mark the volume Failed.
        Ok(None) => return None,
        Err(e) => {
            mark_failed(state, reason, &format!("rescan rebuild failed: {e:#}"));
            return None;
        }
    };

    {
        let mut index = state.index.write();
        *index = new_index;
        // Invalidate the scope cache while the index write lock is held: its
        // EntryIdx values belong to the old generation. Clearing it here (rather
        // than after the lock drops) guarantees a reader never sees the new,
        // possibly smaller index alongside a stale EntryIdx, which would index
        // out of bounds in VolumeIndex::path_of during scope validation.
        *state.scope_cache.lock() = None;
    }

    match cursor {
        Some(c) => {
            *state.phase.write() = VolumePhase::Live;
            tracing::info!(
                volume = %state.mount_prefix(),
                entries = state.index.read().len(),
                "rescan complete; tailing resumed"
            );
            Some(c)
        }
        None => {
            // Rebuilt and correct, but no journal to tail (deletion + recreate
            // failed): fresh as of now but will not receive live updates. Report
            // Offline so Status/query responses flag the volume incomplete
            // instead of falsely Live.
            *state.phase.write() = VolumePhase::Offline;
            tracing::warn!(volume = %state.mount_prefix(), "rescan rebuilt index but no journal is available; live updates paused");
            None
        }
    }
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
    use super::{DEGRADE_AFTER, ESCALATE_AFTER, REASON_MASK, RETRY_BACKOFF_MAX, RETRY_BACKOFF_MIN};
    use goz_core::types::Frn;
    use goz_core::usn::ops_for;
    use goz_core::usn::record::ParsedUsnRecord;
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
