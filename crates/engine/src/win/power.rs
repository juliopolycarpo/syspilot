//! Power settings of the active scheme through `powrprof`.

use std::ptr;

use syspilot_proto::action::parse_guid;
use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, LocalFree};
use windows_sys::Win32::System::Power::{
    PowerGetActiveScheme, PowerReadACValueIndex, PowerReadDCValueIndex, PowerSetActiveScheme,
    PowerWriteACValueIndex, PowerWriteDCValueIndex,
};
use windows_sys::core::GUID;

use super::os_error;
use crate::os::{OsError, OsErrorKind, Power, PowerValues};

/// The active Windows power scheme.
pub struct WinPower;

fn guid(text: &str, what: &str) -> Result<GUID, OsError> {
    parse_guid(text)
        .map(|(data1, data2, data3, data4)| GUID { data1, data2, data3, data4 })
        .ok_or_else(|| {
            OsError::new(
                OsErrorKind::Other,
                format!("(power {what}): expected a GUID | received: {text:?}"),
                0,
            )
        })
}

fn active_scheme() -> Result<GUID, OsError> {
    let mut scheme: *mut GUID = ptr::null_mut();
    // SAFETY: a null root key selects the current user's power policy;
    // `scheme` is a valid out-pointer.
    let status = unsafe { PowerGetActiveScheme(ptr::null_mut(), &mut scheme) };
    if status != 0 || scheme.is_null() {
        return Err(os_error("read the active power scheme", status));
    }
    // SAFETY: on success `scheme` points to a GUID allocated by the system.
    let copy = unsafe { *scheme };
    // SAFETY: the system allocated `scheme` with `LocalAlloc`; it is freed
    // exactly once and not used afterwards.
    unsafe { LocalFree(scheme.cast()) };
    Ok(copy)
}

impl Power for WinPower {
    fn read(&self, subgroup: &str, setting: &str) -> Result<Option<PowerValues>, OsError> {
        let context = || format!("read power setting {subgroup}/{setting}");
        let (subgroup_guid, setting_guid) =
            (guid(subgroup, "subgroup")?, guid(setting, "setting")?);
        let scheme = active_scheme()?;
        let (mut ac, mut dc) = (0u32, 0u32);
        // SAFETY: all GUID pointers refer to locals that outlive the call;
        // `ac` is a valid out-pointer.
        let status = unsafe {
            PowerReadACValueIndex(ptr::null_mut(), &scheme, &subgroup_guid, &setting_guid, &mut ac)
        };
        match status {
            0 => {}
            ERROR_FILE_NOT_FOUND => return Ok(None),
            code => return Err(os_error(context(), code)),
        }
        // SAFETY: as above, with `dc` as the out-pointer.
        let status = unsafe {
            PowerReadDCValueIndex(ptr::null_mut(), &scheme, &subgroup_guid, &setting_guid, &mut dc)
        };
        match status {
            0 => Ok(Some(PowerValues { ac, dc })),
            ERROR_FILE_NOT_FOUND => Ok(None),
            code => Err(os_error(context(), code)),
        }
    }

    fn write(
        &self,
        subgroup: &str,
        setting: &str,
        ac: Option<u32>,
        dc: Option<u32>,
    ) -> Result<(), OsError> {
        let context = || format!("write power setting {subgroup}/{setting}");
        let (subgroup_guid, setting_guid) =
            (guid(subgroup, "subgroup")?, guid(setting, "setting")?);
        let scheme = active_scheme()?;
        if let Some(ac) = ac {
            // SAFETY: all GUID pointers refer to locals that outlive the call.
            let status = unsafe {
                PowerWriteACValueIndex(ptr::null_mut(), &scheme, &subgroup_guid, &setting_guid, ac)
            };
            if status != 0 {
                return Err(os_error(context(), status));
            }
        }
        if let Some(dc) = dc {
            // SAFETY: as above.
            let status = unsafe {
                PowerWriteDCValueIndex(ptr::null_mut(), &scheme, &subgroup_guid, &setting_guid, dc)
            };
            if status != 0 {
                return Err(os_error(context(), status));
            }
        }
        // Written indexes take effect only when the scheme is activated
        // again, even if it is already the active one.
        // SAFETY: `scheme` refers to a local that outlives the call.
        let status = unsafe { PowerSetActiveScheme(ptr::null_mut(), &scheme) };
        if status == 0 { Ok(()) } else { Err(os_error(context(), status)) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_guid_is_rejected_before_any_call() {
        let error = WinPower.read("not-a-guid", "also-not").unwrap_err();
        assert_eq!(error.context, "(power subgroup): expected a GUID | received: \"not-a-guid\"");
    }
}
