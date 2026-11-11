//! Daemon orchestration shared by console (`gozd run`) and service
//! (`gozd run --service`) modes: list the volumes, start indexing each of them,
//! then serve queries over the pipe while they index, until stopped.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tokio::sync::watch;

use crate::supervisor::Supervisor;
use crate::volume_state::VolumeRegistry;
use crate::{bootstrap, server};

/// The running index. Shared by console and service entry points.
pub(crate) struct Engine {
    pub volumes: Arc<VolumeRegistry>,
    /// Owns every tail thread; see [`Supervisor::shutdown`] for teardown.
    pub supervisor: Supervisor,
}

/// Lists the machine's volumes and starts indexing every fixed NTFS one,
/// returning before any is indexed.
///
/// Each volume is in the registry as `Bootstrapping` before this returns, so
/// a client that asks while the first builds run gets results flagged
/// incomplete, not "the daemon is not running", and `Hello.ready` is false
/// until every volume is built. Volumes attached later, or not ready yet, are
/// found by the supervisor's scans.
pub(crate) fn start_engine() -> Result<Engine> {
    // The index options, before any volume builds with them.
    let options = crate::config::init().get();
    tracing::info!(?options, "index options");
    let listed = bootstrap::list_volumes()?;
    if !listed.iter().any(bootstrap::indexable) {
        bail!("no fixed NTFS volumes found");
    }
    let volumes = Arc::new(VolumeRegistry::default());
    crate::config::folder_sizes_switched(&volumes.current(), options.index_folder_size);
    let supervisor =
        Supervisor::spawn(volumes.clone(), &listed).context("starting the volume supervisor")?;
    Ok(Engine {
        volumes,
        supervisor,
    })
}

/// Best-effort memory settling once startup indexing has finished, which the
/// supervisor runs one time, when no volume is `Bootstrapping`. Never fatal:
/// logs and continues.
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

/// Console entry: start indexing, serve on the pipe, and stop on Ctrl+C.
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

    let engine = start_engine()?;
    tracing::info!(
        volumes = engine.volumes.current().len(),
        "indexing; queries are served meanwhile, flagged incomplete (Ctrl+C to stop)"
    );

    // A never-triggered channel: console stop comes from Ctrl+C inside the
    // server. `_stop_tx` is held so the receiver never sees a closed channel.
    // Nothing waits on `ready` here; the server logs when it listens.
    let (_stop_tx, stop_rx) = watch::channel(false);
    let result = server::run(engine.volumes, stop_rx, || {});

    engine.supervisor.shutdown();
    result
}
