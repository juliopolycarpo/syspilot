//! Service start types through the service control manager.

use std::ptr;

use syspilot_proto::win::wide;
use windows_sys::Win32::Foundation::ERROR_SERVICE_DOES_NOT_EXIST;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::System::Services::{
    ChangeServiceConfig2W, ChangeServiceConfigW, CloseServiceHandle, OpenSCManagerW, OpenServiceW,
    QUERY_SERVICE_CONFIGW, QueryServiceConfig2W, QueryServiceConfigW, SC_HANDLE,
    SC_MANAGER_CONNECT, SERVICE_AUTO_START, SERVICE_BOOT_START, SERVICE_CHANGE_CONFIG,
    SERVICE_CONFIG_DELAYED_AUTO_START_INFO, SERVICE_DELAYED_AUTO_START_INFO, SERVICE_DEMAND_START,
    SERVICE_DISABLED, SERVICE_NO_CHANGE, SERVICE_QUERY_CONFIG, SERVICE_SYSTEM_START,
};

use super::{last_error, os_error};
use crate::os::{OsError, OsErrorKind, ServiceStart, Services};

/// The Windows service control manager.
pub struct WinServices;

/// A service control handle, closed on drop.
struct ScHandle(SC_HANDLE);

impl Drop for ScHandle {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a non-null handle from `OpenSCManagerW` or
        // `OpenServiceW`, closed exactly once.
        unsafe { CloseServiceHandle(self.0) };
    }
}

/// Opens `service` with `access`. `Ok(None)` when it does not exist.
fn open_service(service: &str, access: u32) -> Result<Option<ScHandle>, OsError> {
    // SAFETY: null machine and database names select the local active
    // database.
    let manager = unsafe { OpenSCManagerW(ptr::null(), ptr::null(), SC_MANAGER_CONNECT) };
    if manager.is_null() {
        return Err(last_error("open the service control manager"));
    }
    let manager = ScHandle(manager);
    let name = wide(service);
    // SAFETY: `manager` is open; `name` is NUL-terminated.
    let handle = unsafe { OpenServiceW(manager.0, name.as_ptr(), access) };
    if !handle.is_null() {
        return Ok(Some(ScHandle(handle)));
    }
    // SAFETY: trivially safe; reads thread-local state.
    match unsafe { GetLastError() } {
        ERROR_SERVICE_DOES_NOT_EXIST => Ok(None),
        code => Err(os_error(format!("open service {service}"), code)),
    }
}

impl Services for WinServices {
    fn start_type(&self, service: &str) -> Result<Option<ServiceStart>, OsError> {
        let Some(handle) = open_service(service, SERVICE_QUERY_CONFIG)? else { return Ok(None) };
        // The documented maximum size of the configuration is 8 KiB. `u64`
        // elements give the buffer the alignment the structure needs.
        let mut buffer = [0u64; 1024];
        let mut needed = 0u32;
        // SAFETY: `buffer` is writable, 8192 bytes long and suitably aligned
        // for `QUERY_SERVICE_CONFIGW`; `needed` is a valid out-pointer.
        let ok = unsafe {
            QueryServiceConfigW(
                handle.0,
                buffer.as_mut_ptr().cast::<QUERY_SERVICE_CONFIGW>(),
                size_of_val(&buffer) as u32,
                &mut needed,
            )
        };
        if ok == 0 {
            return Err(last_error(format!("query service {service}")));
        }
        // SAFETY: the call succeeded, so the buffer starts with an
        // initialised `QUERY_SERVICE_CONFIGW`.
        let start = unsafe { (*buffer.as_ptr().cast::<QUERY_SERVICE_CONFIGW>()).dwStartType };
        Ok(Some(match start {
            SERVICE_BOOT_START => ServiceStart::Boot,
            SERVICE_SYSTEM_START => ServiceStart::System,
            SERVICE_AUTO_START if is_delayed(&handle) => ServiceStart::AutomaticDelayed,
            SERVICE_AUTO_START => ServiceStart::Automatic,
            SERVICE_DEMAND_START => ServiceStart::Manual,
            SERVICE_DISABLED => ServiceStart::Disabled,
            other => {
                return Err(OsError::new(
                    OsErrorKind::Other,
                    format!(
                        "query service {service}: expected start type 0..=4 | received: {other}"
                    ),
                    0,
                ));
            }
        }))
    }

    fn set_start_type(&self, service: &str, start: ServiceStart) -> Result<(), OsError> {
        let context = || format!("configure service {service}");
        let handle = open_service(service, SERVICE_CHANGE_CONFIG)?
            .ok_or_else(|| os_error(context(), ERROR_SERVICE_DOES_NOT_EXIST))?;
        let start_type = match start {
            ServiceStart::Boot => SERVICE_BOOT_START,
            ServiceStart::System => SERVICE_SYSTEM_START,
            ServiceStart::Automatic | ServiceStart::AutomaticDelayed => SERVICE_AUTO_START,
            ServiceStart::Manual => SERVICE_DEMAND_START,
            ServiceStart::Disabled => SERVICE_DISABLED,
        };
        // SAFETY: `handle` is open with change access; `SERVICE_NO_CHANGE`
        // and null pointers leave every other setting untouched.
        let ok = unsafe {
            ChangeServiceConfigW(
                handle.0,
                SERVICE_NO_CHANGE,
                start_type,
                SERVICE_NO_CHANGE,
                ptr::null(),
                ptr::null(),
                ptr::null_mut(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
            )
        };
        if ok == 0 {
            return Err(last_error(context()));
        }
        if start_type == SERVICE_AUTO_START {
            let info = SERVICE_DELAYED_AUTO_START_INFO {
                fDelayedAutostart: i32::from(start == ServiceStart::AutomaticDelayed),
            };
            // SAFETY: `info` is the structure this information level
            // expects and outlives the call.
            let ok = unsafe {
                ChangeServiceConfig2W(
                    handle.0,
                    SERVICE_CONFIG_DELAYED_AUTO_START_INFO,
                    ptr::from_ref(&info).cast(),
                )
            };
            if ok == 0 {
                return Err(last_error(context()));
            }
        }
        Ok(())
    }
}

/// Whether an automatic service has the delayed-start flag. A failed query
/// reads as "not delayed".
fn is_delayed(handle: &ScHandle) -> bool {
    let mut info = SERVICE_DELAYED_AUTO_START_INFO { fDelayedAutostart: 0 };
    let mut needed = 0u32;
    // SAFETY: `info` is writable and exactly the size passed; `needed` is a
    // valid out-pointer.
    let ok = unsafe {
        QueryServiceConfig2W(
            handle.0,
            SERVICE_CONFIG_DELAYED_AUTO_START_INFO,
            ptr::from_mut(&mut info).cast(),
            size_of::<SERVICE_DELAYED_AUTO_START_INFO>() as u32,
            &mut needed,
        )
    };
    ok != 0 && info.fDelayedAutostart != 0
}
