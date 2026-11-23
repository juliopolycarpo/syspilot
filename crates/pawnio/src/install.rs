//! Detecting an installed PawnIO without opening the device.

use std::ptr;

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6464KEY, RegGetValueW,
};

use crate::device::wide;

/// Where users get the driver.
pub const INSTALL_URL: &str = "https://pawnio.eu";

/// The uninstall key the PawnIO installer writes.
const UNINSTALL_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\PawnIO";

/// The version the installer recorded, e.g. `2.2.0.0`, or `None` when the
/// uninstall entry is missing.
///
/// This works without elevation and without the device. It tells what was
/// installed, not what is running; [`crate::PawnIo::driver_version`] is
/// authoritative.
pub fn installed_version() -> Option<String> {
    let key = wide(UNINSTALL_KEY);
    let value = wide("DisplayVersion");
    let flags = RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY;

    let mut bytes = 0u32;
    // SAFETY: both names are NUL-terminated and outlive the call; a null
    // data pointer asks only for the required size, written to `bytes`.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            flags,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut bytes,
        )
    };
    if status != ERROR_SUCCESS || bytes == 0 {
        return None;
    }

    let mut buffer = vec![0u16; (bytes as usize).div_ceil(2)];
    let mut capacity = (buffer.len() * 2) as u32;
    // SAFETY: `buffer` is valid for `capacity` bytes of writes; the names
    // are as above.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            flags,
            ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut capacity,
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    decode_registry_string(&buffer[..(capacity as usize / 2).min(buffer.len())])
}

/// Text of a `REG_SZ` buffer: up to the first NUL, trimmed, `None` if empty.
fn decode_registry_string(units: &[u16]) -> Option<String> {
    let end = units.iter().position(|unit| *unit == 0).unwrap_or(units.len());
    let text = String::from_utf16_lossy(&units[..end]);
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_text_stops_at_the_first_nul() {
        let units: Vec<u16> = "2.2.0.0\0junk".encode_utf16().collect();
        assert_eq!(decode_registry_string(&units), Some("2.2.0.0".to_owned()));
    }

    #[test]
    fn empty_or_blank_registry_text_is_none() {
        assert_eq!(decode_registry_string(&[]), None);
        assert_eq!(decode_registry_string(&[0, 0x31]), None);
        let blank: Vec<u16> = "  \0".encode_utf16().collect();
        assert_eq!(decode_registry_string(&blank), None);
    }

    #[test]
    fn a_missing_terminator_is_tolerated() {
        let units: Vec<u16> = "2.0.0".encode_utf16().collect();
        assert_eq!(decode_registry_string(&units), Some("2.0.0".to_owned()));
    }

    #[test]
    fn installed_version_when_present_parses_as_a_version() {
        // Machine-dependent: absent on CI, present where PawnIO is installed.
        if let Some(text) = installed_version() {
            assert!(
                crate::Version::parse(&text).is_some(),
                "(pawnio.installed_version): expected a dotted version | received: {text:?}"
            );
        }
    }

    #[test]
    fn install_url_is_the_vendor_site() {
        assert_eq!(INSTALL_URL, "https://pawnio.eu");
    }
}
