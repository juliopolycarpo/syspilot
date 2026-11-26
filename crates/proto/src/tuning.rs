//! Hardware tuning: the limits, clocks and voltages a device lets SysPilot
//! change, what the profile asks for, and what the service and its tuner
//! exchange.
//!
//! A *device* is one piece of hardware with a vendor interface for tuning,
//! today a Radeon graphics card through AMD's driver. It offers *controls*,
//! each an integer with a range the device itself reports. The profile holds
//! a [`TuningSetting`] per device: the controls the user changed. Every
//! control the setting does not list keeps the device's default, so a setting
//! describes the whole device and turning it off puts the defaults back.
//!
//! The service never loads a vendor library. It starts `syspilot-tune.exe`
//! from its own directory for one job, writes one [`TuneJob`] frame to its
//! standard input and reads one [`TuneReport`] frame from its standard
//! output. Both always come from the same build, so this carries no version.
//! Frames use [`crate::frame`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// File name of the tuner executable, next to the service's.
pub const TUNER_EXECUTABLE: &str = "syspilot-tune.exe";
/// Most devices a profile may hold settings for.
pub const MAX_TUNING_DEVICES: usize = 16;
/// Most controls one setting may list.
pub const MAX_TUNING_VALUES: usize = 64;
/// Longest device or control key.
const KEY_MAX: usize = 64;

/// What a control changes. The app words each kind; a backend only says
/// which one a control is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TuningKind {
    /// How much power the device may draw, relative to its default.
    PowerLimit,
    /// The temperature at which the device slows itself down.
    TemperatureLimit,
    /// The temperature the device's own fan control aims for.
    FanTargetTemperature,
    /// The speed the fan may reach before the device slows down instead.
    FanMaxSpeed,
    /// The speed the fan never goes below while it turns.
    FanMinSpeed,
    /// The lowest clock the device slows down to in order to stay quiet.
    QuietClockFloor,
    /// The clock of one performance state of the processor core.
    CoreClock,
    /// The voltage of one performance state of the processor core.
    CoreVoltage,
    /// The clock of one performance state of the memory.
    MemoryClock,
    /// The voltage that goes with one performance state of the memory.
    MemoryVoltage,
}

/// The unit of a control's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TuningUnit {
    /// Percent, possibly negative (a change relative to the default).
    Percent,
    /// Degrees Celsius.
    Celsius,
    /// Megahertz.
    Megahertz,
    /// Millivolts.
    Millivolt,
    /// Revolutions per minute.
    Rpm,
}

impl TuningUnit {
    /// The unit as written after a value.
    pub fn suffix(self) -> &'static str {
        match self {
            Self::Percent => "%",
            Self::Celsius => "°C",
            Self::Megahertz => "MHz",
            Self::Millivolt => "mV",
            Self::Rpm => "RPM",
        }
    }
}

/// One thing a device lets SysPilot change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TuningControl {
    /// Stable key within the device, e.g. `power-limit` or `core-p7-clock`.
    /// A profile setting names the control by it.
    pub key: String,
    /// What the control changes.
    pub kind: TuningKind,
    /// The performance state the control belongs to, for the kinds that
    /// exist once per state. State 0 is the slowest.
    pub state: Option<u8>,
    /// The unit of every value below.
    pub unit: TuningUnit,
    /// Lowest value the device accepts.
    pub min: i32,
    /// Highest value the device accepts.
    pub max: i32,
    /// The increment the device suggests for a slider or a spin button. A
    /// hint only: devices accept values between steps, and their own
    /// defaults are often not on one.
    pub step: i32,
    /// The value the device uses when nothing was changed.
    pub default: i32,
    /// The value in effect now.
    pub current: i32,
    /// The value the profile asks for. `None` when the profile leaves the
    /// control at its default. Filled in by the service; a tuner's report
    /// leaves it empty.
    pub wanted: Option<i32>,
}

impl TuningControl {
    /// Whether the device accepts `value` for this control: inside its range.
    pub fn accepts(&self, value: i32) -> bool {
        (self.min..=self.max).contains(&value)
    }

    /// What the control holds while a setting is in effect: the wanted
    /// value, or the default for a control the setting does not list.
    pub fn expected(&self) -> i32 {
        self.wanted.unwrap_or(self.default)
    }

    /// What the control holds once `values` is in effect: its listed value,
    /// or the default when `values` does not list it.
    pub fn value_in(&self, values: &BTreeMap<String, i32>) -> i32 {
        values.get(&self.key).copied().unwrap_or(self.default)
    }
}

/// Why a value is not acceptable for a control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuningValueError {
    /// The device has no control with this key.
    UnknownControl {
        /// The key that was asked for.
        key: String,
    },
    /// The value is outside the range.
    OutOfRange {
        /// The control's key.
        key: String,
        /// The value that was asked for.
        value: i32,
        /// Lowest accepted value.
        min: i32,
        /// Highest accepted value.
        max: i32,
    },
}

impl std::fmt::Display for TuningValueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownControl { key } => {
                write!(f, "(tuning.values): expected a control of the device | received: {key:?}")
            }
            Self::OutOfRange { key, value, min, max } => {
                write!(f, "(tuning.values.{key}): expected {min}..={max} | received: {value}")
            }
        }
    }
}

impl std::error::Error for TuningValueError {}

/// Checks `values` against what a device offers. The first value that does
/// not fit is the error.
pub fn check_values(
    controls: &[TuningControl],
    values: &BTreeMap<String, i32>,
) -> Result<(), TuningValueError> {
    for (key, &value) in values {
        let Some(control) = controls.iter().find(|control| &control.key == key) else {
            return Err(TuningValueError::UnknownControl { key: key.clone() });
        };
        if !control.accepts(value) {
            return Err(TuningValueError::OutOfRange {
                key: key.clone(),
                value,
                min: control.min,
                max: control.max,
            });
        }
    }
    Ok(())
}

/// What the profile asks of one device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TuningSetting {
    /// The device key, e.g. `gpu.amd-699f.bus-3`, as the status lists it.
    pub device: String,
    /// Whether the setting is in effect. One that is not leaves the device
    /// to its driver.
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    /// The value per control key. A control that is not listed keeps the
    /// device's default.
    #[serde(default)]
    pub values: BTreeMap<String, i32>,
}

fn enabled_by_default() -> bool {
    true
}

/// Whether `key` is made of lowercase letters, digits, `.` and `-` only and
/// has an acceptable length. Keys are shown to the user and compared byte by
/// byte, so each has one spelling only.
pub fn is_tuning_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= KEY_MAX
        && key
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b".-".contains(&byte))
}

/// How a device stands relative to its profile setting.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TuningState {
    /// The profile has no setting in effect for the device: its driver, or
    /// another tool, decides.
    #[default]
    Unmanaged,
    /// Every control holds what the setting asks for.
    Applied,
    /// The setting is in effect but the device holds other values: it has not
    /// been applied yet, or something changed them.
    Different,
    /// Something keeps changing the values back. SysPilot stopped applying
    /// them until the next trigger: the settings change, it is asked again,
    /// or the service starts, the machine wakes or a user signs in.
    Contested,
    /// Not applied on purpose: the machine stopped within a minute of the
    /// last time these values were applied, so they are held back until the
    /// user changes them or asks for them again.
    Held,
    /// Applying did not work.
    Failed {
        /// Why, for the user.
        reason: String,
    },
    /// The setting names a device this machine does not have right now.
    Missing,
}

/// One tunable device as the service reports it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TuningDevice {
    /// Stable key, e.g. `gpu.amd-699f.bus-3`.
    pub key: String,
    /// The device's name, e.g. `Radeon RX 550 Series`.
    pub name: String,
    /// What the values travel through, e.g. `AMD Radeon driver (Overdrive N)`.
    pub interface: String,
    /// What can be changed. Empty for a device that is [`TuningState::Missing`].
    pub controls: Vec<TuningControl>,
    /// Whether the profile has a setting in effect for the device.
    pub enabled: bool,
    /// Where the device stands.
    pub state: TuningState,
}

impl TuningDevice {
    /// `device` as a tuner found it, with what `setting` asks for filled in
    /// as each control's `wanted` value.
    pub fn reported(
        device: &TunedDevice,
        setting: Option<&TuningSetting>,
        state: TuningState,
    ) -> Self {
        let wanted = |control: &TuningControl| {
            setting.and_then(|setting| setting.values.get(&control.key).copied())
        };
        Self {
            key: device.key.clone(),
            name: device.name.clone(),
            interface: device.interface.clone(),
            controls: device
                .controls
                .iter()
                .map(|control| TuningControl { wanted: wanted(control), ..control.clone() })
                .collect(),
            enabled: setting.is_some_and(|setting| setting.enabled),
            state,
        }
    }

    /// The row for a setting whose device is not there: named by its key,
    /// with nothing to change.
    pub fn missing(setting: &TuningSetting) -> Self {
        Self {
            key: setting.device.clone(),
            name: setting.device.clone(),
            interface: String::new(),
            controls: Vec::new(),
            enabled: setting.enabled,
            state: TuningState::Missing,
        }
    }
}

/// Tuning as the service sees it. Part of the service status.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TuningStatus {
    /// Every device with a tuning interface, then any device a profile
    /// setting names that is not there.
    pub devices: Vec<TuningDevice>,
    /// Why the devices could not be looked at, when that failed.
    pub problem: Option<String>,
}

/// What the tuner is asked to do. An empty job only reports.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TuneJob {
    /// Settings to put in effect: the listed values, and the default for
    /// every other control of the device.
    pub apply: Vec<TuningSetting>,
    /// Keys of devices to put back to their defaults.
    pub reset: Vec<String>,
}

impl TuneJob {
    /// Whether the job changes nothing.
    pub fn is_look(&self) -> bool {
        self.apply.is_empty() && self.reset.is_empty()
    }
}

/// One device as the tuner found it, after doing its job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunedDevice {
    /// Stable key.
    pub key: String,
    /// The device's name.
    pub name: String,
    /// What the values travel through.
    pub interface: String,
    /// The controls with the values in effect after the job.
    pub controls: Vec<TuningControl>,
    /// Why the job's part for this device did not work, if it did not.
    pub problem: Option<String>,
}

/// What the tuner found and did.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TuneReport {
    /// Every device with a tuning interface.
    pub devices: Vec<TunedDevice>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{read_frame, write_frame};

    fn control(key: &str, min: i32, max: i32, step: i32) -> TuningControl {
        TuningControl {
            key: key.into(),
            kind: TuningKind::PowerLimit,
            state: None,
            unit: TuningUnit::Percent,
            min,
            max,
            step,
            default: 0,
            current: 0,
            wanted: None,
        }
    }

    #[test]
    fn a_control_accepts_every_value_in_its_range_whatever_the_step() {
        let clock = control("core-p7-clock", 214, 1800, 5);
        // 1183 is this card's own default for its top state: not on a step.
        for good in [214, 215, 1183, 1800] {
            assert!(clock.accepts(good), "(clock): expected {good} to be accepted");
        }
        for bad in [213, 1801, i32::MIN, i32::MAX] {
            assert!(!clock.accepts(bad), "(clock): expected {bad} to be refused");
        }
    }

    #[test]
    fn a_control_expects_the_wanted_value_or_else_its_default() {
        let mut limit = TuningControl { default: 3, ..control("power-limit", -75, 75, 1) };
        assert_eq!(limit.expected(), 3);
        limit.wanted = Some(-20);
        assert_eq!(limit.expected(), -20);
    }

    #[test]
    fn a_control_holds_its_listed_value_or_else_its_default() {
        let limit = TuningControl { default: 3, ..control("power-limit", -75, 75, 1) };
        assert_eq!(limit.value_in(&BTreeMap::new()), 3);
        assert_eq!(limit.value_in(&[("power-limit".to_owned(), -20)].into()), -20);
        assert_eq!(limit.value_in(&[("core-p7-clock".to_owned(), 900)].into()), 3);
    }

    #[test]
    fn a_reported_device_carries_what_its_setting_wants() {
        let tuned = TunedDevice {
            key: "gpu.amd-699f.bus-3".into(),
            name: "Radeon RX 550 Series".into(),
            interface: "AMD Radeon driver (Overdrive N)".into(),
            controls: vec![
                control("power-limit", -75, 75, 1),
                control("core-p7-clock", 214, 1800, 5),
            ],
            problem: None,
        };
        let setting = TuningSetting {
            device: tuned.key.clone(),
            enabled: true,
            values: [("power-limit".to_owned(), -20)].into(),
        };
        let device = TuningDevice::reported(&tuned, Some(&setting), TuningState::Applied);
        let wanted: Vec<_> = device.controls.iter().map(|control| control.wanted).collect();
        assert_eq!(wanted, [Some(-20), None]);
        assert!(device.enabled, "(device.enabled): expected true for an enabled setting");
        assert_eq!(
            (device.name.as_str(), device.state),
            (tuned.name.as_str(), TuningState::Applied)
        );

        let unset = TuningDevice::reported(&tuned, None, TuningState::Unmanaged);
        assert!(!unset.enabled && unset.controls.iter().all(|control| control.wanted.is_none()));
    }

    #[test]
    fn a_missing_device_is_named_by_its_settings_key() {
        let setting = TuningSetting {
            device: "gpu.amd-67df.bus-4".into(),
            enabled: false,
            values: BTreeMap::new(),
        };
        let device = TuningDevice::missing(&setting);
        assert_eq!(
            (device.key.as_str(), device.name.as_str()),
            ("gpu.amd-67df.bus-4", "gpu.amd-67df.bus-4")
        );
        assert_eq!((device.enabled, device.state), (false, TuningState::Missing));
        assert!(device.controls.is_empty() && device.interface.is_empty());
    }

    #[test]
    fn values_are_checked_against_the_devices_controls() {
        let controls = [control("power-limit", -75, 75, 1), control("core-p7-clock", 214, 1800, 5)];
        let values = |pairs: &[(&str, i32)]| -> BTreeMap<String, i32> {
            pairs.iter().map(|(key, value)| ((*key).to_owned(), *value)).collect()
        };
        assert_eq!(check_values(&controls, &values(&[("power-limit", -20)])), Ok(()));
        assert_eq!(check_values(&controls, &BTreeMap::new()), Ok(()));

        let unknown = check_values(&controls, &values(&[("fan-max-speed", 3000)])).unwrap_err();
        assert_eq!(
            unknown.to_string(),
            "(tuning.values): expected a control of the device | received: \"fan-max-speed\""
        );
        let too_high = check_values(&controls, &values(&[("core-p7-clock", 1805)])).unwrap_err();
        assert_eq!(
            too_high.to_string(),
            "(tuning.values.core-p7-clock): expected 214..=1800 | received: 1805"
        );
    }

    #[test]
    fn keys_have_one_spelling() {
        for good in ["gpu.amd-699f.bus-3", "power-limit", "core-p7-clock", "a"] {
            assert!(is_tuning_key(good), "(key): expected {good:?} to be accepted");
        }
        let long = "a".repeat(65);
        for bad in ["", "Power-Limit", "power limit", "power_limit", "gpu/0", long.as_str()] {
            assert!(!is_tuning_key(bad), "(key): expected {bad:?} to be refused");
        }
    }

    #[test]
    fn every_unit_has_a_suffix() {
        let units = [
            (TuningUnit::Percent, "%"),
            (TuningUnit::Celsius, "°C"),
            (TuningUnit::Megahertz, "MHz"),
            (TuningUnit::Millivolt, "mV"),
            (TuningUnit::Rpm, "RPM"),
        ];
        for (unit, suffix) in units {
            assert_eq!(unit.suffix(), suffix);
        }
    }

    #[test]
    fn a_job_and_its_report_survive_the_wire() {
        let setting = TuningSetting {
            device: "gpu.amd-699f.bus-3".into(),
            enabled: true,
            values: [("power-limit".to_owned(), -20)].into(),
        };
        let job = TuneJob { apply: vec![setting], reset: vec!["gpu.amd-67df.bus-4".into()] };
        assert!(!job.is_look() && TuneJob::default().is_look());
        let report = TuneReport {
            devices: vec![TunedDevice {
                key: "gpu.amd-699f.bus-3".into(),
                name: "Radeon RX 550 Series".into(),
                interface: "AMD Radeon driver (Overdrive N)".into(),
                controls: vec![TuningControl {
                    state: Some(7),
                    kind: TuningKind::CoreClock,
                    unit: TuningUnit::Megahertz,
                    ..control("core-p7-clock", 214, 1800, 5)
                }],
                problem: Some("(power-limit): expected the driver to take it".into()),
            }],
        };
        let mut wire = Vec::new();
        write_frame(&mut wire, &job).unwrap();
        write_frame(&mut wire, &report).unwrap();
        let mut wire = std::io::Cursor::new(wire);
        assert_eq!(read_frame::<_, TuneJob>(&mut wire).unwrap(), job);
        assert_eq!(read_frame::<_, TuneReport>(&mut wire).unwrap(), report);
    }

    #[test]
    fn the_status_survives_the_wire_in_every_state() {
        let states = [
            TuningState::Unmanaged,
            TuningState::Applied,
            TuningState::Different,
            TuningState::Contested,
            TuningState::Held,
            TuningState::Failed { reason: "the driver refused".into() },
            TuningState::Missing,
        ];
        for state in states {
            let status = TuningStatus {
                devices: vec![TuningDevice {
                    key: "gpu.amd-699f.bus-3".into(),
                    name: "Radeon RX 550 Series".into(),
                    interface: "AMD Radeon driver (Overdrive N)".into(),
                    controls: vec![TuningControl {
                        wanted: Some(-20),
                        ..control("power-limit", -75, 75, 1)
                    }],
                    enabled: true,
                    state,
                }],
                problem: None,
            };
            let mut wire = Vec::new();
            write_frame(&mut wire, &status).unwrap();
            assert_eq!(
                read_frame::<_, TuningStatus>(&mut std::io::Cursor::new(wire)).unwrap(),
                status
            );
        }
        assert_eq!(TuningDevice::default().state, TuningState::Unmanaged);
    }
}
