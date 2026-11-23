//! The real driver: one open handle on `\Device\PawnIO`.

use std::ffi::c_void;
use std::ptr;

use windows_sys::Wdk::System::IO::NtDeviceIoControlFile;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, GENERIC_READ,
    GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

use crate::abi::{
    DEVICE_PATH, IOCTL_EXECUTE_FN, IOCTL_LOAD_BINARY, IOCTL_VERSION, encode_execute_input,
};
use crate::error::{Error, status};
use crate::executor::Executor;
use crate::version::{MIN_DRIVER_VERSION, Version};

/// An open handle on the PawnIO device.
///
/// One handle holds one module: [`PawnIo::load`] succeeds once per handle,
/// and [`Executor::execute`] calls functions of that module. A service that
/// uses several modules opens a handle for each. The handle is closed (and
/// the module unloaded) on drop.
#[derive(Debug)]
pub struct PawnIo {
    handle: HANDLE,
}

// SAFETY: a file handle is a process-wide token; the driver serialises
// calls on one handle with its own mutex, so use from any thread is fine.
unsafe impl Send for PawnIo {}
// SAFETY: as above; `&PawnIo` only issues IOCTLs, which the kernel serialises.
unsafe impl Sync for PawnIo {}

impl PawnIo {
    /// Opens the device and checks the driver version against
    /// [`MIN_DRIVER_VERSION`].
    ///
    /// # Errors
    ///
    /// [`Error::DriverAbsent`] when the device does not exist,
    /// [`Error::NotElevated`] when the caller may not open it,
    /// [`Error::DriverTooOld`] when the driver is older than supported.
    pub fn open() -> Result<Self, Error> {
        let device = Self::open_device()?;
        let found = device.driver_version()?;
        if found < MIN_DRIVER_VERSION {
            return Err(Error::DriverTooOld { found, required: MIN_DRIVER_VERSION });
        }
        Ok(device)
    }

    fn open_device() -> Result<Self, Error> {
        let path = wide(DEVICE_PATH);
        // SAFETY: `path` is NUL-terminated and outlives the call; the
        // security attributes and template handle are null, which is valid.
        // Without `FILE_FLAG_OVERLAPPED` the handle is synchronous, so
        // `NtDeviceIoControlFile` returns the final status.
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            // SAFETY: trivially safe; reads the thread's last error, which
            // nothing has overwritten since `CreateFileW` failed.
            let win32 = unsafe { GetLastError() };
            return Err(match win32 {
                ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => Error::DriverAbsent { win32 },
                ERROR_ACCESS_DENIED => Error::NotElevated,
                other => Error::OpenFailed { win32: other },
            });
        }
        Ok(Self { handle })
    }

    /// The driver version, from `IOCTL_PIO_VERSION`.
    pub fn driver_version(&self) -> Result<Version, Error> {
        let mut raw = 0u32;
        // SAFETY: the output buffer is a live `u32`, exactly the four bytes
        // the IOCTL requires; there is no input buffer.
        let result =
            unsafe { self.ioctl(IOCTL_VERSION, &[], ptr::from_mut(&mut raw).cast::<c_void>(), 4) };
        match result {
            status::SUCCESS => Ok(Version::from_raw(raw)),
            other => Err(Error::Status { operation: "version".into(), status: other }),
        }
    }

    /// Loads a module into this handle. `blob` is the release `.bin` file
    /// as is.
    ///
    /// # Errors
    ///
    /// [`Error::SignatureRefused`], [`Error::ModuleMalformed`],
    /// [`Error::NotSupported`] when the module refuses this machine,
    /// [`Error::AlreadyLoaded`] on a second load.
    pub fn load(&self, blob: &[u8]) -> Result<(), Error> {
        if u32::try_from(blob.len()).is_err() {
            return Err(Error::BufferTooLarge { bytes: blob.len() });
        }
        // SAFETY: `blob` is valid for reads for its whole length for the
        // duration of the call; the IOCTL has no output.
        let result = unsafe { self.ioctl(IOCTL_LOAD_BINARY, blob, ptr::null_mut(), 0) };
        match result {
            status::SUCCESS => Ok(()),
            other => Err(Error::from_load_status(other)),
        }
    }

    /// Issues one IOCTL and returns its NTSTATUS.
    ///
    /// # Safety
    ///
    /// `output` must be null with `output_len == 0`, or valid for writes of
    /// `output_len` bytes until the call returns. `input.len()` and
    /// `output_len` must fit `u32` (callers check).
    unsafe fn ioctl(&self, code: u32, input: &[u8], output: *mut c_void, output_len: usize) -> u32 {
        let mut block = IO_STATUS_BLOCK::default();
        let input_ptr =
            if input.is_empty() { ptr::null() } else { input.as_ptr().cast::<c_void>() };
        // SAFETY: the handle is open for the life of `self`; `block` is a
        // valid out-pointer; the buffers are valid per this function's
        // contract; no event or APC is passed because the handle is
        // synchronous.
        let result = unsafe {
            NtDeviceIoControlFile(
                self.handle,
                ptr::null_mut(),
                None,
                ptr::null(),
                &mut block,
                code,
                input_ptr,
                input.len() as u32,
                output,
                output_len as u32,
            )
        };
        result as u32
    }
}

impl Executor for PawnIo {
    fn execute(&self, function: &str, input: &[u64], output: &mut [u64]) -> Result<(), Error> {
        let request = encode_execute_input(function, input)?;
        let output_bytes = std::mem::size_of_val(output);
        if u32::try_from(output_bytes).is_err() {
            return Err(Error::BufferTooLarge { bytes: output_bytes });
        }
        output.fill(0);
        let output_ptr =
            if output.is_empty() { ptr::null_mut() } else { output.as_mut_ptr().cast::<c_void>() };
        // SAFETY: `output_ptr` is null for an empty slice, otherwise valid
        // for `output_bytes` bytes of writes; `request` fits `u32`
        // (checked in `encode_execute_input`).
        let result = unsafe { self.ioctl(IOCTL_EXECUTE_FN, &request, output_ptr, output_bytes) };
        match result {
            status::SUCCESS => Ok(()),
            other => Err(Error::from_execute_status(function, input, other)),
        }
    }
}

impl Drop for PawnIo {
    fn drop(&mut self) {
        // SAFETY: `self.handle` is a valid handle owned exclusively by
        // `self` (returned by `CreateFileW`) and is not used afterwards.
        unsafe { CloseHandle(self.handle) };
    }
}

/// UTF-16, NUL-terminated, for `…W` APIs.
pub(crate) fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_strings_are_nul_terminated() {
        assert_eq!(wide("ab"), [0x61, 0x62, 0]);
    }

    #[test]
    fn opening_never_panics_whatever_the_machine_state() {
        // Without the driver this is DriverAbsent, without elevation
        // NotElevated; with both it opens. Each is a typed outcome.
        match PawnIo::open() {
            Ok(_) | Err(Error::DriverAbsent { .. } | Error::NotElevated) => {}
            Err(other) => {
                panic!("(pawnio.open): expected a typed open outcome | received: {other}")
            }
        }
    }
}
