//! Windows Installer: reading a package's version and starting an
//! installation.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use syspilot_proto::win::wide;
use windows_sys::Win32::System::ApplicationInstallationAndServicing::{
    MSIDBOPEN_READONLY, MSIHANDLE, MsiCloseHandle, MsiDatabaseOpenViewW, MsiOpenDatabaseW,
    MsiRecordGetStringW, MsiViewExecute, MsiViewFetch,
};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

use crate::release::Packages;

/// `CREATE_NO_WINDOW`: the installer runs from a service and shows nothing.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Longest version text read from a package, in UTF-16 units.
const VERSION_MAX: usize = 64;
const VERSION_QUERY: &str = "SELECT `Value` FROM `Property` WHERE `Property` = 'ProductVersion'";

/// A Windows Installer handle, closed on drop.
struct MsiHandle(MSIHANDLE);

impl Drop for MsiHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful call and is closed once.
        unsafe { MsiCloseHandle(self.0) };
    }
}

/// Turns a Windows Installer status into a result.
fn checked(status: u32, action: &str, package: &Path) -> Result<(), String> {
    if status == 0 {
        Ok(())
    } else {
        Err(format!(
            "(package): expected to {action} of {} | received: Windows Installer error {status}",
            package.display()
        ))
    }
}

/// The Windows system directory, where `msiexec.exe` is. Asked of Windows
/// and not of the environment, which the caller could have changed.
fn system_directory() -> Result<PathBuf, String> {
    let mut buffer = [0u16; 260];
    // SAFETY: `buffer` is as long as the count given.
    let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    if length == 0 || length >= buffer.len() {
        return Err("(system directory): expected Windows to name it | received: nothing".into());
    }
    Ok(PathBuf::from(String::from_utf16_lossy(&buffer[..length])))
}

/// The real Windows Installer.
pub struct WindowsInstaller;

impl Packages for WindowsInstaller {
    fn product_version(&self, package: &Path) -> Result<String, String> {
        let path = wide(&package.to_string_lossy());
        let query = wide(VERSION_QUERY);
        let mut handle: MSIHANDLE = 0;
        // SAFETY: `path` is NUL-terminated; the persist argument is the
        // read-only constant; `handle` is a valid out-pointer.
        let status = unsafe { MsiOpenDatabaseW(path.as_ptr(), MSIDBOPEN_READONLY, &mut handle) };
        checked(status, "open the database", package)?;
        let database = MsiHandle(handle);
        // SAFETY: `database` is open; `query` is NUL-terminated; `handle` is
        // a valid out-pointer.
        let status = unsafe { MsiDatabaseOpenViewW(database.0, query.as_ptr(), &mut handle) };
        checked(status, "query the properties", package)?;
        let view = MsiHandle(handle);
        // SAFETY: `view` is open; the query has no parameters, so no record.
        checked(unsafe { MsiViewExecute(view.0, 0) }, "query the properties", package)?;
        // SAFETY: `view` was executed; `handle` is a valid out-pointer.
        let status = unsafe { MsiViewFetch(view.0, &mut handle) };
        checked(status, "find the ProductVersion", package)?;
        let record = MsiHandle(handle);
        let mut buffer = [0u16; VERSION_MAX];
        let mut length = buffer.len() as u32;
        // SAFETY: `record` is a fetched record with one field; `buffer` is
        // `length` units long, and `length` receives the text's length
        // without the terminator.
        let status = unsafe { MsiRecordGetStringW(record.0, 1, buffer.as_mut_ptr(), &mut length) };
        checked(status, "read the ProductVersion", package)?;
        Ok(String::from_utf16_lossy(&buffer[..length as usize]))
    }

    fn install(&self, package: &Path, log: &Path) -> Result<(), String> {
        // By full path: nothing is searched for, so nothing else can run in
        // its place. Its standard streams are closed, so it holds nothing of
        // this process, which it is about to replace.
        let msiexec = system_directory()?.join("msiexec.exe");
        Command::new(&msiexec)
            .arg("/i")
            .arg(package)
            .args(["/qn", "/norestart", "/l*v"])
            .arg(log)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map(drop)
            .map_err(|error| format!("start {}: {error}", msiexec.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_system_directory_holds_windows_installer() {
        let directory = system_directory().unwrap();
        assert!(directory.join("msiexec.exe").is_file(), "{}", directory.display());
    }

    #[test]
    fn a_file_that_is_not_a_package_is_reported_with_its_path() {
        let file = std::env::temp_dir()
            .join(format!("SysPilot-Test.update-not-a-package-{}.msi", std::process::id()));
        std::fs::write(&file, b"not a package").unwrap();
        let problem = WindowsInstaller.product_version(&file).unwrap_err();
        std::fs::remove_file(&file).unwrap();
        assert!(
            problem.starts_with("(package): expected to open the database of")
                && problem.contains("SysPilot-Test.update-not-a-package"),
            "{problem}"
        );
    }
}
