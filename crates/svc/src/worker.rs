//! The service's side of the sensor worker.
//!
//! Reading sensors pulls in Windows components that cost several megabytes
//! and never give them back, and it talks to buses and a driver that can
//! stall. The service therefore never reads hardware itself. While someone
//! needs values it runs the worker executable, asks it for a sample once a
//! second, and lets it exit when nobody is looking. An idle service stays
//! small, and a stuck device can only stall the worker.
//!
//! The protocol is in [`syspilot_proto::worker`].

use std::io::{self, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use syspilot_hw::{Provider, SensorSpec};
use syspilot_proto::frame::{read_frame, write_frame};
use syspilot_proto::message::{DriverState, FanRuntime, FanStatus, SystemInfo};
use syspilot_proto::sensor::SensorList;
use syspilot_proto::worker::{
    FANS_REQUEST, FanPlan, Probed, RESUME_REQUEST, SAMPLE_REQUEST, SUSPEND_REQUEST,
    WORKER_EXECUTABLE, WorkerFrame,
};

use crate::hardware::{DriverReport, Hardware};

/// How long a worker may take to probe the machine. The first use of the
/// Windows performance counters alone takes over a second.
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
/// How long a worker may take to answer a sample request. The service's
/// event loop waits for it, so this bounds what a stuck device can cost.
const SAMPLE_TIMEOUT: Duration = Duration::from_millis(750);
/// How long a worker gets to exit by itself before it is terminated.
const EXIT_GRACE: Duration = Duration::from_secs(2);
/// How long after a worker failed a new one is started. Long enough for a
/// machine that is resuming or overloaded to settle.
const RESTART_DELAY: Duration = Duration::from_secs(5);
/// The wait doubles after each restart that fails, up to this.
const RESTART_DELAY_LIMIT: Duration = Duration::from_secs(60);

/// Starts `executable` from the service's own directory with piped standard
/// input and output, no error output and no window. It is addressed by its
/// full path, never searched for, so nothing else can be run in its place.
pub(crate) fn spawn_sibling(executable: &str) -> io::Result<(Child, ChildStdin, ChildStdout)> {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
    let path = std::env::current_exe()?.with_file_name(executable);
    let mut process = Command::new(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // A console service must not flash a window.
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()?;
    match process.stdin.take().zip(process.stdout.take()) {
        Some((input, output)) => Ok((process, input, output)),
        None => {
            let _ = process.kill();
            Err(io::Error::other(format!("{executable} has no standard streams")))
        }
    }
}

/// The two ends of a started worker.
pub struct WorkerLink {
    /// Where sample requests are written. Dropping it ends the worker.
    pub requests: Box<dyn Write + Send>,
    /// Where the worker's frames arrive.
    pub frames: Box<dyn Read + Send>,
    /// The process, when the worker is one.
    pub process: Option<Child>,
}

/// Starts workers.
pub trait WorkerSpawner: Send + Sync {
    /// Starts one worker.
    fn spawn(&self) -> io::Result<WorkerLink>;
}

/// Starts the worker executable that sits next to the service's (see
/// [`spawn_sibling`]).
pub struct ProcessSpawner;

impl WorkerSpawner for ProcessSpawner {
    fn spawn(&self) -> io::Result<WorkerLink> {
        let (process, requests, frames) = spawn_sibling(WORKER_EXECUTABLE)?;
        Ok(WorkerLink {
            requests: Box::new(requests),
            frames: Box::new(frames),
            process: Some(process),
        })
    }
}

/// What the service wants of the fans and what the controller last said,
/// shared by [`WorkerHardware`] (which is told) and the worker that is
/// currently live (which carries it out and reports).
#[derive(Default)]
struct FanLink {
    /// The curves to follow. `None` until the service first has any.
    plan: Option<FanPlan>,
    /// Counts plans, so that a worker knows whether it has the latest.
    serial: u64,
    /// The machine is asleep or about to be: the controller lets go.
    suspended: bool,
    /// The controller's last report. Empty when no worker is live, which
    /// reads as "the BIOS holds every channel".
    runtime: Vec<FanRuntime>,
    /// The input of the worker that is sampling, so that a plan or a sleep
    /// reaches it at once and not with the next sample. Weak: the worker
    /// owns its input, and ending it must close the pipe.
    pipe: Weak<Mutex<WorkerPipe>>,
}

type SharedFans = Arc<Mutex<FanLink>>;

fn fans_of(shared: &SharedFans) -> MutexGuard<'_, FanLink> {
    // What the link holds stays consistent whichever thread panicked.
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The input of one worker and what that worker was already told about the
/// fans; a new worker has been told nothing.
///
/// Lock order: the link first and only briefly, never while holding this.
struct WorkerPipe {
    writer: Box<dyn Write + Send>,
    sent_serial: u64,
    sent_suspended: bool,
}

fn pipe_of(pipe: &Mutex<WorkerPipe>) -> MutexGuard<'_, WorkerPipe> {
    pipe.lock().unwrap_or_else(PoisonError::into_inner)
}

impl WorkerPipe {
    fn send(&mut self, bytes: &[u8]) -> Option<()> {
        self.writer.write_all(bytes).and_then(|()| self.writer.flush()).ok()
    }

    /// Tells the worker about a newer plan and a change of sleep, if any.
    /// `None` when the worker cannot be written to.
    fn push_fans(&mut self, shared: &SharedFans) -> Option<()> {
        let (plan, serial, suspended) = {
            let link = fans_of(shared);
            let news = (link.serial != self.sent_serial).then(|| link.plan.clone()).flatten();
            (news, link.serial, link.suspended)
        };
        if let Some(plan) = plan {
            // The byte and the frame go out in one write.
            let mut bytes = vec![FANS_REQUEST];
            write_frame(&mut bytes, &plan).ok()?;
            self.send(&bytes)?;
        }
        self.sent_serial = serial;
        if suspended != self.sent_suspended {
            self.send(&[if suspended { SUSPEND_REQUEST } else { RESUME_REQUEST }])?;
            self.sent_suspended = suspended;
        }
        Some(())
    }
}

/// A running worker as the service holds it.
struct Worker {
    /// `None` once the worker was told to end.
    requests: Option<Arc<Mutex<WorkerPipe>>>,
    /// Frames, read by a thread so that waiting for one can time out.
    frames: Receiver<WorkerFrame>,
    process: Option<Child>,
    fans: SharedFans,
}

impl Worker {
    fn start(link: WorkerLink, fans: SharedFans) -> Self {
        let (sender, frames) = mpsc::channel();
        let mut source = link.frames;
        // Ends when the worker closes its output or the receiver is gone.
        std::thread::spawn(move || {
            while let Ok(frame) = read_frame::<_, WorkerFrame>(&mut source) {
                if sender.send(frame).is_err() {
                    break;
                }
            }
        });
        let pipe = WorkerPipe { writer: link.requests, sent_serial: 0, sent_suspended: false };
        Self { requests: Some(Arc::new(Mutex::new(pipe))), frames, process: link.process, fans }
    }

    /// Waits for the worker's report of what it found.
    fn probed(&mut self) -> Result<Probed, String> {
        match self.frames.recv_timeout(PROBE_TIMEOUT) {
            Ok(WorkerFrame::Probed(probed)) => Ok(*probed),
            Ok(WorkerFrame::Sample(_) | WorkerFrame::Fans(_)) => {
                Err("(sensor worker): expected its report first | received: another frame".into())
            }
            Err(RecvTimeoutError::Timeout) => Err(format!(
                "(sensor worker): expected a report within {} s | received: nothing",
                PROBE_TIMEOUT.as_secs()
            )),
            Err(RecvTimeoutError::Disconnected) => {
                Err("(sensor worker): expected a report | received: the worker ended".into())
            }
        }
    }

    /// The worker's report if it has arrived, without waiting. `Some(None)`
    /// means the worker will never send one.
    fn probed_by_now(&mut self) -> Option<Option<Probed>> {
        match self.frames.try_recv() {
            Ok(WorkerFrame::Probed(probed)) => Some(Some(*probed)),
            Ok(WorkerFrame::Sample(_) | WorkerFrame::Fans(_)) | Err(TryRecvError::Disconnected) => {
                Some(None)
            }
            Err(TryRecvError::Empty) => None,
        }
    }

    /// Asks for one sample. `None` when the worker is gone or too slow. The
    /// fan plan and the controller's reports travel along: the plan goes
    /// first, and a report that arrives before the answer is kept.
    fn sample(&mut self) -> Option<Vec<Option<f64>>> {
        let pipe = self.requests.clone()?;
        // A worker that samples is the one a plan or a sleep must reach.
        fans_of(&self.fans).pipe = Arc::downgrade(&pipe);
        {
            let mut pipe = pipe_of(&pipe);
            pipe.push_fans(&self.fans)?;
            pipe.send(&[SAMPLE_REQUEST])?;
        }
        let deadline = Instant::now() + SAMPLE_TIMEOUT;
        loop {
            let wait = deadline.saturating_duration_since(Instant::now());
            match self.frames.recv_timeout(wait) {
                Ok(WorkerFrame::Sample(values)) => return Some(values),
                Ok(WorkerFrame::Fans(runtime)) => fans_of(&self.fans).runtime = runtime,
                Ok(WorkerFrame::Probed(_)) | Err(_) => return None,
            }
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // The end of its input is the worker's signal to finish what it is
        // doing and exit, which lets it leave buses and devices tidy.
        self.requests = None;
        let Some(mut process) = self.process.take() else { return };
        let deadline = Instant::now() + EXIT_GRACE;
        while Instant::now() < deadline {
            if matches!(process.try_wait(), Ok(Some(_)) | Err(_)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = process.kill();
        let _ = process.wait();
    }
}

/// Where a provider stands with its worker.
enum Link {
    /// The worker answers.
    Live(Worker),
    /// The worker failed. A new one is started at `retry_at`; if that one
    /// fails too, the next wait is `delay`.
    Down { retry_at: Instant, delay: Duration },
    /// A new worker was started and is probing the machine. Nobody waits
    /// for it: each sample only looks whether its report has arrived.
    Restarting { worker: Worker, since: Instant, delay: Duration },
}

/// The sensor specs a sensor list describes, in its order.
fn specs_of(sensors: &SensorList) -> Vec<SensorSpec> {
    sensors
        .sensors
        .iter()
        .filter_map(|sensor| {
            let device = sensors.devices.get(sensor.device as usize)?;
            Some(SensorSpec {
                device: device.name.clone(),
                class: device.class,
                key: sensor.key.clone(),
                label: sensor.label.clone(),
                kind: sensor.kind,
            })
        })
        .collect()
}

/// The worker's sensors, presented to the service's hub as one provider.
struct WorkerProvider {
    link: Link,
    spawner: Arc<dyn WorkerSpawner>,
    /// Wait before the first restart; doubled after each failed one.
    restart_delay: Duration,
    specs: Vec<SensorSpec>,
    needs_driver: Vec<bool>,
    fans: SharedFans,
}

impl WorkerProvider {
    fn new(
        worker: Worker,
        sensors: &SensorList,
        spawner: Arc<dyn WorkerSpawner>,
        restart_delay: Duration,
        fans: SharedFans,
    ) -> Self {
        let needs_driver = sensors.sensors.iter().map(|sensor| sensor.driver).collect();
        Self {
            link: Link::Live(worker),
            spawner,
            restart_delay,
            specs: specs_of(sensors),
            needs_driver,
            fans,
        }
    }

    /// The state after a failure: no worker, and a restart after `delay`.
    fn down(delay: Duration) -> Link {
        Link::Down { retry_at: Instant::now() + delay, delay: (delay * 2).min(RESTART_DELAY_LIMIT) }
    }

    /// One step with the worker: its values, or `None` while there are none.
    /// Only a live worker is ever waited for, and only once: after a missed
    /// answer it is replaced, so a stuck device costs one stall per restart.
    fn step(&mut self) -> Option<Vec<Option<f64>>> {
        let link = std::mem::replace(&mut self.link, Self::down(self.restart_delay));
        let (link, values) = match link {
            Link::Live(mut worker) => match worker.sample() {
                Some(values) if values.len() == self.specs.len() => {
                    (Link::Live(worker), Some(values))
                }
                _ => {
                    // The worker hands the fans back when it ends; what it
                    // last reported is no longer true.
                    fans_of(&self.fans).runtime.clear();
                    (Self::down(self.restart_delay), None)
                }
            },
            Link::Down { retry_at, delay } if Instant::now() >= retry_at => {
                match self.spawner.spawn() {
                    Ok(link) => {
                        let worker = Worker::start(link, self.fans.clone());
                        (Link::Restarting { worker, since: Instant::now(), delay }, None)
                    }
                    Err(_) => (Self::down(delay), None),
                }
            }
            Link::Down { retry_at, delay } => (Link::Down { retry_at, delay }, None),
            Link::Restarting { mut worker, since, delay } => match worker.probed_by_now() {
                // The same sensors in the same order, or its values would
                // land on the wrong IDs.
                Some(Some(probed)) if specs_of(&probed.sensors) == self.specs => {
                    let values = worker.sample();
                    (Link::Live(worker), values)
                }
                Some(_) => (Self::down(delay), None),
                None if since.elapsed() > PROBE_TIMEOUT => (Self::down(delay), None),
                None => (Link::Restarting { worker, since, delay }, None),
            },
        };
        self.link = link;
        values.filter(|values| values.len() == self.specs.len())
    }
}

impl Provider for WorkerProvider {
    fn sensors(&self) -> Vec<SensorSpec> {
        self.specs.clone()
    }

    fn sample(&mut self, out: &mut Vec<Option<f64>>) {
        match self.step() {
            Some(values) => out.extend(values),
            None => out.extend(std::iter::repeat_n(None, self.specs.len())),
        }
    }

    fn sensor_needs_driver(&self, index: usize) -> bool {
        self.needs_driver.get(index).copied().unwrap_or(false)
    }
}

impl Drop for WorkerProvider {
    fn drop(&mut self) {
        // Sampling stopped: the worker ends and with it the controller.
        fans_of(&self.fans).runtime.clear();
    }
}

/// The machine's hardware, reached through workers.
pub struct WorkerHardware {
    spawner: Arc<dyn WorkerSpawner>,
    /// Wait before a failed worker is replaced; see [`WorkerProvider`].
    restart_delay: Duration,
    /// Reads the installed driver version without opening the driver. A
    /// change means the last report is out of date.
    installed_driver: fn() -> Option<String>,
    /// The last report, with the installed driver version it was made under.
    report: Option<(Option<String>, Probed)>,
    /// A worker that was started without waiting and has not reported yet.
    probing: Option<(Worker, Instant)>,
    /// A worker that has reported and can become the next providers.
    ready: Option<(Worker, Probed)>,
    /// The last worker started without waiting failed. Kept until
    /// [`Hardware::providers`] is asked, so that it answers "none" instead
    /// of trying again while the caller waits.
    failed: bool,
    /// What the service wants of the fans and what the controller said.
    fans: SharedFans,
}

impl WorkerHardware {
    /// Hardware read by workers from `spawner`.
    pub fn new(spawner: Arc<dyn WorkerSpawner>, installed_driver: fn() -> Option<String>) -> Self {
        Self {
            spawner,
            restart_delay: RESTART_DELAY,
            installed_driver,
            report: None,
            probing: None,
            ready: None,
            failed: false,
            fans: SharedFans::default(),
        }
    }

    /// Whether the stored report still describes the machine.
    fn report_is_current(&self) -> bool {
        let installed = (self.installed_driver)();
        self.report.as_ref().is_some_and(|(seen, _)| *seen == installed)
    }

    /// Looks whether the worker started by [`Self::probe_without_waiting`]
    /// has reported. Returns whether it is still being waited for.
    fn still_probing(&mut self) -> bool {
        let Some((mut worker, since)) = self.probing.take() else { return false };
        match worker.probed_by_now() {
            Some(Some(probed)) => {
                self.remember(Ok(probed.clone()));
                self.ready = Some((worker, probed));
            }
            Some(None) => {
                self.fail("(sensor worker): expected a report | received: the worker ended")
            }
            None if since.elapsed() > PROBE_TIMEOUT => {
                self.fail("(sensor worker): expected a report in time | received: nothing");
            }
            None => {
                self.probing = Some((worker, since));
                return true;
            }
        }
        false
    }

    fn fail(&mut self, problem: &str) {
        self.remember(Err(problem.to_owned()));
        self.failed = true;
    }

    /// Starts a worker and returns at once. Returns whether one is now on
    /// its way; `false` means it could not be started, which is remembered
    /// like any failed look.
    fn probe_without_waiting(&mut self) -> bool {
        match self.spawner.spawn() {
            Ok(link) => {
                self.probing = Some((Worker::start(link, self.fans.clone()), Instant::now()));
                true
            }
            Err(error) => {
                self.fail(&format!("The sensor worker could not be started: {error}"));
                false
            }
        }
    }

    /// The real machine.
    pub fn machine() -> Self {
        Self::new(Arc::new(ProcessSpawner), syspilot_pawnio::installed_version)
    }

    fn start(&mut self) -> Result<(Worker, Probed), String> {
        let link = self
            .spawner
            .spawn()
            .map_err(|error| format!("The sensor worker could not be started: {error}"))?;
        let mut worker = Worker::start(link, self.fans.clone());
        let probed = worker.probed()?;
        Ok((worker, probed))
    }

    /// Sends what is new to the live worker's input, if there is one. A
    /// worker that cannot be written to is dealt with by its next sample.
    fn push_to(&self, pipe: Option<Arc<Mutex<WorkerPipe>>>) {
        if let Some(pipe) = pipe {
            let _ = pipe_of(&pipe).push_fans(&self.fans);
        }
    }

    /// Stores a report, or the fact that none could be made. Either way
    /// the next question is answered from memory until the driver changes.
    fn remember(&mut self, outcome: Result<Probed, String>) {
        let installed = (self.installed_driver)();
        let probed = outcome.unwrap_or_else(|error| {
            Probed::nothing(match &installed {
                Some(_) => DriverState::Failed { error },
                None => DriverState::Missing,
            })
        });
        self.report = Some((installed, probed));
    }

    /// The current report, made by a short-lived worker if there is none
    /// or the installed driver changed since.
    fn report(&mut self) -> &Probed {
        if !self.report_is_current() {
            // The worker is dropped right after reporting, which ends it.
            let outcome = self.start().map(|(_worker, probed)| probed);
            self.remember(outcome);
        }
        // `remember` stored one above if there was none.
        &self.report.get_or_insert_with(|| (None, Probed::nothing(DriverState::Missing))).1
    }
}

impl Hardware for WorkerHardware {
    fn driver(&mut self) -> DriverReport {
        let report = self.report();
        DriverReport { state: report.driver.clone(), features: report.features.clone() }
    }

    fn report_pending(&mut self) -> bool {
        self.still_probing() || (!self.report_is_current() && self.probe_without_waiting())
    }

    fn providers_pending(&mut self) -> bool {
        if self.still_probing() {
            return true;
        }
        self.ready.is_none() && !self.failed && self.probe_without_waiting()
    }

    fn release(&mut self) {
        // Dropping the worker ends it.
        self.ready = None;
    }

    fn providers(&mut self) -> Vec<Box<dyn Provider>> {
        if std::mem::take(&mut self.failed) {
            return Vec::new();
        }
        // A worker that already reported, or else one that is waited for.
        let started = match self.ready.take() {
            Some(ready) => Ok(ready),
            None => self.start(),
        };
        match started {
            Ok((worker, probed)) => {
                let provider = WorkerProvider::new(
                    worker,
                    &probed.sensors,
                    self.spawner.clone(),
                    self.restart_delay,
                    self.fans.clone(),
                );
                self.remember(Ok(probed));
                if provider.specs.is_empty() { Vec::new() } else { vec![Box::new(provider)] }
            }
            Err(error) => {
                self.remember(Err(error));
                Vec::new()
            }
        }
    }

    fn system_info(&mut self) -> SystemInfo {
        self.report().system.clone()
    }

    fn fan_layout(&mut self) -> FanStatus {
        self.report().fans.clone()
    }

    fn known_fan_layout(&mut self) -> Option<FanStatus> {
        let current = self.report_is_current();
        self.report.as_ref().filter(|_| current).map(|(_, probed)| probed.fans.clone())
    }

    fn set_fan_plan(&mut self, plan: FanPlan) {
        let pipe = {
            let mut link = fans_of(&self.fans);
            link.plan = Some(plan);
            link.serial += 1;
            link.pipe.upgrade()
        };
        self.push_to(pipe);
    }

    fn fan_runtime(&mut self) -> Vec<FanRuntime> {
        fans_of(&self.fans).runtime.clone()
    }

    fn set_fans_suspended(&mut self, suspended: bool) {
        let pipe = {
            let mut link = fans_of(&self.fans);
            link.suspended = suspended;
            link.pipe.upgrade()
        };
        // A machine about to sleep cannot wait for the next sample.
        self.push_to(pipe);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use syspilot_hw::Hub;
    use syspilot_proto::message::{Feature, SystemSummary};
    use syspilot_proto::sensor::{DeviceClass, DeviceInfo, SensorInfo, SensorKind};

    /// What every fake worker reports: two temperature sensors, the second
    /// one needing the driver.
    fn fake_report() -> Probed {
        let sensor = |id, key: &str, driver| SensorInfo {
            id,
            device: 0,
            key: key.into(),
            label: key.into(),
            kind: SensorKind::Temperature,
            driver,
        };
        Probed {
            driver: DriverState::Ready { version: "2.2.0".into() },
            features: vec![Feature {
                key: "cpu-sensors".into(),
                title: "Processor temperature and power".into(),
                available: true,
                detail: "Fake processor".into(),
            }],
            system: SystemInfo {
                summary: SystemSummary { computer: "FAKE-PC".into(), ..SystemSummary::default() },
                sections: Vec::new(),
            },
            sensors: SensorList {
                generation: 0,
                devices: vec![DeviceInfo { name: "Fake".into(), class: DeviceClass::Cpu }],
                sensors: vec![sensor(0, "cpu.temp", false), sensor(1, "board.temp", true)],
            },
            fans: Default::default(),
        }
    }

    /// The values a fake worker answers every request with.
    const FAKE_VALUES: [Option<f64>; 2] = [Some(41.0), None];

    /// A worker on a thread of this process: speaks the protocol with
    /// [`fake_report`] and [`FAKE_VALUES`] until its input ends.
    fn fake_worker(mut requests: impl Read, mut frames: impl Write) {
        if write_frame(&mut frames, &WorkerFrame::Probed(Box::new(fake_report()))).is_err() {
            return;
        }
        let mut request = [0u8; 1];
        while matches!(requests.read(&mut request), Ok(1)) {
            if write_frame(&mut frames, &WorkerFrame::Sample(FAKE_VALUES.to_vec())).is_err() {
                return;
            }
        }
    }

    /// How a [`FakeSpawner`]'s workers behave.
    #[derive(Clone, Copy, PartialEq)]
    enum Behaviour {
        /// Like the real worker.
        Works,
        /// Reports, then never answers a sample request.
        StopsAnswering,
        /// The first worker stops answering; the ones after it work.
        OnlyFirstStopsAnswering,
        /// Cannot be started.
        FailsToStart,
    }

    /// Starts fake workers over in-memory pipes.
    struct FakeSpawner {
        behaviour: Behaviour,
        started: Arc<AtomicU32>,
        /// The far ends of silent workers, kept open so that they look
        /// stuck instead of gone.
        held: Arc<Mutex<Vec<Box<dyn Send>>>>,
    }

    impl FakeSpawner {
        fn new(behaviour: Behaviour) -> (Self, Arc<AtomicU32>) {
            let started = Arc::new(AtomicU32::new(0));
            (Self { behaviour, started: started.clone(), held: Arc::default() }, started)
        }
    }

    impl WorkerSpawner for FakeSpawner {
        fn spawn(&self) -> io::Result<WorkerLink> {
            if self.behaviour == Behaviour::FailsToStart {
                return Err(io::Error::new(io::ErrorKind::NotFound, "no executable"));
            }
            let earlier = self.started.fetch_add(1, Ordering::Relaxed);
            let (request_reader, request_writer) = io::pipe()?;
            let (frame_reader, mut frame_writer) = io::pipe()?;
            let works = match self.behaviour {
                Behaviour::Works => true,
                Behaviour::OnlyFirstStopsAnswering => earlier > 0,
                Behaviour::StopsAnswering | Behaviour::FailsToStart => false,
            };
            if works {
                std::thread::spawn(move || fake_worker(request_reader, frame_writer));
            } else {
                write_frame(&mut frame_writer, &WorkerFrame::Probed(Box::new(fake_report())))
                    .map_err(io::Error::other)?;
                let mut held = self.held.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                held.push(Box::new(request_reader));
                held.push(Box::new(frame_writer));
            }
            Ok(WorkerLink {
                requests: Box::new(request_writer),
                frames: Box::new(frame_reader),
                process: None,
            })
        }
    }

    fn no_driver() -> Option<String> {
        None
    }

    fn hardware(behaviour: Behaviour) -> (WorkerHardware, Arc<AtomicU32>) {
        let (spawner, started) = FakeSpawner::new(behaviour);
        (WorkerHardware::new(Arc::new(spawner), no_driver), started)
    }

    #[test]
    fn service_reads_the_workers_sensors_through_one_provider() {
        let (mut hardware, started) = hardware(Behaviour::Works);
        let mut providers = hardware.providers();
        assert_eq!(providers.len(), 1);
        let keys: Vec<String> = providers[0].sensors().into_iter().map(|spec| spec.key).collect();
        assert_eq!(keys, ["cpu.temp", "board.temp"]);

        let mut values = Vec::new();
        providers[0].sample(&mut values);
        assert_eq!(values, FAKE_VALUES);

        // The report that came with the providers answers these too.
        assert_eq!(hardware.driver().state, DriverState::Ready { version: "2.2.0".into() });
        assert_eq!(hardware.driver().features[0].key, "cpu-sensors");
        assert_eq!(hardware.system_info().summary.computer, "FAKE-PC");
        assert_eq!(started.load(Ordering::Relaxed), 1, "one worker served everything");
    }

    #[test]
    fn sensors_that_need_the_driver_stay_marked_across_the_worker() {
        let (mut hardware, _started) = hardware(Behaviour::Works);
        let relisted = Hub::new(hardware.providers(), 7);
        let marks: Vec<bool> = relisted.list().sensors.iter().map(|sensor| sensor.driver).collect();
        assert_eq!(marks, [false, true]);
        assert_eq!(relisted.list().devices, fake_report().sensors.devices);
        assert_eq!(relisted.list().generation, 7, "the service numbers the sensor set");
    }

    #[test]
    fn status_alone_uses_one_short_lived_worker_and_remembers_its_report() {
        let (mut hardware, started) = hardware(Behaviour::Works);
        assert_eq!(hardware.driver().features.len(), 1);
        assert_eq!(hardware.system_info().summary.computer, "FAKE-PC");
        assert_eq!(hardware.driver().features.len(), 1);
        assert_eq!(started.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_worker_that_stops_answering_costs_one_timeout_and_reads_nothing() {
        let (mut hardware, _started) = hardware(Behaviour::StopsAnswering);
        let mut providers = hardware.providers();
        let mut values = Vec::new();

        let before = Instant::now();
        providers[0].sample(&mut values);
        assert_eq!(values, [None, None]);
        assert!(before.elapsed() >= SAMPLE_TIMEOUT, "the first sample waits for the worker");

        values.clear();
        let before = Instant::now();
        providers[0].sample(&mut values);
        assert_eq!(values, [None, None]);
        assert!(
            before.elapsed() < SAMPLE_TIMEOUT / 2,
            "(worker provider): expected a failed worker not to be waited for again | waited: {:?}",
            before.elapsed()
        );
    }

    /// Asks until the look has ended, as the event loop does.
    fn wait_until(mut pending: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while pending() {
            assert!(
                Instant::now() < deadline,
                "(worker hardware): expected the look to end | received: still pending after 5 s"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn a_look_without_waiting_serves_the_report_and_then_the_providers() {
        let (mut hardware, started) = hardware(Behaviour::Works);
        wait_until(|| hardware.report_pending());
        assert_eq!(hardware.system_info().summary.computer, "FAKE-PC");
        assert!(!hardware.providers_pending(), "the worker that reported is ready to be used");

        let mut providers = hardware.providers();
        let mut values = Vec::new();
        providers[0].sample(&mut values);
        assert_eq!(values, FAKE_VALUES);
        assert_eq!(started.load(Ordering::Relaxed), 1, "one worker did all of it");

        // With that worker handed out, the next providers need a new one.
        wait_until(|| hardware.providers_pending());
        hardware.release();
        assert!(!hardware.report_pending(), "the report itself is still current");
        assert_eq!(started.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_look_that_cannot_start_is_not_pending_and_gives_no_providers() {
        let (mut hardware, _started) = hardware(Behaviour::FailsToStart);
        assert!(!hardware.providers_pending());
        assert!(hardware.providers().is_empty());
        assert!(!hardware.report_pending(), "the failure is the report until the driver changes");
        assert_eq!(hardware.driver().state, DriverState::Missing);
    }

    #[test]
    fn a_failed_worker_is_replaced_without_waiting_and_the_values_come_back() {
        let (mut hardware, started) = hardware(Behaviour::OnlyFirstStopsAnswering);
        hardware.restart_delay = Duration::ZERO;
        let mut providers = hardware.providers();
        let mut values = Vec::new();
        providers[0].sample(&mut values);
        assert_eq!(values, [None, None], "the first worker missed its answer");

        // The replacement reports on its own thread; no sample waits for it.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            values.clear();
            let before = Instant::now();
            providers[0].sample(&mut values);
            assert!(
                before.elapsed() < SAMPLE_TIMEOUT,
                "(worker provider): expected a restart not to block a sample | waited: {:?}",
                before.elapsed()
            );
            if values == FAKE_VALUES {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "(worker provider): expected values from the replacement worker | received: {values:?}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(started.load(Ordering::Relaxed), 2, "exactly one replacement");
    }

    #[test]
    fn a_replacement_that_cannot_start_is_retried_ever_later() {
        let first = WorkerProvider::down(Duration::from_secs(5));
        let Link::Down { delay, .. } = first else { panic!("expected the down state") };
        assert_eq!(delay, Duration::from_secs(10), "the next wait is doubled");
        let Link::Down { delay, .. } = WorkerProvider::down(Duration::from_secs(45)) else {
            panic!("expected the down state")
        };
        assert_eq!(delay, RESTART_DELAY_LIMIT, "and capped");
    }

    #[test]
    fn a_worker_that_cannot_start_is_reported_once_and_not_retried() {
        let (spawner, started) = FakeSpawner::new(Behaviour::FailsToStart);
        let mut installed = WorkerHardware::new(Arc::new(spawner), || Some("2.2.0".into()));
        assert!(installed.providers().is_empty());
        let DriverState::Failed { error } = installed.driver().state else {
            panic!("(worker hardware): expected a failed driver state when the worker is missing");
        };
        assert!(error.starts_with("The sensor worker could not be started"), "{error}");
        assert_eq!(installed.system_info(), SystemInfo::default());
        assert_eq!(started.load(Ordering::Relaxed), 0);

        let (mut absent, _) = hardware(Behaviour::FailsToStart);
        assert_eq!(absent.driver().state, DriverState::Missing, "no driver, nothing to fail");
    }

    #[test]
    fn a_driver_that_was_installed_since_makes_the_report_stale() {
        static INSTALLED: AtomicU32 = AtomicU32::new(0);
        fn installed() -> Option<String> {
            (INSTALLED.load(Ordering::Relaxed) == 1).then(|| "2.2.0".to_owned())
        }
        let (spawner, started) = FakeSpawner::new(Behaviour::Works);
        let mut hardware = WorkerHardware::new(Arc::new(spawner), installed);
        hardware.driver();
        hardware.driver();
        assert_eq!(started.load(Ordering::Relaxed), 1);

        INSTALLED.store(1, Ordering::Relaxed);
        hardware.driver();
        assert_eq!(started.load(Ordering::Relaxed), 2, "the machine is probed again");
    }

    #[test]
    fn dropping_the_worker_ends_it() {
        let (request_reader, request_writer) = io::pipe().unwrap();
        let (frame_reader, frame_writer) = io::pipe().unwrap();
        let worker_thread = std::thread::spawn(move || fake_worker(request_reader, frame_writer));
        let link = WorkerLink {
            requests: Box::new(request_writer),
            frames: Box::new(frame_reader),
            process: None,
        };
        let mut worker = Worker::start(link, SharedFans::default());
        worker.probed().unwrap();
        drop(worker);
        // The fake returns when its input ends; a join that never returns
        // would fail the test by timing out.
        worker_thread.join().unwrap();
    }

    /// What a fan-aware fake worker was asked to do, in order.
    #[derive(Debug, Clone, PartialEq)]
    enum Told {
        Plan(usize),
        Suspend,
        Resume,
    }

    /// What the fake controller answers a plan with.
    fn manual_report() -> Vec<FanRuntime> {
        vec![FanRuntime {
            channel: "board.pwm1".into(),
            phase: syspilot_proto::message::FanPhase::Manual,
            target_duty_pct: Some(40),
            problem: None,
        }]
    }

    /// A worker that speaks the fan part of the protocol: it records what it
    /// is told, answers a plan with a controller report as the real worker
    /// does, and answers samples until `answers` of them were given.
    fn fan_worker(
        mut requests: impl Read,
        mut frames: impl Write,
        told: Arc<Mutex<Vec<Told>>>,
        answers: u32,
    ) {
        if write_frame(&mut frames, &WorkerFrame::Probed(Box::new(fake_report()))).is_err() {
            return;
        }
        let record = |entry| told.lock().unwrap_or_else(PoisonError::into_inner).push(entry);
        let mut answered = 0;
        let mut request = [0u8; 1];
        while matches!(requests.read(&mut request), Ok(1)) {
            let sent = match request[0] {
                SAMPLE_REQUEST if answered < answers => {
                    answered += 1;
                    write_frame(&mut frames, &WorkerFrame::Sample(FAKE_VALUES.to_vec()))
                }
                FANS_REQUEST => {
                    let Ok(plan) = read_frame::<_, FanPlan>(&mut requests) else { return };
                    record(Told::Plan(plan.curves.len()));
                    write_frame(&mut frames, &WorkerFrame::Fans(manual_report()))
                }
                SUSPEND_REQUEST => {
                    record(Told::Suspend);
                    Ok(())
                }
                RESUME_REQUEST => {
                    record(Told::Resume);
                    Ok(())
                }
                _ => Ok(()),
            };
            if sent.is_err() {
                return;
            }
        }
    }

    /// Starts fan-aware workers; the first one stops answering after
    /// `first_answers` samples, the ones after it do not.
    struct FanSpawner {
        told: Arc<Mutex<Vec<Told>>>,
        started: AtomicU32,
        first_answers: u32,
    }

    impl WorkerSpawner for FanSpawner {
        fn spawn(&self) -> io::Result<WorkerLink> {
            let earlier = self.started.fetch_add(1, Ordering::Relaxed);
            let answers = if earlier == 0 { self.first_answers } else { u32::MAX };
            let (request_reader, request_writer) = io::pipe()?;
            let (frame_reader, frame_writer) = io::pipe()?;
            let told = self.told.clone();
            std::thread::spawn(move || fan_worker(request_reader, frame_writer, told, answers));
            Ok(WorkerLink {
                requests: Box::new(request_writer),
                frames: Box::new(frame_reader),
                process: None,
            })
        }
    }

    fn fan_hardware(first_answers: u32) -> (WorkerHardware, Arc<Mutex<Vec<Told>>>) {
        let told = Arc::new(Mutex::new(Vec::new()));
        let spawner = FanSpawner { told: told.clone(), started: AtomicU32::new(0), first_answers };
        let mut hardware = WorkerHardware::new(Arc::new(spawner), no_driver);
        hardware.restart_delay = Duration::ZERO;
        (hardware, told)
    }

    fn told_so_far(told: &Mutex<Vec<Told>>) -> Vec<Told> {
        told.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Waits until the fake worker, on its own thread, has been told `count`
    /// things.
    fn wait_for_told(told: &Mutex<Vec<Told>>, count: usize) -> Vec<Told> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let now = told_so_far(told);
            if now.len() >= count {
                return now;
            }
            assert!(
                Instant::now() < deadline,
                "(fake worker): expected {count} fan requests | received: {now:?}"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn some_plan(curves: usize) -> FanPlan {
        let curve = syspilot_proto::profile::FanCurve {
            channel: "board.pwm1".into(),
            enabled: true,
            sources: vec!["cpu.temp".into()],
            points: vec![
                syspilot_proto::profile::CurvePoint(30, 20),
                syspilot_proto::profile::CurvePoint(80, 100),
            ],
            hysteresis_c: 0,
            min_duty_pct: 0,
            ramp_pct_per_sec: 10,
        };
        FanPlan { store_directory: "unused".into(), curves: vec![curve; curves] }
    }

    #[test]
    fn a_plan_rides_with_the_next_sample_and_the_controllers_report_comes_back() {
        let (mut hardware, told) = fan_hardware(u32::MAX);
        let mut providers = hardware.providers();
        hardware.set_fan_plan(some_plan(2));
        assert_eq!(hardware.fan_runtime(), Vec::<FanRuntime>::new(), "nothing reported yet");

        let mut values = Vec::new();
        providers[0].sample(&mut values);
        assert_eq!(values, FAKE_VALUES, "the report in front of the sample does not hide it");
        assert_eq!(wait_for_told(&told, 1), [Told::Plan(2)]);
        assert_eq!(hardware.fan_runtime(), manual_report());

        // The same plan is not sent again.
        values.clear();
        providers[0].sample(&mut values);
        assert_eq!(told_so_far(&told), [Told::Plan(2)]);
    }

    #[test]
    fn sleep_and_wake_reach_the_live_worker_without_waiting_for_a_sample() {
        let (mut hardware, told) = fan_hardware(u32::MAX);
        let mut providers = hardware.providers();
        let mut values = Vec::new();
        providers[0].sample(&mut values);

        hardware.set_fans_suspended(true);
        assert_eq!(wait_for_told(&told, 1), [Told::Suspend], "no sample was asked for");
        hardware.set_fans_suspended(true);
        hardware.set_fans_suspended(false);
        assert_eq!(wait_for_told(&told, 2), [Told::Suspend, Told::Resume]);
    }

    #[test]
    fn a_plan_set_while_no_worker_samples_waits_for_the_next_one() {
        let (mut hardware, told) = fan_hardware(u32::MAX);
        hardware.set_fan_plan(some_plan(1));
        hardware.set_fans_suspended(true);
        assert_eq!(told_so_far(&told), Vec::<Told>::new(), "there is no worker to tell");

        let mut providers = hardware.providers();
        let mut values = Vec::new();
        providers[0].sample(&mut values);
        assert_eq!(wait_for_told(&told, 2), [Told::Plan(1), Told::Suspend]);
    }

    #[test]
    fn a_replacement_worker_gets_the_plan_and_a_lost_one_takes_its_report_with_it() {
        // The first worker answers one sample, then goes silent.
        let (mut hardware, told) = fan_hardware(1);
        let mut providers = hardware.providers();
        hardware.set_fan_plan(some_plan(1));
        let mut values = Vec::new();
        providers[0].sample(&mut values);
        wait_for_told(&told, 1);
        assert_eq!(hardware.fan_runtime(), manual_report());

        // The silent worker costs one timeout, and what it reported is gone.
        values.clear();
        providers[0].sample(&mut values);
        assert_eq!(values, [None, None]);
        assert_eq!(hardware.fan_runtime(), Vec::<FanRuntime>::new());

        // Its replacement is told the plan again.
        let deadline = Instant::now() + Duration::from_secs(5);
        while hardware.fan_runtime().is_empty() {
            values.clear();
            providers[0].sample(&mut values);
            assert!(
                Instant::now() < deadline,
                "(worker provider): expected the replacement to report | received: nothing"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(wait_for_told(&told, 2), [Told::Plan(1), Told::Plan(1)]);
    }

    #[test]
    fn ending_the_sampling_clears_the_report() {
        let (mut hardware, _told) = fan_hardware(u32::MAX);
        let mut providers = hardware.providers();
        hardware.set_fan_plan(some_plan(1));
        let mut values = Vec::new();
        providers[0].sample(&mut values);
        assert_eq!(hardware.fan_runtime(), manual_report());
        drop(providers);
        assert_eq!(hardware.fan_runtime(), Vec::<FanRuntime>::new());
    }

    #[test]
    fn the_worker_executable_is_expected_beside_the_service() {
        // No worker is started here; this pins where it is looked for.
        let expected = std::env::current_exe().unwrap().with_file_name(WORKER_EXECUTABLE);
        assert_eq!(expected.file_name().unwrap(), "syspilot-sensors.exe");
        assert_eq!(expected.parent(), std::env::current_exe().unwrap().parent());
    }
}
