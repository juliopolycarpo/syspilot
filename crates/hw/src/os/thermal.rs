//! ACPI thermal zone temperatures.
//!
//! Firmware exposes thermal zones to Windows as the `Thermal Zone
//! Information` performance counter set, one instance per zone. What a zone
//! measures (CPU package, chassis, VRM, ...) is up to the board vendor and
//! the name is only an ACPI path, so the zones are shown as they are under a
//! single device. Many desktop boards expose no zone at all, and then there
//! is no provider.

use syspilot_proto::sensor::{DeviceClass, SensorKind};

use super::pdh::{CounterId, Query};
use crate::hub::{Provider, SensorSpec};

/// English counter path; the value is in Kelvin.
const TEMPERATURE_COUNTER: &str = r"\Thermal Zone Information(*)\Temperature";
/// Offset between Kelvin and Celsius.
const KELVIN_AT_ZERO_CELSIUS: f64 = 273.15;
/// Celsius range accepted from a zone. Firmware that has no reading reports
/// zero Kelvin, and a few boards report garbage.
const PLAUSIBLE_CELSIUS: std::ops::RangeInclusive<f64> = -50.0..=150.0;

/// Where zone temperatures come from.
pub(crate) trait ZoneSource: Send {
    /// Takes one sample. `false` if it failed.
    fn collect(&mut self) -> bool;
    /// Visits each zone that has a value as `(instance name, Kelvin)`.
    fn for_each_zone(&mut self, visit: &mut dyn FnMut(&str, f64));
}

/// Celsius from a zone's Kelvin reading, `None` for a reading that cannot be
/// a temperature.
fn celsius_from_kelvin(kelvin: f64) -> Option<f64> {
    let celsius = kelvin - KELVIN_AT_ZERO_CELSIUS;
    (kelvin > 0.0 && PLAUSIBLE_CELSIUS.contains(&celsius)).then_some(celsius)
}

/// A display label for an ACPI zone instance such as `\_TZ.TZ00`: the path
/// without its leading backslash.
fn zone_label(instance: &str) -> String {
    instance.trim_start_matches('\\').to_owned()
}

/// [`ZoneSource`] backed by PDH.
struct PdhZones {
    query: Query,
    counter: CounterId,
}

impl PdhZones {
    fn open() -> Option<Self> {
        let mut query = Query::open()?;
        let counter = query.add_counter(TEMPERATURE_COUNTER)?;
        Some(Self { query, counter })
    }
}

impl ZoneSource for PdhZones {
    fn collect(&mut self) -> bool {
        self.query.collect()
    }

    fn for_each_zone(&mut self, visit: &mut dyn FnMut(&str, f64)) {
        self.query.for_each_instance(self.counter, visit);
    }
}

/// The ACPI thermal zones that existed when the provider was built.
pub struct ThermalZoneProvider {
    source: Box<dyn ZoneSource>,
    /// Instance names, sorted; the position is the zone number in the key.
    zones: Vec<String>,
}

impl ThermalZoneProvider {
    /// Builds the provider if `source` lists at least one zone.
    fn from_source(mut source: Box<dyn ZoneSource>) -> Option<Self> {
        if !source.collect() {
            return None;
        }
        let mut zones = Vec::new();
        source.for_each_zone(&mut |instance, _| zones.push(instance.to_owned()));
        zones.sort();
        zones.dedup();
        (!zones.is_empty()).then_some(Self { source, zones })
    }
}

impl Provider for ThermalZoneProvider {
    fn sensors(&self) -> Vec<SensorSpec> {
        self.zones
            .iter()
            .enumerate()
            .map(|(number, instance)| SensorSpec {
                device: "ACPI thermal zones".to_owned(),
                class: DeviceClass::Board,
                key: format!("thermal.zone{number}.temp"),
                label: zone_label(instance),
                kind: SensorKind::Temperature,
            })
            .collect()
    }

    fn sample(&mut self, out: &mut Vec<Option<f64>>) {
        let start = out.len();
        out.resize(start + self.zones.len(), None);
        if !self.source.collect() {
            return;
        }
        let zones = &self.zones;
        self.source.for_each_zone(&mut |instance, kelvin| {
            if let Ok(position) = zones.binary_search_by(|zone| zone.as_str().cmp(instance)) {
                out[start + position] = celsius_from_kelvin(kelvin);
            }
        });
    }
}

/// The thermal zone provider, if the firmware exposes any zone.
pub fn probe() -> Vec<Box<dyn Provider>> {
    let Some(source) = PdhZones::open() else { return Vec::new() };
    match ThermalZoneProvider::from_source(Box::new(source)) {
        Some(provider) => vec![Box::new(provider)],
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// Replays scripted collections, each a list of `(zone, Kelvin)`; the
    /// last one repeats. `None` is a failed collection.
    struct ScriptedZones {
        frames: VecDeque<Option<Vec<(String, f64)>>>,
        current: Vec<(String, f64)>,
    }

    impl ZoneSource for ScriptedZones {
        fn collect(&mut self) -> bool {
            let frame = if self.frames.len() > 1 {
                self.frames.pop_front().flatten()
            } else {
                self.frames.front().cloned().flatten()
            };
            let ok = frame.is_some();
            self.current = frame.unwrap_or_default();
            ok
        }

        fn for_each_zone(&mut self, visit: &mut dyn FnMut(&str, f64)) {
            for (name, kelvin) in &self.current {
                visit(name, *kelvin);
            }
        }
    }

    fn zones(readings: &[(&str, f64)]) -> Option<Vec<(String, f64)>> {
        Some(readings.iter().map(|&(name, kelvin)| (name.to_owned(), kelvin)).collect())
    }

    fn scripted(frames: Vec<Option<Vec<(String, f64)>>>) -> Box<ScriptedZones> {
        Box::new(ScriptedZones { frames: frames.into(), current: Vec::new() })
    }

    // ---- pure conversions ------------------------------------------------

    #[test]
    fn kelvin_converts_to_celsius() {
        let celsius = celsius_from_kelvin(318.15).unwrap();
        assert!((celsius - 45.0).abs() < 1e-9, "expected 45 | received {celsius}");
    }

    #[test]
    fn readings_that_cannot_be_temperatures_are_dropped() {
        for kelvin in [0.0, -3.0, 5.0, 900.0, f64::NAN, f64::INFINITY] {
            assert_eq!(celsius_from_kelvin(kelvin), None, "{kelvin} K");
        }
    }

    #[test]
    fn zone_label_drops_the_leading_backslash_of_an_acpi_path() {
        assert_eq!(zone_label(r"\_TZ.TZ00"), "_TZ.TZ00");
        assert_eq!(zone_label("THM0"), "THM0");
    }

    // ---- provider ----------------------------------------------------------------

    #[test]
    fn zones_become_numbered_sensors_of_one_board_device() {
        let source = scripted(vec![zones(&[(r"\_TZ.TZ01", 310.0), (r"\_TZ.TZ00", 305.0)])]);
        let provider = ThermalZoneProvider::from_source(source).unwrap();
        let specs = provider.sensors();
        let summary: Vec<_> = specs.iter().map(|s| (s.key.as_str(), s.label.as_str())).collect();
        assert_eq!(
            summary,
            [("thermal.zone0.temp", "_TZ.TZ00"), ("thermal.zone1.temp", "_TZ.TZ01")]
        );
        assert!(specs.iter().all(|s| s.device == "ACPI thermal zones"));
        assert!(specs.iter().all(|s| s.class == DeviceClass::Board));
        assert!(specs.iter().all(|s| s.kind == SensorKind::Temperature));
    }

    #[test]
    fn machine_without_zones_has_no_provider() {
        assert!(ThermalZoneProvider::from_source(scripted(vec![zones(&[])])).is_none());
        assert!(ThermalZoneProvider::from_source(scripted(vec![None])).is_none(), "failed read");
    }

    #[test]
    fn samples_are_celsius_in_declaration_order_whatever_the_counter_order() {
        let source = scripted(vec![
            zones(&[("A", 300.0), ("B", 301.0)]),
            zones(&[("B", 331.15), ("A", 321.15)]),
        ]);
        let mut provider = ThermalZoneProvider::from_source(source).unwrap();
        let mut out = Vec::new();
        provider.sample(&mut out);
        let values: Vec<f64> = out.iter().map(|value| value.unwrap()).collect();
        assert!((values[0] - 48.0).abs() < 1e-9 && (values[1] - 58.0).abs() < 1e-9, "{values:?}");
    }

    #[test]
    fn zone_missing_from_a_sample_or_unreadable_gives_none_in_its_slot() {
        let source = scripted(vec![
            zones(&[("A", 300.0), ("B", 301.0), ("C", 302.0)]),
            zones(&[("A", 320.0), ("C", 0.0)]), // B vanished, C reads zero Kelvin
        ]);
        let mut provider = ThermalZoneProvider::from_source(source).unwrap();
        let mut out = Vec::new();
        provider.sample(&mut out);
        assert_eq!(out.len(), 3);
        assert!(out[0].is_some());
        assert_eq!((out[1], out[2]), (None, None));
    }

    #[test]
    fn zone_that_appears_after_probing_is_ignored() {
        let source = scripted(vec![zones(&[("A", 300.0)]), zones(&[("A", 300.0), ("Z", 305.0)])]);
        let mut provider = ThermalZoneProvider::from_source(source).unwrap();
        let mut out = Vec::new();
        provider.sample(&mut out);
        assert_eq!(out.len(), 1, "one value per declared sensor");
    }

    #[test]
    fn failed_collection_gives_one_none_per_zone_and_appends_to_existing_output() {
        let source = scripted(vec![zones(&[("A", 300.0), ("B", 300.0)]), None]);
        let mut provider = ThermalZoneProvider::from_source(source).unwrap();
        let mut out = vec![Some(1.0)];
        provider.sample(&mut out);
        assert_eq!(out, [Some(1.0), None, None]);
    }
}
