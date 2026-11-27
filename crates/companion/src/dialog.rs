//! The window in reserve: the Windows task dialog, for when the drawn
//! window cannot be made (see [`crate::window`]).
//!
//! While the update runs it shows the steps as a checklist and the bar. A
//! timer in the dialog's own thread ticks the [`Follower`] and redraws only
//! what changed, so every window call stays on one thread.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use syspilot_proto::win::wide;
use windows_sys::Win32::Foundation::{HWND, LPARAM, S_OK, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::{
    TASKDIALOG_BUTTON, TASKDIALOGCONFIG, TASKDIALOGCONFIG_0, TD_ERROR_ICON, TDCBF_CLOSE_BUTTON,
    TDE_CONTENT, TDE_MAIN_INSTRUCTION, TDF_CALLBACK_TIMER, TDF_SHOW_PROGRESS_BAR, TDM_CLICK_BUTTON,
    TDM_SET_ELEMENT_TEXT, TDM_SET_PROGRESS_BAR_POS, TDM_SET_PROGRESS_BAR_RANGE, TDN_CREATED,
    TDN_TIMER, TaskDialogIndirect,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{IDCLOSE, SendMessageW};
use windows_sys::core::{HRESULT, PCWSTR};

use crate::follow::{Follower, Tick};
use crate::progress::BAR_END;
use crate::screen::{self, Mark, Row};
use crate::surface::{Choice, ProgressEnd, Surface};
use crate::watch::{Phase, Step};

/// The dialog's title.
const TITLE: &str = "SysPilot update";
/// The icon resource the build embeds: the app's.
const ICON_RESOURCE: u16 = 1;
/// The button that opens the app from the failure dialog.
const OPEN_BUTTON: i32 = 100;
/// Width of both dialogs, in dialog units: room for the path of Windows
/// Installer's log on one line, which the dialog would otherwise shorten
/// in the middle. One width for both, so that a failure replaces the
/// progress in the same place.
const WIDTH: u32 = 300;

/// The checklist as text: done steps ticked, the running one marked, the
/// rest open.
pub fn checklist(rows: &[Row]) -> String {
    rows.iter()
        .map(|row| {
            let mark = match row.mark {
                Mark::Done => '\u{2714}',
                Mark::Current => '\u{25B6}',
                Mark::Pending => '\u{25CB}',
                Mark::Failed => '\u{2716}',
            };
            format!("{mark}   {}", row.text)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A resource ID in the form Windows takes in place of a name.
fn resource(id: u16) -> PCWSTR {
    usize::from(id) as PCWSTR
}

/// What the progress dialog's callback keeps between calls.
struct Progress<'a> {
    shared: &'a Mutex<Phase>,
    follower: Follower,
    /// Text on show, kept alive for as long as the dialog may read it.
    content: Vec<u16>,
    instruction: Vec<u16>,
    /// The final phase, set just before the dialog is closed by the watch.
    outcome: Option<Phase>,
}

fn send(window: HWND, message: i32, wparam: WPARAM, lparam: LPARAM) {
    // SAFETY: `window` is the live task dialog the callback was given, and
    // every message sent is a task dialog message whose pointer argument,
    // if any, points into a buffer `Progress` keeps alive.
    unsafe { SendMessageW(window, message as u32, wparam, lparam) };
}

impl Progress<'_> {
    fn set_text(window: HWND, element: i32, kept: &mut Vec<u16>, text: &str) {
        let text = wide(text);
        if *kept != text {
            *kept = text;
            send(window, TDM_SET_ELEMENT_TEXT, element as WPARAM, kept.as_ptr() as LPARAM);
        }
    }

    fn tick(&mut self, window: HWND) {
        let phase = self.shared.lock().unwrap_or_else(PoisonError::into_inner).clone();
        match self.follower.tick(&phase, Instant::now()) {
            Tick::Show(screen) => {
                let position = screen.bar.map_or(0, |bar| bar.position);
                send(window, TDM_SET_PROGRESS_BAR_POS, position as WPARAM, 0);
                Self::set_text(window, TDE_CONTENT, &mut self.content, &checklist(&screen.rows));
                let headline = &screen.headline;
                Self::set_text(window, TDE_MAIN_INSTRUCTION, &mut self.instruction, headline);
            }
            // The failure dialog takes this one's place at once, so there is
            // no moment in which to show the bar in its error state.
            Tick::End(phase) => {
                self.outcome = Some(phase);
                send(window, TDM_CLICK_BUTTON, IDCLOSE as WPARAM, 0);
            }
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
    let progress = unsafe { &mut *(data as *mut Progress<'_>) };
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
fn show_progress(shared: &Mutex<Phase>, follower: Follower) -> ProgressEnd {
    let title = wide(TITLE);
    let first = screen::Screen::working(None, Step::Preparing, 0);
    let footer = wide(screen::NOTE);
    let mut progress = Progress {
        shared,
        follower,
        content: wide(&checklist(&first.rows)),
        instruction: wide(&first.headline),
        outcome: None,
    };
    let instruction = progress.instruction.clone();
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

/// Says that something went wrong, why, and, when `can_open`, offers to
/// open the app.
fn show_failure(headline: &str, reason: &str, can_open: bool) -> Choice {
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

/// The task dialogs as a [`Surface`]: one dialog for the progress and
/// another, in its place, for a failure.
pub struct TaskDialogs;

impl Surface for TaskDialogs {
    fn follow(&mut self, shared: &Arc<Mutex<Phase>>, follower: Follower) -> ProgressEnd {
        show_progress(shared, follower)
    }

    fn fail(&mut self, headline: &str, reason: &str, can_open: bool) -> Choice {
        show_failure(headline, reason, can_open)
    }

    fn hide(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::rows;

    #[test]
    fn the_checklist_ticks_what_is_done_and_marks_what_runs() {
        assert_eq!(
            checklist(&rows(Some(Step::Installing))),
            "\u{2714}   Download and check the new version\n\
             \u{25B6}   Install it\n\
             \u{25CB}   Start SysPilot again"
        );
        assert!(
            checklist(&rows(None)).lines().all(|line| line.starts_with('\u{2714}')),
            "(done): expected every step ticked | received: {}",
            checklist(&rows(None))
        );
    }

    #[test]
    fn the_checklist_has_the_same_number_of_lines_at_every_step() {
        // The dialog does not resize while the update runs.
        for step in Step::ALL {
            assert_eq!(checklist(&rows(Some(step))).lines().count(), Step::ALL.len(), "{step:?}");
        }
    }
}
