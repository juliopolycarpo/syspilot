//! Win32 plumbing shared by the service and its clients: wide strings,
//! security descriptors, the pipe stream and the shared-memory mapping.

use std::ffi::c_void;
use std::io::{self, Read, Write};
use std::ptr;
use std::sync::atomic::AtomicU64;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_PIPE_BUSY, GENERIC_READ, GENERIC_WRITE, GetLastError,
    HANDLE, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
    SE_KERNEL_OBJECT,
};
use windows_sys::Win32::Security::{
    IsWellKnownSid, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES,
    WinBuiltinAdministratorsSid, WinLocalSystemSid,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, OPEN_EXISTING, ReadFile, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, WriteFile,
};
use windows_sys::Win32::System::IO::CancelIoEx;
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
    OpenFileMappingW, PAGE_READWRITE, UnmapViewOfFile,
};
use windows_sys::Win32::System::Pipes::WaitNamedPipeW;

use crate::sensor::{SECTION_BYTES, SECTION_WORDS};

/// Converts a string to a NUL-terminated UTF-16 buffer for `…W` APIs.
pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A handle that is closed on drop.
#[derive(Debug)]
pub struct OwnedHandle(HANDLE);

impl OwnedHandle {
    /// Takes ownership of `handle`, or returns the last OS error when it is
    /// null or `INVALID_HANDLE_VALUE`.
    pub fn new(handle: HANDLE) -> io::Result<Self> {
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(handle))
        }
    }

    /// The raw handle, valid for as long as `self` lives.
    pub fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a valid handle owned exclusively by `self`
        // (checked in `new`) and is not used after this call.
        unsafe { CloseHandle(self.0) };
    }
}

// SAFETY: a kernel handle is a process-wide token; the objects wrapped here
// (files, pipes, sections, events) may be used from any thread.
unsafe impl Send for OwnedHandle {}
// SAFETY: as above; concurrent use is serialised by the kernel.
unsafe impl Sync for OwnedHandle {}

/// A security descriptor built from an SDDL string.
pub struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

impl SecurityDescriptor {
    /// Parses `sddl`, e.g. `D:(A;;GA;;;SY)(A;;GA;;;BA)`.
    pub fn from_sddl(sddl: &str) -> io::Result<Self> {
        let sddl = wide(sddl);
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: `sddl` is NUL-terminated and outlives the call;
        // `descriptor` is a valid out-pointer.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        };
        if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(Self(descriptor)) }
    }

    /// `SECURITY_ATTRIBUTES` referring to this descriptor, for `Create…`
    /// calls. The result borrows `self`.
    pub fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        }
    }

    /// The raw descriptor, valid for as long as `self` lives.
    pub fn raw(&self) -> PSECURITY_DESCRIPTOR {
        self.0
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: the descriptor was allocated by the SDDL conversion with
        // `LocalAlloc` and is freed exactly once.
        unsafe { LocalFree(self.0) };
    }
}

/// Whether the kernel object behind `handle` is owned by LocalSystem or the
/// Administrators group, i.e. was created by privileged code.
pub fn is_owned_by_system_or_administrators(handle: &OwnedHandle) -> io::Result<bool> {
    let handle = handle.raw();
    let mut owner: PSID = ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: `handle` is a valid kernel object handle supplied by the
    // caller; the out-pointers are valid; unused outputs are null.
    let status = unsafe {
        GetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    // SAFETY: on success `owner` points into `descriptor`, which is alive
    // until the `LocalFree` below.
    let trusted = unsafe {
        IsWellKnownSid(owner, WinLocalSystemSid) != 0
            || IsWellKnownSid(owner, WinBuiltinAdministratorsSid) != 0
    };
    // SAFETY: `descriptor` was allocated by `GetSecurityInfo` and is freed once.
    unsafe { LocalFree(descriptor) };
    Ok(trusted)
}

/// A connected byte-mode named pipe, client or server end.
#[derive(Debug)]
pub struct PipeStream {
    handle: OwnedHandle,
}

impl PipeStream {
    /// Connects to the pipe `name` as a client, waiting up to `timeout_ms`
    /// when all server instances are busy.
    ///
    /// The connection allows the server to identify the client but not to
    /// act as it, so a process squatting on the pipe name cannot borrow an
    /// administrator's token.
    pub fn connect(name: &str, timeout_ms: u32) -> io::Result<Self> {
        let name = wide(name);
        loop {
            // SAFETY: `name` is NUL-terminated and outlives the call; all
            // other arguments are plain values or null.
            let handle = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    ptr::null(),
                    OPEN_EXISTING,
                    SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                    ptr::null_mut(),
                )
            };
            if handle != INVALID_HANDLE_VALUE {
                return Ok(Self { handle: OwnedHandle::new(handle)? });
            }
            // SAFETY: trivially safe; reads the calling thread's last error.
            if unsafe { GetLastError() } != ERROR_PIPE_BUSY {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `name` is NUL-terminated and outlives the call.
            if unsafe { WaitNamedPipeW(name.as_ptr(), timeout_ms) } == 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }

    /// Wraps the server end of a connected pipe instance.
    pub fn from_handle(handle: OwnedHandle) -> Self {
        Self { handle }
    }

    /// The pipe handle.
    pub fn handle(&self) -> &OwnedHandle {
        &self.handle
    }

    /// A token that lets another thread abort a blocked read or write on
    /// this stream.
    pub fn canceller(&self) -> PipeCanceller {
        PipeCanceller(self.handle.raw())
    }
}

/// Aborts blocked I/O on a [`PipeStream`] from another thread.
///
/// Must not outlive the stream it was created from: the caller keeps the
/// stream alive (for example in a thread that is joined after cancelling).
#[derive(Debug, Clone, Copy)]
pub struct PipeCanceller(HANDLE);

// SAFETY: `CancelIoEx` is documented as callable from any thread for any
// handle; the handle value itself is just a number.
unsafe impl Send for PipeCanceller {}
// SAFETY: as above.
unsafe impl Sync for PipeCanceller {}

impl PipeCanceller {
    /// Cancels pending I/O on the stream. The blocked call fails with
    /// `ERROR_OPERATION_ABORTED`.
    pub fn cancel(&self) {
        // SAFETY: the creator guarantees the stream (and so the handle) is
        // still alive; a null overlapped pointer cancels all requests.
        unsafe { CancelIoEx(self.0, ptr::null()) };
    }
}

impl Read for PipeStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let mut read = 0u32;
        let len = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
        // SAFETY: `buffer` is valid for writes of `len` bytes; `read` is a
        // valid out-pointer; the handle is owned by `self`.
        let ok = unsafe {
            ReadFile(self.handle.raw(), buffer.as_mut_ptr(), len, &mut read, ptr::null_mut())
        };
        if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(read as usize) }
    }
}

impl Write for PipeStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let mut written = 0u32;
        let len = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
        // SAFETY: `buffer` is valid for reads of `len` bytes; `written` is a
        // valid out-pointer; the handle is owned by `self`.
        let ok = unsafe {
            WriteFile(self.handle.raw(), buffer.as_ptr(), len, &mut written, ptr::null_mut())
        };
        if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(written as usize) }
    }

    fn flush(&mut self) -> io::Result<()> {
        // Pipe writes are complete when `WriteFile` returns. Flushing the
        // file buffers would block until the peer has read everything.
        Ok(())
    }
}

/// The sensor section mapped into this process.
pub struct MappedSection {
    view: *const AtomicU64,
    // Declared after `view` only for clarity: the view is unmapped in
    // `Drop` before the handle field is dropped.
    _handle: OwnedHandle,
}

// SAFETY: the mapping is plain shared memory accessed only through
// `AtomicU64`, which is safe to share and send across threads.
unsafe impl Send for MappedSection {}
// SAFETY: as above.
unsafe impl Sync for MappedSection {}

impl MappedSection {
    /// Creates the section `name` with the given security descriptor and
    /// maps it writable. Fails if an object of that name already exists, so
    /// the service never adopts a section another process prepared.
    pub fn create(name: &str, security: &SecurityDescriptor) -> io::Result<Self> {
        let name = wide(name);
        let attributes = security.attributes();
        // SAFETY: `attributes` and `name` outlive the call; the size is a
        // constant; `INVALID_HANDLE_VALUE` requests a pagefile-backed section.
        let handle = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                &attributes,
                PAGE_READWRITE,
                0,
                SECTION_BYTES as u32,
                name.as_ptr(),
            )
        };
        // SAFETY: trivially safe; must be read before any other API call.
        let already_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        let handle = OwnedHandle::new(handle)?;
        if already_exists {
            return Err(io::Error::from_raw_os_error(ERROR_ALREADY_EXISTS as i32));
        }
        Self::map(handle, FILE_MAP_READ | FILE_MAP_WRITE)
    }

    /// Opens the existing section `name` read-only.
    pub fn open(name: &str) -> io::Result<Self> {
        let name = wide(name);
        // SAFETY: `name` is NUL-terminated and outlives the call.
        let handle = unsafe { OpenFileMappingW(FILE_MAP_READ, 0, name.as_ptr()) };
        Self::map(OwnedHandle::new(handle)?, FILE_MAP_READ)
    }

    fn map(handle: OwnedHandle, access: u32) -> io::Result<Self> {
        // SAFETY: `handle` is a valid section handle; mapping
        // `SECTION_BYTES` from offset zero is within the section, or the
        // call fails and returns null.
        let view = unsafe { MapViewOfFile(handle.raw(), access, 0, 0, SECTION_BYTES) };
        if view.Value.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { view: view.Value.cast::<AtomicU64>(), _handle: handle })
    }

    /// The section as atomic words, for [`crate::sensor::SectionReader`]
    /// and [`crate::sensor::SectionWriter`]. Storing through a mapping made
    /// by [`MappedSection::open`] faults; readers only load.
    pub fn words(&self) -> &[AtomicU64] {
        // SAFETY: the view is `SECTION_BYTES` long, page-aligned (so aligned
        // for `AtomicU64`), zero-initialised by the kernel, and stays mapped
        // until `self` is dropped. `AtomicU64` permits shared mutation.
        unsafe { std::slice::from_raw_parts(self.view, SECTION_WORDS) }
    }
}

impl Drop for MappedSection {
    fn drop(&mut self) {
        let address = MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view as *mut c_void };
        // SAFETY: `address` is the base of a view mapped in `map` and not
        // yet unmapped; no reference into it outlives `self`.
        unsafe { UnmapViewOfFile(address) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sensor::{Sample, SectionReader, SectionWriter};

    /// World-readable, so the test also passes for a non-elevated user.
    const TEST_SDDL: &str = "D:(A;;GA;;;WD)";

    fn unique_name(tag: &str) -> String {
        format!(r"Local\SysPilot-Test.{tag}.{}", std::process::id())
    }

    #[test]
    fn wide_strings_are_nul_terminated() {
        assert_eq!(wide("ab"), [0x61, 0x62, 0]);
    }

    #[test]
    fn invalid_sddl_is_an_error() {
        assert!(SecurityDescriptor::from_sddl("not sddl").is_err());
    }

    #[test]
    fn section_written_through_one_mapping_is_read_through_another() {
        let name = unique_name("section");
        let security = SecurityDescriptor::from_sddl(TEST_SDDL).unwrap();
        let writable = MappedSection::create(&name, &security).unwrap();
        let mut writer = SectionWriter::new(writable.words()).unwrap();
        writer.publish(42, 1, [Sample { id: 5, value: Some(36.6) }]);

        let read_only = MappedSection::open(&name).unwrap();
        let mut samples = Vec::new();
        let header = SectionReader::new(read_only.words()).unwrap().read(&mut samples).unwrap();
        assert_eq!(header.timestamp_ms, 42);
        assert_eq!(samples, [Sample { id: 5, value: Some(36.6) }]);
    }

    #[test]
    fn creating_a_section_that_exists_is_refused() {
        let name = unique_name("exclusive");
        let security = SecurityDescriptor::from_sddl(TEST_SDDL).unwrap();
        let _first = MappedSection::create(&name, &security).unwrap();
        let error = MappedSection::create(&name, &security).err().unwrap();
        assert_eq!(error.raw_os_error(), Some(ERROR_ALREADY_EXISTS as i32));
    }

    #[test]
    fn opening_a_missing_section_fails() {
        assert!(MappedSection::open(&unique_name("missing")).is_err());
    }

    #[test]
    fn connecting_to_a_missing_pipe_reports_not_found() {
        let error = PipeStream::connect(r"\\.\pipe\SysPilot-Test.no-such-pipe", 10).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}
