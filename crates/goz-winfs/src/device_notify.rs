//! Removal notifications for the device a volume handle is open on.
//!
//! Windows refuses to remove a device while handles to it are open, and the
//! daemon keeps each indexed volume open, so an indexed external disk could
//! not be safely removed while gozd ran. A handle notification
//! (`CM_Register_Notification` on the volume handle) tells the holder when a
//! removal is asked for, in time to close its handles, and how the removal
//! ended. The callback runs on a system thread pool, the same for the service
//! and for a console run, and the removal waits while it blocks.
//!
//! See [Registering for device notification](https://learn.microsoft.com/windows/win32/devio/registering-for-device-notification)
//! and [`CM_Register_Notification`](https://learn.microsoft.com/windows/win32/api/cfgmgr32/nf-cfgmgr32-cm_register_notification).

use core::ffi::c_void;

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_MapCrToWin32Err, CM_NOTIFY_ACTION, CM_NOTIFY_ACTION_DEVICEQUERYREMOVE,
    CM_NOTIFY_ACTION_DEVICEQUERYREMOVEFAILED, CM_NOTIFY_ACTION_DEVICEREMOVECOMPLETE,
    CM_NOTIFY_ACTION_DEVICEREMOVEPENDING, CM_NOTIFY_EVENT_DATA, CM_NOTIFY_FILTER,
    CM_NOTIFY_FILTER_TYPE_DEVICEHANDLE, CM_Register_Notification, CM_Unregister_Notification,
    CR_SUCCESS, HCMNOTIFICATION,
};
use windows_sys::Win32::Foundation::{ERROR_GEN_FAILURE, ERROR_SUCCESS};

use crate::error::WinError;
use crate::volume::VolumeHandle;

/// What Windows says about the device a [`DeviceNotification`] watches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceEvent {
    /// A removal is asked for: every handle on the device must be closed
    /// before the callback returns, or the removal fails.
    QueryRemove,
    /// The removal asked for did not happen; another holder kept the device.
    QueryRemoveFailed,
    /// The device is going or gone, whether a removal was asked for or the
    /// disk was pulled.
    Removed,
}

/// The callback a registration calls, boxed so its address is stable.
type Callback = Box<dyn Fn(DeviceEvent) + Send + Sync>;

/// A registered removal notification, unregistered on drop.
///
/// Never drop one inside its own callback: unregistering waits for the
/// callbacks in flight, so it would wait for itself.
pub struct DeviceNotification {
    handle: HCMNOTIFICATION,
    /// What the system calls through the registration's context. Freed only
    /// after unregistering, which returns once no call is in flight.
    callback: *mut Callback,
}

// SAFETY: the registration handle may be unregistered from any thread, and
// the callback it owns is `Send + Sync`.
unsafe impl Send for DeviceNotification {}

impl DeviceNotification {
    /// Calls `on_event` for each removal event of the device `volume` is open
    /// on. The registration outlives the handle: once the handle is closed
    /// for a removal, the outcome still arrives.
    pub fn register(
        volume: &VolumeHandle,
        on_event: impl Fn(DeviceEvent) + Send + Sync + 'static,
    ) -> Result<Self, WinError> {
        let callback: *mut Callback = Box::into_raw(Box::new(Box::new(on_event)));
        let mut filter = CM_NOTIFY_FILTER {
            cbSize: size_of::<CM_NOTIFY_FILTER>() as u32,
            FilterType: CM_NOTIFY_FILTER_TYPE_DEVICEHANDLE,
            ..Default::default()
        };
        filter.u.DeviceHandle.hTarget = volume.as_handle();
        let mut handle: HCMNOTIFICATION = core::ptr::null_mut();
        // SAFETY: `filter` is a fully initialized device-handle filter on a
        // live volume handle, `callback` stays valid until the registration
        // is unregistered (see `Drop`), and `handle` is a live out-param.
        let cr = unsafe {
            CM_Register_Notification(
                &filter,
                callback as *const c_void,
                Some(on_notify),
                &mut handle,
            )
        };
        if cr != CR_SUCCESS {
            // SAFETY: the registration failed, so the system kept no copy of
            // `callback`; this is its only owner.
            drop(unsafe { Box::from_raw(callback) });
            // SAFETY: plain FFI on a CONFIGRET value.
            let code = unsafe { CM_MapCrToWin32Err(cr, ERROR_GEN_FAILURE) };
            return Err(WinError {
                code,
                context: "CM_Register_Notification",
            });
        }
        Ok(Self { handle, callback })
    }
}

impl Drop for DeviceNotification {
    fn drop(&mut self) {
        // SAFETY: `handle` came from a successful registration and is
        // unregistered exactly once, here. The call returns only once no
        // callback for it is running, so the box is freed after its last use.
        unsafe {
            CM_Unregister_Notification(self.handle);
            drop(Box::from_raw(self.callback));
        }
    }
}

/// The registration's callback: maps the action and calls the boxed
/// callback. Returns `ERROR_SUCCESS` to every action, which grants a query:
/// the callback has let go of the device by the time it returns.
unsafe extern "system" fn on_notify(
    _notification: HCMNOTIFICATION,
    context: *const c_void,
    action: CM_NOTIFY_ACTION,
    _data: *const CM_NOTIFY_EVENT_DATA,
    _size: u32,
) -> u32 {
    let event = match action {
        CM_NOTIFY_ACTION_DEVICEQUERYREMOVE => DeviceEvent::QueryRemove,
        CM_NOTIFY_ACTION_DEVICEQUERYREMOVEFAILED => DeviceEvent::QueryRemoveFailed,
        CM_NOTIFY_ACTION_DEVICEREMOVEPENDING | CM_NOTIFY_ACTION_DEVICEREMOVECOMPLETE => {
            DeviceEvent::Removed
        }
        _ => return ERROR_SUCCESS,
    };
    // SAFETY: `context` is the callback box `register` passed, alive until
    // unregistering returns, which waits for this call to finish.
    let callback = unsafe { &*(context as *const Callback) };
    // A panic must not unwind into the system's thread pool.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(event)));
    ERROR_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file handle identifies its volume's device as well, so the
    /// registration works without elevation: it is made and undone, and
    /// nothing is called without an event.
    #[test]
    fn registers_on_a_handle_and_unregisters_on_drop() {
        let file = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
        let handle = VolumeHandle::from_file(file);
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = calls.clone();
        let notification = DeviceNotification::register(&handle, move |_| {
            counted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        })
        .expect("registering on a file handle");
        drop(notification);
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 0);
    }
}
