//! Per-volume bootstrap: read the MFT once through FILE_LAYOUT, falling back
//! to an MFT enumeration only when that pass stops, wiring the raw Win32
//! buffers from `goz-winfs` through the pure parsers in `goz-core` into a
//! `VolumeIndex`.
//!
//! This is the bridge the whole design turns on: `goz-winfs` hands us bytes,
//! `goz-core` interprets them, and neither knows about the other. The daemon
//! (here) owns the buffers and the loop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use goz_core::index::{FrnMap, NTFS_ROOT_FRN, VolumeIndex};
use goz_core::layout::{RECOMMENDED_LAYOUT_FLAGS, walk_layout_buffer};
use goz_core::usn::walk_enum_buffer;
use goz_winfs::{
    JournalInfo, JournalQuery, VolumeHandle, VolumeInfo, WinError, create_usn_journal,
    enum_usn_data, enumerate_volumes, query_file_layout, query_usn_journal, volume_total_bytes,
};

/// Floor for the USN journal: large enough that routine churn does not wrap it
/// before we finish bootstrapping. [`journal_target_bytes`] may ask for more.
const JOURNAL_MIN_SIZE: u64 = 64 * 1024 * 1024;
/// Ceiling for [`journal_target_bytes`]: past 1 GiB the horizon is weeks and
/// the space stops being free.
const JOURNAL_MAX_SIZE: u64 = 1024 * 1024 * 1024;
/// 8 MiB trimmed from the front when the journal is full. Kept small on
/// purpose: NTFS trims slowly, and a large delta stalls the writer.
const JOURNAL_ALLOC_DELTA: u64 = 8 * 1024 * 1024;
/// 1 MiB ioctl buffer: ~100 ENUM round-trips per million files.
const IO_BUFFER_BYTES: usize = 1024 * 1024;

/// A freshly bootstrapped volume: its index plus the retained volume handle
/// and journal cursor the daemon needs to tail it live.
pub(crate) struct BootstrappedVolume {
    pub guid_path: String,
    pub mounts: Vec<String>,
    pub layout_secs: f64,
    /// ENUM pass duration; `Some` only when the FILE_LAYOUT pass stopped and
    /// the enumeration named the rest.
    pub enum_secs: Option<f64>,
    /// The FILE_LAYOUT pass stopped part way, so the files ENUM named carry no
    /// size or date.
    pub metadata_pending: bool,
    /// What the volume's journal setup got, which decides how it starts.
    pub journal: JournalSetup,
    pub index: VolumeIndex,
    /// Kept open for the live tail (moved into the tail thread by the daemon).
    pub handle: VolumeHandle,
}

impl BootstrappedVolume {
    pub(crate) fn entries(&self) -> usize {
        self.index.len()
    }
}

/// A volume that could not be bootstrapped, carried so the daemon can still
/// represent it.
///
/// Skipping a failed volume silently is worse than reporting it Offline: every
/// honesty signal (`--status`, `Hello.ready`, each page's `volumes_incomplete`)
/// is computed by iterating the volume set, so a volume that was never inserted
/// has no phase to be non-Live and is unreportable. A query scoped to it then
/// returns zero hits with exit 0, indistinguishable from "no such file".
pub(crate) struct FailedVolume {
    pub guid_path: String,
    pub mounts: Vec<String>,
    pub reason: String,
}

/// Everything `scan_all` learned: the volumes it indexed, and the ones it could
/// not. Both halves must reach the engine.
pub(crate) struct Scan {
    pub ok: Vec<BootstrappedVolume>,
    pub failed: Vec<FailedVolume>,
}

/// Bootstraps every fixed NTFS volume. A volume that fails to bootstrap is
/// reported and returned in `failed` rather than aborting the whole run, so one
/// unreadable volume never hides the others' results and never disappears.
pub(crate) fn scan_all() -> Result<Scan> {
    // The raw volume and journal reads need these privileges; if the process is
    // not elevated the open itself fails with a clear message, so a
    // privilege-enable failure here is only a warning.
    if let Err(e) = goz_winfs::enable_volume_privileges() {
        tracing::warn!(error = %e, "could not enable volume privileges; continuing");
    }

    let volumes = enumerate_volumes().context("enumerating volumes")?;
    let targets: Vec<VolumeInfo> = volumes
        .into_iter()
        .filter(|v| v.is_fixed && v.is_ntfs)
        .collect();
    if targets.is_empty() {
        bail!("no fixed NTFS volumes found");
    }

    // One thread per volume. The MFT scans are independent, so a USB disk
    // waking from spin-down or a second SSD no longer queues behind the
    // largest volume (measured: the non-system volumes were 21% of a 36 s
    // bootstrap, all of it now overlapped). Results are collected in
    // enumeration order so the volume set is deterministic whichever thread
    // finishes first. Two large partitions on one spinning disk would
    // interleave their MFT reads; accepted, since such layouts are rare and
    // the cost is bounded by the 1 MiB ioctl buffer per seek.
    let results: Vec<Result<BootstrappedVolume>> = std::thread::scope(|s| {
        let threads: Vec<_> = targets
            .iter()
            .map(|vol| {
                let label = vol
                    .mounts
                    .first()
                    .map_or(vol.guid_path.as_str(), String::as_str);
                std::thread::Builder::new()
                    .name(format!("goz-bootstrap-{label}"))
                    .spawn_scoped(s, move || bootstrap_volume(vol))
            })
            .collect();
        threads
            .into_iter()
            .map(|t| match t {
                Ok(t) => t
                    .join()
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("bootstrap thread panicked"))),
                Err(e) => Err(anyhow::Error::from(e).context("spawning bootstrap thread")),
            })
            .collect()
    });

    let mut ok = Vec::new();
    let mut failed = Vec::new();
    for (vol, result) in targets.iter().zip(results) {
        match result {
            Ok(scan) => ok.push(scan),
            Err(e) => {
                tracing::error!(
                    volume = %vol.guid_path,
                    error = %format!("{e:#}"),
                    "volume failed to bootstrap; it will be reported Failed and serve no results"
                );
                failed.push(FailedVolume {
                    guid_path: vol.guid_path.clone(),
                    mounts: vol.mounts.clone(),
                    reason: format!("{e:#}"),
                });
            }
        }
    }
    if ok.is_empty() {
        bail!("no volume could be bootstrapped (see the log for per-volume errors)");
    }
    Ok(Scan { ok, failed })
}

/// Bootstraps a single volume: journal setup, then [`build_index`].
fn bootstrap_volume(vol: &VolumeInfo) -> Result<BootstrappedVolume> {
    let handle = goz_winfs::open_volume(&vol.guid_path).with_context(|| {
        format!(
            "opening {} (must run elevated for raw volume access)",
            vol.guid_path
        )
    })?;

    // The journal is only needed for the live tail; a one-shot scan reads the
    // MFT directly, so an unavailable journal is not fatal here.
    let journal = ensure_journal(&handle, &vol.guid_path);
    // First-time bootstrap passes no stop flag, so the rebuild can never abort.
    let built = build_index(&handle, None)?.expect("build_index cannot abort without a stop flag");

    Ok(BootstrappedVolume {
        guid_path: vol.guid_path.clone(),
        mounts: vol.mounts.clone(),
        layout_secs: built.layout_secs,
        enum_secs: built.enum_secs,
        metadata_pending: built.metadata_pending,
        journal,
        index: built.index,
        handle,
    })
}

/// A freshly built index and how its passes went.
pub(crate) struct BuiltIndex {
    pub index: VolumeIndex,
    pub layout_secs: f64,
    /// `Some` only when the FILE_LAYOUT pass stopped and ENUM ran.
    pub enum_secs: Option<f64>,
    /// The FILE_LAYOUT pass stopped, so some sizes and dates are unknown.
    pub metadata_pending: bool,
}

/// Builds a fresh [`VolumeIndex`] for an already-open volume handle. Shared by
/// first-time bootstrap and the live tail's rescan (after a USN journal
/// wrap/deletion), which rebuilds the index on the handle it already owns.
/// Returns `None` when `stop` aborts the build. The journal is the caller's
/// concern.
///
/// One FILE_LAYOUT pass yields, per file, every hard-link name with its
/// parent, the attributes, size and last-write time: everything the index
/// stores, so the MFT is read once. The ENUM pass that used to run first
/// carried only one name and parent per record and read the same MFT again
/// (4.2M entries, warm: 17.8 s for both passes, 12.2 s for FILE_LAYOUT alone).
/// It also never returned `$Extend`, so that directory's children sat under
/// lost+found forever and every query paid the presentability walk. ENUM now
/// runs only when the FILE_LAYOUT pass stops part way, into the same index, so
/// a failed read still drops no file; the files it names carry no size or
/// date. Approach measured and proposed by SynoSoftware in issue #1.
pub(crate) fn build_index(
    handle: &VolumeHandle,
    stop: Option<&AtomicBool>,
) -> Result<Option<BuiltIndex>> {
    let cancelled = || stop.is_some_and(|s| s.load(Ordering::Relaxed));
    let mut index = VolumeIndex::new(NTFS_ROOT_FRN, FrnMap::sparse());
    let mut buf = vec![0u8; IO_BUFFER_BYTES];

    // --- FILE_LAYOUT: names, links, sizes, dates in one MFT read ----------
    let layout_start = Instant::now();
    let mut restart = true;
    let mut layout_complete = true;
    loop {
        if cancelled() {
            return Ok(None); // shutdown arrived mid-rebuild: abort cooperatively
        }
        match query_file_layout(handle, RECOMMENDED_LAYOUT_FLAGS, restart, &mut buf) {
            Ok(Some(bytes)) => {
                let files =
                    walk_layout_buffer(&buf[..bytes]).context("parsing FILE_LAYOUT buffer")?;
                for f in &files {
                    index.insert_layout(f);
                }
                restart = false;
            }
            Ok(None) => break, // end of scan
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "FILE_LAYOUT pass stopped; enumerating the remaining names, whose sizes and dates stay unknown"
                );
                layout_complete = false;
                break;
            }
        }
    }
    let layout_secs = layout_start.elapsed().as_secs_f64();

    // --- ENUM fallback: name what a stopped FILE_LAYOUT pass did not reach --
    let mut enum_secs = None;
    if !layout_complete {
        let enum_start = Instant::now();
        let mut next_frn: u64 = 0;
        loop {
            if cancelled() {
                return Ok(None); // shutdown arrived mid-rebuild: abort cooperatively
            }
            let Some(bytes) =
                enum_usn_data(handle, next_frn, &mut buf).context("FSCTL_ENUM_USN_DATA")?
            else {
                break; // ERROR_HANDLE_EOF: enumeration complete
            };
            let walk = walk_enum_buffer(&buf[..bytes]).context("parsing ENUM buffer")?;
            for rec in &walk.records {
                index.insert_enum(rec);
            }
            // The kernel guarantees forward progress; guard against a stuck
            // cursor anyway so a driver quirk can never spin forever.
            if walk.next_start_frn == next_frn && walk.records.is_empty() {
                break;
            }
            next_frn = walk.next_start_frn;
        }
        enum_secs = Some(enum_start.elapsed().as_secs_f64());
    }

    // Every record has been seen: densify the FRN map, build the name
    // order, return the growth slack. Here rather than in the caller so a
    // rescan's index gets the same treatment as the first build's.
    index.optimize_storage();

    Ok(Some(BuiltIndex {
        index,
        layout_secs,
        enum_secs,
        metadata_pending: !layout_complete,
    }))
}

/// How large the USN journal on a volume should be, given the volume's total
/// size (`None` when it could not be read).
///
/// The journal is the only thing that lets a restart replay instead of
/// rescan, so its size sets the longest downtime goz can recover from without
/// a full MFT scan. Measured on a busy development C: (2 TB, ~4M entries):
/// about 4.5 MB of journal per hour, so Windows' default 32 MB holds roughly
/// nine hours and a cursor saved at night is purged by morning. The cost is
/// disk: the journal never shrinks, and its maximum is real space once the
/// volume has churned that much. Every other journal consumer (Windows Search,
/// backup tools) shares it and is only helped by a larger one.
///
/// Policy: 1/4096 of the volume, clamped to [[`JOURNAL_MIN_SIZE`], 1 GiB].
/// A 256 GB laptop disk gets 64 MiB, a 2 TB disk 512 MiB (about four days of
/// the churn above), anything from 4 TB up the 1 GiB cap. Everything leaves
/// the system's 32 MB in place and makes the user find the setting; its author
/// runs 128 MiB himself and suggests 256 MiB to 1 GiB for large busy volumes,
/// so this automates what he does by hand. `FSCTL_CREATE_USN_JOURNAL` treats a
/// value below the current maximum as a no-op, so no clamp against it is
/// needed.
fn journal_target_bytes(volume_bytes: Option<u64>) -> u64 {
    volume_bytes
        .map_or(JOURNAL_MIN_SIZE, |bytes| bytes / 4096)
        .clamp(JOURNAL_MIN_SIZE, JOURNAL_MAX_SIZE)
}

/// What [`ensure_journal`] got for a volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JournalSetup {
    /// A usable journal, and the cursor to tail it from.
    Active(JournalInfo),
    /// No journal, and the volume refused to create one: it never will.
    Refused,
    /// No usable journal right now (a failed query, a deletion in progress, a
    /// journal created but not active yet); a later attempt may get one.
    Unavailable,
}

impl JournalSetup {
    /// The cursor to tail from, if the journal is usable.
    pub(crate) fn cursor(self) -> Option<JournalInfo> {
        match self {
            JournalSetup::Active(c) => Some(c),
            JournalSetup::Refused | JournalSetup::Unavailable => None,
        }
    }
}

/// Sets up the volume's USN journal and says what it got.
///
/// Never fatal: a one-shot scan reads the MFT directly and does not need the
/// journal. An active journal smaller than [`journal_target_bytes`] is grown
/// in place; a failure to grow is an error (not a warning: it went unread as a
/// warning for months while every restart fell back to a full scan) but the
/// journal is still used at its current size. An absent journal is created on
/// the spot and used if creation succeeds. Only a create the volume refuses is
/// [`JournalSetup::Refused`]; a failed query, a deletion in progress or a new
/// journal that is not active yet is [`JournalSetup::Unavailable`], which a
/// later attempt may cure.
pub(crate) fn ensure_journal(handle: &VolumeHandle, guid_path: &str) -> JournalSetup {
    let target = journal_target_bytes(volume_total_bytes(guid_path).ok());
    setup_journal(
        target,
        || query_usn_journal(handle),
        |size| create_usn_journal(handle, size, JOURNAL_ALLOC_DELTA),
    )
}

/// [`ensure_journal`]'s decisions, with the journal query and create passed in
/// so tests can script them.
fn setup_journal(
    target: u64,
    mut query: impl FnMut() -> Result<JournalQuery, WinError>,
    mut create: impl FnMut(u64) -> Result<(), WinError>,
) -> JournalSetup {
    match query() {
        Ok(JournalQuery::Active(info)) => {
            if info.maximum_size >= target {
                return JournalSetup::Active(info);
            }
            let wanted_mib = target / (1024 * 1024);
            let actual_mib = info.maximum_size / (1024 * 1024);
            if let Err(e) = create(target) {
                tracing::error!(
                    error = %e,
                    wanted_mib,
                    actual_mib,
                    "could not grow the USN journal; at this size a restart after more than a few hours of changes must rescan the whole volume"
                );
                return JournalSetup::Active(info);
            }
            // Growing keeps the journal ID and every record, so the pre-grow
            // cursor stays valid; re-query only so the reported size is the
            // new one, and keep the old info if the re-query misbehaves.
            match query() {
                Ok(JournalQuery::Active(grown)) => {
                    let to_mib = grown.maximum_size / (1024 * 1024);
                    if grown.maximum_size > info.maximum_size {
                        tracing::info!(from_mib = actual_mib, to_mib, "grew the USN journal");
                    } else {
                        // NTFS accepted the call but kept its own ceiling: a
                        // tiny volume (recovery partitions sit at 1 MiB) or
                        // a size that only applies at the next trim.
                        tracing::debug!(
                            wanted_mib,
                            actual_mib,
                            "USN journal size unchanged; the filesystem capped it"
                        );
                    }
                    JournalSetup::Active(grown)
                }
                other => {
                    tracing::error!(
                        state = ?other,
                        "journal state changed right after growing it; keeping the earlier cursor"
                    );
                    JournalSetup::Active(info)
                }
            }
        }
        Ok(JournalQuery::NotActive) => {
            match create(target) {
                Ok(()) => match query() {
                    Ok(JournalQuery::Active(info)) => JournalSetup::Active(info),
                    // We just created a journal and it is not active. Rare, but
                    // naming it matters: a volume that should have tailed live
                    // will not until its tail can read the journal.
                    Ok(other) => {
                        tracing::warn!(
                            state = ?other,
                            "created a USN journal but it is not active; live updates unavailable"
                        );
                        JournalSetup::Unavailable
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "created a USN journal but could not re-query it; live updates unavailable"
                        );
                        JournalSetup::Unavailable
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "no USN journal and could not create one; live updates unavailable"
                    );
                    JournalSetup::Refused
                }
            }
        }
        Ok(JournalQuery::DeleteInProgress) => {
            tracing::warn!("USN journal deletion in progress; live updates unavailable");
            JournalSetup::Unavailable
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not query the USN journal; live updates unavailable");
            JournalSetup::Unavailable
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;

    #[test]
    fn journal_target_scales_with_the_volume_between_the_clamps() {
        assert_eq!(journal_target_bytes(None), JOURNAL_MIN_SIZE);
        assert_eq!(journal_target_bytes(Some(0)), JOURNAL_MIN_SIZE);
        assert_eq!(journal_target_bytes(Some(256 * GIB)), 64 * MIB);
        assert_eq!(journal_target_bytes(Some(2048 * GIB)), 512 * MIB);
        assert_eq!(journal_target_bytes(Some(4096 * GIB)), GIB);
        assert_eq!(journal_target_bytes(Some(16 * 1024 * GIB)), GIB);
    }

    const ERROR_ACCESS_DENIED: u32 = 5;
    const ERROR_WRITE_PROTECT: u32 = 19;
    const ERROR_NOT_READY: u32 = 21;

    fn info(maximum_size: u64) -> JournalInfo {
        JournalInfo {
            journal_id: 7,
            first_usn: 0,
            next_usn: 4096,
            lowest_valid_usn: 0,
            max_usn: i64::MAX,
            maximum_size,
        }
    }

    fn err(code: u32) -> WinError {
        WinError {
            code,
            context: "test",
        }
    }

    /// Runs [`setup_journal`] against scripted query answers and one create
    /// outcome; returns what it decided and how many creates it attempted.
    fn setup(
        queries: Vec<Result<JournalQuery, WinError>>,
        create: Result<(), WinError>,
    ) -> (JournalSetup, usize) {
        let mut answers = queries.into_iter();
        let mut creates = 0;
        let got = setup_journal(
            64 * MIB,
            || answers.next().expect("one journal query too many"),
            |_| {
                creates += 1;
                create.clone()
            },
        );
        (got, creates)
    }

    #[test]
    fn only_a_refused_create_means_the_volume_never_gets_a_journal() {
        use JournalQuery::{DeleteInProgress, NotActive};
        assert_eq!(
            setup(vec![Err(err(ERROR_NOT_READY))], Ok(())),
            (JournalSetup::Unavailable, 0)
        );
        assert_eq!(
            setup(vec![Ok(DeleteInProgress)], Ok(())),
            (JournalSetup::Unavailable, 0)
        );
        assert_eq!(
            setup(vec![Ok(NotActive), Ok(NotActive)], Ok(())),
            (JournalSetup::Unavailable, 1)
        );
        assert_eq!(
            setup(vec![Ok(NotActive), Err(err(ERROR_NOT_READY))], Ok(())),
            (JournalSetup::Unavailable, 1)
        );
        assert_eq!(
            setup(vec![Ok(NotActive)], Err(err(ERROR_WRITE_PROTECT))),
            (JournalSetup::Refused, 1)
        );
    }

    #[test]
    fn an_active_journal_is_used_grown_or_not() {
        use JournalQuery::{Active, NotActive};
        assert_eq!(
            setup(vec![Ok(Active(info(GIB)))], Ok(())),
            (JournalSetup::Active(info(GIB)), 0)
        );
        assert_eq!(
            setup(vec![Ok(Active(info(MIB)))], Err(err(ERROR_ACCESS_DENIED))),
            (JournalSetup::Active(info(MIB)), 1)
        );
        assert_eq!(
            setup(
                vec![Ok(Active(info(MIB))), Ok(Active(info(64 * MIB)))],
                Ok(())
            ),
            (JournalSetup::Active(info(64 * MIB)), 1)
        );
        assert_eq!(
            setup(vec![Ok(NotActive), Ok(Active(info(64 * MIB)))], Ok(())),
            (JournalSetup::Active(info(64 * MIB)), 1)
        );
    }
}
