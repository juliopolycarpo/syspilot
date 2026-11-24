//! The data directory under `%ProgramData%` and the files in it.
//!
//! The profile can hold commands that the service runs as LocalSystem, so
//! who may write this directory decides who may become LocalSystem.
//! `%ProgramData%` lets any user create subdirectories, which means a
//! directory that already exists cannot be trusted just because it is
//! there: [`DataDir::secure`] refuses one that is not owned by LocalSystem
//! or Administrators, and (re)applies a protected ACL to one that is.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::ptr;

use syspilot_engine::enforcer::StateStore;
use syspilot_proto::Instance;
use syspilot_proto::win::{SecurityDescriptor, wide};
use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    GetNamedSecurityInfoW, SE_FILE_OBJECT, SetNamedSecurityInfoW,
};
use windows_sys::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, IsWellKnownSid,
    OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    WinBuiltinAdministratorsSid, WinLocalSystemSid,
};
use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;

use crate::core::PROFILE_NAME;

/// LocalSystem and Administrators: full control. Nobody else. Protected, so
/// nothing is inherited from `%ProgramData%`.
///
/// The owner is set to Administrators explicitly. Left to the default, a
/// directory created by an elevated administrator is owned by that user's
/// own SID, which is the same SID the user has when not elevated, and the
/// ownership check on the next start could not tell the two apart. Only an
/// elevated process may assign this owner, so creation fails for anyone
/// else.
const PRIVATE_SDDL: &str = "O:BAD:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";
/// As above, plus read access for users: sensor logs are not secrets.
const LOGS_SDDL: &str = "O:BAD:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)";

/// The directories of one instance.
#[derive(Debug, Clone)]
pub struct DataDir {
    root: PathBuf,
}

impl DataDir {
    /// The data directory of `instance` under `%ProgramData%`.
    pub fn for_instance(instance: &Instance) -> Self {
        let program_data =
            std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into());
        Self::at(Path::new(&program_data).join(instance.name()))
    }

    /// A data directory at an explicit path.
    pub fn at(root: PathBuf) -> Self {
        Self { root }
    }

    /// The root directory: holds the profile.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Snapshots and history.
    pub fn state(&self) -> PathBuf {
        self.root.join("state")
    }

    /// Sensor logs.
    pub fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// Where the updater downloads a release and Windows Installer logs
    /// installing it. What lands here is run as LocalSystem once its
    /// signature has been verified, so nobody else may write it in between.
    pub fn updates(&self) -> PathBuf {
        self.root.join("updates")
    }

    /// Creates the directories with their ACLs, or verifies and repairs
    /// existing ones. Fails when an existing directory has an untrusted
    /// owner.
    pub fn secure(&self) -> Result<(), String> {
        secure_directory(&self.root, PRIVATE_SDDL)?;
        secure_directory(&self.state(), PRIVATE_SDDL)?;
        secure_directory(&self.updates(), PRIVATE_SDDL)?;
        secure_directory(&self.logs(), LOGS_SDDL)
    }

    /// Deletes the directory and everything in it.
    pub fn remove(&self) -> io::Result<()> {
        match fs::remove_dir_all(&self.root) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }
}

fn os_failure(action: &str, path: &Path, code: u32) -> String {
    format!("{action} {}: {}", path.display(), io::Error::from_raw_os_error(code as i32))
}

/// Creates `path` with the ACL in `sddl`, or, if it exists, checks its
/// owner and replaces its ACL.
fn secure_directory(path: &Path, sddl: &str) -> Result<(), String> {
    let descriptor = SecurityDescriptor::from_sddl(sddl)
        .map_err(|error| format!("(sddl): expected a valid descriptor | received: {error}"))?;
    let attributes = descriptor.attributes();
    let path_w = wide(&path.to_string_lossy());
    // SAFETY: `path_w` is NUL-terminated; `attributes` borrows `descriptor`,
    // which outlives the call.
    if unsafe { CreateDirectoryW(path_w.as_ptr(), &attributes) } != 0 {
        return Ok(());
    }
    // SAFETY: trivially safe; reads thread-local state.
    let code = unsafe { GetLastError() };
    if code != ERROR_ALREADY_EXISTS {
        return Err(os_failure("create", path, code));
    }
    if !path.is_dir() {
        return Err(format!(
            "(data directory): expected a directory | received: a file at {}",
            path.display()
        ));
    }
    if !is_owned_by_system_or_administrators(&path_w)
        .map_err(|code| os_failure("inspect", path, code))?
    {
        return Err(format!(
            "(data directory): expected {} to be owned by SYSTEM or Administrators | received: another owner. \
             Someone else created it; remove it and install again.",
            path.display()
        ));
    }

    let mut dacl: *mut ACL = ptr::null_mut();
    let (mut present, mut defaulted) = (0, 0);
    // SAFETY: `descriptor` is a valid security descriptor; the three
    // out-pointers are valid.
    let ok = unsafe {
        GetSecurityDescriptorDacl(descriptor.raw(), &mut present, &mut dacl, &mut defaulted)
    };
    if ok == 0 || present == 0 {
        // SAFETY: trivially safe; reads thread-local state.
        return Err(os_failure("read the ACL for", path, unsafe { GetLastError() }));
    }
    // SAFETY: `path_w` is NUL-terminated; `dacl` points into `descriptor`,
    // which outlives the call; owner, group and SACL are not being set.
    let status = unsafe {
        SetNamedSecurityInfoW(
            path_w.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            dacl,
            ptr::null(),
        )
    };
    if status == 0 { Ok(()) } else { Err(os_failure("secure", path, status)) }
}

fn is_owned_by_system_or_administrators(path_w: &[u16]) -> Result<bool, u32> {
    let mut owner: PSID = ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: `path_w` is NUL-terminated; the requested out-pointers are
    // valid and the others null.
    let status = unsafe {
        GetNamedSecurityInfoW(
            path_w.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(status);
    }
    // SAFETY: on success `owner` points into `descriptor`, which is alive
    // until the `LocalFree` below.
    let trusted = unsafe {
        IsWellKnownSid(owner, WinLocalSystemSid) != 0
            || IsWellKnownSid(owner, WinBuiltinAdministratorsSid) != 0
    };
    // SAFETY: `descriptor` was allocated by the call above; freed once.
    unsafe { LocalFree(descriptor) };
    Ok(trusted)
}

/// Stores documents as files: the profile in the root, everything else
/// under `state`.
#[derive(Debug, Clone)]
pub struct FileStore {
    dir: DataDir,
}

impl FileStore {
    /// A store on a directory prepared with [`DataDir::secure`].
    pub fn new(dir: DataDir) -> Self {
        Self { dir }
    }

    fn path_of(&self, name: &str) -> PathBuf {
        if name == PROFILE_NAME { self.dir.root().join(name) } else { self.dir.state().join(name) }
    }
}

impl StateStore for FileStore {
    fn load(&self, name: &str) -> Option<String> {
        fs::read_to_string(self.path_of(name)).ok()
    }

    fn save(&self, name: &str, content: &str) -> Result<(), String> {
        let path = self.path_of(name);
        let temporary = path.with_extension("tmp");
        // Write a sibling file and rename it over the target: a crash or a
        // power cut leaves either the old document or the new one.
        let written = fs::File::create(&temporary).and_then(|mut file| {
            file.write_all(content.as_bytes())?;
            file.sync_all()
        });
        written
            .and_then(|()| fs::rename(&temporary, &path))
            .map_err(|error| format!("save {}: {error}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::privilege::is_process_elevated;

    /// A unique directory under the system temp directory, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("SysPilot-Test-storage-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn data_dir_is_named_after_the_instance() {
        let dir = DataDir::for_instance(&Instance::named("SysPilot-Test").unwrap());
        assert!(dir.root().ends_with("SysPilot-Test"), "{}", dir.root().display());
        assert!(dir.state().ends_with(r"SysPilot-Test\state"));
        assert!(dir.logs().ends_with(r"SysPilot-Test\logs"));
        assert!(dir.updates().ends_with(r"SysPilot-Test\updates"));
    }

    #[test]
    fn secure_creates_the_directories_and_is_repeatable() {
        let temp = TempDir::new("secure");
        let dir = DataDir::at(temp.0.join("data"));
        fs::create_dir_all(&temp.0).unwrap();
        if !is_process_elevated() {
            // A standard user may not hand ownership to Administrators.
            let problem = dir.secure().unwrap_err();
            assert!(problem.starts_with("create "), "{problem}");
            return;
        }
        dir.secure().unwrap();
        assert!(dir.state().is_dir() && dir.logs().is_dir() && dir.updates().is_dir());
        // The second run meets the directories it created and accepts them.
        dir.secure().unwrap();
    }

    #[test]
    fn secure_refuses_a_directory_owned_by_an_ordinary_account() {
        let temp = TempDir::new("owner");
        fs::create_dir_all(&temp.0).unwrap();
        // Owned by the account running the tests, as a directory planted by
        // a standard user would be.
        let own_sid = syspilot_engine::win::process_user_sid().unwrap();
        let descriptor = SecurityDescriptor::from_sddl(&format!("O:{own_sid}")).unwrap();
        let path = temp.0.join("data");
        let path_w = wide(&path.to_string_lossy());
        // SAFETY: `path_w` is NUL-terminated; the attributes borrow
        // `descriptor`, which outlives the call.
        let created = unsafe { CreateDirectoryW(path_w.as_ptr(), &descriptor.attributes()) };
        assert_ne!(created, 0, "{}", io::Error::last_os_error());

        let problem = DataDir::at(path).secure().unwrap_err();
        assert!(problem.contains("owned by SYSTEM or Administrators"), "{problem}");
    }

    #[test]
    fn secure_refuses_a_file_in_place_of_the_directory() {
        let temp = TempDir::new("file");
        fs::create_dir_all(&temp.0).unwrap();
        let path = temp.0.join("data");
        fs::write(&path, "not a directory").unwrap();
        let problem = DataDir::at(path).secure().unwrap_err();
        assert!(problem.contains("expected a directory"), "{problem}");
    }

    #[test]
    fn store_round_trips_and_separates_profile_from_state() {
        let temp = TempDir::new("store");
        let dir = DataDir::at(temp.0.clone());
        fs::create_dir_all(dir.state()).unwrap();
        let store = FileStore::new(dir.clone());

        assert_eq!(store.load(PROFILE_NAME), None);
        store.save(PROFILE_NAME, "schema = 1\n").unwrap();
        store.save("applied.toml", "one").unwrap();
        store.save("applied.toml", "two").unwrap();

        assert_eq!(store.load(PROFILE_NAME).as_deref(), Some("schema = 1\n"));
        assert_eq!(store.load("applied.toml").as_deref(), Some("two"));
        assert!(dir.root().join(PROFILE_NAME).is_file());
        assert!(dir.state().join("applied.toml").is_file());
        assert!(!dir.state().join("applied.tmp").exists(), "no temporary file is left behind");
    }

    #[test]
    fn save_into_a_missing_directory_fails_with_the_path() {
        let temp = TempDir::new("missing");
        let store = FileStore::new(DataDir::at(temp.0.clone()));
        let problem = store.save("applied.toml", "x").unwrap_err();
        assert!(problem.starts_with("save ") && problem.contains("applied.toml"), "{problem}");
    }

    #[test]
    fn remove_deletes_everything_and_tolerates_absence() {
        let temp = TempDir::new("remove");
        let dir = DataDir::at(temp.0.clone());
        fs::create_dir_all(dir.state()).unwrap();
        fs::write(dir.state().join("x"), "x").unwrap();
        dir.remove().unwrap();
        assert!(!dir.root().exists());
        dir.remove().unwrap();
    }
}
