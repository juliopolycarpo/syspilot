//! AMD Radeon graphics cards through the driver's Overdrive N interface
//! (the RX 400 and 500 series and the Vega cards).
//!
//! The driver describes what it allows: a set of flags for what can be
//! changed at all, a range for each quantity, and the performance states
//! with a mark on each for whether its clock and voltage can be edited.
//! This module turns that into controls and back, and never writes a value
//! outside the range the driver gave.
//!
//! The driver groups its values into four structures (core states, memory
//! states, fan, power) and takes each as a whole. Applying a setting writes
//! a structure only when what it should hold differs from what it holds: its
//! defaults with the setting's values laid over them. A structure nothing is
//! set in goes back to the driver's own defaults the way AMD's sample does
//! it. Newer cards (RX 5000 and later) speak Overdrive 8 and are not listed.
//!
//! The calls themselves are behind [`AdlApi`], so the logic runs against
//! [`FakeAdl`] in tests and never touches a card there.

use std::collections::BTreeMap;
use std::fmt;

use syspilot_proto::tuning::{TunedDevice, TuningControl, TuningKind, TuningUnit, check_values};

use super::TuningBackend;

/// The Overdrive generation this module speaks.
pub const OVERDRIVE_N: i32 = 7;
/// What the values of a Radeon card travel through, for the user.
pub const INTERFACE: &str = "AMD Radeon driver (Overdrive N)";

/// `ADLODNControlType`: which values a call reads or what a write means.
pub mod mode {
    /// The values in effect.
    pub const CURRENT: i32 = 0;
    /// The driver's defaults.
    pub const DEFAULT: i32 = 1;
    /// The driver decides: how a fan or power structure is put back.
    pub const AUTO: i32 = 2;
    /// The caller's values.
    pub const MANUAL: i32 = 3;
}

/// `ADLODNFeatureControl`: what the card lets a program change.
pub mod feature {
    /// Core clocks per state.
    pub const SCLK_DPM: i32 = 1 << 0;
    /// Memory clocks per state.
    pub const MCLK_DPM: i32 = 1 << 1;
    /// Core voltages per state.
    pub const SCLK_VDD: i32 = 1 << 2;
    /// Memory voltages per state.
    pub const MCLK_VDD: i32 = 1 << 3;
    /// The fan's lowest speed.
    pub const FAN_SPEED_MIN: i32 = 1 << 4;
    /// The fan's target speed.
    pub const FAN_SPEED_TARGET: i32 = 1 << 5;
    /// The lowest clock the card throttles to for the sake of the fan.
    pub const ACOUSTIC_LIMIT_SCLK: i32 = 1 << 6;
    /// The temperature the fan aims for.
    pub const TEMPERATURE_FAN_MAX: i32 = 1 << 7;
    /// The highest operating temperature.
    pub const TEMPERATURE_SYSTEM: i32 = 1 << 8;
    /// The power limit.
    pub const POWER_LIMIT: i32 = 1 << 9;
    /// A memory clock may be set below its default. Without it the default
    /// of a state is the lowest clock that state takes, whatever the range
    /// says (`ADL_ODN_MCLK_UNDERCLOCK_ENABLE`).
    pub const MCLK_UNDERCLOCK: i32 = 1 << 14;
}

/// `ADLODNDPMMaskType`: what can be edited in one performance state.
pub mod editable {
    /// The state's clock.
    pub const CLOCK: i32 = 1 << 0;
    /// The state's voltage.
    pub const VDDC: i32 = 1 << 1;
}

/// `ADLODNParameterRange`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OdnRange {
    /// Not used by the driver for a range.
    pub mode: i32,
    /// Lowest value.
    pub min: i32,
    /// Highest value.
    pub max: i32,
    /// Suggested increment.
    pub step: i32,
    /// Default value.
    pub default: i32,
}

/// `ADLODNCapabilitiesX2`. Clock ranges are in units of 10 kHz, except the
/// minimum performance clock, which is in MHz.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OdnCapabilities {
    /// How many performance states a levels structure holds.
    pub maximum_levels: i32,
    /// [`feature`] flags.
    pub flags: i32,
    /// Core clock.
    pub engine_clock: OdnRange,
    /// Memory clock.
    pub memory_clock: OdnRange,
    /// Voltage, mV.
    pub vddc: OdnRange,
    /// Power limit, percent.
    pub power: OdnRange,
    /// Highest operating temperature, °C.
    pub power_tune_temperature: OdnRange,
    /// Fan target temperature, °C.
    pub fan_temperature: OdnRange,
    /// Fan speed, RPM.
    pub fan_speed: OdnRange,
    /// Minimum performance clock, MHz.
    pub minimum_performance_clock: OdnRange,
    /// Not used here.
    pub throttle_notification: OdnRange,
    /// Not used here.
    pub auto_system_clock: OdnRange,
}

/// `ADLODNPerformanceLevelX2`: one performance state.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OdnLevel {
    /// Clock in units of 10 kHz. Zero for a state the card does not have.
    pub clock: i32,
    /// Voltage, mV.
    pub vddc: i32,
    /// Whether the state is in use. The driver reads back a bit per state
    /// and expects 0 or 1 in a write.
    pub enabled: i32,
    /// [`editable`] flags.
    pub control: i32,
}

/// The performance states of the core or of the memory
/// (`ADLODNPerformanceLevelsX2` without its size fields).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OdnLevels {
    /// A [`mode`].
    pub mode: i32,
    /// The states, slowest first. Always as many as the capabilities say,
    /// with zeroed entries for states the card lacks.
    pub levels: Vec<OdnLevel>,
}

/// `ADLODNFanControl`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OdnFan {
    /// A [`mode`].
    pub mode: i32,
    /// The fan's own control mode; passed back as read.
    pub fan_control_mode: i32,
    /// How the current speed is expressed; passed back as read.
    pub current_fan_speed_mode: i32,
    /// The current speed.
    pub current_fan_speed: i32,
    /// The speed the fan may reach, RPM.
    pub target_fan_speed: i32,
    /// The temperature the fan aims for, °C.
    pub target_temperature: i32,
    /// The lowest clock the card throttles to, MHz.
    pub min_performance_clock: i32,
    /// The fan's lowest speed, RPM.
    pub min_fan_limit: i32,
}

/// `ADLODNPowerLimitSetting`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OdnPower {
    /// A [`mode`].
    pub mode: i32,
    /// Power limit, percent relative to the default.
    pub tdp_limit: i32,
    /// Highest operating temperature, °C.
    pub max_operating_temperature: i32,
}

/// One output of a card as the driver lists it. A card has one entry per
/// display output, all with the same bus location.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AdlAdapter {
    /// The index every other call takes.
    pub index: i32,
    /// PCI bus number.
    pub bus: i32,
    /// PCI device number.
    pub device: i32,
    /// PCI function number.
    pub function: i32,
    /// The card's name.
    pub name: String,
    /// The Plug and Play ID, e.g. `PCI\VEN_1002&DEV_699F&SUBSYS_…`.
    pub pnp: String,
}

/// A call into the driver's library that did not work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdlError {
    /// The library is not on this machine: no AMD driver is installed.
    NotInstalled,
    /// The library lacks a function this module needs.
    MissingExport(&'static str),
    /// A function returned something other than `ADL_OK`.
    Call {
        /// The function.
        function: &'static str,
        /// What it returned.
        code: i32,
    },
}

impl fmt::Display for AdlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInstalled => {
                f.write_str("(atiadlxx.dll): expected the AMD driver's library | received: none")
            }
            Self::MissingExport(function) => {
                write!(f, "(atiadlxx.dll): expected the function {function} | received: none")
            }
            Self::Call { function, code } => {
                write!(f, "({function}): expected ADL_OK (0) | received: {code}")
            }
        }
    }
}

impl std::error::Error for AdlError {}

/// The calls of the driver's library this module makes.
pub trait AdlApi {
    /// Every output of every card.
    fn adapters(&mut self) -> Result<Vec<AdlAdapter>, AdlError>;
    /// The Overdrive generation of the card behind `adapter`, when Overdrive
    /// is supported and enabled on it.
    fn overdrive_version(&mut self, adapter: i32) -> Option<i32>;
    /// What the card allows.
    fn capabilities(&mut self, adapter: i32) -> Result<OdnCapabilities, AdlError>;
    /// `count` core states, the current ones or the defaults by `mode`.
    fn system_clocks(
        &mut self,
        adapter: i32,
        mode: i32,
        count: usize,
    ) -> Result<OdnLevels, AdlError>;
    /// Writes the core states.
    fn set_system_clocks(&mut self, adapter: i32, levels: &OdnLevels) -> Result<(), AdlError>;
    /// `count` memory states, the current ones or the defaults by `mode`.
    fn memory_clocks(
        &mut self,
        adapter: i32,
        mode: i32,
        count: usize,
    ) -> Result<OdnLevels, AdlError>;
    /// Writes the memory states.
    fn set_memory_clocks(&mut self, adapter: i32, levels: &OdnLevels) -> Result<(), AdlError>;
    /// The fan structure in effect.
    fn fan(&mut self, adapter: i32) -> Result<OdnFan, AdlError>;
    /// Writes the fan structure.
    fn set_fan(&mut self, adapter: i32, fan: &OdnFan) -> Result<(), AdlError>;
    /// The power structure in effect.
    fn power(&mut self, adapter: i32) -> Result<OdnPower, AdlError>;
    /// Writes the power structure.
    fn set_power(&mut self, adapter: i32, power: &OdnPower) -> Result<(), AdlError>;
}

/// Hundredths of a megahertz, the driver's unit for state clocks, per MHz.
const CLOCK_UNITS_PER_MHZ: i32 = 100;

/// The device key of a card: its PCI device ID and bus, which stay the same
/// across boots and driver updates, unlike the driver's adapter index.
fn device_key(adapter: &AdlAdapter) -> String {
    let upper = adapter.pnp.to_ascii_uppercase();
    let id: String = upper
        .split_once("DEV_")
        .map(|(_, rest)| rest.chars().take_while(char::is_ascii_hexdigit).collect())
        .unwrap_or_default();
    let id = if id.is_empty() { "unknown".to_owned() } else { id.to_ascii_lowercase() };
    format!("gpu.amd-{id}.bus-{}", adapter.bus)
}

/// One entry per card: the first output of each bus location.
///
/// The driver's "present" flag of an output is not consulted. It says whether
/// the output is part of the caller's desktop, and is off for every output
/// when the caller is a service. The library is asked for connected cards
/// only, which is what keeps a card that was removed out of the list.
fn cards(adapters: Vec<AdlAdapter>) -> Vec<AdlAdapter> {
    let mut cards: Vec<AdlAdapter> = Vec::new();
    for adapter in adapters {
        let location = (adapter.bus, adapter.device, adapter.function);
        if cards.iter().all(|card| (card.bus, card.device, card.function) != location) {
            cards.push(adapter);
        }
    }
    cards
}

/// Which of the driver's four structures a control lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    Core,
    Memory,
    Fan,
    Power,
}

/// Where a control's value sits in the driver's structures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    PowerLimit,
    TemperatureLimit,
    FanTargetTemperature,
    FanMaxSpeed,
    FanMinSpeed,
    QuietClockFloor,
    /// The clock of core (`false`) or memory (`true`) state `index`.
    Clock {
        memory: bool,
        index: usize,
    },
    /// The voltage of core or memory state `index`.
    Voltage {
        memory: bool,
        index: usize,
    },
}

impl Slot {
    fn group(self) -> Group {
        match self {
            Self::PowerLimit | Self::TemperatureLimit => Group::Power,
            Self::FanTargetTemperature
            | Self::FanMaxSpeed
            | Self::FanMinSpeed
            | Self::QuietClockFloor => Group::Fan,
            Self::Clock { memory, .. } | Self::Voltage { memory, .. } => {
                if memory {
                    Group::Memory
                } else {
                    Group::Core
                }
            }
        }
    }
}

/// Everything the driver said about one card.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CardState {
    capabilities: OdnCapabilities,
    core: OdnLevels,
    core_default: OdnLevels,
    memory: OdnLevels,
    memory_default: OdnLevels,
    fan: OdnFan,
    power: OdnPower,
}

/// A range in the control's unit, or `None` when the driver gave none.
fn usable(range: OdnRange, divisor: i32) -> Option<(i32, i32, i32)> {
    // Rounded inwards, so a converted bound is never outside the driver's.
    let min = range.min.div_euclid(divisor) + i32::from(range.min.rem_euclid(divisor) != 0);
    let max = range.max.div_euclid(divisor);
    (min < max).then_some((min, max, (range.step / divisor).max(1)))
}

impl CardState {
    fn read(adl: &mut dyn AdlApi, adapter: i32) -> Result<Self, AdlError> {
        let capabilities = adl.capabilities(adapter)?;
        let count = usize::try_from(capabilities.maximum_levels).unwrap_or(0);
        Ok(Self {
            capabilities,
            core: adl.system_clocks(adapter, mode::CURRENT, count)?,
            core_default: adl.system_clocks(adapter, mode::DEFAULT, count)?,
            memory: adl.memory_clocks(adapter, mode::CURRENT, count)?,
            memory_default: adl.memory_clocks(adapter, mode::DEFAULT, count)?,
            fan: adl.fan(adapter)?,
            power: adl.power(adapter)?,
        })
    }

    fn has(&self, flag: i32) -> bool {
        self.capabilities.flags & flag == flag
    }

    /// The controls of the card, each with the slot its value sits in.
    fn controls(&self) -> Vec<(TuningControl, Slot)> {
        let caps = &self.capabilities;
        let mut controls = Vec::new();
        let mut single =
            |key: &str, kind, unit, range: OdnRange, default: i32, current: i32, slot: Slot| {
                if let Some((min, max, step)) = usable(range, 1) {
                    controls.push((
                        TuningControl {
                            key: key.to_owned(),
                            kind,
                            state: None,
                            unit,
                            min,
                            max,
                            step,
                            default,
                            current,
                            wanted: None,
                        },
                        slot,
                    ));
                }
            };
        if self.has(feature::POWER_LIMIT) {
            single(
                "power-limit",
                TuningKind::PowerLimit,
                TuningUnit::Percent,
                caps.power,
                caps.power.default,
                self.power.tdp_limit,
                Slot::PowerLimit,
            );
        }
        if self.has(feature::TEMPERATURE_SYSTEM) {
            single(
                "temperature-limit",
                TuningKind::TemperatureLimit,
                TuningUnit::Celsius,
                caps.power_tune_temperature,
                caps.power_tune_temperature.default,
                self.power.max_operating_temperature,
                Slot::TemperatureLimit,
            );
        }
        if self.has(feature::TEMPERATURE_FAN_MAX) {
            single(
                "fan-target-temperature",
                TuningKind::FanTargetTemperature,
                TuningUnit::Celsius,
                caps.fan_temperature,
                caps.fan_temperature.default,
                self.fan.target_temperature,
                Slot::FanTargetTemperature,
            );
        }
        if self.has(feature::FAN_SPEED_TARGET) {
            single(
                "fan-max-speed",
                TuningKind::FanMaxSpeed,
                TuningUnit::Rpm,
                caps.fan_speed,
                caps.fan_speed.default,
                self.fan.target_fan_speed,
                Slot::FanMaxSpeed,
            );
        }
        if self.has(feature::FAN_SPEED_MIN) {
            // The driver gives one range for both fan speeds and its default
            // is the target's; the lowest speed defaults to the range's start.
            single(
                "fan-min-speed",
                TuningKind::FanMinSpeed,
                TuningUnit::Rpm,
                caps.fan_speed,
                caps.fan_speed.min,
                self.fan.min_fan_limit,
                Slot::FanMinSpeed,
            );
        }
        if self.has(feature::ACOUSTIC_LIMIT_SCLK) {
            single(
                "quiet-clock-floor",
                TuningKind::QuietClockFloor,
                TuningUnit::Megahertz,
                caps.minimum_performance_clock,
                caps.minimum_performance_clock.default,
                self.fan.min_performance_clock,
                Slot::QuietClockFloor,
            );
        }
        self.state_controls(false, &mut controls);
        self.state_controls(true, &mut controls);
        controls
    }

    /// The per-state controls of the core (`memory` false) or the memory.
    fn state_controls(&self, memory: bool, controls: &mut Vec<(TuningControl, Slot)>) {
        let caps = &self.capabilities;
        let (levels, defaults, clock_range, prefix) = if memory {
            (&self.memory, &self.memory_default, caps.memory_clock, "memory")
        } else {
            (&self.core, &self.core_default, caps.engine_clock, "core")
        };
        let (clock_flag, voltage_flag) = if memory {
            (feature::MCLK_DPM, feature::MCLK_VDD)
        } else {
            (feature::SCLK_DPM, feature::SCLK_VDD)
        };
        let (clock_kind, voltage_kind) = if memory {
            (TuningKind::MemoryClock, TuningKind::MemoryVoltage)
        } else {
            (TuningKind::CoreClock, TuningKind::CoreVoltage)
        };
        let clock_range = usable(clock_range, CLOCK_UNITS_PER_MHZ).filter(|_| self.has(clock_flag));
        let voltage_range = usable(caps.vddc, 1).filter(|_| self.has(voltage_flag));
        for (index, (level, default)) in levels.levels.iter().zip(&defaults.levels).enumerate() {
            // A zeroed entry is a state the card does not have.
            if level.clock <= 0 || default.clock <= 0 {
                continue;
            }
            let state = u8::try_from(index).ok();
            let mut push = |suffix: &str, kind, unit, (min, max, step), default, current, slot| {
                controls.push((
                    TuningControl {
                        key: format!("{prefix}-p{index}-{suffix}"),
                        kind,
                        state,
                        unit,
                        min,
                        max,
                        step,
                        default,
                        current,
                        wanted: None,
                    },
                    slot,
                ));
            };
            if let Some((min, max, step)) =
                clock_range.filter(|_| level.control & editable::CLOCK != 0)
            {
                let default_mhz = default.clock / CLOCK_UNITS_PER_MHZ;
                // A card that does not allow memory underclocking takes no
                // memory clock below the state's default.
                let floor = memory && !self.has(feature::MCLK_UNDERCLOCK);
                let min = if floor { min.max(default_mhz.min(max)) } else { min };
                push(
                    "clock",
                    clock_kind,
                    TuningUnit::Megahertz,
                    (min, max, step),
                    default_mhz,
                    level.clock / CLOCK_UNITS_PER_MHZ,
                    Slot::Clock { memory, index },
                );
            }
            if let Some(range) = voltage_range.filter(|_| level.control & editable::VDDC != 0) {
                push(
                    "voltage",
                    voltage_kind,
                    TuningUnit::Millivolt,
                    range,
                    default.vddc,
                    level.vddc,
                    Slot::Voltage { memory, index },
                );
            }
        }
    }
}

/// The states a levels structure should hold: the defaults with `values`
/// laid over them, in the form the driver takes for a manual write.
fn wanted_levels(
    defaults: &OdnLevels,
    controls: &[(TuningControl, Slot)],
    values: &BTreeMap<String, i32>,
    memory: bool,
) -> OdnLevels {
    let mut wanted = defaults.clone();
    wanted.mode = mode::MANUAL;
    for level in &mut wanted.levels {
        level.enabled = i32::from(level.enabled > 0);
    }
    for (control, slot) in controls {
        let Some(&value) = values.get(&control.key) else { continue };
        match *slot {
            Slot::Clock { memory: of_memory, index } if of_memory == memory => {
                // Whole megahertz only: the fraction of a default is given up
                // when the user sets the clock.
                wanted.levels[index].clock = value * CLOCK_UNITS_PER_MHZ;
            }
            Slot::Voltage { memory: of_memory, index } if of_memory == memory => {
                wanted.levels[index].vddc = value;
            }
            _ => {}
        }
    }
    wanted
}

/// Whether two levels structures hold the same clocks and voltages.
fn same_states(left: &OdnLevels, right: &OdnLevels) -> bool {
    left.levels.len() == right.levels.len()
        && left
            .levels
            .iter()
            .zip(&right.levels)
            .all(|(a, b)| (a.clock, a.vddc) == (b.clock, b.vddc))
}

/// Refuses states whose clocks fall from one state to the next: a faster
/// state must not be slower than the one below it.
fn check_rising(levels: &OdnLevels, what: &str) -> Result<(), String> {
    let real: Vec<(usize, &OdnLevel)> =
        levels.levels.iter().enumerate().filter(|(_, level)| level.clock > 0).collect();
    for pair in real.windows(2) {
        let ((below, slower), (index, faster)) = (pair[0], pair[1]);
        if faster.clock < slower.clock {
            return Err(format!(
                "({what}-p{index}-clock): expected at least the {} MHz of state {below} | received: {} MHz",
                slower.clock / CLOCK_UNITS_PER_MHZ,
                faster.clock / CLOCK_UNITS_PER_MHZ
            ));
        }
    }
    Ok(())
}

/// AMD Radeon cards on Overdrive N, as a tuning backend.
pub struct Radeon<A> {
    adl: A,
}

impl<A: AdlApi> Radeon<A> {
    /// A backend over the driver's library.
    pub fn new(adl: A) -> Self {
        Self { adl }
    }

    /// The cards that speak Overdrive N, each with its key.
    fn cards(&mut self) -> Vec<(String, AdlAdapter)> {
        let adapters = self.adl.adapters().unwrap_or_default();
        cards(adapters)
            .into_iter()
            .filter(|card| self.adl.overdrive_version(card.index) == Some(OVERDRIVE_N))
            .map(|card| (device_key(&card), card))
            .collect()
    }

    fn card(&mut self, key: &str) -> Result<AdlAdapter, String> {
        self.cards().into_iter().find(|(known, _)| known == key).map(|(_, card)| card).ok_or_else(
            || {
                format!(
                    "(tuning device): expected a Radeon card on Overdrive N | received: {key:?}"
                )
            },
        )
    }

    /// Writes one of the level structures if it does not hold `wanted`.
    fn write_levels(
        &mut self,
        adapter: i32,
        memory: bool,
        current: &OdnLevels,
        defaults: &OdnLevels,
        wanted: &OdnLevels,
    ) -> Result<(), AdlError> {
        if same_states(current, wanted) {
            return Ok(());
        }
        // Back to the defaults is the driver's own reset: its defaults
        // written back in default mode, as its sample does.
        let levels = if same_states(defaults, wanted) { defaults } else { wanted };
        if memory {
            self.adl.set_memory_clocks(adapter, levels)
        } else {
            self.adl.set_system_clocks(adapter, levels)
        }
    }
}

impl<A: AdlApi> TuningBackend for Radeon<A> {
    fn devices(&mut self) -> Vec<TunedDevice> {
        self.cards()
            .into_iter()
            .map(|(key, card)| {
                let (controls, problem) = match CardState::read(&mut self.adl, card.index) {
                    Ok(state) => {
                        (state.controls().into_iter().map(|(control, _)| control).collect(), None)
                    }
                    Err(error) => (Vec::new(), Some(error.to_string())),
                };
                TunedDevice {
                    key,
                    name: card.name,
                    interface: INTERFACE.to_owned(),
                    controls,
                    problem,
                }
            })
            .collect()
    }

    fn apply(&mut self, key: &str, values: &BTreeMap<String, i32>) -> Result<(), String> {
        let adapter = self.card(key)?.index;
        let state = CardState::read(&mut self.adl, adapter).map_err(|error| error.to_string())?;
        let controls = state.controls();
        let plain: Vec<TuningControl> =
            controls.iter().map(|(control, _)| control.clone()).collect();
        check_values(&plain, values).map_err(|problem| problem.to_string())?;

        // Everything is worked out and checked before the first write.
        let core = wanted_levels(&state.core_default, &controls, values, false);
        let memory = wanted_levels(&state.memory_default, &controls, values, true);
        check_rising(&core, "core")?;
        check_rising(&memory, "memory")?;

        let value_of = |slot: Slot| {
            let control = controls.iter().find(|(_, own)| *own == slot).map(|(control, _)| control);
            control.map(|control| values.get(&control.key).copied().unwrap_or(control.default))
        };
        let set_in = |group: Group| {
            controls
                .iter()
                .any(|(control, slot)| slot.group() == group && values.contains_key(&control.key))
        };
        let has = |group: Group| controls.iter().any(|(_, slot)| slot.group() == group);

        let mut fan = state.fan;
        fan.target_temperature =
            value_of(Slot::FanTargetTemperature).unwrap_or(fan.target_temperature);
        fan.target_fan_speed = value_of(Slot::FanMaxSpeed).unwrap_or(fan.target_fan_speed);
        fan.min_fan_limit = value_of(Slot::FanMinSpeed).unwrap_or(fan.min_fan_limit);
        fan.min_performance_clock =
            value_of(Slot::QuietClockFloor).unwrap_or(fan.min_performance_clock);
        fan.mode = if set_in(Group::Fan) { mode::MANUAL } else { mode::AUTO };
        // One range covers both speeds, so each can pass it and the pair
        // still be upside down.
        let both_speeds = value_of(Slot::FanMinSpeed).zip(value_of(Slot::FanMaxSpeed));
        if let Some((lowest, highest)) = both_speeds.filter(|(lowest, highest)| lowest > highest) {
            return Err(format!(
                "(tuning.values.fan-min-speed): expected at most the {highest} RPM of \
                 fan-max-speed | received: {lowest} RPM"
            ));
        }

        let mut power = state.power;
        power.tdp_limit = value_of(Slot::PowerLimit).unwrap_or(power.tdp_limit);
        power.max_operating_temperature =
            value_of(Slot::TemperatureLimit).unwrap_or(power.max_operating_temperature);
        power.mode = if set_in(Group::Power) { mode::MANUAL } else { mode::AUTO };

        let written = (|| {
            if has(Group::Core) {
                self.write_levels(adapter, false, &state.core, &state.core_default, &core)?;
            }
            if has(Group::Memory) {
                self.write_levels(adapter, true, &state.memory, &state.memory_default, &memory)?;
            }
            // The mode is not compared: the driver does not report it back.
            let same_fan = OdnFan { mode: state.fan.mode, ..fan } == state.fan;
            if has(Group::Fan) && !same_fan {
                self.adl.set_fan(adapter, &fan)?;
            }
            let same_power = OdnPower { mode: state.power.mode, ..power } == state.power;
            if has(Group::Power) && !same_power {
                self.adl.set_power(adapter, &power)?;
            }
            Ok(())
        })();
        written.map_err(|error: AdlError| error.to_string())
    }

    fn reset(&mut self, key: &str) -> Result<(), String> {
        // Every control at its default is a setting with no values.
        self.apply(key, &BTreeMap::new())
    }
}

/// A driver library held in memory: cards with their structures, and every
/// write recorded.
#[derive(Debug, Clone, Default)]
pub struct FakeAdl {
    /// The outputs the library lists.
    pub adapters: Vec<AdlAdapter>,
    /// The Overdrive generation of every card; `None` for no Overdrive.
    pub version: Option<i32>,
    /// What the cards allow.
    pub capabilities: OdnCapabilities,
    /// Core states in effect and by default.
    pub core: (OdnLevels, OdnLevels),
    /// Memory states in effect and by default.
    pub memory: (OdnLevels, OdnLevels),
    /// The fan structure in effect.
    pub fan: OdnFan,
    /// The power structure in effect.
    pub power: OdnPower,
    /// Names of the functions that fail, with the code they return.
    pub failing: BTreeMap<&'static str, i32>,
    /// Every write, by function name, in order.
    pub writes: Vec<&'static str>,
}

impl FakeAdl {
    fn check(&self, function: &'static str) -> Result<(), AdlError> {
        match self.failing.get(function) {
            Some(&code) => Err(AdlError::Call { function, code }),
            None => Ok(()),
        }
    }

    fn levels(pair: &(OdnLevels, OdnLevels), mode: i32, count: usize) -> OdnLevels {
        let mut levels = if mode == mode::DEFAULT { pair.1.clone() } else { pair.0.clone() };
        levels.mode = mode;
        levels.levels.resize(count, OdnLevel::default());
        levels
    }

    /// What a levels write leaves in effect, as the driver does it: the
    /// defaults for a write in default mode, else the values written.
    fn store(pair: &mut (OdnLevels, OdnLevels), written: &OdnLevels) {
        pair.0 = if written.mode == mode::DEFAULT { pair.1.clone() } else { written.clone() };
        pair.0.mode = mode::CURRENT;
    }
}

impl AdlApi for FakeAdl {
    fn adapters(&mut self) -> Result<Vec<AdlAdapter>, AdlError> {
        self.check("ADL2_Adapter_AdapterInfo_Get")?;
        Ok(self.adapters.clone())
    }

    fn overdrive_version(&mut self, _adapter: i32) -> Option<i32> {
        self.version
    }

    fn capabilities(&mut self, _adapter: i32) -> Result<OdnCapabilities, AdlError> {
        self.check("ADL2_OverdriveN_CapabilitiesX2_Get")?;
        Ok(self.capabilities)
    }

    fn system_clocks(
        &mut self,
        _adapter: i32,
        mode: i32,
        count: usize,
    ) -> Result<OdnLevels, AdlError> {
        self.check("ADL2_OverdriveN_SystemClocksX2_Get")?;
        Ok(Self::levels(&self.core, mode, count))
    }

    fn set_system_clocks(&mut self, _adapter: i32, levels: &OdnLevels) -> Result<(), AdlError> {
        self.writes.push("ADL2_OverdriveN_SystemClocksX2_Set");
        self.check("ADL2_OverdriveN_SystemClocksX2_Set")?;
        Self::store(&mut self.core, levels);
        Ok(())
    }

    fn memory_clocks(
        &mut self,
        _adapter: i32,
        mode: i32,
        count: usize,
    ) -> Result<OdnLevels, AdlError> {
        self.check("ADL2_OverdriveN_MemoryClocksX2_Get")?;
        Ok(Self::levels(&self.memory, mode, count))
    }

    fn set_memory_clocks(&mut self, _adapter: i32, levels: &OdnLevels) -> Result<(), AdlError> {
        self.writes.push("ADL2_OverdriveN_MemoryClocksX2_Set");
        self.check("ADL2_OverdriveN_MemoryClocksX2_Set")?;
        Self::store(&mut self.memory, levels);
        Ok(())
    }

    fn fan(&mut self, _adapter: i32) -> Result<OdnFan, AdlError> {
        self.check("ADL2_OverdriveN_FanControl_Get")?;
        Ok(self.fan)
    }

    fn set_fan(&mut self, _adapter: i32, fan: &OdnFan) -> Result<(), AdlError> {
        self.writes.push("ADL2_OverdriveN_FanControl_Set");
        self.check("ADL2_OverdriveN_FanControl_Set")?;
        // The driver does not report the mode of a write back.
        self.fan = OdnFan { mode: mode::CURRENT, ..*fan };
        Ok(())
    }

    fn power(&mut self, _adapter: i32) -> Result<OdnPower, AdlError> {
        self.check("ADL2_OverdriveN_PowerLimit_Get")?;
        Ok(self.power)
    }

    fn set_power(&mut self, _adapter: i32, power: &OdnPower) -> Result<(), AdlError> {
        self.writes.push("ADL2_OverdriveN_PowerLimit_Set");
        self.check("ADL2_OverdriveN_PowerLimit_Set")?;
        self.power = OdnPower { mode: mode::CURRENT, ..*power };
        Ok(())
    }
}

impl FakeAdl {
    /// The reference machine's Radeon RX 550 as its driver (31.0.21925.1001)
    /// described it on 2026-11-25: every structure below is what the library
    /// returned, with nothing changed from the defaults.
    pub fn rx550() -> Self {
        let range = |min, max, step, default| OdnRange { mode: 0, min, max, step, default };
        let level = |clock, vddc, enabled, control| OdnLevel { clock, vddc, enabled, control };
        let padded = |mut levels: Vec<OdnLevel>| {
            levels.resize(8, OdnLevel::default());
            OdnLevels { mode: mode::CURRENT, levels }
        };
        let core = padded(vec![
            level(21_400, 700, 1, 4),
            level(55_100, 750, 2, 7),
            level(73_400, 750, 4, 7),
            level(98_000, 875, 8, 7),
            level(104_600, 900, 16, 7),
            level(109_800, 950, 32, 7),
            level(112_400, 1000, 64, 7),
            level(118_300, 1050, 128, 7),
        ]);
        let memory = padded(vec![
            level(30_000, 700, 1, 4),
            level(62_500, 700, 2, 4),
            level(150_000, 875, 4, 7),
        ]);
        // Six outputs, all on the one card.
        let adapters = (0..6)
            .map(|index| AdlAdapter {
                index,
                bus: 3,
                device: 0,
                function: 0,
                name: "Radeon RX550/550 Series".into(),
                pnp: r"PCI\VEN_1002&DEV_699F&SUBSYS_0B041002&REV_C7\4&16866DE7&0&0010".into(),
            })
            .collect();
        Self {
            adapters,
            version: Some(OVERDRIVE_N),
            capabilities: OdnCapabilities {
                maximum_levels: 8,
                flags: 0x4000_724F,
                engine_clock: range(21_400, 180_000, 500, 21_400),
                memory_clock: range(100_000, 200_000, 500, 30_000),
                vddc: range(700, 1075, 1, 1075),
                power: range(-75, 75, 1, 0),
                power_tune_temperature: range(43, 93, 1, 93),
                fan_temperature: range(43, 93, 1, 70),
                fan_speed: range(2070, 6900, 1, 4600),
                minimum_performance_clock: range(214, 1183, 5, 850),
                throttle_notification: OdnRange::default(),
                auto_system_clock: OdnRange::default(),
            },
            core: (core.clone(), core),
            memory: (memory.clone(), memory),
            fan: OdnFan {
                mode: 0,
                fan_control_mode: 1,
                current_fan_speed_mode: 0,
                current_fan_speed: 0,
                target_fan_speed: 4600,
                target_temperature: 70,
                min_performance_clock: 850,
                min_fan_limit: 2070,
            },
            power: OdnPower { mode: 0, tdp_limit: 0, max_operating_temperature: 93 },
            failing: BTreeMap::new(),
            writes: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARD: &str = "gpu.amd-699f.bus-3";

    fn values(pairs: &[(&str, i32)]) -> BTreeMap<String, i32> {
        pairs.iter().map(|(key, value)| ((*key).to_owned(), *value)).collect()
    }

    fn control<'a>(device: &'a TunedDevice, key: &str) -> &'a TuningControl {
        device.controls.iter().find(|control| control.key == key).unwrap_or_else(|| {
            let keys: Vec<_> = device.controls.iter().map(|control| &control.key).collect();
            panic!("(controls): expected {key} | received: {keys:?}")
        })
    }

    fn only_card(radeon: &mut Radeon<FakeAdl>) -> TunedDevice {
        let mut devices = radeon.devices();
        assert_eq!(devices.len(), 1, "(devices): expected one card | received: {devices:?}");
        devices.remove(0)
    }

    #[test]
    fn the_six_outputs_of_one_card_are_one_device_with_a_stable_key() {
        let mut radeon = Radeon::new(FakeAdl::rx550());
        let card = only_card(&mut radeon);
        assert_eq!(card.key, CARD);
        assert_eq!(card.name, "Radeon RX550/550 Series");
        assert_eq!(card.interface, INTERFACE);
        assert_eq!(card.problem, None);
    }

    #[test]
    fn the_key_survives_a_missing_device_id_and_a_card_is_listed_once() {
        let odd = AdlAdapter { bus: 5, pnp: "ROOT\\DISPLAY".into(), ..AdlAdapter::default() };
        assert_eq!(device_key(&odd), "gpu.amd-unknown.bus-5");
        let lower =
            AdlAdapter { bus: 1, pnp: "pci\\ven_1002&dev_67df&rev_e7".into(), ..odd.clone() };
        assert_eq!(device_key(&lower), "gpu.amd-67df.bus-1");
        assert!(syspilot_proto::tuning::is_tuning_key(&device_key(&lower)));

        let first = AdlAdapter { index: 4, ..odd.clone() };
        let second = AdlAdapter { index: 5, ..odd.clone() };
        let elsewhere = AdlAdapter { index: 9, bus: 6, ..odd };
        assert_eq!(
            cards(vec![first.clone(), second, elsewhere.clone()]),
            [first, elsewhere],
            "(cards): expected the first output of each bus location"
        );
    }

    #[test]
    fn the_rx550_offers_what_its_flags_and_states_allow_and_nothing_else() {
        let mut radeon = Radeon::new(FakeAdl::rx550());
        let card = only_card(&mut radeon);
        let keys: Vec<&str> = card.controls.iter().map(|control| control.key.as_str()).collect();
        // No fan speed or temperature control: this card's flags lack them.
        // State 0 of the core and the two lower memory states are fixed.
        let mut expected = vec!["power-limit".to_owned(), "quiet-clock-floor".to_owned()];
        for state in 1..=7 {
            expected.push(format!("core-p{state}-clock"));
            expected.push(format!("core-p{state}-voltage"));
        }
        expected.extend(["memory-p2-clock".to_owned(), "memory-p2-voltage".to_owned()]);
        assert_eq!(keys, expected);

        let limit = control(&card, "power-limit");
        assert_eq!((limit.min, limit.max, limit.default, limit.current), (-75, 75, 0, 0));
        assert_eq!((limit.kind, limit.unit), (TuningKind::PowerLimit, TuningUnit::Percent));

        let top = control(&card, "core-p7-clock");
        assert_eq!((top.min, top.max, top.step), (214, 1800, 5), "10 kHz units became MHz");
        assert_eq!((top.default, top.current, top.state), (1183, 1183, Some(7)));
        let volts = control(&card, "core-p7-voltage");
        assert_eq!((volts.min, volts.max, volts.default), (700, 1075, 1050));
        assert_eq!(volts.unit, TuningUnit::Millivolt);

        let memory = control(&card, "memory-p2-clock");
        assert_eq!((memory.min, memory.max, memory.default), (1000, 2000, 1500));
        let floor = control(&card, "quiet-clock-floor");
        assert_eq!((floor.min, floor.max, floor.default), (214, 1183, 850), "already in MHz");
    }

    #[test]
    fn a_card_with_every_flag_offers_the_fan_and_temperature_controls_too() {
        let mut adl = FakeAdl::rx550();
        adl.capabilities.flags |= feature::FAN_SPEED_MIN
            | feature::FAN_SPEED_TARGET
            | feature::TEMPERATURE_FAN_MAX
            | feature::TEMPERATURE_SYSTEM;
        let card = only_card(&mut Radeon::new(adl));
        let fan_max = control(&card, "fan-max-speed");
        assert_eq!((fan_max.min, fan_max.max, fan_max.default), (2070, 6900, 4600));
        assert_eq!(control(&card, "fan-min-speed").default, 2070);
        assert_eq!(control(&card, "fan-target-temperature").default, 70);
        assert_eq!(control(&card, "temperature-limit").default, 93);
    }

    #[test]
    fn without_the_underclock_flag_a_memory_clock_does_not_go_below_its_default() {
        let mut adl = FakeAdl::rx550();
        adl.capabilities.flags &= !feature::MCLK_UNDERCLOCK;
        let mut radeon = Radeon::new(adl);
        let card = only_card(&mut radeon);
        let memory = control(&card, "memory-p2-clock");
        assert_eq!((memory.min, memory.max, memory.default), (1500, 2000, 1500));
        // The core clocks keep the driver's range.
        assert_eq!(control(&card, "core-p7-clock").min, 214);

        let refused = radeon.apply(CARD, &values(&[("memory-p2-clock", 1400)])).unwrap_err();
        assert_eq!(
            refused,
            "(tuning.values.memory-p2-clock): expected 1500..=2000 | received: 1400"
        );
        assert!(radeon.adl.writes.is_empty(), "{:?}", radeon.adl.writes);
        radeon.apply(CARD, &values(&[("memory-p2-clock", 1600)])).unwrap();
    }

    #[test]
    fn a_lowest_fan_speed_above_the_highest_is_refused_before_any_write() {
        let mut adl = FakeAdl::rx550();
        adl.capabilities.flags |= feature::FAN_SPEED_MIN | feature::FAN_SPEED_TARGET;
        let mut radeon = Radeon::new(adl);
        let upside_down = values(&[("fan-min-speed", 6000), ("fan-max-speed", 3000)]);
        assert_eq!(
            radeon.apply(CARD, &upside_down).unwrap_err(),
            "(tuning.values.fan-min-speed): expected at most the 3000 RPM of fan-max-speed | \
             received: 6000 RPM"
        );
        // Against the default of the other speed too: 4600 RPM.
        assert!(radeon.apply(CARD, &values(&[("fan-min-speed", 5000)])).is_err());
        assert!(radeon.adl.writes.is_empty(), "{:?}", radeon.adl.writes);
        radeon.apply(CARD, &values(&[("fan-min-speed", 3000)])).unwrap();
        assert_eq!(radeon.adl.fan.min_fan_limit, 3000);
    }

    #[test]
    fn a_range_the_driver_left_empty_offers_no_control() {
        let mut adl = FakeAdl::rx550();
        adl.capabilities.power = OdnRange::default();
        let card = only_card(&mut Radeon::new(adl));
        assert!(card.controls.iter().all(|control| control.key != "power-limit"));
        assert_eq!(usable(OdnRange { min: 21_450, max: 21_499, ..OdnRange::default() }, 100), None);
        let inwards = OdnRange { min: 21_450, max: 180_050, step: 500, ..OdnRange::default() };
        assert_eq!(usable(inwards, 100), Some((215, 1800, 5)), "bounds are rounded inwards");
    }

    #[test]
    fn cards_on_another_overdrive_generation_or_none_are_not_listed() {
        for version in [Some(8), Some(6), None] {
            let adl = FakeAdl { version, ..FakeAdl::rx550() };
            assert_eq!(Radeon::new(adl).devices(), Vec::new(), "(overdrive {version:?})");
        }
        let mut broken = FakeAdl::rx550();
        broken.failing.insert("ADL2_Adapter_AdapterInfo_Get", -1);
        assert_eq!(Radeon::new(broken).devices(), Vec::new());
    }

    #[test]
    fn a_card_that_cannot_be_read_is_listed_with_the_reason() {
        let mut adl = FakeAdl::rx550();
        adl.failing.insert("ADL2_OverdriveN_FanControl_Get", -8);
        let card = only_card(&mut Radeon::new(adl));
        assert!(card.controls.is_empty());
        assert_eq!(
            card.problem.as_deref(),
            Some("(ADL2_OverdriveN_FanControl_Get): expected ADL_OK (0) | received: -8")
        );
    }

    #[test]
    fn a_power_limit_writes_the_power_structure_only_and_reads_back() {
        let mut radeon = Radeon::new(FakeAdl::rx550());
        radeon.apply(CARD, &values(&[("power-limit", -20)])).unwrap();
        assert_eq!(radeon.adl.writes, ["ADL2_OverdriveN_PowerLimit_Set"]);
        assert_eq!(radeon.adl.power.tdp_limit, -20);
        assert_eq!(radeon.adl.power.max_operating_temperature, 93, "left as read");
        assert_eq!(control(&only_card(&mut radeon), "power-limit").current, -20);

        // The same setting again finds it in place and writes nothing.
        radeon.apply(CARD, &values(&[("power-limit", -20)])).unwrap();
        assert_eq!(radeon.adl.writes.len(), 1, "{:?}", radeon.adl.writes);
    }

    #[test]
    fn state_values_are_laid_over_the_defaults_and_written_in_manual_mode() {
        let mut radeon = Radeon::new(FakeAdl::rx550());
        radeon.apply(CARD, &values(&[("core-p7-clock", 1150), ("core-p7-voltage", 1000)])).unwrap();
        assert_eq!(radeon.adl.writes, ["ADL2_OverdriveN_SystemClocksX2_Set"]);
        let written = &radeon.adl.core.0;
        assert_eq!((written.levels[7].clock, written.levels[7].vddc), (115_000, 1000));
        assert_eq!((written.levels[6].clock, written.levels[6].vddc), (112_400, 1000));
        assert_eq!(
            written.levels.iter().map(|level| level.enabled).collect::<Vec<_>>(),
            [1; 8],
            "(enabled): expected 0 or 1 per state, as the driver wants it in a write"
        );

        // A second setting replaces the first: what it does not list is back
        // at its default, not left where the first setting put it.
        radeon.apply(CARD, &values(&[("core-p6-voltage", 950)])).unwrap();
        let written = &radeon.adl.core.0;
        assert_eq!((written.levels[7].clock, written.levels[7].vddc), (118_300, 1050));
        assert_eq!(written.levels[6].vddc, 950);
    }

    #[test]
    fn a_memory_value_and_the_quiet_floor_go_to_their_own_structures() {
        let mut radeon = Radeon::new(FakeAdl::rx550());
        radeon
            .apply(CARD, &values(&[("memory-p2-clock", 1600), ("quiet-clock-floor", 700)]))
            .unwrap();
        assert_eq!(
            radeon.adl.writes,
            ["ADL2_OverdriveN_MemoryClocksX2_Set", "ADL2_OverdriveN_FanControl_Set"]
        );
        assert_eq!(radeon.adl.memory.0.levels[2].clock, 160_000);
        let fan = radeon.adl.fan;
        assert_eq!(fan.min_performance_clock, 700);
        assert_eq!(
            (fan.target_fan_speed, fan.target_temperature, fan.min_fan_limit, fan.fan_control_mode),
            (4600, 70, 2070, 1),
            "(fan): expected the fields with no control to stay as read"
        );
    }

    #[test]
    fn a_reset_puts_back_the_defaults_and_writes_only_what_was_changed() {
        let mut radeon = Radeon::new(FakeAdl::rx550());
        radeon.reset(CARD).unwrap();
        assert_eq!(radeon.adl.writes, Vec::<&str>::new(), "nothing differs from the defaults");

        radeon.apply(CARD, &values(&[("power-limit", 10), ("core-p7-clock", 1200)])).unwrap();
        radeon.adl.writes.clear();
        radeon.reset(CARD).unwrap();
        assert_eq!(
            radeon.adl.writes,
            ["ADL2_OverdriveN_SystemClocksX2_Set", "ADL2_OverdriveN_PowerLimit_Set"]
        );
        let card = only_card(&mut radeon);
        assert!(
            card.controls.iter().all(|control| control.current == control.default),
            "(after reset): expected every control at its default | received: {card:?}"
        );
    }

    #[test]
    fn a_value_out_of_range_or_for_no_control_writes_nothing() {
        let mut radeon = Radeon::new(FakeAdl::rx550());
        let too_much = radeon.apply(CARD, &values(&[("core-p7-voltage", 1100)])).unwrap_err();
        assert_eq!(
            too_much,
            "(tuning.values.core-p7-voltage): expected 700..=1075 | received: 1100"
        );
        // State 0 is fixed on this card and the fan speed is not offered.
        for key in ["core-p0-clock", "fan-max-speed", "memory-p0-clock"] {
            let problem = radeon.apply(CARD, &values(&[(key, 800)])).unwrap_err();
            assert!(problem.contains("expected a control of the device"), "{key}: {problem}");
        }
        assert_eq!(radeon.adl.writes, Vec::<&str>::new());
    }

    #[test]
    fn a_state_slower_than_the_one_below_it_is_refused_before_any_write() {
        let mut radeon = Radeon::new(FakeAdl::rx550());
        let problem = radeon
            .apply(CARD, &values(&[("power-limit", -10), ("core-p7-clock", 1000)]))
            .unwrap_err();
        assert_eq!(
            problem,
            "(core-p7-clock): expected at least the 1124 MHz of state 6 | received: 1000 MHz"
        );
        assert_eq!(radeon.adl.writes, Vec::<&str>::new(), "not even the power limit");
    }

    #[test]
    fn a_write_the_driver_refuses_is_reported_with_its_function_and_code() {
        let mut adl = FakeAdl::rx550();
        adl.failing.insert("ADL2_OverdriveN_PowerLimit_Set", -1);
        let mut radeon = Radeon::new(adl);
        assert_eq!(
            radeon.apply(CARD, &values(&[("power-limit", -20)])).unwrap_err(),
            "(ADL2_OverdriveN_PowerLimit_Set): expected ADL_OK (0) | received: -1"
        );
        assert_eq!(
            radeon.apply("gpu.amd-0000.bus-9", &BTreeMap::new()).unwrap_err(),
            "(tuning device): expected a Radeon card on Overdrive N | received: \"gpu.amd-0000.bus-9\""
        );
    }

    #[test]
    fn states_are_compared_by_clock_and_voltage_and_must_rise() {
        let levels = |clocks: &[i32]| OdnLevels {
            mode: mode::CURRENT,
            levels: clocks.iter().map(|&clock| OdnLevel { clock, ..OdnLevel::default() }).collect(),
        };
        let mut other_mode = levels(&[100, 200]);
        other_mode.mode = mode::MANUAL;
        other_mode.levels[0].enabled = 1;
        assert!(same_states(&levels(&[100, 200]), &other_mode), "mode and enabled do not count");
        assert!(!same_states(&levels(&[100, 200]), &levels(&[100, 300])));
        assert!(!same_states(&levels(&[100]), &levels(&[100, 0])));

        // Equal clocks are fine, and the zeroed states of a short list do
        // not count as a fall.
        assert_eq!(check_rising(&levels(&[100, 100, 300, 0, 0]), "memory"), Ok(()));
        assert!(check_rising(&levels(&[300, 0, 200]), "memory").is_err());
    }

    #[test]
    fn errors_name_the_function_and_what_it_returned() {
        assert_eq!(
            AdlError::NotInstalled.to_string(),
            "(atiadlxx.dll): expected the AMD driver's library | received: none"
        );
        assert_eq!(
            AdlError::MissingExport("ADL2_Main_Control_Create").to_string(),
            "(atiadlxx.dll): expected the function ADL2_Main_Control_Create | received: none"
        );
    }
}
