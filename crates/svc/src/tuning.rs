//! Hardware tuning as the service runs it.
//!
//! The profile holds a setting per device (see [`syspilot_proto::tuning`]).
//! The service decides *when* to look at the devices and when to apply a
//! setting; *how* is the tuner's business, a separate executable that runs
//! for the length of one job. The service loads no vendor library.
//!
//! A driver forgets tuning values when the machine restarts or the driver
//! does, and the vendor's own tool may write its values at logon. So a
//! setting is not written once: after a *trigger* (service start, a change of
//! the settings, wake, logon, "apply again") the devices are looked at a few
//! times over three minutes, and a device that does not hold its setting gets
//! it applied again. After three applies in one such round something else
//! keeps changing the values, and the device is left *contested* until the
//! next trigger. With nothing in effect and no client connected, nothing is
//! scheduled at all.
//!
//! Values that make the machine stop must not be applied again at the next
//! start. A mark is therefore kept on disk from just before an apply until
//! the machine is seen alive a minute later, or the service stops in an
//! orderly way. A service that finds the mark at start holds every setting
//! back until the user changes it or asks for it again.
//!
//! [`Tuning`] is the state machine, free of processes and files so that it
//! runs against [`FakeTuner`] and [`FakeApplyMark`] in tests. [`ProcessTuner`]
//! and [`FileApplyMark`] are the real things.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::os::windows::io::AsRawHandle;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use syspilot_proto::frame::{read_frame, write_frame};
use syspilot_proto::tuning::{
    TUNER_EXECUTABLE, TuneJob, TuneReport, TunedDevice, TuningDevice, TuningSetting, TuningState,
    TuningStatus, check_values,
};

use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
use windows_sys::Win32::System::Threading::WaitForSingleObject;

use crate::events::{Event, EventQueue};
use crate::worker::spawn_sibling;

/// When the devices are looked at after a trigger, counted from it. The
/// later looks catch a vendor tool that writes its own values after logon.
const LOOKS_AFTER_TRIGGER: [Duration; 3] =
    [Duration::ZERO, Duration::from_secs(45), Duration::from_secs(180)];
/// How long after an apply the devices are looked at again. A machine that
/// is still there then has survived the values.
const SURVIVAL_PERIOD: Duration = Duration::from_secs(60);
/// Applies in one round before a device counts as contested.
const MAX_APPLIES: u8 = 3;
/// How old a report may be before a status request has it made again, while
/// a client is connected.
const REFRESH_AFTER: Duration = Duration::from_secs(10);
/// How long the tuner gets for one job, in milliseconds. A driver call
/// normally takes a few; one that takes this long hangs.
const JOB_TIMEOUT_MS: u32 = 15_000;
/// Name of the mark file in the state directory.
const APPLY_MARK_FILE: &str = "tuning-applying";

/// What a finished job reported, or why it failed.
pub type Outcome = Result<TuneReport, String>;

/// Runs tuning jobs.
pub trait Tuner {
    /// Starts `job` without waiting for it. When it ends, an
    /// [`Event::TuningFinished`] reaches the event loop.
    fn start(&mut self, job: TuneJob);
}

/// The mark that says "values were applied and the machine has not been
/// seen alive since". It must survive the service, which is its point.
pub trait ApplyMark {
    /// Whether the mark is there.
    fn is_set(&self) -> bool;
    /// Puts the mark there.
    fn set(&mut self);
    /// Takes the mark away.
    fn clear(&mut self);
}

/// The mark as a file in the service's state directory, which only
/// LocalSystem and administrators can write.
pub struct FileApplyMark(PathBuf);

impl FileApplyMark {
    /// The mark in `state_directory`.
    pub fn in_directory(state_directory: PathBuf) -> Self {
        Self(state_directory.join(APPLY_MARK_FILE))
    }
}

impl ApplyMark for FileApplyMark {
    fn is_set(&self) -> bool {
        self.0.is_file()
    }

    fn set(&mut self) {
        // A mark that cannot be written only loses the protection it gives;
        // it is no reason to refuse the user's values.
        let _ = std::fs::write(&self.0, b"");
    }

    fn clear(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Runs the tuner executable that sits next to the service's (see
/// [`spawn_sibling`]).
pub struct ProcessTuner {
    queue: Arc<EventQueue>,
}

impl ProcessTuner {
    /// A tuner that reports to `queue`.
    pub fn new(queue: Arc<EventQueue>) -> Self {
        Self { queue }
    }
}

/// Runs one job in the tuner process and waits for its report, at most
/// [`JOB_TIMEOUT_MS`].
fn run_tuner_process(job: &TuneJob) -> Outcome {
    let (mut process, mut input, mut output) = spawn_sibling(TUNER_EXECUTABLE)
        .map_err(|error| format!("The tuner could not be started: {error}"))?;
    // Written from a thread of its own, so that a tuner that never takes a
    // job larger than the pipe's buffer still ends at the timeout: ending it
    // fails the write. Dropping the input after the job is what tells the
    // tuner that nothing more comes.
    let job = job.clone();
    let writer = std::thread::spawn(move || {
        let sent = write_frame(&mut input, &job).and_then(|()| Ok(input.flush()?));
        drop(input);
        sent.map_err(|error| error.to_string())
    });
    // The report is read while the tuner runs, so that one larger than the
    // pipe's buffer does not block it. The read ends when the tuner closes
    // its output, which ending the tuner does.
    let reader = std::thread::spawn(move || read_frame::<_, TuneReport>(&mut output));
    // SAFETY: the handle is the child's, valid until `process` is dropped.
    let waited = unsafe { WaitForSingleObject(process.as_raw_handle(), JOB_TIMEOUT_MS) };
    let in_time = waited == WAIT_OBJECT_0;
    if !in_time {
        // A tuner that is still there may be stuck in a driver call.
        let _ = process.kill();
    }
    let _ = process.wait();
    let sent = writer.join().unwrap_or_else(|_| Err("the writing thread panicked".to_owned()));
    let report = reader.join();
    match (sent, in_time, report) {
        (Err(error), ..) => Err(format!("(tuner): expected to take its job | received: {error}")),
        (Ok(()), false, _) => Err(format!(
            "(tuner): expected its report within {} s | received: nothing",
            JOB_TIMEOUT_MS / 1000
        )),
        (Ok(()), true, Ok(Ok(report))) => Ok(report),
        (Ok(()), true, Ok(Err(error))) => {
            Err(format!("(tuner): expected its report | received: {error}"))
        }
        (Ok(()), true, Err(_)) => {
            Err("(tuner): expected its report | received: nothing".to_owned())
        }
    }
}

impl Tuner for ProcessTuner {
    fn start(&mut self, job: TuneJob) {
        let queue = self.queue.clone();
        // The event loop must not wait for a driver.
        std::thread::spawn(move || queue.push(Event::TuningFinished(run_tuner_process(&job))));
    }
}

/// The job that puts every device with a setting in effect back to its
/// driver's defaults, for when SysPilot is removed. `None` when no setting
/// is in effect, so that a machine that never tuned anything starts no tuner.
pub fn reset_job(settings: &[TuningSetting]) -> Option<TuneJob> {
    let reset: Vec<String> = settings
        .iter()
        .filter(|setting| setting.enabled)
        .map(|setting| setting.device.clone())
        .collect();
    (!reset.is_empty()).then(|| TuneJob { apply: Vec::new(), reset })
}

/// Runs `job` and waits for it; for the command line, which has no event
/// loop. Returns the first problem a device or the tuner reported.
pub fn run_and_wait(job: &TuneJob) -> Result<(), String> {
    first_problem(run_tuner_process(job)?)
}

/// The first problem a device in `report` names, with the device's name.
fn first_problem(report: TuneReport) -> Result<(), String> {
    match report.devices.into_iter().find_map(|device| Some((device.name, device.problem?))) {
        Some((name, problem)) => Err(format!("{name}: {problem}")),
        None => Ok(()),
    }
}

/// Whether `device` holds something else than `setting` asks for: a listed
/// value, or the default of a control the setting does not list.
fn differs(device: &TunedDevice, setting: &TuningSetting) -> bool {
    let unknown = setting.values.keys().any(|key| device.controls.iter().all(|c| &c.key != key));
    unknown
        || device
            .controls
            .iter()
            .any(|control| control.current != control.value_in(&setting.values))
}

/// Why `settings` cannot be taken, judged against the devices in `report`:
/// a value outside its range or for a control the device lacks. Devices the
/// report does not list are not judged.
fn refusal(report: &TuneReport, settings: &[TuningSetting]) -> Option<String> {
    settings.iter().find_map(|setting| {
        let device = report.devices.iter().find(|device| device.key == setting.device)?;
        let problem = check_values(&device.controls, &setting.values).err()?;
        Some(format!("{}: {problem}", device.name))
    })
}

/// A job under way and the round it started in.
#[derive(Default)]
struct Running {
    job: TuneJob,
    /// [`Tuning::round`] when the job started.
    round: u64,
}

/// When and whether tuning settings are applied.
pub struct Tuning {
    tuner: Box<dyn Tuner>,
    mark: Box<dyn ApplyMark>,
    /// What the profile asks for.
    settings: Vec<TuningSetting>,
    /// The last report and when it was made.
    report: Option<(TuneReport, Instant)>,
    /// Why the last job failed, if it did.
    problem: Option<String>,
    /// The job under way. One at a time.
    running: Option<Running>,
    /// Counts the triggers. A trigger clears `failed` so that devices are
    /// tried again, so a job that began before it must not put its failure
    /// back: that would skip the settings sent to recover from it.
    round: u64,
    /// When the devices are looked at next, soonest first.
    looks: BTreeSet<Instant>,
    /// Applies per device since the last trigger.
    applies: BTreeMap<String, u8>,
    /// Devices whose values something keeps changing. Nothing is applied to
    /// them until the next trigger; the other devices are not affected.
    contested: BTreeSet<String>,
    /// The mark was found at start; nothing is applied until the user
    /// changes the settings or asks for them again.
    held: bool,
    /// When values were last applied, until the machine is seen alive
    /// [`SURVIVAL_PERIOD`] later.
    applied_at: Option<Instant>,
    /// Devices a setting was applied to since the service started. One of
    /// them whose setting is no longer in effect is put back to its defaults.
    managed: BTreeSet<String>,
    /// Devices whose last apply did not work, with the reason. They are not
    /// tried again until the next trigger.
    failed: BTreeMap<String, String>,
}

impl Tuning {
    /// The state at service start. With a setting in effect the devices are
    /// looked at as soon as the loop runs, unless the mark of an apply the
    /// machine did not survive is found.
    pub fn new(
        tuner: Box<dyn Tuner>,
        mark: Box<dyn ApplyMark>,
        settings: Vec<TuningSetting>,
        now: Instant,
    ) -> Self {
        let mut tuning = Self {
            tuner,
            mark,
            settings,
            report: None,
            problem: None,
            running: None,
            round: 0,
            looks: BTreeSet::new(),
            applies: BTreeMap::new(),
            contested: BTreeSet::new(),
            held: false,
            applied_at: None,
            managed: BTreeSet::new(),
            failed: BTreeMap::new(),
        };
        if !tuning.anything_in_effect() {
            // A mark with no setting left to hold back means nothing.
            tuning.mark.clear();
        } else if tuning.mark.is_set() {
            tuning.held = true;
        } else {
            tuning.trigger(now);
        }
        tuning
    }

    fn anything_in_effect(&self) -> bool {
        self.settings.iter().any(|setting| setting.enabled)
    }

    fn setting_in_effect(&self, device: &str) -> Option<&TuningSetting> {
        self.settings.iter().find(|setting| setting.enabled && setting.device == device)
    }

    /// Whether any job has ended, so that there is something to say: every
    /// job that ends leaves either a report or a problem.
    fn looked(&self) -> bool {
        self.report.is_some() || self.problem.is_some()
    }

    fn look_at(&mut self, at: Instant) {
        self.looks.insert(at);
    }

    /// Starts a new round: the devices are looked at now and twice more.
    fn trigger(&mut self, now: Instant) {
        self.round += 1;
        self.applies.clear();
        self.contested.clear();
        self.failed.clear();
        for delay in LOOKS_AFTER_TRIGGER {
            self.look_at(now + delay);
        }
    }

    /// A trigger from outside: only worth a round when something is, or
    /// was, in effect and is not held back.
    fn trigger_if_wanted(&mut self, now: Instant) {
        if !self.held && (self.anything_in_effect() || !self.managed.is_empty()) {
            self.trigger(now);
        }
    }

    /// Why `settings` cannot be taken, judged against what is known about
    /// the devices. `None` when nothing is known to be wrong.
    pub fn refusal(&self, settings: &[TuningSetting]) -> Option<String> {
        self.report.as_ref().and_then(|(report, _)| refusal(report, settings))
    }

    /// Takes new settings from the profile and starts a round for them. The
    /// user has spoken: what was held back or contested is tried again.
    pub fn set_settings(&mut self, settings: Vec<TuningSetting>, now: Instant) {
        self.settings = settings;
        self.release_hold();
        if self.anything_in_effect() || !self.managed.is_empty() {
            self.trigger(now);
        } else if self.looked() {
            // Nothing to apply, but what clients are shown changed.
            self.look_at(now);
        }
    }

    /// Lifts the hold a start after an abnormal stop put on the settings.
    /// Without a hold the mark is left alone: it then belongs to values
    /// applied moments ago, which the machine has yet to survive.
    fn release_hold(&mut self) {
        if std::mem::take(&mut self.held) {
            self.mark.clear();
        }
    }

    /// "Apply again": a new round, also for what was held back or contested.
    pub fn apply_again(&mut self, now: Instant) {
        self.release_hold();
        self.trigger(now);
    }

    /// The machine woke up: drivers may have put their defaults back.
    pub fn resumed(&mut self, now: Instant) {
        self.trigger_if_wanted(now);
    }

    /// A user logged on: a vendor tool may be about to write its values.
    pub fn logged_on(&mut self, now: Instant) {
        self.trigger_if_wanted(now);
    }

    /// The service stops in an orderly way: whatever was applied, the machine
    /// survived it as far as this service can tell.
    pub fn stopping(&mut self) {
        // An apply under way may still be writing once the service is gone:
        // the machine has yet to survive its values.
        let applying = self.running.as_ref().is_some_and(|running| !running.job.apply.is_empty());
        if !self.held && !applying {
            self.mark.clear();
        }
    }

    /// Whether a status request has to wait for the first look at the
    /// devices. Asking makes that look due.
    pub fn first_look_pending(&mut self, now: Instant) -> bool {
        if self.looked() {
            return false;
        }
        if self.running.is_none() && self.looks.first().is_none_or(|due| *due > now) {
            self.look_at(now);
        }
        true
    }

    /// Has the devices looked at again when what is known about them is old
    /// and somebody is watching.
    pub fn refresh_if_stale(&mut self, now: Instant, clients: usize) {
        let stale =
            self.report.as_ref().is_none_or(|(_, at)| now.duration_since(*at) > REFRESH_AFTER);
        if clients > 0 && stale && self.looked() && self.running.is_none() {
            self.look_at(now);
        }
    }

    fn start(&mut self, job: TuneJob) {
        // Stamped as the job goes out: an apply decided by a look that ended
        // after a trigger belongs to the round that trigger began.
        self.running = Some(Running { job: job.clone(), round: self.round });
        self.tuner.start(job);
    }

    /// Starts a look when one is due and no job is under way.
    pub fn run_due(&mut self, now: Instant) {
        if self.running.is_some() || self.looks.first().is_none_or(|due| *due > now) {
            return;
        }
        self.looks.retain(|due| *due > now);
        self.start(TuneJob::default());
    }

    /// How long until [`Tuning::run_due`] has something to do. While a job
    /// runs its end wakes the loop.
    pub fn next_wake(&self, now: Instant) -> Option<Duration> {
        let due = self.looks.first().filter(|_| self.running.is_none())?;
        Some(due.saturating_duration_since(now))
    }

    /// A job ended.
    pub fn finished(&mut self, outcome: Outcome, now: Instant) {
        let Running { job, round } = self.running.take().unwrap_or_default();
        // A job from before the latest trigger still says what it did to the
        // devices, but its failure is not held against the round after it.
        let current = round == self.round;
        let report = match outcome {
            Ok(report) => report,
            Err(reason) => {
                // What an apply did is unknown; it is not tried again by
                // itself, and the next look says where the devices stand.
                for setting in &job.apply {
                    if current {
                        self.failed.insert(setting.device.clone(), reason.clone());
                    }
                    // Values may have been written: turning the setting off
                    // must still put the defaults back.
                    self.managed.insert(setting.device.clone());
                }
                if !job.apply.is_empty() {
                    // Values may have been written before it failed: the
                    // mark stays until the machine is seen alive with them.
                    self.applied_at = Some(now);
                    self.look_at(now + SURVIVAL_PERIOD);
                }
                self.problem = Some(reason);
                return;
            }
        };
        self.problem = None;
        if job.is_look() {
            self.after_look(&report, now);
        } else {
            self.after_change(&job, &report, current, now);
        }
        self.report = Some((report, now));
    }

    /// Records what an apply or a reset did and has the result looked at.
    /// `current` is whether the job began in the round that is now running;
    /// only then is a device's problem held against it.
    fn after_change(&mut self, job: &TuneJob, report: &TuneReport, current: bool, now: Instant) {
        let problem_of = |key: &str| {
            report.devices.iter().find(|device| device.key == key).and_then(|d| d.problem.clone())
        };
        for key in &job.reset {
            // A reset that failed is tried again by the next look.
            if problem_of(key).is_none() {
                self.managed.remove(key);
            }
        }
        for setting in &job.apply {
            // Also when the apply failed: a Radeon's core, memory, fan and
            // power values are written one after another, so some may be on
            // the card. Turning the setting off must still reset it.
            self.managed.insert(setting.device.clone());
            if let Some(problem) = problem_of(&setting.device)
                && current
            {
                self.failed.insert(setting.device.clone(), problem);
            }
        }
        if !job.apply.is_empty() {
            self.applied_at = Some(now);
            self.look_at(now + SURVIVAL_PERIOD);
        }
    }

    /// Compares the devices with the settings and starts what is needed.
    fn after_look(&mut self, report: &TuneReport, now: Instant) {
        if self.applied_at.is_some_and(|at| now.duration_since(at) >= SURVIVAL_PERIOD) {
            self.applied_at = None;
            self.mark.clear();
        }
        let present = |key: &str| report.devices.iter().find(|device| device.key == key);
        let reset: Vec<String> = self
            .managed
            .iter()
            .filter(|key| self.setting_in_effect(key).is_none() && present(key).is_some())
            .cloned()
            .collect();
        let mut apply: Vec<TuningSetting> = Vec::new();
        let mut holding: Vec<String> = Vec::new();
        let mut given_up: Vec<String> = Vec::new();
        if !self.held {
            for setting in self.settings.iter().filter(|setting| setting.enabled) {
                let Some(device) = present(&setting.device) else { continue };
                let key = &setting.device;
                if !differs(device, setting) {
                    holding.push(key.clone());
                } else if self.failed.contains_key(key) || self.contested.contains(key) {
                    // Not tried again until the next trigger.
                } else if self.applies.get(key).is_some_and(|&count| count >= MAX_APPLIES) {
                    given_up.push(key.clone());
                } else {
                    apply.push(setting.clone());
                }
            }
        }
        // A device found holding its setting is ours too, also when this
        // service did not write the values: an earlier run of it did, before
        // a restart of the service the driver did not notice.
        self.managed.extend(holding);
        self.contested.extend(given_up);
        if apply.is_empty() && reset.is_empty() {
            return;
        }
        if !apply.is_empty() {
            for setting in &apply {
                *self.applies.entry(setting.device.clone()).or_default() += 1;
            }
            // Before the write: if the machine stops on these values, the
            // mark is what the next start finds.
            self.mark.set();
        }
        self.start(TuneJob { apply, reset });
    }

    /// Where one device stands.
    fn state_of(&self, device: &TunedDevice) -> TuningState {
        let Some(setting) = self.setting_in_effect(&device.key) else {
            return TuningState::Unmanaged;
        };
        if self.held {
            return TuningState::Held;
        }
        if let Some(reason) = self.failed.get(&device.key) {
            return TuningState::Failed { reason: reason.to_owned() };
        }
        match (differs(device, setting), self.contested.contains(&device.key)) {
            (false, _) => TuningState::Applied,
            (true, true) => TuningState::Contested,
            (true, false) => TuningState::Different,
        }
    }

    /// What clients are told.
    pub fn status(&self) -> TuningStatus {
        let Some((report, _)) = &self.report else {
            return TuningStatus { devices: Vec::new(), problem: self.problem.clone() };
        };
        let setting_of = |key: &str| self.settings.iter().find(|setting| setting.device == key);
        let mut devices: Vec<TuningDevice> = report
            .devices
            .iter()
            .map(|device| {
                let state = match (&device.problem, self.setting_in_effect(&device.key)) {
                    // A device that cannot even be read.
                    (Some(reason), Some(_)) if device.controls.is_empty() => {
                        TuningState::Failed { reason: reason.clone() }
                    }
                    _ => self.state_of(device),
                };
                TuningDevice::reported(device, setting_of(&device.key), state)
            })
            .collect();
        // A setting for a device that is not there has to show somewhere.
        let absent = self
            .settings
            .iter()
            .filter(|setting| report.devices.iter().all(|device| device.key != setting.device));
        devices.extend(absent.map(TuningDevice::missing));
        TuningStatus { devices, problem: self.problem.clone() }
    }
}

#[cfg(test)]
pub use fake::{FakeApplyMark, FakeTuner};

#[cfg(test)]
mod fake {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, MutexGuard, PoisonError};
    use syspilot_hw::tuning::{FakeTuningBackend, TuningBackend};

    /// What the fake tuner holds: the devices, the jobs it was given and,
    /// when it answers by itself, where the answer goes.
    #[derive(Default)]
    pub struct FakeTunerState {
        /// The devices, as a backend in memory.
        pub backend: FakeTuningBackend,
        /// Every job started, oldest first.
        pub jobs: Vec<TuneJob>,
        /// When set, the next job fails with this instead of being run.
        pub failure: Option<String>,
        queue: Option<Arc<EventQueue>>,
    }

    /// A tuner over devices in memory. On its own it only records jobs and
    /// the test plays the outcome back with [`FakeTuner::outcome_of_last`];
    /// given a queue, it answers each job through it as the real one does.
    #[derive(Clone, Default)]
    pub struct FakeTuner(Arc<Mutex<FakeTunerState>>);

    impl FakeTuner {
        /// A tuner that answers through `queue`.
        pub fn answering(queue: Arc<EventQueue>) -> Self {
            let tuner = Self::default();
            tuner.state().queue = Some(queue);
            tuner
        }

        /// The devices and the record of jobs.
        pub fn state(&self) -> MutexGuard<'_, FakeTunerState> {
            self.0.lock().unwrap_or_else(PoisonError::into_inner)
        }

        /// Every job started, oldest first.
        pub fn jobs(&self) -> Vec<TuneJob> {
            self.state().jobs.clone()
        }

        fn run(state: &mut FakeTunerState, job: &TuneJob) -> Outcome {
            if let Some(failure) = state.failure.take() {
                return Err(failure);
            }
            let backend: Box<dyn TuningBackend + '_> = Box::new(&mut state.backend);
            Ok(syspilot_hw::tuning::run(job, &mut [backend]))
        }

        /// Carries out the last job against the devices and returns what the
        /// real tuner would report.
        pub fn outcome_of_last(&self) -> Outcome {
            let mut state = self.state();
            let job = state.jobs.last().cloned().unwrap_or_default();
            Self::run(&mut state, &job)
        }
    }

    impl Tuner for FakeTuner {
        fn start(&mut self, job: TuneJob) {
            let mut state = self.state();
            state.jobs.push(job.clone());
            if let Some(queue) = state.queue.clone() {
                let outcome = Self::run(&mut state, &job);
                queue.push(Event::TuningFinished(outcome));
            }
        }
    }

    /// A mark in memory.
    #[derive(Clone, Default)]
    pub struct FakeApplyMark(Arc<AtomicBool>);

    impl FakeApplyMark {
        /// A mark that is already there, as after a machine that stopped.
        pub fn left_behind() -> Self {
            Self(Arc::new(AtomicBool::new(true)))
        }
    }

    impl ApplyMark for FakeApplyMark {
        fn is_set(&self) -> bool {
            self.0.load(Ordering::Relaxed)
        }

        fn set(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }

        fn clear(&mut self) {
            self.0.store(false, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syspilot_proto::tuning::{TuningControl, TuningKind, TuningUnit};

    const CARD: &str = "gpu.amd-699f.bus-3";

    fn control(key: &str, min: i32, max: i32, default: i32) -> TuningControl {
        TuningControl {
            key: key.into(),
            kind: TuningKind::PowerLimit,
            state: None,
            unit: TuningUnit::Percent,
            min,
            max,
            step: 1,
            default,
            current: default,
            wanted: None,
        }
    }

    fn card() -> TunedDevice {
        TunedDevice {
            key: CARD.into(),
            name: "Radeon RX 550 Series".into(),
            interface: "Fake".into(),
            controls: vec![
                control("power-limit", -75, 75, 0),
                control("core-p7-voltage", 700, 1075, 1050),
            ],
            problem: None,
        }
    }

    fn setting(values: &[(&str, i32)]) -> TuningSetting {
        TuningSetting {
            device: CARD.into(),
            enabled: true,
            values: values.iter().map(|(key, value)| ((*key).to_owned(), *value)).collect(),
        }
    }

    /// Asserts whether `mark` is there. `when` says at which point of the
    /// test, so that a failure reads without the source at hand.
    #[track_caller]
    fn assert_mark_is(mark: &dyn ApplyMark, expected: bool, when: &str) {
        let word = |set: bool| if set { "set" } else { "cleared" };
        let received = mark.is_set();
        assert!(
            received == expected,
            "(mark {when}): expected {} | received: {}",
            word(expected),
            word(received)
        );
    }

    struct Fixture {
        tuner: FakeTuner,
        mark: FakeApplyMark,
        tuning: Tuning,
        now: Instant,
    }

    impl Fixture {
        fn new(settings: Vec<TuningSetting>) -> Self {
            Self::with_mark(settings, FakeApplyMark::default())
        }

        fn with_mark(settings: Vec<TuningSetting>, mark: FakeApplyMark) -> Self {
            let (tuner, now) = (FakeTuner::default(), Instant::now());
            tuner.state().backend.devices = vec![card()];
            let tuning =
                Tuning::new(Box::new(tuner.clone()), Box::new(mark.clone()), settings, now);
            Self { tuner, mark, tuning, now }
        }

        /// Lets `seconds` pass, then runs what is due and answers every job
        /// that starts, until nothing is under way.
        fn pass(&mut self, seconds: u64) {
            self.now += Duration::from_secs(seconds);
            self.tuning.run_due(self.now);
            self.settle();
        }

        fn settle(&mut self) {
            while self.tuning.running.is_some() {
                let outcome = self.tuner.outcome_of_last();
                self.tuning.finished(outcome, self.now);
            }
        }

        fn device(&self) -> TuningDevice {
            let status = self.tuning.status();
            status.devices.into_iter().find(|device| device.key == CARD).unwrap_or_else(|| {
                panic!("(tuning status): expected the device {CARD} | received: none")
            })
        }

        #[track_caller]
        fn assert_mark(&self, expected: bool, when: &str) {
            assert_mark_is(&self.mark, expected, when);
        }

        #[track_caller]
        fn assert_state(&self, expected: &TuningState, when: &str) {
            let state = self.device().state;
            assert!(
                state == *expected,
                "(state {when}): expected {expected:?} | received: {state:?}"
            );
        }

        /// Whether the service counts the device as one it applied a setting
        /// to, which is what turning the setting off later resets.
        fn manages_device(&self) -> bool {
            self.tuning.managed.contains(CARD)
        }

        fn current(&self, key: &str) -> i32 {
            let device = self.device();
            let found = device.controls.iter().find(|control| control.key == key);
            match found {
                Some(control) => control.current,
                None => {
                    let keys: Vec<_> = device.controls.iter().map(|c| c.key.as_str()).collect();
                    panic!("(tuning status): expected a control {key} | received: {keys:?}")
                }
            }
        }

        /// What another tool does: writes its own value behind our back.
        fn overwrite(&self, key: &str, value: i32) {
            let mut state = self.tuner.state();
            let controls = &mut state.backend.devices[0].controls;
            match controls.iter_mut().find(|control| control.key == key) {
                Some(control) => control.current = value,
                None => {
                    let keys: Vec<_> = controls.iter().map(|c| c.key.as_str()).collect();
                    panic!("(fake device): expected a control {key} | received: {keys:?}")
                }
            }
        }

        fn kinds_of_jobs(&self) -> Vec<&'static str> {
            let kind = |job: &TuneJob| match (job.apply.is_empty(), job.reset.is_empty()) {
                (true, true) => "look",
                (false, true) => "apply",
                (true, false) => "reset",
                (false, false) => "apply+reset",
            };
            self.tuner.jobs().iter().map(kind).collect()
        }
    }

    #[test]
    fn with_no_setting_nothing_is_scheduled_until_somebody_asks() {
        let mut fixture = Fixture::new(Vec::new());
        assert_eq!(fixture.tuning.next_wake(fixture.now), None);
        fixture.pass(3600);
        assert_eq!(fixture.tuner.jobs(), Vec::new(), "an idle service starts no tuner");
        assert_eq!(fixture.tuning.status(), TuningStatus::default());

        assert!(fixture.tuning.first_look_pending(fixture.now), "a status request waits");
        assert_eq!(fixture.tuning.next_wake(fixture.now), Some(Duration::ZERO));
        fixture.pass(0);
        assert!(!fixture.tuning.first_look_pending(fixture.now));
        let device = fixture.device();
        assert_eq!((device.enabled, &device.state), (false, &TuningState::Unmanaged));
        assert_eq!(device.controls.len(), 2);
        assert_eq!(fixture.tuning.next_wake(fixture.now), None, "and then nothing again");
    }

    #[test]
    fn a_setting_is_applied_at_start_checked_again_and_then_left_alone() {
        let mut fixture = Fixture::new(vec![setting(&[("power-limit", -20)])]);
        assert_eq!(fixture.tuning.next_wake(fixture.now), Some(Duration::ZERO));
        fixture.pass(0);
        assert_eq!(fixture.kinds_of_jobs(), ["look", "apply"]);
        assert_eq!(fixture.current("power-limit"), -20);
        // The report of the apply already shows the values in place.
        assert_eq!(fixture.device().state, TuningState::Applied);
        // It stays until the machine is seen alive.
        fixture.assert_mark(true, "right after the apply");

        fixture.pass(45);
        fixture.assert_mark(true, "45 s after the apply, short of the survival period");
        fixture.pass(15);
        fixture.assert_mark(false, "60 s after the apply");
        fixture.pass(120);
        assert_eq!(fixture.kinds_of_jobs(), ["look", "apply", "look", "look", "look"]);
        assert_eq!(fixture.tuning.next_wake(fixture.now), None, "the round is over");
        let device = fixture.device();
        assert!(device.enabled);
        assert_eq!(device.controls[0].wanted, Some(-20));
        assert_eq!(device.controls[1].wanted, None, "not listed: the default is expected");
    }

    #[test]
    fn values_another_tool_wrote_are_applied_again_and_then_contested() {
        let mut fixture = Fixture::new(vec![setting(&[("power-limit", -20)])]);
        fixture.pass(0);
        fixture.overwrite("power-limit", 5);
        fixture.pass(45);
        assert_eq!(fixture.current("power-limit"), -20, "(second apply): expected ours back");
        fixture.overwrite("power-limit", 5);
        fixture.pass(60);
        assert_eq!(fixture.current("power-limit"), -20, "(third apply)");
        fixture.overwrite("power-limit", 5);
        fixture.pass(60);
        assert_eq!(fixture.current("power-limit"), 5, "the fourth time it is left alone");
        assert_eq!(fixture.device().state, TuningState::Contested);
        fixture.pass(3600);
        let applies = fixture.kinds_of_jobs().iter().filter(|kind| **kind == "apply").count();
        assert_eq!(applies, usize::from(MAX_APPLIES));

        // The user asks again: a new round.
        fixture.tuning.apply_again(fixture.now);
        fixture.pass(0);
        assert_eq!(fixture.current("power-limit"), -20);
        assert_eq!(fixture.device().state, TuningState::Applied);
    }

    #[test]
    fn a_control_the_setting_does_not_list_is_held_at_its_default() {
        let mut fixture = Fixture::new(vec![setting(&[("power-limit", -20)])]);
        fixture.pass(0);
        fixture.overwrite("core-p7-voltage", 900);
        fixture.pass(45);
        assert_eq!(fixture.current("core-p7-voltage"), 1050);
    }

    #[test]
    fn a_mark_left_by_a_machine_that_stopped_holds_the_settings_back() {
        let mut fixture = Fixture::with_mark(
            vec![setting(&[("core-p7-voltage", 800)])],
            FakeApplyMark::left_behind(),
        );
        assert_eq!(fixture.tuning.next_wake(fixture.now), None, "nothing is applied by itself");
        fixture.tuning.logged_on(fixture.now);
        fixture.tuning.resumed(fixture.now);
        assert_eq!(fixture.tuning.next_wake(fixture.now), None, "not at logon or wake either");

        assert!(fixture.tuning.first_look_pending(fixture.now));
        fixture.pass(0);
        assert_eq!(fixture.kinds_of_jobs(), ["look"], "a look for the status, and no apply");
        assert_eq!(fixture.device().state, TuningState::Held);
        assert_eq!(fixture.current("core-p7-voltage"), 1050);
        fixture.tuning.stopping();
        // Stopping does not forget what the last start found.
        fixture.assert_mark(true, "after a stop that follows a held start");

        fixture.tuning.apply_again(fixture.now);
        fixture.pass(0);
        assert_eq!(fixture.current("core-p7-voltage"), 800);
        assert_eq!(fixture.device().state, TuningState::Applied);
    }

    #[test]
    fn new_settings_release_what_was_held_and_a_mark_without_settings_means_nothing() {
        let mut fixture = Fixture::with_mark(
            vec![setting(&[("core-p7-voltage", 800)])],
            FakeApplyMark::left_behind(),
        );
        fixture.tuning.set_settings(vec![setting(&[("core-p7-voltage", 1000)])], fixture.now);
        fixture.pass(0);
        assert_eq!(fixture.current("core-p7-voltage"), 1000);

        let idle = Fixture::with_mark(Vec::new(), FakeApplyMark::left_behind());
        idle.assert_mark(false, "found at a start with no settings");
        let off = TuningSetting { enabled: false, ..setting(&[("power-limit", -20)]) };
        let switched_off = Fixture::with_mark(vec![off], FakeApplyMark::left_behind());
        switched_off.assert_mark(false, "found at a start with every setting off");
    }

    #[test]
    fn asking_again_or_saving_keeps_the_mark_of_values_not_yet_survived() {
        let mut fixture = Fixture::new(vec![setting(&[("core-p7-voltage", 800)])]);
        fixture.pass(0);
        fixture.assert_mark(true, "right after the apply");
        // Ten seconds in, the user asks again and saves the same values:
        // nothing is applied, and the machine has still not survived them.
        fixture.pass(10);
        fixture.tuning.apply_again(fixture.now);
        fixture.pass(0);
        fixture.tuning.set_settings(vec![setting(&[("core-p7-voltage", 800)])], fixture.now);
        fixture.pass(0);
        assert_eq!(fixture.kinds_of_jobs().iter().filter(|kind| **kind == "apply").count(), 1);
        fixture.assert_mark(true, "10 s after an apply, with a trigger and a save since");
        fixture.pass(50);
        fixture.assert_mark(false, "60 s after the apply");
    }

    #[test]
    fn an_apply_whose_tuner_failed_keeps_the_mark_only_until_the_machine_is_seen_alive() {
        let mut fixture = Fixture::new(vec![setting(&[("power-limit", -20)])]);
        fixture.tuning.run_due(fixture.now);
        let look = fixture.tuner.outcome_of_last();
        fixture.tuning.finished(look, fixture.now);
        assert_eq!(fixture.kinds_of_jobs(), ["look", "apply"]);
        // The tuner hangs in the apply and is ended: what it wrote is unknown.
        let hung = "(tuner): expected its report within 15 s | received: nothing";
        fixture.tuning.finished(Err(hung.into()), fixture.now);
        fixture.assert_mark(true, "right after the apply that failed");
        assert_eq!(fixture.tuning.status().problem.as_deref(), Some(hung));

        fixture.pass(60);
        fixture.assert_mark(false, "at a look 60 s after the apply that failed");
        assert_eq!(fixture.device().state, TuningState::Failed { reason: hung.into() });
        fixture.pass(120);
        assert_eq!(fixture.kinds_of_jobs().iter().filter(|kind| **kind == "apply").count(), 1);
    }

    #[test]
    fn an_orderly_stop_takes_the_mark_away() {
        let mut fixture = Fixture::new(vec![setting(&[("power-limit", -20)])]);
        fixture.pass(0);
        fixture.assert_mark(true, "right after the apply");
        fixture.tuning.stopping();
        fixture.assert_mark(false, "after an orderly stop");
    }

    #[test]
    fn turning_a_setting_off_puts_the_defaults_back_once() {
        let mut fixture = Fixture::new(vec![setting(&[("power-limit", -20)])]);
        fixture.pass(0);
        let off = TuningSetting { enabled: false, ..setting(&[("power-limit", -20)]) };
        fixture.tuning.set_settings(vec![off], fixture.now);
        fixture.pass(0);
        assert_eq!(fixture.kinds_of_jobs(), ["look", "apply", "look", "reset"]);
        assert_eq!(fixture.current("power-limit"), 0);
        let device = fixture.device();
        assert_eq!((device.enabled, &device.state), (false, &TuningState::Unmanaged));
        assert_eq!(device.controls[0].wanted, Some(-20), "the values are kept for later");

        // Later looks leave the device to its driver.
        fixture.overwrite("power-limit", 9);
        fixture.pass(45);
        fixture.pass(180);
        assert_eq!(fixture.current("power-limit"), 9);
        assert_eq!(fixture.kinds_of_jobs().iter().filter(|kind| **kind == "reset").count(), 1);
    }

    #[test]
    fn a_device_that_kept_its_values_through_a_service_restart_is_reset_when_turned_off() {
        // The driver still holds what the service applied before it restarted.
        let mut fixture = Fixture::new(vec![setting(&[("power-limit", -20)])]);
        fixture.overwrite("power-limit", -20);
        fixture.pass(0);
        assert_eq!(fixture.kinds_of_jobs(), ["look"], "nothing to apply: the values are there");

        let off = TuningSetting { enabled: false, ..setting(&[("power-limit", -20)]) };
        fixture.tuning.set_settings(vec![off], fixture.now);
        fixture.pass(0);
        assert_eq!(fixture.kinds_of_jobs(), ["look", "look", "reset"]);
        assert_eq!(
            fixture.current("power-limit"),
            0,
            "(power-limit after turning off): expected the default 0 | received: the setting kept"
        );
    }

    #[test]
    fn an_apply_whose_tuner_failed_is_still_put_back_when_turned_off() {
        let mut fixture = Fixture::new(vec![setting(&[("power-limit", -20)])]);
        fixture.tuning.run_due(fixture.now);
        let look = fixture.tuner.outcome_of_last();
        fixture.tuning.finished(look, fixture.now);
        // The tuner wrote the value and then hung.
        fixture.overwrite("power-limit", -20);
        fixture.tuning.finished(Err("(tuner): hung".into()), fixture.now);

        let off = TuningSetting { enabled: false, ..setting(&[("power-limit", -20)]) };
        fixture.tuning.set_settings(vec![off], fixture.now);
        fixture.pass(0);
        assert_eq!(fixture.current("power-limit"), 0, "(after turning off): expected the default");
    }

    /// Plays a look and leaves the apply it leads to under way, unanswered.
    fn fixture_with_an_apply_under_way(settings: Vec<TuningSetting>) -> Fixture {
        let mut fixture = Fixture::new(settings);
        fixture.tuning.run_due(fixture.now);
        let look = fixture.tuner.outcome_of_last();
        fixture.tuning.finished(look, fixture.now);
        assert_eq!(fixture.kinds_of_jobs(), ["look", "apply"], "the apply is under way");
        fixture
    }

    #[test]
    fn a_tuner_failure_from_before_a_trigger_does_not_block_the_round_it_started() {
        let mut fixture = fixture_with_an_apply_under_way(vec![setting(&[("power-limit", -20)])]);
        // The user asks again while the apply runs, and then the tuner hangs
        // and is ended. That failure belongs to the round before the trigger.
        fixture.tuning.apply_again(fixture.now);
        fixture.tuning.finished(Err("(tuner): hung".into()), fixture.now);
        fixture.assert_state(&TuningState::Different, "after the failure of an earlier round");
        // What the failed apply wrote is unknown all the same.
        assert!(fixture.manages_device(), "(managed): expected {CARD} | received: nothing");
        fixture.assert_mark(true, "after the failure of an earlier round");

        fixture.pass(0);
        assert_eq!(fixture.kinds_of_jobs(), ["look", "apply", "look", "apply"]);
        assert_eq!(fixture.current("power-limit"), -20);
        fixture.assert_state(&TuningState::Applied, "after the new round");
    }

    #[test]
    fn a_device_problem_from_before_a_trigger_does_not_block_the_round_it_started() {
        // 90 is outside -75..=75: applying it ends in a problem on the device.
        let mut fixture = fixture_with_an_apply_under_way(vec![setting(&[("power-limit", 90)])]);
        // The user saves a value that fits while that apply runs.
        fixture.tuning.set_settings(vec![setting(&[("power-limit", -20)])], fixture.now);
        let refused = fixture.tuner.outcome_of_last();
        fixture.tuning.finished(refused, fixture.now);
        fixture.assert_state(&TuningState::Different, "after the problem of an earlier round");
        assert!(fixture.manages_device(), "(managed): expected {CARD} | received: nothing");
        fixture.assert_mark(true, "after the problem of an earlier round");

        fixture.pass(0);
        assert_eq!(fixture.current("power-limit"), -20);
        fixture.assert_state(&TuningState::Applied, "after the new round");
    }

    #[test]
    fn a_device_whose_apply_failed_part_way_is_put_back_when_turned_off() {
        let mut fixture = Fixture::new(vec![setting(&[("power-limit", -20)])]);
        let reason = "(driver): expected ADL_OK (0) | received: -1";
        fixture.tuner.state().backend.failing.insert(CARD.into(), reason.into());
        fixture.pass(0);
        fixture.assert_state(&TuningState::Failed { reason: reason.into() }, "after the apply");
        // The card took the power limit before a later write failed, as a
        // Radeon's separate writes can leave it.
        fixture.overwrite("power-limit", -20);
        fixture.tuner.state().backend.failing.clear();

        let off = TuningSetting { enabled: false, ..setting(&[("power-limit", -20)]) };
        fixture.tuning.set_settings(vec![off], fixture.now);
        fixture.pass(0);
        assert_eq!(
            fixture.kinds_of_jobs(),
            ["look", "apply", "look", "reset"],
            "(jobs after turning off): expected a reset to follow the failed apply"
        );
        assert_eq!(fixture.current("power-limit"), 0);
    }

    #[test]
    fn an_apply_decided_after_a_trigger_belongs_to_the_round_of_that_trigger() {
        let mut fixture = Fixture::new(vec![setting(&[("power-limit", -20)])]);
        fixture.tuning.run_due(fixture.now);
        // The look is under way when the user asks again; its end decides
        // the apply, and that apply is the new round's.
        fixture.tuning.apply_again(fixture.now);
        let look = fixture.tuner.outcome_of_last();
        fixture.tuning.finished(look, fixture.now);
        assert_eq!(fixture.kinds_of_jobs(), ["look", "apply"], "the apply is under way");
        fixture.tuning.finished(Err("(tuner): hung".into()), fixture.now);
        let failed = TuningState::Failed { reason: "(tuner): hung".into() };
        fixture.assert_state(&failed, "after the failure of the round's own apply");
    }

    #[test]
    fn a_contested_device_does_not_stop_another_from_being_kept() {
        const OTHER: &str = "gpu.amd-67df.bus-4";
        let mut fixture = Fixture::new(Vec::new());
        fixture.tuner.state().backend.devices.push(TunedDevice { key: OTHER.into(), ..card() });
        let other = TuningSetting { device: OTHER.into(), ..setting(&[("power-limit", 10)]) };
        fixture.tuning.set_settings(vec![setting(&[("power-limit", -20)]), other], fixture.now);
        fixture.pass(0);
        // Another tool fights over the first card only.
        for _ in 0..4 {
            fixture.overwrite("power-limit", 5);
            fixture.pass(60);
        }
        assert_eq!(fixture.device().state, TuningState::Contested);

        // The second card loses its value too, and a client's look finds it.
        fixture.tuner.state().backend.devices[1].controls[0].current = 0;
        fixture.now += Duration::from_secs(60);
        fixture.tuning.refresh_if_stale(fixture.now, 1);
        fixture.pass(0);
        let status = fixture.tuning.status();
        let second =
            status.devices.iter().find(|device| device.key == OTHER).unwrap_or_else(|| {
                panic!("(tuning status): expected the device {OTHER} | received: none")
            });
        assert_eq!(
            (second.controls[0].current, &second.state),
            (10, &TuningState::Applied),
            "(second card): expected its setting kept | received: given up with the first"
        );
    }

    #[test]
    fn a_stop_while_an_apply_runs_keeps_the_mark() {
        let mut fixture = Fixture::new(vec![setting(&[("power-limit", -20)])]);
        fixture.tuning.run_due(fixture.now);
        let look = fixture.tuner.outcome_of_last();
        fixture.tuning.finished(look, fixture.now);
        assert_eq!(fixture.kinds_of_jobs(), ["look", "apply"], "the apply is under way");
        fixture.tuning.stopping();
        fixture.assert_mark(true, "after a stop during an apply");
    }

    #[test]
    fn the_first_device_problem_of_a_report_is_the_outcome() {
        assert_eq!(first_problem(TuneReport::default()), Ok(()));
        let broken = TunedDevice { problem: Some("the driver refused".into()), ..card() };
        let report = TuneReport { devices: vec![card(), broken] };
        assert_eq!(first_problem(report), Err("Radeon RX 550 Series: the driver refused".into()));
    }

    #[test]
    fn a_device_never_applied_to_is_never_reset() {
        let off = TuningSetting { enabled: false, ..setting(&[("power-limit", -20)]) };
        let mut fixture = Fixture::new(Vec::new());
        fixture.overwrite("power-limit", 9);
        fixture.tuning.set_settings(vec![off], fixture.now);
        assert!(fixture.tuning.first_look_pending(fixture.now));
        fixture.pass(0);
        assert_eq!(fixture.kinds_of_jobs(), ["look"]);
        assert_eq!(fixture.current("power-limit"), 9, "another tool's value is not ours to undo");
    }

    #[test]
    fn a_value_the_device_refuses_fails_once_and_is_not_retried() {
        let mut fixture = Fixture::new(vec![setting(&[("power-limit", 90)])]);
        fixture.pass(0);
        let failed = fixture.device().state;
        assert_eq!(
            failed,
            TuningState::Failed {
                reason: "(tuning.values.power-limit): expected -75..=75 | received: 90".into()
            }
        );
        fixture.pass(45);
        fixture.pass(180);
        assert_eq!(fixture.kinds_of_jobs(), ["look", "apply", "look", "look"]);
    }

    #[test]
    fn settings_are_refused_against_what_is_known_about_the_devices() {
        let mut fixture = Fixture::new(Vec::new());
        // Nothing is known yet: nothing can be said against a setting.
        assert_eq!(fixture.tuning.refusal(&[setting(&[("power-limit", 90)])]), None);
        fixture.tuning.first_look_pending(fixture.now);
        fixture.pass(0);
        assert_eq!(
            fixture.tuning.refusal(&[setting(&[("power-limit", 90)])]).as_deref(),
            Some(
                "Radeon RX 550 Series: (tuning.values.power-limit): expected -75..=75 | received: 90"
            )
        );
        assert!(fixture.tuning.refusal(&[setting(&[("fan-max-speed", 1)])]).is_some());
        assert_eq!(fixture.tuning.refusal(&[setting(&[("power-limit", -20)])]), None);
        let elsewhere =
            TuningSetting { device: "gpu.amd-0000.bus-9".into(), ..setting(&[("x", 1)]) };
        assert_eq!(fixture.tuning.refusal(&[elsewhere]), None, "an absent device is not judged");
    }

    #[test]
    fn a_setting_for_a_device_that_is_not_there_is_listed_as_missing() {
        let elsewhere = TuningSetting { device: "gpu.amd-0000.bus-9".into(), ..setting(&[]) };
        let mut fixture = Fixture::new(vec![elsewhere]);
        fixture.pass(0);
        assert_eq!(fixture.kinds_of_jobs(), ["look"], "nothing to apply to");
        let status = fixture.tuning.status();
        assert_eq!(status.devices.len(), 2);
        let missing = &status.devices[1];
        assert_eq!(
            (missing.key.as_str(), &missing.state),
            ("gpu.amd-0000.bus-9", &TuningState::Missing)
        );
        assert!(missing.enabled && missing.controls.is_empty());
    }

    #[test]
    fn a_tuner_that_fails_is_the_statuss_problem_and_a_later_look_clears_it() {
        let mut fixture = Fixture::new(Vec::new());
        fixture.tuner.state().failure = Some("The tuner could not be started: not found".into());
        fixture.tuning.first_look_pending(fixture.now);
        fixture.pass(0);
        assert!(!fixture.tuning.first_look_pending(fixture.now), "a failed look is still a look");
        let status = fixture.tuning.status();
        assert_eq!(status.problem.as_deref(), Some("The tuner could not be started: not found"));
        assert!(status.devices.is_empty());

        fixture.tuning.refresh_if_stale(fixture.now, 1);
        fixture.pass(0);
        assert_eq!(fixture.tuning.status().problem, None);
        assert_eq!(fixture.tuning.status().devices.len(), 1);
    }

    #[test]
    fn a_stale_report_is_refreshed_only_while_somebody_watches() {
        let mut fixture = Fixture::new(Vec::new());
        fixture.tuning.first_look_pending(fixture.now);
        fixture.pass(0);
        fixture.tuning.refresh_if_stale(fixture.now, 1);
        assert_eq!(fixture.tuning.next_wake(fixture.now), None, "the report is fresh");
        fixture.now += REFRESH_AFTER + Duration::from_secs(1);
        fixture.tuning.refresh_if_stale(fixture.now, 0);
        assert_eq!(fixture.tuning.next_wake(fixture.now), None, "nobody is watching");
        fixture.tuning.refresh_if_stale(fixture.now, 1);
        assert_eq!(fixture.tuning.next_wake(fixture.now), Some(Duration::ZERO));
        fixture.overwrite("power-limit", 7);
        fixture.pass(0);
        assert_eq!(fixture.current("power-limit"), 7, "what another tool set is shown");
    }

    #[test]
    fn wake_and_logon_start_a_round_only_when_something_is_in_effect() {
        let mut idle = Fixture::new(Vec::new());
        idle.tuning.resumed(idle.now);
        idle.tuning.logged_on(idle.now);
        assert_eq!(idle.tuning.next_wake(idle.now), None);

        let mut fixture = Fixture::new(vec![setting(&[("power-limit", -20)])]);
        fixture.pass(0);
        fixture.pass(200);
        assert_eq!(fixture.tuning.next_wake(fixture.now), None);
        // The driver came back from sleep with its defaults.
        fixture.overwrite("power-limit", 0);
        fixture.tuning.resumed(fixture.now);
        fixture.pass(0);
        assert_eq!(fixture.current("power-limit"), -20);
    }

    #[test]
    fn the_mark_is_a_file_that_comes_and_goes() {
        let directory =
            std::env::temp_dir().join(format!("SysPilot-Test-tuning-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let mut mark = FileApplyMark::in_directory(directory.clone());
        assert_mark_is(&mark, false, "in a directory that has none");
        mark.set();
        assert_mark_is(&mark, true, "after being set");
        let file = directory.join(APPLY_MARK_FILE);
        assert!(file.is_file(), "(mark file): expected {file:?} to exist | received: nothing");
        let reader = FileApplyMark::in_directory(directory.clone());
        assert_mark_is(&reader, true, "seen by another handle, as after a restart");
        mark.clear();
        mark.clear();
        assert_mark_is(&mark, false, "after being cleared twice");
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn removal_resets_exactly_the_devices_with_a_setting_in_effect() {
        let off = TuningSetting { device: "gpu.off".into(), enabled: false, ..setting(&[]) };
        assert_eq!(reset_job(&[]), None);
        assert_eq!(reset_job(std::slice::from_ref(&off)), None);
        let job = reset_job(&[setting(&[("power-limit", -20)]), off]);
        assert_eq!(job, Some(TuneJob { apply: Vec::new(), reset: vec![CARD.into()] }));
    }

    #[test]
    fn a_device_differs_by_a_listed_value_a_default_or_an_unknown_control() {
        let device = card();
        assert!(!differs(&device, &setting(&[])), "all at their defaults, nothing listed");
        assert!(differs(&device, &setting(&[("power-limit", -20)])));
        let mut moved = card();
        moved.controls[1].current = 900;
        assert!(differs(&moved, &setting(&[])), "a default that is not in place");
        assert!(!differs(&moved, &setting(&[("core-p7-voltage", 900)])));
        assert!(differs(&device, &setting(&[("fan-max-speed", 3000)])));
    }
}
