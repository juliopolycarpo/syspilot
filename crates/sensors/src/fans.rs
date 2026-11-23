//! The worker's fan control.
//!
//! The service sends the profile's curves ([`FanPlan`]) and the worker
//! follows them next to the registers: it reads the sensors itself, so a
//! step needs no round trip, and when the service goes away (its end of the
//! pipe closes) the worker is the one that hands every fan back to the BIOS.
//!
//! Without the `fan-control` cargo feature [`Fans`] does nothing at all: no
//! type here can open a fan bus, so a default build of this program has no
//! path that writes to a fan. With the feature it holds a
//! `syspilot_hw::fan::session::FanSession`, which exists only for a board
//! whose catalog entry a human validated.

use std::time::Instant;

use syspilot_proto::message::FanRuntime;
use syspilot_proto::worker::FanPlan;

#[cfg(feature = "fan-control")]
pub use active::{Access, FanAccess, Fans};
#[cfg(not(feature = "fan-control"))]
pub use inert::{Access, Fans};

/// What the sensors read at one moment, by key.
#[cfg_attr(
    not(feature = "fan-control"),
    allow(dead_code, reason = "only a build with fan control reads a sample")
)]
pub struct Sample<'a> {
    /// The sensor keys.
    pub keys: &'a [String],
    /// One value per key; `None` is a reading that failed.
    pub values: &'a [Option<f64>],
    /// When the sample was taken.
    pub at: Instant,
}

#[cfg(not(feature = "fan-control"))]
mod inert {
    use super::{FanPlan, FanRuntime, Sample};

    /// What would open a fan session. There is none in this build.
    pub enum Access {}

    /// Fan control of a build without the feature: a plan is ignored.
    pub struct Fans;

    impl Fans {
        /// A build without the feature has no access to give.
        pub fn new(access: Option<Access>) -> Self {
            // The type has no value, so there is nothing to keep.
            if let Some(never) = access {
                match never {}
            }
            Self
        }

        /// Ignored: this build never writes to a fan.
        pub fn set_plan(&mut self, _plan: FanPlan) {}

        /// Whether the worker should step on its own timer.
        pub fn needs_steps(&self) -> bool {
            false
        }

        /// Nothing to step.
        pub fn tick(&mut self, _sample: &Sample<'_>) {}

        /// Nothing to release.
        pub fn suspend(&mut self) {}

        /// Nothing to retake.
        pub fn resume(&mut self) {}

        /// Nothing to release.
        pub fn stop(&mut self) {}

        /// There is never anything to report.
        pub fn take_changes(&mut self) -> Option<Vec<FanRuntime>> {
            None
        }
    }
}

#[cfg(feature = "fan-control")]
mod active {
    use std::path::Path;

    use syspilot_hw::fan::curve::Readings;
    use syspilot_hw::fan::session::FanSession;
    use syspilot_proto::message::FanPhase;
    use syspilot_proto::profile::FanCurve;

    use super::{FanPlan, FanRuntime, Instant, Sample};

    /// How a session is opened. The real one asks the shipped board catalog
    /// for a grant first; tests open one over the replay bus.
    pub trait FanAccess: Send {
        /// Opens a session for the enabled curves of `curves`, keeping the
        /// saved BIOS registers in `store_directory`. The session also hands
        /// back every channel a crashed predecessor left recorded there,
        /// whatever `curves` say. `Ok(None)` when there is nothing to run
        /// and nothing to hand back.
        fn open(
            &mut self,
            curves: &[FanCurve],
            store_directory: &Path,
        ) -> Result<Option<FanSession>, String>;
    }

    /// What opens a session.
    pub type Access = Box<dyn FanAccess>;

    /// The worker's fan control: at most one session, and what it last said.
    pub struct Fans {
        access: Option<Access>,
        session: Option<FanSession>,
        reported: Vec<FanRuntime>,
        changed: bool,
        suspended: bool,
        last_step: Option<Instant>,
    }

    impl Fans {
        /// Fan control with `access`, or none for a board that may not be
        /// controlled: a plan then opens nothing.
        pub fn new(access: Option<Access>) -> Self {
            Self {
                access,
                session: None,
                reported: Vec::new(),
                changed: false,
                suspended: false,
                last_step: None,
            }
        }

        fn report(&mut self, runtime: Vec<FanRuntime>) {
            if runtime != self.reported {
                self.reported = runtime;
                self.changed = true;
            }
        }

        /// Replaces the curves being followed. The old session hands the fans
        /// back before the new one takes anything. A board that may be
        /// controlled is opened even for a plan with no curve, so that a
        /// channel a crashed predecessor left in manual mode is handed back
        /// whether or not the plan mentions it.
        pub fn set_plan(&mut self, plan: FanPlan) {
            if let Some(mut old) = self.session.take() {
                old.stop();
            }
            self.last_step = None;
            let enabled: Vec<&FanCurve> =
                plan.curves.iter().filter(|curve| curve.enabled).collect();
            let Some(access) = self.access.as_mut() else {
                // The board is read-only: the status already says why, and
                // nothing is opened.
                if enabled.is_empty() {
                    self.report(Vec::new());
                }
                return;
            };
            match access.open(&plan.curves, Path::new(&plan.store_directory)) {
                Ok(None) => self.report(Vec::new()),
                Ok(Some(mut session)) => {
                    if self.suspended {
                        session.suspend();
                    }
                    let runtime = session.runtime();
                    // Only a session with work left is kept: one that just
                    // handed the channels back has nothing more to do.
                    if session.has_work() {
                        // A panic in this process must hand the fans back too.
                        session.install_panic_hook();
                        self.session = Some(session);
                    }
                    self.report(runtime);
                }
                Err(problem) => {
                    let runtime = enabled
                        .iter()
                        .map(|curve| FanRuntime {
                            channel: curve.channel.clone(),
                            phase: FanPhase::Bios,
                            target_duty_pct: None,
                            problem: Some(problem.clone()),
                        })
                        .collect();
                    self.report(runtime);
                }
            }
        }

        /// Whether the worker should step on its own timer when the service
        /// is slow: yes while a session holds fans.
        pub fn needs_steps(&self) -> bool {
            self.session.is_some()
        }

        /// One control step from `sample`.
        pub fn tick(&mut self, sample: &Sample<'_>) {
            let Some(session) = self.session.as_mut() else { return };
            let elapsed = self.last_step.map_or(0.0, |previous| {
                sample.at.saturating_duration_since(previous).as_secs_f64()
            });
            self.last_step = Some(sample.at);
            // Only the sensors the curves read. A key the sample lacks stays
            // out of the map, which the controller counts as a failed source.
            let readings: Readings = session
                .sensor_keys()
                .iter()
                .filter_map(|wanted| {
                    let index = sample.keys.iter().position(|key| key == wanted)?;
                    Some((wanted.clone(), sample.values.get(index).copied().flatten()))
                })
                .collect();
            let runtime = session.tick(&readings, elapsed);
            // A session kept only to retry a restore ends when it succeeds.
            if !session.has_work() {
                self.session = None;
            }
            self.report(runtime);
        }

        /// The machine is about to sleep: the fans go back to the BIOS.
        pub fn suspend(&mut self) {
            self.suspended = true;
            if let Some(session) = self.session.as_mut() {
                let runtime = session.suspend();
                self.report(runtime);
            }
        }

        /// The machine woke up.
        pub fn resume(&mut self) {
            self.suspended = false;
            self.last_step = None;
            if let Some(session) = self.session.as_mut() {
                session.resume();
            }
        }

        /// Hands every fan back to the BIOS and drops the session.
        pub fn stop(&mut self) {
            if let Some(mut session) = self.session.take() {
                let runtime = session.stop();
                self.report(runtime);
            }
        }

        /// What changed since the last call, for the service.
        pub fn take_changes(&mut self) -> Option<Vec<FanRuntime>> {
            std::mem::take(&mut self.changed).then(|| self.reported.clone())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::fan_fakes::{
            BIOS_MODE, CPU, ReplayHandles, crashed_replay_access, curve, refusing_access,
            replay_access,
        };

        const DIRECTORY: &str = r"C:\data\SysPilot";

        fn fans_on_replay() -> (Fans, ReplayHandles) {
            let (access, handles) = replay_access();
            (Fans::new(Some(access)), handles)
        }

        fn plan(curves: Vec<FanCurve>) -> FanPlan {
            FanPlan { store_directory: DIRECTORY.into(), curves }
        }

        fn step(fans: &mut Fans, temperature: Option<f64>, count: usize) {
            let keys = [CPU.to_owned()];
            for _ in 0..count {
                let values = [temperature];
                fans.tick(&Sample { keys: &keys, values: &values, at: Instant::now() });
            }
        }

        #[test]
        fn a_plan_opens_a_session_in_the_given_directory_and_reports_its_channels() {
            let (mut fans, handles) = fans_on_replay();
            assert!(fans.take_changes().is_none(), "nothing to say before a plan");
            fans.set_plan(plan(vec![curve(true)]));
            assert_eq!(handles.directories(), [DIRECTORY]);
            assert!(fans.needs_steps());
            let first = fans.take_changes().unwrap();
            assert_eq!(first[0].channel, "board.pwm1");
            assert_eq!(first[0].phase, FanPhase::Bios);
            assert!(fans.take_changes().is_none(), "a report is delivered once");
            assert!(handles.fan_writes().is_empty(), "opening a session writes no register");
        }

        #[test]
        fn steps_take_control_after_good_readings_and_report_only_changes() {
            let (mut fans, handles) = fans_on_replay();
            fans.set_plan(plan(vec![curve(true)]));
            fans.take_changes();
            step(&mut fans, Some(45.0), 2);
            assert!(fans.take_changes().is_none(), "two readings are not enough yet");
            assert!(handles.fan_writes().is_empty());
            step(&mut fans, Some(45.0), 1);
            let runtime = fans.take_changes().unwrap();
            assert_eq!(
                (runtime[0].phase, runtime[0].target_duty_pct),
                (FanPhase::Manual, Some(40))
            );
            step(&mut fans, Some(45.0), 3);
            assert!(fans.take_changes().is_none(), "the same duty is not reported again");
        }

        #[test]
        fn a_sensor_missing_from_the_sample_sends_the_fan_to_full_speed() {
            let (mut fans, handles) = fans_on_replay();
            fans.set_plan(plan(vec![curve(true)]));
            step(&mut fans, Some(45.0), 3);
            fans.take_changes();
            // The sample has other sensors but not the curve's source.
            let keys = ["gpu0.temp".to_owned()];
            let values = [Some(40.0)];
            fans.tick(&Sample { keys: &keys, values: &values, at: Instant::now() });
            let runtime = fans.take_changes().unwrap();
            assert_eq!(runtime[0].phase, FanPhase::FailSafe);
            assert_eq!(runtime[0].target_duty_pct, Some(100));
            assert_eq!(handles.fan_writes().last(), Some(&(0x109, 0xFF)));
        }

        #[test]
        fn suspend_and_stop_hand_the_registers_back() {
            let (mut fans, handles) = fans_on_replay();
            fans.set_plan(plan(vec![curve(true)]));
            step(&mut fans, Some(45.0), 3);
            fans.suspend();
            assert_eq!(handles.fan_writes().last(), Some(&(0x102, BIOS_MODE)));

            fans.resume();
            step(&mut fans, Some(45.0), 3);
            assert_eq!(fans.take_changes().unwrap()[0].phase, FanPhase::Manual);
            fans.stop();
            assert_eq!(handles.fan_writes().last(), Some(&(0x102, BIOS_MODE)));
            assert!(!fans.needs_steps());
            let writes = handles.fan_writes().len();
            step(&mut fans, Some(45.0), 3);
            assert_eq!(handles.fan_writes().len(), writes, "nothing is written after a stop");
        }

        #[test]
        fn a_new_plan_hands_the_fans_back_before_the_new_session_takes_them() {
            let (mut fans, handles) = fans_on_replay();
            fans.set_plan(plan(vec![curve(true)]));
            step(&mut fans, Some(45.0), 3);
            let before = handles.fan_writes().len();
            fans.set_plan(plan(vec![curve(true)]));
            let released = handles.fan_writes();
            assert!(released.len() > before, "the registers were written back: {released:?}");
            assert_eq!(released.last(), Some(&(0x102, BIOS_MODE)));
            assert!(fans.needs_steps(), "the new session is open and waits for good readings");
        }

        #[test]
        fn a_plan_without_an_enabled_curve_releases_and_reports_nothing_to_follow() {
            let (mut fans, handles) = fans_on_replay();
            fans.set_plan(plan(vec![curve(true)]));
            step(&mut fans, Some(45.0), 3);
            fans.take_changes();
            fans.set_plan(plan(vec![curve(false)]));
            assert_eq!(handles.fan_writes().last(), Some(&(0x102, BIOS_MODE)));
            assert_eq!(fans.take_changes(), Some(Vec::new()));
            assert!(!fans.needs_steps());
        }

        fn crashed_fans() -> (Fans, ReplayHandles) {
            let (access, handles) = crashed_replay_access();
            assert_eq!(handles.stored_channels(), ["pwm1"], "setup: the crash left a record");
            (Fans::new(Some(access)), handles)
        }

        fn pwm1_in(phase: FanPhase) -> Vec<FanRuntime> {
            vec![FanRuntime {
                channel: "board.pwm1".into(),
                phase,
                target_duty_pct: None,
                problem: None,
            }]
        }

        #[test]
        fn a_plan_without_curves_hands_back_a_channel_a_crashed_worker_left_in_manual_mode() {
            let (mut fans, handles) = crashed_fans();
            fans.set_plan(plan(Vec::new()));
            assert_eq!(
                handles.fan_writes().last(),
                Some(&(0x102, BIOS_MODE)),
                "(recovery): expected pwm1's BIOS mode written back | received: {:?}",
                handles.fan_writes()
            );
            assert_eq!(handles.stored_channels(), Vec::<String>::new(), "the record is cleared");
            assert_eq!(fans.take_changes(), Some(pwm1_in(FanPhase::Bios)));
            assert!(!fans.needs_steps(), "nothing is left to follow, so no session is kept");
        }

        #[test]
        fn a_plan_whose_only_curve_is_disabled_still_hands_the_crashed_channel_back() {
            let (mut fans, handles) = crashed_fans();
            fans.set_plan(plan(vec![curve(false)]));
            assert_eq!(handles.fan_writes().last(), Some(&(0x102, BIOS_MODE)));
            assert_eq!(handles.stored_channels(), Vec::<String>::new());
            assert!(!fans.needs_steps());
        }

        #[test]
        fn a_plan_with_nothing_to_follow_and_nothing_to_hand_back_writes_nothing() {
            let (mut fans, handles) = fans_on_replay();
            fans.set_plan(plan(Vec::new()));
            assert!(handles.fan_writes().is_empty());
            assert_eq!(fans.take_changes(), None, "an idle worker says nothing");
            assert!(!fans.needs_steps());
        }

        #[test]
        fn a_crashed_channel_is_handed_back_before_its_curve_takes_it() {
            let (mut fans, handles) = crashed_fans();
            fans.set_plan(plan(vec![curve(true)]));
            assert_eq!(handles.fan_writes().last(), Some(&(0x102, BIOS_MODE)), "recovery first");
            assert_eq!(handles.stored_channels(), Vec::<String>::new());
            assert!(fans.needs_steps(), "the curve is followed from here");
        }

        #[test]
        fn a_session_that_cannot_open_is_reported_per_channel_and_writes_nothing() {
            let (access, handles) =
                refusing_access("(fan session): expected the fan bus to open | received: busy");
            let mut fans = Fans::new(Some(access));
            fans.set_plan(plan(vec![curve(true)]));
            let runtime = fans.take_changes().unwrap();
            assert_eq!(runtime[0].phase, FanPhase::Bios);
            assert!(runtime[0].problem.as_deref().unwrap().contains("busy"), "{runtime:?}");
            assert!(!fans.needs_steps());
            assert!(handles.fan_writes().is_empty());
        }

        #[test]
        fn a_board_without_access_opens_nothing_for_any_plan() {
            let mut fans = Fans::new(None);
            fans.set_plan(plan(vec![curve(true)]));
            assert!(!fans.needs_steps());
            assert!(fans.take_changes().is_none());
        }

        #[test]
        fn a_plan_that_arrives_while_suspended_starts_released() {
            let (mut fans, handles) = fans_on_replay();
            fans.suspend();
            fans.set_plan(plan(vec![curve(true)]));
            step(&mut fans, Some(45.0), 5);
            assert!(handles.fan_writes().is_empty(), "nothing is written while suspended");
        }
    }
}
