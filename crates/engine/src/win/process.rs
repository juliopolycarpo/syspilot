//! Command execution and logged-on users.

use std::ffi::c_void;
use std::ptr;
use std::time::Duration;

use syspilot_proto::action::{CommandLine, RunAs};
use syspilot_proto::win::{OwnedHandle, wide};
use windows_sys::Win32::Foundation::{
    ERROR_NO_TOKEN, ERROR_PRIVILEGE_NOT_HELD, GetLastError, HANDLE, LocalFree, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows_sys::Win32::System::Environment::{
    CreateEnvironmentBlock, DestroyEnvironmentBlock, ExpandEnvironmentStringsW,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::RemoteDesktop::{
    WTS_SESSION_INFOW, WTSActive, WTSEnumerateSessionsW, WTSFreeMemory, WTSQueryUserToken,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW,
    CreateProcessW, GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, PROCESS_INFORMATION,
    ResumeThread, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};

use super::{last_error, os_error};
use crate::os::{Launcher, OsError, OsErrorKind, Sessions};

/// Runs commands with `CreateProcess`, as the service account or as the
/// active interactive user.
pub struct WinLauncher;

/// Logged-on users from the Remote Desktop Services session list.
pub struct WinSessions;

/// A user environment block, destroyed on drop.
struct EnvironmentBlock(*mut c_void);

impl Drop for EnvironmentBlock {
    fn drop(&mut self) {
        // SAFETY: the block came from `CreateEnvironmentBlock` and is
        // destroyed exactly once.
        unsafe { DestroyEnvironmentBlock(self.0) };
    }
}

/// Quotes one argument so that the C runtime of the child parses it back
/// to exactly `argument` (the `CommandLineToArgvW` rules).
fn quote_argument(argument: &str, out: &mut String) {
    let needs_quotes = argument.is_empty() || argument.contains([' ', '\t', '\n', '\x0b', '"']);
    if !needs_quotes {
        out.push_str(argument);
        return;
    }
    out.push('"');
    let mut backslashes = 0;
    for character in argument.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                // Backslashes before a quote are doubled, and the quote
                // itself is escaped.
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            other => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(other);
                backslashes = 0;
            }
        }
    }
    // Backslashes before the closing quote are doubled so it stays a quote.
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
}

/// The full command line: the quoted program followed by quoted arguments.
fn build_command_line(program: &str, arguments: &[String]) -> String {
    let mut line = String::new();
    quote_argument(program, &mut line);
    for argument in arguments {
        line.push(' ');
        quote_argument(argument, &mut line);
    }
    line
}

/// Expands `%Variable%` references using this process's environment.
fn expand_environment(text: &str) -> Result<String, OsError> {
    let source = wide(text);
    let mut buffer = vec![0u16; 512];
    loop {
        // SAFETY: `source` is NUL-terminated; `buffer` is valid for writes
        // of the length passed.
        let needed = unsafe {
            ExpandEnvironmentStringsW(source.as_ptr(), buffer.as_mut_ptr(), buffer.len() as u32)
        } as usize;
        if needed == 0 {
            return Err(last_error(format!("expand {text}")));
        }
        if needed <= buffer.len() {
            // `needed` counts the terminating NUL.
            return Ok(String::from_utf16_lossy(&buffer[..needed - 1]));
        }
        buffer.resize(needed, 0);
    }
}

/// The primary token of a logged-on user, preferring the active session.
/// `Ok(None)` when this process may not obtain user tokens (it is not
/// running as LocalSystem, i.e. console mode).
fn active_user_token() -> Result<Option<OwnedHandle>, OsError> {
    let mut sessions = session_ids();
    // Active sessions first: that is the user in front of the machine.
    sessions.sort_by_key(|(_, active)| !active);
    for (session, _) in sessions {
        let mut token: HANDLE = ptr::null_mut();
        // SAFETY: `token` is a valid out-pointer.
        if unsafe { WTSQueryUserToken(session, &mut token) } != 0 {
            return OwnedHandle::new(token).map(Some).map_err(|error| {
                os_error("query the user token", error.raw_os_error().unwrap_or(0) as u32)
            });
        }
        // SAFETY: trivially safe; reads thread-local state.
        if unsafe { GetLastError() } == ERROR_PRIVILEGE_NOT_HELD {
            return Ok(None);
        }
    }
    Err(OsError::new(OsErrorKind::NotFound, "run as user: no user is logged on", ERROR_NO_TOKEN))
}

/// IDs of all sessions, with whether each is the active one.
fn session_ids() -> Vec<(u32, bool)> {
    let mut info: *mut WTS_SESSION_INFOW = ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: a null server handle is the local server; both out-pointers
    // are valid.
    if unsafe { WTSEnumerateSessionsW(ptr::null_mut(), 0, 1, &mut info, &mut count) } == 0 {
        return Vec::new();
    }
    // SAFETY: on success `info` points to `count` initialised entries.
    let sessions = unsafe { std::slice::from_raw_parts(info, count as usize) }
        .iter()
        .map(|session| (session.SessionId, session.State == WTSActive))
        .collect();
    // SAFETY: `info` was allocated by `WTSEnumerateSessionsW` and is freed
    // exactly once, after the slice above is no longer used.
    unsafe { WTSFreeMemory(info.cast()) };
    sessions
}

/// The string SID of the user a token belongs to.
fn token_user_sid(token: &OwnedHandle) -> Option<String> {
    // TOKEN_USER is followed by the SID it points to; 256 bytes hold any
    // SID. `u64` elements give the buffer the alignment the structure needs.
    let mut buffer = [0u64; 32];
    let mut returned = 0u32;
    // SAFETY: `buffer` is writable for the size passed and suitably
    // aligned; `returned` is a valid out-pointer.
    let ok = unsafe {
        GetTokenInformation(
            token.raw(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            size_of_val(&buffer) as u32,
            &mut returned,
        )
    };
    if ok == 0 {
        return None;
    }
    // SAFETY: the call succeeded, so the buffer starts with an initialised
    // `TOKEN_USER` whose SID pointer refers into the same buffer.
    let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut text: *mut u16 = ptr::null_mut();
    // SAFETY: `sid` is a valid SID (see above); `text` is a valid
    // out-pointer.
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return None;
    }
    // SAFETY: on success `text` is a NUL-terminated string; the loop stops
    // at the terminator.
    let length = unsafe { (0..).take_while(|&index| *text.add(index) != 0).count() };
    // SAFETY: `text` is valid for `length` UTF-16 units (measured above).
    let sid_string = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, length) });
    // SAFETY: `text` was allocated with `LocalAlloc` by the conversion and
    // is freed exactly once, after its last use.
    unsafe { LocalFree(text.cast()) };
    Some(sid_string)
}

/// The SID of the user this process runs as.
pub fn process_user_sid() -> Option<String> {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: the pseudo-handle from `GetCurrentProcess` is always valid;
    // `token` is a valid out-pointer.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return None;
    }
    token_user_sid(&OwnedHandle::new(token).ok()?)
}

/// The SID of the user logged on to `session`, if any.
///
/// Outside LocalSystem (console mode) user tokens cannot be queried; the
/// user running this process stands in, so development runs behave like a
/// service with exactly one logged-on user.
pub fn session_user_sid(session: u32) -> Option<String> {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: `token` is a valid out-pointer.
    if unsafe { WTSQueryUserToken(session, &mut token) } != 0 {
        return token_user_sid(&OwnedHandle::new(token).ok()?);
    }
    // SAFETY: trivially safe; reads thread-local state.
    if unsafe { GetLastError() } == ERROR_PRIVILEGE_NOT_HELD {
        return process_user_sid();
    }
    None
}

impl Sessions for WinSessions {
    fn interactive_user_sids(&self) -> Vec<String> {
        let mut sids: Vec<String> = session_ids()
            .into_iter()
            .filter_map(|(session, _)| session_user_sid(session))
            .collect();
        sids.sort();
        sids.dedup();
        sids
    }
}

impl Launcher for WinLauncher {
    fn run(&self, command: &CommandLine, run_as: RunAs, timeout: Duration) -> Result<i32, OsError> {
        let program = expand_environment(&command.program)?;
        let context = || format!("run {program}");
        let application = wide(&program);
        // `CreateProcessW` may modify the command line buffer in place.
        let mut command_line = wide(&build_command_line(&program, &command.args));

        let user_token = match run_as {
            RunAs::System => None,
            RunAs::User => active_user_token()?,
        };
        let environment = match &user_token {
            Some(token) => {
                let mut block: *mut c_void = ptr::null_mut();
                // SAFETY: `block` is a valid out-pointer; `token` is a valid
                // primary token.
                if unsafe { CreateEnvironmentBlock(&mut block, token.raw(), 0) } == 0 {
                    return Err(last_error(context()));
                }
                Some(EnvironmentBlock(block))
            }
            None => None,
        };

        // A job that kills its processes when closed: a timed-out command
        // cannot leave children behind.
        // SAFETY: null attributes and name create an anonymous job.
        let job = OwnedHandle::new(unsafe { CreateJobObjectW(ptr::null(), ptr::null()) })
            .map_err(|_| last_error(context()))?;
        // SAFETY: the structure is plain data for which all-zero is valid.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `limits` matches the information class and outlives the
        // call; the size passed is its size.
        let ok = unsafe {
            SetInformationJobObject(
                job.raw(),
                JobObjectExtendedLimitInformation,
                ptr::from_ref(&limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(last_error(context()));
        }

        let mut desktop = wide(r"winsta0\default");
        // SAFETY: the structure is plain data for which all-zero is valid.
        let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
        startup.cb = size_of::<STARTUPINFOW>() as u32;
        if user_token.is_some() {
            // The interactive desktop of the user's session.
            startup.lpDesktop = desktop.as_mut_ptr();
        }
        // SAFETY: the structure is plain data for which all-zero is valid.
        let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let flags = CREATE_SUSPENDED | CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT;
        let environment_pointer =
            environment.as_ref().map_or(ptr::null(), |block| block.0.cast_const());
        // SAFETY: `application` and `command_line` are NUL-terminated and
        // outlive the call; `startup` and `process` are valid; the
        // environment block, when present, is a valid Unicode block; the
        // token, when present, is a valid primary token.
        let created = unsafe {
            match &user_token {
                Some(token) => CreateProcessAsUserW(
                    token.raw(),
                    application.as_ptr(),
                    command_line.as_mut_ptr(),
                    ptr::null(),
                    ptr::null(),
                    0,
                    flags,
                    environment_pointer,
                    ptr::null(),
                    &startup,
                    &mut process,
                ),
                None => CreateProcessW(
                    application.as_ptr(),
                    command_line.as_mut_ptr(),
                    ptr::null(),
                    ptr::null(),
                    0,
                    flags,
                    environment_pointer,
                    ptr::null(),
                    &startup,
                    &mut process,
                ),
            }
        };
        if created == 0 {
            return Err(last_error(context()));
        }
        let process_handle =
            OwnedHandle::new(process.hProcess).map_err(|_| last_error(context()))?;
        let thread_handle = OwnedHandle::new(process.hThread).map_err(|_| last_error(context()))?;

        // The process is still suspended, so it cannot spawn a child that
        // escapes the job. If assignment fails the process itself is still
        // terminated on timeout.
        // SAFETY: both handles are valid and owned here.
        unsafe { AssignProcessToJobObject(job.raw(), process_handle.raw()) };
        // SAFETY: `thread_handle` is the suspended primary thread.
        unsafe { ResumeThread(thread_handle.raw()) };

        let timeout_ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
        // SAFETY: `process_handle` is a valid process handle.
        match unsafe { WaitForSingleObject(process_handle.raw(), timeout_ms) } {
            WAIT_OBJECT_0 => {
                let mut exit_code = 0u32;
                // SAFETY: `process_handle` is valid; `exit_code` is a valid
                // out-pointer.
                if unsafe { GetExitCodeProcess(process_handle.raw(), &mut exit_code) } == 0 {
                    return Err(last_error(context()));
                }
                Ok(exit_code as i32)
            }
            WAIT_TIMEOUT => {
                // SAFETY: both handles are valid; terminating an already
                // exited process is harmless.
                unsafe {
                    TerminateJobObject(job.raw(), 1);
                    TerminateProcess(process_handle.raw(), 1);
                }
                Err(OsError::new(
                    OsErrorKind::Timeout,
                    format!("{}: still running after {} s", context(), timeout.as_secs()),
                    WAIT_TIMEOUT,
                ))
            }
            _ => Err(last_error(context())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quoted(argument: &str) -> String {
        let mut out = String::new();
        quote_argument(argument, &mut out);
        out
    }

    #[test]
    fn plain_arguments_are_not_quoted() {
        assert_eq!(quoted("/flushdns"), "/flushdns");
        assert_eq!(quoted(r"C:\path\file.txt"), r"C:\path\file.txt");
    }

    #[test]
    fn arguments_with_spaces_or_quotes_are_quoted_and_escaped() {
        assert_eq!(quoted(""), r#""""#);
        assert_eq!(quoted("a b"), r#""a b""#);
        assert_eq!(quoted(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quoted(r"C:\dir with space\"), r#""C:\dir with space\\""#);
        assert_eq!(quoted(r#"a\"b"#), r#""a\\\"b""#);
    }

    #[test]
    fn command_line_starts_with_the_quoted_program() {
        let line = build_command_line(r"C:\Program Files\x.exe", &["/a".into(), "b c".into()]);
        assert_eq!(line, r#""C:\Program Files\x.exe" /a "b c""#);
    }

    #[test]
    fn environment_variables_are_expanded() {
        let expanded = expand_environment(r"%SystemRoot%\System32\cmd.exe").unwrap();
        assert!(!expanded.contains('%'), "{expanded}");
        assert!(expanded.to_ascii_lowercase().ends_with(r"\system32\cmd.exe"), "{expanded}");
        assert_eq!(expand_environment(r"C:\plain.exe").unwrap(), r"C:\plain.exe");
    }

    #[test]
    fn this_process_has_a_user_sid() {
        let sid = process_user_sid().unwrap();
        assert!(sid.starts_with("S-1-"), "{sid}");
    }
}
