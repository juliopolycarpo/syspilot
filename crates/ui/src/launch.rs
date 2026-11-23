//! Starting other things: an elevated copy of the app, the service
//! installer, a web page, a folder.

use std::io;
use std::path::Path;
use std::ptr::null;

use syspilot_proto::win::wide;
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// File name of the service executable, expected next to the app.
const SERVICE_EXECUTABLE: &str = "syspilot-svc.exe";

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

/// Starts this executable again with administrator rights and the given
/// arguments. Windows asks for consent; a refusal is an error.
pub fn relaunch_elevated(arguments: &[String]) -> io::Result<()> {
    let executable = std::env::current_exe()?;
    shell_execute("runas", &executable.to_string_lossy(), &join_arguments(arguments))
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
