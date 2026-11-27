//! A fan-control session: the profile's curves put on the channels of one
//! validated board, run by the fail-safe controller. Compiled only with the
//! `fan-control` cargo feature.
//!
//! The sensor worker holds one session while the profile has an enabled
//! curve, or while a channel a crashed predecessor left in manual mode waits
//! to be handed back. It feeds [`FanSession::tick`] the sensor readings once a control
//! step and sends what comes back to the service as [`FanRuntime`]s. The
//! session adds nothing to the controller's rules (full speed when a sensor
//! fails, the BIOS mode back on stop, suspend, shutdown and panic); it
//! translates between the profile's types and the controller's, and keeps
//! what the status view needs: each channel's phase, last duty and problem.
//!
//! A session is only possible with a [`ControlGrant`], which only a validated
//! catalog entry yields, so no code path here reaches a board nobody tried.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use syspilot_proto::message::{FanPhase, FanRuntime};
use syspilot_proto::profile::FanCurve;

use crate::bus::{BusLocks, SuperIo};
use crate::chips::Detected;

use super::board::{BoardChannel, ControlGrant};
use super::curve::{Curve, CurvePoint, Readings, TemperatureSource};
use super::failsafe::{
    ChannelEvent, ChannelPlan, Event, FailSafePolicy, FanChannel, FanController, OriginalStore,
    PanicRelease, Phase, Tuning, install_panic_release, remove_panic_release,
};
use super::layout::channel_key;
use super::nct677x::{Nct677xChannel, Nct677xFanBus};

/// Why a session could not start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionError {
    expected: String,
    received: String,
}

impl SessionError {
    fn new(expected: impl Into<String>, received: impl Into<String>) -> Self {
        Self { expected: expected.into(), received: received.into() }
    }
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "(fan session): expected {} | received: {}", self.expected, self.received)
    }
}

impl std::error::Error for SessionError {}

/// What the session knows about one channel, kept between ticks.
struct Tracked {
    runtime: FanRuntime,
    /// The controller's id for the channel: `pwm` and its number. `None` for
    /// a curve whose channel the board does not have, which is reported and
    /// never controlled.
    controller_id: Option<String>,
}

/// The curves of a profile running on one board's fans.
pub struct FanSession {
    controller: FanController,
    /// What a panic hook restores from; see [`FanSession::install_panic_hook`].
    panic_release: Arc<PanicRelease>,
    tracked: Vec<Tracked>,
    sources: Vec<String>,
    /// Whether any curve runs. A session without one exists only to hand
    /// back what a crashed predecessor left in the store.
    follows_curves: bool,
}

/// The pwm output number of a controller channel id (`pwm3` is 3).
fn pwm_of_id(id: &str) -> Option<u8> {
    id.strip_prefix("pwm")?.parse().ok()
}

fn phase_of(phase: Phase) -> FanPhase {
    match phase {
        Phase::Bios => FanPhase::Bios,
        Phase::Manual => FanPhase::Manual,
        Phase::FailSafe => FanPhase::FailSafe,
        Phase::ReleasePending => FanPhase::ReleasePending,
        Phase::Blocked => FanPhase::Blocked,
    }
}

fn percent(duty: f64) -> u8 {
    duty.round().clamp(0.0, 100.0) as u8
}

/// The plan for one curve on one channel. The board entry's minimum duty is
/// a floor the curve cannot go below; a higher `min-duty-pct` in the profile
/// raises it.
fn plan_for(curve: &FanCurve, channel: &BoardChannel) -> Result<ChannelPlan, String> {
    let points = curve
        .points
        .iter()
        .map(|point| CurvePoint {
            temperature: f64::from(point.temp_c()),
            duty: f64::from(point.duty_pct()),
        })
        .collect();
    let source = if let [only] = curve.sources.as_slice() {
        TemperatureSource::single(only.clone())
    } else {
        TemperatureSource::maximum_of(curve.sources.clone()).map_err(|error| error.to_string())?
    };
    let mut limits = channel.clone();
    limits.limits.min_duty = limits.limits.min_duty.max(curve.min_duty_pct);
    let ramp = Some(f64::from(curve.ramp_pct_per_sec));
    let tuning = Tuning {
        hysteresis: f64::from(curve.hysteresis_c),
        max_rise_per_second: ramp,
        max_fall_per_second: ramp,
    };
    let curve = Curve::new(points).map_err(|error| error.to_string())?;
    ChannelPlan::new(&limits, curve, source, tuning).map_err(|error| error.to_string())
}

impl FanSession {
    /// Opens the board's fan bus and puts every enabled curve on its channel.
    ///
    /// `io` must be a fresh handle on the Super I/O module (see
    /// [`Nct677xFanBus::open`]). A curve for a channel the board entry does
    /// not list, or one the engine refuses, is reported as that channel's
    /// problem and left to the BIOS; the others run. Crash recovery runs
    /// before the first tick, so a channel a dead predecessor left in manual
    /// mode is back with the BIOS (or raised to its minimum) first.
    ///
    /// Recovery covers every channel the store holds a record for, not only
    /// the channels of `curves`: a channel whose curve was removed, disabled
    /// or moved, or a profile with no curve at all, still gets its BIOS mode
    /// back. Those channels are reported too, and a session may then exist
    /// only for them (see [`FanSession::has_work`]).
    ///
    /// `Ok(None)` when there is nothing to do: no enabled curve and nothing
    /// in the store. The bus is not opened and nothing is written.
    pub fn start(
        grant: &ControlGrant<'_>,
        detected: &Detected,
        curves: &[FanCurve],
        io: Box<dyn SuperIo>,
        locks: Arc<dyn BusLocks>,
        mut store: Box<dyn OriginalStore>,
    ) -> Result<Option<Self>, SessionError> {
        let enabled: Vec<&FanCurve> = curves.iter().filter(|curve| curve.enabled).collect();
        let stored = store.channels().map_err(|error| {
            SessionError::new("the stored originals to be listed", error.to_string())
        })?;
        let mut tracked = Vec::new();
        let mut planned: Vec<(ChannelPlan, &BoardChannel)> = Vec::new();
        let mut sources = BTreeSet::new();
        for curve in &enabled {
            let key = curve.channel.clone();
            let refuse = |problem: String| Tracked {
                runtime: FanRuntime {
                    channel: key.clone(),
                    phase: FanPhase::Bios,
                    target_duty_pct: None,
                    problem: Some(problem),
                },
                controller_id: None,
            };
            let Some(pwm) = curve.pwm_output() else {
                tracked.push(refuse(format!(
                    "(fan curve): expected a channel key like board.pwm2 | received: {}",
                    curve.channel
                )));
                continue;
            };
            let Some(channel) = grant.channel(pwm) else {
                tracked.push(refuse(format!(
                    "(fan curve): expected a channel of the board's entry | received: {}",
                    curve.channel
                )));
                continue;
            };
            match plan_for(curve, channel) {
                Ok(plan) => {
                    tracked.push(Tracked {
                        runtime: FanRuntime {
                            channel: channel_key(pwm),
                            phase: FanPhase::Bios,
                            target_duty_pct: None,
                            problem: None,
                        },
                        controller_id: Some(plan.id().to_owned()),
                    });
                    sources.extend(curve.sources.iter().cloned());
                    planned.push((plan, channel));
                }
                Err(problem) => tracked.push(refuse(problem)),
            }
        }
        // Every other channel with a record is handed back and never followed.
        let mut restored: Vec<(ChannelPlan, &BoardChannel)> = Vec::new();
        for id in stored {
            if planned.iter().any(|(plan, _)| plan.id() == id) {
                continue;
            }
            let Some(channel) = pwm_of_id(&id).and_then(|pwm| grant.channel(pwm)) else {
                // A record the board's entry cannot explain stays untouched:
                // nothing here knows which registers it was about.
                tracked.push(Tracked {
                    runtime: FanRuntime {
                        channel: id.clone(),
                        phase: FanPhase::Blocked,
                        target_duty_pct: None,
                        problem: Some(format!(
                            "(fan recovery): expected a stored original of a channel of the board's entry | received: {id}; the record is kept"
                        )),
                    },
                    controller_id: None,
                });
                continue;
            };
            let plan = ChannelPlan::restore_only(channel)
                .map_err(|error| SessionError::new("a valid recovery plan", error.to_string()))?;
            tracked.push(Tracked {
                runtime: FanRuntime {
                    channel: channel_key(channel.pwm),
                    phase: FanPhase::Bios,
                    target_duty_pct: None,
                    problem: None,
                },
                controller_id: Some(plan.id().to_owned()),
            });
            restored.push((plan, channel));
        }
        if planned.is_empty() && restored.is_empty() {
            if tracked.is_empty() {
                return Ok(None);
            }
            return Err(SessionError::new(
                if enabled.is_empty() {
                    "a stored original of a channel of the board's entry"
                } else {
                    "at least one curve on a channel of the board's entry"
                },
                tracked
                    .iter()
                    .filter_map(|entry| entry.runtime.problem.clone())
                    .collect::<Vec<_>>()
                    .join("; "),
            ));
        }
        let follows_curves = !planned.is_empty();
        let bus = Nct677xFanBus::open(detected, grant, io, locks)
            .map_err(|error| SessionError::new("the fan bus to open", error.to_string()))?;
        let mut channels: Vec<(ChannelPlan, Box<dyn FanChannel>)> = Vec::new();
        for (plan, channel) in planned.into_iter().chain(restored) {
            let io = Nct677xChannel::new(bus.clone(), channel)
                .map_err(|error| SessionError::new("a fan channel to open", error.to_string()))?;
            channels.push((plan, Box::new(io)));
        }
        let controller = FanController::new(FailSafePolicy::default(), store, channels)
            .map_err(|error| SessionError::new("a valid controller", error.to_string()))?;
        let panic_release = controller.panic_release();
        let mut session = Self {
            controller,
            panic_release,
            tracked,
            sources: sources.into_iter().collect(),
            follows_curves,
        };
        let events = session.controller.recover();
        session.apply(&events);
        Ok(Some(session))
    }

    /// Whether the session still has a reason to exist: a curve runs, or a
    /// channel waits for a restore that failed and is retried on every step.
    /// A session that has neither can be dropped.
    pub fn has_work(&self) -> bool {
        self.follows_curves
            || self.tracked.iter().any(|entry| {
                entry.controller_id.as_deref().and_then(|id| self.controller.phase(id))
                    == Some(Phase::ReleasePending)
            })
    }

    /// Makes a panic anywhere in the process hand the fans back to the BIOS
    /// (see [`install_panic_release`]). Called by the process that owns the
    /// session, not by the session itself: a hook is process-wide. The
    /// process has one hook and one slot, so installing a session replaces
    /// the previous session's entry, and dropping the session empties it.
    ///
    /// The hook restores from [`PanicRelease`], not from the controller: a
    /// panic during `tick`, `suspend` or `stop` runs the hook in the middle
    /// of a step, with the controller borrowed and its registers possibly
    /// half written. The session shares nothing with the hook but that
    /// record, so no lock is held across register I/O for the hook to wait on.
    pub fn install_panic_hook(&self) {
        install_panic_release(self.panic_release.clone());
    }

    /// The sensor keys the curves read; readings of any other are not needed.
    pub fn sensor_keys(&self) -> &[String] {
        &self.sources
    }

    /// What each channel is doing now, in the order of the profile's curves.
    pub fn runtime(&self) -> Vec<FanRuntime> {
        self.tracked.iter().map(|entry| entry.runtime.clone()).collect()
    }

    /// One control step: `readings` by sensor key and the seconds since the
    /// previous step. Returns what each channel is doing afterwards.
    pub fn tick(&mut self, readings: &Readings, elapsed_seconds: f64) -> Vec<FanRuntime> {
        let events = self.controller.tick(readings, elapsed_seconds);
        self.apply(&events);
        self.runtime()
    }

    /// The machine is about to sleep: every channel goes back to the BIOS and
    /// nothing is written until [`FanSession::resume`].
    pub fn suspend(&mut self) -> Vec<FanRuntime> {
        let events = self.controller.suspend();
        self.apply(&events);
        self.runtime()
    }

    /// The machine woke up. Control comes back only after the sensors have
    /// read well again for a few steps.
    pub fn resume(&mut self) {
        self.controller.resume();
    }

    /// Hands every channel back to the BIOS for good.
    pub fn stop(&mut self) -> Vec<FanRuntime> {
        let events = self.controller.stop();
        self.apply(&events);
        self.runtime()
    }

    /// Folds the controller's events into the tracked runtime and reads the
    /// phases it ends in.
    fn apply(&mut self, events: &[Event]) {
        for event in events {
            let Some(entry) = self
                .tracked
                .iter_mut()
                .find(|entry| entry.controller_id.as_deref() == Some(event.channel.as_str()))
            else {
                continue;
            };
            let runtime = &mut entry.runtime;
            match &event.event {
                ChannelEvent::TookControl { duty }
                | ChannelEvent::DutySet { duty }
                | ChannelEvent::RaisedToMinimum { duty } => {
                    runtime.target_duty_pct = Some(percent(*duty));
                    // Following the curve again: whatever went wrong is over.
                    if !matches!(event.event, ChannelEvent::RaisedToMinimum { .. }) {
                        runtime.problem = None;
                    }
                }
                ChannelEvent::FailSafe { reason } => {
                    runtime.target_duty_pct = Some(100);
                    runtime.problem = Some(reason.clone());
                }
                ChannelEvent::Released { .. } => {
                    runtime.target_duty_pct = None;
                    runtime.problem = None;
                }
                ChannelEvent::Blocked { reason } => runtime.problem = Some(reason.clone()),
                ChannelEvent::Problem { detail } => runtime.problem = Some(detail.clone()),
            }
        }
        for entry in &mut self.tracked {
            if let Some(phase) =
                entry.controller_id.as_deref().and_then(|id| self.controller.phase(id))
            {
                entry.runtime.phase = phase_of(phase);
            }
        }
    }
}

impl Drop for FanSession {
    fn drop(&mut self) {
        // The panic slot holds a session's release (and with it the bus
        // handle) until told otherwise; a session that ends must let go.
        remove_panic_release(&self.panic_release);
    }
}

#[cfg(test)]
mod tests {
    use std::mem::ManuallyDrop;
    use std::sync::{Mutex, MutexGuard, PoisonError};

    use super::*;
    use crate::bus::replay::{ReplaySuperIo, SuperIoWrite, superio_hwm_key};
    use crate::bus::{BusKind, FakeBusLocks, Fixture, SuperIoSlot};
    use crate::chips::nct67xx::Chip;
    use crate::fan::board::BoardCatalog;
    use crate::fan::failsafe::{ControlSnapshot, StoreError, serialize_panic_hook_tests};
    use syspilot_proto::profile::CurvePoint as ProfilePoint;

    const BASE: u16 = 0x0A20;
    const BOARD: &str = r#"
[[board]]
manufacturer = "ACME"
product = "TEST BOARD"
chip = "NCT6779D"
validated = true
validated-by = "A. Tester"
validated-on = "2026-12-01"

[[board.channel]]
pwm = 1
tach = 1
label = "CPU fan"
min-duty = 20
may-stop = false

[[board.channel]]
pwm = 3
label = "Case fan"
min-duty = 25
may-stop = false
"#;
    /// What the BIOS left: SmartFan IV with tolerance bits set, duty 0x66.
    const BIOS_MODE: u8 = 0x45;
    const BIOS_DUTY: u8 = 0x66;
    const CPU: &str = "cpu.package.temp";

    /// An in-memory original store, shared by clones, so a "restarted"
    /// session finds what a "crashed" one saved.
    #[derive(Clone, Default)]
    struct MemoryOriginalStore(Arc<Mutex<Vec<(String, ControlSnapshot)>>>);

    impl MemoryOriginalStore {
        /// The records, also after a test thread panicked while holding them,
        /// so that a store method never panics inside its `Result`.
        fn records(&self) -> MutexGuard<'_, Vec<(String, ControlSnapshot)>> {
            self.0.lock().unwrap_or_else(PoisonError::into_inner)
        }
    }

    impl OriginalStore for MemoryOriginalStore {
        fn load(&mut self, channel: &str) -> Result<Option<ControlSnapshot>, StoreError> {
            let records = self.records();
            Ok(records.iter().find(|(id, _)| id == channel).map(|(_, record)| record.clone()))
        }

        fn save(&mut self, channel: &str, original: &ControlSnapshot) -> Result<(), StoreError> {
            self.records().push((channel.to_owned(), original.clone()));
            Ok(())
        }

        fn clear(&mut self, channel: &str) -> Result<(), StoreError> {
            self.records().retain(|(id, _)| id != channel);
            Ok(())
        }

        fn channels(&mut self) -> Result<Vec<String>, StoreError> {
            Ok(self.records().iter().map(|(id, _)| id.clone()).collect())
        }
    }

    fn detected() -> Detected {
        Detected::Nuvoton {
            slot: SuperIoSlot::Primary,
            chip: Chip::Nct6779D,
            id_and_revision: 0xC562,
            base: BASE,
            lock_register: 0,
        }
    }

    /// Outputs 1 and 3 of an NCT6779D under BIOS control.
    fn chip_fixture() -> Fixture {
        let mut fixture = Fixture::new();
        let mut set = |address: u16, value: u8| {
            fixture.set(
                superio_hwm_key(SuperIoSlot::Primary, (address >> 8) as u8, address as u8),
                u64::from(value),
            );
        };
        for (mode, duty, read_back) in [(0x102, 0x109, 0x001), (0x302, 0x309, 0x011)] {
            set(mode, BIOS_MODE);
            set(duty, BIOS_DUTY);
            set(read_back, BIOS_DUTY);
        }
        fixture
    }

    /// The replayed board a session runs on, with the handles a test checks.
    struct Rig {
        catalog: BoardCatalog,
        io_log: Arc<Mutex<Vec<SuperIoWrite>>>,
        io: Option<ReplaySuperIo>,
        locks: Arc<FakeBusLocks>,
        store: MemoryOriginalStore,
    }

    impl Rig {
        fn new() -> Self {
            Self::over(chip_fixture())
        }

        fn over(fixture: Fixture) -> Self {
            let io = ReplaySuperIo::new(fixture);
            Self {
                catalog: BoardCatalog::parse("test", BOARD).unwrap(),
                io_log: io.write_log(),
                io: Some(io),
                locks: Arc::new(FakeBusLocks::free()),
                store: MemoryOriginalStore::default(),
            }
        }

        /// A chip a crashed predecessor left with output 3 in manual mode at
        /// 6 % (below its 25 % minimum), and the store still holding the
        /// BIOS registers it saved for it.
        fn crashed_on_pwm3() -> Self {
            let mut fixture = chip_fixture();
            for (address, value) in [(0x302u16, BIOS_MODE & 0x0F), (0x309, 0x10), (0x011, 0x10)] {
                fixture.set(
                    superio_hwm_key(SuperIoSlot::Primary, (address >> 8) as u8, address as u8),
                    u64::from(value),
                );
            }
            let rig = Self::over(fixture);
            rig.store.0.lock().unwrap().push((
                "pwm3".into(),
                ControlSnapshot { registers: vec![(0x309, BIOS_DUTY), (0x302, BIOS_MODE)] },
            ));
            rig
        }

        fn start_if_any(
            &mut self,
            curves: &[FanCurve],
        ) -> Result<Option<FanSession>, SessionError> {
            let grant =
                self.catalog.offer("ACME", "TEST BOARD", Chip::Nct6779D).map_err(|reason| {
                    SessionError::new(
                        "the test board to be offered fan control",
                        format!("{reason:?}"),
                    )
                })?;
            let io = self.io.take().expect("a rig starts one session");
            FanSession::start(
                &grant,
                &detected(),
                curves,
                Box::new(io),
                self.locks.clone(),
                Box::new(self.store.clone()),
            )
        }

        fn start(&mut self, curves: &[FanCurve]) -> Result<FanSession, SessionError> {
            self.start_if_any(curves).map(|session| session.expect("the rig has work to do"))
        }

        /// The fan registers written so far, as `(address, value)`.
        fn fan_writes(&self) -> Vec<(u16, u8)> {
            self.io_log
                .lock()
                .unwrap()
                .iter()
                .filter_map(|write| match write {
                    SuperIoWrite::HwmFanRegister { address, value, .. } => Some((*address, *value)),
                    _ => None,
                })
                .collect()
        }

        /// A register of the replayed chip: the last value written to it, or
        /// the fixture's.
        fn register(&self, address: u16) -> u8 {
            self.fan_writes().iter().rev().find(|(written, _)| *written == address).map_or_else(
                || match address {
                    0x102 | 0x302 => BIOS_MODE,
                    _ => BIOS_DUTY,
                },
                |(_, value)| *value,
            )
        }
    }

    fn curve(channel: &str) -> FanCurve {
        FanCurve {
            channel: channel.into(),
            enabled: true,
            sources: vec![CPU.into()],
            points: vec![ProfilePoint(30, 20), ProfilePoint(60, 60), ProfilePoint(80, 100)],
            hysteresis_c: 0,
            min_duty_pct: 0,
            ramp_pct_per_sec: 100,
        }
    }

    fn readings(cpu: Option<f64>) -> Readings {
        [(CPU.to_owned(), cpu)].into_iter().collect()
    }

    fn steps(session: &mut FanSession, cpu: Option<f64>, count: usize) -> Vec<FanRuntime> {
        let mut last = Vec::new();
        for _ in 0..count {
            last = session.tick(&readings(cpu), 1.0);
        }
        last
    }

    #[test]
    fn a_curve_is_followed_after_three_good_readings_and_reported() {
        let mut rig = Rig::new();
        let mut session = rig.start(&[curve("board.pwm1")]).unwrap();
        assert_eq!(session.sensor_keys(), [CPU]);
        assert_eq!(session.runtime()[0].phase, FanPhase::Bios, "nothing is taken before ticks");
        assert!(rig.fan_writes().is_empty(), "starting writes no fan register");

        let runtime = steps(&mut session, Some(45.0), 3);
        // 45 degrees on 30:20 .. 60:60 is 40 %.
        assert_eq!(
            runtime,
            [FanRuntime {
                channel: "board.pwm1".into(),
                phase: FanPhase::Manual,
                target_duty_pct: Some(40),
                problem: None,
            }]
        );
        assert_eq!(rig.register(0x102), BIOS_MODE & 0x0F, "manual mode, tolerance bits kept");
        assert_eq!(rig.register(0x109), 102, "40 % of 255");
        assert_eq!(rig.locks.live_guards(), 0);
    }

    #[test]
    fn a_failing_sensor_holds_full_speed_with_the_reason_then_the_curve_returns() {
        let mut rig = Rig::new();
        let mut session = rig.start(&[curve("board.pwm1")]).unwrap();
        steps(&mut session, Some(45.0), 3);

        let runtime = steps(&mut session, None, 1);
        assert_eq!(runtime[0].phase, FanPhase::FailSafe);
        assert_eq!(runtime[0].target_duty_pct, Some(100));
        let reason = runtime[0].problem.clone().unwrap();
        assert!(
            reason.contains(CPU),
            "(fail-safe reason): expected the sensor's key | received: {reason}"
        );
        assert_eq!(rig.register(0x109), 0xFF);

        // Three good readings, then the ramp (100 % per second here) follows
        // the curve again and the problem is gone.
        let runtime = steps(&mut session, Some(45.0), 3);
        assert_eq!(runtime[0].phase, FanPhase::Manual);
        assert_eq!(runtime[0].problem, None);
        assert_eq!(runtime[0].target_duty_pct, Some(40));
    }

    #[test]
    fn stopping_hands_the_registers_back_and_reports_the_bios() {
        let mut rig = Rig::new();
        let mut session = rig.start(&[curve("board.pwm1")]).unwrap();
        steps(&mut session, Some(45.0), 3);
        let runtime = session.stop();
        assert_eq!(runtime[0].phase, FanPhase::Bios);
        assert_eq!(runtime[0].target_duty_pct, None);
        assert_eq!(rig.register(0x102), BIOS_MODE);
        assert_eq!(rig.register(0x109), BIOS_DUTY);
        assert!(rig.store.0.lock().unwrap().is_empty(), "the stored original is cleared");
        let writes = rig.fan_writes().len();
        steps(&mut session, Some(45.0), 3);
        assert_eq!(rig.fan_writes().len(), writes, "nothing is written after a stop");
    }

    #[test]
    fn dropping_the_session_hands_the_registers_back() {
        let mut rig = Rig::new();
        let mut session = rig.start(&[curve("board.pwm1")]).unwrap();
        steps(&mut session, Some(45.0), 3);
        drop(session);
        assert_eq!(rig.register(0x102), BIOS_MODE);
        assert_eq!(rig.register(0x109), BIOS_DUTY);
    }

    #[test]
    fn the_panic_hooks_record_restores_the_registers_in_the_middle_of_a_step() {
        let mut rig = Rig::new();
        let mut session = rig.start(&[curve("board.pwm1")]).unwrap();
        steps(&mut session, Some(45.0), 3);
        assert_eq!(rig.register(0x102), BIOS_MODE & 0x0F, "setup: manual mode");

        // What the hook holds is all it uses: not the session, not the
        // controller, which a panicking step would still be borrowing.
        let release = session.panic_release.clone();
        assert!(release.release(), "(panic path): expected pwm1 restored");
        let writes = rig.fan_writes();
        assert_eq!(
            writes.last(),
            Some(&(0x102, BIOS_MODE)),
            "(panic path): expected the BIOS mode to be the last write | received: {writes:?}"
        );
        assert_eq!(rig.register(0x109), BIOS_DUTY);
        assert!(rig.store.0.lock().unwrap().is_empty(), "the stored original is cleared");
        assert_eq!(rig.locks.live_guards(), 0);

        // A session that survives sees a channel that is no longer its own
        // and writes nothing over the BIOS's registers.
        let after = rig.fan_writes().len();
        steps(&mut session, Some(45.0), 3);
        assert_eq!(rig.fan_writes().len(), after, "nothing is written after the panic restore");
    }

    #[test]
    fn a_replaced_or_ended_session_lets_go_of_its_bus_handle_despite_the_panic_hook() {
        let _serial = serialize_panic_hook_tests();
        let mut first_rig = Rig::new();
        let mut second_rig = Rig::new();
        // The bus holds one clone of the rig's bus lock; the rig the other.
        let first_session = first_rig.start(&[curve("board.pwm1")]).unwrap();
        first_session.install_panic_hook();
        assert_eq!(Arc::strong_count(&first_rig.locks), 2, "setup: the session holds its bus");

        // The process replaces the session: the new one is installed, and the
        // old one is dropped.
        let second_session = second_rig.start(&[curve("board.pwm1")]).unwrap();
        second_session.install_panic_hook();
        drop(first_session);
        assert_eq!(
            Arc::strong_count(&first_rig.locks),
            1,
            "(panic hook): expected the replaced session's bus handle dropped | received it still held"
        );
        assert_eq!(Arc::strong_count(&second_rig.locks), 2, "the live session keeps its own");

        // A session that ends with nothing to replace it lets go too.
        drop(second_session);
        assert_eq!(
            Arc::strong_count(&second_rig.locks),
            1,
            "(panic hook): expected the ended session's bus handle dropped | received it still held"
        );
    }

    #[test]
    fn suspend_releases_and_control_returns_only_after_good_readings() {
        let mut rig = Rig::new();
        let mut session = rig.start(&[curve("board.pwm1")]).unwrap();
        steps(&mut session, Some(45.0), 3);
        let runtime = session.suspend();
        assert_eq!(runtime[0].phase, FanPhase::Bios);
        assert_eq!(rig.register(0x102), BIOS_MODE);

        let writes = rig.fan_writes().len();
        steps(&mut session, Some(45.0), 5);
        assert_eq!(rig.fan_writes().len(), writes, "ticks while suspended write nothing");

        session.resume();
        assert_eq!(steps(&mut session, Some(45.0), 2)[0].phase, FanPhase::Bios);
        assert_eq!(steps(&mut session, Some(45.0), 1)[0].phase, FanPhase::Manual);
    }

    #[test]
    fn a_crash_leaves_manual_mode_and_the_next_session_restores_the_stored_bios_mode() {
        let mut rig = Rig::new();
        let mut crashed = rig.start(&[curve("board.pwm1")]).unwrap();
        steps(&mut crashed, Some(45.0), 3);
        // Its `Drop` would hand the fans back; a crash does not get to run it.
        let _crashed = ManuallyDrop::new(crashed);
        assert_eq!(rig.register(0x102), BIOS_MODE & 0x0F, "the crash left manual mode");
        assert_eq!(rig.store.0.lock().unwrap().len(), 1);

        // A restart: a new handle on the same (replayed) chip state, which a
        // real restart would find in the registers. The replay bus holds the
        // fixture, so the check is on what the recovery writes.
        let before = rig.fan_writes().len();
        let mut next = Rig::new();
        next.store = rig.store.clone();
        let restarted = next.start(&[curve("board.pwm1")]).unwrap();
        let restored = next.fan_writes();
        assert!(before > 0 && !restored.is_empty(), "recovery writes the stored registers");
        assert_eq!(
            restored.last(),
            Some(&(0x102, BIOS_MODE)),
            "(recovery): expected the BIOS mode to be the last write"
        );
        assert_eq!(restarted.runtime()[0].phase, FanPhase::Bios);
        assert!(next.store.0.lock().unwrap().is_empty());
    }

    #[test]
    fn a_curve_for_a_channel_the_board_does_not_list_is_reported_and_never_written() {
        let mut rig = Rig::new();
        let mut session = rig.start(&[curve("board.pwm1"), curve("board.pwm4")]).unwrap();
        let runtime = steps(&mut session, Some(45.0), 3);
        assert_eq!(runtime[0].phase, FanPhase::Manual);
        assert_eq!(runtime[1].channel, "board.pwm4");
        assert_eq!(runtime[1].phase, FanPhase::Bios);
        assert_eq!(
            runtime[1].problem.as_deref(),
            Some("(fan curve): expected a channel of the board's entry | received: board.pwm4")
        );
        assert!(
            rig.fan_writes().iter().all(|(address, _)| [0x102, 0x109].contains(address)),
            "only pwm1's registers are written"
        );
    }

    #[test]
    fn a_curve_only_on_a_channel_the_board_lacks_refuses_the_session_and_opens_no_bus() {
        let mut rig = Rig::new();
        let error = rig.start(&[curve("board.pwm5")]).err().unwrap().to_string();
        assert!(
            error.starts_with(
                "(fan session): expected at least one curve on a channel of the board's entry"
            ),
            "{error}"
        );
        assert!(rig.fan_writes().is_empty());
        assert!(rig.io_log.lock().unwrap().is_empty(), "the bus was not even opened");
    }

    /// The writes to output 3's registers, which no curve of these tests
    /// follows.
    fn pwm3_writes(rig: &Rig) -> Vec<(u16, u8)> {
        rig.fan_writes()
            .into_iter()
            .filter(|(address, _)| [0x302, 0x309].contains(address))
            .collect()
    }

    #[test]
    fn a_plan_with_no_enabled_curve_and_an_empty_store_opens_nothing() {
        let mut off = curve("board.pwm1");
        off.enabled = false;
        for curves in [vec![], vec![off]] {
            let mut rig = Rig::new();
            let session = rig.start_if_any(&curves).unwrap();
            assert!(session.is_none(), "(fan session): expected no session | received one");
            assert!(rig.io_log.lock().unwrap().is_empty(), "the bus was not even opened");
        }
    }

    #[test]
    fn a_crashed_channel_is_restored_when_the_plan_has_no_curves_at_all() {
        let mut rig = Rig::crashed_on_pwm3();
        let session = rig.start(&[]).unwrap();

        let writes = pwm3_writes(&rig);
        assert_eq!(
            writes.last(),
            Some(&(0x302, BIOS_MODE)),
            "(recovery): expected pwm3's BIOS mode to be the last write | received: {writes:?}"
        );
        assert_eq!(
            writes.first(),
            Some(&(0x309, 64)),
            "(recovery): expected pwm3 raised to its 25 % minimum first | received: {writes:?}"
        );
        assert!(
            rig.store.0.lock().unwrap().is_empty(),
            "(recovery): expected pwm3's record cleared once restored"
        );
        assert_eq!(
            session.runtime(),
            [FanRuntime {
                channel: "board.pwm3".into(),
                phase: FanPhase::Bios,
                target_duty_pct: None,
                problem: None,
            }]
        );
        assert!(!session.has_work(), "nothing is left to follow or retry");
    }

    #[test]
    fn a_crashed_channel_whose_curve_moved_is_restored_and_never_followed() {
        let mut rig = Rig::crashed_on_pwm3();
        let mut session = rig.start(&[curve("board.pwm1")]).unwrap();
        let after_recovery = pwm3_writes(&rig);
        assert_eq!(after_recovery.last(), Some(&(0x302, BIOS_MODE)), "{after_recovery:?}");
        assert!(rig.store.0.lock().unwrap().is_empty());

        let runtime = steps(&mut session, Some(45.0), 6);
        assert_eq!(
            runtime.iter().map(|entry| (entry.channel.as_str(), entry.phase)).collect::<Vec<_>>(),
            [("board.pwm1", FanPhase::Manual), ("board.pwm3", FanPhase::Bios)]
        );
        assert_eq!(pwm3_writes(&rig), after_recovery, "pwm3 is never written after recovery");
        assert!(session.has_work(), "pwm1 still follows its curve");
    }

    #[test]
    fn a_stored_original_the_board_cannot_explain_is_reported_blocked_and_kept() {
        let mut rig = Rig::new();
        rig.store
            .0
            .lock()
            .unwrap()
            .push(("pwm9".into(), ControlSnapshot { registers: vec![(0x909, 1), (0x902, 2)] }));
        let session = rig.start(&[curve("board.pwm1")]).unwrap();
        let runtime = session.runtime();
        assert_eq!(runtime[1].channel, "pwm9");
        assert_eq!(runtime[1].phase, FanPhase::Blocked);
        assert!(runtime[1].problem.as_deref().unwrap().contains("received: pwm9"), "{runtime:?}");
        assert_eq!(rig.store.0.lock().unwrap().len(), 1, "the record is kept");
        assert!(rig.fan_writes().is_empty(), "nothing is written for a channel nobody knows");

        // With no curve to run, that alone is not a session.
        let mut rig = Rig::new();
        rig.store
            .0
            .lock()
            .unwrap()
            .push(("pwm9".into(), ControlSnapshot { registers: vec![(0x909, 1), (0x902, 2)] }));
        let error = rig.start(&[]).err().unwrap().to_string();
        assert!(
            error.starts_with(
                "(fan session): expected a stored original of a channel of the board's entry"
            ) && error.contains("received: (fan recovery)"),
            "{error}"
        );
        assert!(rig.io_log.lock().unwrap().is_empty(), "the bus was not opened");
    }

    #[test]
    fn a_stored_original_that_cannot_be_listed_refuses_the_session() {
        struct UnreadableStore;
        impl OriginalStore for UnreadableStore {
            fn load(&mut self, _: &str) -> Result<Option<ControlSnapshot>, StoreError> {
                Err(StoreError("an injected load failure".into()))
            }
            fn save(&mut self, _: &str, _: &ControlSnapshot) -> Result<(), StoreError> {
                Err(StoreError("an injected save failure".into()))
            }
            fn clear(&mut self, _: &str) -> Result<(), StoreError> {
                Err(StoreError("an injected clear failure".into()))
            }
            fn channels(&mut self) -> Result<Vec<String>, StoreError> {
                Err(StoreError("an injected list failure".into()))
            }
        }
        let rig = Rig::new();
        let grant = rig.catalog.offer("ACME", "TEST BOARD", Chip::Nct6779D).unwrap();
        let error = FanSession::start(
            &grant,
            &detected(),
            &[curve("board.pwm1")],
            Box::new(ReplaySuperIo::new(chip_fixture())),
            rig.locks.clone(),
            Box::new(UnreadableStore),
        )
        .err()
        .unwrap()
        .to_string();
        assert!(
            error.starts_with("(fan session): expected the stored originals to be listed")
                && error.contains("an injected list failure"),
            "{error}"
        );
    }

    #[test]
    fn the_profiles_minimum_duty_raises_the_boards_floor_and_never_lowers_it() {
        // Board floor 20 for pwm1; the profile asks for 50.
        let mut high = curve("board.pwm1");
        high.min_duty_pct = 50;
        let mut rig = Rig::new();
        let mut session = rig.start(&[high]).unwrap();
        // 31 degrees is 20 % on the curve; the floor lifts it to 50.
        assert_eq!(steps(&mut session, Some(31.0), 3)[0].target_duty_pct, Some(50));

        // A profile floor below the board's does not lower it.
        let mut low = curve("board.pwm1");
        low.min_duty_pct = 5;
        let mut rig = Rig::new();
        let mut session = rig.start(&[low]).unwrap();
        assert_eq!(steps(&mut session, Some(10.0), 3)[0].target_duty_pct, Some(20));
    }

    #[test]
    fn the_hottest_of_several_sources_drives_the_curve() {
        let mut two = curve("board.pwm1");
        two.sources = vec![CPU.into(), "gpu0.temp".into()];
        let mut rig = Rig::new();
        let mut session = rig.start(&[two]).unwrap();
        assert_eq!(session.sensor_keys(), ["cpu.package.temp", "gpu0.temp"]);
        let both: Readings = [(CPU.to_owned(), Some(30.0)), ("gpu0.temp".to_owned(), Some(60.0))]
            .into_iter()
            .collect();
        let mut last = Vec::new();
        for _ in 0..3 {
            last = session.tick(&both, 1.0);
        }
        assert_eq!(last[0].target_duty_pct, Some(60), "the GPU is hotter");
    }

    #[test]
    fn a_bus_held_by_another_program_stops_the_session_from_starting() {
        let mut rig = Rig::new();
        rig.locks = Arc::new(FakeBusLocks::held_by_another_program(&[BusKind::Isa]));
        let error = rig.start(&[curve("board.pwm1")]).err().unwrap().to_string();
        assert!(error.starts_with("(fan session): expected the fan bus to open"), "{error}");
        assert!(rig.fan_writes().is_empty());
    }
}
