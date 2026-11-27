//! Starting other things: an elevated copy of the app, the service
//! installer, a web page, a folder.

use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::IntoRawHandle;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr::{null, null_mut};

use syspilot_proto::Instance;
use syspilot_proto::message::{ServiceStatus, UpdateState};
use syspilot_proto::update::COMPANION_EXECUTABLE;
use syspilot_proto::win::{OwnedHandle, is_owned_by_system_or_administrators, wide};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, READ_CONTROL};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Argument [`relaunch_elevated`] adds, so the copy it starts knows it was
/// started for elevation and does not ask again if Windows did not elevate it.
pub const RELAUNCHED_ARGUMENT: &str = "--elevated-restart";

/// File name of the service executable, expected next to the app.
const SERVICE_EXECUTABLE: &str = "syspilot-svc.exe";

/// The folder of the update companion's copy, in the data directory.
const COMPANION_FOLDER: &str = "companion";

/// File name of the window icon, expected next to the app.
const ICON_FILE: &str = "syspilot.ico";

/// Where the icon file is if it sits beside `executable`.
fn icon_beside(executable: &Path) -> Option<String> {
    let icon = executable.with_file_name(ICON_FILE);
    if icon.is_file() { icon.to_str().map(str::to_owned) } else { None }
}

/// The window icon's path, or `None` when the file is not beside the app
/// (the window then keeps the default icon). The toolkit wants a path
/// that lives as long as the program, so it is looked up once and kept.
pub fn window_icon() -> Option<&'static str> {
    static ICON: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    ICON.get_or_init(|| icon_beside(&std::env::current_exe().ok()?)).as_deref()
}

/// Quotes one argument the way `CommandLineToArgvW` splits it again.
pub fn quote_argument(argument: &str) -> String {
    if !argument.is_empty() && !argument.contains([' ', '\t', '"']) {
        return argument.to_owned();
    }
    let mut quoted = String::from('"');
    let mut backslashes = 0;
    for character in argument.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                // Backslashes before a quote would escape it: double them.
                quoted.extend(std::iter::repeat_n('\\', backslashes + 1));
                backslashes = 0;
            }
            _ => backslashes = 0,
        }
        quoted.push(character);
    }
    // Likewise before the closing quote.
    quoted.extend(std::iter::repeat_n('\\', backslashes));
    quoted.push('"');
    quoted
}

/// Joins arguments into one command-line tail.
pub fn join_arguments(arguments: &[String]) -> String {
    arguments.iter().map(|argument| quote_argument(argument)).collect::<Vec<_>>().join(" ")
}

/// Whether `link` is a web address the app is willing to open. Links come
/// from the catalog and the service; anything but HTTPS is not followed.
pub fn is_web_link(link: &str) -> bool {
    link.strip_prefix("https://")
        .is_some_and(|rest| !rest.is_empty() && !rest.contains(char::is_whitespace))
}

fn shell_execute(verb: &str, file: &str, parameters: &str) -> io::Result<()> {
    let (verb, file, parameters) = (wide(verb), wide(file), wide(parameters));
    // SAFETY: the three strings are NUL-terminated buffers that outlive the
    // call; the remaining pointers are optional and null.
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            parameters.as_ptr(),
            null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecute reports success as a value above 32.
    if result as usize > 32 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// Whether this process runs with an elevated (full administrator) token.
/// An administrator who has not elevated gets `false`, like a standard user.
pub fn is_elevated() -> bool {
    let mut token: HANDLE = null_mut();
    // SAFETY: `GetCurrentProcess` has no preconditions; it returns a
    // pseudo-handle that is always valid and needs no closing.
    let process = unsafe { GetCurrentProcess() };
    // SAFETY: `process` is the always-valid pseudo-handle from above; `token`
    // is a valid out-pointer.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut returned = 0u32;
    // SAFETY: `elevation` is writable for the size passed; `returned` is a
    // valid out-pointer; `token` is the open handle from above.
    let queried = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&raw mut elevation).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    };
    // SAFETY: `token` was opened above and is closed exactly once.
    unsafe { CloseHandle(token) };
    queried != 0 && elevation.TokenIsElevated != 0
}

/// The arguments for the elevated copy: the given ones plus
/// [`RELAUNCHED_ARGUMENT`], once.
fn elevated_arguments(arguments: &[String]) -> Vec<String> {
    let mut carried = arguments.to_vec();
    if !carried.iter().any(|argument| argument == RELAUNCHED_ARGUMENT) {
        carried.push(RELAUNCHED_ARGUMENT.to_owned());
    }
    carried
}

/// Starts this executable again with administrator rights and the given
/// arguments. Windows asks for consent; a refusal is an error.
pub fn relaunch_elevated(arguments: &[String]) -> io::Result<()> {
    let executable = std::env::current_exe()?;
    let carried = elevated_arguments(arguments);
    shell_execute("runas", &executable.to_string_lossy(), &join_arguments(&carried))
}

/// Runs the service executable that sits next to the app with
/// administrator rights, e.g. to install and start the service.
pub fn run_service_command_elevated(arguments: &[String]) -> io::Result<()> {
    let executable = std::env::current_exe()?.with_file_name(SERVICE_EXECUTABLE);
    if !executable.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{SERVICE_EXECUTABLE} is not next to the app"),
        ));
    }
    shell_execute("runas", &executable.to_string_lossy(), &join_arguments(arguments))
}

/// What the update companion is told about an update it is to follow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handover {
    /// The instance being updated.
    pub instance: Instance,
    /// The version that runs now.
    pub from: String,
    /// The version being installed, when a look has found it.
    pub to: Option<String>,
}

impl Handover {
    /// The handover for updating `instance`, whose service reports
    /// `status`. `None` for a copy that does not update itself.
    pub fn for_update(instance: &Instance, status: &ServiceStatus) -> Option<Self> {
        let to = match &status.update.state {
            UpdateState::Unavailable => return None,
            UpdateState::Available { version } => Some(version.clone()),
            _ => None,
        };
        Some(Self { instance: instance.clone(), from: status.version.clone(), to })
    }

    /// The companion's command line, for opening `app` again.
    fn arguments(&self, app: &Path) -> Vec<String> {
        let mut arguments = vec![
            "--instance".to_owned(),
            self.instance.name().to_owned(),
            "--from".to_owned(),
            self.from.clone(),
            "--app".to_owned(),
            app.display().to_string(),
        ];
        if let Some(to) = &self.to {
            arguments.extend(["--to".to_owned(), to.clone()]);
        }
        arguments
    }
}

/// The data directory of `instance`, which the service creates and locks
/// down so that only SYSTEM and administrators can write it.
fn data_directory(instance: &Instance) -> PathBuf {
    let program_data = std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into());
    Path::new(&program_data).join(instance.name())
}

/// Refuses a data directory that the service did not lock down: one whose
/// owner is neither SYSTEM nor Administrators.
///
/// Existing is not enough. `%ProgramData%` comes from this process's
/// environment, which the user can set without elevation, and any user can
/// make folders in the real one; an elevated companion run from a folder
/// the user owns would run whatever the user swapped in, or a DLL they put
/// beside it. The service gives its directory the Administrators owner, and
/// only an elevated process can.
fn require_privileged_owner(directory: &Path) -> io::Result<()> {
    // A directory opens only with backup semantics; reading its owner needs
    // no more than READ_CONTROL.
    let directory_file = std::fs::OpenOptions::new()
        .access_mode(READ_CONTROL)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(directory)?;
    let handle = OwnedHandle::new(directory_file.into_raw_handle() as HANDLE)?;
    if is_owned_by_system_or_administrators(&handle)? {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "(data directory {}): expected owner SYSTEM or Administrators | received: another owner",
                directory.display()
            ),
        ))
    }
}

/// Copies the companion at `source` into its folder in `data_directory`
/// and returns the copy's path.
///
/// The companion runs from there and not from the installation folder:
/// Windows Installer would find it holding a file it has to replace, and
/// leave the update for a restart.
fn place_companion(source: &Path, data_directory: &Path) -> io::Result<PathBuf> {
    if !source.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{COMPANION_EXECUTABLE} is not next to the app"),
        ));
    }
    let folder = data_directory.join(COMPANION_FOLDER);
    // Not `create_dir_all`: the data directory exists and is locked down
    // while the service runs. Were it missing, a folder made in its place
    // would take the open permissions of %ProgramData%, and an elevated
    // program must not run what any user could have put there.
    match std::fs::create_dir(&folder) {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
        _ => {}
    }
    let copy = folder.join(COMPANION_EXECUTABLE);
    // Fails while an earlier companion still runs from the copy; the
    // caller then goes on without one.
    std::fs::copy(source, &copy)?;
    Ok(copy)
}

/// Starts the update companion for `handover`, from a copy of the one next
/// to the app, with this app's rights.
pub fn start_update_companion(handover: &Handover) -> io::Result<()> {
    let app = std::env::current_exe()?;
    let source = app.with_file_name(COMPANION_EXECUTABLE);
    let data = data_directory(&handover.instance);
    require_privileged_owner(&data)?;
    let copy = place_companion(&source, &data)?;
    let mut command = Command::new(&copy);
    command.args(handover.arguments(&app));
    // Its working folder is its own, so that it holds nothing open in the
    // installation folder.
    if let Some(folder) = copy.parent() {
        command.current_dir(folder);
    }
    command.spawn().map(drop)
}

/// Opens an HTTPS link in the default browser.
pub fn open_web_link(link: &str) -> io::Result<()> {
    if !is_web_link(link) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("(link): expected an https:// address | received: {link:?}"),
        ));
    }
    shell_execute("open", link, "")
}

/// Opens a folder in File Explorer.
pub fn open_folder(folder: &str) -> io::Result<()> {
    if !Path::new(folder).is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("The folder {folder} does not exist yet."),
        ));
    }
    shell_execute("explore", folder, "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_icon_is_found_only_when_it_is_beside_the_executable() {
        let directory =
            std::env::temp_dir().join(format!("SysPilot-Test-icon-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let executable = directory.join("syspilot.exe");
        assert_eq!(icon_beside(&executable), None);

        let icon = directory.join(ICON_FILE);
        std::fs::write(&icon, b"icon").unwrap();
        assert_eq!(icon_beside(&executable).as_deref(), icon.to_str());
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn plain_arguments_stay_as_they_are() {
        assert_eq!(quote_argument("--instance"), "--instance");
        assert_eq!(quote_argument(r"C:\Tools\app.exe"), r"C:\Tools\app.exe");
    }

    #[test]
    fn arguments_with_spaces_quotes_or_nothing_are_quoted() {
        assert_eq!(quote_argument(""), r#""""#);
        assert_eq!(quote_argument("two words"), r#""two words""#);
        assert_eq!(quote_argument(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quote_argument(r"C:\Program Files\"), r#""C:\Program Files\\""#);
        assert_eq!(quote_argument(r#"a\"b"#), r#""a\\\"b""#);
    }

    #[test]
    fn arguments_join_with_single_spaces() {
        let arguments = ["--instance".to_owned(), "SysPilot-Test.ui".to_owned(), "a b".to_owned()];
        assert_eq!(join_arguments(&arguments), r#"--instance SysPilot-Test.ui "a b""#);
    }

    #[test]
    fn elevation_is_what_windows_says_of_this_process() {
        // `whoami /groups` lists the integrity level of the token: High
        // (S-1-16-12288) is an elevated administrator.
        let groups = std::process::Command::new("whoami").arg("/groups").output().unwrap();
        let high = String::from_utf8_lossy(&groups.stdout).contains("S-1-16-12288");
        assert_eq!(is_elevated(), high, "(elevation): whoami says high integrity: {high}");
    }

    #[test]
    fn the_elevated_copy_is_marked_once() {
        let given = ["--page".to_owned(), "sensors".to_owned()];
        let marked = elevated_arguments(&given);
        assert_eq!(marked, ["--page", "sensors", RELAUNCHED_ARGUMENT]);
        assert_eq!(elevated_arguments(&marked), marked, "marked twice");
    }

    #[test]
    fn only_https_links_are_followed() {
        assert!(is_web_link("https://pawnio.eu/"));
        assert!(!is_web_link("http://pawnio.eu/"));
        assert!(!is_web_link("https://"));
        assert!(!is_web_link("https://example.com/ & calc.exe"));
        assert!(!is_web_link(r"C:\Windows\System32\calc.exe"));
        assert!(!is_web_link("file:///C:/Windows"));

        let refused = open_web_link("calc.exe").unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::InvalidInput);
        assert!(refused.to_string().contains("received: \"calc.exe\""), "{refused}");
    }

    fn status(version: &str, state: UpdateState) -> ServiceStatus {
        let mut status = crate::demo::status(crate::demo::Scenario::Healthy);
        status.version = version.into();
        status.update.state = state;
        status
    }

    #[test]
    fn the_handover_names_both_versions_when_a_look_found_the_new_one() {
        let instance = Instance::named("SysPilot-Test.ui").unwrap();
        let found = UpdateState::Available { version: "0.3.1".into() };
        let handover = Handover::for_update(&instance, &status("0.3.0", found)).unwrap();
        assert_eq!(
            handover.arguments(Path::new(r"C:\Program Files\SysPilot\syspilot.exe")),
            [
                "--instance",
                "SysPilot-Test.ui",
                "--from",
                "0.3.0",
                "--app",
                r"C:\Program Files\SysPilot\syspilot.exe",
                "--to",
                "0.3.1",
            ]
        );
    }

    #[test]
    fn the_handover_leaves_the_target_out_when_it_is_not_known() {
        let instance = Instance::product();
        let handover = Handover::for_update(&instance, &status("0.3.0", UpdateState::Idle));
        let handover = handover.expect("(idle): expected a handover");
        assert_eq!(handover.to, None);
        assert!(!handover.arguments(Path::new("a.exe")).contains(&"--to".to_owned()));
    }

    #[test]
    fn a_copy_that_does_not_update_itself_hands_over_nothing() {
        let unavailable = status("0.3.0", UpdateState::Unavailable);
        assert_eq!(Handover::for_update(&Instance::product(), &unavailable), None);
    }

    #[test]
    fn the_data_directory_is_the_instances_under_program_data() {
        let directory = data_directory(&Instance::named("SysPilot-Test.ui").unwrap());
        let program_data = std::env::var_os("ProgramData").unwrap_or_default();
        assert_eq!(directory, Path::new(&program_data).join("SysPilot-Test.ui"));
    }

    /// A scratch directory with a stand-in companion, removed on drop.
    struct CompanionScratch {
        root: PathBuf,
        source: PathBuf,
    }

    impl CompanionScratch {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("SysPilot-Test.ui-companion-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let installed = root.join("installed");
            std::fs::create_dir_all(&installed).unwrap();
            let source = installed.join(COMPANION_EXECUTABLE);
            std::fs::write(&source, b"first build").unwrap();
            Self { root, source }
        }
    }

    impl Drop for CompanionScratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn the_companion_is_copied_into_its_folder_and_replaced_by_a_newer_copy() {
        let scratch = CompanionScratch::new("copied");
        let data = scratch.root.join("data");
        std::fs::create_dir(&data).unwrap();

        let copy = place_companion(&scratch.source, &data).unwrap();
        assert_eq!(copy, data.join(COMPANION_FOLDER).join(COMPANION_EXECUTABLE));
        assert_eq!(std::fs::read(&copy).unwrap(), b"first build");

        std::fs::write(&scratch.source, b"second build").unwrap();
        let again = place_companion(&scratch.source, &data).unwrap();
        assert_eq!(
            std::fs::read(&again).unwrap(),
            b"second build",
            "(second copy): expected the newer companion over the older one"
        );
    }

    #[test]
    fn no_data_directory_is_made_where_the_service_made_none() {
        let scratch = CompanionScratch::new("no-data");
        let data = scratch.root.join("data");
        let refused = place_companion(&scratch.source, &data).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::NotFound, "(missing data directory): {refused}");
        assert!(!data.exists(), "(missing data directory): expected it still missing");
    }

    #[test]
    fn a_data_directory_the_user_owns_is_refused() {
        // What a user makes, here or under a %ProgramData% they pointed
        // elsewhere: owned by their own account, elevated or not.
        let scratch = CompanionScratch::new("user-owned");
        let refused = require_privileged_owner(&scratch.root).unwrap_err();
        assert_eq!(
            refused.kind(),
            io::ErrorKind::PermissionDenied,
            "(user-owned data directory): expected PermissionDenied | received: {refused}"
        );
        assert!(
            refused.to_string().contains("expected owner SYSTEM or Administrators"),
            "{refused}"
        );
    }

    #[test]
    fn a_directory_owned_by_system_is_accepted() {
        // %ProgramData% itself belongs to SYSTEM on every Windows install.
        let program_data =
            std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into());
        let accepted = require_privileged_owner(Path::new(&program_data));
        assert!(accepted.is_ok(), "(%ProgramData%): expected accepted | received: {accepted:?}");
    }

    #[test]
    fn a_missing_data_directory_is_refused_as_missing() {
        let scratch = CompanionScratch::new("owner-missing");
        let refused = require_privileged_owner(&scratch.root.join("data")).unwrap_err();
        assert_eq!(
            refused.kind(),
            io::ErrorKind::NotFound,
            "(missing data directory): expected NotFound | received: {refused}"
        );
    }

    #[test]
    fn a_missing_companion_is_reported_by_name() {
        let scratch = CompanionScratch::new("missing");
        std::fs::remove_file(&scratch.source).unwrap();
        let refused = place_companion(&scratch.source, &scratch.root).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::NotFound);
        assert!(refused.to_string().contains(COMPANION_EXECUTABLE), "{refused}");
    }

    #[test]
    fn missing_folder_is_reported_instead_of_opened() {
        let missing = open_folder(r"C:\SysPilot-Test.no-such-folder").unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
    }
}
