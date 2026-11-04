//! Volume supervision: one background thread that keeps the indexed volumes
//! in step with the machine and keeps a tail on each of them.
//!
//! A volume is indexed by its tail: the supervisor opens the volume and spawns
//! a tail whose first act is the full build. That one path serves a volume
//! listed at startup, a volume attached later, a volume whose start failed (a
//! disk not ready at boot), and a tail that exited. Each scan the supervisor
//! also lists the machine's volumes on a worker thread, adds the fixed NTFS
//! volumes it does not index yet, and removes the indexed volumes no longer
//! listed (see [`diff_volumes`]). A volume is served from the moment it is
//! found, `Bootstrapping` until its first build is published.
//!
//! For each tail it reads the [`Pulse`] and gives one of three verdicts, one
//! per pulse state:
//!
//! - a **stale beat** means the thread is wedged in a synchronous journal
//!   read (observed live: a spinning-down HDD pended one
//!   `FSCTL_READ_USN_JOURNAL` IRP forever), so its I/O is cancelled and the
//!   tail's ordinary retry path takes over;
//! - **busy** (a full MFT rebuild) is legitimate silence and is left alone,
//!   up to a far larger bound of its own, because a rebuild blocks in the
//!   same kind of syscall and can wedge the same way;
//! - **retired** means the thread exited (a failed rebuild, rescan budget
//!   spent, or a panic) and is revived on a bounded budget: the volume is
//!   reopened by GUID and a fresh tail spawned whose first act is a full
//!   rescan. A volume whose tail never started is retried on the same
//!   budget. A tail that keeps dying gets one `error!` and is dropped from
//!   supervision rather than alarmed about forever. A tail that retired
//!   because its volume refused a new journal left a Snapshot, which has
//!   nothing to tail and nothing to recover, so it is released without a
//!   revival.
//!
//! A disk is let go of when Windows asks to remove it, between scans. The
//! start worker registers for the removal events of the volume it opened
//! (see [`watch_removal`]), and a removal query reaches this thread through
//! [`Signals`]: the volume leaves the registry and its tail stops, closing the
//! handle that would fail the removal, before the query is answered. The
//! volume stays out of discovery until Windows says how the removal ended; if
//! it failed, the volume is indexed again.
//!
//! The supervisor owns the [`TailHandle`]s outright: it is the only place
//! that can replace one (revival) and the natural place to stop them all, so
//! teardown is [`Supervisor::shutdown`] and nothing else holds a tail.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use goz_core::types::VolumePhase;
use goz_winfs::{DeviceEvent, DeviceNotification, VolumeInfo, WinError};
use parking_lot::{Condvar, Mutex};

use crate::bootstrap;
use crate::pulse::{Pulse, awake_secs};
use crate::tail::{self, TailHandle};
use crate::volume_state::{VolumeRegistry, VolumeState};

/// Pause between supervision scans, and so the longest a volume attached
/// meanwhile waits to be found.
const SCAN_INTERVAL: Duration = Duration::from_secs(30);

/// How long a scan waits for its volume listing. A listing takes milliseconds,
/// but a sick device can block it, and the scan must not stop rescuing the
/// other tails meanwhile.
const LISTING_WAIT: Duration = Duration::from_secs(2);

/// How long a beat may go stale before the tail is presumed wedged and its
/// synchronous I/O cancelled. Must comfortably exceed the tail's retry
/// backoff ceiling plus a scan interval, or a healthy tail sleeping out its
/// backoff could be shot mid-nap; a test pins that coupling.
const BEAT_STALL: Duration = Duration::from_secs(300);

/// How long `Busy` (a full MFT rebuild) may run before it too is presumed
/// wedged. Rebuilds block in the same synchronous ioctls as journal reads
/// and can wedge the same way, so exempting them forever would leave the
/// founding failure unrescuable whenever it struck mid-rescan. Generous:
/// a healthy rebuild is tens of seconds, minutes on a huge slow disk.
const BUSY_STALL: Duration = Duration::from_secs(30 * 60);

/// Revival attempts a tail gets before the supervisor stops trying. The
/// budget is consecutive: staying healthy for [`HEALTHY_AFTER`] refunds it,
/// so a volume that dies once a month is revived every time, while one that
/// dies every scan is given up on within minutes.
const MAX_REVIVES: u32 = 3;

/// How long a revived tail must beat CONTINUOUSLY, in time awake, before its
/// failure history is forgotten. Continuous beating, not incarnation age: a
/// revived tail can spend longer than this in its initial rebuild alone, and
/// refunding on age would let a volume that dies right after every rebuild
/// revive forever, one full MFT rebuild per cycle, the exact storm the budget
/// exists to stop.
const HEALTHY_AFTER: Duration = Duration::from_secs(10 * 60);

/// How stale an observed beat may be and still count toward the refund. A
/// wedged tail's stamp freezes, and until [`BEAT_STALL`] declares the wedge
/// the frozen stamp would otherwise keep an earlier anchor alive, letting
/// partially earned health mature into a refund while the tail hangs. Must
/// cover one scan interval (a beat is at most one scan old when observed)
/// and stay far under [`BEAT_STALL`]; a test pins both.
const BEAT_FRESH: Duration = Duration::from_secs(60);

/// What one scan of one tail decided. Pure data so [`assess`] stays a
/// function of its inputs and the whole decision table is unit-testable.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    /// Healthy, or legitimately busy: nothing to do.
    Nothing,
    /// A revived tail has proven itself; forget its failure history.
    ResetBudget,
    /// Presumed wedged in a synchronous syscall: cancel its I/O.
    CancelIo { stalled_secs: u64 },
    /// No tail is running (it exited, or never started) and the budget has
    /// room: start one.
    Revive,
    /// No tail is running and the budget is spent: stop supervising it.
    GiveUp,
    /// The thread exited and left its volume a Snapshot: nothing failed and
    /// there is nothing to tail, so stop supervising it without a revival.
    Release,
}

/// Decides what to do about one tail. `proven_healthy` is whether the
/// current incarnation has beaten continuously for [`HEALTHY_AFTER`]; it
/// only matters alongside `revive_attempts > 0` (an original has no history
/// to refund). `snapshot` is whether the volume is a Snapshot; it only
/// matters for a retired tail.
fn assess(
    pulse: Pulse,
    now: u64,
    revive_attempts: u32,
    proven_healthy: bool,
    snapshot: bool,
) -> Action {
    match pulse {
        // The volume refused a journal: a revival would read the whole MFT
        // again only to be refused again, and giving up would log an error
        // for a volume that is fine.
        Pulse::Retired if snapshot => Action::Release,
        Pulse::Retired => {
            if revive_attempts >= MAX_REVIVES {
                Action::GiveUp
            } else {
                Action::Revive
            }
        }
        Pulse::Busy(since) => {
            let stalled_secs = now.saturating_sub(since);
            if stalled_secs >= BUSY_STALL.as_secs() {
                Action::CancelIo { stalled_secs }
            } else {
                Action::Nothing
            }
        }
        Pulse::Beat(at) => {
            let stalled_secs = now.saturating_sub(at);
            if stalled_secs >= BEAT_STALL.as_secs() {
                Action::CancelIo { stalled_secs }
            } else if revive_attempts > 0 && proven_healthy {
                Action::ResetBudget
            } else {
                Action::Nothing
            }
        }
    }
}

/// Advances the continuous-health anchor for one observation. Health means
/// FRESH beats observed while the volume's phase is `Live`, and nothing
/// else. A beat with the phase Offline is a turning loop that tracks
/// nothing (the read-retry path beats once per backoff turn forever), and a
/// stale beat is a loop that has stopped turning; counting either would
/// refund the budget of a volume delivering zero service and make give-up
/// unreachable, the exact churn the budget exists to stop. Like the snapshot
/// check in `Supervised::scan`, this reads the phase for what it means (is
/// the volume served?), never to decide stall handling.
fn accrue_health(prev: Option<u64>, pulse: Pulse, live: bool, now: u64) -> Option<u64> {
    match pulse {
        Pulse::Beat(at) if live && now.saturating_sub(at) < BEAT_FRESH.as_secs() => {
            Some(prev.unwrap_or(now))
        }
        _ => None,
    }
}

/// How one listing of the machine's volumes changes the indexed set.
#[derive(Debug)]
struct VolumeChanges<'a> {
    /// Listed, fixed NTFS, and not indexed: index these.
    found: Vec<&'a VolumeInfo>,
    /// Indexed and no longer listed: drop these.
    gone: Vec<&'a str>,
}

/// Compares `listed`, the machine's volumes now, with the GUIDs of the
/// `indexed` ones.
///
/// Only a volume missing from the listing is gone. A listed volume that now
/// classifies differently stays indexed: a filesystem query that fails once
/// (a waking disk) reads as not NTFS, and dropping the volume for it would
/// cost a full rebuild on the next scan. A listed volume goz does not index
/// yet is asked again every scan, so a disk that was not ready, or a BitLocker
/// volume unlocked later, is found once it reads as fixed NTFS.
fn diff_volumes<'a>(indexed: &[&'a str], listed: &'a [VolumeInfo]) -> VolumeChanges<'a> {
    let is_listed = |guid: &str| listed.iter().any(|v| v.guid_path == guid);
    VolumeChanges {
        found: listed
            .iter()
            .filter(|v| !indexed.contains(&v.guid_path.as_str()) && bootstrap::indexable(v))
            .collect(),
        gone: indexed.iter().copied().filter(|g| !is_listed(g)).collect(),
    }
}

/// The phase a volume reports once supervision gives up on it, or `None` to
/// keep the honest phase its tail left (`Failed` or `Offline`, each logged
/// with its reason). A volume still `Bootstrapping` never got an index and
/// would otherwise claim to be indexing forever; one still claiming to track
/// has lost its tail.
fn given_up(phase: &VolumePhase) -> Option<VolumePhase> {
    let reason = match phase {
        VolumePhase::Bootstrapping => "never indexed: its tail kept stopping",
        VolumePhase::Live | VolumePhase::Rescanning => {
            "tail thread kept dying; live updates stopped"
        }
        VolumePhase::Snapshot | VolumePhase::Offline | VolumePhase::Failed { .. } => return None,
    };
    Some(VolumePhase::Failed {
        reason: reason.into(),
    })
}

/// What wakes the supervision thread before its next scan: shutdown, and the
/// removal events of the indexed volumes (see [`Fleet::on_removal`]). Under
/// one mutex so the scan sleep is interruptible: teardown flips `stopped` and
/// a notification posts a removal, each waking the condvar, so neither waits
/// out a full [`SCAN_INTERVAL`].
struct Signals {
    state: Mutex<SignalState>,
    wake: Condvar,
}

#[derive(Default)]
struct SignalState {
    stopped: bool,
    /// Set once shutdown has stopped every tail, so every volume is let go
    /// of; a removal query asked during shutdown waits for it.
    tails_stopped: bool,
    /// Posted by the volumes' removal notifications, oldest first.
    posted: Vec<Posted>,
}

/// One removal event of the volume `guid`.
struct Removal {
    guid: String,
    event: DeviceEvent,
}

/// A removal as posted: for a query, with the notification callback that
/// waits on the answer, told once the volume is let go of so the removal
/// finds it closed.
struct Posted {
    removal: Removal,
    done: Option<mpsc::SyncSender<()>>,
}

/// How long a removal query waits for the supervisor to let go of its volume.
/// Covers a pending start's drain and a tail's teardown
/// ([`PENDING_DRAIN_WAIT`] plus [`TEARDOWN_WAIT`]) behind a listing wait; a
/// healthy tail is let go of within a teardown poll.
const RELEASE_WAIT: Duration = Duration::from_secs(12);

/// How long a volume let go of for a removal stays out of discovery when
/// Windows never says how the removal ended. It always says, so this only
/// keeps a missed event from hiding a disk that stayed.
const REMOVAL_OUTCOME_WAIT: Duration = Duration::from_secs(60);

impl Signals {
    /// Posts a removal event of the volume `guid` and, for a query, waits
    /// until the supervisor has let go of the volume, at most
    /// [`RELEASE_WAIT`]. Runs on the notification's thread-pool thread.
    fn post(&self, guid: &str, event: DeviceEvent) {
        let (done, released) = match event {
            DeviceEvent::QueryRemove => {
                let (tx, rx) = mpsc::sync_channel(1);
                (Some(tx), Some(rx))
            }
            DeviceEvent::QueryRemoveFailed | DeviceEvent::Removed => (None, None),
        };
        {
            let mut state = self.state.lock();
            if state.stopped {
                // Shutdown answers no removal, but it stops every tail,
                // which lets go of every volume; a query waits for that.
                if released.is_some() {
                    let until = Instant::now() + RELEASE_WAIT;
                    while !state.tails_stopped && Instant::now() < until {
                        self.wake.wait_until(&mut state, until);
                    }
                }
                return;
            }
            state.posted.push(Posted {
                removal: Removal {
                    guid: guid.to_owned(),
                    event,
                },
                done,
            });
        }
        self.wake.notify_all();
        // Disconnected means the query was dropped: by shutdown once every
        // tail had stopped, which let go of the volume too, or by a
        // supervisor that panicked and can let go of nothing.
        if let Some(released) = released
            && let Err(mpsc::RecvTimeoutError::Timeout) = released.recv_timeout(RELEASE_WAIT)
        {
            tracing::warn!(
                guid,
                "the volume was not let go of in time for its removal; the removal may fail"
            );
        }
    }

    /// Waits until shutdown, a posted removal or `until`, and returns the
    /// removals posted, or `None` once stopped.
    fn wait(&self, until: Instant) -> Option<Vec<Posted>> {
        let mut state = self.state.lock();
        while !state.stopped && state.posted.is_empty() && Instant::now() < until {
            self.wake.wait_until(&mut state, until);
        }
        (!state.stopped).then(|| std::mem::take(&mut state.posted))
    }

    /// Says every tail has stopped, so every volume is let go of: the
    /// queries still posted are dropped, which releases their callbacks,
    /// and a query asked from now on returns at once.
    fn tails_stopped(&self) {
        let mut state = self.state.lock();
        state.stopped = true;
        state.tails_stopped = true;
        state.posted.clear();
        drop(state);
        self.wake.notify_all();
    }
}

/// The supervision thread and the means to stop it. Owns every tail: they
/// live inside the thread while it runs and are shut down by it on exit.
pub(crate) struct Supervisor {
    signals: Arc<Signals>,
    join: JoinHandle<()>,
    /// Real handle to the supervision thread, so `shutdown` can cancel any
    /// blocked synchronous call it might be inside. Defensive: blocking
    /// start and listing I/O lives on worker threads and the supervisor's own
    /// waits are bounded, but a rescue that costs one no-op cancel per poll
    /// is cheap insurance against a future blocking call slipping in. `None`
    /// only if duplication failed, which merely forfeits that rescue.
    io: Option<goz_winfs::ThreadIoHandle>,
}

impl Supervisor {
    /// Adds every fixed NTFS volume in `listed`, the machine's volumes at
    /// startup, to `volumes` as `Bootstrapping`, starts a tail on each, and
    /// starts the supervision thread. Returns before any volume is indexed,
    /// so queries are answered, flagged incomplete, while they index. Fails
    /// only when the OS refuses the thread, after stopping what it started:
    /// nothing would own the tails, find volumes or revive them.
    pub(crate) fn spawn(
        volumes: Arc<VolumeRegistry>,
        listed: &[VolumeInfo],
    ) -> std::io::Result<Supervisor> {
        let signals = Arc::new(Signals {
            state: Mutex::new(SignalState::default()),
            wake: Condvar::new(),
        });
        let mut fleet = Fleet::new(volumes, signals.clone());
        fleet.starting = Some(Instant::now());
        fleet.reconcile(listed, Instant::now());
        // `Builder::spawn` consumes its closure even when thread creation
        // fails, so the fleet rides in a slot the failure path can take back;
        // dropping it would detach tails that could then never be joined.
        let slot = Arc::new(Mutex::new(Some(fleet)));
        let thread_slot = slot.clone();
        let thread_signals = signals.clone();
        let (io_tx, io_rx) = std::sync::mpsc::sync_channel(1);
        match std::thread::Builder::new()
            .name("goz-volume-supervisor".into())
            .spawn(move || {
                // Same per-thread duplication the tails perform, and for the
                // same reason: shutdown may need to cancel this thread's own
                // blocked volume I/O. Unlike a tail, a failure here is not
                // fatal to the thread; it only forfeits that one rescue.
                let _ = io_tx.send(goz_winfs::current_thread_io_handle());
                let fleet = thread_slot
                    .lock()
                    .take()
                    .expect("the supervisor's fleet is taken exactly once");
                supervise(fleet, &thread_signals);
            }) {
            Ok(join) => {
                let io = io_rx.recv().ok().and_then(Result::ok);
                if io.is_none() {
                    tracing::warn!(
                        "no real handle to the supervisor thread; shutdown cannot cancel its blocked I/O"
                    );
                }
                Ok(Supervisor { signals, join, io })
            }
            Err(e) => {
                if let Some(fleet) = slot.lock().take() {
                    fleet.stop_all();
                }
                Err(e)
            }
        }
    }

    /// Stops supervision and every tail, joining all of it. The supervision
    /// thread stops the tails itself (it owns them).
    ///
    /// The supervisor's join is bounded and cancel-assisted for the same
    /// reason its tails' joins are: a start in flight can be blocked in
    /// `open_volume` against a sick device, and a bare join on that would
    /// hang teardown forever.
    pub(crate) fn shutdown(self) {
        self.signals.state.lock().stopped = true;
        self.signals.wake.notify_all();
        let deadline = Instant::now() + SUPERVISOR_JOIN_WAIT;
        while !self.join.is_finished() && Instant::now() < deadline {
            // A no-op (ERROR_NOT_FOUND) unless the thread really is blocked
            // in a syscall; its sleeps and lock waits are not cancellable I/O
            // and are unaffected.
            if let Some(io) = &self.io {
                let _ = goz_winfs::cancel_synchronous_io(io);
            }
            std::thread::sleep(TEARDOWN_POLL);
        }
        if self.join.is_finished() {
            let _ = self.join.join();
        } else {
            tracing::error!(
                "volume supervisor survived its stop signal and every I/O cancel; abandoning it to process exit"
            );
        }
    }
}

/// What a listing worker hands back.
type Listing = Result<Vec<VolumeInfo>, WinError>;

/// Everything the supervision thread owns.
struct Fleet {
    volumes: Arc<VolumeRegistry>,
    /// The indexed volumes that have a tail or may get one. A volume leaves
    /// this list, and stays indexed, when its tail is given up on or released
    /// as a snapshot.
    supervised: Vec<Supervised>,
    /// The listing running on its worker thread, if one is, and when it
    /// began.
    listing: Option<(mpsc::Receiver<Listing>, Instant)>,
    /// When the daemon started, until no volume is `Bootstrapping`.
    starting: Option<Instant>,
    /// Where the volumes' removal notifications post.
    signals: Arc<Signals>,
    /// Volumes let go of for a removal that has not ended, with the
    /// notifications that will say how it ends. Discovery leaves them out.
    removing: HashMap<String, Removing>,
    /// Volumes removed from the machine, and when. A listing begun before
    /// the removal can still name one, so discovery leaves it out until a
    /// listing begun after.
    removed: Vec<(String, Instant)>,
}

/// A volume let go of for a removal that has not ended.
struct Removing {
    notifications: Vec<DeviceNotification>,
    since: Instant,
}

impl Fleet {
    fn new(volumes: Arc<VolumeRegistry>, signals: Arc<Signals>) -> Fleet {
        Fleet {
            volumes,
            supervised: Vec::new(),
            listing: None,
            starting: None,
            signals,
            removing: HashMap::new(),
            removed: Vec::new(),
        }
    }

    /// Brings the indexed volumes in step with `listed`, the volumes on the
    /// machine as a listing begun at `taken` saw them, by the rule
    /// [`diff_volumes`] decides. A volume being removed, or removed after the
    /// listing began, is not found.
    fn reconcile(&mut self, listed: &[VolumeInfo], taken: Instant) {
        self.removed.retain(|(_, at)| *at > taken);
        let current = self.volumes.current();
        let indexed: Vec<&str> = current.iter().map(|v| v.guid.as_str()).collect();
        let changes = diff_volumes(&indexed, listed);
        for guid in changes.gone {
            self.remove(guid);
        }
        for vol in changes.found {
            let guid = vol.guid_path.as_str();
            if self.removing.contains_key(guid) || self.removed.iter().any(|(g, _)| g == guid) {
                continue;
            }
            let state = Arc::new(VolumeState::found(
                vol.guid_path.clone(),
                vol.mounts.clone(),
            ));
            tracing::info!(
                volume = %state.mount_prefix(),
                guid = %state.guid,
                "volume found; indexing it"
            );
            self.volumes.add(state.clone());
            self.supervised
                .push(Supervised::start(state, self.signals.clone()));
        }
    }

    /// Stops indexing a volume that is gone: queries leave it out from now
    /// on, and its tail stops.
    fn remove(&mut self, guid: &str) {
        retire(self.release(guid, "volume is gone; no longer indexed"));
    }

    /// Stops indexing `guid`, logging `why`: queries leave it out from now
    /// on, and its tail stops, which closes its handle on the volume. Returns
    /// the volume's removal notifications, for the caller to keep or retire.
    fn release(&mut self, guid: &str, why: &str) -> Vec<DeviceNotification> {
        self.volumes.remove(guid);
        let Some(i) = self.supervised.iter().position(|s| s.state.guid == guid) else {
            tracing::info!(guid, "{why}");
            return Vec::new();
        };
        let gone = self.supervised.swap_remove(i);
        tracing::info!(volume = %gone.state.mount_prefix(), guid, "{why}");
        let (tails, notifications) = gone.into_parts(Instant::now() + PENDING_DRAIN_WAIT);
        stop_tails(tails);
        notifications
    }

    /// Acts on a removal event of a volume, posted by its notification (see
    /// [`Signals::post`]); a query is answered once this returns. Returns
    /// whether the volumes should be listed now: a removal that failed leaves
    /// a disk to index again.
    fn on_removal(&mut self, removal: Removal) -> bool {
        let Removal { guid, event } = removal;
        let indexed = self.volumes.current().iter().any(|v| v.guid == guid);
        match event {
            DeviceEvent::QueryRemove => {
                // Let go of the volume, which closes its tail's handle. The
                // notifications stay registered to hear how the removal ends.
                let notifications = if indexed {
                    self.release(&guid, "the volume is being removed; letting go of it")
                } else {
                    Vec::new()
                };
                self.removing
                    .entry(guid)
                    .or_insert_with(|| Removing {
                        notifications: Vec::new(),
                        since: Instant::now(),
                    })
                    .notifications
                    .extend(notifications);
                false
            }
            DeviceEvent::QueryRemoveFailed => {
                let Some(removing) = self.removing.remove(&guid) else {
                    return false;
                };
                tracing::info!(
                    guid,
                    "the volume's removal did not happen; indexing it again"
                );
                retire(removing.notifications);
                true
            }
            DeviceEvent::Removed => {
                // Dropped now rather than at the next listing; a disk pulled
                // without asking has its tail stopped here.
                let mut notifications = if indexed {
                    self.release(&guid, "the volume was removed; no longer indexed")
                } else {
                    Vec::new()
                };
                if let Some(removing) = self.removing.remove(&guid) {
                    tracing::info!(guid, "the volume was removed");
                    notifications.extend(removing.notifications);
                }
                retire(notifications);
                self.removed.retain(|(g, _)| *g != guid);
                self.removed.push((guid, Instant::now()));
                false
            }
        }
    }

    /// Forgets the removals Windows never said the end of after
    /// [`REMOVAL_OUTCOME_WAIT`], so a disk that stayed is found again.
    fn expire_removing(&mut self) {
        let expired: Vec<String> = self
            .removing
            .iter()
            .filter(|(_, r)| r.since.elapsed() >= REMOVAL_OUTCOME_WAIT)
            .map(|(guid, _)| guid.clone())
            .collect();
        for guid in expired {
            if let Some(removing) = self.removing.remove(&guid) {
                tracing::warn!(
                    guid,
                    "no word on how the volume's removal ended; finding it again"
                );
                retire(removing.notifications);
            }
        }
    }

    /// Lists the machine's volumes and reconciles the indexed ones with the
    /// listing. Listing asks every volume for its filesystem, which a sick
    /// device can block, so it runs on a worker thread and is waited for only
    /// [`LISTING_WAIT`]; one still running then is reconciled at a later
    /// scan, never stacked.
    fn discover(&mut self) {
        let (rx, taken) = match self.listing.take() {
            Some(listing) => listing,
            None => {
                // Buffered, so the worker never waits on this thread.
                let (tx, rx) = mpsc::sync_channel(1);
                let taken = Instant::now();
                let spawned = std::thread::Builder::new()
                    .name("goz-volume-listing".into())
                    .spawn(move || {
                        let _ = tx.send(goz_winfs::enumerate_volumes());
                    });
                if let Err(e) = spawned {
                    tracing::warn!(
                        error = %e,
                        "could not start listing volumes; trying again next scan"
                    );
                    return;
                }
                (rx, taken)
            }
        };
        match rx.recv_timeout(LISTING_WAIT) {
            Ok(Ok(listed)) => self.reconcile(&listed, taken),
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "could not list volumes; trying again next scan")
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                tracing::debug!("listing volumes is slow; reconciling at a later scan");
                self.listing = Some((rx, taken));
            }
            // The worker died without reporting; the next scan lists again.
            Err(mpsc::RecvTimeoutError::Disconnected) => {}
        }
    }

    /// Once no volume is `Bootstrapping`, logs the startup totals and settles
    /// the working set, one time only, as the daemon did once every volume
    /// was built before it served.
    fn settle_once_indexed(&mut self) {
        let Some(started) = self.starting else {
            return;
        };
        let volumes = self.volumes.current();
        if volumes
            .iter()
            .any(|v| *v.phase.read() == VolumePhase::Bootstrapping)
        {
            return;
        }
        let entries: usize = volumes.iter().map(|v| v.index.read().len()).sum();
        tracing::info!(
            entries,
            volumes = volumes.len(),
            elapsed_s = started.elapsed().as_secs_f64(),
            "startup indexing finished"
        );
        crate::run::settle_working_set();
        self.starting = None;
    }

    /// Stops every tail, resolving in-flight starts first so a tail a worker
    /// hands over is stopped with the rest. A removal query is answered only
    /// then, with every volume let go of, and the removal notifications are
    /// unregistered last, on this thread: no callback waits on it any more
    /// (see [`Signals::post`]).
    fn stop_all(self) {
        let deadline = Instant::now() + PENDING_DRAIN_WAIT;
        let (tails, notifications): (Vec<_>, Vec<_>) = self
            .supervised
            .into_iter()
            .map(|s| s.into_parts(deadline))
            .unzip();
        stop_tails(tails.into_iter().flatten().collect());
        self.signals.tails_stopped();
        drop(notifications);
        drop(self.removing);
    }
}

/// Unregisters removal notifications off the supervision thread:
/// unregistering waits for the callbacks in flight, and one may be waiting on
/// this thread to answer it (see [`Signals::post`]). Should no thread start,
/// they unregister here, which waits at most that callback's
/// [`RELEASE_WAIT`].
fn retire(notifications: Vec<DeviceNotification>) {
    if notifications.is_empty() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("goz-unregister".into())
        .spawn(move || drop(notifications));
}

/// What a start worker hands back: the tail, and the notification that
/// reports the removal of its volume (`None` when registering failed, which
/// costs only safe removal of the disk).
struct Started {
    tail: TailHandle,
    notification: Option<DeviceNotification>,
}

/// One supervised volume: its tail, if one runs, plus its revival history.
struct Supervised {
    state: Arc<VolumeState>,
    /// `None` until a start succeeds.
    tail: Option<TailHandle>,
    /// Reports the removal of the volume the current tail opened.
    notification: Option<DeviceNotification>,
    /// Where `notification`, and those of later starts, post.
    signals: Arc<Signals>,
    /// Revivals since the budget was last refunded.
    revive_attempts: u32,
    /// When, in [`awake_secs`], the current unbroken run of healthy
    /// observations (a fresh `Beat` with the phase `Live`, see
    /// [`accrue_health`]) began; cleared by anything else and by a revival.
    /// This, not the incarnation's age, is what earns a budget refund.
    beating_since: Option<u64>,
    /// A start in flight on its worker thread, polled each scan. `Some`
    /// suppresses further actions on this volume until the worker reports.
    pending: Option<PendingRevival>,
}

/// A tail start (the first, or a revival) running on its own short-lived
/// worker thread. It opens the volume, and that open is blocking I/O against
/// possibly-sick hardware; run inline it would wedge the scan loop and cost
/// every OTHER volume its stall rescue and revival, so the supervisor never
/// blocks on it.
struct PendingRevival {
    /// Delivers the worker's outcome over a RENDEZVOUS channel: the send can
    /// only complete into a live receive, never into a buffer, so there is
    /// no window where a tail sits sent-but-unowned. Once this receiver
    /// drops, the worker's send fails and the worker shuts down any tail it
    /// built; abandoning a wedged worker can never leak a live writer.
    rx: mpsc::Receiver<std::io::Result<Started>>,
    /// Real handle to the worker thread, so a wedged `open_volume` can be
    /// cancelled; `None` only if duplication failed on the worker.
    io: Option<goz_winfs::ThreadIoHandle>,
    /// Joined when the worker reports; abandoned if it never does.
    worker: JoinHandle<()>,
    /// The blocked-revival warning fires once, not per scan.
    reported_blocked: bool,
}

/// Grace given a stopping tail to notice its flag before the first cancel
/// wave, and between waves. Healthy tails exit within one idle poll or one
/// backoff slice (both 50 ms), so one poll of grace covers them all.
const TEARDOWN_POLL: Duration = Duration::from_millis(250);
/// Total time teardown fights for a tail before abandoning it. Covers every
/// clean exit path with a wide margin (backoff sleeps are interruptible,
/// rescans poll the stop flag between ioctls, and the stat/link-walk batches
/// check it per file); only a thread wedged in an IRP that ignores repeated
/// cancels can still be running at the deadline.
const TEARDOWN_WAIT: Duration = Duration::from_secs(5);

/// Total grace teardown gives ALL in-flight revival workers together, not
/// each: a shared deadline, so a machine with many volumes wedging at once
/// cannot stretch teardown past what [`SUPERVISOR_JOIN_WAIT`] covers.
const PENDING_DRAIN_WAIT: Duration = Duration::from_secs(3);

/// Total time [`Supervisor::shutdown`] waits for the supervision thread
/// itself. It must comfortably exceed the thread's legitimate teardown worst
/// case: a scan waiting out [`LISTING_WAIT`], then [`TEARDOWN_WAIT`] plus
/// [`PENDING_DRAIN_WAIT`]. A test pins the coupling.
const SUPERVISOR_JOIN_WAIT: Duration = Duration::from_secs(15);

/// The supervisor thread body: run the scan loop, then stop every tail. The
/// scan loop is unwind-guarded because the tails were moved in here: a panic
/// that dropped their handles would detach every tail thread with no stop
/// flag set, leaving live index writers nobody could stop, join, or rescue,
/// while `Supervisor::shutdown` returned as if teardown had succeeded.
fn supervise(mut fleet: Fleet, signals: &Signals) {
    // AssertUnwindSafe: the closure touches only `fleet` (handles, plain
    // counters, and the registry's lock-guarded list), which stays
    // structurally valid wherever a panic lands; the worst carryover is a
    // half-updated budget on tails that are about to be stopped anyway.
    let scanning = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        scan_loop(&mut fleet, signals);
    }));
    if scanning.is_err() {
        // Do NOT stop the tails: they are healthy writers, and stopping
        // them would silently end tracking while every phase still says
        // Live and the query server keeps serving. Only supervision (stall
        // rescue, revival, finding and dropping volumes) is lost; park until
        // shutdown so teardown still stops and joins everything properly.
        // A removal posted meanwhile is dropped unanswered, so its callback
        // does not wait for what nobody can do.
        tracing::error!(
            "volume supervisor panicked; tails keep running unsupervised (no stuck-I/O rescue, revival, volume changes or safe removal) until shutdown"
        );
        while signals.wait(Instant::now() + SCAN_INTERVAL).is_some() {}
    }
    fleet.stop_all();
}

/// Resolves one in-flight start at teardown or removal: cancel the worker's
/// blocked I/O, give it a moment to report, and take any tail it built so the
/// caller stops it with the rest. A worker that stays wedged is abandoned;
/// dropping its receiver makes its eventual send fail, and the worker then
/// shuts down anything it built, so nothing leaks.
fn drain_pending(pending: PendingRevival, mount: &str, deadline: Instant) -> Option<Started> {
    if let Some(io) = &pending.io {
        let _ = goz_winfs::cancel_synchronous_io(io);
    }
    // The deadline is shared across every pending drain; once it passes,
    // this degrades to a try_recv so a fleet of wedged workers costs
    // teardown [`PENDING_DRAIN_WAIT`] in total, not each.
    let grace = deadline.saturating_duration_since(Instant::now());
    match pending.rx.recv_timeout(grace) {
        Ok(outcome) => {
            let _ = pending.worker.join();
            outcome.ok()
        }
        Err(_) => {
            tracing::warn!(
                volume = %mount,
                "a tail start is still blocked; abandoning it (it cleans up anything it builds)"
            );
            None
        }
    }
}

/// Scan every tail each interval, act on its pulse, and drop the ones that
/// need no more supervision; then bring the volumes in step with the machine.
/// Returns when the stop flag flips.
fn scan_loop(fleet: &mut Fleet, signals: &Signals) {
    let mut next_scan = Instant::now() + SCAN_INTERVAL;
    loop {
        // A removal is acted on as soon as it is posted, between scans: the
        // removal it asks about waits on the answer.
        let Some(posted) = signals.wait(next_scan) else {
            return;
        };
        let mut relist = false;
        for Posted { removal, done } in posted {
            relist |= fleet.on_removal(removal);
            // Answered only now, with the volume let go of: the removal
            // fails while any handle on it is open.
            if let Some(done) = done {
                let _ = done.send(());
            }
        }
        if relist {
            // A volume found now must get its first poll a full scan
            // interval after its start, the grace `poll_revival` assumes.
            fleet.discover();
            next_scan = Instant::now() + SCAN_INTERVAL;
        }
        if Instant::now() < next_scan {
            continue;
        }
        let now = awake_secs();
        let supervised = &mut fleet.supervised;
        let mut i = 0;
        while i < supervised.len() {
            if supervised[i].scan(now) {
                i += 1;
                continue;
            }
            // Given up on, or released as a snapshot: its thread already
            // retired, so this join is immediate. Dropping the entry
            // releases the pulse and the duplicated thread handle, which the
            // old watchdog held (and alarmed through) for the life of the
            // process.
            let done = supervised.swap_remove(i);
            retire(done.notification.into_iter().collect());
            if let Some(tail) = done.tail {
                tail.shutdown();
            }
        }
        fleet.expire_removing();
        // After the scan, so a volume found now gets its first poll a full
        // scan interval after its start, the grace `poll_revival` assumes;
        // the interval runs from here for the same reason, since the listing
        // and any removals it brings can take seconds.
        fleet.discover();
        fleet.settle_once_indexed();
        next_scan = Instant::now() + SCAN_INTERVAL;
    }
}

/// Stops every tail, rescuing wedged ones: flag them all first, then
/// repeatedly cancel blocked synchronous I/O until each thread exits. A
/// thread inside a pended IRP can never observe its stop flag (the founding
/// failure), so a bare join here would hang teardown forever. The old design
/// got this rescue by accident, from a detached watchdog that outlived
/// teardown; this makes it explicit. A thread whose IRP ignores every cancel
/// is abandoned with an error at the deadline rather than hanging shutdown;
/// process exit reclaims it.
fn stop_tails(mut tails: Vec<TailHandle>) {
    // Every flag first: a serial signal-and-join would leave the tails
    // behind a wedged one never even asked to stop.
    for t in &tails {
        t.signal_stop();
    }
    let deadline = Instant::now() + TEARDOWN_WAIT;
    let mut grace = true;
    loop {
        let mut i = 0;
        while i < tails.len() {
            if tails[i].is_finished() {
                tails.swap_remove(i).shutdown(); // exited: join is immediate
            } else {
                i += 1;
            }
        }
        if tails.is_empty() {
            return;
        }
        if Instant::now() >= deadline {
            break;
        }
        if !grace {
            // Still running after a grace poll means blocked in a syscall.
            // Aborting it lands the tail in its error path, which rechecks
            // the stop flag promptly. `Ok(false)` (not in a cancellable
            // wait right now) and errors are retried next round.
            for t in &tails {
                let _ = goz_winfs::cancel_synchronous_io(&t.io_handle);
            }
        }
        grace = false;
        std::thread::sleep(TEARDOWN_POLL);
    }
    for t in tails {
        tracing::error!(
            volume = %t.mount,
            thread_id = t.io_handle.thread_id,
            "tail thread survived its stop flag and every I/O cancel; abandoning it to process exit"
        );
        drop(t);
    }
}

impl Supervised {
    /// A volume that has no tail yet, with its first start under way. The
    /// first start is not a revival, so it costs no budget.
    fn start(state: Arc<VolumeState>, signals: Arc<Signals>) -> Supervised {
        let mut supervised = Supervised {
            state,
            tail: None,
            notification: None,
            signals,
            revive_attempts: 0,
            beating_since: None,
            pending: None,
        };
        supervised.launch();
        supervised
    }

    /// Every tail this volume holds or has in flight, resolving a pending
    /// start by `deadline` (see [`drain_pending`]), so the caller stops them
    /// together, and the removal notifications that came with them.
    fn into_parts(self, deadline: Instant) -> (Vec<TailHandle>, Vec<DeviceNotification>) {
        let mut tails = Vec::new();
        let mut notifications = Vec::new();
        if let Some(pending) = self.pending
            && let Some(fresh) = drain_pending(pending, self.state.mount_prefix(), deadline)
        {
            tails.push(fresh.tail);
            notifications.extend(fresh.notification);
        }
        tails.extend(self.tail);
        notifications.extend(self.notification);
        (tails, notifications)
    }

    /// One scan of this volume. Returns false when supervision should end.
    fn scan(&mut self, now: u64) -> bool {
        if self.pending.is_some() {
            self.poll_revival();
            return true;
        }
        // No tail yet reads as a retired one: started again while budget
        // lasts.
        let pulse = self
            .tail
            .as_ref()
            .map_or(Pulse::Retired, |t| t.pulse.load());
        // Read only for a retired tail, so a running one still costs one
        // atomic load per scan.
        let snapshot =
            pulse == Pulse::Retired && matches!(*self.state.phase.read(), VolumePhase::Snapshot);
        // Track the unbroken run of healthy observations this incarnation
        // has shown. Sampled at scan granularity, which can only
        // under-count health, never fake it. The anchor is only ever
        // consulted for a refund, so a never-revived original skips the
        // bookkeeping (and the phase read) entirely; the common path costs
        // exactly the one atomic load the old watchdog cost.
        if self.revive_attempts > 0 {
            let live = matches!(*self.state.phase.read(), VolumePhase::Live);
            self.beating_since = accrue_health(self.beating_since, pulse, live, now);
        }
        let proven_healthy = self
            .beating_since
            .is_some_and(|since| now.saturating_sub(since) >= HEALTHY_AFTER.as_secs());
        match assess(pulse, now, self.revive_attempts, proven_healthy, snapshot) {
            Action::Nothing => true,
            Action::ResetBudget => {
                tracing::debug!(
                    volume = %self.state.mount_prefix(),
                    "revived tail has stayed healthy; revival budget refunded"
                );
                self.revive_attempts = 0;
                self.beating_since = None;
                true
            }
            Action::CancelIo { stalled_secs } => {
                if let Some(tail) = &self.tail {
                    cancel_stuck_io(tail, stalled_secs);
                }
                true
            }
            Action::Revive => {
                self.launch_revival();
                true
            }
            Action::GiveUp => {
                self.give_up();
                false
            }
            Action::Release => {
                // The tail logged why it stopped; this is no failure.
                tracing::debug!(
                    volume = %self.state.mount_prefix(),
                    "tail stopped on a snapshot volume; nothing to revive"
                );
                false
            }
        }
    }

    /// Replaces a retired tail, or retries a start that failed: charge the
    /// attempt, then [`launch`](Self::launch). The attempt is charged up
    /// front: any failure leaves the retired tail (or none) in place, a later
    /// scan lands back in [`Action::Revive`], and the budget converts
    /// repeated failure into [`Action::GiveUp`] with at least a scan interval
    /// of backoff between tries.
    fn launch_revival(&mut self) {
        self.revive_attempts += 1;
        self.beating_since = None;
        // Every deliberate exit sets Failed, Offline or Snapshot itself (a
        // Snapshot is released, not revived), so a phase still claiming to
        // track here is the panic signature. Correct it
        // before the revival window opens: a revival blocked on sick
        // hardware must not extend a false Live indefinitely.
        {
            let mut phase = self.state.phase.write();
            if matches!(*phase, VolumePhase::Live | VolumePhase::Rescanning) {
                *phase = VolumePhase::Offline;
            }
        }
        if self.tail.is_some() {
            tracing::warn!(
                volume = %self.state.mount_prefix(),
                attempt = self.revive_attempts,
                budget = MAX_REVIVES,
                "tail thread exited; reviving it"
            );
        } else {
            tracing::warn!(
                volume = %self.state.mount_prefix(),
                attempt = self.revive_attempts,
                budget = MAX_REVIVES,
                "the volume's tail did not start; retrying"
            );
        }
        self.launch();
    }

    /// Hands the blocking work of starting a tail (volume open, spawn) to a
    /// worker thread, which [`poll_revival`](Self::poll_revival) polls on
    /// later scans. The tail's first act is the full build.
    fn launch(&mut self) {
        let state = self.state.clone();
        let signals = self.signals.clone();
        let (io_tx, io_rx) = mpsc::sync_channel(1);
        // Capacity 0 (rendezvous) on purpose: a buffered channel would let
        // the worker's send "succeed" with nobody listening, right as
        // teardown's drain gives up, and the buffered TailHandle would then
        // be dropped un-stopped when the receiver drops. Rendezvous makes
        // that unrepresentable; the worker just parks in send until the next
        // poll or fails the send and cleans up itself.
        let (result_tx, result_rx) = mpsc::sync_channel(0);
        let spawned = std::thread::Builder::new()
            .name(format!("goz-start-{}", self.state.mount_prefix()))
            .spawn(move || {
                // Duplicated ON the worker (the pseudo-handle is per-thread)
                // so the supervisor can cancel an open a sick device pends.
                let _ = io_tx.send(goz_winfs::current_thread_io_handle());
                let outcome = goz_winfs::open_volume(&state.guid)
                    .map_err(|e| std::io::Error::other(format!("opening the volume: {e}")))
                    .and_then(|handle| {
                        // Before the tail takes the handle, so a removal
                        // asked for from now on reaches the supervisor.
                        let notification = watch_removal(&handle, &state, signals);
                        tail::spawn(state.clone(), handle)
                            .map(|tail| Started { tail, notification })
                    });
                // A volume with no index says why now, not when the next scan
                // collects this outcome, up to 30 s later, as a volume that
                // failed at startup always did.
                if let Err(e) = &outcome
                    && !state.indexed.load(Ordering::Acquire)
                {
                    *state.phase.write() = VolumePhase::Failed {
                        reason: e.to_string(),
                    };
                }
                if let Err(returned) = result_tx.send(outcome) {
                    // The supervisor stopped listening (teardown abandoned
                    // this worker). A tail built anyway must not run
                    // unowned; its thread already checks a fresh stop flag.
                    if let Ok(started) = returned.0 {
                        started.tail.shutdown();
                    }
                }
            });
        match spawned {
            Ok(worker) => {
                // The worker sends its handle before any blocking work, so
                // this recv is prompt.
                let io = io_rx.recv().ok().and_then(Result::ok);
                self.pending = Some(PendingRevival {
                    rx: result_rx,
                    io,
                    worker,
                    reported_blocked: false,
                });
            }
            Err(e) => {
                tracing::error!(
                    volume = %self.state.mount_prefix(),
                    error = %e,
                    "could not start a worker to start the tail; retrying next scan while budget lasts"
                );
            }
        }
    }

    /// One poll of an in-flight start. The first poll happens a full scan
    /// interval after launch, which is the grace a legitimate spin-up gets;
    /// a worker still silent after that is presumed blocked in the open and
    /// has its I/O cancelled each scan until it reports.
    fn poll_revival(&mut self) {
        let Some(pending) = &mut self.pending else {
            return;
        };
        match pending.rx.try_recv() {
            Ok(outcome) => {
                let pending = self.pending.take().expect("pending checked above");
                // Reported means the worker is past its send; this join is
                // immediate.
                let _ = pending.worker.join();
                match outcome {
                    Ok(Started { tail, notification }) => {
                        tracing::info!(volume = %tail.mount, "tail started");
                        // The old thread already retired, so this join is
                        // immediate; shutting it down (not just dropping
                        // it) reaps the thread.
                        if let Some(old) = self.tail.replace(tail) {
                            old.shutdown();
                        }
                        // The fresh tail's handle is the one to watch now.
                        if let Some(old) = std::mem::replace(&mut self.notification, notification) {
                            retire(vec![old]);
                        }
                    }
                    Err(e) => {
                        tracing::error!(
                            volume = %self.state.mount_prefix(),
                            error = %e,
                            "starting the tail failed; retrying next scan while budget lasts"
                        );
                        // A volume with no index says why, as a volume that
                        // failed at startup always did; the next try reports
                        // Bootstrapping again once its tail builds. An indexed
                        // volume keeps the phase its last tail left.
                        if !self.state.indexed.load(Ordering::Acquire) {
                            *self.state.phase.write() = VolumePhase::Failed {
                                reason: e.to_string(),
                            };
                        }
                    }
                }
            }
            Err(mpsc::TryRecvError::Empty) => {
                if !pending.reported_blocked {
                    tracing::warn!(
                        volume = %self.state.mount_prefix(),
                        "starting the tail is blocked opening the volume; cancelling its I/O"
                    );
                    pending.reported_blocked = true;
                }
                if let Some(io) = &pending.io {
                    let _ = goz_winfs::cancel_synchronous_io(io);
                }
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                let pending = self.pending.take().expect("pending checked above");
                let _ = pending.worker.join();
                tracing::error!(
                    volume = %self.state.mount_prefix(),
                    "the worker starting the tail died without reporting; retrying next scan while budget lasts"
                );
            }
        }
    }

    /// The budget is spent: say so once, loudly, and stop supervising, with
    /// the phase [`given_up`] decides.
    fn give_up(&self) {
        tracing::error!(
            volume = %self.state.mount_prefix(),
            attempts = self.revive_attempts,
            "the volume's tail keeps failing; giving up until the volume is attached again or the daemon restarts"
        );
        let mut phase = self.state.phase.write();
        if let Some(failed) = given_up(&phase) {
            *phase = failed;
        }
    }
}

/// Registers for the removal events of the volume `handle` is open on,
/// posting them to `signals`. A volume without a registration is still
/// indexed; only safe removal of its disk needs the service stopped.
fn watch_removal(
    handle: &goz_winfs::VolumeHandle,
    state: &VolumeState,
    signals: Arc<Signals>,
) -> Option<DeviceNotification> {
    let guid = state.guid.clone();
    match DeviceNotification::register(handle, move |event| signals.post(&guid, event)) {
        Ok(notification) => Some(notification),
        Err(e) => {
            tracing::warn!(
                volume = %state.mount_prefix(),
                error = %e,
                "cannot watch for the volume's removal; ejecting its disk needs the service stopped"
            );
            None
        }
    }
}

/// Cancels the blocked synchronous I/O of a presumed-wedged tail. The
/// aborted call returns `ERROR_OPERATION_ABORTED` to the tail loop, whose
/// ordinary retry path (backoff, Offline past the degrade window, recovery
/// on success) takes over. Repeats every scan while the stall persists, so
/// an IRP that ignores one cancel gets another.
fn cancel_stuck_io(tail: &TailHandle, stalled_secs: u64) {
    tracing::error!(
        volume = %tail.mount,
        stalled_secs,
        thread_id = tail.io_handle.thread_id,
        "tail thread heartbeat stalled; cancelling its blocked synchronous I/O"
    );
    match goz_winfs::cancel_synchronous_io(&tail.io_handle) {
        Ok(true) => {
            tracing::warn!(volume = %tail.mount, "stuck journal I/O cancelled; tail will retry")
        }
        Ok(false) => {
            tracing::warn!(volume = %tail.mount, "tail stalled but not in a cancellable wait")
        }
        Err(e) => {
            tracing::error!(volume = %tail.mount, error = %e, "cancelling stuck I/O failed")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A day awake, in [`awake_secs`].
    const NOW: u64 = 86_400;

    /// Volume GUIDs no disk has, so a start on one fails to open at once.
    const C: &str = r"\\?\Volume{0000000c-0000-0000-0000-000000000000}\";
    const D: &str = r"\\?\Volume{0000000d-0000-0000-0000-000000000000}\";
    const STICK: &str = r"\\?\Volume{00000005-0000-0000-0000-000000000000}\";
    const PLUGGED: &str = r"\\?\Volume{00000009-0000-0000-0000-000000000000}\";
    const UNPLUGGED: &str = r"\\?\Volume{00000006-0000-0000-0000-000000000000}\";

    fn listed(guid: &str, is_fixed: bool, is_ntfs: bool) -> VolumeInfo {
        VolumeInfo {
            guid_path: guid.into(),
            mounts: Vec::new(),
            is_fixed,
            is_ntfs,
        }
    }

    fn fleet() -> (Fleet, Arc<VolumeRegistry>) {
        let volumes = Arc::new(VolumeRegistry::default());
        (Fleet::new(volumes.clone(), signals()), volumes)
    }

    fn indexed(volumes: &VolumeRegistry) -> Vec<String> {
        volumes.current().iter().map(|v| v.guid.clone()).collect()
    }

    /// Polls a start until its worker reports.
    fn settle_start(s: &mut Supervised) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while s.pending.is_some() {
            assert!(Instant::now() < deadline, "the start never reported");
            std::thread::sleep(Duration::from_millis(5));
            s.poll_revival();
        }
    }

    /// A volume that left the listing is gone and a fixed NTFS one that is
    /// new is found. An indexed volume that now reads as not NTFS (its
    /// filesystem query failed once, as on a waking disk) stays and is not
    /// found again, or every misread would cost a full rebuild; a removable
    /// stick is never found.
    #[test]
    fn only_a_volume_that_left_the_listing_is_gone() {
        let now = [
            listed(C, true, true),
            listed(D, true, false),
            listed(STICK, false, true),
            listed(PLUGGED, true, true),
        ];
        let changes = diff_volumes(&[C, D, UNPLUGGED], &now);
        let found: Vec<&str> = changes.found.iter().map(|v| v.guid_path.as_str()).collect();
        assert_eq!(found, [PLUGGED]);
        assert_eq!(changes.gone, [UNPLUGGED]);

        let unchanged = diff_volumes(&[C, D, PLUGGED], &now);
        assert!(unchanged.found.is_empty() && unchanged.gone.is_empty());
    }

    /// A listed volume is in the registry, `Bootstrapping`, as soon as the
    /// listing is reconciled, with its start running off the scan thread;
    /// one that leaves the listing leaves the registry and supervision, and
    /// its start is drained and stopped.
    #[test]
    fn a_listing_adds_found_volumes_and_drops_gone_ones() {
        let (mut fleet, volumes) = fleet();
        fleet.reconcile(
            &[
                listed(C, true, true),
                listed(STICK, false, true),
                listed(D, true, false),
            ],
            Instant::now(),
        );
        assert_eq!(indexed(&volumes), [C]);
        assert_eq!(
            *volumes.current()[0].phase.read(),
            VolumePhase::Bootstrapping
        );
        assert_eq!(fleet.supervised.len(), 1);
        assert!(fleet.supervised[0].pending.is_some());

        fleet.reconcile(&[listed(C, true, false)], Instant::now());
        assert_eq!(indexed(&volumes), [C], "a misread volume stays indexed");
        assert_eq!(fleet.supervised.len(), 1, "and is not started twice");

        fleet.reconcile(&[], Instant::now());
        assert!(volumes.current().is_empty());
        assert!(fleet.supervised.is_empty());
    }

    fn removal(guid: &str, event: DeviceEvent) -> Removal {
        Removal {
            guid: guid.into(),
            event,
        }
    }

    fn signals() -> Arc<Signals> {
        Arc::new(Signals {
            state: Mutex::new(SignalState::default()),
            wake: Condvar::new(),
        })
    }

    /// A volume whose removal is asked for is let go of: it leaves the
    /// registry and supervision, and stays out of discovery while the
    /// removal is under way. A removal that fails has the volume listed
    /// again and indexed; a volume removed, asked or not, stays out until a
    /// listing begun after the removal names it.
    #[test]
    fn a_removal_lets_go_of_the_volume_until_it_ends() {
        let (mut fleet, volumes) = fleet();
        let both = [listed(C, true, true), listed(D, true, true)];
        let before = Instant::now();
        fleet.reconcile(&both, before);
        assert_eq!(indexed(&volumes), [C, D]);

        assert!(!fleet.on_removal(removal(D, DeviceEvent::QueryRemove)));
        assert_eq!(indexed(&volumes), [C]);
        assert_eq!(fleet.supervised.len(), 1);
        fleet.reconcile(&both, Instant::now());
        assert_eq!(
            indexed(&volumes),
            [C],
            "not found while it is being removed"
        );

        assert!(fleet.on_removal(removal(D, DeviceEvent::QueryRemoveFailed)));
        fleet.reconcile(&both, Instant::now());
        assert_eq!(
            indexed(&volumes),
            [C, D],
            "the disk stayed, so it is indexed"
        );

        // Pulled without asking.
        assert!(!fleet.on_removal(removal(D, DeviceEvent::Removed)));
        assert_eq!(indexed(&volumes), [C], "a pulled disk is dropped at once");
        fleet.reconcile(&both, before);
        assert_eq!(
            indexed(&volumes),
            [C],
            "a listing from before still names it"
        );
        fleet.reconcile(&both, Instant::now());
        assert_eq!(
            indexed(&volumes),
            [C, D],
            "named after, it was attached again"
        );

        // Asked, then removed: let go of at the query, kept out by the
        // removal once the query's own hold ends.
        let asked = Instant::now();
        assert!(!fleet.on_removal(removal(D, DeviceEvent::QueryRemove)));
        assert!(!fleet.on_removal(removal(D, DeviceEvent::Removed)));
        assert!(fleet.removing.is_empty());
        fleet.reconcile(&both, asked);
        assert_eq!(
            indexed(&volumes),
            [C],
            "a listing from before still names it"
        );
        fleet.reconcile(&both, Instant::now());
        assert_eq!(
            indexed(&volumes),
            [C, D],
            "named after, it was attached again"
        );

        assert!(!fleet.on_removal(removal(D, DeviceEvent::QueryRemoveFailed)));
        assert!(!fleet.on_removal(removal(UNPLUGGED, DeviceEvent::Removed)));
        assert_eq!(
            indexed(&volumes),
            [C, D],
            "events for no removal change nothing"
        );
    }

    /// A removal wakes the supervision thread at once rather than at its
    /// next scan, and a query waits for its answer. During shutdown a query
    /// waits until every tail has stopped, and after that not at all.
    #[test]
    fn a_posted_removal_wakes_the_wait_and_a_query_waits_for_its_answer() {
        let signals = signals();
        let poster = signals.clone();
        let asked = std::thread::spawn(move || {
            let started = Instant::now();
            poster.post(D, DeviceEvent::QueryRemove);
            started.elapsed()
        });
        let waiting = Instant::now();
        let posted = signals
            .wait(waiting + Duration::from_secs(20))
            .expect("not stopped");
        assert!(
            waiting.elapsed() < Duration::from_secs(5),
            "woken by the post"
        );
        assert_eq!(posted.len(), 1);
        assert_eq!(posted[0].removal.event, DeviceEvent::QueryRemove);
        std::thread::sleep(Duration::from_millis(50));
        assert!(!asked.is_finished(), "the query waits for its answer");
        for done in posted.into_iter().filter_map(|p| p.done) {
            let _ = done.send(());
        }
        assert!(asked.join().unwrap() < Duration::from_secs(5));

        let idle = Instant::now();
        let posted = signals.wait(idle + Duration::from_millis(20));
        assert!(posted.is_some_and(|p| p.is_empty()));
        assert!(idle.elapsed() >= Duration::from_millis(20));

        signals.state.lock().stopped = true;
        assert!(
            signals
                .wait(Instant::now() + Duration::from_secs(20))
                .is_none()
        );
        let poster = signals.clone();
        let asked = std::thread::spawn(move || {
            let started = Instant::now();
            poster.post(D, DeviceEvent::QueryRemove);
            started.elapsed()
        });
        std::thread::sleep(Duration::from_millis(50));
        assert!(!asked.is_finished(), "waits for the tails to stop");
        signals.tails_stopped();
        assert!(asked.join().unwrap() < Duration::from_secs(5));
        let started = Instant::now();
        signals.post(D, DeviceEvent::QueryRemove);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "no wait once they have"
        );
    }

    /// A volume that cannot be opened (a disk not ready at boot) says why at
    /// once, as a volume that failed at startup always did, and is retried on
    /// the revival budget; once it is spent the volume stays `Failed` with
    /// that reason and leaves supervision. The daemon used to try a volume
    /// once, at startup, and never again.
    #[test]
    fn a_volume_that_cannot_be_opened_is_retried_then_left_failed() {
        let (mut fleet, volumes) = fleet();
        fleet.reconcile(&[listed(C, true, true)], Instant::now());
        let state = volumes.current()[0].clone();
        let opening_failed = || {
            matches!(&*state.phase.read(),
                VolumePhase::Failed { reason } if reason.starts_with("opening the volume"))
        };
        let s = &mut fleet.supervised[0];
        // The worker reports the failure itself, before any scan polls it.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !opening_failed() {
            assert!(Instant::now() < deadline, "got {:?}", *state.phase.read());
            std::thread::sleep(Duration::from_millis(5));
        }
        settle_start(s);
        assert!(opening_failed(), "got {:?}", *state.phase.read());
        assert_eq!(s.revive_attempts, 0, "the first start is free");

        for attempt in 1..=MAX_REVIVES {
            assert!(s.scan(NOW), "retried while budget lasts");
            assert_eq!(s.revive_attempts, attempt);
            assert!(s.pending.is_some());
            settle_start(s);
            assert!(opening_failed());
        }
        assert!(!s.scan(NOW), "given up once the budget is spent");
        assert!(opening_failed(), "keeps the reason it failed for");
        assert_eq!(indexed(&volumes), [C], "and stays reported");
    }

    /// Giving up on a volume never indexed fails it instead of leaving it
    /// `Bootstrapping` forever, and one still claiming to track is corrected;
    /// a phase its tail left with a reason is kept.
    #[test]
    fn giving_up_never_leaves_a_volume_claiming_progress() {
        for phase in [
            VolumePhase::Bootstrapping,
            VolumePhase::Live,
            VolumePhase::Rescanning,
        ] {
            assert!(
                matches!(given_up(&phase), Some(VolumePhase::Failed { .. })),
                "{phase:?}"
            );
        }
        for phase in [
            VolumePhase::Snapshot,
            VolumePhase::Offline,
            VolumePhase::Failed {
                reason: "kept".into(),
            },
        ] {
            assert_eq!(given_up(&phase), None, "{phase:?}");
        }
    }
    fn beat_ago(secs: u64) -> Pulse {
        Pulse::Beat(NOW - secs)
    }
    fn busy_for(secs: u64) -> Pulse {
        Pulse::Busy(NOW - secs)
    }

    /// The decision table, exhaustively. This is the fix for the bug where
    /// the watchdog could not tell a wedged tail from an exited one: each
    /// pulse state maps to exactly one family of actions, and Retired never
    /// reaches CancelIo.
    #[test]
    fn assess_decides_by_pulse_state() {
        // A fresh beat is left alone.
        assert_eq!(assess(beat_ago(0), NOW, 0, false, false), Action::Nothing);
        // A beat inside the stall window is left alone even at the edge.
        let edge = BEAT_STALL.as_secs() - 1;
        assert_eq!(
            assess(beat_ago(edge), NOW, 0, false, false),
            Action::Nothing
        );
        // Past the window: presumed wedged, cancel.
        let past = BEAT_STALL.as_secs();
        assert_eq!(
            assess(beat_ago(past), NOW, 0, false, false),
            Action::CancelIo { stalled_secs: past }
        );
        // Busy is exempt where a beat would already have been shot...
        assert_eq!(
            assess(busy_for(past), NOW, 0, false, false),
            Action::Nothing
        );
        // ...but not forever: a wedged rebuild is still a wedge.
        let long = BUSY_STALL.as_secs();
        assert_eq!(
            assess(busy_for(long), NOW, 0, false, false),
            Action::CancelIo { stalled_secs: long }
        );
        // Retired is never a cancel target, however long ago it happened:
        // it is revived while budget lasts and given up on after.
        assert_eq!(assess(Pulse::Retired, NOW, 0, false, false), Action::Revive);
        assert_eq!(
            assess(Pulse::Retired, NOW, MAX_REVIVES - 1, false, false),
            Action::Revive
        );
        assert_eq!(
            assess(Pulse::Retired, NOW, MAX_REVIVES, false, false),
            Action::GiveUp
        );
    }

    /// A tail that retired on a snapshot volume (a rescan found the volume
    /// refuses a journal) is released, with budget left or spent: a revival
    /// would rebuild the index only to be refused again, and giving up would
    /// leave an error for a volume that is fine. A running tail is judged by
    /// its pulse alone.
    #[test]
    fn a_retired_snapshot_is_released_not_revived() {
        assert_eq!(assess(Pulse::Retired, NOW, 0, false, true), Action::Release);
        assert_eq!(
            assess(Pulse::Retired, NOW, MAX_REVIVES, false, true),
            Action::Release
        );
        let past = BEAT_STALL.as_secs();
        assert_eq!(
            assess(beat_ago(past), NOW, 0, false, true),
            Action::CancelIo { stalled_secs: past }
        );
    }

    /// The budget refund: only a beating revived tail that has proven itself
    /// gets its history erased. An original (no revivals) has nothing to
    /// refund, an unproven revival keeps its count, and a busy or stalled
    /// revival proves nothing.
    #[test]
    fn budget_refund_requires_a_proven_healthy_beat() {
        assert_eq!(
            assess(beat_ago(0), NOW, 1, true, false),
            Action::ResetBudget
        );
        assert_eq!(assess(beat_ago(0), NOW, 0, true, false), Action::Nothing);
        assert_eq!(assess(beat_ago(0), NOW, 1, false, false), Action::Nothing);
        assert_eq!(assess(busy_for(0), NOW, 1, true, false), Action::Nothing);
        // A stalled revived tail is a wedge first; the refund never masks it.
        let past = BEAT_STALL.as_secs();
        assert_eq!(
            assess(beat_ago(past), NOW, 1, true, false),
            Action::CancelIo { stalled_secs: past }
        );
    }

    /// Health accrual: only a FRESH `Beat` observed while the volume is
    /// `Live` starts or extends the run. An Offline beat is a turning retry
    /// loop that tracks nothing, and a stale beat is a loop that stopped
    /// turning (a wedge the stall threshold has not yet declared); either
    /// refunding would make give-up unreachable and revive a dead volume
    /// forever, one full MFT rebuild per cycle.
    #[test]
    fn health_accrues_only_on_fresh_live_beats() {
        // A fresh live beat starts the run and a later one preserves the
        // anchor.
        let started = accrue_health(None, beat_ago(0), true, NOW);
        assert_eq!(started, Some(NOW));
        let later = NOW + SCAN_INTERVAL.as_secs();
        assert_eq!(
            accrue_health(started, Pulse::Beat(later), true, later),
            Some(NOW)
        );
        // The same beat with the volume not Live breaks the run.
        assert_eq!(accrue_health(started, beat_ago(0), false, NOW), None);
        // A stale beat breaks it even while Live: six healthy minutes then
        // a wedge under the stall threshold must not mature into a refund,
        // so the frozen stamp cannot carry the run.
        let stale = BEAT_FRESH.as_secs();
        assert_eq!(accrue_health(started, beat_ago(stale), true, NOW), None);
        // Busy and Retired break it regardless of phase.
        assert_eq!(accrue_health(started, busy_for(0), true, NOW), None);
        assert_eq!(accrue_health(started, Pulse::Retired, true, NOW), None);
    }

    /// A scan reads the clock once, before it loads any pulse, so a tail that
    /// stamps during the scan is ahead of `now`. That must read as "not
    /// stalled", never as a huge stall that shoots a healthy tail.
    #[test]
    fn a_stamp_newer_than_now_never_cancels() {
        assert_eq!(
            assess(Pulse::Beat(NOW + 600), NOW, 0, false, false),
            Action::Nothing
        );
        assert_eq!(
            assess(Pulse::Busy(NOW + 600), NOW, 0, false, false),
            Action::Nothing
        );
    }

    /// The stall threshold must comfortably exceed the tail's longest
    /// legitimate silence: a retry sleeping out its full backoff, observed a
    /// scan interval late. Shrinking either constant past the other would
    /// shoot healthy tails mid-backoff.
    #[test]
    fn beat_stall_tolerates_a_full_backoff_sleep() {
        assert!(
            BEAT_STALL >= tail::RETRY_BACKOFF_MAX + SCAN_INTERVAL + SCAN_INTERVAL,
            "a tail sleeping out RETRY_BACKOFF_MAX must never look stalled"
        );
        // And busy tolerance dwarfs beat tolerance: a rebuild is minutes of
        // legitimate silence, not seconds.
        assert!(BUSY_STALL > BEAT_STALL);
        // Freshness must cover one scan interval (an observed beat is up to
        // one scan old on a healthy tail) yet declare staleness well before
        // the stall threshold, or the refund window reopens.
        assert!(BEAT_FRESH >= SCAN_INTERVAL);
        assert!(BEAT_FRESH < BEAT_STALL);
        // Shutdown must outwait the supervisor's own legitimate worst case,
        // a scan waiting for its listing, then draining pending starts and
        // stopping its tails, with slack; anything tighter would abandon a
        // supervisor that was merely busy.
        assert!(
            SUPERVISOR_JOIN_WAIT
                >= LISTING_WAIT + TEARDOWN_WAIT + PENDING_DRAIN_WAIT + TEARDOWN_WAIT
        );
        // A scan waits for its listing far less than a scan interval, so a
        // blocked listing cannot stretch the scans that rescue other tails.
        assert!(LISTING_WAIT * 10 <= SCAN_INTERVAL);
        // A removal query outwaits letting go of its volume behind a listing
        // wait, or the removal would go ahead with the handle still open.
        assert!(RELEASE_WAIT >= LISTING_WAIT + PENDING_DRAIN_WAIT + TEARDOWN_WAIT);
        // A zero budget would turn every retirement into instant give-up and
        // make revival dead code.
        const { assert!(MAX_REVIVES > 0) };
    }
}
