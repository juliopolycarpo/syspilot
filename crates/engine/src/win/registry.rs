//! Registry access through the Win32 registry API.

use std::ptr;

use syspilot_proto::win::wide;
use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, HKEY_USERS, KEY_QUERY_VALUE, KEY_SET_VALUE, KEY_WOW64_64KEY,
    REG_OPTION_NON_VOLATILE, RegCloseKey, RegCreateKeyExW, RegDeleteKeyExW, RegDeleteValueW,
    RegOpenKeyExW, RegQueryInfoKeyW, RegQueryValueExW, RegSetValueExW,
};

use super::os_error;
use crate::os::{Hive, OsError, RegValue, Registry};

/// The Windows registry. Always uses the 64-bit view.
pub struct WinRegistry;

/// An open registry key, closed on drop.
struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: `self.0` was returned by a successful open/create call and
        // is closed exactly once.
        unsafe { RegCloseKey(self.0) };
    }
}

/// The predefined root and the NUL-terminated subkey path for `key`.
fn locate(hive: &Hive, key: &str) -> (HKEY, Vec<u16>) {
    match hive {
        Hive::Machine => (HKEY_LOCAL_MACHINE, wide(key)),
        Hive::User(sid) => (HKEY_USERS, wide(&format!(r"{sid}\{key}"))),
    }
}

/// Opens an existing key. `Ok(None)` when it does not exist.
fn open(hive: &Hive, key: &str, access: u32) -> Result<Option<Key>, OsError> {
    let (root, path) = locate(hive, key);
    let mut handle: HKEY = ptr::null_mut();
    // SAFETY: `path` is NUL-terminated and outlives the call; `handle` is a
    // valid out-pointer; `root` is a predefined key.
    let status =
        unsafe { RegOpenKeyExW(root, path.as_ptr(), 0, access | KEY_WOW64_64KEY, &mut handle) };
    match status {
        0 => Ok(Some(Key(handle))),
        ERROR_FILE_NOT_FOUND => Ok(None),
        code => Err(os_error(format!(r"open {hive}\{key}"), code)),
    }
}

impl Registry for WinRegistry {
    fn read(&self, hive: &Hive, key: &str, name: &str) -> Result<Option<RegValue>, OsError> {
        let Some(handle) = open(hive, key, KEY_QUERY_VALUE)? else { return Ok(None) };
        let name_w = wide(name);
        let mut data = vec![0u8; 256];
        loop {
            let mut kind = 0u32;
            let mut size = data.len() as u32;
            // SAFETY: `name_w` is NUL-terminated; `data` is valid for writes
            // of `size` bytes; `kind` and `size` are valid out-pointers.
            let status = unsafe {
                RegQueryValueExW(
                    handle.0,
                    name_w.as_ptr(),
                    ptr::null(),
                    &mut kind,
                    data.as_mut_ptr(),
                    &mut size,
                )
            };
            match status {
                0 => {
                    data.truncate(size as usize);
                    return Ok(Some(RegValue { kind, data }));
                }
                // The value grew between calls, or was larger than the
                // first guess: `size` now holds what is needed.
                ERROR_MORE_DATA => data.resize(size.max(data.len() as u32 * 2) as usize, 0),
                ERROR_FILE_NOT_FOUND => return Ok(None),
                code => return Err(os_error(format!(r"read {hive}\{key}\{name}"), code)),
            }
        }
    }

    fn write(&self, hive: &Hive, key: &str, name: &str, value: &RegValue) -> Result<(), OsError> {
        let context = || format!(r"write {hive}\{key}\{name}");
        let (root, path) = locate(hive, key);
        let mut handle: HKEY = ptr::null_mut();
        // SAFETY: `path` is NUL-terminated and outlives the call; `handle`
        // is a valid out-pointer; unused pointer arguments are null.
        let status = unsafe {
            RegCreateKeyExW(
                root,
                path.as_ptr(),
                0,
                ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE | KEY_WOW64_64KEY,
                ptr::null(),
                &mut handle,
                ptr::null_mut(),
            )
        };
        if status != 0 {
            return Err(os_error(context(), status));
        }
        let handle = Key(handle);
        let name_w = wide(name);
        // SAFETY: `name_w` is NUL-terminated; `value.data` is valid for
        // reads of its length, which is passed alongside.
        let status = unsafe {
            RegSetValueExW(
                handle.0,
                name_w.as_ptr(),
                0,
                value.kind,
                value.data.as_ptr(),
                value.data.len() as u32,
            )
        };
        if status == 0 { Ok(()) } else { Err(os_error(context(), status)) }
    }

    fn delete_value(&self, hive: &Hive, key: &str, name: &str) -> Result<(), OsError> {
        let Some(handle) = open(hive, key, KEY_SET_VALUE)? else { return Ok(()) };
        let name_w = wide(name);
        // SAFETY: `name_w` is NUL-terminated; `handle` is an open key.
        match unsafe { RegDeleteValueW(handle.0, name_w.as_ptr()) } {
            0 | ERROR_FILE_NOT_FOUND => Ok(()),
            code => Err(os_error(format!(r"delete {hive}\{key}\{name}"), code)),
        }
    }

    fn key_exists(&self, hive: &Hive, key: &str) -> Result<bool, OsError> {
        match open(hive, key, KEY_QUERY_VALUE) {
            Ok(found) => Ok(found.is_some()),
            // Being refused proves the key is there.
            Err(error) if error.code == ERROR_ACCESS_DENIED => Ok(true),
            Err(error) => Err(error),
        }
    }

    fn delete_key_if_empty(&self, hive: &Hive, key: &str) -> Result<bool, OsError> {
        // Never remove a key directly under the hive root: those belong to
        // Windows, whatever a snapshot says.
        if !key.contains('\\') {
            return Ok(false);
        }
        let Some(handle) = open(hive, key, KEY_QUERY_VALUE)? else { return Ok(false) };
        let mut subkeys = 0u32;
        let mut values = 0u32;
        // SAFETY: `handle` is an open key; the two counters are valid
        // out-pointers and every other output is null (not requested).
        let status = unsafe {
            RegQueryInfoKeyW(
                handle.0,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
                &mut subkeys,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut values,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if status != 0 {
            return Err(os_error(format!(r"inspect {hive}\{key}"), status));
        }
        if subkeys != 0 || values != 0 {
            return Ok(false);
        }
        drop(handle);
        let (root, path) = locate(hive, key);
        // SAFETY: `path` is NUL-terminated and outlives the call.
        match unsafe { RegDeleteKeyExW(root, path.as_ptr(), KEY_WOW64_64KEY, 0) } {
            0 => Ok(true),
            ERROR_FILE_NOT_FOUND => Ok(false),
            code => Err(os_error(format!(r"delete key {hive}\{key}"), code)),
        }
    }
}
