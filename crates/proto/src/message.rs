//! IPC messages.
//!
//! A connection starts with [`Request::Hello`]. After a successful
//! handshake the client sends requests and receives exactly one response
//! each, until it sends [`Request::Subscribe`]: from then on the service
//! pushes [`Response::Event`] frames and the client sends nothing more.
//!
//! Requests come in two privilege levels. Open requests (status, catalog,
//! states, sensors) are available to any local authenticated user.
//! Everything that changes configuration or system state, and everything
//! that reveals the profile (which holds user-defined command lines),
//! requires the client to be an elevated administrator. See
//! [`Request::requires_elevation`].

use serde::{Deserialize, Serialize};

use crate::catalog::Catalog;
use crate::profile::{CustomAction, FanCurve, LoggingConfig, Profile, UpdateMode};
use crate::sensor::SensorList;

/// A request from a client to the service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// Handshake; must be the first frame on a connection.
    Hello {
        /// The client's [`crate::PROTOCOL_VERSION`].
        protocol: u16,
    },
    /// Service health and capabilities.
    Status,
    /// The tweak catalog with all texts.
    Catalog,
    /// Current state of every catalog tweak and custom action.
    States,
    /// The stored profile. Requires elevation: the profile is readable by
    /// administrators only, on disk and here.
    Profile,
    /// The most recent enforcement events, newest first. Requires
    /// elevation: details can quote custom command lines.
    History {
        /// Most events to return.
        limit: u32,
    },
    /// Sensor metadata.
    Sensors,
    /// Static system information.
    SystemInfo,
    /// Turns the connection into an event stream. While at least one
    /// subscriber with `sensors` set is connected, the service samples
    /// sensors and publishes them to the shared-memory section.
    Subscribe {
        /// Whether the client reads live sensor values.
        sensors: bool,
    },

    /// Enables (snapshot, apply, enforce) or disables (revert) a catalog
    /// tweak. Mutating.
    SetTweak {
        /// Catalog ID.
        id: String,
        /// Desired enforcement state.
        enabled: bool,
    },
    /// Adds or replaces a custom action. Mutating.
    PutCustom(CustomAction),
    /// Reverts and removes a custom action. Mutating.
    RemoveCustom {
        /// Custom action ID.
        id: String,
    },
    /// Re-evaluates one enforced entry now, clearing a contested state.
    /// Mutating.
    Reapply {
        /// Tweak or custom action ID.
        id: String,
    },
    /// Replaces the whole profile: reverts what is no longer enabled and
    /// applies what is new. Mutating.
    SetProfile(Profile),
    /// Replaces the logging settings. Mutating.
    SetLogging(LoggingConfig),
    /// Replaces the fan curves. Mutating.
    SetFans(Vec<FanCurve>),
    /// Deletes the enforcement history. Mutating.
    ClearHistory,
    /// Stops an instance running in console mode. A Windows service refuses
    /// it: services are stopped through the service control manager.
    /// Mutating.
    Shutdown,
    /// Chooses how SysPilot keeps itself up to date. Mutating.
    SetUpdates(UpdateMode),
    /// Looks for a new version now, whatever the mode. Mutating: it makes the
    /// service reach out to the network.
    CheckForUpdate,
    /// Downloads, verifies and installs the newest version. Mutating.
    InstallUpdate,
}

impl Request {
    /// Whether the client must be an elevated administrator: true for
    /// every request that changes configuration or system state, and for
    /// those that reveal the profile.
    pub fn requires_elevation(&self) -> bool {
        match self {
            Self::Hello { .. }
            | Self::Status
            | Self::Catalog
            | Self::States
            | Self::Sensors
            | Self::SystemInfo
            | Self::Subscribe { .. } => false,
            Self::Profile
            | Self::History { .. }
            | Self::SetTweak { .. }
            | Self::PutCustom(_)
            | Self::RemoveCustom { .. }
            | Self::Reapply { .. }
            | Self::SetProfile(_)
            | Self::SetLogging(_)
            | Self::SetFans(_)
            | Self::ClearHistory
            | Self::Shutdown
            | Self::SetUpdates(_)
            | Self::CheckForUpdate
            | Self::InstallUpdate => true,
        }
    }
}

/// A response from the service.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Response {
    /// Handshake accepted.
    Hello(Welcome),
    /// The request succeeded and has no payload.
    Ok,
    /// The request failed.
    Error(ServiceError),
    /// Answer to [`Request::Status`].
    Status(ServiceStatus),
    /// Answer to [`Request::Catalog`].
    Catalog(Catalog),
    /// Answer to [`Request::States`].
    States(Vec<EntryStatus>),
    /// Answer to [`Request::Profile`].
    Profile(Profile),
    /// Answer to [`Request::History`].
    History(Vec<HistoryEvent>),
    /// Answer to [`Request::Sensors`].
    Sensors(SensorList),
    /// Answer to [`Request::SystemInfo`].
    SystemInfo(SystemInfo),
    /// Answer to [`Request::Subscribe`]; events follow.
    Subscribed,
    /// Pushed on a subscribed connection.
    Event(Event),
}

/// Handshake result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    /// The service's protocol version (equal to the client's).
    pub protocol: u16,
    /// Service version, e.g. `0.1.0`.
    pub version: String,
    /// Whether this client may send mutating requests.
    pub elevated: bool,
}

/// Category of a failed request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    /// Client and service speak different protocol versions.
    ProtocolMismatch,
    /// A mutating request from a client that is not an elevated
    /// administrator.
    AccessDenied,
    /// The referenced tweak, action or sensor does not exist.
    NotFound,
    /// The request payload failed validation.
    Invalid,
    /// The request was valid but could not be carried out.
    Failed,
    /// This build or this machine does not support the feature.
    Unsupported,
}

/// A failed request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceError {
    /// Category, for the client to branch on.
    pub code: ErrorCode,
    /// Explanation for the user.
    pub message: String,
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for ServiceError {}

/// Something changed in the service; the client should refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    /// Entry states or the history changed.
    StatesChanged,
    /// The profile changed.
    ProfileChanged,
    /// The set of sensors changed; metadata must be fetched again.
    SensorsChanged,
    /// Driver or feature availability changed.
    StatusChanged,
    /// A new sensor sample is in the shared-memory section. Sent only to
    /// subscribers that asked for sensors.
    Sampled,
    /// The service is stopping.
    Stopping,
}

/// Where an action stands relative to its desired state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActionState {
    /// The system holds the desired state.
    Compliant,
    /// The system holds a different state.
    Different,
    /// Enforced, but another writer keeps changing it back. Enforcement is
    /// paused and retried later.
    Contested {
        /// Seconds until the next attempt.
        retry_in_secs: u32,
    },
    /// Windows refuses the change (protected setting or access denied).
    NotEnforceable {
        /// What refused it.
        reason: String,
    },
    /// The target does not exist on this system, so there is nothing to do.
    NotApplicable {
        /// What is missing.
        reason: String,
    },
    /// Per-user state with no user logged on to evaluate it for.
    Unknown,
    /// Detecting or applying failed unexpectedly.
    Failed {
        /// The error.
        error: String,
    },
}

/// State of one action, with the value currently on the system.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionStatus {
    /// Where the action stands.
    pub state: ActionState,
    /// Current value in display form, e.g. `dword:3` or `(absent)`.
    pub current: String,
}

/// State of a catalog tweak or a custom action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryStatus {
    /// Tweak or custom action ID.
    pub id: String,
    /// Whether the entry is enforced.
    pub enabled: bool,
    /// Combined state of all actions (the most severe one).
    pub state: ActionState,
    /// State per action, in definition order.
    pub actions: Vec<ActionStatus>,
    /// Times the entry drifted and was restored since it was enabled.
    pub drift_count: u32,
    /// When it last drifted, seconds since the Unix epoch.
    pub last_drift_unix: Option<u64>,
}

/// What happened in an enforcement event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HistoryKind {
    /// Applied for the first time after being enabled.
    Applied,
    /// Found changed by something else and restored.
    Restored,
    /// Reverted to the original state after being disabled.
    Reverted,
    /// Marked contested: another writer keeps changing it.
    Contested,
    /// Windows refused the change.
    Refused,
    /// Detecting or applying failed.
    Failed,
}

/// One line of enforcement history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEvent {
    /// When, seconds since the Unix epoch.
    pub unix_secs: u64,
    /// Tweak or custom action ID.
    pub id: String,
    /// What happened.
    pub kind: HistoryKind,
    /// Details, e.g. `HKLM\…\AllowTelemetry: dword:3 → dword:0`.
    pub detail: String,
}

/// Installation state of the PawnIO driver.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DriverState {
    /// PawnIO is not installed.
    Missing,
    /// Installed and usable.
    Ready {
        /// Driver version, e.g. `2.2.0`.
        version: String,
    },
    /// Installed, but older than the service supports.
    TooOld {
        /// Installed version.
        version: String,
        /// Oldest supported version.
        required: String,
    },
    /// Installed, but it could not be opened.
    Failed {
        /// The error.
        error: String,
    },
}

/// A capability that depends on the driver, the hardware or the build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Feature {
    /// Stable key, e.g. `cpu-sensors`, `board-sensors`, `memory-sensors`,
    /// `fan-control`.
    pub key: String,
    /// Display name.
    pub title: String,
    /// Whether it works on this machine right now.
    pub available: bool,
    /// What it found when available, or why it is not.
    pub detail: String,
}

/// Whether SysPilot may drive the fans of this machine, and if not, why.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum FanAvailability {
    /// This build has no fan control: the `fan-control` cargo feature is off,
    /// as in every release.
    #[default]
    NotInBuild,
    /// Fan speeds are shown and nothing is written.
    ReadOnly {
        /// Why, in words for the user: PawnIO is missing, the board is not in
        /// the catalog, or its entry has not been validated.
        reason: String,
    },
    /// The board's entry is validated, the driver works and curves in the
    /// profile are followed.
    Controllable,
}

/// Where a fan channel stands. Mirrors the controller's phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum FanPhase {
    /// The BIOS controls the channel.
    #[default]
    Bios,
    /// SysPilot holds the channel in manual mode and follows its curve.
    Manual,
    /// A temperature source failed, so the fan is held at 100 %.
    FailSafe,
    /// Handing the channel back to the BIOS failed and is retried.
    ReleasePending,
    /// Found in manual mode with no saved original; left alone.
    Blocked,
}

/// One fan output of the board: what the catalog says about it, what the
/// profile asks of it and what the controller is doing.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FanChannelStatus {
    /// The channel key, `board.pwm<N>`: the key of its duty sensor. A profile
    /// curve names the channel by this key.
    pub channel: String,
    /// The header's name on the board, from the catalog entry. The key when
    /// the board has no entry.
    pub label: String,
    /// The key of the tachometer sensor of the fan on this output, when the
    /// catalog entry names its input, e.g. `board.fan2`.
    pub tach_sensor: Option<String>,
    /// The lowest duty the catalog entry allows, percent; 0 when unknown.
    pub min_duty_pct: u8,
    /// The profile's curve for this channel: temperature and duty points.
    /// Empty when the profile has none.
    pub curve: Vec<crate::profile::CurvePoint>,
    /// Sensor keys that drive the curve; the hottest wins.
    pub sources: Vec<String>,
    /// Whether the controller follows the curve: the profile enables it and
    /// the board is [`FanAvailability::Controllable`].
    pub active: bool,
    /// The duty the controller last asked for, percent. `None` while the
    /// BIOS holds the channel.
    pub target_duty_pct: Option<u8>,
    /// Where the channel stands.
    pub phase: FanPhase,
    /// The last thing that went wrong with this channel, if it is still so.
    pub problem: Option<String>,
}

/// Fan control as the service sees it. Part of [`ServiceStatus`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FanStatus {
    /// Whether curves are followed, and if not why.
    pub availability: FanAvailability,
    /// The board as the catalog names it, e.g. `MACHINIST E5-MR9A PRO`; empty
    /// when the machine's board is unknown.
    pub board: String,
    /// The board entry's channels and any profile curve on a channel the
    /// entry does not list. Empty for a board that is not in the catalog.
    pub channels: Vec<FanChannelStatus>,
}

/// What the controller does with one channel right now; the part of
/// [`FanChannelStatus`] that changes while the service runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FanRuntime {
    /// The channel key, as in [`FanChannelStatus::channel`].
    pub channel: String,
    /// Where the channel stands.
    pub phase: FanPhase,
    /// The duty last asked for, percent.
    pub target_duty_pct: Option<u8>,
    /// The last problem, if it is still so.
    pub problem: Option<String>,
}

/// Where the service stands with updating itself.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum UpdateState {
    /// Nothing is known to be newer and nothing is under way.
    #[default]
    Idle,
    /// Looking for a new version.
    Checking,
    /// A newer version is published and has not been installed.
    Available {
        /// The version, e.g. `0.2.1`.
        version: String,
    },
    /// The new version is being downloaded, verified or installed. The
    /// service stops while its files are replaced.
    Installing,
    /// The last check or installation did not work.
    Failed {
        /// Why, for the user.
        reason: String,
    },
    /// This copy does not update itself: a console run or a test instance.
    Unavailable,
}

/// Updates as the service sees them. Part of [`ServiceStatus`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UpdateStatus {
    /// What the profile asks for.
    pub mode: UpdateMode,
    /// Where things stand.
    pub state: UpdateState,
    /// When a check last succeeded, seconds since the Unix epoch. `None`
    /// since the service started.
    pub checked_unix: Option<u64>,
}

/// Answer to [`Request::Status`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceStatus {
    /// Service version.
    pub version: String,
    /// Instance name the service runs under.
    pub instance: String,
    /// Seconds since the service started.
    pub uptime_secs: u64,
    /// Whether it runs as a console process instead of a Windows service.
    pub console: bool,
    /// PawnIO driver state.
    pub driver: DriverState,
    /// Where the user can get PawnIO.
    pub driver_url: String,
    /// Driver- and hardware-dependent capabilities.
    pub features: Vec<Feature>,
    /// Connected event subscribers.
    pub subscribers: u32,
    /// Whether sensors are being sampled right now.
    pub sampling: bool,
    /// Windows build number, for filtering catalog entries.
    pub os_build: u32,
    /// Directory sensor logs are written to.
    pub log_dir: String,
    /// Whether this build can drive fans. Off until fan control has been
    /// validated on real hardware.
    pub fan_control: bool,
    /// What fan control does on this machine: channels, curves and phases.
    pub fans: FanStatus,
    /// Whether a new version is available and what is done about it.
    pub update: UpdateStatus,
    /// Things the user should know about, e.g. a profile that could not be
    /// read. Empty when all is well.
    pub problems: Vec<String>,
}

/// Answer to [`Request::SystemInfo`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SystemInfo {
    /// Headline facts for the summary view.
    pub summary: SystemSummary,
    /// Everything else, grouped for display.
    pub sections: Vec<InfoSection>,
}

/// Headline facts about the machine. Empty strings mean "unknown".
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SystemSummary {
    /// Computer name.
    pub computer: String,
    /// System or motherboard model.
    pub model: String,
    /// Processor name.
    pub cpu: String,
    /// Graphics adapter names.
    pub gpus: Vec<String>,
    /// Installed memory in bytes.
    pub memory_bytes: u64,
    /// Windows edition, version and build.
    pub os: String,
    /// BIOS vendor, version and date.
    pub bios: String,
}

/// A titled group of label/value rows.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct InfoSection {
    /// Group title, e.g. `Motherboard` or `Memory module 1`.
    pub title: String,
    /// Rows as (label, value).
    pub rows: Vec<(String, String)>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{Action, RegData, RegistryAction, Scope};
    use crate::frame::{read_frame, write_frame};
    use std::io::Cursor;

    fn round_trip<T>(message: &T) -> T
    where
        T: Serialize + serde::de::DeserializeOwned,
    {
        let mut wire = Vec::new();
        write_frame(&mut wire, message).unwrap();
        read_frame(&mut Cursor::new(wire)).unwrap()
    }

    fn custom_action() -> CustomAction {
        CustomAction {
            id: "custom.scratch".into(),
            title: "Scratch".into(),
            enabled: true,
            action: Action::Registry(RegistryAction {
                scope: Scope::Machine,
                key: r"SOFTWARE\SysPilot-Test".into(),
                name: "Value".into(),
                data: RegData::Dword(1),
            }),
            triggers: vec![],
        }
    }

    #[test]
    fn every_request_round_trips() {
        let requests = [
            Request::Hello { protocol: 1 },
            Request::Status,
            Request::Catalog,
            Request::States,
            Request::Profile,
            Request::History { limit: 50 },
            Request::Sensors,
            Request::SystemInfo,
            Request::Subscribe { sensors: true },
            Request::SetTweak { id: "privacy.x".into(), enabled: true },
            Request::PutCustom(custom_action()),
            Request::RemoveCustom { id: "custom.scratch".into() },
            Request::Reapply { id: "privacy.x".into() },
            Request::SetProfile(Profile::default()),
            Request::SetLogging(LoggingConfig::default()),
            Request::SetFans(vec![]),
            Request::ClearHistory,
            Request::Shutdown,
            Request::SetUpdates(UpdateMode::Automatic),
            Request::CheckForUpdate,
            Request::InstallUpdate,
        ];
        for request in requests {
            assert_eq!(round_trip(&request), request);
        }
    }

    #[test]
    fn only_requests_that_neither_change_nor_reveal_configuration_are_open() {
        let open = [
            Request::Hello { protocol: 1 },
            Request::Status,
            Request::Catalog,
            Request::States,
            Request::Sensors,
            Request::SystemInfo,
            Request::Subscribe { sensors: false },
        ];
        assert!(open.iter().all(|request| !request.requires_elevation()));

        let privileged = [
            Request::Profile,
            Request::History { limit: 1 },
            Request::SetTweak { id: String::new(), enabled: false },
            Request::PutCustom(custom_action()),
            Request::RemoveCustom { id: String::new() },
            Request::Reapply { id: String::new() },
            Request::SetProfile(Profile::default()),
            Request::SetLogging(LoggingConfig::default()),
            Request::SetFans(vec![]),
            Request::ClearHistory,
            Request::Shutdown,
            Request::SetUpdates(UpdateMode::Off),
            Request::CheckForUpdate,
            Request::InstallUpdate,
        ];
        assert!(privileged.iter().all(Request::requires_elevation));
    }

    fn fan_status_with_every_availability() -> [FanStatus; 3] {
        let channel = FanChannelStatus {
            channel: "board.pwm2".into(),
            label: "Fan 2".into(),
            tach_sensor: Some("board.fan2".into()),
            min_duty_pct: 30,
            curve: vec![crate::profile::CurvePoint(40, 30), crate::profile::CurvePoint(80, 100)],
            sources: vec!["cpu.package.temp".into()],
            active: true,
            target_duty_pct: Some(45),
            phase: FanPhase::FailSafe,
            problem: Some("(pwm2): expected a reading | received: none".into()),
        };
        let board = "MACHINIST E5-MR9A PRO".to_owned();
        [
            FanStatus {
                availability: FanAvailability::Controllable,
                board: board.clone(),
                channels: vec![channel],
            },
            FanStatus {
                availability: FanAvailability::ReadOnly { reason: "not validated".into() },
                board,
                channels: vec![],
            },
            FanStatus::default(),
        ]
    }

    #[test]
    fn the_fan_status_survives_the_wire_in_every_availability() {
        for fans in fan_status_with_every_availability() {
            assert_eq!(round_trip(&fans), fans);
        }
        assert_eq!(
            FanStatus::default().availability,
            FanAvailability::NotInBuild,
            "(fan status): expected a build without the feature to be the default"
        );
    }

    #[test]
    fn the_update_status_survives_the_wire_in_every_state() {
        let states = [
            UpdateState::Idle,
            UpdateState::Checking,
            UpdateState::Available { version: "0.2.1".into() },
            UpdateState::Installing,
            UpdateState::Failed { reason: "github.com did not answer".into() },
            UpdateState::Unavailable,
        ];
        for state in states {
            let status = UpdateStatus {
                mode: UpdateMode::Automatic,
                state,
                checked_unix: Some(1_800_000_000),
            };
            assert_eq!(round_trip(&status), status);
        }
        let fresh = UpdateStatus::default();
        assert_eq!(
            (fresh.mode, fresh.state),
            (UpdateMode::Notify, UpdateState::Idle),
            "(update status): expected a fresh install to look and tell, nothing more"
        );
    }

    #[test]
    fn responses_round_trip() {
        let status = EntryStatus {
            id: "privacy.x".into(),
            enabled: true,
            state: ActionState::Contested { retry_in_secs: 120 },
            actions: vec![ActionStatus {
                state: ActionState::NotEnforceable { reason: "access denied".into() },
                current: "dword:3".into(),
            }],
            drift_count: 4,
            last_drift_unix: Some(1_700_000_000),
        };
        let responses = [
            Response::Hello(Welcome { protocol: 1, version: "0.1.0".into(), elevated: false }),
            Response::Ok,
            Response::Error(ServiceError { code: ErrorCode::AccessDenied, message: "no".into() }),
            Response::States(vec![status]),
            Response::History(vec![HistoryEvent {
                unix_secs: 1,
                id: "privacy.x".into(),
                kind: HistoryKind::Restored,
                detail: "dword:3 → dword:0".into(),
            }]),
            Response::SystemInfo(SystemInfo {
                summary: SystemSummary { memory_bytes: 64 << 30, ..SystemSummary::default() },
                sections: vec![InfoSection {
                    title: "Motherboard".into(),
                    rows: vec![("Product".into(), "E5-MR9A PRO".into())],
                }],
            }),
            Response::Subscribed,
            Response::Event(Event::StatesChanged),
        ];
        for response in responses {
            assert_eq!(round_trip(&response), response);
        }
    }
}
