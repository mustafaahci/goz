//! Windows service integration (M8).
//!
//! `install` registers `gozd` as a LocalSystem auto-start service so the index
//! is warm from boot for every user (admin or standard) with no per-session
//! elevation. `uninstall` stops and removes it. `run` is the SCM entry point:
//! Windows launches `gozd.exe run --service`, which hands the process to the
//! service dispatcher and reports `StartPending` (with a heartbeat checkpoint)
//! → `Running` → `StopPending` (with one too) → `Stopped`. `Running` comes
//! once the volumes are listed and the pipe is claimed, before any volume is
//! indexed; results are flagged incomplete until each volume's first build is
//! published.

use std::ffi::OsString;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::watch;
use windows_service::service::{
    ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept,
    ServiceErrorControl, ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{
    self, ServiceControlHandlerResult, ServiceStatusHandle,
};
use windows_service::service_dispatcher;
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use crate::{run, server};
use goz_core::proto::SERVICE_NAME;

const DISPLAY_NAME: &str = "goz file index";
const DESCRIPTION: &str = "Indexes NTFS volumes (MFT + USN journal) for instant file search.";
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

/// How often a start or a stop under way reports a fresh checkpoint.
const PENDING_HEARTBEAT: Duration = Duration::from_secs(2);

/// The wait hint of every pending report: two heartbeats, so one late report
/// never reads as a hang.
const PENDING_WAIT_HINT: Duration = Duration::from_secs(4);

// Win32 error codes we treat as non-fatal for idempotent install/uninstall.
const ERROR_SERVICE_EXISTS: i32 = 1073;
const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;
const ERROR_SERVICE_ALREADY_RUNNING: i32 = 1056;

/// `ServiceInfo` describing how the SCM should launch us. `current_exe()`
/// resolves to the installed `gozd.exe`, and `run --service` is the argv the
/// SCM starts it with.
fn service_info() -> Result<ServiceInfo> {
    let exe = std::env::current_exe().context("resolving the gozd.exe path")?;
    Ok(ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(DISPLAY_NAME),
        service_type: SERVICE_TYPE,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe,
        launch_arguments: vec![OsString::from("run"), OsString::from("--service")],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    })
}

/// Registers the auto-start service and starts it now, so search works
/// immediately after install without a reboot. Idempotent: re-running refreshes
/// the config (e.g. after an app upgrade moved the binary) and ensures it runs.
pub(crate) fn install() -> Result<()> {
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .context("opening the service control manager (are you elevated?)")?;

    let info = service_info()?;
    let access = ServiceAccess::CHANGE_CONFIG | ServiceAccess::START | ServiceAccess::QUERY_STATUS;

    let service = match manager.create_service(&info, access) {
        Ok(service) => service,
        Err(windows_service::Error::Winapi(e))
            if e.raw_os_error() == Some(ERROR_SERVICE_EXISTS) =>
        {
            // Already installed → open and refresh config (upgrade-safe).
            let service = manager
                .open_service(SERVICE_NAME, access)
                .context("opening the existing goz service")?;
            service
                .change_config(&info)
                .context("updating the goz service configuration")?;
            service
        }
        Err(e) => return Err(e).context("creating the goz service"),
    };

    // Best-effort description; not worth failing the install over.
    let _ = service.set_description(DESCRIPTION);
    // Let the SCM supervise crash recovery: restart on failure with backoff,
    // so a dead daemon self-heals without app involvement or a UAC prompt.
    let _ = set_failure_actions(&service);
    start_if_stopped(&service).context("starting the goz service")?;
    println!("goz service installed and started.");
    Ok(())
}

/// Stops (if running) and deletes the service. Succeeds even if it was never
/// installed, so an installer's uninstall step is safe to run unconditionally.
pub(crate) fn uninstall() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("opening the service control manager (are you elevated?)")?;

    let service = match manager.open_service(
        SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
    ) {
        Ok(service) => service,
        Err(windows_service::Error::Winapi(e))
            if e.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST) =>
        {
            println!("goz service is not installed; nothing to remove.");
            return Ok(());
        }
        Err(e) => return Err(e).context("opening the goz service"),
    };

    // Stop first so the record is deleted immediately rather than being marked
    // for deletion until the process exits.
    if let Ok(status) = service.query_status()
        && status.current_state != ServiceState::Stopped
    {
        let _ = service.stop();
        wait_for_stopped(&service);
    }

    service.delete().context("deleting the goz service")?;
    println!("goz service stopped and removed.");
    Ok(())
}

/// Prints the service's install/run state (for the installer/app to probe).
pub(crate) fn status() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("opening the service control manager")?;
    match manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS) {
        Ok(service) => {
            let status = service.query_status().context("querying the goz service")?;
            println!("goz service: {:?}", status.current_state);
            Ok(())
        }
        Err(windows_service::Error::Winapi(e))
            if e.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST) =>
        {
            println!("goz service: NotInstalled");
            Ok(())
        }
        Err(e) => Err(e).context("opening the goz service"),
    }
}

/// Configures SCM crash recovery: restart the service on failure with backoff
/// (5s, 30s, then 60s for subsequent failures), resetting the failure count
/// after a day of health. `on_non_crash_failures` also covers a clean exit with
/// a non-zero code. Best-effort: a machine policy may forbid it.
fn set_failure_actions(service: &windows_service::service::Service) -> Result<()> {
    let actions = ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(86_400)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![
            ServiceAction {
                action_type: ServiceActionType::Restart,
                delay: Duration::from_secs(5),
            },
            ServiceAction {
                action_type: ServiceActionType::Restart,
                delay: Duration::from_secs(30),
            },
            ServiceAction {
                action_type: ServiceActionType::Restart,
                delay: Duration::from_secs(60),
            },
        ]),
    };
    service.update_failure_actions(actions)?;
    service.set_failure_actions_on_non_crash_failures(true)?;
    Ok(())
}

/// Starts the service unless it is already running/starting.
fn start_if_stopped(service: &windows_service::service::Service) -> Result<()> {
    let already_up = service
        .query_status()
        .map(|s| {
            matches!(
                s.current_state,
                ServiceState::Running | ServiceState::StartPending
            )
        })
        .unwrap_or(false);
    if already_up {
        return Ok(());
    }
    match service.start::<&std::ffi::OsStr>(&[]) {
        Ok(()) => Ok(()),
        Err(windows_service::Error::Winapi(e))
            if e.raw_os_error() == Some(ERROR_SERVICE_ALREADY_RUNNING) =>
        {
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// Polls until the service reports `Stopped` or a short deadline elapses.
fn wait_for_stopped(service: &windows_service::service::Service) {
    for _ in 0..50 {
        match service.query_status() {
            Ok(s) if s.current_state == ServiceState::Stopped => return,
            Ok(_) => std::thread::sleep(Duration::from_millis(200)),
            Err(_) => return,
        }
    }
}

/// SCM entry point (`gozd run --service`): hands the process to the service
/// dispatcher. Blocks until the service stops.
pub(crate) fn run() -> Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .context("starting the service dispatcher (is gozd being launched by the SCM?)")?;
    Ok(())
}

windows_service::define_windows_service!(ffi_service_main, service_main);

/// The generated dispatcher calls this on a dedicated thread once the SCM
/// connects. Errors are logged (no console exists in service mode).
fn service_main(_args: Vec<OsString>) {
    if let Err(e) = run_service() {
        tracing::error!(error = %e, "goz service exited with an error");
    }
}

fn run_service() -> Result<()> {
    init_service_tracing();
    tracing::info!("goz service starting");

    // Stop signal: SCM control handler → async pipe server. `watch` retains the
    // last value, so a Stop that races ahead of the server is never lost.
    let (stop_tx, stop_rx) = watch::channel(false);
    // Ready signal: engine thread → this thread once the pipe server is ready
    // (see `server::run`), or why it never got there.
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();

    // The status handle isn't available until `register` returns, but the
    // control handler (which fires only after startup) needs it. A OnceLock
    // bridges the two safely.
    let handle_cell: Arc<OnceLock<ServiceStatusHandle>> = Arc::new(OnceLock::new());
    let handler_cell = handle_cell.clone();

    // Set by the Stop control, and by the engine once its pipe server ends
    // for any reason; from then on this thread reports StopPending until the
    // engine thread ends. The checkpoint is shared so it only ever grows.
    let stopping = Arc::new(AtomicBool::new(false));
    let checkpoint = Arc::new(AtomicU32::new(0));
    let handler_stopping = stopping.clone();
    let handler_checkpoint = checkpoint.clone();
    let engine_stopping = stopping.clone();

    let event_handler = move |control: ServiceControl| -> ServiceControlHandlerResult {
        match control {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                handler_stopping.store(true, Ordering::Release);
                if let Some(handle) = handler_cell.get() {
                    let _ = handle.set_service_status(stop_pending(&handler_checkpoint));
                }
                let _ = stop_tx.send(true);
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)
        .context("registering the service control handler")?;
    let _ = handle_cell.set(status_handle);

    // Report StartPending right away; listing the volumes can take a moment
    // while a disk wakes up.
    status_handle.set_service_status(status_report(
        ServiceState::StartPending,
        ServiceControlAccept::empty(),
        1,
        PENDING_WAIT_HINT,
    ))?;

    // Start indexing and serve on a dedicated thread until stopped. The SCM
    // thread stays free to pump checkpoints and receive Stop.
    let engine_thread = std::thread::spawn(move || -> Result<()> {
        let served = serve(stop_rx, &ready_tx, &engine_stopping);
        // Ends StartPending with the reason when the failure came first; once
        // Running was reported, the stop wait reads past it.
        if let Err(e) = &served {
            let _ = ready_tx.send(Err(format!("{e:#}")));
        }
        served
    });

    // Heartbeat StartPending until the engine reports ready (or fails).
    let mut start_checkpoint = 1u32;
    let ready = loop {
        match ready_rx.recv_timeout(PENDING_HEARTBEAT) {
            Ok(result) => break result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                start_checkpoint += 1;
                status_handle.set_service_status(status_report(
                    ServiceState::StartPending,
                    ServiceControlAccept::empty(),
                    start_checkpoint,
                    PENDING_WAIT_HINT,
                ))?;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break Err("engine thread exited before signalling ready".to_string());
            }
        }
    };

    match ready {
        Ok(()) => {
            tracing::info!("service running; serving queries while the volumes index");
            status_handle.set_service_status(status_report(
                ServiceState::Running,
                ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
                0,
                Duration::default(),
            ))?;
        }
        Err(msg) => {
            tracing::error!(reason = %msg, "goz service failed to start");
            status_handle
                .set_service_status(stopped_report(ServiceExitCode::ServiceSpecific(1)))?;
            let _ = engine_thread.join();
            return Err(anyhow::anyhow!(msg));
        }
    }

    // Wait for the engine thread, which ends after the Stop control flips
    // `stop_tx`, or when the pipe server fails or the thread panics. Its
    // teardown can take the supervisor's whole bounded wait, so the stop
    // reports a fresh checkpoint every heartbeat rather than one wait hint
    // for all of it.
    wait_for_engine(&ready_rx, &stopping, PENDING_HEARTBEAT, || {
        let _ = status_handle.set_service_status(stop_pending(&checkpoint));
    });
    let exit_code = service_exit_code(engine_thread.join());
    tracing::info!(?exit_code, "goz service stopped");
    status_handle.set_service_status(stopped_report(exit_code))?;
    Ok(())
}

/// Starts the engine and serves until stopped, sending `Ok` on `ready` once
/// the pipe server is ready to report `Running` (see [`server::run`]), and
/// setting `stopping` once it ends, before the teardown.
fn serve(
    stop: watch::Receiver<bool>,
    ready: &mpsc::Sender<Result<(), String>>,
    stopping: &AtomicBool,
) -> Result<()> {
    let engine = run::start_engine()?;
    let ready = ready.clone();
    let served = server::run(engine.volumes, stop, move || {
        let _ = ready.send(Ok(()));
    });
    stopping.store(true, Ordering::Release);
    engine.supervisor.shutdown();
    served
}

/// Waits until the engine thread ends, which disconnects `ended`, calling
/// `report` every `heartbeat` while `stopping` is set.
fn wait_for_engine<T>(
    ended: &mpsc::Receiver<T>,
    stopping: &AtomicBool,
    heartbeat: Duration,
    mut report: impl FnMut(),
) {
    loop {
        match ended.recv_timeout(heartbeat) {
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
            // A failure sent after Running changes nothing here: the flag
            // says whether the teardown runs.
            Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {
                if stopping.load(Ordering::Acquire) {
                    report();
                }
            }
        }
    }
}

/// A `StopPending` report carrying the next value of `checkpoint`.
fn stop_pending(checkpoint: &AtomicU32) -> ServiceStatus {
    status_report(
        ServiceState::StopPending,
        ServiceControlAccept::empty(),
        checkpoint.fetch_add(1, Ordering::Relaxed) + 1,
        PENDING_WAIT_HINT,
    )
}

/// The exit code the service stops with once the engine thread has ended.
///
/// Only a requested stop is 0. The SCM's failure actions (see
/// [`set_failure_actions`]) restart the service only on a non-zero exit, so a
/// failed pipe server or a panic must not report 0, or every client finds no
/// daemon until the next boot.
fn service_exit_code(engine: std::thread::Result<Result<()>>) -> ServiceExitCode {
    match engine {
        Ok(Ok(())) => ServiceExitCode::NO_ERROR,
        Ok(Err(e)) => {
            tracing::error!(error = %e, "pipe server failed");
            ServiceExitCode::ServiceSpecific(1)
        }
        Err(_) => {
            tracing::error!("engine thread panicked");
            ServiceExitCode::ServiceSpecific(1)
        }
    }
}

fn status_report(
    state: ServiceState,
    controls_accepted: ServiceControlAccept,
    checkpoint: u32,
    wait_hint: Duration,
) -> ServiceStatus {
    ServiceStatus {
        service_type: SERVICE_TYPE,
        current_state: state,
        controls_accepted,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint,
        wait_hint,
        process_id: None,
    }
}

fn stopped_report(exit_code: ServiceExitCode) -> ServiceStatus {
    ServiceStatus {
        service_type: SERVICE_TYPE,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code,
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    }
}

/// Best-effort file logging for service mode (no console). Writes to
/// `%SystemRoot%\Logs\goz\gozd.log`; any failure skips logging.
///
/// The log must live where only SYSTEM and Administrators can create entries.
/// Standard users may create folders in `%ProgramData%` itself and in any
/// subfolder that inherits its ACL, so a user could pre-create
/// `%ProgramData%\goz\logs` as a junction before the service first ran and make
/// this LocalSystem writer create or append to a file of their choosing.
fn init_service_tracing() {
    let Ok(system_root) = std::env::var("SystemRoot") else {
        return;
    };
    let dir = std::path::Path::new(&system_root).join("Logs").join("goz");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("gozd.log"))
    else {
        return;
    };
    let file = Arc::new(file);
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        // `RUST_LOG` in the service's environment picks what is logged, as
        // in a console run: `info,gozd::query=debug` adds a line per query
        // saying where its time went.
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(move || FileWriter(file.clone()))
        .try_init();
}

/// Adapts a shared `File` into a `MakeWriter` target (`&File: Write`).
struct FileWriter(Arc<std::fs::File>);

impl std::io::Write for FileWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // `&File: Write`; UFCS avoids needing the trait in scope.
        std::io::Write::write(&mut &*self.0, buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        std::io::Write::flush(&mut &*self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PENDING_HEARTBEAT, PENDING_WAIT_HINT, service_exit_code, stop_pending, wait_for_engine,
    };
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, mpsc};
    use std::time::Duration;
    use windows_service::service::{ServiceControlAccept, ServiceExitCode, ServiceState};

    /// Waiting for the engine reports nothing while it serves, then a fresh
    /// checkpoint every heartbeat from the stop until the engine ends, so a
    /// teardown longer than one wait hint still shows progress.
    #[test]
    fn a_stop_reports_progress_until_the_engine_ends() {
        let heartbeat = Duration::from_millis(20);
        let (tx, ended) = mpsc::channel::<()>();
        let stopping = Arc::new(AtomicBool::new(false));
        let engine_stopping = stopping.clone();
        let engine = std::thread::spawn(move || {
            std::thread::sleep(heartbeat * 5);
            engine_stopping.store(true, Ordering::Release);
            std::thread::sleep(heartbeat * 10);
            drop(tx);
        });
        let mut reports = 0;
        wait_for_engine(&ended, &stopping, heartbeat, || {
            assert!(stopping.load(Ordering::Acquire), "a report before the stop");
            reports += 1;
        });
        engine.join().unwrap();
        assert!(
            reports >= 3,
            "{reports} reports over ten heartbeats of teardown"
        );
    }

    /// Every stop report is due well inside the wait hint the last one gave,
    /// and its checkpoint grows, whichever thread sends it.
    #[test]
    fn stop_reports_grow_their_checkpoint_inside_the_wait_hint() {
        assert!(PENDING_WAIT_HINT >= PENDING_HEARTBEAT * 2);
        let checkpoint = AtomicU32::new(0);
        let first = stop_pending(&checkpoint);
        let second = stop_pending(&checkpoint);
        assert_eq!(first.current_state, ServiceState::StopPending);
        assert_eq!(first.controls_accepted, ServiceControlAccept::empty());
        assert_eq!(first.wait_hint, PENDING_WAIT_HINT);
        assert!(first.checkpoint > 0);
        assert!(second.checkpoint > first.checkpoint);
    }

    /// A requested stop exits 0; a failed pipe server or a panicked engine
    /// thread exits non-zero, which is what makes the SCM restart the service.
    #[test]
    fn only_a_requested_stop_exits_zero() {
        assert_eq!(service_exit_code(Ok(Ok(()))), ServiceExitCode::NO_ERROR);
        let failed = service_exit_code(Ok(Err(anyhow::anyhow!("pipe connect"))));
        assert_ne!(failed, ServiceExitCode::NO_ERROR);
        let panicked = std::thread::spawn(|| -> anyhow::Result<()> { panic!("engine") }).join();
        assert_ne!(service_exit_code(panicked), ServiceExitCode::NO_ERROR);
    }
}
