//! Turns the phase the watch publishes into the screen to show, moment by
//! moment, and says when the window is to end. Both windows tick it from a
//! timer in their own thread.

use std::time::{Duration, Instant};

use crate::progress::position;
use crate::screen::Screen;
use crate::watch::{Phase, Step};

/// How long the full bar and the ticked list stay before the app opens.
pub const DONE_PAUSE: Duration = Duration::from_millis(900);

/// When the window ends an update the watch never finished: a look can
/// block on a service that accepted the connection and never answers, and
/// then the watch's own timeout is never reached.
#[derive(Debug, Clone)]
pub struct GiveUp {
    /// How long after the window opened.
    after: Duration,
    /// What the update then ends as.
    phase: Phase,
    /// When the window opened.
    opened: Instant,
}

impl GiveUp {
    /// Gives up `after` now, as `phase`.
    pub fn new(after: Duration, phase: Phase) -> Self {
        Self::counting_from(Instant::now(), after, phase)
    }

    /// Gives up `after` `opened`, as `phase`.
    pub fn counting_from(opened: Instant, after: Duration, phase: Phase) -> Self {
        Self { after, phase, opened }
    }

    /// Whether it is time, at `now`.
    fn due(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.opened) >= self.after
    }
}

/// What a tick says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tick {
    /// Show this.
    Show(Screen),
    /// The update is over, as this phase: end the window.
    End(Phase),
}

/// See the module documentation.
#[derive(Debug)]
pub struct Follower {
    target: Option<String>,
    /// The step on show and since when.
    step: Option<(Step, Instant)>,
    /// When the update was first seen done.
    done_since: Option<Instant>,
    give_up: GiveUp,
}

impl Follower {
    /// Follows an update to `target`, when known.
    pub fn new(target: Option<&str>, give_up: GiveUp) -> Self {
        Self { target: target.map(str::to_owned), step: None, done_since: None, give_up }
    }

    /// The screen for `phase` at `now`, or the end.
    pub fn tick(&mut self, phase: &Phase, now: Instant) -> Tick {
        match phase {
            Phase::Working(_) if self.give_up.due(now) => Tick::End(self.give_up.phase.clone()),
            &Phase::Working(step) => {
                let since = match self.step {
                    Some((shown, since)) if shown == step => since,
                    _ => self.step.insert((step, now)).1,
                };
                let at = position(step, now.saturating_duration_since(since));
                Tick::Show(Screen::working(self.target.as_deref(), step, at))
            }
            Phase::Ready => {
                let since = *self.done_since.get_or_insert(now);
                // A moment to see it end well before the app takes over.
                if now.saturating_duration_since(since) >= DONE_PAUSE {
                    Tick::End(Phase::Ready)
                } else {
                    Tick::Show(Screen::done(self.target.as_deref()))
                }
            }
            Phase::Failed(_) | Phase::AlreadyCurrent => Tick::End(phase.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::BAR_END;
    use crate::screen::Mark;

    fn follower(opened: Instant) -> Follower {
        let give_up =
            GiveUp::counting_from(opened, Duration::from_secs(60), Phase::Failed("stuck".into()));
        Follower::new(Some("0.3.1"), give_up)
    }

    fn shown(tick: Tick) -> Screen {
        match tick {
            Tick::Show(screen) => screen,
            Tick::End(phase) => panic!("(tick): expected a screen | received: the end, {phase:?}"),
        }
    }

    #[test]
    fn the_bar_starts_again_from_each_step_and_moves_within_it() {
        let opened = Instant::now();
        let mut follower = follower(opened);
        let working = Phase::Working(Step::Installing);
        let first = shown(follower.tick(&working, opened)).bar.unwrap().position;
        let later = shown(follower.tick(&working, opened + Duration::from_secs(5))).bar.unwrap();
        assert!(
            later.position > first,
            "(5 s into a step): expected the bar past {first} | received: {}",
            later.position
        );
    }

    #[test]
    fn done_shows_for_a_moment_then_ends_ready() {
        let opened = Instant::now();
        let mut follower = follower(opened);
        let done = shown(follower.tick(&Phase::Ready, opened));
        assert!(done.rows.iter().all(|row| row.mark == Mark::Done), "{:?}", done.rows);
        assert_eq!(done.bar.unwrap().position, BAR_END);
        assert!(matches!(follower.tick(&Phase::Ready, opened + DONE_PAUSE / 2), Tick::Show(_)));
        assert_eq!(follower.tick(&Phase::Ready, opened + DONE_PAUSE), Tick::End(Phase::Ready));
    }

    #[test]
    fn a_failure_ends_at_once() {
        let opened = Instant::now();
        let failed = Phase::Failed("why".into());
        assert_eq!(follower(opened).tick(&failed, opened), Tick::End(failed.clone()));
        assert_eq!(
            follower(opened).tick(&Phase::AlreadyCurrent, opened),
            Tick::End(Phase::AlreadyCurrent)
        );
    }

    #[test]
    fn the_window_gives_up_only_once_its_wait_is_over() {
        let opened = Instant::now();
        let mut follower = follower(opened);
        let working = Phase::Working(Step::Preparing);
        assert!(
            matches!(follower.tick(&working, opened + Duration::from_secs(59)), Tick::Show(_)),
            "(59 s of 60): expected still waiting | received: given up"
        );
        assert_eq!(
            follower.tick(&working, opened + Duration::from_secs(60)),
            Tick::End(Phase::Failed("stuck".into())),
            "(60 s of 60): expected given up"
        );
    }
}
