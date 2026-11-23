//! A scripted backend: the app without a service.
//!
//! `syspilot --demo` shows every view with believable data and without
//! touching the system. It is what the views are reviewed against, and the
//! fake the app's tests run on.

use std::sync::Mutex;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use syspilot_proto::action::{Action, RegData, RegistryAction, Scope};
use syspilot_proto::catalog::Catalog;
use syspilot_proto::message::{
    ActionState, ActionStatus, DriverState, EntryStatus, FanStatus, Feature, HistoryEvent,
    HistoryKind, InfoSection, Request, ServiceStatus, SystemInfo, SystemSummary,
};
use syspilot_proto::profile::{CustomAction, Profile};
use syspilot_proto::sensor::{DeviceClass, DeviceInfo, Sample, SensorInfo, SensorKind, SensorList};

use crate::backend::{Backend, Change, Failure, Listener};
use crate::model::Snapshot;

/// The built-in catalog, so the demo shows the real texts.
const CATALOG_SOURCES: [&str; 5] = [
    include_str!("../../../catalog/tweaks/performance.toml"),
    include_str!("../../../catalog/tweaks/privacy.toml"),
    include_str!("../../../catalog/tweaks/security.toml"),
    include_str!("../../../catalog/tweaks/reliability.toml"),
    include_str!("../../../catalog/tweaks/other.toml"),
];

/// Which situation the demo plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scenario {
    /// Elevated, driver ready, a few entries needing attention.
    Healthy,
    /// As `Healthy`, but PawnIO is not installed.
    NoDriver,
    /// The app runs without administrator rights.
    StandardUser,
    /// The service is not running.
    Stopped,
}

impl Scenario {
    /// Parses the value of `--demo`.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "healthy" => Some(Self::Healthy),
            "no-driver" => Some(Self::NoDriver),
            "standard-user" => Some(Self::StandardUser),
            "stopped" => Some(Self::Stopped),
            _ => None,
        }
    }
}

/// A demo sensor: its description and the wave it follows.
struct Wave {
    device: u32,
    key: &'static str,
    label: &'static str,
    kind: SensorKind,
    driver: bool,
    base: f64,
    swing: f64,
    period_secs: f64,
}

const fn wave(
    device: u32,
    key: &'static str,
    label: &'static str,
    kind: SensorKind,
    driver: bool,
    (base, swing, period_secs): (f64, f64, f64),
) -> Wave {
    Wave { device, key, label, kind, driver, base, swing, period_secs }
}

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
const MIB: f64 = 1024.0 * 1024.0;

const DEVICES: [(&str, DeviceClass); 6] = [
    ("Example Core 8-Core Processor", DeviceClass::Cpu),
    ("Example Graphics 4070", DeviceClass::Gpu),
    ("Memory", DeviceClass::Memory),
    ("Example NVMe SSD 1TB", DeviceClass::Storage),
    ("Example Z790 board", DeviceClass::Board),
    ("Ethernet", DeviceClass::Network),
];

const WAVES: [Wave; 22] = [
    wave(0, "cpu0.load", "Total load", SensorKind::Load, false, (18.0, 14.0, 23.0)),
    wave(0, "cpu0.package.temp", "Package", SensorKind::Temperature, true, (52.0, 9.0, 31.0)),
    wave(0, "cpu0.package.power", "Package power", SensorKind::Power, true, (38.0, 22.0, 23.0)),
    wave(0, "cpu0.frequency", "Clock", SensorKind::Frequency, false, (4300.0, 600.0, 17.0)),
    wave(0, "cpu0.core0.temp", "Core 1", SensorKind::Temperature, true, (50.0, 10.0, 19.0)),
    wave(0, "cpu0.core1.temp", "Core 2", SensorKind::Temperature, true, (49.0, 8.0, 27.0)),
    wave(1, "gpu0.load", "Load", SensorKind::Load, false, (9.0, 8.0, 13.0)),
    wave(1, "gpu0.temp", "Temperature", SensorKind::Temperature, false, (44.0, 4.0, 41.0)),
    wave(
        1,
        "gpu0.memory.used",
        "Memory in use",
        SensorKind::Data,
        false,
        (2.1 * GIB, 0.3 * GIB, 37.0),
    ),
    wave(2, "memory.load", "In use", SensorKind::Load, false, (41.0, 3.0, 53.0)),
    wave(2, "memory.used", "Used", SensorKind::Data, false, (13.1 * GIB, 0.9 * GIB, 53.0)),
    wave(3, "disk0.temp", "Temperature", SensorKind::Temperature, false, (39.0, 2.0, 61.0)),
    wave(3, "disk0.read", "Read", SensorKind::Throughput, false, (9.0 * MIB, 9.0 * MIB, 11.0)),
    wave(3, "disk0.write", "Write", SensorKind::Throughput, false, (3.0 * MIB, 3.0 * MIB, 7.0)),
    wave(4, "board.fan1", "CPU fan", SensorKind::Fan, true, (910.0, 160.0, 31.0)),
    wave(4, "board.fan2", "Rear fan", SensorKind::Fan, true, (640.0, 60.0, 43.0)),
    wave(4, "board.temp1", "Motherboard", SensorKind::Temperature, true, (36.0, 1.5, 71.0)),
    // Raw inputs as a real chip reports them, including one nothing is
    // wired to: the app must fold these away and not call 103 °C hot.
    wave(4, "board.temp.auxtin1", "AUXTIN1", SensorKind::Temperature, true, (103.0, 0.0, 1.0)),
    wave(4, "board.vin0", "VIN0", SensorKind::Voltage, true, (1.2, 0.01, 17.0)),
    wave(4, "board.pwm2", "Fan 2 duty", SensorKind::Duty, true, (42.0, 4.0, 43.0)),
    wave(5, "net0.down", "Download", SensorKind::Throughput, false, (0.6 * MIB, 0.6 * MIB, 9.0)),
    wave(5, "net0.up", "Upload", SensorKind::Throughput, false, (0.05 * MIB, 0.05 * MIB, 5.0)),
];

/// Mutable part of the demo.
struct DemoState {
    snapshot: Snapshot,
    /// One sender per open listener, to announce data changes.
    listeners: Vec<mpsc::Sender<()>>,
}

/// A backend that plays a [`Scenario`] from memory.
pub struct DemoBackend {
    scenario: Scenario,
    state: Mutex<DemoState>,
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs())
}

fn entry(
    id: &str,
    enabled: bool,
    state: ActionState,
    current: &str,
    drift_count: u32,
) -> EntryStatus {
    EntryStatus {
        id: id.to_owned(),
        enabled,
        state: state.clone(),
        actions: vec![ActionStatus { state, current: current.to_owned() }],
        drift_count,
        last_drift_unix: (drift_count > 0).then(|| unix_now() - 5400),
    }
}

/// States that make the demo interesting; every other entry is off.
fn scripted_states() -> Vec<EntryStatus> {
    let applied = |id, restored| entry(id, true, ActionState::Compliant, "dword:0", restored);
    vec![
        applied("privacy.advertising-id", 0),
        applied("privacy.tailored-experiences", 2),
        applied("privacy.feedback-requests", 0),
        applied("privacy.diagnostic-data-required", 7),
        applied("privacy.activity-history", 0),
        applied("performance.background-apps", 0),
        applied("other.start-recommendations", 1),
        applied("security.llmnr", 0),
        applied("security.smb1-server", 0),
        entry(
            "privacy.web-search",
            true,
            ActionState::Contested { retry_in_secs: 840 },
            "dword:1",
            23,
        ),
        entry(
            "security.memory-integrity",
            true,
            ActionState::NotEnforceable {
                reason: "The setting is locked by your organisation's policy.".into(),
            },
            "dword:0",
            0,
        ),
        entry(
            "privacy.recall-snapshots",
            false,
            ActionState::NotApplicable { reason: "needs Windows build 26100 or later".into() },
            "(absent)",
            0,
        ),
        entry("other.transparency", false, ActionState::Compliant, "dword:0", 0),
        entry("security.audit-secure-boot", false, ActionState::Compliant, "dword:1", 0),
        entry(
            "security.audit-secure-boot-certificates",
            false,
            ActionState::Different,
            "dword:0",
            0,
        ),
        entry("security.audit-uac-enabled", false, ActionState::Compliant, "dword:1", 0),
        entry("security.audit-uac-secure-desktop", false, ActionState::Compliant, "dword:1", 0),
        entry("security.audit-uac-secure-ui-paths", false, ActionState::Compliant, "dword:1", 0),
        entry("custom.keep-numlock", true, ActionState::Compliant, "sz:2", 3),
    ]
}

fn states_for(catalog: &Catalog) -> Vec<EntryStatus> {
    let scripted = scripted_states();
    let mut states: Vec<EntryStatus> =
        catalog
            .tweaks
            .iter()
            .map(|tweak| {
                scripted.iter().find(|status| status.id == tweak.id).cloned().unwrap_or_else(|| {
                    entry(&tweak.id, false, ActionState::Different, "dword:1", 0)
                })
            })
            .collect();
    states.extend(scripted.into_iter().filter(|status| status.id.starts_with("custom.")));
    states
}

fn profile_for(states: &[EntryStatus]) -> Profile {
    Profile {
        enabled: states
            .iter()
            .filter(|status| status.enabled && !status.id.starts_with("custom."))
            .map(|status| status.id.clone())
            .collect(),
        custom: vec![CustomAction {
            id: "custom.keep-numlock".into(),
            title: "Keep Num Lock on at sign-in".into(),
            enabled: true,
            action: Action::Registry(RegistryAction {
                scope: Scope::User,
                key: r"Control Panel\Keyboard".into(),
                name: "InitialKeyboardIndicators".into(),
                data: RegData::Sz("2".into()),
            }),
            triggers: Vec::new(),
        }],
        ..Profile::default()
    }
}

fn history() -> Vec<HistoryEvent> {
    let now = unix_now();
    let event = |ago: u64, id: &str, kind, detail: &str| HistoryEvent {
        unix_secs: now - ago,
        id: id.to_owned(),
        kind,
        detail: detail.to_owned(),
    };
    vec![
        event(
            95,
            "privacy.web-search",
            HistoryKind::Contested,
            "Changed 20 times in one minute; paused for 15 minutes",
        ),
        event(
            160,
            "privacy.web-search",
            HistoryKind::Restored,
            r"HKCU\Software\Policies\Microsoft\Windows\Explorer\DisableSearchBoxSuggestions: dword:0 → dword:1",
        ),
        event(
            5400,
            "privacy.diagnostic-data-required",
            HistoryKind::Restored,
            r"HKLM\SOFTWARE\Policies\Microsoft\Windows\DataCollection\AllowTelemetry: dword:3 → dword:1",
        ),
        event(
            9100,
            "security.memory-integrity",
            HistoryKind::Refused,
            "Access denied: the value is managed by policy",
        ),
        event(
            86_400 + 300,
            "custom.keep-numlock",
            HistoryKind::Restored,
            r"HKCU\Control Panel\Keyboard\InitialKeyboardIndicators: sz:0 → sz:2",
        ),
        event(
            2 * 86_400,
            "security.smb1-server",
            HistoryKind::Applied,
            r"HKLM\SYSTEM\CurrentControlSet\Services\LanmanServer\Parameters\SMB1: (absent) → dword:0",
        ),
        event(
            2 * 86_400 + 40,
            "other.widgets",
            HistoryKind::Reverted,
            r"HKLM\SOFTWARE\Policies\Microsoft\Dsh\AllowNewsAndInterests: dword:0 → (absent)",
        ),
    ]
}

fn sensors(with_driver: bool) -> SensorList {
    SensorList {
        generation: 1,
        devices: DEVICES
            .iter()
            .map(|(name, class)| DeviceInfo { name: (*name).to_owned(), class: *class })
            .collect(),
        sensors: WAVES
            .iter()
            .filter(|wave| with_driver || !wave.driver)
            .enumerate()
            .map(|(id, wave)| SensorInfo {
                id: id as u32,
                device: wave.device,
                key: wave.key.to_owned(),
                label: wave.label.to_owned(),
                kind: wave.kind,
                driver: wave.driver,
            })
            .collect(),
    }
}

/// The readings `tick` seconds into the demo.
fn sample(with_driver: bool, tick: u64) -> Vec<Sample> {
    WAVES
        .iter()
        .filter(|wave| with_driver || !wave.driver)
        .enumerate()
        .map(|(id, wave)| {
            let phase = tick as f64 / wave.period_secs * std::f64::consts::TAU;
            // A second, faster wave keeps the lines from looking drawn.
            let value = wave.base + wave.swing * (0.75 * phase.sin() + 0.25 * (phase * 3.7).cos());
            Sample { id: id as u32, value: Some(value.max(0.0)) }
        })
        .collect()
}

fn status(scenario: Scenario) -> ServiceStatus {
    let with_driver = scenario != Scenario::NoDriver;
    let feature = |key: &str, title: &str, available: bool, detail: &str| Feature {
        key: key.to_owned(),
        title: title.to_owned(),
        available,
        detail: detail.to_owned(),
    };
    let needs_driver = "Needs PawnIO";
    ServiceStatus {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        instance: "SysPilot-Demo".into(),
        uptime_secs: 3 * 86_400 + 4 * 3600 + 12 * 60,
        console: false,
        driver: if with_driver {
            DriverState::Ready { version: "2.2.0".into() }
        } else {
            DriverState::Missing
        },
        driver_url: "https://pawnio.eu/".into(),
        features: vec![
            feature(
                "cpu-sensors",
                "Processor temperature and power",
                with_driver,
                if with_driver { "Example Core, 8 cores" } else { needs_driver },
            ),
            feature(
                "board-sensors",
                "Motherboard fans and temperatures",
                with_driver,
                if with_driver { "Example Super I/O chip" } else { needs_driver },
            ),
            feature(
                "memory-sensors",
                "Memory module temperatures",
                false,
                if with_driver {
                    "The memory modules have no temperature sensor"
                } else {
                    needs_driver
                },
            ),
            feature(
                "chipset-sensors",
                "Chipset temperature",
                with_driver,
                if with_driver { "Example PCH, PCI thermal BAR0" } else { needs_driver },
            ),
            feature("fan-control", "Fan control", false, "Not part of this version"),
        ],
        subscribers: 1,
        sampling: true,
        os_build: 26_200,
        log_dir: r"C:\ProgramData\SysPilot\logs".into(),
        fan_control: false,
        fans: FanStatus::default(),
        problems: Vec::new(),
    }
}

fn system() -> SystemInfo {
    let rows = |rows: &[(&str, &str)]| {
        rows.iter().map(|(label, value)| ((*label).to_owned(), (*value).to_owned())).collect()
    };
    SystemInfo {
        summary: SystemSummary {
            computer: "DEMO-PC".into(),
            model: "Example Z790 board".into(),
            cpu: "Example Core 8-Core Processor".into(),
            gpus: vec!["Example Graphics 4070".into()],
            memory_bytes: 32 * 1024 * 1024 * 1024,
            os: "Windows 11 Pro 25H2 (build 26200)".into(),
            bios: "Example BIOS 1.20, 2026-03-14".into(),
        },
        sections: vec![
            InfoSection {
                title: "Processor".into(),
                rows: rows(&[
                    ("Name", "Example Core 8-Core Processor"),
                    ("Cores", "8"),
                    ("Threads", "16"),
                    ("Base speed", "3.40 GHz"),
                    ("Virtualisation", "Enabled"),
                ]),
            },
            InfoSection {
                title: "Memory module 1".into(),
                rows: rows(&[
                    ("Size", "16.0 GB"),
                    ("Type", "DDR5"),
                    ("Speed", "5600 MT/s"),
                    ("Slot", "DIMM A2"),
                ]),
            },
            InfoSection {
                title: "Memory module 2".into(),
                rows: rows(&[
                    ("Size", "16.0 GB"),
                    ("Type", "DDR5"),
                    ("Speed", "5600 MT/s"),
                    ("Slot", "DIMM B2"),
                ]),
            },
            InfoSection {
                title: "Storage".into(),
                rows: rows(&[
                    ("Example NVMe SSD 1TB", "931 GB, NVMe, healthy"),
                    ("C:", "412 GB free of 930 GB, NTFS, BitLocker on"),
                ]),
            },
            InfoSection {
                title: "Security".into(),
                rows: rows(&[("Secure Boot", "On"), ("TPM", "2.0"), ("Firmware", "UEFI")]),
            },
        ],
    }
}

impl DemoBackend {
    /// A demo playing `scenario`.
    pub fn new(scenario: Scenario) -> Self {
        // The catalog is validated by its own tests; a broken one shows an
        // empty demo instead of taking the app down.
        let catalog = Catalog::parse_all(CATALOG_SOURCES).unwrap_or_default();
        let states = states_for(&catalog);
        let elevated = scenario != Scenario::StandardUser;
        let snapshot = Snapshot {
            elevated,
            status: Some(status(scenario)),
            profile: elevated.then(|| profile_for(&states)),
            history: if elevated { history() } else { Vec::new() },
            sensors: sensors(scenario != Scenario::NoDriver),
            system: system(),
            catalog,
            states,
        };
        Self { scenario, state: Mutex::new(DemoState { snapshot, listeners: Vec::new() }) }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, DemoState> {
        // The state stays usable after a panic elsewhere: it is plain data.
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Applies a request to the demo snapshot the way the service would.
fn apply(snapshot: &mut Snapshot, request: &Request) -> Result<(), Failure> {
    let unknown = |id: &str| Failure::Other(format!("There is no entry called {id}."));
    let record = |snapshot: &mut Snapshot, id: &str, kind| {
        let event = HistoryEvent {
            unix_secs: unix_now(),
            id: id.to_owned(),
            kind,
            detail: "Demo: nothing was changed on this PC".into(),
        };
        snapshot.history.insert(0, event);
    };
    match request {
        Request::SetTweak { id, enabled } => {
            let status =
                snapshot.states.iter_mut().find(|s| s.id == *id).ok_or_else(|| unknown(id))?;
            status.enabled = *enabled;
            status.drift_count = 0;
            status.state = if *enabled { ActionState::Compliant } else { ActionState::Different };
            if let Some(profile) = &mut snapshot.profile {
                profile.enabled.retain(|enabled_id| enabled_id != id);
                if *enabled {
                    profile.enabled.push(id.clone());
                }
            }
            let kind = if *enabled { HistoryKind::Applied } else { HistoryKind::Reverted };
            record(snapshot, id, kind);
        }
        Request::Reapply { id } => {
            let status =
                snapshot.states.iter_mut().find(|s| s.id == *id).ok_or_else(|| unknown(id))?;
            status.state = ActionState::Compliant;
            record(snapshot, id, HistoryKind::Restored);
        }
        Request::PutCustom(custom) => {
            let state =
                if custom.enabled { ActionState::Compliant } else { ActionState::Different };
            snapshot.states.retain(|status| status.id != custom.id);
            snapshot.states.push(entry(&custom.id, custom.enabled, state, "", 0));
            if let Some(profile) = &mut snapshot.profile {
                profile.custom.retain(|existing| existing.id != custom.id);
                profile.custom.push(custom.clone());
            }
            if custom.enabled {
                record(snapshot, &custom.id, HistoryKind::Applied);
            }
        }
        Request::RemoveCustom { id } => {
            snapshot.states.retain(|status| status.id != *id);
            if let Some(profile) = &mut snapshot.profile {
                profile.custom.retain(|existing| existing.id != *id);
            }
            record(snapshot, id, HistoryKind::Reverted);
        }
        Request::SetLogging(logging) => {
            if let Some(profile) = &mut snapshot.profile {
                profile.logging = logging.clone();
            }
        }
        Request::ClearHistory => snapshot.history.clear(),
        _ => return Err(Failure::Other("The demo does not support this.".into())),
    }
    Ok(())
}

impl Backend for DemoBackend {
    fn load(&self) -> Result<Snapshot, Failure> {
        if self.scenario == Scenario::Stopped {
            return Err(Failure::NotRunning);
        }
        Ok(self.state().snapshot.clone())
    }

    fn send(&self, request: &Request) -> Result<(), Failure> {
        let mut state = self.state();
        if !state.snapshot.elevated {
            return Err(Failure::Other("Administrator rights are required.".into()));
        }
        apply(&mut state.snapshot, request)?;
        // Tell every listener that is still there.
        state.listeners.retain(|listener| listener.send(()).is_ok());
        Ok(())
    }

    fn listen(&self) -> Result<Box<dyn Listener>, Failure> {
        if self.scenario == Scenario::Stopped {
            return Err(Failure::NotRunning);
        }
        let (sender, changes) = mpsc::channel();
        self.state().listeners.push(sender);
        Ok(Box::new(DemoListener {
            changes,
            with_driver: self.scenario != Scenario::NoDriver,
            tick: 0,
        }))
    }
}

/// Produces a sample every second and relays data changes in between.
struct DemoListener {
    changes: mpsc::Receiver<()>,
    with_driver: bool,
    tick: u64,
}

impl Listener for DemoListener {
    fn next_change(&mut self) -> Result<Change, Failure> {
        match self.changes.recv_timeout(Duration::from_secs(1)) {
            Ok(()) => Ok(Change::Data),
            Err(RecvTimeoutError::Timeout) => {
                self.tick += 1;
                Ok(Change::Sample { generation: 1, samples: sample(self.with_driver, self.tick) })
            }
            Err(RecvTimeoutError::Disconnected) => Err(Failure::NotRunning),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{self, ViewLevel};

    #[test]
    fn demo_covers_the_whole_built_in_catalog() {
        let snapshot = DemoBackend::new(Scenario::Healthy).load().unwrap();
        assert!(snapshot.catalog.tweaks.len() >= 50, "{}", snapshot.catalog.tweaks.len());
        for tweak in &snapshot.catalog.tweaks {
            assert!(
                snapshot.states.iter().any(|status| status.id == tweak.id),
                "(demo state): expected a state for every tweak | missing: {}",
                tweak.id
            );
        }
        for status in scripted_states().iter().filter(|s| !s.id.starts_with("custom.")) {
            assert!(
                snapshot.catalog.get(&status.id).is_some(),
                "(demo state): expected a catalog ID | received: {}",
                status.id
            );
        }
    }

    #[test]
    fn demo_shows_every_kind_of_state() {
        let snapshot = DemoBackend::new(Scenario::Healthy).load().unwrap();
        let overview = model::overview(&snapshot);
        assert_eq!((overview.checks_passed, overview.checks_total), (4, 5));
        assert_eq!(overview.problems, 2);
        assert_eq!(model::attention(&snapshot).len(), 3);
        assert_eq!(model::categories(&snapshot, ViewLevel::Advanced).len(), 5);
    }

    #[test]
    fn turning_a_tweak_on_changes_state_profile_and_history() {
        let demo = DemoBackend::new(Scenario::Healthy);
        let mut listener = demo.listen().unwrap();
        let request = Request::SetTweak { id: "other.widgets".into(), enabled: true };
        demo.send(&request).unwrap();

        assert_eq!(listener.next_change(), Ok(Change::Data));
        let snapshot = demo.load().unwrap();
        let status = snapshot.states.iter().find(|s| s.id == "other.widgets").unwrap();
        assert!(status.enabled);
        assert_eq!(status.state, ActionState::Compliant);
        assert!(snapshot.profile.unwrap().enabled.contains(&"other.widgets".to_owned()));
        assert_eq!(snapshot.history[0].kind, HistoryKind::Applied);
    }

    #[test]
    fn unknown_entry_is_refused_by_name() {
        let demo = DemoBackend::new(Scenario::Healthy);
        let request = Request::SetTweak { id: "no.such-entry".into(), enabled: true };
        let refusal = demo.send(&request).unwrap_err();
        assert_eq!(refusal, Failure::Other("There is no entry called no.such-entry.".into()));
    }

    #[test]
    fn standard_user_sees_states_but_cannot_change_or_read_the_profile() {
        let demo = DemoBackend::new(Scenario::StandardUser);
        let snapshot = demo.load().unwrap();
        assert!(!snapshot.elevated);
        assert_eq!(snapshot.profile, None);
        assert!(snapshot.history.is_empty());
        assert!(!snapshot.states.is_empty());
        assert!(demo.send(&Request::ClearHistory).is_err());
    }

    #[test]
    fn stopped_service_fails_every_call_as_not_running() {
        let demo = DemoBackend::new(Scenario::Stopped);
        assert_eq!(demo.load(), Err(Failure::NotRunning));
        assert!(matches!(demo.listen(), Err(Failure::NotRunning)));
    }

    #[test]
    fn without_the_driver_only_windows_sensors_remain() {
        let snapshot = DemoBackend::new(Scenario::NoDriver).load().unwrap();
        assert!(snapshot.sensors.sensors.iter().all(|sensor| !sensor.driver));
        assert_eq!(snapshot.status.unwrap().driver, DriverState::Missing);
        let ids: Vec<u32> = sample(false, 3).iter().map(|sample| sample.id).collect();
        let listed: Vec<u32> = snapshot.sensors.sensors.iter().map(|sensor| sensor.id).collect();
        assert_eq!(ids, listed, "samples and metadata use the same IDs");
    }

    #[test]
    fn listener_ticks_a_sample_each_second() {
        let demo = DemoBackend::new(Scenario::Healthy);
        let mut listener = demo.listen().unwrap();
        let Ok(Change::Sample { generation, samples }) = listener.next_change() else {
            panic!("(demo listener): expected a sample after one second");
        };
        assert_eq!(generation, 1);
        assert_eq!(samples.len(), WAVES.len());
        assert!(samples.iter().all(|sample| sample.value.is_some_and(|value| value >= 0.0)));
    }

    #[test]
    fn scenario_names_parse() {
        assert_eq!(Scenario::parse("no-driver"), Some(Scenario::NoDriver));
        assert_eq!(Scenario::parse("standard-user"), Some(Scenario::StandardUser));
        assert_eq!(Scenario::parse("nonsense"), None);
    }
}
