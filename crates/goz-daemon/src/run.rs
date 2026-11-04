//! Daemon orchestration shared by console (`gozd run`) and service
//! (`gozd run --service`) modes: bootstrap every volume, spawn a tail thread per
//! volume with a journal, then serve queries over the pipe until stopped.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use goz_core::types::VolumePhase;
use parking_lot::RwLock;
use tokio::sync::watch;

use crate::bootstrap::JournalSetup;
use crate::supervisor::Supervisor;
use crate::tail::{self, TailHandle};
use crate::volume_state::{VolumeSet, VolumeState};
use crate::{bootstrap, server};

/// A fully bootstrapped index: every volume indexed and (where a journal
/// exists) tailing live. Shared by console and service entry points.
pub(crate) struct Engine {
    pub volumes: VolumeSet,
    /// Owns every tail thread; see [`Supervisor::shutdown`] for teardown.
    pub supervisor: Supervisor,
    pub total_entries: usize,
    pub elapsed: Duration,
}

/// Bootstraps every fixed NTFS volume and spawns the live-tail threads.
///
/// Blocking and potentially slow (tens of seconds on a large disk): callers
/// that must stay responsive to an external supervisor (the SCM) run this on a
/// dedicated thread and report progress while it works.
pub(crate) fn build_engine() -> Result<Engine> {
    let started = Instant::now();
    let scan = bootstrap::scan_all()?;

    let mut states: Vec<Arc<VolumeState>> = Vec::new();
    let mut tails: Vec<TailHandle> = Vec::new();
    let mut total_entries = 0usize;

    // A volume that failed to bootstrap still gets a VolumeState, with an empty
    // index and phase Failed. Every honesty signal iterates the volume set, so a
    // volume that is absent from it cannot be reported as anything: `--status`
    // would list the survivors as live, `Hello.ready` would stay true, and a
    // query scoped to the missing volume would return zero hits with exit 0,
    // indistinguishable from "that file does not exist".
    for fv in scan.failed {
        states.push(Arc::new(VolumeState {
            guid: fv.guid_path,
            mounts: fv.mounts,
            index: RwLock::new(goz_core::index::VolumeIndex::new(
                goz_core::index::NTFS_ROOT_FRN,
                goz_core::index::FrnMap::sparse(),
            )),
            phase: RwLock::new(VolumePhase::Failed { reason: fv.reason }),
            metadata_pending: RwLock::new(false),
            journal_max_bytes: RwLock::new(None),
            scope_cache: parking_lot::Mutex::new(None),
        }));
    }

    for bv in scan.ok {
        total_entries += bv.entries();
        let state = Arc::new(VolumeState {
            guid: bv.guid_path.clone(),
            mounts: bv.mounts.clone(),
            index: RwLock::new(bv.index),
            phase: RwLock::new(bootstrap_phase(&bv.journal)),
            // True only when the FILE_LAYOUT pass stopped and ENUM named the
            // rest without sizes or dates.
            metadata_pending: RwLock::new(bv.metadata_pending),
            journal_max_bytes: RwLock::new(bv.journal.cursor().map(|c| c.maximum_size)),
            scope_cache: parking_lot::Mutex::new(None),
        });

        let cursor = match bv.journal {
            JournalSetup::Active(cursor) => {
                tracing::info!(
                    volume = %state.mount_prefix(),
                    entries = state.index.read().len(),
                    "tailing journal"
                );
                Some(cursor)
            }
            // Offline, not Snapshot: the index was built while nothing could
            // read the journal, so changes made since may be missing. The tail
            // starts without a cursor, which makes it rebuild the index once it
            // can read the journal, and the volume goes Live from there.
            JournalSetup::Unavailable => {
                tracing::warn!(
                    volume = %state.mount_prefix(),
                    "USN journal unavailable; the tail will rebuild the index once it can read the journal"
                );
                None
            }
            JournalSetup::Refused => {
                tracing::info!(
                    volume = %state.mount_prefix(),
                    "no USN journal; volume is indexed but its index is a snapshot and cannot track changes"
                );
                // Snapshot, not Offline: nothing is wrong and nothing will
                // change. Reporting Offline here made `volumes_incomplete` true
                // forever, so every query printed "results may be incomplete"
                // for a volume that was fully indexed. (Recovery partitions
                // used to land here on every machine; that was a broken
                // `FSCTL_CREATE_USN_JOURNAL` code, not the filesystem
                // refusing, and they now get a journal like any other volume.)
                // `bv.handle` drops here (CloseHandle); no tail for this volume.
                states.push(state);
                continue;
            }
        };
        match tail::spawn(state.clone(), bv.handle, cursor) {
            Ok(h) => tails.push(h),
            Err(e) => {
                // Thread creation or its handle duplication failed (resource
                // pressure); either way no tail is running. Degrade just this
                // volume to Offline instead of aborting the whole engine
                // bootstrap; status will flag it incomplete.
                tracing::error!(
                    volume = %state.mount_prefix(),
                    error = %e,
                    "failed to spawn tail thread; volume indexed but will not receive live updates"
                );
                *state.phase.write() = VolumePhase::Offline;
            }
        }
        states.push(state);
    }

    // One supervisor for every tail: cancels journal I/O that a sick device
    // pends forever (observed on a spinning-down HDD), which no in-loop error
    // handling can see, and revives tails that exit outright, on a bounded
    // budget. It takes ownership of the tails; teardown goes through it.
    let supervisor = Supervisor::spawn(tails);

    // Purge THIS thread's allocator heap before handing off. mimalloc heaps
    // are per-thread and the post-publish purge in `settle_working_set` runs
    // on a DIFFERENT thread (the SCM thread in service mode), which once
    // left ~100 MB of dead bootstrap memory committed for the daemon's
    // lifetime. The bootstrap transients (buffers, the sparse FRN maps
    // swapped for dense, pre-shrink slack) are freed on the per-volume
    // bootstrap threads, whose heaps `abandoned_page_purge` returns at thread
    // exit; this covers whatever this thread allocated on the way.
    // SAFETY: mi_collect takes no pointers and is safe from any thread.
    unsafe { libmimalloc_sys::mi_collect(true) };

    Ok(Engine {
        volumes: Arc::new(states),
        supervisor,
        total_entries,
        elapsed: started.elapsed(),
    })
}

/// The phase a bootstrapped volume reports until its tail says otherwise. Only
/// a volume that refused a journal is a Snapshot; one whose journal could not
/// be read yet is not tracking, and its tail rebuilds the index once it can.
fn bootstrap_phase(journal: &JournalSetup) -> VolumePhase {
    match journal {
        JournalSetup::Active(_) => VolumePhase::Live,
        JournalSetup::Unavailable => VolumePhase::Offline,
        JournalSetup::Refused => VolumePhase::Snapshot,
    }
}

/// Best-effort post-bootstrap memory settling, shared by console + service
/// modes. Never fatal: logs and continues.
///
/// Default: purge the allocator and TRIM the working set once, the same
/// one-shot mem_trim Everything performs after indexing. Pages return on
/// first touch as soft faults, so the one-time cost is ~0.2 ms/MB spread over
/// the first queries, and the process's visible memory drops to what queries
/// actually touch instead of the bootstrap peak.
///
/// `GOZ_PIN_WORKING_SET=1` opts into the opposite trade: pin the resident set
/// as a hard minimum so a low-memory burst can never page the index out and
/// no query ever pays a soft-fault storm. For latency-sensitive setups.
pub(crate) fn settle_working_set() {
    // Have mimalloc return freed bootstrap memory to the OS first: whichever
    // branch follows, transient bootstrap garbage (enumeration buffers,
    // pre-shrink slack) must not be counted, pinned, or trimmed-and-refaulted.
    // SAFETY: mi_collect takes no pointers and is documented safe to call
    // from any thread at any time.
    unsafe { libmimalloc_sys::mi_collect(true) };
    let pin = std::env::var_os("GOZ_PIN_WORKING_SET").is_some_and(|v| v == "1");
    if pin {
        match goz_winfs::pin_working_set() {
            Ok(()) => tracing::info!("working set pinned (index kept resident)"),
            // Best-effort: the memory manager may refuse a large hard minimum.
            // The index still works, so this is a quiet note, not a warning.
            Err(e) => tracing::debug!(error = %e, "working set not pinned (best-effort)"),
        }
    } else {
        match goz_winfs::trim_working_set() {
            Ok(()) => {
                tracing::info!("working set trimmed once (GOZ_PIN_WORKING_SET=1 to pin instead)")
            }
            Err(e) => tracing::debug!(error = %e, "working set not trimmed (best-effort)"),
        }
    }
}

/// Console entry: bootstrap, serve on the pipe, and stop on Ctrl+C.
pub(crate) fn run_console() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    if !goz_winfs::is_elevated() {
        tracing::warn!("gozd is not elevated; raw volume access will likely fail");
    }

    let engine = build_engine()?;
    tracing::info!(
        entries = engine.total_entries,
        volumes = engine.volumes.len(),
        elapsed_s = engine.elapsed.as_secs_f64(),
        "bootstrap complete; serving queries (Ctrl+C to stop)"
    );
    settle_working_set();

    // A never-triggered channel: console stop comes from Ctrl+C inside the
    // server. `_stop_tx` is held so the receiver never sees a closed channel.
    let (_stop_tx, stop_rx) = watch::channel(false);
    let result = server::run(engine.volumes, stop_rx);

    engine.supervisor.shutdown();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_refused_journal_makes_a_snapshot() {
        let info = goz_winfs::JournalInfo {
            journal_id: 7,
            first_usn: 0,
            next_usn: 4096,
            lowest_valid_usn: 0,
            max_usn: i64::MAX,
            maximum_size: 64 << 20,
        };
        assert_eq!(
            bootstrap_phase(&JournalSetup::Active(info)),
            VolumePhase::Live
        );
        assert_eq!(
            bootstrap_phase(&JournalSetup::Refused),
            VolumePhase::Snapshot
        );
        assert!(!bootstrap_phase(&JournalSetup::Unavailable).is_complete());
    }
}
