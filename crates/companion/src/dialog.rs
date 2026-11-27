//! The companion's window: a Windows task dialog.
//!
//! While the update runs it shows the three steps as a checklist and a
//! progress bar that keeps moving (see [`crate::progress`]). A timer in the
//! dialog's own thread reads the phase the watch publishes and redraws only
//! what changed, so every window call stays on one thread.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use syspilot_proto::win::wide;
use windows_sys::Win32::Foundation::{HWND, LPARAM, S_OK, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::{
    PBST_ERROR, TASKDIALOG_BUTTON, TASKDIALOGCONFIG, TASKDIALOGCONFIG_0, TD_ERROR_ICON,
    TDCBF_CLOSE_BUTTON, TDE_CONTENT, TDE_MAIN_INSTRUCTION, TDF_CALLBACK_TIMER,
    TDF_SHOW_PROGRESS_BAR, TDM_CLICK_BUTTON, TDM_SET_ELEMENT_TEXT, TDM_SET_PROGRESS_BAR_POS,
    TDM_SET_PROGRESS_BAR_RANGE, TDM_SET_PROGRESS_BAR_STATE, TDN_CREATED, TDN_TIMER,
    TaskDialogIndirect,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{IDCLOSE, SendMessageW};
use windows_sys::core::{HRESULT, PCWSTR};

use crate::progress::{BAR_END, position};
use crate::watch::{Phase, Step};

/// The dialog's title.
const TITLE: &str = "SysPilot update";
/// The icon resource the build embeds: the app's.
const ICON_RESOURCE: u16 = 1;
/// How long the full bar and the ticked list stay before the app opens.
const DONE_PAUSE: Duration = Duration::from_millis(900);
/// The button that opens the app from the failure dialog.
const OPEN_BUTTON: i32 = 100;
/// Width of both dialogs, in dialog units: room for the path of Windows
/// Installer's log on one line, which the dialog would otherwise shorten
/// in the middle. One width for both, so that a failure replaces the
/// progress in the same place.
const WIDTH: u32 = 300;

/// What the user is told a step is about, as it runs.
fn step_text(step: Step) -> &'static str {
    match step {
        Step::Preparing => "Download and check the new version",
        Step::Installing => "Install it",
        Step::Restarting => "Start SysPilot again",
    }
}

/// The checklist: steps before `current` ticked, `current` marked, the
/// rest open. `None` ticks them all.
pub fn checklist(current: Option<Step>) -> String {
    Step::ALL
        .iter()
        .map(|&step| {
            let mark = match current {
                Some(at) if step == at => '\u{25B6}',
                Some(at) if step > at => '\u{25CB}',
                _ => '\u{2714}',
            };
            format!("{mark}   {}", step_text(step))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The headline while the update runs.
pub fn headline(target: Option<&str>) -> String {
    match target {
        Some(version) => format!("Updating SysPilot to {version}"),
        None => "Updating SysPilot".to_owned(),
    }
}

/// The headline once it is done.
pub fn done_headline(target: Option<&str>) -> String {
    match target {
        Some(version) => format!("SysPilot {version} is installed"),
        None => "The update is installed".to_owned(),
    }
}

/// A resource ID in the form Windows takes in place of a name.
fn resource(id: u16) -> PCWSTR {
    usize::from(id) as PCWSTR
}

/// What the progress dialog's callback keeps between calls.
struct Progress {
    shared: Arc<Mutex<Phase>>,
    target: Option<String>,
    /// The step on show and since when.
    step: Option<(Step, Instant)>,
    /// Text on show, kept alive for as long as the dialog may read it.
    content: Vec<u16>,
    instruction: Vec<u16>,
    /// When the bar was filled for a finished update.
    done_since: Option<Instant>,
    /// The final phase, set just before the dialog is closed by the watch.
    outcome: Option<Phase>,
}

fn send(window: HWND, message: i32, wparam: WPARAM, lparam: LPARAM) {
    // SAFETY: `window` is the live task dialog the callback was given, and
    // every message sent is a task dialog message whose pointer argument,
    // if any, points into a buffer `Progress` keeps alive.
    unsafe { SendMessageW(window, message as u32, wparam, lparam) };
}

impl Progress {
    fn set_text(window: HWND, element: i32, kept: &mut Vec<u16>, text: &str) {
        let text = wide(text);
        if *kept != text {
            *kept = text;
            send(window, TDM_SET_ELEMENT_TEXT, element as WPARAM, kept.as_ptr() as LPARAM);
        }
    }

    fn set_bar(window: HWND, position: u32) {
        send(window, TDM_SET_PROGRESS_BAR_POS, position as WPARAM, 0);
    }

    fn close(&mut self, window: HWND, outcome: Phase) {
        self.outcome = Some(outcome);
        send(window, TDM_CLICK_BUTTON, IDCLOSE as WPARAM, 0);
    }

    fn tick(&mut self, window: HWND) {
        let phase = self.shared.lock().unwrap_or_else(PoisonError::into_inner).clone();
        match phase {
            Phase::Working(step) => {
                let since = match self.step {
                    Some((shown, since)) if shown == step => since,
                    _ => self.step.insert((step, Instant::now())).1,
                };
                Self::set_bar(window, position(step, since.elapsed()));
                Self::set_text(window, TDE_CONTENT, &mut self.content, &checklist(Some(step)));
            }
            Phase::Ready => {
                let since = *self.done_since.get_or_insert_with(Instant::now);
                Self::set_bar(window, BAR_END);
                Self::set_text(window, TDE_CONTENT, &mut self.content, &checklist(None));
                let done = done_headline(self.target.as_deref());
                Self::set_text(window, TDE_MAIN_INSTRUCTION, &mut self.instruction, &done);
                // A moment to see it end well before the app takes over.
                if since.elapsed() >= DONE_PAUSE {
                    self.close(window, Phase::Ready);
                }
            }
            Phase::Failed(_) => {
                send(window, TDM_SET_PROGRESS_BAR_STATE, PBST_ERROR as WPARAM, 0);
                self.close(window, phase);
            }
            Phase::AlreadyCurrent => self.close(window, phase),
        }
    }
}

unsafe extern "system" fn progress_callback(
    window: HWND,
    notification: u32,
    _wparam: WPARAM,
    _lparam: LPARAM,
    data: isize,
) -> HRESULT {
    // SAFETY: `data` is the `Progress` that `show_progress` passed in the
    // configuration; it outlives the dialog, and only this thread, inside
    // this callback, touches it while the dialog runs.
    let progress = unsafe { &mut *(data as *mut Progress) };
    match notification as i32 {
        TDN_CREATED => {
            let range = (BAR_END as LPARAM) << 16;
            send(window, TDM_SET_PROGRESS_BAR_RANGE, 0, range);
            progress.tick(window);
        }
        TDN_TIMER if progress.outcome.is_none() => progress.tick(window),
        _ => {}
    }
    S_OK
}

/// Shows the update as it runs, following the phases published in
/// `shared`, until the watch ends it or the user closes the window.
pub fn show_progress(shared: Arc<Mutex<Phase>>, target: Option<&str>) -> ProgressEnd {
    let title = wide(TITLE);
    let instruction = wide(&headline(target));
    let footer = wide(
        "SysPilot opens again by itself when the update is done. \
         Closing this window does not stop the update.",
    );
    let mut progress = Progress {
        shared,
        target: target.map(str::to_owned),
        step: None,
        content: wide(&checklist(Some(Step::Preparing))),
        instruction: instruction.clone(),
        done_since: None,
        outcome: None,
    };
    // SAFETY: a null name asks for this executable's module, always loaded.
    let module = unsafe { GetModuleHandleW(std::ptr::null()) };
    let config = TASKDIALOGCONFIG {
        cbSize: size_of::<TASKDIALOGCONFIG>() as u32,
        hInstance: module,
        dwFlags: TDF_SHOW_PROGRESS_BAR | TDF_CALLBACK_TIMER,
        dwCommonButtons: TDCBF_CLOSE_BUTTON,
        pszWindowTitle: title.as_ptr(),
        Anonymous1: TASKDIALOGCONFIG_0 { pszMainIcon: resource(ICON_RESOURCE) },
        pszMainInstruction: instruction.as_ptr(),
        pszContent: progress.content.as_ptr(),
        pszFooter: footer.as_ptr(),
        pfCallback: Some(progress_callback),
        lpCallbackData: (&raw mut progress) as isize,
        cxWidth: WIDTH,
        ..Default::default()
    };
    // SAFETY: every string in `config` is NUL-terminated and outlives the
    // call; the callback data points at `progress`, which does too. The
    // out-pointers are optional and null.
    let shown = unsafe {
        TaskDialogIndirect(
            &config,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    match (shown, progress.outcome) {
        (S_OK, Some(phase)) => ProgressEnd::Ended(phase),
        (S_OK, None) => ProgressEnd::Closed,
        _ => ProgressEnd::NotShown,
    }
}

/// How the progress dialog ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgressEnd {
    /// The watch ended it, at this phase.
    Ended(Phase),
    /// The user closed it first.
    Closed,
    /// Windows did not show it.
    NotShown,
}

/// What the user picked in the failure dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// Open the app, which shows the update's state.
    OpenApp,
    /// Close.
    Close,
}

/// Says that something went wrong, why, and, when `can_open`, offers to
/// open the app.
pub fn show_failure(headline: &str, reason: &str, can_open: bool) -> Choice {
    let (title, headline, reason) = (wide(TITLE), wide(headline), wide(reason));
    let open_text = wide("Open SysPilot");
    let buttons = [TASKDIALOG_BUTTON { nButtonID: OPEN_BUTTON, pszButtonText: open_text.as_ptr() }];
    let config = TASKDIALOGCONFIG {
        cbSize: size_of::<TASKDIALOGCONFIG>() as u32,
        dwCommonButtons: TDCBF_CLOSE_BUTTON,
        pszWindowTitle: title.as_ptr(),
        Anonymous1: TASKDIALOGCONFIG_0 { pszMainIcon: TD_ERROR_ICON },
        pszMainInstruction: headline.as_ptr(),
        pszContent: reason.as_ptr(),
        cButtons: if can_open { buttons.len() as u32 } else { 0 },
        pButtons: buttons.as_ptr(),
        nDefaultButton: if can_open { OPEN_BUTTON } else { IDCLOSE },
        cxWidth: WIDTH,
        ..Default::default()
    };
    let mut clicked = 0;
    // SAFETY: every string in `config` and the button array are
    // NUL-terminated or sized as declared and outlive the call; `clicked`
    // is a valid out-pointer and the other two are optional and null.
    let shown = unsafe {
        TaskDialogIndirect(&config, &mut clicked, std::ptr::null_mut(), std::ptr::null_mut())
    };
    if shown == S_OK && clicked == OPEN_BUTTON { Choice::OpenApp } else { Choice::Close }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_checklist_ticks_what_is_done_and_marks_what_runs() {
        assert_eq!(
            checklist(Some(Step::Installing)),
            "\u{2714}   Download and check the new version\n\
             \u{25B6}   Install it\n\
             \u{25CB}   Start SysPilot again"
        );
        assert!(
            checklist(None).lines().all(|line| line.starts_with('\u{2714}')),
            "(done): expected every step ticked | received: {}",
            checklist(None)
        );
    }

    #[test]
    fn the_checklist_has_the_same_number_of_lines_at_every_step() {
        // The dialog does not resize while the update runs.
        for step in Step::ALL {
            assert_eq!(checklist(Some(step)).lines().count(), Step::ALL.len(), "{step:?}");
        }
    }

    #[test]
    fn headlines_name_the_version_when_it_is_known() {
        assert_eq!(headline(Some("0.3.1")), "Updating SysPilot to 0.3.1");
        assert_eq!(headline(None), "Updating SysPilot");
        assert_eq!(done_headline(Some("0.3.1")), "SysPilot 0.3.1 is installed");
        assert_eq!(done_headline(None), "The update is installed");
    }
}
