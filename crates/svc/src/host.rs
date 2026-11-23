//! The service's event loop.
//!
//! One thread owns all state and sleeps in a single wait until something
//! happens: a client request, a registry change, a session or power event,
//! or the next thing that is due (a contested retry, a paused watch, a
//! sensor tick). With no client connected and nothing contested there is
//! nothing due, and the wait has no timeout.

use std::io;
use std::sync::Arc;
use std::sync::mpsc::SyncSender;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use syspilot_engine::enforcer::{StateStore, TriggerEvent};
use syspilot_hw::Hub;
use syspilot_proto::Instance;
use syspilot_proto::message::{
    DriverState, ErrorCode, Event as ClientEvent, Request, Response, ServiceError, ServiceStatus,
};
use syspilot_proto::sensor::{SectionWriter, SensorList};
use syspilot_proto::win::{MappedSection, SecurityDescriptor};
use windows_sys::Win32::System::Threading::{INFINITE, WaitForSingleObject};

use crate::core::{Core, Effects};
use crate::events::{Event, EventQueue};
use crate::hardware::{DRIVER_URL, Hardware};
use crate::logger::SensorLogger;
use crate::watch::WatchSet;

/// The sensor section: writable by the service, readable by local users.
/// The owner is explicit for the reason given at the pipe's descriptor.
const SECTION_SDDL: &str = "O:BAD:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GR;;;AU)";
const SAMPLE_PERIOD: Duration = Duration::from_secs(1);

/// A client that receives events.
struct Subscriber {
    id: u64,
    sensors: bool,
    sink: SyncSender<ClientEvent>,
}

/// Whether the loop keeps running after an event.
#[derive(PartialEq)]
enum Flow {
    Continue,
    Stop,
}

/// What the host needs besides the core.
pub struct HostParts {
    /// Which pipe and section names to use.
    pub instance: Instance,
    /// Whether this is a console run rather than a Windows service.
    pub console: bool,
    /// The Windows build number, for the status view.
    pub os_build: u32,
    /// Sensors and inventory.
    pub hardware: Box<dyn Hardware>,
    /// Where sensor logs go.
    pub logger: SensorLogger,
    /// The log directory as shown to the user.
    pub log_dir: String,
}

/// The running service.
pub struct Host<S> {
    parts: HostParts,
    queue: Arc<EventQueue>,
    core: Core<S>,
    watches: WatchSet,
    section: MappedSection,
    subscribers: Vec<Subscriber>,
    /// Open sensor providers. Exists only while someone may want values.
    hub: Option<Hub>,
    /// Counts sensor sets; clients re-read the list when it changes.
    generation: u32,
    /// When the next sample is due. `None` while nobody needs values.
    next_sample: Option<Instant>,
    /// The driver state the open providers were probed under.
    probed_driver: Option<DriverState>,
    logger_problem: Option<String>,
    started: Instant,
}

fn unix_millis() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_millis() as u64)
}

impl<S: StateStore + Clone + 'static> Host<S> {
    /// Creates the sensor section and the host. The pipe is not served yet;
    /// see [`crate::pipe::start`].
    pub fn new(core: Core<S>, queue: Arc<EventQueue>, parts: HostParts) -> io::Result<Self> {
        let security = SecurityDescriptor::from_sddl(SECTION_SDDL)?;
        let section = MappedSection::create(&parts.instance.sensor_section_name(), &security)?;
        Ok(Self {
            watches: WatchSet::new(queue.clone()),
            parts,
            queue,
            core,
            section,
            subscribers: Vec::new(),
            hub: None,
            generation: 0,
            next_sample: None,
            probed_driver: None,
            logger_problem: None,
            started: Instant::now(),
        })
    }

    /// Applies the profile, then serves events until told to stop.
    pub fn run(&mut self) {
        self.core.boot();
        self.parts.logger.configure(self.core.profile().logging.clone());
        self.settle();
        loop {
            while let Some(event) = self.queue.pop() {
                if self.handle(event) == Flow::Stop {
                    return self.shut_down();
                }
            }
            self.run_due(Instant::now());
            let timeout = self.time_until_next_wake(Instant::now());
            // Round up: waking a millisecond early would find nothing due
            // and loop once more for nothing.
            let timeout_ms = timeout.map_or(INFINITE, |wait| {
                wait.as_micros().div_ceil(1000).min(u128::from(INFINITE - 1)) as u32
            });
            // SAFETY: the queue's event handle is valid for as long as the
            // queue, which `self` keeps alive.
            unsafe { WaitForSingleObject(self.queue.signal(), timeout_ms) };
        }
    }

    fn shut_down(&mut self) {
        self.broadcast(ClientEvent::Stopping, false);
        self.subscribers.clear();
        self.parts.logger.close();
        self.stop_sampling();
        // The pipe outlives the loop until the process exits; requests
        // that still arrive must fail instead of waiting for an answer.
        self.queue.close();
    }

    fn handle(&mut self, event: Event) -> Flow {
        match event {
            Event::Stop => return Flow::Stop,
            Event::Request { request, elevated, reply } => {
                let (response, effects) = self.respond(*request, elevated);
                // The client may have gone; that is its business.
                let _ = reply.send(response);
                self.after_effects(effects);
                if effects.stop {
                    return Flow::Stop;
                }
            }
            Event::Subscribe { id, sensors, sink } => {
                self.subscribers.push(Subscriber { id, sensors, sink });
                self.settle();
            }
            Event::Unsubscribed(id) => {
                self.subscribers.retain(|subscriber| subscriber.id != id);
                self.settle();
            }
            Event::RegistryChanged(id) => {
                if let Some(spec) = self.watches.fired(id, Instant::now()) {
                    self.trigger(&TriggerEvent::Change(spec));
                }
            }
            Event::SessionLogon(session) => {
                if let Some(sid) = syspilot_engine::win::session_user_sid(session) {
                    self.trigger(&TriggerEvent::Logon { sid });
                }
            }
            // Buffered log rows should not be lost if the machine does not
            // come back.
            Event::Suspend => self.parts.logger.close(),
            Event::Resume => self.trigger(&TriggerEvent::Resume),
        }
        Flow::Continue
    }

    /// Runs an enforcement trigger and reports what it changed.
    fn trigger(&mut self, event: &TriggerEvent) {
        if self.core.enforcer().trigger(event) {
            self.broadcast(ClientEvent::StatesChanged, false);
        }
        self.sync_watches();
    }

    fn sync_watches(&mut self) {
        let wanted = self.core.enforcer().watches();
        self.watches.reconcile(&wanted);
    }

    fn respond(&mut self, request: Request, elevated: bool) -> (Response, Effects) {
        let response = match request {
            Request::Status => Response::Status(self.status()),
            Request::Sensors => Response::Sensors(self.sensor_list()),
            Request::SystemInfo => Response::SystemInfo(self.parts.hardware.system_info()),
            Request::Hello { .. } | Request::Subscribe { .. } => {
                // The pipe module answers these; reaching here is a bug in it.
                let message = "(request): expected the connection layer to handle this";
                Response::Error(ServiceError { code: ErrorCode::Invalid, message: message.into() })
            }
            request => return self.core.handle(request, elevated),
        };
        (response, Effects::default())
    }

    fn after_effects(&mut self, effects: Effects) {
        if effects.logging {
            self.parts.logger.configure(self.core.profile().logging.clone());
            self.logger_problem = None;
        }
        if effects.profile {
            self.broadcast(ClientEvent::ProfileChanged, false);
        }
        if effects.states {
            self.broadcast(ClientEvent::StatesChanged, false);
        }
        if effects != Effects::default() {
            self.settle();
        }
    }

    fn status(&mut self) -> ServiceStatus {
        let report = self.parts.hardware.driver();
        // A driver that appeared or went away changes which sensors exist.
        if self.hub.is_some() && self.probed_driver.as_ref() != Some(&report.state) {
            self.open_hub();
            self.broadcast(ClientEvent::SensorsChanged, false);
        }
        let mut problems = self.core.problems();
        problems.extend(self.logger_problem.clone());
        ServiceStatus {
            version: env!("CARGO_PKG_VERSION").into(),
            instance: self.parts.instance.name().into(),
            uptime_secs: self.started.elapsed().as_secs(),
            console: self.parts.console,
            driver: report.state,
            driver_url: DRIVER_URL.into(),
            features: report.features,
            subscribers: self.subscribers.len() as u32,
            sampling: self.next_sample.is_some(),
            os_build: self.parts.os_build,
            log_dir: self.parts.log_dir.clone(),
            fan_control: self.core.fan_control(),
            problems,
        }
    }

    fn sensor_list(&mut self) -> SensorList {
        if self.hub.is_none() {
            self.open_hub();
        }
        let list = self.hub.as_ref().map(|hub| hub.list().clone()).unwrap_or_default();
        // A list requested by a client that does not stay is not a reason
        // to keep the providers open.
        self.settle();
        list
    }

    /// Opens the sensor providers as a new sensor set.
    fn open_hub(&mut self) {
        self.probed_driver = Some(self.parts.hardware.driver().state);
        self.generation += 1;
        self.hub = Some(Hub::new(self.parts.hardware.providers(), self.generation));
    }

    /// Brings watches, providers and the sampling timer in line with what
    /// is currently needed. Cheap when nothing changed.
    fn settle(&mut self) {
        self.sync_watches();
        let wants_values = self.parts.logger.is_enabled()
            || self.subscribers.iter().any(|subscriber| subscriber.sensors);
        if wants_values {
            if self.hub.is_none() {
                self.open_hub();
            }
            if self.next_sample.is_none() {
                self.next_sample = Some(Instant::now());
            }
        } else {
            self.stop_sampling();
            // Connected clients may still ask for the list; keep it for them.
            if self.subscribers.is_empty() {
                self.hub = None;
            }
        }
    }

    fn stop_sampling(&mut self) {
        if self.next_sample.take().is_some() {
            // An empty sample tells readers the values they hold are stale.
            if let Ok(mut writer) = SectionWriter::new(self.section.words()) {
                writer.publish(0, self.generation, []);
            }
        }
    }

    /// Does whatever has become due: retries, resumed watches, a sample.
    fn run_due(&mut self, now: Instant) {
        if self.core.enforcer().time_until_deadline() == Some(Duration::ZERO) {
            self.trigger(&TriggerEvent::Timer);
        }
        for spec in self.watches.resume_due(now) {
            self.trigger(&TriggerEvent::Change(spec));
        }
        if self.next_sample.is_some_and(|due| due <= now) {
            self.sample(now);
        }
    }

    fn sample(&mut self, now: Instant) {
        let Some(hub) = self.hub.as_mut() else { return };
        hub.sample();
        if let Ok(mut writer) = SectionWriter::new(self.section.words()) {
            writer.publish(unix_millis(), self.generation, hub.samples());
        }
        let recorded = self.parts.logger.record(unix_millis() / 1000, hub.list(), hub.values());
        let problem = recorded.err().map(|error| format!("Sensor logging failed: {error}"));
        if problem.is_some() != self.logger_problem.is_some() {
            self.logger_problem = problem;
            self.broadcast(ClientEvent::StatusChanged, false);
        }
        self.broadcast(ClientEvent::Sampled, true);
        // Keep a steady one-second rhythm, but do not try to catch up after
        // a stall (sleep, a slow provider): skip to the next whole period.
        let due = self.next_sample.map_or(now, |due| due + SAMPLE_PERIOD);
        self.next_sample = Some(if due > now { due } else { now + SAMPLE_PERIOD });
        // The tick is also how a vanished sensor client is noticed.
        if self.subscribers.iter().all(|subscriber| !subscriber.sensors) {
            self.settle();
        }
    }

    /// Sends `event` to every subscriber, or only to those reading sensors.
    /// A subscriber that is gone or has stopped reading is dropped, which
    /// closes its connection.
    fn broadcast(&mut self, event: ClientEvent, sensors_only: bool) {
        self.subscribers.retain(|subscriber| {
            (sensors_only && !subscriber.sensors) || subscriber.sink.try_send(event).is_ok()
        });
    }

    fn time_until_next_wake(&mut self, now: Instant) -> Option<Duration> {
        let until = |at: Instant| at.saturating_duration_since(now);
        [
            self.core.enforcer().time_until_deadline(),
            self.watches.next_resume().map(until),
            self.next_sample.map(until),
        ]
        .into_iter()
        .flatten()
        .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::FakeHardware;
    use crate::pipe;
    use crate::privilege::is_process_elevated;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread::JoinHandle;
    use syspilot_engine::fake::FakeOs;
    use syspilot_engine::os::{Hive, RegValue, reg_type};
    use syspilot_engine::store::MemoryStateStore;
    use syspilot_proto::catalog::Catalog;
    use syspilot_proto::profile::LoggingConfig;
    use syspilot_proto::sensor::SectionReader;
    use syspilot_proto::{Client, EventStream};

    const KEY: &str = r"SOFTWARE\SysPilot-Test\Host";
    const CATALOG: &str = r#"
[[tweak]]
id = "other.scratch"
title = "Scratch value"
summary = "Sets a scratch value."
level = "simple"
category = "other"
source = "https://learn.microsoft.com/windows/"
[[tweak.action]]
registry = { scope = "machine", key = 'SOFTWARE\SysPilot-Test\Host', name = "Value", data = { dword = 1 } }
"#;

    static NEXT_INSTANCE: AtomicUsize = AtomicUsize::new(0);

    /// A host over a fake OS and fake hardware, served on a real pipe and
    /// section under a unique test instance.
    struct RunningHost {
        instance: Instance,
        queue: Arc<EventQueue>,
        fake: FakeOs,
        hardware: FakeHardware,
        log_dir: PathBuf,
        thread: Option<JoinHandle<()>>,
    }

    impl RunningHost {
        fn start() -> Self {
            assert!(
                is_process_elevated(),
                "(test environment): expected an elevated process, which the Global\\ sensor section requires | received: a standard user"
            );
            let name = format!(
                "SysPilot-Test.host-{}-{}",
                std::process::id(),
                NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed)
            );
            let instance = Instance::named(&name).unwrap();
            let queue = EventQueue::new().unwrap();
            let fake = FakeOs::default();
            let hardware = FakeHardware::default();
            let log_dir = std::env::temp_dir().join(&name);
            std::fs::create_dir_all(&log_dir).unwrap();

            pipe::start(&instance, queue.clone()).unwrap();
            // The host is built on the thread that runs it: it is not `Send`.
            let (ready, started) = std::sync::mpsc::channel();
            let thread = std::thread::spawn({
                let (instance, queue, fake) = (instance.clone(), queue.clone(), fake.clone());
                let (hardware, log_dir) = (hardware.clone(), log_dir.clone());
                move || {
                    let catalog = Catalog::parse(CATALOG).unwrap();
                    let store = MemoryStateStore::default();
                    let core = Core::new(fake.os(), store, catalog, 26100, true);
                    let parts = HostParts {
                        instance,
                        console: true,
                        os_build: 26100,
                        hardware: Box::new(hardware),
                        logger: SensorLogger::new(log_dir.clone(), LoggingConfig::default()),
                        log_dir: log_dir.display().to_string(),
                    };
                    let mut host = Host::new(core, queue, parts).unwrap();
                    ready.send(()).unwrap();
                    host.run();
                }
            });
            started.recv().unwrap();
            Self { instance, queue, fake, hardware, log_dir, thread: Some(thread) }
        }

        fn client(&self) -> Client {
            Client::connect(&self.instance).unwrap()
        }

        fn subscribe(&self, sensors: bool) -> EventStream {
            self.client().subscribe(sensors).unwrap()
        }

        fn value(&self) -> Option<RegValue> {
            self.fake.registry.value(&Hive::Machine, KEY, "Value")
        }
    }

    impl Drop for RunningHost {
        fn drop(&mut self) {
            self.queue.push(Event::Stop);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
            let _ = std::fs::remove_dir_all(&self.log_dir);
        }
    }

    fn dword(value: u32) -> RegValue {
        RegValue { kind: reg_type::DWORD, data: value.to_le_bytes().to_vec() }
    }

    /// Reads events until `wanted` arrives; other events are skipped.
    fn wait_for(events: &mut EventStream, wanted: ClientEvent) {
        for _ in 0..50 {
            if events.next_event().unwrap() == wanted {
                return;
            }
        }
        panic!("expected the event {wanted:?} | received: 50 other events");
    }

    fn enable_scratch(client: &mut Client) {
        client.command(&Request::SetTweak { id: "other.scratch".into(), enabled: true }).unwrap();
    }

    #[test]
    fn status_describes_the_service() {
        let host = RunningHost::start();
        let status = host.client().status().unwrap();
        assert_eq!(status.instance, host.instance.name());
        assert_eq!(status.version, env!("CARGO_PKG_VERSION"));
        assert!(status.console);
        assert_eq!(status.driver, DriverState::Missing);
        assert_eq!(status.driver_url, "https://pawnio.eu/");
        assert_eq!(status.features.len(), 1);
        assert!(!status.features[0].available, "driver features are off without the driver");
        assert!(!status.sampling, "nobody asked for sensor values");
        assert_eq!(status.problems, Vec::<String>::new());
    }

    #[test]
    fn enabling_a_tweak_applies_it_and_notifies_subscribers() {
        let host = RunningHost::start();
        let mut events = host.subscribe(false);
        let mut client = host.client();
        enable_scratch(&mut client);
        assert_eq!(host.value(), Some(dword(1)));
        wait_for(&mut events, ClientEvent::ProfileChanged);
        wait_for(&mut events, ClientEvent::StatesChanged);
        let states = client.states().unwrap();
        assert!(states[0].enabled);
    }

    #[test]
    fn sampling_runs_only_while_a_sensor_subscriber_is_connected() {
        let host = RunningHost::start();
        let mut observer = host.subscribe(false);
        assert!(!host.client().status().unwrap().sampling);
        assert_eq!(host.hardware.probes(), 0, "no provider is opened for a plain subscriber");

        let mut sensors = host.subscribe(true);
        wait_for(&mut sensors, ClientEvent::Sampled);
        let mut client = host.client();
        assert!(client.status().unwrap().sampling);
        let list = client.sensors().unwrap();
        assert_eq!(list.sensors.len(), 1);
        assert_eq!(list.sensors[0].key, "cpu.temp");

        let section = MappedSection::open(&host.instance.sensor_section_name()).unwrap();
        let reader = SectionReader::new(section.words()).unwrap();
        let mut samples = Vec::new();
        let header = reader.read(&mut samples).unwrap();
        assert_eq!(header.generation, list.generation);
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].value, Some(42.5));

        // The sensor client leaves: the next tick notices and stops.
        drop(sensors);
        let stopped = (0..100).any(|_| {
            std::thread::sleep(Duration::from_millis(50));
            !client.status().unwrap().sampling
        });
        assert!(stopped, "sampling continued without a sensor subscriber");
        let header = reader.read(&mut samples).unwrap();
        assert!(samples.is_empty(), "stale values were left in the section");
        assert_eq!(header.timestamp_ms, 0);
        assert_eq!(host.hardware.probes(), 1);
        // The plain subscriber saw none of the sensor ticks.
        drop(client);
        enable_scratch(&mut host.client());
        assert_eq!(observer.next_event().unwrap(), ClientEvent::ProfileChanged);
    }

    #[test]
    fn a_driver_that_appears_adds_its_sensors() {
        let host = RunningHost::start();
        let mut sensors = host.subscribe(true);
        wait_for(&mut sensors, ClientEvent::Sampled);
        let mut client = host.client();
        let before = client.sensors().unwrap();
        assert_eq!(before.sensors.len(), 1);

        host.hardware.set_driver(DriverState::Ready { version: "2.2.0".into() });
        let status = client.status().unwrap();
        assert!(status.features[0].available);
        wait_for(&mut sensors, ClientEvent::SensorsChanged);
        let after = client.sensors().unwrap();
        assert_eq!(after.sensors.len(), 2);
        assert!(after.sensors[1].key == "board.fan1");
        assert_ne!(after.generation, before.generation, "a new sensor set has a new generation");
    }

    #[test]
    fn logging_keeps_sampling_without_any_client() {
        let host = RunningHost::start();
        let logging = LoggingConfig { enabled: true, interval_secs: 1, ..LoggingConfig::default() };
        let mut client = host.client();
        client.command(&Request::SetLogging(logging)).unwrap();
        assert!(client.status().unwrap().sampling);
        client.command(&Request::SetLogging(LoggingConfig::default())).unwrap();
        assert!(!client.status().unwrap().sampling);

        let logs: Vec<_> = std::fs::read_dir(&host.log_dir).unwrap().flatten().collect();
        assert_eq!(logs.len(), 1, "one log file was written");
        let text = std::fs::read_to_string(logs[0].path()).unwrap();
        assert!(text.starts_with("time,cpu.temp\n"), "{text}");
        assert!(text.trim_end().ends_with(",42.5"), "{text}");
    }

    #[test]
    fn stopping_tells_subscribers_and_ends_the_loop() {
        let mut host = RunningHost::start();
        let mut events = host.subscribe(false);
        host.client().command(&Request::Shutdown).unwrap();
        wait_for(&mut events, ClientEvent::Stopping);
        host.thread.take().unwrap().join().unwrap();
    }

    #[test]
    fn an_ended_loop_whose_pipe_lingers_is_not_an_active_console_run() {
        let mut host = RunningHost::start();
        assert!(crate::install::console_run_is_active(&host.instance));

        // The loop ends but the process, and with it the pipe, lives on:
        // the state of a service that has just reported itself stopped.
        host.queue.push(Event::Stop);
        host.thread.take().unwrap().join().unwrap();
        assert!(Client::connect(&host.instance).is_ok(), "the pipe still accepts connections");
        assert!(!crate::install::console_run_is_active(&host.instance));
    }

    #[test]
    fn standard_requests_reach_the_core() {
        let host = RunningHost::start();
        let mut client = host.client();
        assert_eq!(client.catalog().unwrap().tweaks.len(), 1);
        assert_eq!(client.system_info().unwrap().summary.computer, "FAKE-PC");
        assert_eq!(client.profile().unwrap().enabled, Vec::<String>::new());
    }
}
