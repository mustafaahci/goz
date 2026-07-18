//! Windows service integration (M8).
//!
//! `install` registers `gozd` as a LocalSystem auto-start service so the index
//! is warm from boot for every user (admin or standard) with no per-session
//! elevation. `uninstall` stops and removes it. `run` is the SCM entry point:
//! Windows launches `gozd.exe run --service`, which hands the process to the
//! service dispatcher and reports `StartPending` (with a heartbeat checkpoint,
//! because bootstrap takes tens of seconds) → `Running` → `Stopped`.

use std::ffi::OsString;
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

/// Service key name (the SCM/registry key). Keep stable across versions.
/// (The pipe name is independent: `goz_core::proto::PIPE_NAME`.)
pub(crate) const SERVICE_NAME: &str = "goz";
const DISPLAY_NAME: &str = "goz file index";
const DESCRIPTION: &str =
    "Indexes NTFS volumes (MFT + USN journal) for instant file search in arsivle.";
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

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
    // Ready signal: engine thread → this thread once bootstrap finishes.
    let (ready_tx, ready_rx) = mpsc::channel::<Result<usize, String>>();

    // The status handle isn't available until `register` returns, but the
    // control handler (which fires only after startup) needs it. A OnceLock
    // bridges the two safely.
    let handle_cell: Arc<OnceLock<ServiceStatusHandle>> = Arc::new(OnceLock::new());
    let handler_cell = handle_cell.clone();

    let event_handler = move |control: ServiceControl| -> ServiceControlHandlerResult {
        match control {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                if let Some(handle) = handler_cell.get() {
                    let _ = handle.set_service_status(status_report(
                        ServiceState::StopPending,
                        ServiceControlAccept::empty(),
                        1,
                        Duration::from_secs(10),
                    ));
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

    // Report StartPending right away; bootstrap can take tens of seconds.
    status_handle.set_service_status(status_report(
        ServiceState::StartPending,
        ServiceControlAccept::empty(),
        1,
        Duration::from_secs(4),
    ))?;

    // Build the index on a dedicated thread, then serve until stopped. The SCM
    // thread stays free to pump checkpoints and receive Stop.
    let engine_thread = std::thread::spawn(move || {
        let engine = match run::build_engine() {
            Ok(engine) => engine,
            Err(e) => {
                let _ = ready_tx.send(Err(e.to_string()));
                return;
            }
        };
        let _ = ready_tx.send(Ok(engine.total_entries));
        if let Err(e) = server::run(engine.volumes, stop_rx) {
            tracing::error!(error = %e, "pipe server error");
        }
        engine.supervisor.shutdown();
    });

    // Heartbeat StartPending until the engine reports ready (or fails).
    let mut checkpoint = 1u32;
    let ready = loop {
        match ready_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(result) => break result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                checkpoint += 1;
                status_handle.set_service_status(status_report(
                    ServiceState::StartPending,
                    ServiceControlAccept::empty(),
                    checkpoint,
                    Duration::from_secs(4),
                ))?;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break Err("engine thread exited before signalling ready".to_string());
            }
        }
    };

    match ready {
        Ok(entries) => {
            tracing::info!(entries, "index ready; service running");
            status_handle.set_service_status(status_report(
                ServiceState::Running,
                ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
                0,
                Duration::default(),
            ))?;
            run::settle_working_set();
        }
        Err(msg) => {
            tracing::error!(reason = %msg, "goz service failed to start");
            status_handle
                .set_service_status(stopped_report(ServiceExitCode::ServiceSpecific(1)))?;
            let _ = engine_thread.join();
            return Err(anyhow::anyhow!(msg));
        }
    }

    // Block until the engine thread returns; it does so once the server stops
    // after the Stop control flips `stop_tx`.
    let _ = engine_thread.join();

    tracing::info!("goz service stopped");
    status_handle.set_service_status(stopped_report(ServiceExitCode::Win32(0)))?;
    Ok(())
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
/// `%ProgramData%\goz\logs\gozd.log`; any failure skips logging.
fn init_service_tracing() {
    let Ok(program_data) = std::env::var("ProgramData") else {
        return;
    };
    let dir = std::path::Path::new(&program_data).join("goz").join("logs");
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
        .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
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
