//! The fail-safe fan controller. Compiled only with the `fan-control` cargo
//! feature: it is the code that decides when a fan register is written.
//!
//! The controller drives [`FanChannel`]s, one per fan output, from sensor
//! readings and the curves of [`super::curve`]. It owns these rules, each
//! tested against fakes:
//!
//! 1. **The BIOS mode is saved before the first write.** The registers that
//!    define a channel's original control mode are read, handed to the
//!    [`OriginalStore`] (a file in the service, so it survives a crash) and
//!    only then is the channel written.
//! 2. **BIOS control comes back** on stop, before shutdown, on suspend and
//!    from the panic path ([`PanicRelease`], which does not need the
//!    controller); a restore that fails is retried on every tick and keeps its
//!    record.
//! 3. **A sensor that fails drives its fans to 100 %**: a missing, `None`,
//!    non-finite or implausible reading, or a gap between ticks so long that
//!    the machine was probably asleep. Only the channels that use the sensor
//!    are affected; the others keep following their curves.
//! 4. **Control is retaken only after the sensors read correctly again**: after
//!    a fail-safe and after a resume, for a number of consecutive good
//!    readings ([`FailSafePolicy::good_readings_before_control`]).
//! 5. **A restart finds the fans safe.** The stored original of a crashed
//!    predecessor is the real BIOS mode; the channel is raised to at least its
//!    minimum duty if below, then handed back to the BIOS. A channel found in
//!    manual mode with no stored original is never recorded as an original
//!    (that state is the crashed program's, or another tool's): it is raised
//!    to at least its minimum duty and left alone, blocked from control. Every
//!    channel in the store is visited, with a curve in the plan or not
//!    ([`ChannelPlan::restore_only`]).
//!
//! The release profile builds with `panic = "abort"`, so `Drop` does not run
//! on a panic there. The process has to install [`install_panic_release`];
//! this crate does not install a hook by itself.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, Once, PoisonError, TryLockError};
use std::time::{Duration, Instant};

use crate::bus::BusError;

use super::board::BoardChannel;
use super::curve::{
    Curve, CurveError, CurveFollower, CurvePoint, FollowerSettings, MAX_DUTY, Readings,
    TemperatureSource,
};

// ---------------------------------------------------------------------------
// What a channel is
// ---------------------------------------------------------------------------

/// The registers that define a channel's control mode, as read before the
/// first write: `(register address, value)` in the order they must be
/// restored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlSnapshot {
    /// Register addresses and the values they held.
    pub registers: Vec<(u16, u8)>,
}

/// What a channel is doing now.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChannelState {
    /// Whether the output follows the manual-duty register (so SysPilot, or
    /// another tool, set it) rather than a BIOS automatic mode.
    pub manual: bool,
    /// The duty now, percent.
    pub duty_percent: f64,
}

/// The I/O of one fan output. Implemented on a real chip by the Super I/O
/// write path and in tests by a fake.
pub trait FanChannel: Send {
    /// Reads the registers that define the channel's original control mode.
    fn read_original(&mut self) -> Result<ControlSnapshot, BusError>;

    /// Reads the channel's mode and duty.
    fn read_state(&mut self) -> Result<ChannelState, BusError>;

    /// Takes manual control at `duty_percent`: the duty is written first and
    /// the mode switched after, so the output never jumps to a stale value.
    fn take_manual(&mut self, duty_percent: f64) -> Result<(), BusError>;

    /// Sets the duty of a channel in manual mode.
    fn set_duty(&mut self, duty_percent: f64) -> Result<(), BusError>;

    /// Writes `original` back, handing the channel to the BIOS again.
    fn restore(&mut self, original: &ControlSnapshot) -> Result<(), BusError>;

    /// A second handle on the same output, for the panic path
    /// ([`PanicRelease`]): it must restore without anything the controller
    /// holds across its own I/O, so it shares the bus and nothing else.
    fn duplicate(&self) -> Box<dyn FanChannel>;
}

/// The store could not read or write a record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreError(pub String);

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "(original store): expected the record to be read or written | received: {}",
            self.0
        )
    }
}

impl std::error::Error for StoreError {}

/// Where the saved originals live. The service keeps them in a file so that
/// a crash does not lose the only copy of the BIOS mode.
pub trait OriginalStore: Send {
    /// The original saved for `channel` and not yet cleared.
    fn load(&mut self, channel: &str) -> Result<Option<ControlSnapshot>, StoreError>;

    /// Saves the original of `channel`; durable when it returns.
    fn save(&mut self, channel: &str, original: &ControlSnapshot) -> Result<(), StoreError>;

    /// Forgets the original of `channel`, after it was restored.
    fn clear(&mut self, channel: &str) -> Result<(), StoreError>;

    /// Every channel that has a saved original, in no particular order. Crash
    /// recovery visits these whether or not the current plan has a curve for
    /// them: a channel is only safe once its record is gone.
    fn channels(&mut self) -> Result<Vec<String>, StoreError>;
}

// ---------------------------------------------------------------------------
// Plan, policy, errors
// ---------------------------------------------------------------------------

/// A rule of the controller that a configuration breaks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControllerError {
    expected: String,
    received: String,
}

impl fmt::Display for ControllerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "(fan controller): expected {} | received: {}", self.expected, self.received)
    }
}

impl std::error::Error for ControllerError {}

/// How a channel's curve is shaped, beyond the limits the board entry sets.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tuning {
    /// °C a falling temperature must drop before the duty goes down.
    pub hysteresis: f64,
    /// The most the duty may rise per second, percent; `None` for no limit.
    pub max_rise_per_second: Option<f64>,
    /// The most the duty may fall per second, percent; `None` for no limit.
    pub max_fall_per_second: Option<f64>,
}

/// What one channel follows.
pub struct ChannelPlan {
    id: String,
    source: TemperatureSource,
    follower: CurveFollower,
    /// Whether the controller may ever take the channel. `false` for a
    /// channel that is only handed back to the BIOS ([`ChannelPlan::restore_only`]).
    following: bool,
}

impl ChannelPlan {
    /// A plan that never takes control: for a channel a crashed predecessor
    /// recorded in the store and the current profile has no curve for. The
    /// controller restores it, clears its record, retries a failed restore on
    /// every tick, and writes nothing else.
    ///
    /// The curve and source are placeholders the controller never reads (a
    /// plan that does not follow is skipped before them); the placeholder is
    /// full duty, the safe answer should one ever be used.
    pub fn restore_only(channel: &BoardChannel) -> Result<Self, CurveError> {
        let point = |temperature, duty| CurvePoint { temperature, duty };
        let settings = FollowerSettings {
            hysteresis: 0.0,
            min_duty: f64::from(channel.limits.min_duty),
            max_rise_per_second: None,
            max_fall_per_second: None,
        };
        Ok(Self {
            id: format!("pwm{}", channel.pwm),
            source: TemperatureSource::single(""),
            follower: CurveFollower::new(
                Curve::new(vec![point(0.0, MAX_DUTY), point(100.0, MAX_DUTY)])?,
                settings,
            )?,
            following: false,
        })
    }

    /// A plan for a channel of a validated board: the minimum duty comes from
    /// the board entry's limits, so a user's curve cannot go below it.
    pub fn new(
        channel: &BoardChannel,
        curve: Curve,
        source: TemperatureSource,
        tuning: Tuning,
    ) -> Result<Self, CurveError> {
        let settings = FollowerSettings {
            hysteresis: tuning.hysteresis,
            min_duty: f64::from(channel.limits.min_duty),
            max_rise_per_second: tuning.max_rise_per_second,
            max_fall_per_second: tuning.max_fall_per_second,
        };
        Ok(Self {
            id: format!("pwm{}", channel.pwm),
            source,
            follower: CurveFollower::new(curve, settings)?,
            following: true,
        })
    }

    /// The channel's id, `pwm` and its output number.
    pub fn id(&self) -> &str {
        &self.id
    }

    fn minimum_duty(&self) -> f64 {
        self.follower.settings().min_duty
    }
}

/// When to give up on a sensor and when to trust it again.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FailSafePolicy {
    /// Consecutive failed ticks before the fans go to 100 %. 1 reacts to the
    /// first bad reading; more tolerates a flaky bus at the price of staying
    /// on the last duty meanwhile.
    pub faults_before_fail_safe: u32,
    /// Consecutive good ticks before control is taken or retaken.
    pub good_readings_before_control: u32,
    /// A tick later than this many seconds after the previous one counts as a
    /// failure: the machine most likely slept without telling the service.
    pub max_seconds_between_ticks: f64,
}

impl Default for FailSafePolicy {
    fn default() -> Self {
        Self {
            faults_before_fail_safe: 1,
            good_readings_before_control: 3,
            max_seconds_between_ticks: 15.0,
        }
    }
}

impl FailSafePolicy {
    fn validate(&self) -> Result<(), ControllerError> {
        let refuse = |expected: &str, received: String| {
            Err(ControllerError { expected: expected.to_owned(), received })
        };
        if self.faults_before_fail_safe == 0 {
            return refuse("faults-before-fail-safe of 1 or more", "0".into());
        }
        if self.good_readings_before_control == 0 {
            return refuse("good-readings-before-control of 1 or more", "0".into());
        }
        if !self.max_seconds_between_ticks.is_finite() || self.max_seconds_between_ticks <= 0.0 {
            return refuse(
                "max-seconds-between-ticks above 0",
                format!("{}", self.max_seconds_between_ticks),
            );
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// Why control was handed back to the BIOS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseReason {
    /// The service stopped control.
    Stop,
    /// The machine is about to shut down.
    Shutdown,
    /// The machine is about to sleep.
    Suspend,
    /// The controller was dropped.
    Dropped,
    /// A crashed predecessor left a stored original.
    Recovery,
    /// An earlier release failed and is being retried.
    Retry,
    /// A write failed, so the controller let go.
    WriteFailed,
}

/// What the controller did to one channel.
#[derive(Debug, Clone, PartialEq)]
pub enum ChannelEvent {
    /// Manual control taken, at this duty.
    TookControl {
        /// Percent.
        duty: f64,
    },
    /// The duty was set from the curve.
    DutySet {
        /// Percent.
        duty: f64,
    },
    /// The fan went to 100 % because of this.
    FailSafe {
        /// What failed.
        reason: String,
    },
    /// Control returned to the BIOS.
    Released {
        /// Why.
        reason: ReleaseReason,
    },
    /// A channel found below its minimum duty was raised to it.
    RaisedToMinimum {
        /// Percent.
        duty: f64,
    },
    /// The channel is in manual mode with no stored original, so it is left
    /// alone and never controlled.
    Blocked {
        /// What was found.
        reason: String,
    },
    /// Something failed and is reported; the controller carried on safely.
    Problem {
        /// What.
        detail: String,
    },
}

/// A [`ChannelEvent`] and the channel it happened to.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// The channel id, `pwm` and its number.
    pub channel: String,
    /// What happened.
    pub event: ChannelEvent,
}

/// Where a channel stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// The BIOS controls it.
    Bios,
    /// SysPilot holds it in manual mode and follows the curve.
    Manual,
    /// SysPilot holds it at 100 % because a sensor failed.
    FailSafe,
    /// A restore failed; it is retried on every tick.
    ReleasePending,
    /// Found in manual mode without a stored original; left alone.
    Blocked,
}

// ---------------------------------------------------------------------------
// What the panic path shares with the controller
// ---------------------------------------------------------------------------

/// Locks `mutex` even if a panic poisoned it: everything behind the locks
/// of this section stays consistent whichever thread panicked, because none
/// of them is held across anything that can panic.
fn locked<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Locks `mutex`, waiting until `deadline`; `None` when it is still taken
/// then. A thread that holds the lock may be the one that panicked and is
/// now running the hook, which would wait for itself forever.
fn locked_before<T: ?Sized>(mutex: &Mutex<T>, deadline: Instant) -> Option<MutexGuard<'_, T>> {
    loop {
        match mutex.try_lock() {
            Ok(guard) => return Some(guard),
            Err(TryLockError::Poisoned(poisoned)) => return Some(poisoned.into_inner()),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(TryLockError::WouldBlock) => return None,
        }
    }
}

/// A channel's saved BIOS registers, shared by the controller and the panic
/// path. The lock is held only to copy or replace the value, never across
/// register I/O, so the panic path can always read it.
#[derive(Clone, Default)]
struct Original(Arc<Mutex<Option<ControlSnapshot>>>);

impl Original {
    fn get(&self) -> Option<ControlSnapshot> {
        locked(&self.0).clone()
    }

    fn set(&self, original: Option<ControlSnapshot>) {
        *locked(&self.0) = original;
    }
}

/// The controller's [`OriginalStore`], shared with the panic path. Each call
/// takes the lock for that call alone, so the controller never holds it
/// across register I/O.
#[derive(Clone)]
struct SharedStore(Arc<Mutex<Box<dyn OriginalStore>>>);

impl SharedStore {
    /// Forgets `channel`'s record, giving up at `deadline`. `false` when it
    /// could not: the record stays, and the next start restores the same
    /// registers again, which is harmless.
    fn clear_before(&self, channel: &str, deadline: Instant) -> bool {
        locked_before(&self.0, deadline).is_some_and(|mut store| store.clear(channel).is_ok())
    }
}

impl OriginalStore for SharedStore {
    fn load(&mut self, channel: &str) -> Result<Option<ControlSnapshot>, StoreError> {
        locked(&self.0).load(channel)
    }

    fn save(&mut self, channel: &str, original: &ControlSnapshot) -> Result<(), StoreError> {
        locked(&self.0).save(channel, original)
    }

    fn clear(&mut self, channel: &str) -> Result<(), StoreError> {
        locked(&self.0).clear(channel)
    }

    fn channels(&mut self) -> Result<Vec<String>, StoreError> {
        locked(&self.0).channels()
    }
}

// ---------------------------------------------------------------------------
// One managed channel
// ---------------------------------------------------------------------------

struct ManagedChannel {
    plan: ChannelPlan,
    io: Box<dyn FanChannel>,
    phase: Phase,
    /// The saved BIOS registers while SysPilot holds the channel, or while a
    /// restore is pending. Set before the first write. Shared with the panic
    /// path, which restores from it without the controller.
    original: Original,
    faults: u32,
    good_readings: u32,
}

impl ManagedChannel {
    fn say(&self, out: &mut Vec<Event>, event: ChannelEvent) {
        out.push(Event { channel: self.plan.id.clone(), event });
    }

    fn problem(&self, out: &mut Vec<Event>, what: &str, error: impl fmt::Display) {
        self.say(
            out,
            ChannelEvent::Problem { detail: format!("{}: {what}: {error}", self.plan.id) },
        );
    }

    fn forget_progress(&mut self) {
        self.faults = 0;
        self.good_readings = 0;
        self.plan.follower.reset();
    }

    /// Whether the channel is, or may be, away from BIOS control.
    fn needs_release(&self) -> bool {
        self.original.get().is_some()
            || matches!(self.phase, Phase::Manual | Phase::FailSafe | Phase::ReleasePending)
    }

    /// Hands the channel back to the BIOS. A failure keeps the saved original
    /// and leaves the channel pending, to be retried.
    fn release(
        &mut self,
        store: &mut dyn OriginalStore,
        reason: ReleaseReason,
        out: &mut Vec<Event>,
    ) {
        let Some(original) = self.original.get() else {
            // Nothing was saved, so nothing was taken (a blocked channel was
            // never ours).
            if self.phase != Phase::Blocked {
                self.phase = Phase::Bios;
            }
            self.forget_progress();
            return;
        };
        let restored = self
            .io
            .restore(&original)
            .map_err(|error| ("restoring the BIOS registers", error.to_string()))
            .and_then(|()| {
                store
                    .clear(&self.plan.id)
                    .map_err(|error| ("clearing the stored original", error.to_string()))
            });
        match restored {
            Ok(()) => {
                self.original.set(None);
                self.phase = Phase::Bios;
                self.say(out, ChannelEvent::Released { reason });
            }
            Err((what, error)) => {
                self.phase = Phase::ReleasePending;
                self.problem(out, what, error);
            }
        }
        self.forget_progress();
    }

    /// Sends the fan to 100 %. When even that write fails the BIOS gets the
    /// channel back, which is the next best thing.
    fn fail_safe(&mut self, store: &mut dyn OriginalStore, reason: String, out: &mut Vec<Event>) {
        match self.io.set_duty(MAX_DUTY) {
            Ok(()) => {
                self.phase = Phase::FailSafe;
                self.good_readings = 0;
                self.say(out, ChannelEvent::FailSafe { reason });
            }
            Err(error) => {
                self.problem(out, "writing 100 % for the fail-safe", error);
                self.release(store, ReleaseReason::WriteFailed, out);
            }
        }
    }

    /// Saves the original, then takes manual control at the curve's duty.
    fn take_control(
        &mut self,
        store: &mut dyn OriginalStore,
        temperature: f64,
        out: &mut Vec<Event>,
    ) {
        // A stored original that is still there is the real BIOS mode: the
        // channel's registers may already hold a crashed predecessor's manual
        // mode, which must not be saved as the original.
        let original = match store.load(&self.plan.id) {
            Ok(Some(stored)) => stored,
            Ok(None) => {
                let read = match self.io.read_original() {
                    Ok(read) => read,
                    Err(error) => {
                        return self.problem(out, "reading the original registers", error);
                    }
                };
                if let Err(error) = store.save(&self.plan.id, &read) {
                    return self.problem(out, "saving the original registers", error);
                }
                read
            }
            Err(error) => return self.problem(out, "reading the stored original", error),
        };
        let duty = match self.plan.follower.step(temperature, 0.0) {
            Ok(duty) => duty,
            Err(error) => return self.problem(out, "following the curve", error),
        };
        // Kept before the write, so any path that fails or panics from here
        // can restore it.
        self.original.set(Some(original));
        match self.io.take_manual(duty) {
            Ok(()) => {
                self.phase = Phase::Manual;
                self.say(out, ChannelEvent::TookControl { duty });
            }
            Err(error) => {
                self.problem(out, "taking manual control", error);
                self.release(store, ReleaseReason::WriteFailed, out);
            }
        }
    }

    fn follow_curve(
        &mut self,
        store: &mut dyn OriginalStore,
        temperature: f64,
        elapsed_seconds: f64,
        out: &mut Vec<Event>,
    ) {
        match self.plan.follower.step(temperature, elapsed_seconds) {
            Ok(duty) => match self.io.set_duty(duty) {
                Ok(()) => self.say(out, ChannelEvent::DutySet { duty }),
                Err(error) => {
                    self.problem(out, "setting the duty", error);
                    self.release(store, ReleaseReason::WriteFailed, out);
                }
            },
            // Elapsed time the engine refuses: no way to know what to do, so
            // cool.
            Err(error) => self.fail_safe(store, error.to_string(), out),
        }
    }

    fn tick(
        &mut self,
        store: &mut dyn OriginalStore,
        policy: &FailSafePolicy,
        readings: &Readings,
        elapsed_seconds: f64,
        out: &mut Vec<Event>,
    ) {
        match self.phase {
            Phase::Blocked => return,
            Phase::ReleasePending => {
                // Nothing else until the BIOS has the channel back.
                self.release(store, ReleaseReason::Retry, out);
                return;
            }
            Phase::Bios | Phase::Manual | Phase::FailSafe => {}
        }
        if !self.plan.following {
            return;
        }
        // NaN is not a gap that can be trusted either.
        let reading = if elapsed_seconds.is_nan()
            || elapsed_seconds > policy.max_seconds_between_ticks
        {
            Err(format!(
                "(tick gap): expected at most {} s since the previous tick | received: {elapsed_seconds} s",
                policy.max_seconds_between_ticks
            ))
        } else {
            self.plan.source.resolve(readings).map_err(|fault| fault.to_string())
        };
        let temperature = match reading {
            Ok(temperature) => temperature,
            Err(reason) => {
                self.good_readings = 0;
                self.faults += 1;
                if self.phase == Phase::Manual && self.faults >= policy.faults_before_fail_safe {
                    self.fail_safe(store, reason, out);
                }
                return;
            }
        };
        self.faults = 0;
        self.good_readings = self.good_readings.saturating_add(1);
        match self.phase {
            Phase::Manual => self.follow_curve(store, temperature, elapsed_seconds, out),
            Phase::Bios if self.good_readings >= policy.good_readings_before_control => {
                self.plan.follower.reset();
                self.take_control(store, temperature, out);
            }
            Phase::FailSafe if self.good_readings >= policy.good_readings_before_control => {
                // Ramp down from where the fan is, at the channel's limit.
                self.plan.follower.restart_from(MAX_DUTY);
                self.phase = Phase::Manual;
                self.follow_curve(store, temperature, elapsed_seconds, out);
            }
            _ => {}
        }
    }

    /// Crash recovery of one channel.
    fn recover(&mut self, store: &mut dyn OriginalStore, out: &mut Vec<Event>) {
        let stored = match store.load(&self.plan.id) {
            Ok(stored) => stored,
            Err(error) => {
                self.problem(out, "reading the stored original", error);
                None
            }
        };
        let state = match self.io.read_state() {
            Ok(state) => Some(state),
            Err(error) => {
                self.problem(out, "reading the channel state", error);
                None
            }
        };
        // Whatever the original, a channel in manual mode never stays below
        // its minimum duty.
        if let Some(state) = state.filter(|state| state.manual) {
            let minimum = self.plan.minimum_duty();
            if state.duty_percent < minimum {
                match self.io.set_duty(minimum) {
                    Ok(()) => self.say(out, ChannelEvent::RaisedToMinimum { duty: minimum }),
                    Err(error) => self.problem(out, "raising to the minimum duty", error),
                }
            }
        }
        match (stored, state) {
            (Some(stored), _) => {
                self.original.set(Some(stored));
                self.release(store, ReleaseReason::Recovery, out);
            }
            (None, Some(state)) if state.manual => {
                self.phase = Phase::Blocked;
                self.say(
                    out,
                    ChannelEvent::Blocked {
                        reason: format!(
                            "manual mode at {:.0} % with no stored original; not recorded as the BIOS mode",
                            state.duty_percent
                        ),
                    },
                );
            }
            (None, _) => {}
        }
    }
}

// ---------------------------------------------------------------------------
// The controller
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Running,
    Suspended,
    Stopped,
}

/// Drives a set of fan channels and keeps them safe.
pub struct FanController {
    channels: Vec<ManagedChannel>,
    store: SharedStore,
    policy: FailSafePolicy,
    mode: Mode,
    panic: Arc<PanicRelease>,
}

impl FanController {
    /// A controller of `channels`. Every channel starts under BIOS control;
    /// call [`FanController::recover`] once before the first tick.
    pub fn new(
        policy: FailSafePolicy,
        store: Box<dyn OriginalStore>,
        channels: Vec<(ChannelPlan, Box<dyn FanChannel>)>,
    ) -> Result<Self, ControllerError> {
        policy.validate()?;
        let mut seen = BTreeSet::new();
        for (plan, _) in &channels {
            if !seen.insert(plan.id.clone()) {
                return Err(ControllerError {
                    expected: "each channel listed once".into(),
                    received: format!("`{}` twice", plan.id),
                });
            }
        }
        let store = SharedStore(Arc::new(Mutex::new(store)));
        let mut for_panic = Vec::new();
        let channels = channels
            .into_iter()
            .map(|(plan, io)| {
                let original = Original::default();
                for_panic.push(PanicChannel {
                    id: plan.id.clone(),
                    io: Mutex::new(io.duplicate()),
                    original: original.clone(),
                });
                ManagedChannel {
                    plan,
                    io,
                    phase: Phase::Bios,
                    original,
                    faults: 0,
                    good_readings: 0,
                }
            })
            .collect();
        let panic = Arc::new(PanicRelease { channels: for_panic, store: store.clone() });
        Ok(Self { channels, store, policy, mode: Mode::Running, panic })
    }

    /// The way back for a panic hook: restores every channel the controller
    /// holds without taking the controller's lock. See [`PanicRelease`].
    pub fn panic_release(&self) -> Arc<PanicRelease> {
        self.panic.clone()
    }

    /// Where a channel stands; `None` for an id the controller does not manage.
    pub fn phase(&self, channel: &str) -> Option<Phase> {
        self.channels.iter().find(|managed| managed.plan.id == channel).map(|managed| managed.phase)
    }

    /// Finds the channels the way a crashed predecessor may have left them and
    /// makes them safe: see rule 5 in the module documentation.
    pub fn recover(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        for channel in &mut self.channels {
            channel.recover(&mut self.store, &mut events);
        }
        events
    }

    /// One control step: `readings` is the sensor sample and `elapsed_seconds`
    /// the time since the previous tick. Does nothing while suspended or after
    /// a stop.
    pub fn tick(&mut self, readings: &Readings, elapsed_seconds: f64) -> Vec<Event> {
        let mut events = Vec::new();
        if self.mode != Mode::Running {
            return events;
        }
        for channel in &mut self.channels {
            channel.tick(&mut self.store, &self.policy, readings, elapsed_seconds, &mut events);
        }
        events
    }

    fn release_all(&mut self, reason: ReleaseReason) -> Vec<Event> {
        let mut events = Vec::new();
        for channel in &mut self.channels {
            if channel.needs_release() {
                channel.release(&mut self.store, reason, &mut events);
            }
        }
        events
    }

    /// Hands every channel back to the BIOS and stops controlling.
    pub fn stop(&mut self) -> Vec<Event> {
        self.mode = Mode::Stopped;
        self.release_all(ReleaseReason::Stop)
    }

    /// [`FanController::stop`] for a machine that is about to shut down.
    pub fn prepare_shutdown(&mut self) -> Vec<Event> {
        self.mode = Mode::Stopped;
        self.release_all(ReleaseReason::Shutdown)
    }

    /// Hands every channel back to the BIOS for a sleep. Ticks do nothing
    /// until [`FanController::resume`].
    pub fn suspend(&mut self) -> Vec<Event> {
        if self.mode == Mode::Running {
            self.mode = Mode::Suspended;
        }
        self.release_all(ReleaseReason::Suspend)
    }

    /// Ends a suspend. Control is retaken tick by tick, only once the
    /// sensors have read correctly for
    /// [`FailSafePolicy::good_readings_before_control`] ticks in a row.
    pub fn resume(&mut self) {
        if self.mode == Mode::Suspended {
            self.mode = Mode::Running;
            for channel in &mut self.channels {
                channel.forget_progress();
            }
        }
    }
}

impl Drop for FanController {
    fn drop(&mut self) {
        if self.mode != Mode::Stopped {
            self.release_all(ReleaseReason::Dropped);
        }
    }
}

// ---------------------------------------------------------------------------
// Panic path
// ---------------------------------------------------------------------------

/// How long the panic path waits for any lock or bus another thread is
/// using. Short: the process is going down.
const PANIC_LOCK_WAIT: Duration = Duration::from_millis(250);

/// One channel as the panic path sees it: its own handle on the output and
/// the saved registers it shares with the controller.
struct PanicChannel {
    id: String,
    /// Locked only by the panic path, which can run on two threads at once
    /// when two threads panic.
    io: Mutex<Box<dyn FanChannel>>,
    original: Original,
}

/// Hands every channel of one controller back to the BIOS from a panic hook,
/// **without going through the controller**.
///
/// A panic during `tick`, `suspend` or `stop` runs the hook on the same
/// thread, in the middle of the step: the controller is borrowed and a
/// register may be half written (with `panic = "abort"` the hook is all that
/// runs). A hook that reached the controller through a lock would wait on
/// the very lock the step holds across its register I/O, give up and leave
/// the channel in manual mode. So the hook gets what it needs from
/// the controller beforehand, in three pieces that each sit behind their own
/// lock, held only for an instant and never across register I/O: the
/// channel's saved registers ([`Original`], copied or replaced), the
/// channel's own duplicate handle on the bus ([`FanChannel::duplicate`]) and
/// the store ([`SharedStore`], locked per call). The controller writes the
/// saved registers before its first write to a channel, so whatever step
/// panicked, a channel that may be away from the BIOS has its registers
/// here.
///
/// Every wait is bounded by [`PANIC_LOCK_WAIT`]: a hook that blocks forever
/// would keep the process alive with the fans in manual mode, which is worse
/// than giving up. The one thing it cannot do is restore while the panicking
/// thread itself is inside a bus transaction (the bus lock is held by the
/// thread that is now running the hook); the saved registers stay in the
/// store then and the next start restores them.
pub struct PanicRelease {
    channels: Vec<PanicChannel>,
    store: SharedStore,
}

impl PanicRelease {
    /// Restores every channel that has saved registers and clears its
    /// record. Returns whether every such channel was restored. Never
    /// panics: a failure leaves the record for the next start.
    pub fn release(&self) -> bool {
        self.release_within(PANIC_LOCK_WAIT)
    }

    fn release_within(&self, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        let mut every_channel_restored = true;
        for channel in &self.channels {
            // Nothing saved means nothing taken, or already handed back.
            let Some(original) = channel.original.get() else { continue };
            let restored = locked_before(&channel.io, deadline)
                .is_some_and(|mut io| io.restore(&original).is_ok());
            if restored {
                channel.original.set(None);
                // Best effort: a record left behind restores the same
                // registers again at the next start.
                self.store.clear_before(&channel.id, deadline);
            } else {
                every_channel_restored = false;
            }
        }
        every_channel_restored
    }
}

/// The release the process-wide panic hook runs: that of the session that is
/// live now, or none. The slot is the only thing that holds a session's
/// [`PanicRelease`] (and so its bus handle) besides the session itself, so
/// replacing or clearing it is what lets a finished session's PawnIO handle
/// go.
static PANIC_SLOT: Mutex<Option<Arc<PanicRelease>>> = Mutex::new(None);

/// Makes `release` the one the panic hook runs, replacing (and dropping)
/// the previous one, and installs the hook the first time. There is one
/// hook per process however many sessions come and go: the hook reads the
/// slot when a panic happens, so it never holds a session itself.
/// In release builds (`panic = "abort"`) the hook is the only thing that
/// runs on a panic.
pub fn install_panic_release(release: Arc<PanicRelease>) {
    set_panic_slot(Some(release));
    static HOOK: Once = Once::new();
    HOOK.call_once(install_panic_hook);
}

/// Empties the slot if it still holds `release`: the session it belongs to
/// ended. A newer session's release stays.
pub fn remove_panic_release(release: &Arc<PanicRelease>) {
    let mut slot = locked(&PANIC_SLOT);
    if slot.as_ref().is_some_and(|installed| Arc::ptr_eq(installed, release)) {
        let removed = slot.take();
        // Dropped after the lock is free: dropping a release closes a bus.
        drop(slot);
        drop(removed);
    }
}

fn set_panic_slot(release: Option<Arc<PanicRelease>>) {
    let replaced = std::mem::replace(&mut *locked(&PANIC_SLOT), release);
    drop(replaced);
}

/// Tests that touch the process-wide slot or deliberately panic on a thread
/// (which runs the process-wide hook) hold this, so that one test's hook
/// never restores another test's fans.
#[cfg(test)]
pub(crate) fn serialize_panic_hook_tests() -> MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    locked(&SERIAL)
}

/// Wraps the current panic hook with one that runs the slot's release first.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Bounded like everything else on this path: the slot lock is only
        // held for a swap, but a hook must never be the thing that hangs.
        let installed = locked_before(&PANIC_SLOT, Instant::now() + PANIC_LOCK_WAIT)
            .and_then(|slot| slot.clone());
        if let Some(release) = installed {
            release.release();
        }
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::chips::nct67xx::Chip;
    use crate::fan::board::BoardCatalog;
    use crate::fan::curve::CurvePoint;

    // -- Fakes ----------------------------------------------------------

    /// Everything the fakes do, in order, so a test can assert what happened
    /// before what.
    #[derive(Clone, Default)]
    struct EventLog(Arc<Mutex<Vec<String>>>);

    impl EventLog {
        fn push(&self, entry: impl Into<String>) {
            self.0.lock().unwrap().push(entry.into());
        }

        fn entries(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }

        fn clear(&self) {
            self.0.lock().unwrap().clear();
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    enum FanOperation {
        ReadOriginal,
        TakeManual,
        SetDuty,
        Restore,
    }

    /// The mode and duty registers of one fake output.
    struct FakeRegisters {
        mode: u8,
        duty: u8,
        failing: BTreeSet<FanOperation>,
        /// The duty (percent) after every change while in manual mode.
        manual_duties: Vec<f64>,
    }

    const MODE_REGISTER: u16 = 0x102;
    const DUTY_REGISTER: u16 = 0x109;
    /// What the BIOS left: SmartFan IV with a tolerance of 5, duty 0x66.
    const BIOS_MODE: u8 = 0x45;
    const BIOS_DUTY: u8 = 0x66;

    /// A named fake of one fan output: a mode register and a duty register
    /// that behave like the NCT6775 family's (high nibble 0 is manual).
    #[derive(Clone)]
    struct FakeFanChannel {
        id: String,
        registers: Arc<Mutex<FakeRegisters>>,
        log: EventLog,
    }

    impl FakeFanChannel {
        fn new(id: &str, log: &EventLog) -> Self {
            Self {
                id: id.to_owned(),
                registers: Arc::new(Mutex::new(FakeRegisters {
                    mode: BIOS_MODE,
                    duty: BIOS_DUTY,
                    failing: BTreeSet::new(),
                    manual_duties: Vec::new(),
                })),
                log: log.clone(),
            }
        }

        fn registers(&self) -> (u8, u8) {
            let registers = self.registers.lock().unwrap();
            (registers.mode, registers.duty)
        }

        /// The duty in percent as the chip would report it.
        fn duty_percent(&self) -> f64 {
            f64::from(self.registers().1) / 2.55
        }

        fn is_manual(&self) -> bool {
            self.registers().0 & 0xF0 == 0
        }

        fn make_fail(&self, operation: FanOperation) {
            self.registers.lock().unwrap().failing.insert(operation);
        }

        fn make_work(&self, operation: FanOperation) {
            self.registers.lock().unwrap().failing.remove(&operation);
        }

        /// A crashed predecessor's state: manual mode at `percent`.
        fn leave_manual_at(&self, percent: f64) {
            let mut registers = self.registers.lock().unwrap();
            registers.mode &= 0x0F;
            registers.duty = (percent * 2.55).round() as u8;
        }

        fn manual_duties(&self) -> Vec<f64> {
            self.registers.lock().unwrap().manual_duties.clone()
        }

        fn check(&self, operation: FanOperation) -> Result<(), BusError> {
            if locked(&self.registers).failing.contains(&operation) {
                return Err(BusError::rejected(
                    format!("fake channel {}", self.id),
                    "the operation to work",
                    format!("an injected {operation:?} failure"),
                ));
            }
            Ok(())
        }

        fn write_duty(&self, percent: f64) {
            let mut registers = self.registers.lock().unwrap();
            registers.duty = (percent * 2.55).round() as u8;
            let manual = registers.mode & 0xF0 == 0;
            let duty = f64::from(registers.duty) / 2.55;
            if manual {
                registers.manual_duties.push(duty);
            }
        }
    }

    impl FanChannel for FakeFanChannel {
        fn read_original(&mut self) -> Result<ControlSnapshot, BusError> {
            self.log.push(format!("{}: read_original", self.id));
            self.check(FanOperation::ReadOriginal)?;
            let (mode, duty) = self.registers();
            Ok(ControlSnapshot { registers: vec![(MODE_REGISTER, mode), (DUTY_REGISTER, duty)] })
        }

        fn read_state(&mut self) -> Result<ChannelState, BusError> {
            Ok(ChannelState { manual: self.is_manual(), duty_percent: self.duty_percent() })
        }

        fn take_manual(&mut self, duty_percent: f64) -> Result<(), BusError> {
            self.log.push(format!("{}: take_manual({duty_percent:.0})", self.id));
            self.check(FanOperation::TakeManual)?;
            // The duty first, then the mode: as the real write path does.
            let mut registers = locked(&self.registers);
            registers.duty = (duty_percent * 2.55).round() as u8;
            registers.mode &= 0x0F;
            let duty = f64::from(registers.duty) / 2.55;
            registers.manual_duties.push(duty);
            Ok(())
        }

        fn set_duty(&mut self, duty_percent: f64) -> Result<(), BusError> {
            self.log.push(format!("{}: set_duty({duty_percent:.0})", self.id));
            self.check(FanOperation::SetDuty)?;
            if !self.is_manual() {
                return Err(BusError::rejected(
                    format!("fake channel {}", self.id),
                    "manual mode before a duty write",
                    format!("mode {:#04x}", self.registers().0),
                ));
            }
            self.write_duty(duty_percent);
            Ok(())
        }

        fn restore(&mut self, original: &ControlSnapshot) -> Result<(), BusError> {
            self.log.push(format!("{}: restore", self.id));
            self.check(FanOperation::Restore)?;
            let mut registers = locked(&self.registers);
            for &(address, value) in &original.registers {
                match address {
                    MODE_REGISTER => registers.mode = value,
                    DUTY_REGISTER => registers.duty = value,
                    other => {
                        panic!("(fake channel): expected a known register | received: {other:#x}")
                    }
                }
            }
            Ok(())
        }

        fn duplicate(&self) -> Box<dyn FanChannel> {
            Box::new(self.clone())
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    enum StoreOperation {
        Load,
        Save,
        Clear,
    }

    #[derive(Default)]
    struct StoreRecords {
        records: BTreeMap<String, ControlSnapshot>,
        failing: BTreeSet<StoreOperation>,
    }

    /// A named fake of the original store; clones share the records, which is
    /// how a "restarted" controller finds what its predecessor saved.
    #[derive(Clone)]
    struct FakeOriginalStore {
        state: Arc<Mutex<StoreRecords>>,
        log: EventLog,
    }

    impl FakeOriginalStore {
        fn new(log: &EventLog) -> Self {
            Self { state: Arc::default(), log: log.clone() }
        }

        fn record(&self, channel: &str) -> Option<ControlSnapshot> {
            self.state.lock().unwrap().records.get(channel).cloned()
        }

        fn make_fail(&self, operation: StoreOperation) {
            self.state.lock().unwrap().failing.insert(operation);
        }

        fn check(&self, operation: StoreOperation) -> Result<(), StoreError> {
            if locked(&self.state).failing.contains(&operation) {
                return Err(StoreError(format!("an injected {operation:?} failure")));
            }
            Ok(())
        }
    }

    impl OriginalStore for FakeOriginalStore {
        fn load(&mut self, channel: &str) -> Result<Option<ControlSnapshot>, StoreError> {
            self.check(StoreOperation::Load)?;
            Ok(self.record(channel))
        }

        fn save(&mut self, channel: &str, original: &ControlSnapshot) -> Result<(), StoreError> {
            self.log.push(format!("store.save {channel}"));
            self.check(StoreOperation::Save)?;
            locked(&self.state).records.insert(channel.to_owned(), original.clone());
            Ok(())
        }

        fn clear(&mut self, channel: &str) -> Result<(), StoreError> {
            self.log.push(format!("store.clear {channel}"));
            self.check(StoreOperation::Clear)?;
            locked(&self.state).records.remove(channel);
            Ok(())
        }

        fn channels(&mut self) -> Result<Vec<String>, StoreError> {
            self.check(StoreOperation::Load)?;
            Ok(locked(&self.state).records.keys().cloned().collect())
        }
    }

    // -- Rig ------------------------------------------------------------

    /// A validated board with three outputs: pwm1 and pwm2 with minimum duty
    /// 20 and 30, pwm3 that may stop.
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
label = "CPU fan"
min-duty = 20
may-stop = false

[[board.channel]]
pwm = 2
label = "Case fan"
min-duty = 30
may-stop = false

[[board.channel]]
pwm = 3
label = "Rear fan"
min-duty = 0
may-stop = true
"#;

    /// Duty follows the temperature from 30 to 60 °C, 100 % at 80 °C.
    fn curve() -> Curve {
        let point = |temperature, duty| CurvePoint { temperature, duty };
        Curve::new(vec![point(20.0, 0.0), point(30.0, 30.0), point(60.0, 60.0), point(80.0, 100.0)])
            .unwrap()
    }

    const NO_RAMP: Tuning =
        Tuning { hysteresis: 0.0, max_rise_per_second: None, max_fall_per_second: None };

    struct Rig {
        controller: FanController,
        log: EventLog,
        store: FakeOriginalStore,
        channels: Vec<FakeFanChannel>,
    }

    /// pwm1 follows `cpu`, pwm2 follows `gpu`, pwm3 follows the maximum of
    /// both.
    fn rig_with(policy: FailSafePolicy, tuning: Tuning) -> Rig {
        let log = EventLog::default();
        let store = FakeOriginalStore::new(&log);
        let channels: Vec<FakeFanChannel> =
            (1..=3).map(|number| FakeFanChannel::new(&format!("pwm{number}"), &log)).collect();
        let controller = controller_over(&store, &channels, policy, tuning);
        Rig { controller, log, store, channels }
    }

    fn controller_over(
        store: &FakeOriginalStore,
        channels: &[FakeFanChannel],
        policy: FailSafePolicy,
        tuning: Tuning,
    ) -> FanController {
        let catalog = BoardCatalog::parse("test", BOARD).unwrap();
        let grant = catalog.offer("ACME", "TEST BOARD", Chip::Nct6779D).unwrap();
        let sources = [
            TemperatureSource::single("cpu"),
            TemperatureSource::single("gpu"),
            TemperatureSource::maximum_of(vec!["cpu".into(), "gpu".into()]).unwrap(),
        ];
        let planned = channels
            .iter()
            .zip(sources)
            .enumerate()
            .map(|(index, (channel, source))| {
                let board_channel = grant.channel(index as u8 + 1).unwrap();
                let plan = ChannelPlan::new(board_channel, curve(), source, tuning).unwrap();
                (plan, Box::new(channel.clone()) as Box<dyn FanChannel>)
            })
            .collect();
        FanController::new(policy, Box::new(store.clone()), planned).unwrap()
    }

    fn rig() -> Rig {
        rig_with(FailSafePolicy::default(), NO_RAMP)
    }

    fn readings(cpu: Option<f64>, gpu: Option<f64>) -> Readings {
        [("cpu".to_owned(), cpu), ("gpu".to_owned(), gpu)].into_iter().collect()
    }

    fn good() -> Readings {
        readings(Some(40.0), Some(50.0))
    }

    /// Ticks `count` times, one second apart.
    fn tick(rig: &mut Rig, readings: &Readings, count: usize) -> Vec<Event> {
        (0..count).flat_map(|_| rig.controller.tick(readings, 1.0)).collect()
    }

    /// A rig whose three channels are under manual control.
    fn controlled() -> Rig {
        let mut rig = rig();
        tick(&mut rig, &good(), 3);
        assert!(rig.channels.iter().all(FakeFanChannel::is_manual), "setup: control taken");
        rig
    }

    fn all_bios(rig: &Rig) -> bool {
        rig.channels.iter().all(|channel| channel.registers() == (BIOS_MODE, BIOS_DUTY))
    }

    fn event(channel: &str, event: ChannelEvent) -> Event {
        Event { channel: channel.to_owned(), event }
    }

    fn problems(events: &[Event]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|event| match &event.event {
                ChannelEvent::Problem { detail } => Some(detail.as_str()),
                _ => None,
            })
            .collect()
    }

    // -- Rule 1: the original is saved before the first write -------------

    #[test]
    fn nothing_is_read_or_written_until_the_sensors_have_read_well_enough_times() {
        let mut rig = rig();
        assert!(tick(&mut rig, &good(), 2).is_empty());
        assert!(rig.log.entries().is_empty(), "{:?}", rig.log.entries());
        assert!(all_bios(&rig));
        assert_eq!(rig.controller.phase("pwm1"), Some(Phase::Bios));
    }

    #[test]
    fn the_original_registers_are_saved_before_the_first_write() {
        let mut rig = rig();
        let events = tick(&mut rig, &good(), 3);
        let entries = rig.log.entries();
        // pwm1 follows cpu = 40 °C: 40 %.
        assert_eq!(
            entries[..3],
            ["pwm1: read_original", "store.save pwm1", "pwm1: take_manual(40)"]
        );
        assert_eq!(
            rig.store.record("pwm1"),
            Some(ControlSnapshot {
                registers: vec![(MODE_REGISTER, BIOS_MODE), (DUTY_REGISTER, BIOS_DUTY)]
            })
        );
        assert_eq!(events[0], event("pwm1", ChannelEvent::TookControl { duty: 40.0 }));
        assert_eq!(rig.controller.phase("pwm1"), Some(Phase::Manual));
        // The mode register is manual now and the duty is 40 %.
        assert!(rig.channels[0].is_manual());
        assert!((rig.channels[0].duty_percent() - 40.0).abs() < 0.3);
    }

    #[test]
    fn a_channel_follows_its_curve_after_taking_control() {
        let mut rig = controlled();
        rig.controller.tick(&readings(Some(50.0), Some(30.0)), 1.0);
        assert!((rig.channels[0].duty_percent() - 50.0).abs() < 0.3, "cpu 50 °C -> 50 %");
        assert!(
            (rig.channels[1].duty_percent() - 30.0).abs() < 0.3,
            "gpu 30 °C -> 30 % (the channel minimum is 30)"
        );
    }

    #[test]
    fn a_curve_cannot_take_a_channel_below_its_board_minimum() {
        let mut rig = controlled();
        // 20 °C is 0 % on the curve; pwm1 needs 20, pwm2 30, pwm3 may stop.
        let cold = readings(Some(20.0), Some(20.0));
        rig.controller.tick(&cold, 1.0);
        assert!((rig.channels[0].duty_percent() - 20.0).abs() < 0.3);
        assert!((rig.channels[1].duty_percent() - 30.0).abs() < 0.3);
        assert_eq!(rig.channels[2].duty_percent(), 0.0);
    }

    // -- Rule 2: BIOS control comes back ---------------------------------

    #[test]
    fn stop_restores_the_bios_registers_and_clears_the_stored_original() {
        let mut rig = controlled();
        let events = rig.controller.stop();
        assert!(all_bios(&rig));
        assert_eq!(rig.store.record("pwm1"), None);
        assert_eq!(
            events[0],
            event("pwm1", ChannelEvent::Released { reason: ReleaseReason::Stop })
        );
        assert_eq!(rig.controller.phase("pwm1"), Some(Phase::Bios));
    }

    #[test]
    fn nothing_is_written_after_a_stop() {
        let mut rig = controlled();
        rig.controller.stop();
        rig.log.clear();
        assert!(tick(&mut rig, &good(), 5).is_empty());
        assert!(rig.log.entries().is_empty());
        assert!(all_bios(&rig));
    }

    #[test]
    fn shutdown_and_suspend_restore_the_bios_registers() {
        type Release = fn(&mut FanController) -> Vec<Event>;
        let cases: [(Release, ReleaseReason); 2] = [
            (FanController::prepare_shutdown, ReleaseReason::Shutdown),
            (FanController::suspend, ReleaseReason::Suspend),
        ];
        for (release, reason) in cases {
            let mut rig = controlled();
            let events = release(&mut rig.controller);
            assert!(all_bios(&rig), "{reason:?}");
            assert_eq!(rig.store.record("pwm1"), None, "{reason:?}");
            assert!(
                events.contains(&event("pwm1", ChannelEvent::Released { reason })),
                "{reason:?}"
            );
        }
    }

    #[test]
    fn dropping_the_controller_restores_the_bios_registers() {
        let rig = controlled();
        let Rig { controller, store, channels, .. } = rig;
        drop(controller);
        assert!(channels.iter().all(|channel| channel.registers() == (BIOS_MODE, BIOS_DUTY)));
        assert_eq!(store.record("pwm2"), None);
    }

    #[test]
    fn a_release_with_nothing_taken_writes_nothing() {
        let mut rig = rig();
        assert!(rig.controller.stop().is_empty());
        assert!(rig.log.entries().is_empty());
    }

    #[test]
    fn a_failed_restore_keeps_its_record_and_is_retried_before_anything_else() {
        let mut rig = controlled();
        rig.channels[0].make_fail(FanOperation::Restore);
        let events = rig.controller.suspend();
        assert_eq!(rig.controller.phase("pwm1"), Some(Phase::ReleasePending));
        assert!(problems(&events)[0].starts_with("pwm1: restoring the BIOS registers"));
        assert!(rig.store.record("pwm1").is_some(), "the original is kept for the retry");
        assert!(rig.channels[0].is_manual());
        assert_eq!(rig.controller.phase("pwm2"), Some(Phase::Bios), "other channels are released");

        // The retry happens on the next tick, once the fault is gone, and
        // that tick does not take control again.
        rig.channels[0].make_work(FanOperation::Restore);
        rig.controller.resume();
        let events = rig.controller.tick(&good(), 1.0);
        assert!(
            events
                .contains(&event("pwm1", ChannelEvent::Released { reason: ReleaseReason::Retry }))
        );
        assert_eq!(rig.channels[0].registers(), (BIOS_MODE, BIOS_DUTY));
        assert_eq!(rig.store.record("pwm1"), None);
        assert_eq!(rig.controller.phase("pwm1"), Some(Phase::Bios));
    }

    #[test]
    fn a_failed_store_clear_after_a_restore_is_retried_too() {
        let mut rig = controlled();
        rig.store.make_fail(StoreOperation::Clear);
        let events = rig.controller.stop();
        assert!(problems(&events)[0].starts_with("pwm1: clearing the stored original"));
        assert_eq!(rig.controller.phase("pwm1"), Some(Phase::ReleasePending));
        assert!(all_bios(&rig), "the registers are the BIOS ones already");
    }

    // -- Rule 3: a failing sensor drives its fans to 100 % ---------------

    #[test]
    fn every_kind_of_bad_reading_sends_the_channel_to_full_duty() {
        let bad: [(&str, Readings); 6] = [
            ("none", readings(None, Some(50.0))),
            ("missing", [("gpu".to_owned(), Some(50.0))].into_iter().collect()),
            ("NaN", readings(Some(f64::NAN), Some(50.0))),
            ("infinite", readings(Some(f64::INFINITY), Some(50.0))),
            ("below the plausible range", readings(Some(-21.0), Some(50.0))),
            ("above the plausible range", readings(Some(126.0), Some(50.0))),
        ];
        for (name, sample) in bad {
            let mut rig = controlled();
            let events = rig.controller.tick(&sample, 1.0);
            assert_eq!(rig.channels[0].registers().1, 255, "{name}: pwm1 at 100 %");
            assert_eq!(rig.controller.phase("pwm1"), Some(Phase::FailSafe), "{name}");
            let reason = events.iter().find_map(|event| match &event.event {
                ChannelEvent::FailSafe { reason } if event.channel == "pwm1" => {
                    Some(reason.clone())
                }
                _ => None,
            });
            assert!(reason.is_some_and(|reason| reason.contains("`cpu`")), "{name}: {events:?}");
        }
    }

    #[test]
    fn the_plausible_range_edges_are_not_a_failure() {
        let mut rig = controlled();
        rig.controller.tick(&readings(Some(-20.0), Some(125.0)), 1.0);
        assert_eq!(rig.controller.phase("pwm1"), Some(Phase::Manual));
        assert_eq!(rig.controller.phase("pwm2"), Some(Phase::Manual));
    }

    #[test]
    fn only_the_channels_that_use_the_failing_sensor_go_to_full_duty() {
        let mut rig = controlled();
        rig.controller.tick(&readings(None, Some(50.0)), 1.0);
        assert_eq!(rig.controller.phase("pwm1"), Some(Phase::FailSafe), "cpu only");
        assert_eq!(rig.controller.phase("pwm3"), Some(Phase::FailSafe), "the maximum includes cpu");
        assert_eq!(rig.controller.phase("pwm2"), Some(Phase::Manual), "gpu only: unaffected");
        assert!((rig.channels[1].duty_percent() - 50.0).abs() < 0.3, "pwm2 keeps following gpu");
        assert_eq!(rig.channels[2].registers().1, 255);
    }

    #[test]
    fn a_failing_sensor_inside_a_maximum_is_not_ignored() {
        let mut rig = controlled();
        // gpu fails, cpu is cool: a maximum that skipped gpu would say 40 °C.
        rig.controller.tick(&readings(Some(40.0), None), 1.0);
        assert_eq!(rig.controller.phase("pwm3"), Some(Phase::FailSafe));
        assert_eq!(rig.controller.phase("pwm1"), Some(Phase::Manual));
    }

    #[test]
    fn the_fail_safe_threshold_is_counted_in_consecutive_failed_ticks() {
        let policy = FailSafePolicy { faults_before_fail_safe: 2, ..FailSafePolicy::default() };
        let mut rig = rig_with(policy, NO_RAMP);
        tick(&mut rig, &good(), 3);
        let before = rig.channels[0].registers();

        rig.controller.tick(&readings(None, Some(50.0)), 1.0);
        assert_eq!(rig.controller.phase("pwm1"), Some(Phase::Manual), "one failure holds");
        assert_eq!(rig.channels[0].registers(), before, "the duty is held, not changed");

        rig.controller.tick(&good(), 1.0);
        rig.controller.tick(&readings(None, Some(50.0)), 1.0);
        assert_eq!(
            rig.controller.phase("pwm1"),
            Some(Phase::Manual),
            "a good tick in between restarts the count"
        );

        rig.controller.tick(&readings(None, Some(50.0)), 1.0);
        assert_eq!(rig.controller.phase("pwm1"), Some(Phase::FailSafe), "two in a row trip it");
        assert_eq!(rig.channels[0].registers().1, 255);
    }

    #[test]
    fn a_long_gap_between_ticks_counts_as_a_failure() {
        let mut rig = controlled();
        let events = rig.controller.tick(&good(), 30.0);
        assert_eq!(rig.controller.phase("pwm1"), Some(Phase::FailSafe));
        assert!(
            events.iter().any(|event| matches!(
                &event.event,
                ChannelEvent::FailSafe { reason } if reason.contains("expected at most 15 s")
                    && reason.contains("received: 30 s")
            )),
            "{events:?}"
        );
    }

    #[test]
    fn a_nonsense_elapsed_time_counts_as_a_failure_too() {
        let mut rig = controlled();
        rig.controller.tick(&good(), f64::NAN);
        assert_eq!(rig.controller.phase("pwm1"), Some(Phase::FailSafe));
    }

    #[test]
    fn full_duty_ignores_the_rise_limit() {
        let tuning = Tuning { max_rise_per_second: Some(2.0), ..NO_RAMP };
        let mut rig = rig_with(FailSafePolicy::default(), tuning);
        tick(&mut rig, &good(), 3);
        rig.controller.tick(&readings(None, Some(50.0)), 1.0);
        assert_eq!(rig.channels[0].registers().1, 255, "straight to 100 %, not 2 %/s");
    }

    #[test]
    fn a_channel_in_fail_safe_stays_at_full_duty_while_the_sensor_stays_bad() {
        let mut rig = controlled();
        tick(&mut rig, &readings(None, Some(50.0)), 5);
        assert_eq!(rig.channels[0].registers().1, 255);
        assert_eq!(rig.controller.phase("pwm1"), Some(Phase::FailSafe));
    }

    #[test]
    fn a_bad_sensor_before_control_is_taken_leaves_the_bios_in_charge() {
        let mut rig = rig();
        let events = tick(&mut rig, &readings(None, None), 10);
        assert!(events.is_empty());
        assert!(all_bios(&rig));
        assert!(rig.log.entries().is_empty());
    }

    #[test]
    fn a_failed_full_duty_write_hands_the_channel_back_to_the_bios() {
        let mut rig = controlled();
        rig.channels[0].make_fail(FanOperation::SetDuty);
        let events = rig.controller.tick(&readings(None, Some(50.0)), 1.0);
        assert!(problems(&events)[0].starts_with("pwm1: writing 100 % for the fail-safe"));
        assert!(events.contains(&event(
            "pwm1",
            ChannelEvent::Released { reason: ReleaseReason::WriteFailed }
        )));
        assert_eq!(rig.channels[0].registers(), (BIOS_MODE, BIOS_DUTY));
        assert_eq!(rig.controller.phase("pwm1"), Some(Phase::Bios));
    }

    // -- Rule 4: control is retaken only after good readings -------------

    #[test]
    fn control_resumes_the_curve_only_after_enough_good_readings() {
        let tuning = Tuning { max_fall_per_second: Some(10.0), ..NO_RAMP };
        let mut rig = rig_with(FailSafePolicy::default(), tuning);
        tick(&mut rig, &good(), 3);
        rig.controller.tick(&readings(None, Some(50.0)), 1.0);
        assert_eq!(rig.channels[0].registers().1, 255);

        // Two good ticks are not enough: the fan stays at 100 %.
        tick(&mut rig, &good(), 2);
        assert_eq!(rig.controller.phase("pwm1"), Some(Phase::FailSafe));
        assert_eq!(rig.channels[0].registers().1, 255);

        // A bad tick in between restarts the count.
        rig.controller.tick(&readings(None, Some(50.0)), 1.0);
        tick(&mut rig, &good(), 2);
        assert_eq!(rig.controller.phase("pwm1"), Some(Phase::FailSafe));

        // The third good tick resumes, ramping down from 100 % at 10 %/s
        // towards the curve's 40 %.
        let events = rig.controller.tick(&good(), 1.0);
        assert_eq!(rig.controller.phase("pwm1"), Some(Phase::Manual));
        assert!(
            events.contains(&event("pwm1", ChannelEvent::DutySet { duty: 90.0 })),
            "{events:?}"
        );
    }

    #[test]
    fn after_a_resume_control_is_not_retaken_while_sensors_still_fail() {
        let mut rig = controlled();
        rig.controller.suspend();
        assert!(all_bios(&rig));
        assert!(tick(&mut rig, &good(), 5).is_empty(), "suspended: ticks do nothing");

        rig.controller.resume();
        rig.log.clear();
        tick(&mut rig, &readings(None, None), 10);
        assert!(rig.log.entries().is_empty(), "no write while the sensors fail");
        assert!(all_bios(&rig));

        // Two good ticks, then a failure: still not enough.
        tick(&mut rig, &good(), 2);
        tick(&mut rig, &readings(None, None), 1);
        tick(&mut rig, &good(), 2);
        assert!(all_bios(&rig));

        let events = tick(&mut rig, &good(), 1);
        assert!(events.contains(&event("pwm1", ChannelEvent::TookControl { duty: 40.0 })));
        assert!(rig.channels[0].is_manual());
    }

    #[test]
    fn a_resume_without_a_suspend_changes_nothing() {
        let mut rig = controlled();
        rig.controller.resume();
        assert_eq!(rig.controller.phase("pwm1"), Some(Phase::Manual));
    }

    #[test]
    fn a_stop_is_final_and_a_resume_does_not_undo_it() {
        let mut rig = controlled();
        rig.controller.stop();
        rig.controller.resume();
        assert!(tick(&mut rig, &good(), 5).is_empty());
        assert!(all_bios(&rig));
    }

    // -- Rule 5: a restart finds the fans safe ---------------------------

    /// A controller that took control and then vanished without releasing:
    /// the fake chip stays in manual mode and the store keeps the record.
    fn crashed_while_controlling() -> (FakeOriginalStore, Vec<FakeFanChannel>, EventLog) {
        let rig = controlled();
        let Rig { controller, store, channels, log } = rig;
        std::mem::forget(controller);
        (store, channels, log)
    }

    #[test]
    fn a_restart_hands_a_crashed_predecessors_channels_back_to_the_bios() {
        let (store, channels, log) = crashed_while_controlling();
        assert!(channels.iter().all(FakeFanChannel::is_manual), "the crash left manual mode");

        let mut restarted = controller_over(&store, &channels, FailSafePolicy::default(), NO_RAMP);
        log.clear();
        let events = restarted.recover();
        assert!(channels.iter().all(|c| c.registers() == (BIOS_MODE, BIOS_DUTY)));
        assert_eq!(store.record("pwm1"), None);
        assert!(
            events.contains(&event(
                "pwm1",
                ChannelEvent::Released { reason: ReleaseReason::Recovery }
            ))
        );
        // The registers that came back are the real BIOS ones, saved by the
        // predecessor, not the manual state the chip was in.
        assert_eq!(restarted.phase("pwm1"), Some(Phase::Bios));
    }

    #[test]
    fn a_restart_never_leaves_a_fan_below_its_minimum_duty_on_the_way() {
        let (store, channels, log) = crashed_while_controlling();
        // The crash left pwm1 at 5 %, below its minimum of 20.
        channels[0].leave_manual_at(5.0);
        let mut restarted = controller_over(&store, &channels, FailSafePolicy::default(), NO_RAMP);
        log.clear();
        let events = restarted.recover();

        assert!(events.contains(&event("pwm1", ChannelEvent::RaisedToMinimum { duty: 20.0 })));
        let entries = log.entries();
        let raise = entries.iter().position(|entry| entry == "pwm1: set_duty(20)").unwrap();
        let restore = entries.iter().position(|entry| entry == "pwm1: restore").unwrap();
        assert!(raise < restore, "raised first, then handed back: {entries:?}");
        // Every duty observed in manual mode after the crash is at least 20 %.
        let after_crash: Vec<f64> = channels[0].manual_duties().into_iter().skip(1).collect();
        assert!(after_crash.iter().all(|&duty| duty >= 19.7), "{after_crash:?}");
    }

    #[test]
    fn a_restart_without_a_stored_original_never_records_manual_mode_as_the_original() {
        let log = EventLog::default();
        let store = FakeOriginalStore::new(&log);
        let channels = vec![
            FakeFanChannel::new("pwm1", &log),
            FakeFanChannel::new("pwm2", &log),
            FakeFanChannel::new("pwm3", &log),
        ];
        // Manual mode at 10 % with no record: the crashed program's, or another
        // tool's. pwm1's minimum is 20.
        channels[0].leave_manual_at(10.0);
        let mut restarted = controller_over(&store, &channels, FailSafePolicy::default(), NO_RAMP);
        let events = restarted.recover();

        assert!(events.contains(&event("pwm1", ChannelEvent::RaisedToMinimum { duty: 20.0 })));
        assert!(matches!(
            &events.iter().find(|e| matches!(e.event, ChannelEvent::Blocked { .. })).unwrap().event,
            ChannelEvent::Blocked { reason } if reason.contains("no stored original")
        ));
        assert_eq!(restarted.phase("pwm1"), Some(Phase::Blocked));
        assert!(channels[0].duty_percent() >= 19.7, "held at least at the minimum");
        assert_eq!(channels[1].registers(), (BIOS_MODE, BIOS_DUTY), "BIOS channels are untouched");

        // Good sensors for a long time: still never controlled, never saved.
        for _ in 0..20 {
            restarted.tick(&good(), 1.0);
        }
        assert_eq!(restarted.phase("pwm1"), Some(Phase::Blocked));
        assert_eq!(store.record("pwm1"), None, "manual mode was never saved as an original");
        assert!(!log.entries().iter().any(|entry| entry == "pwm1: read_original"));
        // A stop leaves the blocked channel as found.
        restarted.stop();
        assert!(channels[0].is_manual());
    }

    #[test]
    fn a_manual_channel_at_or_above_its_minimum_is_left_at_its_duty() {
        let log = EventLog::default();
        let store = FakeOriginalStore::new(&log);
        let channels = [FakeFanChannel::new("pwm1", &log)];
        channels[0].leave_manual_at(60.0);
        let catalog = BoardCatalog::parse("test", BOARD).unwrap();
        let grant = catalog.offer("ACME", "TEST BOARD", Chip::Nct6779D).unwrap();
        let plan = ChannelPlan::new(
            grant.channel(1).unwrap(),
            curve(),
            TemperatureSource::single("cpu"),
            NO_RAMP,
        )
        .unwrap();
        let mut restarted = FanController::new(
            FailSafePolicy::default(),
            Box::new(store),
            vec![(plan, Box::new(channels[0].clone()) as Box<dyn FanChannel>)],
        )
        .unwrap();
        let events = restarted.recover();
        assert!(!events.iter().any(|e| matches!(e.event, ChannelEvent::RaisedToMinimum { .. })));
        assert!((channels[0].duty_percent() - 60.0).abs() < 0.3);
        assert_eq!(restarted.phase("pwm1"), Some(Phase::Blocked));
    }

    #[test]
    fn a_controller_that_skips_recovery_still_uses_the_stored_original() {
        let (store, channels, log) = crashed_while_controlling();
        let mut restarted = controller_over(&store, &channels, FailSafePolicy::default(), NO_RAMP);
        log.clear();
        for _ in 0..3 {
            restarted.tick(&good(), 1.0);
        }
        let entries = log.entries();
        assert!(!entries.contains(&"pwm1: read_original".to_owned()), "{entries:?}");
        assert!(!entries.contains(&"store.save pwm1".to_owned()), "{entries:?}");
        restarted.stop();
        assert!(
            channels.iter().all(|c| c.registers() == (BIOS_MODE, BIOS_DUTY)),
            "the BIOS registers come back, not the crashed manual state"
        );
    }

    #[test]
    fn a_channel_that_was_never_taken_over_is_untouched_by_recovery() {
        let mut rig = rig();
        assert!(rig.controller.recover().is_empty());
        assert!(rig.log.entries().is_empty());
        assert!(all_bios(&rig));
    }

    /// A controller whose only channel is `pwm` of the board, restore-only:
    /// the shape of a channel the plan has no curve for.
    fn restore_only_controller(
        store: &FakeOriginalStore,
        channel: &FakeFanChannel,
        pwm: u8,
    ) -> FanController {
        let catalog = BoardCatalog::parse("test", BOARD).unwrap();
        let grant = catalog.offer("ACME", "TEST BOARD", Chip::Nct6779D).unwrap();
        let plan = ChannelPlan::restore_only(grant.channel(pwm).unwrap()).unwrap();
        FanController::new(
            FailSafePolicy::default(),
            Box::new(store.clone()),
            vec![(plan, Box::new(channel.clone()) as Box<dyn FanChannel>)],
        )
        .unwrap()
    }

    #[test]
    fn a_restore_only_channel_is_handed_back_by_recovery_and_never_taken_over() {
        let (store, channels, log) = crashed_while_controlling();
        let mut restarted = restore_only_controller(&store, &channels[1], 2);
        log.clear();
        let events = restarted.recover();

        assert_eq!(
            channels[1].registers(),
            (BIOS_MODE, BIOS_DUTY),
            "(recovery): expected pwm2 back with the BIOS | received a channel still in manual mode"
        );
        assert_eq!(store.record("pwm2"), None, "the record is cleared once restored");
        assert!(store.record("pwm1").is_some(), "a channel nobody visits keeps its record");
        assert!(
            events.contains(&event(
                "pwm2",
                ChannelEvent::Released { reason: ReleaseReason::Recovery }
            )),
            "{events:?}"
        );
        assert_eq!(restarted.phase("pwm2"), Some(Phase::Bios));

        // Good sensors for a long time: the channel is never read for an
        // original, never written and never taken.
        log.clear();
        for _ in 0..10 {
            restarted.tick(&good(), 1.0);
        }
        assert!(
            log.entries().is_empty(),
            "(restore-only channel): expected no I/O | {:?}",
            log.entries()
        );
        assert_eq!(restarted.phase("pwm2"), Some(Phase::Bios));
    }

    #[test]
    fn a_restore_only_channel_that_cannot_be_restored_stays_pending_and_is_retried_every_tick() {
        let (store, channels, _log) = crashed_while_controlling();
        channels[0].make_fail(FanOperation::Restore);
        let mut restarted = restore_only_controller(&store, &channels[0], 1);
        let events = restarted.recover();
        assert!(problems(&events)[0].starts_with("pwm1: restoring the BIOS registers"));
        assert_eq!(
            restarted.phase("pwm1"),
            Some(Phase::ReleasePending),
            "a channel still in manual mode is not reported as the BIOS's"
        );
        assert!(store.record("pwm1").is_some(), "the record is kept for the retry");
        assert!(channels[0].is_manual());

        channels[0].make_work(FanOperation::Restore);
        let events = restarted.tick(&good(), 1.0);
        assert!(
            events
                .contains(&event("pwm1", ChannelEvent::Released { reason: ReleaseReason::Retry })),
            "{events:?}"
        );
        assert_eq!(channels[0].registers(), (BIOS_MODE, BIOS_DUTY));
        assert_eq!(store.record("pwm1"), None);
    }

    // -- Failures while taking control -----------------------------------

    #[test]
    fn a_store_that_cannot_save_prevents_any_write() {
        let mut rig = rig();
        rig.store.make_fail(StoreOperation::Save);
        let events = tick(&mut rig, &good(), 3);
        assert!(problems(&events)[0].starts_with("pwm1: saving the original registers"));
        assert!(!rig.log.entries().iter().any(|entry| entry.contains("take_manual")));
        assert!(all_bios(&rig));
        assert_eq!(rig.controller.phase("pwm1"), Some(Phase::Bios));
    }

    #[test]
    fn a_store_that_cannot_load_prevents_any_write() {
        let mut rig = rig();
        rig.store.make_fail(StoreOperation::Load);
        let events = tick(&mut rig, &good(), 3);
        assert!(problems(&events)[0].starts_with("pwm1: reading the stored original"));
        assert!(rig.log.entries().is_empty());
    }

    #[test]
    fn an_unreadable_channel_prevents_any_write() {
        let mut rig = rig();
        rig.channels[0].make_fail(FanOperation::ReadOriginal);
        let events = tick(&mut rig, &good(), 3);
        assert!(problems(&events)[0].starts_with("pwm1: reading the original registers"));
        assert_eq!(rig.store.record("pwm1"), None);
        assert_eq!(rig.channels[0].registers(), (BIOS_MODE, BIOS_DUTY));
        assert_eq!(rig.controller.phase("pwm2"), Some(Phase::Manual), "other channels carry on");
    }

    #[test]
    fn a_failed_takeover_restores_what_may_have_been_written() {
        let mut rig = rig();
        rig.channels[0].make_fail(FanOperation::TakeManual);
        let events = tick(&mut rig, &good(), 3);
        assert!(problems(&events)[0].starts_with("pwm1: taking manual control"));
        let entries = rig.log.entries();
        let take = entries.iter().position(|entry| entry.starts_with("pwm1: take_manual")).unwrap();
        let restore = entries.iter().position(|entry| entry == "pwm1: restore").unwrap();
        assert!(take < restore, "{entries:?}");
        assert_eq!(rig.store.record("pwm1"), None);
        assert_eq!(rig.controller.phase("pwm1"), Some(Phase::Bios));
    }

    #[test]
    fn a_failed_duty_write_hands_the_channel_back_to_the_bios() {
        let mut rig = controlled();
        rig.channels[0].make_fail(FanOperation::SetDuty);
        let events = rig.controller.tick(&good(), 1.0);
        assert!(problems(&events)[0].starts_with("pwm1: setting the duty"));
        assert_eq!(rig.channels[0].registers(), (BIOS_MODE, BIOS_DUTY));
        assert_eq!(rig.controller.phase("pwm1"), Some(Phase::Bios));
        assert_eq!(rig.controller.phase("pwm2"), Some(Phase::Manual), "others carry on");
    }

    // -- Construction ----------------------------------------------------

    #[test]
    fn the_policy_refuses_values_that_would_disable_the_fail_safe() {
        let cases = [
            (
                FailSafePolicy { faults_before_fail_safe: 0, ..FailSafePolicy::default() },
                "faults-before-fail-safe of 1 or more | received: 0",
            ),
            (
                FailSafePolicy { good_readings_before_control: 0, ..FailSafePolicy::default() },
                "good-readings-before-control of 1 or more | received: 0",
            ),
            (
                FailSafePolicy { max_seconds_between_ticks: 0.0, ..FailSafePolicy::default() },
                "max-seconds-between-ticks above 0 | received: 0",
            ),
            (
                FailSafePolicy { max_seconds_between_ticks: f64::NAN, ..FailSafePolicy::default() },
                "max-seconds-between-ticks above 0 | received: NaN",
            ),
        ];
        for (policy, detail) in cases {
            let log = EventLog::default();
            let error = FanController::new(policy, Box::new(FakeOriginalStore::new(&log)), vec![])
                .err()
                .expect("the policy should be refused");
            assert_eq!(error.to_string(), format!("(fan controller): expected {detail}"));
        }
    }

    #[test]
    fn the_default_policy_reacts_to_the_first_failure_and_waits_for_three_good_readings() {
        let policy = FailSafePolicy::default();
        assert_eq!(policy.faults_before_fail_safe, 1);
        assert_eq!(policy.good_readings_before_control, 3);
        assert_eq!(policy.max_seconds_between_ticks, 15.0);
    }

    #[test]
    fn a_channel_listed_twice_is_refused() {
        let log = EventLog::default();
        let store = FakeOriginalStore::new(&log);
        let channel = FakeFanChannel::new("pwm1", &log);
        let catalog = BoardCatalog::parse("test", BOARD).unwrap();
        let grant = catalog.offer("ACME", "TEST BOARD", Chip::Nct6779D).unwrap();
        let plans = (0..2)
            .map(|_| {
                let plan = ChannelPlan::new(
                    grant.channel(1).unwrap(),
                    curve(),
                    TemperatureSource::single("cpu"),
                    NO_RAMP,
                )
                .unwrap();
                (plan, Box::new(channel.clone()) as Box<dyn FanChannel>)
            })
            .collect();
        let error = FanController::new(FailSafePolicy::default(), Box::new(store), plans)
            .err()
            .expect("a repeated channel should be refused");
        assert_eq!(
            error.to_string(),
            "(fan controller): expected each channel listed once | received: `pwm1` twice"
        );
    }

    #[test]
    fn a_plan_takes_its_minimum_duty_and_id_from_the_board_channel() {
        let catalog = BoardCatalog::parse("test", BOARD).unwrap();
        let grant = catalog.offer("ACME", "TEST BOARD", Chip::Nct6779D).unwrap();
        let plan = ChannelPlan::new(
            grant.channel(2).unwrap(),
            curve(),
            TemperatureSource::single("gpu"),
            NO_RAMP,
        )
        .unwrap();
        assert_eq!(plan.id(), "pwm2");
        assert_eq!(plan.minimum_duty(), 30.0);
        let refused = ChannelPlan::new(
            grant.channel(2).unwrap(),
            curve(),
            TemperatureSource::single("gpu"),
            Tuning { hysteresis: -1.0, ..NO_RAMP },
        );
        assert!(refused.is_err());
    }

    #[test]
    fn an_unknown_channel_has_no_phase() {
        assert_eq!(rig().controller.phase("pwm9"), None);
    }

    #[test]
    fn the_store_error_names_what_failed() {
        assert_eq!(
            StoreError("disk full".into()).to_string(),
            "(original store): expected the record to be read or written | received: disk full"
        );
    }

    // -- Panic path ------------------------------------------------------

    #[test]
    fn the_panic_path_restores_while_the_controller_lock_is_held() {
        // A process that shares the controller behind a lock holds it across
        // a control step's register I/O, and a panic inside the step runs the
        // hook on that thread, lock still held. (The old hook gave up after
        // 250 ms here and left the fans in manual mode.)
        let rig = controlled();
        let Rig { controller, channels, store, .. } = rig;
        let release = controller.panic_release();
        let shared = Arc::new(Mutex::new(controller));
        let step_in_progress = shared.lock().unwrap();

        let started = Instant::now();
        assert!(
            release.release(),
            "(panic path): expected every channel restored with the controller locked"
        );
        assert!(
            started.elapsed() < PANIC_LOCK_WAIT,
            "(panic path): expected no wait for the controller lock | received: {:?}",
            started.elapsed()
        );
        let registers: Vec<_> = channels.iter().map(FakeFanChannel::registers).collect();
        assert_eq!(
            registers,
            vec![(BIOS_MODE, BIOS_DUTY); 3],
            "(panic path): expected every channel back with the BIOS"
        );
        assert_eq!(store.record("pwm1"), None, "the record is cleared");
        drop(step_in_progress);
    }

    #[test]
    fn the_panic_path_restores_even_when_the_controller_lock_is_poisoned() {
        // Panics on a thread, which runs the process-wide hook.
        let _serial = serialize_panic_hook_tests();
        let rig = controlled();
        let Rig { controller, channels, store, .. } = rig;
        let release = controller.panic_release();
        let shared = Arc::new(Mutex::new(controller));
        let poisoner = shared.clone();
        let result = std::thread::spawn(move || {
            let _held = poisoner.lock().unwrap();
            panic!("injected panic while the controller is locked");
        })
        .join();
        assert!(result.is_err() && shared.is_poisoned(), "setup: the mutex is poisoned");

        assert!(release.release());
        assert!(channels.iter().all(|c| c.registers() == (BIOS_MODE, BIOS_DUTY)));
        assert_eq!(store.record("pwm1"), None);
    }

    #[test]
    fn the_panic_path_with_nothing_taken_writes_nothing() {
        let rig = rig();
        assert!(rig.controller.panic_release().release());
        assert!(rig.log.entries().is_empty(), "{:?}", rig.log.entries());
    }

    #[test]
    fn a_controller_still_alive_after_a_panic_release_does_not_fight_the_bios() {
        // Unwinding builds (and tests) survive a panic on another thread; the
        // controller then still believes it holds the channels.
        let mut rig = controlled();
        assert!(rig.controller.panic_release().release());
        let events = rig.controller.tick(&good(), 1.0);
        assert!(
            problems(&events)[0].starts_with("pwm1: setting the duty"),
            "(controller after a panic release): expected the refused duty write reported | {events:?}"
        );
        assert!(all_bios(&rig), "the BIOS registers are never overwritten with manual ones");
        assert_eq!(rig.controller.phase("pwm1"), Some(Phase::Bios));
        // Nothing is left to restore, so a stop writes nothing more.
        rig.log.clear();
        rig.controller.stop();
        assert!(rig.log.entries().is_empty(), "{:?}", rig.log.entries());
    }

    #[test]
    fn the_panic_path_releases_a_channel_that_panicked_halfway_through_taking_control() {
        // The original is kept before the write, so a release restores a
        // channel whose phase never got to Manual.
        let log = EventLog::default();
        let store = FakeOriginalStore::new(&log);
        let channels = [FakeFanChannel::new("pwm1", &log)];
        let catalog = BoardCatalog::parse("test", BOARD).unwrap();
        let grant = catalog.offer("ACME", "TEST BOARD", Chip::Nct6779D).unwrap();
        let plan = ChannelPlan::new(
            grant.channel(1).unwrap(),
            curve(),
            TemperatureSource::single("cpu"),
            NO_RAMP,
        )
        .unwrap();
        let controller = FanController::new(
            FailSafePolicy::default(),
            Box::new(store.clone()),
            vec![(plan, Box::new(channels[0].clone()) as Box<dyn FanChannel>)],
        )
        .unwrap();
        // The state a panic inside take_manual leaves: original saved and held,
        // registers already manual, phase still Bios.
        let original = channels[0].clone().read_original().unwrap();
        controller.channels[0].original.set(Some(original));
        channels[0].leave_manual_at(10.0);
        assert!(controller.panic_release().release());
        assert_eq!(channels[0].registers(), (BIOS_MODE, BIOS_DUTY));
    }

    #[test]
    fn the_panic_path_gives_up_instead_of_waiting_forever_on_a_channel_it_cannot_reach() {
        let rig = controlled();
        let release = rig.controller.panic_release();
        // Another panic's hook (or a stuck one) is using the channel's handle.
        let busy = release.channels[0].io.lock().unwrap();
        let started = Instant::now();
        assert!(
            !release.release_within(Duration::from_millis(30)),
            "(panic path): expected a refusal to wait forever on a held handle"
        );
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        assert!(rig.channels[0].is_manual(), "the unreachable channel is untouched");
        assert!(rig.store.record("pwm1").is_some(), "its record stays for the next start");
        assert_eq!(rig.channels[1].registers(), (BIOS_MODE, BIOS_DUTY), "the others are restored");
        drop(busy);

        assert!(release.release(), "a later attempt restores it");
        assert!(all_bios(&rig));
    }

    #[test]
    fn a_failed_panic_restore_keeps_the_record_and_never_panics() {
        let rig = controlled();
        rig.channels[0].make_fail(FanOperation::Restore);
        assert!(!rig.controller.panic_release().release());
        assert!(rig.channels[0].is_manual());
        assert!(rig.store.record("pwm1").is_some(), "the next start restores it");
        assert_eq!(rig.channels[1].registers(), (BIOS_MODE, BIOS_DUTY), "the others are restored");
    }

    #[test]
    fn an_installed_panic_hook_restores_the_fans_during_a_control_step_and_chains_on() {
        let _serial = serialize_panic_hook_tests();
        let rig = controlled();
        let Rig { controller, channels, store, .. } = rig;
        let release = controller.panic_release();
        let shared = Arc::new(Mutex::new(controller));

        // The hook is put on top of a witness directly, not through the
        // once-only installer, so this test does not depend on whether
        // another test installed the process's hook first.
        let chained = Arc::new(Mutex::new(false));
        let witness = chained.clone();
        let original_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |_| *witness.lock().unwrap() = true));
        install_panic_hook();
        set_panic_slot(Some(release));
        // The panic of issue 10: inside a step, with the controller locked.
        let step = shared.clone();
        let result = std::thread::spawn(move || {
            let _controller_locked = step.lock().unwrap();
            panic!("injected panic during a control step");
        })
        .join();
        std::panic::set_hook(original_hook);
        set_panic_slot(None);

        assert!(result.is_err());
        let registers: Vec<_> = channels.iter().map(FakeFanChannel::registers).collect();
        assert_eq!(
            registers,
            vec![(BIOS_MODE, BIOS_DUTY); 3],
            "(panic hook): expected the fans restored although the step held the controller"
        );
        assert_eq!(store.record("pwm1"), None);
        assert!(*chained.lock().unwrap(), "the previous hook still ran");
    }

    #[test]
    fn installing_a_release_drops_the_previous_one_and_removing_it_empties_the_slot() {
        let _serial = serialize_panic_hook_tests();
        // A session's release outlives its controller only through the slot.
        let first = {
            let Rig { controller, .. } = controlled();
            let release = controller.panic_release();
            let weak = Arc::downgrade(&release);
            install_panic_release(release);
            weak
        };
        assert!(first.upgrade().is_some(), "the slot holds the live session's release");

        let second = {
            let Rig { controller, .. } = controlled();
            let release = controller.panic_release();
            let weak = Arc::downgrade(&release);
            install_panic_release(release);
            weak
        };
        assert!(
            first.upgrade().is_none(),
            "(panic slot): expected the replaced session's release (and its bus handle) dropped"
        );

        // The end of an older session, whose entry was replaced, must not
        // empty the newer session's slot.
        let older = controlled().controller.panic_release();
        remove_panic_release(&older);
        assert!(
            second.upgrade().is_some(),
            "(panic slot): expected another session's removal to leave this one installed"
        );

        // The controller of `second` is gone; only the slot holds its release,
        // and removing it by identity empties the slot.
        let held = locked(&PANIC_SLOT).clone().expect("the second release is installed");
        remove_panic_release(&held);
        drop(held);
        drop(older);
        assert!(
            second.upgrade().is_none(),
            "(panic slot): expected the removed session's release dropped"
        );
        assert!(locked(&PANIC_SLOT).is_none());
    }

    #[test]
    fn a_panic_with_an_empty_slot_only_chains_to_the_previous_hook() {
        let _serial = serialize_panic_hook_tests();
        set_panic_slot(None);
        let chained = Arc::new(Mutex::new(false));
        let witness = chained.clone();
        let original_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |_| *witness.lock().unwrap() = true));
        install_panic_hook();
        let result = std::thread::spawn(|| panic!("injected panic with no session")).join();
        std::panic::set_hook(original_hook);
        assert!(result.is_err());
        assert!(*chained.lock().unwrap(), "the previous hook ran");
    }
}
