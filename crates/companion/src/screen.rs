//! What the window says, as data: the headline, the steps and how far each
//! is, the bar, the reason for a failure and the buttons. The drawn window
//! and the task dialog both show it, so they cannot disagree.

use crate::progress::BAR_END;
use crate::watch::Step;

/// The footer while an update runs.
pub const NOTE: &str = "SysPilot opens again by itself when the update is done, unless this \
                        window is closed first. Closing it does not stop the update.";

/// What the user is told a step is about, as it runs.
pub fn step_text(step: Step) -> &'static str {
    match step {
        Step::Preparing => "Download and check the new version",
        Step::Installing => "Install it",
        Step::Restarting => "Start SysPilot again",
    }
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

/// Where a step stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    /// Over.
    Done,
    /// Running now.
    Current,
    /// Not reached.
    Pending,
    /// Running when the update stopped.
    Failed,
}

/// One step on the checklist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Where it stands.
    pub mark: Mark,
    /// What it is about.
    pub text: &'static str,
}

/// The rows with steps before `current` done, `current` running and the
/// rest pending. `None` marks them all done.
pub fn rows(current: Option<Step>) -> Vec<Row> {
    Step::ALL
        .iter()
        .map(|&step| {
            let mark = match current {
                Some(at) if step == at => Mark::Current,
                Some(at) if step > at => Mark::Pending,
                _ => Mark::Done,
            };
            Row { mark, text: step_text(step) }
        })
        .collect()
}

/// The progress bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bar {
    /// Out of [`BAR_END`].
    pub position: u32,
    /// Whether it stopped on a failure, which colours it as an error.
    pub failed: bool,
}

/// The picture beside the headline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Badge {
    /// The app's icon.
    App,
    /// An error sign.
    Error,
}

/// What a button does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Opens the app.
    OpenApp,
    /// Closes the window.
    Close,
}

/// A button at the foot of the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Button {
    /// What it does.
    pub command: Command,
    /// What it says.
    pub label: &'static str,
    /// Whether Enter presses it; it is drawn in the accent colour.
    pub default: bool,
}

/// Everything the window shows at one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Screen {
    /// Beside the headline.
    pub badge: Badge,
    /// The headline.
    pub headline: String,
    /// The steps, or none when the failure came before any.
    pub rows: Vec<Row>,
    /// The bar, when there are steps.
    pub bar: Option<Bar>,
    /// Why it failed.
    pub reason: Option<String>,
    /// The footer.
    pub note: Option<&'static str>,
    /// The buttons, left to right.
    pub buttons: Vec<Button>,
}

const CLOSE: Button = Button { command: Command::Close, label: "Close", default: false };

impl Screen {
    /// The update at `step`, with the bar at `position`.
    pub fn working(target: Option<&str>, step: Step, position: u32) -> Self {
        Self {
            badge: Badge::App,
            headline: headline(target),
            rows: rows(Some(step)),
            bar: Some(Bar { position, failed: false }),
            reason: None,
            note: Some(NOTE),
            buttons: vec![CLOSE],
        }
    }

    /// The update done. The footer stays, so the window keeps its size in
    /// the moment before the app opens.
    pub fn done(target: Option<&str>) -> Self {
        Self {
            badge: Badge::App,
            headline: done_headline(target),
            rows: rows(None),
            bar: Some(Bar { position: BAR_END, failed: false }),
            reason: None,
            note: Some(NOTE),
            buttons: vec![CLOSE],
        }
    }

    /// A failure, said with `headline` and `reason`. After `before`, the
    /// screen it replaces, the steps stay, with the one that was running
    /// marked as where it stopped. `can_open` offers to open the app.
    pub fn failed(headline: &str, reason: &str, can_open: bool, before: Option<&Self>) -> Self {
        let rows: Vec<Row> = before.map_or_else(Vec::new, |screen| {
            screen
                .rows
                .iter()
                .map(|row| Row {
                    mark: if row.mark == Mark::Current { Mark::Failed } else { row.mark },
                    text: row.text,
                })
                .collect()
        });
        let stopped = rows.iter().any(|row| row.mark == Mark::Failed);
        let bar = before.and_then(|screen| screen.bar).map(|bar| Bar { failed: stopped, ..bar });
        let buttons = if can_open {
            vec![Button { command: Command::OpenApp, label: "Open SysPilot", default: true }, CLOSE]
        } else {
            vec![Button { default: true, ..CLOSE }]
        };
        Self {
            badge: Badge::Error,
            headline: headline.to_owned(),
            rows,
            bar,
            reason: Some(reason.to_owned()),
            note: None,
            buttons,
        }
    }

    /// The step running now, if any.
    pub fn current(&self) -> Option<&Row> {
        self.rows.iter().find(|row| row.mark == Mark::Current)
    }

    /// The button Enter presses.
    pub fn default_button(&self) -> Option<usize> {
        self.buttons.iter().position(|button| button.default)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marks(screen: &Screen) -> Vec<Mark> {
        screen.rows.iter().map(|row| row.mark).collect()
    }

    #[test]
    fn steps_before_the_current_one_are_done_and_after_it_pending() {
        let screen = Screen::working(Some("0.3.1"), Step::Installing, 500);
        assert_eq!(marks(&screen), [Mark::Done, Mark::Current, Mark::Pending]);
        assert_eq!(screen.current().map(|row| row.text), Some("Install it"));
        assert_eq!(screen.headline, "Updating SysPilot to 0.3.1");
        assert_eq!(screen.bar, Some(Bar { position: 500, failed: false }));
        assert_eq!(screen.default_button(), None, "(working): expected Enter to close nothing");
    }

    #[test]
    fn done_ticks_every_step_and_fills_the_bar() {
        let screen = Screen::done(None);
        assert_eq!(marks(&screen), [Mark::Done; 3]);
        assert_eq!(screen.headline, "The update is installed");
        assert_eq!(screen.bar, Some(Bar { position: BAR_END, failed: false }));
        assert_eq!(screen.note, Screen::working(None, Step::Preparing, 0).note);
    }

    #[test]
    fn a_failure_marks_the_step_that_was_running_and_stops_the_bar_there() {
        let before = Screen::working(None, Step::Installing, 612);
        let failed = Screen::failed("The update did not finish", "Because.", true, Some(&before));
        assert_eq!(marks(&failed), [Mark::Done, Mark::Failed, Mark::Pending]);
        assert_eq!(failed.bar, Some(Bar { position: 612, failed: true }));
        assert_eq!(failed.badge, Badge::Error);
        assert_eq!(failed.reason.as_deref(), Some("Because."));
        assert_eq!(failed.note, None);
    }

    #[test]
    fn a_failure_after_the_update_keeps_it_done() {
        let failed =
            Screen::failed("SysPilot did not open", "No file.", false, Some(&Screen::done(None)));
        assert_eq!(marks(&failed), [Mark::Done; 3]);
        assert_eq!(
            failed.bar,
            Some(Bar { position: BAR_END, failed: false }),
            "(failure after done): expected the full bar, not an error"
        );
    }

    #[test]
    fn a_failure_with_nothing_before_it_has_no_steps() {
        let failed = Screen::failed("The update cannot be followed", "Bad option.", false, None);
        assert!(failed.rows.is_empty(), "(no screen before): received rows {:?}", failed.rows);
        assert_eq!(failed.bar, None);
    }

    #[test]
    fn enter_opens_the_app_when_it_is_offered_and_closes_otherwise() {
        let offered = Screen::failed("h", "r", true, None);
        let labels: Vec<&str> = offered.buttons.iter().map(|button| button.label).collect();
        assert_eq!(labels, ["Open SysPilot", "Close"]);
        assert_eq!(offered.default_button(), Some(0));
        let not_offered = Screen::failed("h", "r", false, None);
        assert_eq!(
            not_offered.buttons[not_offered.default_button().unwrap()].command,
            Command::Close
        );
    }

    #[test]
    fn headlines_name_the_version_when_it_is_known() {
        assert_eq!(headline(Some("0.3.1")), "Updating SysPilot to 0.3.1");
        assert_eq!(headline(None), "Updating SysPilot");
        assert_eq!(done_headline(Some("0.3.1")), "SysPilot 0.3.1 is installed");
        assert_eq!(done_headline(None), "The update is installed");
    }
}
