//! Running under the Windows service control manager, or in a console.
//!
//! Both modes do the same work ([`crate::runtime::serve`]); they differ in
//! who delivers the stop request and the session and power events.

use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Arc, OnceLock};

use syspilot_proto::Instance;
use syspilot_proto::win::wide;
use windows_sys::Win32::Foundation::{
    ERROR_CALL_NOT_IMPLEMENTED, ERROR_FAILED_SERVICE_CONTROLLER_CONNECT,
    ERROR_SERVICE_SPECIFIC_ERROR, GetLastError, NO_ERROR,
};
use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
use windows_sys::Win32::System::EventLog::{
    DeregisterEventSource, EVENTLOG_ERROR_TYPE, RegisterEventSourceW, ReportEventW,
};
use windows_sys::Win32::System::RemoteDesktop::WTSSESSION_NOTIFICATION;
use windows_sys::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SERVICE_ACCEPT_POWEREVENT, SERVICE_ACCEPT_SESSIONCHANGE,
    SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP, SERVICE_CONTROL_INTERROGATE,
    SERVICE_CONTROL_POWEREVENT, SERVICE_CONTROL_SESSIONCHANGE, SERVICE_CONTROL_SHUTDOWN,
    SERVICE_CONTROL_STOP, SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_STATUS,
    SERVICE_STATUS_CURRENT_STATE, SERVICE_STOP_PENDING, SERVICE_STOPPED, SERVICE_TABLE_ENTRYW,
    SERVICE_WIN32_OWN_PROCESS, SetServiceStatus, StartServiceCtrlDispatcherW,
};

use crate::events::{Event, EventQueue};
use crate::runtime;

// From `winuser.h`. windows-sys exposes these only with its window
// management feature, which this windowless service has no other use for.
/// Session-change reason: a user logged on.
const WTS_SESSION_LOGON: u32 = 5;
/// Power event: the system is suspending.
const PBT_APMSUSPEND: u32 = 4;
/// Power event: the system resumed.
const PBT_APMRESUMEAUTOMATIC: u32 = 18;

/// What the callbacks registered with Windows need. They receive no usable
/// context pointer in every case, so this lives in a process-wide slot that
/// is written once before any callback can run.
struct Shared {
    instance: Instance,
    queue: Arc<EventQueue>,
}

static SHARED: OnceLock<Shared> = OnceLock::new();
/// The handle for reporting service status; null in console mode.
static STATUS_HANDLE: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

fn share(instance: &Instance) -> Result<&'static Shared, String> {
    let queue = EventQueue::new().map_err(|error| format!("create the event queue: {error}"))?;
    Ok(SHARED.get_or_init(|| Shared { instance: instance.clone(), queue }))
}

/// Maps a service control to what the event loop should do about it.
/// `session` is the session a session-change control refers to.
fn event_for_control(control: u32, event_type: u32, session: Option<u32>) -> Option<Event> {
    match (control, event_type) {
        (SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN, _) => Some(Event::Stop),
        (SERVICE_CONTROL_SESSIONCHANGE, WTS_SESSION_LOGON) => session.map(Event::SessionLogon),
        (SERVICE_CONTROL_POWEREVENT, PBT_APMSUSPEND) => Some(Event::Suspend),
        // Sent on every wake-up, with or without a user present.
        (SERVICE_CONTROL_POWEREVENT, PBT_APMRESUMEAUTOMATIC) => Some(Event::Resume),
        _ => None,
    }
}

fn report_status(state: SERVICE_STATUS_CURRENT_STATE, failed: bool) {
    let handle = STATUS_HANDLE.load(Ordering::Acquire);
    if handle.is_null() {
        return;
    }
    let accepted = if state == SERVICE_RUNNING {
        SERVICE_ACCEPT_STOP
            | SERVICE_ACCEPT_SHUTDOWN
            | SERVICE_ACCEPT_SESSIONCHANGE
            | SERVICE_ACCEPT_POWEREVENT
    } else {
        0
    };
    let pending = state == SERVICE_START_PENDING || state == SERVICE_STOP_PENDING;
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: accepted,
        dwWin32ExitCode: if failed { ERROR_SERVICE_SPECIFIC_ERROR } else { NO_ERROR },
        dwServiceSpecificExitCode: u32::from(failed),
        dwCheckPoint: u32::from(pending),
        dwWaitHint: if pending { 10_000 } else { 0 },
    };
    // SAFETY: `handle` came from `RegisterServiceCtrlHandlerExW` and stays
    // valid for the life of the process; `status` is a valid structure.
    unsafe { SetServiceStatus(handle, &status) };
}

/// Called by the service control manager on its own thread.
unsafe extern "system" fn on_service_control(
    control: u32,
    event_type: u32,
    event_data: *mut c_void,
    _context: *mut c_void,
) -> u32 {
    let session = (control == SERVICE_CONTROL_SESSIONCHANGE && !event_data.is_null()).then(|| {
        // SAFETY: for a session-change control Windows passes a pointer to
        // a `WTSSESSION_NOTIFICATION` that is valid during the call.
        unsafe { (*event_data.cast::<WTSSESSION_NOTIFICATION>()).dwSessionId }
    });
    match event_for_control(control, event_type, session) {
        Some(event) => {
            if matches!(event, Event::Stop) {
                report_status(SERVICE_STOP_PENDING, false);
            }
            if let Some(shared) = SHARED.get() {
                shared.queue.push(event);
            }
            NO_ERROR
        }
        None if matches!(
            control,
            SERVICE_CONTROL_INTERROGATE
                | SERVICE_CONTROL_SESSIONCHANGE
                | SERVICE_CONTROL_POWEREVENT
        ) =>
        {
            NO_ERROR
        }
        None => ERROR_CALL_NOT_IMPLEMENTED,
    }
}

/// Writes an error to the Windows Application event log, where a service
/// that failed to start can be diagnosed.
fn log_error_event(source: &str, message: &str) {
    let source = wide(source);
    let message = wide(message);
    // SAFETY: `source` is NUL-terminated; a null server name means the
    // local machine.
    let log = unsafe { RegisterEventSourceW(ptr::null(), source.as_ptr()) };
    if log.is_null() {
        return;
    }
    let strings = [message.as_ptr()];
    // SAFETY: `log` is valid; `strings` holds one NUL-terminated string and
    // the count says so; there is no user SID and no binary data.
    unsafe {
        ReportEventW(
            log,
            EVENTLOG_ERROR_TYPE,
            0,
            0,
            ptr::null_mut(),
            1,
            0,
            strings.as_ptr(),
            ptr::null(),
        );
        DeregisterEventSource(log);
    }
}

/// The service's main function, called by the service control manager.
unsafe extern "system" fn service_main(_argc: u32, _argv: *mut *mut u16) {
    let Some(shared) = SHARED.get() else { return };
    let name = wide(shared.instance.name());
    // SAFETY: `name` is NUL-terminated; the handler has the required
    // signature and lives for the whole process; no context is used.
    let handle = unsafe {
        RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(on_service_control), ptr::null())
    };
    if handle.is_null() {
        return;
    }
    STATUS_HANDLE.store(handle, Ordering::Release);
    report_status(SERVICE_START_PENDING, false);
    let outcome = runtime::serve(&shared.instance, false, shared.queue.clone(), || {
        report_status(SERVICE_RUNNING, false);
    });
    if let Err(problem) = &outcome {
        log_error_event(shared.instance.name(), problem);
    }
    report_status(SERVICE_STOPPED, outcome.is_err());
}

/// Runs as a Windows service. Returns when the service has stopped.
pub fn run_as_service(instance: &Instance) -> Result<(), String> {
    let shared = share(instance)?;
    let mut name = wide(shared.instance.name());
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: name.as_mut_ptr(),
            lpServiceProc: Some(service_main),
        },
        // The table ends with an empty entry.
        SERVICE_TABLE_ENTRYW { lpServiceName: ptr::null_mut(), lpServiceProc: None },
    ];
    // SAFETY: `table` is a terminated array that outlives the call, which
    // returns only after the service has stopped.
    if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } != 0 {
        return Ok(());
    }
    // SAFETY: trivially safe; reads thread-local state.
    match unsafe { GetLastError() } {
        ERROR_FAILED_SERVICE_CONTROLLER_CONNECT => {
            Err("This command is for Windows to start the service. To run it yourself, use `run`."
                .into())
        }
        code => Err(format!("connect to the service control manager: error {code}")),
    }
}

/// Called by Windows on a new thread for Ctrl+C, Ctrl+Break and when the
/// console window closes.
unsafe extern "system" fn on_console_control(_kind: u32) -> i32 {
    if let Some(shared) = SHARED.get() {
        shared.queue.push(Event::Stop);
    }
    // Handled: do not kill the process before the loop has shut down.
    1
}

/// Runs in the console until Ctrl+C or a `Shutdown` request.
pub fn run_in_console(instance: &Instance) -> Result<(), String> {
    let shared = share(instance)?;
    // SAFETY: the handler has the required signature and lives for the
    // whole process.
    unsafe { SetConsoleCtrlHandler(Some(on_console_control), 1) };
    runtime::serve(&shared.instance, true, shared.queue.clone(), || {
        println!("{} is running. Press Ctrl+C to stop.", shared.instance.name());
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNRELATED_CONTROL: u32 = 0x80;

    #[test]
    fn stop_and_shutdown_stop_the_loop() {
        for control in [SERVICE_CONTROL_STOP, SERVICE_CONTROL_SHUTDOWN] {
            assert!(matches!(event_for_control(control, 0, None), Some(Event::Stop)));
        }
    }

    #[test]
    fn only_a_logon_with_a_session_becomes_a_logon_event() {
        let logon = event_for_control(SERVICE_CONTROL_SESSIONCHANGE, WTS_SESSION_LOGON, Some(3));
        assert!(matches!(logon, Some(Event::SessionLogon(3))));
        assert!(
            event_for_control(SERVICE_CONTROL_SESSIONCHANGE, WTS_SESSION_LOGON, None).is_none()
        );
        // 6 is WTS_SESSION_LOGOFF.
        assert!(event_for_control(SERVICE_CONTROL_SESSIONCHANGE, 6, Some(3)).is_none());
    }

    #[test]
    fn suspend_and_automatic_resume_are_forwarded() {
        let suspend = event_for_control(SERVICE_CONTROL_POWEREVENT, PBT_APMSUSPEND, None);
        assert!(matches!(suspend, Some(Event::Suspend)));
        let resume = event_for_control(SERVICE_CONTROL_POWEREVENT, PBT_APMRESUMEAUTOMATIC, None);
        assert!(matches!(resume, Some(Event::Resume)));
        // 10 is PBT_APMPOWERSTATUSCHANGE (plugged in, unplugged).
        assert!(event_for_control(SERVICE_CONTROL_POWEREVENT, 10, None).is_none());
    }

    #[test]
    fn unknown_controls_are_ignored() {
        assert!(event_for_control(SERVICE_CONTROL_INTERROGATE, 0, None).is_none());
        assert!(event_for_control(UNRELATED_CONTROL, 0, None).is_none());
    }

    #[test]
    fn dispatcher_outside_the_service_manager_explains_itself() {
        let instance = Instance::named("SysPilot-Test.dispatch").unwrap();
        let problem = run_as_service(&instance).unwrap_err();
        assert!(problem.contains("use `run`"), "{problem}");
    }
}
