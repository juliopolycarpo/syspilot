//! The executable names itself "SysPilot update" for the taskbar and Task
//! Manager, through its version resource.

use std::os::windows::ffi::OsStrExt;

use windows_sys::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
};

fn wide(text: &str) -> Vec<u16> {
    std::ffi::OsStr::new(text).encode_wide().chain(Some(0)).collect()
}

/// The string `name` from the US English block of `exe`'s version resource.
fn version_string(exe: &str, name: &str) -> Option<String> {
    let path = wide(exe);
    // SAFETY: a NUL-terminated path; the handle argument is unused.
    let size = unsafe { GetFileVersionInfoSizeW(path.as_ptr(), std::ptr::null_mut()) };
    if size == 0 {
        return None;
    }
    let mut data = vec![0_u8; size as usize];
    // SAFETY: `data` holds the `size` bytes asked for.
    let read = unsafe { GetFileVersionInfoW(path.as_ptr(), 0, size, data.as_mut_ptr().cast()) };
    if read == 0 {
        return None;
    }
    let query = wide(&format!(r"\StringFileInfo\040904B0\{name}"));
    let mut value: *mut core::ffi::c_void = std::ptr::null_mut();
    let mut length = 0_u32;
    // SAFETY: `data` is the block just read; the out-pointers are valid.
    let found =
        unsafe { VerQueryValueW(data.as_ptr().cast(), query.as_ptr(), &mut value, &mut length) };
    if found == 0 || value.is_null() {
        return None;
    }
    // SAFETY: the value is `length` UTF-16 units, NUL included, inside `data`.
    let units = unsafe { std::slice::from_raw_parts(value.cast::<u16>(), length as usize) };
    Some(String::from_utf16_lossy(units).trim_end_matches('\0').to_owned())
}

#[test]
fn the_executable_names_itself_syspilot_update() {
    let exe = env!("CARGO_BIN_EXE_syspilot-companion");
    for (name, expected) in [
        ("FileDescription", "SysPilot update"),
        ("ProductName", "SysPilot"),
        ("ProductVersion", env!("CARGO_PKG_VERSION")),
    ] {
        let received = version_string(exe, name);
        assert_eq!(
            received.as_deref(),
            Some(expected),
            "({name}): expected {expected:?} | received: {received:?}"
        );
    }
}
