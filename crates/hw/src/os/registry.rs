//! Read-only access to `HKEY_LOCAL_MACHINE` values.
//!
//! Every function takes the key path relative to `HKLM` and returns `None`
//! when the key or value is missing, has another type or cannot be read.
//! Nothing here opens a key for writing.

use std::ffi::c_void;
use std::ptr;

use syspilot_proto::win::wide;
use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW,
};

use super::text::clean_utf16;

/// A `REG_SZ` value, cleaned with [`clean_utf16`].
pub(crate) fn read_string(subkey: &str, value: &str) -> Option<String> {
    let (subkey, value) = (wide(subkey), wide(value));
    let mut bytes = 0u32;
    // SAFETY: both names are NUL-terminated and outlive the call; a null data
    // pointer asks for the size only, which `bytes` receives.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut bytes,
        )
    };
    if status != ERROR_SUCCESS && status != ERROR_MORE_DATA {
        return None;
    }
    // One extra unit: the value may change between the two calls.
    let mut units = vec![0u16; bytes as usize / 2 + 1];
    let mut capacity = (units.len() * 2) as u32;
    // SAFETY: `units` is valid for writes of `capacity` bytes; the names are
    // NUL-terminated and outlive the call.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            ptr::null_mut(),
            units.as_mut_ptr().cast::<c_void>(),
            &mut capacity,
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    // `RegGetValueW` guarantees NUL termination of `REG_SZ` data.
    Some(clean_utf16(&units))
}

/// A `REG_DWORD` value.
pub(crate) fn read_u32(subkey: &str, value: &str) -> Option<u32> {
    let mut data = 0u32;
    read_fixed(subkey, value, RRF_RT_REG_DWORD, ptr::from_mut(&mut data).cast(), 4)?;
    Some(data)
}

/// Reads a value of exactly `size` bytes into `data`.
fn read_fixed(subkey: &str, value: &str, flags: u32, data: *mut c_void, size: u32) -> Option<()> {
    let (subkey, value) = (wide(subkey), wide(value));
    let mut bytes = size;
    // SAFETY: both names are NUL-terminated and outlive the call; the callers
    // pass `data` pointing at a local of exactly `size` bytes.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            value.as_ptr(),
            flags,
            ptr::null_mut(),
            data,
            &mut bytes,
        )
    };
    (status == ERROR_SUCCESS && bytes == size).then_some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CURRENT_VERSION: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";

    #[test]
    fn string_value_that_exists_is_read_and_trimmed() {
        let build = read_string(CURRENT_VERSION, "CurrentBuildNumber").unwrap();
        assert!(
            build.parse::<u32>().is_ok(),
            "CurrentBuildNumber: expected digits | received: {build:?}"
        );
    }

    #[test]
    fn dword_value_that_exists_is_read() {
        let major = read_u32(CURRENT_VERSION, "CurrentMajorVersionNumber").unwrap();
        assert!(major >= 10, "CurrentMajorVersionNumber: expected >= 10 | received: {major}");
    }

    #[test]
    fn missing_key_or_value_is_none() {
        assert_eq!(read_string(r"SOFTWARE\SysPilot-Test\no-such-key", "x"), None);
        assert_eq!(read_string(CURRENT_VERSION, "NoSuchValue"), None);
        assert_eq!(read_u32(CURRENT_VERSION, "NoSuchValue"), None);
    }

    #[test]
    fn value_of_another_type_is_none() {
        // `ProductName` is a string; asking for a DWORD must not reinterpret it.
        assert_eq!(read_u32(CURRENT_VERSION, "ProductName"), None);
        assert_eq!(read_string(CURRENT_VERSION, "CurrentMajorVersionNumber"), None);
    }
}
