//! Registering and removing the Windows service.
//!
//! `install` registers the executable where it is: copying it into place is
//! the installer script's job. `uninstall` puts every changed setting back
//! before it removes the service and its data.

use std::path::{Path, PathBuf};
use std::ptr;
use std::time::{Duration, Instant};
use std::{fs, io};

use syspilot_engine::win;
use syspilot_proto::message::{Request, Response};
use syspilot_proto::profile::Profile;
use syspilot_proto::win::wide;
use syspilot_proto::{APP_PREFERENCES_FILE, Client, Instance};
use windows_sys::Win32::Foundation::{
    ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_EXISTS,
    ERROR_SERVICE_NOT_ACTIVE, GetLastError,
};
use windows_sys::Win32::System::Services::{
    ChangeServiceConfig2W, ChangeServiceConfigW, CloseServiceHandle, ControlService,
    CreateServiceW, DeleteService, OpenSCManagerW, OpenServiceW, QueryServiceStatus, SC_ACTION,
    SC_ACTION_NONE, SC_ACTION_RESTART, SC_HANDLE, SC_MANAGER_CONNECT, SC_MANAGER_CREATE_SERVICE,
    SERVICE_ALL_ACCESS, SERVICE_AUTO_START, SERVICE_CONFIG_DESCRIPTION,
    SERVICE_CONFIG_FAILURE_ACTIONS, SERVICE_CONTROL_STOP, SERVICE_DESCRIPTIONW,
    SERVICE_ERROR_NORMAL, SERVICE_FAILURE_ACTIONSW, SERVICE_NO_CHANGE, SERVICE_RUNNING,
    SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE, SERVICE_STOPPED, SERVICE_WIN32_OWN_PROCESS,
    StartServiceW,
};

use crate::catalog;
use crate::core::Core;
use crate::privilege::is_process_elevated;
use crate::runtime::{NEEDS_ELEVATION, os_build};
use crate::storage::{DataDir, FileStore};

const DESCRIPTION: &str = "Keeps the settings you chose in SysPilot applied and serves hardware \
    readings to the SysPilot app. Stopping it leaves settings as they are but no longer restores them.";
/// How long to wait for the service to start or stop.
const STATE_TIMEOUT: Duration = Duration::from_secs(30);

/// A service control manager handle, closed on drop.
struct ScHandle(SC_HANDLE);

impl Drop for ScHandle {
    fn drop(&mut self) {
        // SAFETY: the handle is valid (checked at construction) and closed
        // exactly once.
        unsafe { CloseServiceHandle(self.0) };
    }
}

fn last_error() -> u32 {
    // SAFETY: trivially safe; reads thread-local state.
    unsafe { GetLastError() }
}

fn failure(action: &str, code: u32) -> String {
    format!("{action}: {}", io::Error::from_raw_os_error(code as i32))
}

fn open_manager() -> Result<ScHandle, String> {
    // SAFETY: null names select the local machine and the active database.
    let manager = unsafe {
        OpenSCManagerW(ptr::null(), ptr::null(), SC_MANAGER_CONNECT | SC_MANAGER_CREATE_SERVICE)
    };
    if manager.is_null() {
        Err(failure("open the service control manager", last_error()))
    } else {
        Ok(ScHandle(manager))
    }
}

/// Opens the service of `instance`, or `None` when it is not installed.
fn open_service(manager: &ScHandle, instance: &Instance) -> Result<Option<ScHandle>, String> {
    let name = wide(instance.name());
    // SAFETY: `manager` is valid and `name` NUL-terminated.
    let service = unsafe { OpenServiceW(manager.0, name.as_ptr(), SERVICE_ALL_ACCESS) };
    if !service.is_null() {
        return Ok(Some(ScHandle(service)));
    }
    match last_error() {
        ERROR_SERVICE_DOES_NOT_EXIST => Ok(None),
        code => Err(failure("open the service", code)),
    }
}

fn current_state(service: &ScHandle) -> Result<SERVICE_STATUS_CURRENT_STATE, String> {
    // SAFETY: all-zero is a valid `SERVICE_STATUS`.
    let mut status: SERVICE_STATUS = unsafe { std::mem::zeroed() };
    // SAFETY: `service` is valid and `status` a valid out-pointer.
    if unsafe { QueryServiceStatus(service.0, &mut status) } == 0 {
        return Err(failure("query the service", last_error()));
    }
    Ok(status.dwCurrentState)
}

/// Polls until the service reaches `wanted`. Polling is fine here: this is
/// a command the user is watching, not the resident service.
fn wait_for_state(service: &ScHandle, wanted: SERVICE_STATUS_CURRENT_STATE) -> Result<(), String> {
    let deadline = Instant::now() + STATE_TIMEOUT;
    loop {
        let state = current_state(service)?;
        if state == wanted {
            return Ok(());
        }
        // A service that was asked to start and is stopped again has failed.
        if wanted == SERVICE_RUNNING && state == SERVICE_STOPPED {
            return Err("The service stopped right after starting. The Windows Application \
                event log has the reason."
                .into());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "(service state): expected {wanted} within {} seconds | received: {state}",
                STATE_TIMEOUT.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The command line Windows runs to start the service.
fn service_command_line(executable: &Path, instance: &Instance) -> String {
    format!("\"{}\" service --instance {}", executable.display(), instance.name())
}

/// Whether `executable` may be registered for `instance`.
///
/// The product service must run from under Program Files, where only
/// administrators can replace the file: a LocalSystem service whose binary
/// a standard user can overwrite hands that user the machine. Named
/// instances exist for development and tests and are not restricted.
fn check_location(
    executable: &Path,
    instance: &Instance,
    program_files: &[PathBuf],
) -> Result<(), String> {
    if !instance.is_product() || program_files.iter().any(|root| executable.starts_with(root)) {
        return Ok(());
    }
    Err(format!(
        "(install location): expected the executable under Program Files | received: {}. \
         Copy it there first (the install script does), or pass --instance for a test instance.",
        executable.display()
    ))
}

fn program_files_roots() -> Vec<PathBuf> {
    ["ProgramFiles", "ProgramW6432"]
        .iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .collect()
}

fn configure(service: &ScHandle) -> Result<(), String> {
    let mut description = wide(DESCRIPTION);
    let info = SERVICE_DESCRIPTIONW { lpDescription: description.as_mut_ptr() };
    // SAFETY: `service` is valid; `info` matches the info level and points
    // at a NUL-terminated string that outlives the call.
    if unsafe {
        ChangeServiceConfig2W(service.0, SERVICE_CONFIG_DESCRIPTION, ptr::from_ref(&info).cast())
    } == 0
    {
        return Err(failure("set the service description", last_error()));
    }
    // Restart twice after a crash, then stay down: a service that keeps
    // crashing should not keep re-applying settings. The count resets after
    // a day without a crash.
    let mut actions = [
        SC_ACTION { Type: SC_ACTION_RESTART, Delay: 5_000 },
        SC_ACTION { Type: SC_ACTION_RESTART, Delay: 30_000 },
        SC_ACTION { Type: SC_ACTION_NONE, Delay: 0 },
    ];
    let recovery = SERVICE_FAILURE_ACTIONSW {
        dwResetPeriod: 86_400,
        lpRebootMsg: ptr::null_mut(),
        lpCommand: ptr::null_mut(),
        cActions: actions.len() as u32,
        lpsaActions: actions.as_mut_ptr(),
    };
    // SAFETY: `service` is valid; `recovery` matches the info level and
    // points at `actions`, which outlives the call.
    if unsafe {
        ChangeServiceConfig2W(
            service.0,
            SERVICE_CONFIG_FAILURE_ACTIONS,
            ptr::from_ref(&recovery).cast(),
        )
    } == 0
    {
        return Err(failure("set the service recovery actions", last_error()));
    }
    Ok(())
}

/// Registers the running executable as the service of `instance` (or
/// updates an existing registration) and starts it.
pub fn install(instance: &Instance, start: bool) -> Result<String, String> {
    if !is_process_elevated() {
        return Err(NEEDS_ELEVATION.into());
    }
    let executable =
        std::env::current_exe().map_err(|error| format!("locate this executable: {error}"))?;
    check_location(&executable, instance, &program_files_roots())?;
    DataDir::for_instance(instance).secure()?;

    let manager = open_manager()?;
    let name = wide(instance.name());
    let command_line = wide(&service_command_line(&executable, instance));
    // SAFETY: `manager` is valid; every string is NUL-terminated and
    // outlives the call; null account and password mean LocalSystem.
    let created = unsafe {
        CreateServiceW(
            manager.0,
            name.as_ptr(),
            name.as_ptr(),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            command_line.as_ptr(),
            ptr::null(),
            ptr::null_mut(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
        )
    };
    let (service, verb) = if !created.is_null() {
        (ScHandle(created), "installed")
    } else {
        match last_error() {
            ERROR_SERVICE_EXISTS => {}
            code => return Err(failure("create the service", code)),
        }
        let Some(service) = open_service(&manager, instance)? else {
            return Err("The service is being removed. Try again in a moment.".into());
        };
        // SAFETY: `service` is valid; only the type, start type and binary
        // path are changed, everything else is left as it is.
        let changed = unsafe {
            ChangeServiceConfigW(
                service.0,
                SERVICE_WIN32_OWN_PROCESS,
                SERVICE_AUTO_START,
                SERVICE_NO_CHANGE,
                command_line.as_ptr(),
                ptr::null(),
                ptr::null_mut(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
            )
        };
        if changed == 0 {
            return Err(failure("update the service", last_error()));
        }
        (service, "updated")
    };
    configure(&service)?;

    if !start {
        return Ok(format!("{} service {verb}.", instance.name()));
    }
    // SAFETY: `service` is valid; no arguments are passed.
    if unsafe { StartServiceW(service.0, 0, ptr::null()) } == 0 {
        match last_error() {
            ERROR_SERVICE_ALREADY_RUNNING => {}
            code => return Err(failure("start the service", code)),
        }
    }
    wait_for_state(&service, SERVICE_RUNNING)?;
    Ok(format!("{} service {verb} and running.", instance.name()))
}

/// Stops the service of `instance` if it is installed and running.
fn stop(service: &ScHandle) -> Result<(), String> {
    // SAFETY: all-zero is a valid `SERVICE_STATUS`.
    let mut status: SERVICE_STATUS = unsafe { std::mem::zeroed() };
    // SAFETY: `service` is valid and `status` a valid out-pointer.
    if unsafe { ControlService(service.0, SERVICE_CONTROL_STOP, &mut status) } == 0 {
        match last_error() {
            ERROR_SERVICE_NOT_ACTIVE => return Ok(()),
            code => return Err(failure("stop the service", code)),
        }
    }
    wait_for_state(service, SERVICE_STOPPED)
}

/// Whether a console run of `instance` is serving requests.
///
/// A pipe that merely accepts a connection is not enough to tell: a service
/// that has just reported itself stopped keeps its pipe until the process
/// is gone, but it no longer answers. Only an answer counts.
pub(crate) fn console_run_is_active(instance: &Instance) -> bool {
    Client::connect(instance)
        .and_then(|mut client| client.status())
        .is_ok_and(|status| status.console)
}

/// Puts back everything the service changed. Returns how many settings
/// could not be restored now.
fn revert_everything(instance: &Instance, data: &DataDir) -> Result<usize, String> {
    let catalog = catalog::builtin().map_err(|error| format!("built-in catalog: {error}"))?;
    let store = FileStore::new(data.clone());
    let mut core = Core::new(win::os(), store, catalog, os_build(), false);
    match core.handle(Request::SetProfile(Profile::default()), true).0 {
        Response::Ok => Ok(core.enforcer().pending_reverts()),
        Response::Error(error) => {
            Err(format!("restore the settings of {}: {error}", instance.name()))
        }
        _ => Err("(revert): expected Ok or an error from the core".into()),
    }
}

/// Where Windows keeps the user profiles, e.g. `C:\Users`.
fn profiles_root() -> PathBuf {
    // The public profile sits beside the others, and its place is in the
    // environment of every account, LocalSystem's included.
    std::env::var_os("PUBLIC")
        .map(PathBuf::from)
        .and_then(|public| public.parent().map(Path::to_owned))
        .unwrap_or_else(|| {
            let mut root = std::env::var_os("SystemDrive").unwrap_or_else(|| "C:".into());
            root.push(r"\Users");
            PathBuf::from(root)
        })
}

/// Whether `path` is a directory in its own right, not a link to one.
fn is_plain_directory(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
}

/// Removes what the app of `instance` kept for each user under
/// `profiles`: its preferences file and the directory around it. Returns
/// how many users had one.
///
/// This runs as an administrator or LocalSystem inside directories their
/// owners control, so it follows no link (a junction in the path could
/// point anywhere) and deletes one file by name and its directory only if
/// that leaves it empty.
fn remove_user_preferences(instance: &Instance, profiles: &Path) -> usize {
    let Ok(users) = fs::read_dir(profiles) else { return 0 };
    let mut removed = 0;
    for user in users.flatten() {
        let mut directory = user.path();
        // Every step of the way, the profile directory itself included.
        let plain = is_plain_directory(&directory)
            && ["AppData", "Local", instance.name()].iter().all(|part| {
                directory.push(part);
                is_plain_directory(&directory)
            });
        let file = directory.join(APP_PREFERENCES_FILE);
        let is_file = fs::symlink_metadata(&file).is_ok_and(|metadata| metadata.is_file());
        if plain && is_file && fs::remove_file(&file).is_ok() {
            removed += 1;
            // Fails when anything else is in it, which is then not ours.
            let _ = fs::remove_dir(&directory);
        }
    }
    removed
}

/// Stops and removes the service of `instance` and deletes its data: the
/// profile, the state, the logs, downloaded updates and what the app kept
/// for each user. Unless `keep_tweaks` is set, every setting it changed is
/// restored first.
pub fn uninstall(instance: &Instance, keep_tweaks: bool) -> Result<String, String> {
    if !is_process_elevated() {
        return Err(NEEDS_ELEVATION.into());
    }
    let manager = open_manager()?;
    let service = open_service(&manager, instance)?;
    if let Some(service) = &service {
        stop(service)?;
    }
    // A console run would keep enforcing while this reverts.
    if console_run_is_active(instance) {
        return Err(format!(
            "{} is running in a console. Stop it there first (Ctrl+C).",
            instance.name()
        ));
    }

    let data = DataDir::for_instance(instance);
    let mut notes = Vec::new();
    if !keep_tweaks && data.root().is_dir() {
        // Only act on state from a directory nobody else could have written.
        match data.secure() {
            Ok(()) => {
                let pending = revert_everything(instance, &data)?;
                if pending > 0 {
                    notes.push(format!(
                        "{pending} setting(s) could not be restored now (they belong to users \
                         who are not signed in, or Windows refused the change)."
                    ));
                }
            }
            Err(problem) => notes.push(format!("Settings were not restored. {problem}")),
        }
    }
    if let Some(service) = &service {
        // SAFETY: `service` is valid.
        if unsafe { DeleteService(service.0) } == 0 {
            return Err(failure("remove the service", last_error()));
        }
    }
    data.remove().map_err(|error| format!("remove {}: {error}", data.root().display()))?;
    remove_user_preferences(instance, &profiles_root());

    let summary = match (service.is_some(), keep_tweaks) {
        (true, false) => "service removed and settings restored.",
        (true, true) => "service removed. Settings were left as they are.",
        (false, _) => "was not installed as a service; its data was removed.",
    };
    notes.insert(0, format!("{} {summary}", instance.name()));
    Ok(notes.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots() -> Vec<PathBuf> {
        vec![PathBuf::from(r"C:\Program Files")]
    }

    #[test]
    fn command_line_quotes_the_path_and_names_the_instance() {
        let instance = Instance::named("SysPilot-Test").unwrap();
        let line = service_command_line(
            Path::new(r"C:\Program Files\SysPilot\syspilot-svc.exe"),
            &instance,
        );
        assert_eq!(
            line,
            r#""C:\Program Files\SysPilot\syspilot-svc.exe" service --instance SysPilot-Test"#
        );
    }

    #[test]
    fn product_service_must_live_under_program_files() {
        let product = Instance::product();
        let inside = Path::new(r"C:\Program Files\SysPilot\syspilot-svc.exe");
        let outside = Path::new(r"C:\Users\someone\Downloads\syspilot-svc.exe");
        assert_eq!(check_location(inside, &product, &roots()), Ok(()));
        let problem = check_location(outside, &product, &roots()).unwrap_err();
        assert!(
            problem.starts_with("(install location): expected the executable under Program Files"),
            "{problem}"
        );
        assert!(problem.contains(r"C:\Users\someone\Downloads\syspilot-svc.exe"), "{problem}");
    }

    #[test]
    fn similarly_named_directory_is_not_program_files() {
        let impostor = Path::new(r"C:\Program Files Evil\syspilot-svc.exe");
        assert!(check_location(impostor, &Instance::product(), &roots()).is_err());
    }

    #[test]
    fn test_instances_may_run_from_anywhere() {
        let instance = Instance::named("SysPilot-Test").unwrap();
        let outside = Path::new(r"C:\src\syspilot\target\release\syspilot-svc.exe");
        assert_eq!(check_location(outside, &instance, &roots()), Ok(()));
    }

    #[test]
    fn program_files_is_known_on_this_machine() {
        assert!(!program_files_roots().is_empty());
    }

    /// A stand-in for `C:\Users` under the system temp directory, removed
    /// on drop.
    struct FakeProfiles(PathBuf);

    impl FakeProfiles {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("SysPilot-Test-profiles-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        /// The app's directory of `user` for `instance`, created.
        fn app_directory(&self, user: &str, instance: &str) -> PathBuf {
            let directory = self.0.join(user).join("AppData").join("Local").join(instance);
            fs::create_dir_all(&directory).unwrap();
            directory
        }
    }

    impl Drop for FakeProfiles {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn uninstalling_removes_each_users_preferences_and_nothing_else() {
        let profiles = FakeProfiles::new("preferences");
        let instance = Instance::named("SysPilot-Test.profiles").unwrap();
        let ana = profiles.app_directory("ana", instance.name());
        let bruno = profiles.app_directory("bruno", instance.name());
        let other = profiles.app_directory("ana", "SomethingElse");
        let never_ran = profiles.app_directory("carla", "Unrelated");
        for directory in [&ana, &bruno, &other] {
            fs::write(directory.join(APP_PREFERENCES_FILE), "level = \"simple\"\n").unwrap();
        }
        // Something that is not the app's, in the app's directory.
        fs::write(bruno.join("notes.txt"), "mine").unwrap();

        assert_eq!(remove_user_preferences(&instance, &profiles.0), 2);
        assert!(!ana.exists(), "an emptied directory goes too");
        assert!(!bruno.join(APP_PREFERENCES_FILE).exists());
        assert!(bruno.join("notes.txt").is_file(), "(uninstall): expected a foreign file kept");
        assert!(other.join(APP_PREFERENCES_FILE).is_file(), "another program's file is kept");
        assert!(never_ran.is_dir());
        assert_eq!(remove_user_preferences(&instance, &profiles.0), 0, "nothing is left");
    }

    #[test]
    fn uninstalling_does_not_follow_a_link_in_a_users_profile() {
        let profiles = FakeProfiles::new("link");
        let instance = Instance::named("SysPilot-Test.profiles").unwrap();
        // Where a user would like LocalSystem to delete a file for them.
        let target = profiles.0.join("elsewhere");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join(APP_PREFERENCES_FILE), "not the app's").unwrap();
        let local = profiles.0.join("mallory").join("AppData").join("Local");
        fs::create_dir_all(&local).unwrap();
        let link = local.join(instance.name());
        let linked = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .output()
            .unwrap();
        assert!(linked.status.success(), "(fixture): expected a junction | received: {linked:?}");

        assert_eq!(remove_user_preferences(&instance, &profiles.0), 0);
        assert!(target.join(APP_PREFERENCES_FILE).is_file(), "the link's target is untouched");
    }

    #[test]
    fn the_profiles_directory_holds_the_public_profile() {
        assert!(profiles_root().join("Public").is_dir(), "{}", profiles_root().display());
    }
}
