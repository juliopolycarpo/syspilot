//! Win32 implementations of the [`crate::os`] traits.
//!
//! Everything calls the API directly: no child processes, no WMI, no COM.

mod power;
mod process;
mod registry;
mod services;

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_PRIVILEGE_NOT_HELD,
    ERROR_SERVICE_DOES_NOT_EXIST, GetLastError,
};

pub use power::WinPower;
pub use process::{WinLauncher, WinSessions, process_user_sid, session_user_sid};
pub use registry::WinRegistry;
pub use services::WinServices;

use crate::os::{Clock, Os, OsError, OsErrorKind};

/// Builds an [`OsError`] from a Win32 error code.
fn os_error(context: impl Into<String>, code: u32) -> OsError {
    let kind = match code {
        ERROR_ACCESS_DENIED | ERROR_PRIVILEGE_NOT_HELD => OsErrorKind::AccessDenied,
        ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND | ERROR_SERVICE_DOES_NOT_EXIST => {
            OsErrorKind::NotFound
        }
        _ => OsErrorKind::Other,
    };
    OsError::new(kind, context, code)
}

/// Builds an [`OsError`] from the calling thread's last error.
fn last_error(context: impl Into<String>) -> OsError {
    // SAFETY: trivially safe; reads thread-local state.
    os_error(context, unsafe { GetLastError() })
}

/// The system clock.
pub struct WinClock {
    started: Instant,
}

impl Default for WinClock {
    fn default() -> Self {
        Self { started: Instant::now() }
    }
}

impl Clock for WinClock {
    fn monotonic(&self) -> Duration {
        self.started.elapsed()
    }

    fn unix_secs(&self) -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs())
    }
}

/// The real operating system.
pub fn os() -> Os {
    Os {
        registry: Box::new(WinRegistry),
        services: Box::new(WinServices),
        power: Box::new(WinPower),
        launcher: Box::new(WinLauncher),
        sessions: Box::new(WinSessions),
        clock: Box::new(WinClock::default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_map_to_the_kinds_the_engine_reacts_to() {
        assert_eq!(os_error("x", ERROR_ACCESS_DENIED).kind, OsErrorKind::AccessDenied);
        assert_eq!(os_error("x", ERROR_PRIVILEGE_NOT_HELD).kind, OsErrorKind::AccessDenied);
        assert_eq!(os_error("x", ERROR_FILE_NOT_FOUND).kind, OsErrorKind::NotFound);
        assert_eq!(os_error("x", ERROR_SERVICE_DOES_NOT_EXIST).kind, OsErrorKind::NotFound);
        assert_eq!(os_error("x", 87).kind, OsErrorKind::Other);
        assert_eq!(os_error("open key", 87).to_string(), "open key: failed (error 87)");
    }

    #[test]
    fn clock_is_monotonic_and_past_2025() {
        let clock = WinClock::default();
        let first = clock.monotonic();
        assert!(clock.monotonic() >= first);
        assert!(clock.unix_secs() > 1_735_689_600, "the wall clock is before 2025");
    }
}
