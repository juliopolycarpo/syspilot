//! Starting other things: an elevated copy of the app, the service
//! installer, a web page, a folder.

use std::io;
use std::path::Path;
use std::ptr::{null, null_mut};

use syspilot_proto::win::wide;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Argument [`relaunch_elevated`] adds, so the copy it starts knows it was
/// started for elevation and does not ask again if Windows did not elevate it.
pub const RELAUNCHED_ARGUMENT: &str = "--elevated-restart";

/// File name of the service executable, expected next to the app.
const SERVICE_EXECUTABLE: &str = "syspilot-svc.exe";

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

    #[test]
    fn missing_folder_is_reported_instead_of_opened() {
        let missing = open_folder(r"C:\SysPilot-Test.no-such-folder").unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
    }
}
