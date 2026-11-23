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
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

use syspilot_hw::{Provider, SensorSpec};
use syspilot_proto::frame::read_frame;
use syspilot_proto::message::{DriverState, SystemInfo};
use syspilot_proto::sensor::SensorList;
use syspilot_proto::worker::{Probed, SAMPLE_REQUEST, WORKER_EXECUTABLE, WorkerFrame};

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
/// `CREATE_NO_WINDOW`: a console service must not flash a window.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

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

/// Starts the worker executable that sits next to the service's. It is
/// addressed by its full path, never searched for, so nothing else can be
/// run in its place.
pub struct ProcessSpawner;

impl WorkerSpawner for ProcessSpawner {
    fn spawn(&self) -> io::Result<WorkerLink> {
        use std::os::windows::process::CommandExt;
        let executable = std::env::current_exe()?.with_file_name(WORKER_EXECUTABLE);
        let mut process = Command::new(executable)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()?;
        let missing = || io::Error::other("the worker has no standard streams");
        let requests = process.stdin.take().ok_or_else(missing)?;
        let frames = process.stdout.take().ok_or_else(missing)?;
        Ok(WorkerLink {
            requests: Box::new(requests),
            frames: Box::new(frames),
            process: Some(process),
        })
    }
}

/// A running worker as the service holds it.
struct Worker {
    /// `None` once the worker was told to end.
    requests: Option<Box<dyn Write + Send>>,
    /// Frames, read by a thread so that waiting for one can time out.
    frames: Receiver<WorkerFrame>,
    process: Option<Child>,
}

impl Worker {
    fn start(link: WorkerLink) -> Self {
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
        Self { requests: Some(link.requests), frames, process: link.process }
    }

    /// Waits for the worker's report of what it found.
    fn probed(&mut self) -> Result<Probed, String> {
        match self.frames.recv_timeout(PROBE_TIMEOUT) {
            Ok(WorkerFrame::Probed(probed)) => Ok(*probed),
            Ok(WorkerFrame::Sample(_)) => {
                Err("(sensor worker): expected its report first | received: a sample".into())
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
            Ok(WorkerFrame::Sample(_)) | Err(TryRecvError::Disconnected) => Some(None),
            Err(TryRecvError::Empty) => None,
        }
    }

    /// Asks for one sample. `None` when the worker is gone or too slow.
    fn sample(&mut self) -> Option<Vec<Option<f64>>> {
        let requests = self.requests.as_mut()?;
        requests.write_all(&[SAMPLE_REQUEST]).and_then(|()| requests.flush()).ok()?;
        match self.frames.recv_timeout(SAMPLE_TIMEOUT) {
            Ok(WorkerFrame::Sample(values)) => Some(values),
            _ => None,
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
}

impl WorkerProvider {
    fn new(
        worker: Worker,
        sensors: &SensorList,
        spawner: Arc<dyn WorkerSpawner>,
        restart_delay: Duration,
    ) -> Self {
        let needs_driver = sensors.sensors.iter().map(|sensor| sensor.driver).collect();
        Self {
            link: Link::Live(worker),
            spawner,
            restart_delay,
            specs: specs_of(sensors),
            needs_driver,
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
                _ => (Self::down(self.restart_delay), None),
            },
            Link::Down { retry_at, delay } if Instant::now() >= retry_at => {
                match self.spawner.spawn() {
                    Ok(link) => {
                        let worker = Worker::start(link);
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
}

impl WorkerHardware {
    /// Hardware read by workers from `spawner`.
    pub fn new(spawner: Arc<dyn WorkerSpawner>, installed_driver: fn() -> Option<String>) -> Self {
        Self { spawner, restart_delay: RESTART_DELAY, installed_driver, report: None }
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
        let mut worker = Worker::start(link);
        let probed = worker.probed()?;
        Ok((worker, probed))
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
        let installed = (self.installed_driver)();
        let current = self.report.as_ref().is_some_and(|(seen, _)| *seen == installed);
        if !current {
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

    fn providers(&mut self) -> Vec<Box<dyn Provider>> {
        match self.start() {
            Ok((worker, probed)) => {
                let provider = WorkerProvider::new(
                    worker,
                    &probed.sensors,
                    self.spawner.clone(),
                    self.restart_delay,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use syspilot_hw::Hub;
    use syspilot_proto::frame::write_frame;
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
        let mut worker = Worker::start(link);
        worker.probed().unwrap();
        drop(worker);
        // The fake returns when its input ends; a join that never returns
        // would fail the test by timing out.
        worker_thread.join().unwrap();
    }

    #[test]
    fn the_worker_executable_is_expected_beside_the_service() {
        // No worker is started here; this pins where it is looked for.
        let expected = std::env::current_exe().unwrap().with_file_name(WORKER_EXECUTABLE);
        assert_eq!(expected.file_name().unwrap(), "syspilot-sensors.exe");
        assert_eq!(expected.parent(), std::env::current_exe().unwrap().parent());
    }
}
