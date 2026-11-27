//! What the companion asks of a window, whichever one shows it: the drawn
//! window ([`crate::window`]) or the task dialog in reserve
//! ([`crate::dialog`]).

use std::sync::{Arc, Mutex};

use crate::follow::Follower;
use crate::watch::Phase;

/// How following an update ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgressEnd {
    /// The watch ended it, at this phase.
    Ended(Phase),
    /// The user closed the window first.
    Closed,
    /// Windows did not show it.
    NotShown,
}

/// What the user picked when told of a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// Open the app, which shows the update's state.
    OpenApp,
    /// Close.
    Close,
}

/// A window that follows an update and tells of a failure.
pub trait Surface {
    /// Shows the update as `follower` makes of the phases published in
    /// `shared`, until it ends or the user closes the window.
    fn follow(&mut self, shared: &Arc<Mutex<Phase>>, follower: Follower) -> ProgressEnd;

    /// Says that something went wrong, why, and, when `can_open`, offers
    /// to open the app. Shows the window again if it was hidden.
    fn fail(&mut self, headline: &str, reason: &str, can_open: bool) -> Choice;

    /// Takes the window off the screen, as the app is about to open.
    fn hide(&mut self);
}
