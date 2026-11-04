//! The state of a Windows service, as the Service Control Manager reports it.
//!
//! The client asks it why the daemon's pipe is missing, so it can print the
//! command that fixes that state. Reading a service's status and
//! configuration needs no elevation.

use core::mem::size_of;
use core::ptr;

use windows_sys::Win32::Foundation::ERROR_SERVICE_DOES_NOT_EXIST;
use windows_sys::Win32::System::Services::{
    CloseServiceHandle, OpenSCManagerW, OpenServiceW, QUERY_SERVICE_CONFIGW, QueryServiceConfigW,
    QueryServiceStatus, SC_HANDLE, SC_MANAGER_CONNECT, SERVICE_CONTINUE_PENDING, SERVICE_DISABLED,
    SERVICE_PAUSE_PENDING, SERVICE_PAUSED, SERVICE_QUERY_CONFIG, SERVICE_QUERY_STATUS,
    SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STOP_PENDING, SERVICE_STOPPED,
};

use crate::error::{WinError, last_error};
use crate::util::to_wide_nul;

/// What the Service Control Manager reports about one service.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceState {
    /// No service has the name.
    NotInstalled,
    /// Stopped, and its start type is disabled, so it cannot be started.
    Disabled,
    /// Stopped.
    Stopped,
    /// Starting, or continuing from a pause.
    Starting,
    Running,
    Stopping,
    /// Paused, or pausing.
    Paused,
}

/// The state of the local service named `name`.
///
/// # Errors
/// Returns [`WinError`] if the Service Control Manager cannot be opened or
/// queried. A service that does not exist is [`ServiceState::NotInstalled`],
/// not an error.
pub fn service_state(name: &str) -> Result<ServiceState, WinError> {
    // SAFETY: null machine and database names select the local machine's
    // active services database; the call reads no other input.
    let manager = unsafe { OpenSCManagerW(ptr::null(), ptr::null(), SC_MANAGER_CONNECT) };
    if manager.is_null() {
        return Err(last_error("OpenSCManagerW"));
    }
    let manager = ScHandle(manager);

    let wide = to_wide_nul(name);
    // SAFETY: `manager` is an open SCM handle, and `wide` is a NUL-terminated
    // UTF-16 name that outlives the call.
    let service = unsafe {
        OpenServiceW(
            manager.0,
            wide.as_ptr(),
            SERVICE_QUERY_STATUS | SERVICE_QUERY_CONFIG,
        )
    };
    if service.is_null() {
        let error = last_error("OpenServiceW");
        return if error.code == ERROR_SERVICE_DOES_NOT_EXIST {
            Ok(ServiceState::NotInstalled)
        } else {
            Err(error)
        };
    }
    let service = ScHandle(service);

    let mut status = SERVICE_STATUS::default();
    // SAFETY: `service` is open with SERVICE_QUERY_STATUS, and `status` is a
    // live, writable SERVICE_STATUS.
    if unsafe { QueryServiceStatus(service.0, &mut status) } == 0 {
        return Err(last_error("QueryServiceStatus"));
    }
    Ok(match status.dwCurrentState {
        SERVICE_STOPPED if start_type(&service)? == SERVICE_DISABLED => ServiceState::Disabled,
        SERVICE_STOPPED => ServiceState::Stopped,
        SERVICE_START_PENDING | SERVICE_CONTINUE_PENDING => ServiceState::Starting,
        SERVICE_RUNNING => ServiceState::Running,
        SERVICE_STOP_PENDING => ServiceState::Stopping,
        SERVICE_PAUSE_PENDING | SERVICE_PAUSED => ServiceState::Paused,
        // The SCM defines no other state; read an unknown one as in flux.
        _ => ServiceState::Starting,
    })
}

/// The start type of `service`, such as `SERVICE_DISABLED`.
fn start_type(service: &ScHandle) -> Result<u32, WinError> {
    // QueryServiceConfigW documents 8 KiB as the most it writes, so one
    // buffer of that size always suffices. `u64` elements align it for the
    // QUERY_SERVICE_CONFIGW at its start.
    const BYTES: usize = 8 * 1024;
    let mut buffer = vec![0u64; BYTES / size_of::<u64>()];
    let mut needed = 0u32;
    // SAFETY: `service` is open with SERVICE_QUERY_CONFIG; `buffer` is live,
    // writable, aligned for QUERY_SERVICE_CONFIGW and `BYTES` long; `needed`
    // is a live out parameter.
    let ok = unsafe {
        QueryServiceConfigW(
            service.0,
            buffer.as_mut_ptr().cast::<QUERY_SERVICE_CONFIGW>(),
            BYTES as u32,
            &mut needed,
        )
    };
    if ok == 0 {
        return Err(last_error("QueryServiceConfigW"));
    }
    // SAFETY: the call succeeded, so the buffer starts with the
    // QUERY_SERVICE_CONFIGW it wrote, and the buffer is aligned for it.
    Ok(unsafe { (*buffer.as_ptr().cast::<QUERY_SERVICE_CONFIGW>()).dwStartType })
}

/// A Service Control Manager handle, closed on drop.
struct ScHandle(SC_HANDLE);

impl Drop for ScHandle {
    fn drop(&mut self) {
        // SAFETY: the handle was opened by this module, is not null, and is
        // closed once, here.
        unsafe { CloseServiceHandle(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::{ServiceState, service_state};

    /// A missing service is a state, not an error, so the client can say
    /// how to install it.
    #[test]
    fn a_service_that_does_not_exist_is_not_installed() {
        assert_eq!(
            service_state("goz-test-no-such-service").expect("any user may ask the SCM"),
            ServiceState::NotInstalled
        );
    }

    /// The RPC service runs on every Windows machine and cannot be stopped.
    #[test]
    fn a_running_service_is_running() {
        assert_eq!(
            service_state("RpcSs").expect("any user may query RpcSs"),
            ServiceState::Running
        );
    }
}
