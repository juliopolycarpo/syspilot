//! Following an update from the outside.
//!
//! The companion sees two things, every half second: what answers on the
//! service's pipe, and whether Windows Installer is busy. [`Watch`] turns
//! that sequence into where the update stands. It holds no processes and no
//! clocks, so every path through an update is a test.
//!
//! An update the user started goes like this, seen from here:
//!
//! 1. The old service answers with the update `Installing`: the updater
//!    downloads and verifies the package.
//! 2. Windows Installer is busy and stops the old service; nothing answers.
//! 3. The new service answers, with another version or another protocol.
//! 4. Windows Installer is done: the update is in place for good.
//!
//! Anything else is the update stopping on the way, and says why.

use std::time::Duration;

use syspilot_proto::message::UpdateState;

/// Longest an update may take before it counts as failed. The service gives
/// Windows Installer as long.
pub const UPDATE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// How long the service may stay away once Windows Installer is idle.
pub const RESTART_TIMEOUT: Duration = Duration::from_secs(2 * 60);
/// How long a new version that answers waits for Windows Installer to be
/// idle. It may be busy with someone else's installation by then.
pub const FINISH_TIMEOUT: Duration = Duration::from_secs(2 * 60);

/// What answered on the service's pipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Service {
    /// The service answered.
    Answering {
        /// Its version, from the handshake.
        version: String,
        /// Where its update stands. Only read from the old version.
        update: UpdateState,
    },
    /// A service answered that speaks another protocol: another build,
    /// which can only be the new one.
    OtherProtocol,
    /// There is no pipe: the service is not running.
    Absent,
    /// The look itself failed (a busy pipe, a connection that broke): it
    /// says nothing about the update either way.
    Unknown,
}

/// One look at the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    /// What answered on the pipe.
    pub service: Service,
    /// Whether Windows Installer runs an installation.
    pub installer_busy: bool,
}

/// The steps the user is shown, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    /// The updater downloads and verifies the new version.
    Preparing,
    /// Windows Installer replaces the files.
    Installing,
    /// The new service starts and Windows Installer finishes.
    Restarting,
}

impl Step {
    /// Every step, in order.
    pub const ALL: [Self; 3] = [Self::Preparing, Self::Installing, Self::Restarting];
}

/// Where the update stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Under way, at this step.
    Working(Step),
    /// The new version runs and Windows Installer is done.
    Ready,
    /// There was nothing newer to install.
    AlreadyCurrent,
    /// It stopped, for this reason.
    Failed(String),
}

impl Phase {
    /// Whether the watch is over.
    pub fn is_final(&self) -> bool {
        !matches!(self, Self::Working(_))
    }
}

/// The state machine. See the module documentation.
#[derive(Debug)]
pub struct Watch {
    /// The version that was running when the update was started.
    from: String,
    /// Windows Installer's log, named when the update fails after it ran.
    log: String,
    /// The old service has stopped answering at least once.
    went_away: bool,
    /// Since when, as elapsed time, the service has been absent while
    /// Windows Installer is idle.
    absent_since: Option<Duration>,
    /// Since when the new version has answered while Windows Installer is
    /// still busy.
    finishing_since: Option<Duration>,
    /// The furthest step shown so far.
    step: Step,
}

/// An update that ran past [`UPDATE_TIMEOUT`], with Windows Installer's
/// `log` named.
pub fn timed_out(log: &str) -> Phase {
    Phase::Failed(format!(
        "The update did not finish within {} minutes. Windows Installer's log is {log}.",
        UPDATE_TIMEOUT.as_secs() / 60
    ))
}

/// How long ago `since` was, starting the count now if it had not begun.
fn waited(since: &mut Option<Duration>, elapsed: Duration) -> Duration {
    elapsed.saturating_sub(*since.get_or_insert(elapsed))
}

impl Watch {
    /// A watch over an update away from version `from`.
    pub fn new(from: &str, log: &str) -> Self {
        Self {
            from: from.to_owned(),
            log: log.to_owned(),
            went_away: false,
            absent_since: None,
            finishing_since: None,
            step: Step::Preparing,
        }
    }

    /// Takes in one look, `elapsed` after the watch began, and says where
    /// the update stands.
    pub fn see(&mut self, observation: &Observation, elapsed: Duration) -> Phase {
        if elapsed >= UPDATE_TIMEOUT {
            return timed_out(&self.log);
        }
        let busy = observation.installer_busy;
        let phase = match &observation.service {
            Service::OtherProtocol => self.new_version(busy, elapsed),
            Service::Answering { version, .. } if *version != self.from => {
                self.new_version(busy, elapsed)
            }
            Service::Answering { update, .. } => self.old_version(update, busy),
            Service::Absent => self.absent(busy, elapsed),
            // Nothing learned: where the update stood still stands.
            Service::Unknown => Phase::Working(self.step),
        };
        match phase {
            // Steps only go forward: a bar that moves back reads as a
            // failure, and a rollback that ends well is not one.
            Phase::Working(step) => {
                self.step = self.step.max(step);
                Phase::Working(self.step)
            }
            done => done,
        }
    }

    fn new_version(&mut self, busy: bool, elapsed: Duration) -> Phase {
        self.absent_since = None;
        if !busy {
            return Phase::Ready;
        }
        if waited(&mut self.finishing_since, elapsed) < FINISH_TIMEOUT {
            // Windows Installer starts the service before it commits; a
            // failure now would still roll the update back.
            Phase::Working(Step::Restarting)
        } else {
            Phase::Ready
        }
    }

    fn old_version(&mut self, update: &UpdateState, busy: bool) -> Phase {
        (self.absent_since, self.finishing_since) = (None, None);
        if self.went_away {
            return if busy {
                // Back for a moment, or a rollback under way.
                Phase::Working(Step::Installing)
            } else {
                Phase::Failed(format!(
                    "The update did not install, and the version that was there runs again. \
                     Windows Installer's log is {}.",
                    self.log
                ))
            };
        }
        match update {
            // Windows Installer busy means the updater has handed over to
            // it; it stops this service a little later.
            UpdateState::Installing if busy => Phase::Working(Step::Installing),
            UpdateState::Installing | UpdateState::Checking => Phase::Working(Step::Preparing),
            UpdateState::Failed { reason } => Phase::Failed(reason.clone()),
            UpdateState::Idle => Phase::AlreadyCurrent,
            UpdateState::Available { .. } => {
                Phase::Failed("The update was not started. Try Update now again.".to_owned())
            }
            UpdateState::Unavailable => Phase::Failed(
                "This copy of SysPilot does not update itself. Install a release to get updates."
                    .to_owned(),
            ),
        }
    }

    fn absent(&mut self, busy: bool, elapsed: Duration) -> Phase {
        self.went_away = true;
        self.finishing_since = None;
        if busy {
            self.absent_since = None;
            return Phase::Working(Step::Installing);
        }
        if waited(&mut self.absent_since, elapsed) < RESTART_TIMEOUT {
            Phase::Working(Step::Restarting)
        } else {
            Phase::Failed(format!(
                "Windows Installer is done, but the SysPilot service did not start again. \
                 Windows Installer's log is {}.",
                self.log
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FROM: &str = "0.3.0";
    const LOG: &str = r"C:\ProgramData\SysPilot\updates\install.log";

    fn answering(version: &str, update: UpdateState) -> Service {
        Service::Answering { version: version.into(), update }
    }

    fn look(service: Service, installer_busy: bool) -> Observation {
        Observation { service, installer_busy }
    }

    fn seconds(count: u64) -> Duration {
        Duration::from_secs(count)
    }

    /// Plays `looks` one second apart and returns every phase.
    fn play(looks: &[Observation]) -> Vec<Phase> {
        let mut watch = Watch::new(FROM, LOG);
        looks.iter().zip(0..).map(|(look, second)| watch.see(look, seconds(second))).collect()
    }

    fn failed_with(phase: &Phase, start: &str) -> bool {
        matches!(phase, Phase::Failed(reason) if reason.starts_with(start))
    }

    #[test]
    fn an_update_that_goes_well_walks_every_step_and_ends_ready() {
        let phases = play(&[
            look(answering(FROM, UpdateState::Installing), false),
            look(answering(FROM, UpdateState::Installing), true),
            look(Service::Absent, true),
            look(answering("0.3.1", UpdateState::Idle), true),
            look(answering("0.3.1", UpdateState::Idle), false),
        ]);
        assert_eq!(
            phases,
            [
                Phase::Working(Step::Preparing),
                Phase::Working(Step::Installing),
                Phase::Working(Step::Installing),
                Phase::Working(Step::Restarting),
                Phase::Ready,
            ]
        );
    }

    #[test]
    fn a_new_service_with_another_protocol_is_the_new_version() {
        let phases = play(&[
            look(answering(FROM, UpdateState::Installing), false),
            look(Service::Absent, true),
            look(Service::OtherProtocol, false),
        ]);
        assert_eq!(phases.last(), Some(&Phase::Ready), "(phases): {phases:?}");
    }

    #[test]
    fn a_refused_download_fails_with_the_updaters_reason() {
        let reason = "(signature): expected a SysPilot release key | received: another key";
        let phases = play(&[
            look(answering(FROM, UpdateState::Installing), false),
            look(answering(FROM, UpdateState::Failed { reason: reason.into() }), false),
        ]);
        assert_eq!(phases[1], Phase::Failed(reason.into()));
    }

    #[test]
    fn nothing_newer_ends_as_already_current() {
        let phases = play(&[look(answering(FROM, UpdateState::Idle), false)]);
        assert_eq!(phases, [Phase::AlreadyCurrent]);
    }

    #[test]
    fn the_old_version_back_after_the_installer_is_a_rollback() {
        let phases = play(&[
            look(Service::Absent, true),
            look(answering(FROM, UpdateState::Idle), true),
            look(answering(FROM, UpdateState::Idle), false),
        ]);
        assert_eq!(phases[1], Phase::Working(Step::Installing));
        assert!(
            failed_with(&phases[2], "The update did not install"),
            "(rollback): expected a failure naming the log | received: {:?}",
            phases[2]
        );
        assert!(matches!(&phases[2], Phase::Failed(reason) if reason.contains(LOG)));
    }

    #[test]
    fn a_service_that_never_comes_back_fails_after_the_restart_timeout() {
        let mut watch = Watch::new(FROM, LOG);
        let gone = look(Service::Absent, false);
        assert_eq!(watch.see(&gone, seconds(10)), Phase::Working(Step::Restarting));
        let almost = seconds(10) + RESTART_TIMEOUT - seconds(1);
        assert_eq!(watch.see(&gone, almost), Phase::Working(Step::Restarting));
        let phase = watch.see(&gone, seconds(10) + RESTART_TIMEOUT);
        assert!(
            failed_with(&phase, "Windows Installer is done, but the SysPilot service"),
            "(service gone): expected a failure | received: {phase:?}"
        );
    }

    #[test]
    fn the_installer_working_again_restarts_the_count() {
        let mut watch = Watch::new(FROM, LOG);
        let (gone, busy) = (look(Service::Absent, false), look(Service::Absent, true));
        watch.see(&gone, seconds(0));
        watch.see(&busy, RESTART_TIMEOUT - seconds(1));
        let phase = watch.see(&gone, RESTART_TIMEOUT + seconds(1));
        assert_eq!(phase, Phase::Working(Step::Restarting));
    }

    #[test]
    fn a_new_version_does_not_wait_forever_for_someone_elses_installation() {
        let mut watch = Watch::new(FROM, LOG);
        let up_but_busy = look(answering("0.3.1", UpdateState::Idle), true);
        assert_eq!(watch.see(&up_but_busy, seconds(5)), Phase::Working(Step::Restarting));
        assert_eq!(watch.see(&up_but_busy, seconds(5) + FINISH_TIMEOUT), Phase::Ready);
    }

    #[test]
    fn a_look_that_fails_during_the_download_is_not_a_rollback() {
        let downloading = look(answering(FROM, UpdateState::Installing), false);
        let phases =
            play(&[downloading.clone(), look(Service::Unknown, false), downloading.clone()]);
        assert_eq!(
            phases,
            [const { Phase::Working(Step::Preparing) }; 3],
            "(failed look mid-download): expected the download to go on"
        );
    }

    #[test]
    fn a_failed_look_does_not_count_toward_the_restart_timeout() {
        let mut watch = Watch::new(FROM, LOG);
        let gone = look(Service::Absent, false);
        watch.see(&gone, seconds(0));
        let unknown = watch.see(&look(Service::Unknown, false), RESTART_TIMEOUT - seconds(1));
        assert_eq!(unknown, Phase::Working(Step::Restarting));
        // Still counted from the first absent look: an unknown look neither
        // ends the wait nor starts it again.
        assert!(watch.see(&gone, RESTART_TIMEOUT).is_final());
    }

    #[test]
    fn waiting_for_the_service_does_not_use_up_the_wait_for_the_installer() {
        let mut watch = Watch::new(FROM, LOG);
        watch.see(&look(Service::Absent, false), seconds(0));
        let up_but_busy = look(answering("0.3.1", UpdateState::Idle), true);
        let later = RESTART_TIMEOUT - seconds(1);
        assert_eq!(watch.see(&up_but_busy, later), Phase::Working(Step::Restarting));
        assert_eq!(
            watch.see(&up_but_busy, later + seconds(1)),
            Phase::Working(Step::Restarting),
            "(new version, installer busy): expected its own wait to have just begun"
        );
    }

    #[test]
    fn steps_never_go_back() {
        let phases = play(&[
            look(Service::Absent, false),
            look(Service::Absent, true),
            look(answering(FROM, UpdateState::Idle), true),
        ]);
        assert_eq!(phases, [const { Phase::Working(Step::Restarting) }; 3]);
    }

    #[test]
    fn an_update_that_takes_too_long_fails_naming_the_log() {
        let mut watch = Watch::new(FROM, LOG);
        let phase = watch.see(&look(Service::Absent, true), UPDATE_TIMEOUT);
        assert!(
            failed_with(&phase, "The update did not finish within 30 minutes")
                && matches!(&phase, Phase::Failed(reason) if reason.contains(LOG)),
            "(timeout): expected a failure naming the log | received: {phase:?}"
        );
    }

    #[test]
    fn only_working_is_not_final() {
        assert!(!Phase::Working(Step::Installing).is_final());
        for done in [Phase::Ready, Phase::AlreadyCurrent, Phase::Failed(String::new())] {
            assert!(done.is_final(), "{done:?}");
        }
    }
}
