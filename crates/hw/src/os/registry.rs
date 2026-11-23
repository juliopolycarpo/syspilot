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
    HKEY, HKEY_LOCAL_MACHINE, KEY_ENUMERATE_SUB_KEYS, RRF_RT_REG_DWORD, RRF_RT_REG_QWORD,
    RRF_RT_REG_SZ, RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW,
};

use super::text::clean_utf16;

/// Longest registry key name, in UTF-16 units, plus the terminator.
const MAX_KEY_NAME_UNITS: usize = 256;

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

/// A `REG_QWORD` value.
pub(crate) fn read_u64(subkey: &str, value: &str) -> Option<u64> {
    let mut data = 0u64;
    read_fixed(subkey, value, RRF_RT_REG_QWORD, ptr::from_mut(&mut data).cast(), 8)?;
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

/// Names of the direct subkeys of `subkey`, in registry order. Empty when
/// the key is missing or cannot be enumerated.
pub(crate) fn subkey_names(subkey: &str) -> Vec<String> {
    let path = wide(subkey);
    let mut key: HKEY = ptr::null_mut();
    // SAFETY: `path` is NUL-terminated and outlives the call; `key` is a
    // valid out-pointer. The key is opened for enumeration only.
    let status = unsafe {
        RegOpenKeyExW(HKEY_LOCAL_MACHINE, path.as_ptr(), 0, KEY_ENUMERATE_SUB_KEYS, &mut key)
    };
    if status != ERROR_SUCCESS {
        return Vec::new();
    }
    let mut names = Vec::new();
    let mut buffer = [0u16; MAX_KEY_NAME_UNITS];
    for index in 0.. {
        let mut units = MAX_KEY_NAME_UNITS as u32;
        // SAFETY: `key` is open; `buffer` is valid for `units` UTF-16 units;
        // the optional outputs are null.
        let status = unsafe {
            RegEnumKeyExW(
                key,
                index,
                buffer.as_mut_ptr(),
                &mut units,
                ptr::null(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        // `ERROR_NO_MORE_ITEMS` is the normal end; any other failure also
        // stops, keeping the names read so far.
        if status != ERROR_SUCCESS {
            break;
        }
        names.push(String::from_utf16_lossy(&buffer[..units as usize]));
    }
    // SAFETY: `key` was opened above and is closed exactly once.
    unsafe { RegCloseKey(key) };
    names
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
    fn qword_value_that_exists_is_read() {
        // `InstallTime` is a REG_QWORD (a FILETIME) on every Windows 10/11.
        let install_time = read_u64(CURRENT_VERSION, "InstallTime").unwrap();
        assert!(
            install_time > 0,
            "InstallTime: expected a FILETIME > 0 | received: {install_time}"
        );
    }

    #[test]
    fn missing_key_or_value_is_none() {
        assert_eq!(read_string(r"SOFTWARE\SysPilot-Test\no-such-key", "x"), None);
        assert_eq!(read_string(CURRENT_VERSION, "NoSuchValue"), None);
        assert_eq!(read_u32(CURRENT_VERSION, "NoSuchValue"), None);
        assert_eq!(read_u64(CURRENT_VERSION, "NoSuchValue"), None);
    }

    #[test]
    fn value_of_another_type_is_none() {
        // `ProductName` is a string; asking for a number must not reinterpret it.
        assert_eq!(read_u32(CURRENT_VERSION, "ProductName"), None);
        assert_eq!(read_u64(CURRENT_VERSION, "ProductName"), None);
        assert_eq!(read_string(CURRENT_VERSION, "CurrentMajorVersionNumber"), None);
        // A DWORD is not a QWORD either: the size must match exactly.
        assert_eq!(read_u64(CURRENT_VERSION, "CurrentMajorVersionNumber"), None);
    }

    #[test]
    fn subkeys_of_a_key_with_children_are_listed() {
        let names = subkey_names(r"HARDWARE\DESCRIPTION\System\CentralProcessor");
        assert!(
            names.iter().any(|name| name == "0"),
            "CentralProcessor subkeys: expected \"0\" | received: {names:?}"
        );
    }

    #[test]
    fn subkeys_of_a_missing_key_are_empty() {
        assert!(subkey_names(r"SOFTWARE\SysPilot-Test\no-such-key").is_empty());
    }
}
