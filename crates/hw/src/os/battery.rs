//! Charge level and charge or discharge power of the system battery.
//!
//! The kernel's aggregated battery state (`SystemBatteryState`) covers every
//! battery of the machine in one call, with no device handles: a desktop
//! reports no battery and gets no provider.

use syspilot_proto::sensor::{DeviceClass, SensorKind};
use windows_sys::Win32::System::Power::{
    BATTERY_UNKNOWN_CAPACITY, BATTERY_UNKNOWN_RATE, CallNtPowerInformation, SYSTEM_BATTERY_STATE,
    SystemBatteryState,
};

use crate::hub::{Provider, SensorSpec};

/// What the kernel reports about the batteries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BatteryState {
    /// A battery is installed.
    pub present: bool,
    /// Full-charge capacity, in mWh.
    pub max_capacity: u32,
    /// Remaining capacity, in mWh.
    pub remaining_capacity: u32,
    /// Raw rate in mW: negative while discharging, positive while charging,
    /// `BATTERY_UNKNOWN_RATE` when the battery does not say.
    pub raw_rate: u32,
}

/// Where the battery state comes from.
pub(crate) trait BatterySource: Send {
    /// The current state, `None` if the query failed.
    fn read(&mut self) -> Option<BatteryState>;
}

/// Charge in percent. `None` when the capacities are unknown or the full
/// capacity is zero; a remaining capacity above the full capacity (seen
/// right after a calibration) is shown as 100 %.
fn charge_percent(state: &BatteryState) -> Option<f64> {
    let known = |capacity: u32| capacity != BATTERY_UNKNOWN_CAPACITY;
    if !state.present
        || state.max_capacity == 0
        || !known(state.max_capacity)
        || !known(state.remaining_capacity)
    {
        return None;
    }
    let percent = f64::from(state.remaining_capacity) / f64::from(state.max_capacity) * 100.0;
    Some(percent.clamp(0.0, 100.0))
}

/// Power flowing into or out of the battery, in watts, without its sign:
/// the sensor kind has no direction. `None` when the battery does not
/// report a rate.
fn flow_watts(raw_rate: u32) -> Option<f64> {
    if raw_rate == BATTERY_UNKNOWN_RATE {
        return None;
    }
    // The field is a ULONG that holds a signed milliwatt value.
    Some(f64::from((raw_rate as i32).unsigned_abs()) / 1000.0)
}

/// [`BatterySource`] backed by `CallNtPowerInformation`.
struct KernelBattery;

impl BatterySource for KernelBattery {
    fn read(&mut self) -> Option<BatteryState> {
        let mut state = SYSTEM_BATTERY_STATE::default();
        // SAFETY: there is no input buffer (null, length 0); the output
        // buffer is a `SYSTEM_BATTERY_STATE` and its exact size is passed.
        let status = unsafe {
            CallNtPowerInformation(
                SystemBatteryState,
                std::ptr::null(),
                0,
                std::ptr::from_mut(&mut state).cast(),
                size_of::<SYSTEM_BATTERY_STATE>() as u32,
            )
        };
        // NTSTATUS: zero and positive values are success.
        (status >= 0).then_some(BatteryState {
            present: state.BatteryPresent,
            max_capacity: state.MaxCapacity,
            remaining_capacity: state.RemainingCapacity,
            raw_rate: state.Rate,
        })
    }
}

/// The battery's sensors.
pub struct BatteryProvider {
    source: Box<dyn BatterySource>,
    /// The battery reported a rate when probed.
    has_rate: bool,
}

impl BatteryProvider {
    /// Builds the provider if `source` reports an installed battery with a
    /// known capacity.
    fn from_source(mut source: Box<dyn BatterySource>) -> Option<Self> {
        let state = source.read().filter(|state| charge_percent(state).is_some())?;
        Some(Self { has_rate: flow_watts(state.raw_rate).is_some(), source })
    }
}

impl Provider for BatteryProvider {
    fn sensors(&self) -> Vec<SensorSpec> {
        let mut specs = vec![SensorSpec {
            device: "Battery".to_owned(),
            class: DeviceClass::Battery,
            key: "battery.level".to_owned(),
            label: "Charge".to_owned(),
            kind: SensorKind::Level,
        }];
        if self.has_rate {
            specs.push(SensorSpec {
                device: "Battery".to_owned(),
                class: DeviceClass::Battery,
                key: "battery.power".to_owned(),
                label: "Charge or discharge power".to_owned(),
                kind: SensorKind::Power,
            });
        }
        specs
    }

    fn sample(&mut self, out: &mut Vec<Option<f64>>) {
        let state = self.source.read();
        out.push(state.as_ref().and_then(charge_percent));
        if self.has_rate {
            out.push(
                state.filter(|state| state.present).and_then(|state| flow_watts(state.raw_rate)),
            );
        }
    }
}

/// The battery provider, if the machine has a battery.
pub fn probe() -> Vec<Box<dyn Provider>> {
    match BatteryProvider::from_source(Box::new(KernelBattery)) {
        Some(provider) => vec![Box::new(provider)],
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// Replays scripted states, then repeats the last one. `None` entries
    /// simulate a failed query.
    struct ScriptedBattery(VecDeque<Option<BatteryState>>);

    impl BatterySource for ScriptedBattery {
        fn read(&mut self) -> Option<BatteryState> {
            if self.0.len() > 1 {
                self.0.pop_front().flatten()
            } else {
                self.0.front().copied().flatten()
            }
        }
    }

    fn scripted(states: &[Option<BatteryState>]) -> Box<ScriptedBattery> {
        Box::new(ScriptedBattery(states.iter().copied().collect()))
    }

    fn state(max: u32, remaining: u32, rate_milliwatts: i32) -> BatteryState {
        BatteryState {
            present: true,
            max_capacity: max,
            remaining_capacity: remaining,
            raw_rate: rate_milliwatts as u32,
        }
    }

    fn unknown_rate(max: u32, remaining: u32) -> BatteryState {
        BatteryState { raw_rate: BATTERY_UNKNOWN_RATE, ..state(max, remaining, 0) }
    }

    // ---- pure conversions ------------------------------------------------

    #[test]
    fn charge_is_the_remaining_share_of_the_full_capacity() {
        assert_eq!(charge_percent(&state(50_000, 12_500, 0)), Some(25.0));
        assert_eq!(charge_percent(&state(50_000, 0, 0)), Some(0.0));
        assert_eq!(charge_percent(&state(50_000, 50_000, 0)), Some(100.0));
    }

    #[test]
    fn charge_above_the_full_capacity_is_capped_at_100() {
        assert_eq!(charge_percent(&state(50_000, 51_000, 0)), Some(100.0));
    }

    #[test]
    fn charge_is_unknown_without_a_battery_or_a_usable_capacity() {
        let absent = BatteryState { present: false, ..state(50_000, 25_000, 0) };
        assert_eq!(charge_percent(&absent), None, "no battery installed");
        assert_eq!(charge_percent(&state(0, 0, 0)), None, "zero full capacity");
        assert_eq!(charge_percent(&state(BATTERY_UNKNOWN_CAPACITY, 5, 0)), None);
        assert_eq!(charge_percent(&state(50_000, BATTERY_UNKNOWN_CAPACITY, 0)), None);
    }

    #[test]
    fn flow_is_the_magnitude_of_the_signed_milliwatt_rate_in_watts() {
        assert_eq!(flow_watts((-15_500i32) as u32), Some(15.5), "discharging");
        assert_eq!(flow_watts(45_000), Some(45.0), "charging");
        assert_eq!(flow_watts(0), Some(0.0), "idle");
    }

    #[test]
    fn unknown_rate_has_no_flow() {
        assert_eq!(flow_watts(BATTERY_UNKNOWN_RATE), None);
    }

    // ---- provider ----------------------------------------------------------------

    #[test]
    fn battery_with_a_rate_declares_level_and_power() {
        let provider =
            BatteryProvider::from_source(scripted(&[Some(state(50_000, 25_000, -9_000))])).unwrap();
        let specs = provider.sensors();
        let summary: Vec<_> = specs.iter().map(|s| (s.key.as_str(), s.kind)).collect();
        assert_eq!(
            summary,
            [("battery.level", SensorKind::Level), ("battery.power", SensorKind::Power)]
        );
        assert!(specs.iter().all(|s| s.class == DeviceClass::Battery && s.device == "Battery"));
    }

    #[test]
    fn battery_without_a_rate_declares_only_the_level() {
        let provider =
            BatteryProvider::from_source(scripted(&[Some(unknown_rate(50_000, 25_000))])).unwrap();
        let keys: Vec<_> = provider.sensors().into_iter().map(|s| s.key).collect();
        assert_eq!(keys, ["battery.level"]);
    }

    #[test]
    fn machine_without_a_battery_has_no_provider() {
        let desktop = BatteryState { present: false, ..state(0, 0, 0) };
        assert!(BatteryProvider::from_source(scripted(&[Some(desktop)])).is_none());
        assert!(BatteryProvider::from_source(scripted(&[None])).is_none(), "query failed");
    }

    #[test]
    fn samples_follow_the_battery_while_it_discharges() {
        let mut provider = BatteryProvider::from_source(scripted(&[
            Some(state(50_000, 25_000, -9_000)),
            Some(state(50_000, 25_000, -9_000)),
            Some(state(50_000, 24_000, -12_000)),
        ]))
        .unwrap();
        let mut out = Vec::new();
        provider.sample(&mut out);
        assert_eq!(out, [Some(50.0), Some(9.0)]);
        out.clear();
        provider.sample(&mut out);
        assert_eq!(out, [Some(48.0), Some(12.0)]);
    }

    #[test]
    fn failed_query_gives_one_none_per_sensor() {
        let mut provider =
            BatteryProvider::from_source(scripted(&[Some(state(50_000, 25_000, 1_000)), None]))
                .unwrap();
        let mut out = Vec::new();
        provider.sample(&mut out);
        assert_eq!(out, [None, None]);
    }

    #[test]
    fn battery_removed_after_probing_gives_none() {
        let removed = BatteryState { present: false, ..state(50_000, 25_000, 1_000) };
        let mut provider = BatteryProvider::from_source(scripted(&[
            Some(state(50_000, 25_000, 1_000)),
            Some(removed),
        ]))
        .unwrap();
        let mut out = Vec::new();
        provider.sample(&mut out);
        assert_eq!(out, [None, None]);
    }

    #[test]
    fn real_machine_probe_has_a_level_exactly_when_the_kernel_reports_a_battery() {
        // The kernel call is the same one the provider makes, so the two
        // must agree on whether a battery exists.
        let reported = KernelBattery.read().and_then(|state| charge_percent(&state));
        assert_eq!(probe().len(), usize::from(reported.is_some()));
    }
}
