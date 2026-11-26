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
    DriverState, ErrorCode, Event as ClientEvent, FanAvailability, Request, Response, ServiceError,
    ServiceStatus,
};
use syspilot_proto::profile::FanCurve;
use syspilot_proto::sensor::{SectionWriter, SensorList};
use syspilot_proto::win::{MappedSection, SecurityDescriptor};
use syspilot_proto::worker::FanPlan;
use windows_sys::Win32::System::Threading::{INFINITE, WaitForSingleObject};

use crate::core::{self, Core, Effects};
use crate::events::{Event, EventQueue};
use crate::fans;
use crate::hardware::{DRIVER_URL, Hardware};
use crate::logger::SensorLogger;
use crate::updates::{Updater, Updates};
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
    /// Where the fan controller keeps the BIOS registers it saved before its
    /// first write, so that a crash cannot lose them. A directory only the
    /// service's own account and administrators can write.
    pub fan_store_directory: String,
    /// What looks for and installs a new version. `None` for a copy that
    /// does not update itself: a console run or a test instance.
    pub updater: Option<Box<dyn Updater>>,
    /// Where Windows Installer logs an update, as shown to the user.
    pub update_log: String,
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
    /// Requests that need a look at the machine that is still under way.
    /// They are answered when it ends; the loop does not wait for it.
    waiting: Vec<(Request, bool, Reply)>,
    /// Whether the hardware said "later" to something since the last check.
    look_pending: bool,
    started: Instant,
    /// The curves the hardware was last told to follow; `None` until the
    /// first plan, which a service that accepts curves sends even when it
    /// has none, so that a worker hands back what a crash left (see
    /// [`FanPlan`]). A service that does not accept curves sends no plan.
    fan_plan: Option<Vec<FanCurve>>,
    /// What clients were last told about the fan controller.
    fan_watch: fans::Watch,
    /// When and whether SysPilot updates itself.
    updates: Updates,
}

/// Where a request's answer goes.
type Reply = SyncSender<Response>;

/// How often a look at the machine that is under way is checked on.
const LOOK_POLL: Duration = Duration::from_millis(25);

fn unix_millis() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_millis() as u64)
}

impl<S: StateStore + Clone + 'static> Host<S> {
    /// Creates the sensor section and the host. The pipe is not served yet;
    /// see [`crate::pipe::start`].
    pub fn new(core: Core<S>, queue: Arc<EventQueue>, mut parts: HostParts) -> io::Result<Self> {
        let security = SecurityDescriptor::from_sddl(SECTION_SDDL)?;
        let section = MappedSection::create(&parts.instance.sensor_section_name(), &security)?;
        let updates = Updates::new(
            parts.updater.take(),
            core.profile().updates,
            std::mem::take(&mut parts.update_log),
            Instant::now(),
        );
        Ok(Self {
            updates,
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
            waiting: Vec::new(),
            look_pending: false,
            started: Instant::now(),
            fan_plan: None,
            fan_watch: fans::Watch::default(),
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
        // Dropping the parked requests fails them for their clients.
        self.waiting.clear();
        self.parts.hardware.release();
        self.parts.logger.close();
        self.stop_sampling();
        // Ends the sensor worker and waits for it. An installer replaces
        // the worker's file as soon as the service reports itself stopped,
        // which it cannot while the worker still runs.
        self.hub = None;
        // The pipe outlives the loop until the process exits; requests
        // that still arrive must fail instead of waiting for an answer.
        self.queue.close();
    }

    fn handle(&mut self, event: Event) -> Flow {
        match event {
            Event::Stop => return Flow::Stop,
            Event::Request { request, elevated, reply } => {
                return self.answer_or_park(*request, elevated, reply);
            }
            Event::Subscribe { id, sensors, sink } => {
                self.subscribers.push(Subscriber { id, sensors, sink });
                self.settle();
            }
            Event::Unsubscribed(id) => {
                self.subscribers.retain(|subscriber| subscriber.id != id);
                self.settle();
                // The app closing is what an automatic update waits for.
                let clients = self.subscribers.len();
                self.update_step(|updates| updates.clients_changed(clients));
            }
            Event::UpdateFinished { job, outcome } => {
                let (clients, now) = (self.subscribers.len(), Instant::now());
                self.update_step(|updates| {
                    updates.finished(job, outcome, clients, now, unix_millis() / 1000);
                });
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
            // come back, and the fans go back to the BIOS before it sleeps.
            Event::Suspend => {
                self.parts.logger.close();
                self.parts.hardware.set_fans_suspended(true);
            }
            Event::Resume => {
                self.parts.hardware.set_fans_suspended(false);
                self.trigger(&TriggerEvent::Resume);
            }
        }
        Flow::Continue
    }

    /// Answers a request, unless it needs a look at the machine that is
    /// still under way: then it is parked and answered when the look ends.
    /// The client waits either way, but the loop stays free to enforce.
    fn answer_or_park(&mut self, request: Request, elevated: bool, reply: Reply) -> Flow {
        let must_wait = match request {
            Request::Status | Request::SystemInfo => self.parts.hardware.report_pending(),
            Request::Sensors => self.hub.is_none() && self.parts.hardware.providers_pending(),
            _ => false,
        };
        if must_wait {
            self.look_pending = true;
            self.waiting.push((request, elevated, reply));
            return Flow::Continue;
        }
        let (response, effects) = self.respond(request, elevated);
        // The client may have gone; that is its business.
        let _ = reply.send(response);
        self.after_effects(effects);
        if effects.stop { Flow::Stop } else { Flow::Continue }
    }

    /// Takes up what was put off for a look at the machine. Whatever still
    /// has to wait says so again and is checked at the next poll.
    fn resume_after_look(&mut self) {
        self.look_pending = false;
        for (request, elevated, reply) in std::mem::take(&mut self.waiting) {
            // None of the requests that can be parked stops the service.
            let _ = self.answer_or_park(request, elevated, reply);
        }
        self.settle();
        if !self.look_pending {
            self.parts.hardware.release();
        }
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

    /// Lets `step` act on the updates and tells subscribers when what they
    /// are shown about them has changed.
    fn update_step(&mut self, step: impl FnOnce(&mut Updates)) {
        let before = self.updates.status();
        step(&mut self.updates);
        if self.updates.status() != before {
            self.broadcast(ClientEvent::StatusChanged, false);
        }
    }

    /// Looks for a new version or installs it, on a client's request.
    fn update_on_request(&mut self, install: bool) -> Response {
        let mut asked = Ok(());
        self.update_step(|updates| {
            asked = if install { updates.install_now() } else { updates.check_now() };
        });
        match asked {
            Ok(()) => Response::Ok,
            Err(message) => Response::Error(ServiceError {
                code: ErrorCode::Unsupported,
                message: message.into(),
            }),
        }
    }

    fn respond(&mut self, request: Request, elevated: bool) -> (Response, Effects) {
        if let Some(refused) = core::refusal(&request, elevated) {
            return (refused, Effects::default());
        }
        let response = match request {
            Request::CheckForUpdate => self.update_on_request(false),
            Request::InstallUpdate => self.update_on_request(true),
            Request::Status => Response::Status(Box::new(self.status())),
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
        if effects.updates {
            let (mode, clients) = (self.core.profile().updates, self.subscribers.len());
            self.update_step(|updates| updates.set_mode(mode, clients, Instant::now()));
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
            // Reopened by `settle`, at once or when the look has ended; a
            // client that asks for the list in between waits for it.
            self.hub = None;
            self.stop_sampling();
            self.settle();
            self.broadcast(ClientEvent::SensorsChanged, false);
        }
        let mut problems = self.core.problems();
        problems.extend(self.logger_problem.clone());
        let layout = self.parts.hardware.fan_layout();
        let runtime = self.parts.hardware.fan_runtime();
        let fans =
            fans::merge(&layout, &self.core.profile().fans, &runtime, self.core.fan_control());
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
            fans,
            update: self.updates.status(),
            tuning: Default::default(),
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
        let curves = self.fan_curves_to_follow();
        self.sync_fan_plan(&curves);
        // A curve needs the worker and its sensors even when nobody looks.
        let wants_values = self.parts.logger.is_enabled()
            || self.subscribers.iter().any(|subscriber| subscriber.sensors)
            || !curves.is_empty();
        if wants_values {
            if self.hub.is_none() {
                if self.parts.hardware.providers_pending() {
                    // Taken up again by `resume_after_look`.
                    self.look_pending = true;
                    return;
                }
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
        // Dropping the hub ended the worker and with it the controller.
        self.poll_fans();
    }

    /// The enabled curves, when this service accepts them and the board may
    /// follow them. Empty is the idle state: no worker is kept for the fans.
    /// Before the first look at the machine the board is not known, and the
    /// curves count, so that the look happens.
    fn fan_curves_to_follow(&mut self) -> Vec<FanCurve> {
        let curves = fans::enabled_curves(self.core.profile());
        if curves.is_empty() || !self.core.fan_control() {
            return Vec::new();
        }
        match self.parts.hardware.known_fan_layout() {
            Some(layout) if layout.availability != FanAvailability::Controllable => Vec::new(),
            _ => curves,
        }
    }

    /// Hands the hardware the curves when they differ from the last ones.
    /// The first plan goes out even when it has no curve: the worker that
    /// receives it hands back any channel a crashed worker left in manual
    /// mode, and a plan with no curve is how it learns where the record is.
    fn sync_fan_plan(&mut self, curves: &[FanCurve]) {
        if !self.core.fan_control() || self.fan_plan.as_deref() == Some(curves) {
            return;
        }
        self.fan_plan = Some(curves.to_vec());
        let plan = FanPlan {
            store_directory: self.parts.fan_store_directory.clone(),
            curves: curves.to_vec(),
        };
        self.parts.hardware.set_fan_plan(plan);
    }

    /// Looks at what the controller reports and tells subscribers when
    /// something they show has changed.
    fn poll_fans(&mut self) {
        let runtime = self.parts.hardware.fan_runtime();
        if self.fan_watch.changed(&runtime) {
            self.broadcast(ClientEvent::StatusChanged, false);
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
        if self.look_pending {
            self.resume_after_look();
        }
        if self.core.enforcer().time_until_deadline() == Some(Duration::ZERO) {
            self.trigger(&TriggerEvent::Timer);
        }
        for spec in self.watches.resume_due(now) {
            self.trigger(&TriggerEvent::Change(spec));
        }
        if self.next_sample.is_some_and(|due| due <= now) {
            self.sample(now);
        }
        self.update_step(|updates| updates.run_due(now));
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
        self.poll_fans();
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
            self.look_pending.then_some(LOOK_POLL),
            self.updates.next_wake(now),
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
    use crate::updates::{FakeUpdater, Job};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread::JoinHandle;
    use syspilot_engine::fake::FakeOs;
    use syspilot_engine::os::{Hive, RegValue, reg_type};
    use syspilot_engine::store::MemoryStateStore;
    use syspilot_proto::catalog::Catalog;
    use syspilot_proto::message::{FanChannelStatus, FanPhase, FanRuntime, FanStatus};
    use syspilot_proto::message::{UpdateState, UpdateStatus};
    use syspilot_proto::profile::LoggingConfig;
    use syspilot_proto::profile::UpdateMode;
    use syspilot_proto::sensor::SectionReader;
    use syspilot_proto::update::UpdaterReport;
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

    /// The directory the host hands the controller; the fake hardware only
    /// records it.
    const FAN_STORE: &str = r"C:\fake\SysPilot\state";
    /// Where the fake updater's installer would log.
    const UPDATE_LOG: &str = r"C:\fake\SysPilot\updates\install.log";

    static NEXT_INSTANCE: AtomicUsize = AtomicUsize::new(0);

    /// A host over a fake OS and fake hardware, served on a real pipe and
    /// section under a unique test instance.
    struct RunningHost {
        instance: Instance,
        queue: Arc<EventQueue>,
        fake: FakeOs,
        hardware: FakeHardware,
        updater: FakeUpdater,
        log_dir: PathBuf,
        thread: Option<JoinHandle<()>>,
    }

    impl RunningHost {
        fn start() -> Self {
            Self::start_with(false)
        }

        /// `fan_curves`: whether the service accepts fan curves, as a build
        /// with the `fan-control` feature does. The hardware is a fake either
        /// way.
        fn start_with(fan_curves: bool) -> Self {
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
            let updater = FakeUpdater::default();
            let log_dir = std::env::temp_dir().join(&name);
            std::fs::create_dir_all(&log_dir).unwrap();

            pipe::start(&instance, queue.clone()).unwrap();
            // The host is built on the thread that runs it: it is not `Send`.
            let (ready, started) = std::sync::mpsc::channel();
            let thread = std::thread::spawn({
                let (instance, queue, fake) = (instance.clone(), queue.clone(), fake.clone());
                let (hardware, log_dir) = (hardware.clone(), log_dir.clone());
                let updater = updater.clone();
                move || {
                    let catalog = Catalog::parse(CATALOG).unwrap();
                    let store = MemoryStateStore::default();
                    let mut core = Core::new(fake.os(), store, catalog, 26100, true);
                    if fan_curves {
                        core.accept_fan_curves_in_tests();
                    }
                    let parts = HostParts {
                        instance,
                        console: true,
                        os_build: 26100,
                        hardware: Box::new(hardware),
                        logger: SensorLogger::new(log_dir.clone(), LoggingConfig::default()),
                        log_dir: log_dir.display().to_string(),
                        fan_store_directory: FAN_STORE.into(),
                        updater: Some(Box::new(updater)),
                        update_log: UPDATE_LOG.into(),
                    };
                    let mut host = Host::new(core, queue, parts).unwrap();
                    ready.send(()).unwrap();
                    host.run();
                }
            });
            started.recv().unwrap();
            Self { instance, queue, fake, hardware, updater, log_dir, thread: Some(thread) }
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
    fn a_look_at_the_machine_parks_its_requests_but_not_enforcement() {
        let host = RunningHost::start();
        host.hardware.set_looking(true);
        let (answered, answers) = std::sync::mpsc::channel();
        let asking: Vec<_> = [Request::Status, Request::Sensors, Request::SystemInfo]
            .into_iter()
            .map(|request| {
                let (mut client, answered) = (host.client(), answered.clone());
                std::thread::spawn(move || {
                    let response = client.request(&request).unwrap();
                    answered.send(std::mem::discriminant(&response)).unwrap();
                })
            })
            .collect();

        // Meanwhile the loop serves everything else, including a sensor
        // subscriber that cannot get values yet.
        let _sensors = host.subscribe(true);
        let mut client = host.client();
        enable_scratch(&mut client);
        assert_eq!(host.value(), Some(dword(1)));
        assert!(client.states().unwrap()[0].enabled);
        assert_eq!(
            answers.try_recv().ok(),
            None,
            "(host): expected hardware requests to wait for the look | received: an answer"
        );
        assert_eq!(host.hardware.probes(), 0, "no provider is opened during the look");

        host.hardware.set_looking(false);
        for thread in asking {
            thread.join().unwrap();
        }
        assert_eq!(answers.try_iter().count(), 3, "every parked request was answered");
        let status = client.status().unwrap();
        assert!(status.sampling, "the subscriber's sampling started once the look ended");
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

    fn curve_on(pwm: u8) -> FanCurve {
        FanCurve {
            channel: format!("board.pwm{pwm}"),
            enabled: true,
            sources: vec!["cpu.temp".into()],
            points: vec![
                syspilot_proto::profile::CurvePoint(30, 20),
                syspilot_proto::profile::CurvePoint(80, 100),
            ],
            hysteresis_c: 2,
            min_duty_pct: 0,
            ramp_pct_per_sec: 10,
        }
    }

    fn board_layout(availability: FanAvailability) -> FanStatus {
        FanStatus {
            availability,
            board: "ACME B550".into(),
            channels: vec![FanChannelStatus {
                channel: "board.pwm1".into(),
                label: "CPU fan".into(),
                tach_sensor: Some("board.fan1".into()),
                min_duty_pct: 20,
                ..FanChannelStatus::default()
            }],
        }
    }

    fn manual_at(duty: u8) -> Vec<FanRuntime> {
        vec![FanRuntime {
            channel: "board.pwm1".into(),
            phase: FanPhase::Manual,
            target_duty_pct: Some(duty),
            problem: None,
        }]
    }

    fn plan_of(curves: Vec<FanCurve>) -> FanPlan {
        FanPlan { store_directory: FAN_STORE.into(), curves }
    }

    #[test]
    fn a_service_without_curves_sends_one_empty_plan_so_a_worker_can_hand_back_a_crash() {
        let host = RunningHost::start_with(true);
        host.hardware.set_fan_layout(board_layout(FanAvailability::Controllable));
        let mut client = host.client();
        enable_scratch(&mut client);
        assert!(!client.status().unwrap().sampling, "no worker is kept for the fans");
        let _sensors = host.subscribe(true);
        assert!(client.status().unwrap().sampling);
        assert_eq!(
            host.hardware.fan_plans(),
            [plan_of(Vec::new())],
            "(fan plan): expected one plan with no curve, which names the store | received"
        );
        assert!(!client.status().unwrap().fans.channels[0].active);
    }

    // A build with the feature always accepts curves, so only a default build
    // can be the service that does not.
    #[cfg(not(feature = "fan-control"))]
    #[test]
    fn a_service_that_does_not_accept_curves_never_mentions_fans_to_the_hardware() {
        let host = RunningHost::start();
        host.hardware.set_fan_layout(board_layout(FanAvailability::Controllable));
        let mut client = host.client();
        enable_scratch(&mut client);
        let _sensors = host.subscribe(true);
        assert!(client.status().unwrap().sampling);
        assert_eq!(host.hardware.fan_plans(), Vec::<FanPlan>::new());
    }

    #[test]
    fn curves_on_a_controllable_board_keep_sampling_and_reach_the_hardware_once() {
        let host = RunningHost::start_with(true);
        host.hardware.set_fan_layout(board_layout(FanAvailability::Controllable));
        let mut client = host.client();
        client.command(&Request::SetFans(vec![curve_on(1)])).unwrap();
        // No sensor client at all, and still the worker is kept running.
        assert!(client.status().unwrap().sampling, "a curve needs sensor values");
        assert_eq!(
            host.hardware.fan_plans(),
            [plan_of(Vec::new()), plan_of(vec![curve_on(1)])],
            "the empty first plan, then the curves"
        );

        // An unrelated change does not repeat the plan.
        enable_scratch(&mut client);
        assert_eq!(host.hardware.fan_plans().len(), 2, "the same curves are not sent again");

        let channel = &client.status().unwrap().fans.channels[0];
        assert!(channel.active);
        assert_eq!(channel.curve.len(), 2);

        // No curves: the hardware is told to let go and sampling ends.
        client.command(&Request::SetFans(Vec::new())).unwrap();
        // The loop answers a command before it settles; a request after it
        // is served after the settling.
        let sampling = client.status().unwrap().sampling;
        let plans = host.hardware.fan_plans();
        assert_eq!(plans.len(), 3);
        assert!(plans[2].curves.is_empty(), "{plans:?}");
        assert!(!sampling, "nothing needs values any more");
    }

    #[test]
    fn curves_on_a_read_only_board_keep_no_worker_and_say_why() {
        let host = RunningHost::start_with(true);
        let reason = "the board's entry has not been validated".to_owned();
        host.hardware
            .set_fan_layout(board_layout(FanAvailability::ReadOnly { reason: reason.clone() }));
        let mut client = host.client();
        client.command(&Request::SetFans(vec![curve_on(1)])).unwrap();
        let status = client.status().unwrap();
        assert!(!status.sampling, "a board that cannot follow curves needs no worker");
        assert_eq!(
            host.hardware.fan_plans(),
            [plan_of(Vec::new())],
            "(fan plan): expected no curve to reach a board that cannot follow them"
        );
        assert_eq!(status.fans.availability, FanAvailability::ReadOnly { reason });
        let channel = &status.fans.channels[0];
        assert!(!channel.active);
        assert_eq!(channel.curve.len(), 2, "the curve is still shown");
        assert_eq!(channel.phase, FanPhase::Bios);
    }

    #[test]
    fn what_the_controller_reports_reaches_the_status_and_the_subscribers() {
        let host = RunningHost::start_with(true);
        host.hardware.set_fan_layout(board_layout(FanAvailability::Controllable));
        let mut events = host.subscribe(false);
        let mut client = host.client();
        client.command(&Request::SetFans(vec![curve_on(1)])).unwrap();
        wait_for(&mut events, ClientEvent::ProfileChanged);

        host.hardware.set_fan_runtime(manual_at(40));
        // The next sample (every second) polls the controller.
        wait_for(&mut events, ClientEvent::StatusChanged);
        let channel = &client.status().unwrap().fans.channels[0];
        assert_eq!((channel.phase, channel.target_duty_pct), (FanPhase::Manual, Some(40)));

        // A source that fails: full speed, with the reason.
        let failing = FanRuntime {
            channel: "board.pwm1".into(),
            phase: FanPhase::FailSafe,
            target_duty_pct: Some(100),
            problem: Some("cpu.temp has no reading".into()),
        };
        host.hardware.set_fan_runtime(vec![failing]);
        wait_for(&mut events, ClientEvent::StatusChanged);
        let channel = &client.status().unwrap().fans.channels[0];
        assert_eq!(channel.phase, FanPhase::FailSafe);
        assert_eq!(channel.problem.as_deref(), Some("cpu.temp has no reading"));
    }

    /// What the fake updater reports back, as the real one does through the
    /// queue.
    fn finish(host: &RunningHost, job: Job, report: UpdaterReport) {
        host.queue.push(Event::UpdateFinished { job, outcome: Ok(report) });
    }

    #[test]
    fn a_look_on_request_shows_the_new_version_and_installing_is_the_users_call() {
        let host = RunningHost::start();
        let mut events = host.subscribe(false);
        let mut client = host.client();
        let fresh = client.status().unwrap().update;
        assert_eq!(fresh, UpdateStatus::default(), "(update status): expected a quiet start");

        client.command(&Request::CheckForUpdate).unwrap();
        wait_for(&mut events, ClientEvent::StatusChanged);
        assert_eq!(client.status().unwrap().update.state, UpdateState::Checking);
        assert_eq!(host.updater.jobs(), [Job::Check]);

        finish(&host, Job::Check, UpdaterReport::Available("9.9.9".into()));
        wait_for(&mut events, ClientEvent::StatusChanged);
        let update = client.status().unwrap().update;
        assert_eq!(update.state, UpdateState::Available { version: "9.9.9".into() });
        assert!(update.checked_unix.is_some());
        assert_eq!(host.updater.jobs(), [Job::Check], "telling mode installs nothing");

        client.command(&Request::InstallUpdate).unwrap();
        wait_for(&mut events, ClientEvent::StatusChanged);
        assert_eq!(client.status().unwrap().update.state, UpdateState::Installing);
        assert_eq!(host.updater.jobs(), [Job::Check, Job::Install]);
    }

    #[test]
    fn automatic_updates_install_once_the_last_client_has_gone() {
        let host = RunningHost::start();
        let mut client = host.client();
        client.command(&Request::SetUpdates(UpdateMode::Automatic)).unwrap();
        assert_eq!(client.status().unwrap().update.mode, UpdateMode::Automatic);
        assert_eq!(client.profile().unwrap().updates, UpdateMode::Automatic);

        // Found while an app is open: shown, not installed.
        let app = host.subscribe(false);
        finish(&host, Job::Check, UpdaterReport::Available("9.9.9".into()));
        let shown = (0..100).any(|_| {
            std::thread::sleep(Duration::from_millis(20));
            client.status().unwrap().update.state != UpdateState::Idle
        });
        assert!(shown, "(update): expected the found version in the status");
        assert_eq!(host.updater.jobs(), [], "an open app is not closed unasked");

        drop(app);
        // The service notices a closed stream when it next writes to it.
        client.command(&Request::SetLogging(LoggingConfig::default())).unwrap();
        let installing = (0..100).any(|_| {
            std::thread::sleep(Duration::from_millis(20));
            host.updater.jobs() == [Job::Install]
        });
        assert!(installing, "(update): expected the installation once the app had closed");
    }

    #[test]
    fn the_machine_going_to_sleep_and_waking_reaches_the_controller() {
        let host = RunningHost::start_with(true);
        host.queue.push(Event::Suspend);
        host.queue.push(Event::Resume);
        let told = (0..100).any(|_| {
            std::thread::sleep(Duration::from_millis(20));
            host.hardware.fan_suspensions().len() == 2
        });
        assert!(told, "(host): expected the sleep and the wake to reach the hardware");
        assert_eq!(host.hardware.fan_suspensions(), [true, false]);
    }
}
