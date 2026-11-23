//! Sensor providers and the hub that samples them.

use std::collections::HashSet;

use syspilot_proto::sensor::{DeviceClass, DeviceInfo, Sample, SensorInfo, SensorKind, SensorList};

/// Description of one sensor, as declared by its provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SensorSpec {
    /// Name of the hardware the sensor belongs to, e.g. the CPU brand
    /// string. Sensors with the same device name and class are grouped.
    pub device: String,
    /// Kind of hardware.
    pub class: DeviceClass,
    /// Stable, unique key, e.g. `cpu0.core3.temp`. Lowercase, dot-separated.
    pub key: String,
    /// Display label within the device, e.g. `Core 3`.
    pub label: String,
    /// Quantity and unit.
    pub kind: SensorKind,
}

/// One source of sensor values.
///
/// A provider is constructed by probing; a probe that finds nothing returns
/// no provider rather than a provider without sensors.
pub trait Provider: Send {
    /// The sensors this provider reads. Fixed for the provider's lifetime.
    fn sensors(&self) -> Vec<SensorSpec>;

    /// Reads every sensor and appends exactly one value per sensor to
    /// `out`, in the order of [`Provider::sensors`]. `None` means the read
    /// failed or the value is implausible.
    fn sample(&mut self, out: &mut Vec<Option<f64>>);

    /// Seconds between reads. Slow buses return more than 1; the hub then
    /// repeats the last values on the ticks in between.
    fn period_secs(&self) -> u32 {
        1
    }

    /// Whether the readings need the PawnIO driver.
    fn needs_driver(&self) -> bool {
        false
    }
}

struct Slot {
    provider: Box<dyn Provider>,
    /// Index of the provider's first sensor in the hub's value array.
    first: usize,
    count: usize,
}

/// A set of providers sampled together.
pub struct Hub {
    slots: Vec<Slot>,
    list: SensorList,
    values: Vec<Option<f64>>,
    scratch: Vec<Option<f64>>,
    tick: u64,
}

impl Hub {
    /// Builds a hub. `generation` identifies this sensor set to clients.
    ///
    /// A sensor whose key was already declared by an earlier provider is a
    /// provider bug; the later one is renamed with a numeric suffix so that
    /// keys stay unique for profiles and logs.
    pub fn new(providers: Vec<Box<dyn Provider>>, generation: u32) -> Self {
        let mut list = SensorList { generation, ..SensorList::default() };
        let mut slots = Vec::with_capacity(providers.len());
        let mut keys = HashSet::new();
        for provider in providers {
            let first = list.sensors.len();
            let driver = provider.needs_driver();
            for spec in provider.sensors() {
                let device = device_index(&mut list.devices, &spec.device, spec.class);
                let key = unique_key(&mut keys, spec.key);
                let id = list.sensors.len() as u32;
                list.sensors.push(SensorInfo {
                    id,
                    device,
                    key,
                    label: spec.label,
                    kind: spec.kind,
                    driver,
                });
            }
            slots.push(Slot { provider, first, count: list.sensors.len() - first });
        }
        let values = vec![None; list.sensors.len()];
        Self { slots, list, values, scratch: Vec::new(), tick: 0 }
    }

    /// Metadata for every sensor.
    pub fn list(&self) -> &SensorList {
        &self.list
    }

    /// Whether the hub has no sensors at all.
    pub fn is_empty(&self) -> bool {
        self.list.sensors.is_empty()
    }

    /// Reads the providers that are due on this tick (one tick per second)
    /// and returns the current value of every sensor, indexed by sensor ID.
    pub fn sample(&mut self) -> &[Option<f64>] {
        for slot in &mut self.slots {
            let period = u64::from(slot.provider.period_secs().max(1));
            if !self.tick.is_multiple_of(period) {
                continue;
            }
            self.scratch.clear();
            slot.provider.sample(&mut self.scratch);
            // A provider that returns the wrong number of values would shift
            // every later reading onto the wrong sensor; pad or cut instead.
            self.scratch.resize(slot.count, None);
            self.values[slot.first..slot.first + slot.count].copy_from_slice(&self.scratch);
        }
        self.tick += 1;
        &self.values
    }

    /// The values of the last [`Hub::sample`], indexed by sensor ID.
    pub fn values(&self) -> &[Option<f64>] {
        &self.values
    }

    /// The last values as section samples.
    pub fn samples(&self) -> impl Iterator<Item = Sample> + '_ {
        self.values.iter().enumerate().map(|(id, value)| Sample { id: id as u32, value: *value })
    }

    /// The last value of the sensor with `key`.
    pub fn value_of(&self, key: &str) -> Option<f64> {
        let sensor = self.list.sensors.iter().find(|sensor| sensor.key == key)?;
        self.values[sensor.id as usize]
    }
}

fn device_index(devices: &mut Vec<DeviceInfo>, name: &str, class: DeviceClass) -> u32 {
    let position = devices.iter().position(|d| d.class == class && d.name == name);
    let index = position.unwrap_or_else(|| {
        devices.push(DeviceInfo { name: name.to_owned(), class });
        devices.len() - 1
    });
    index as u32
}

fn unique_key(keys: &mut HashSet<String>, key: String) -> String {
    if keys.insert(key.clone()) {
        return key;
    }
    let mut suffix = 2;
    loop {
        let candidate = format!("{key}.{suffix}");
        if keys.insert(candidate.clone()) {
            return candidate;
        }
        suffix += 1;
    }
}

/// A provider with scripted values, for tests in this and dependent crates.
#[derive(Debug, Clone)]
pub struct FakeProvider {
    /// Sensors to declare.
    pub specs: Vec<SensorSpec>,
    /// Values returned by every sample.
    pub values: Vec<Option<f64>>,
    /// Sampling period.
    pub period_secs: u32,
    /// Number of times [`Provider::sample`] ran.
    pub samples_taken: std::sync::Arc<std::sync::atomic::AtomicU32>,
}

impl FakeProvider {
    /// A provider with one temperature sensor per `(key, value)` pair on a
    /// device called `Fake`.
    pub fn temperatures(readings: &[(&str, Option<f64>)]) -> Self {
        Self {
            specs: readings
                .iter()
                .map(|(key, _)| SensorSpec {
                    device: "Fake".into(),
                    class: DeviceClass::Cpu,
                    key: (*key).to_owned(),
                    label: (*key).to_owned(),
                    kind: SensorKind::Temperature,
                })
                .collect(),
            values: readings.iter().map(|(_, value)| *value).collect(),
            period_secs: 1,
            samples_taken: Default::default(),
        }
    }
}

impl Provider for FakeProvider {
    fn sensors(&self) -> Vec<SensorSpec> {
        self.specs.clone()
    }

    fn sample(&mut self, out: &mut Vec<Option<f64>>) {
        self.samples_taken.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        out.extend_from_slice(&self.values);
    }

    fn period_secs(&self) -> u32 {
        self.period_secs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn hub_assigns_sequential_ids_and_groups_devices() {
        let first = FakeProvider::temperatures(&[("a.temp", Some(40.0)), ("b.temp", Some(50.0))]);
        let second = FakeProvider::temperatures(&[("c.temp", None)]);
        let mut hub = Hub::new(vec![Box::new(first), Box::new(second)], 7);

        let list = hub.list();
        assert_eq!(list.generation, 7);
        assert_eq!(list.devices.len(), 1, "same device name and class is one device");
        assert_eq!(list.sensors.iter().map(|s| s.id).collect::<Vec<_>>(), [0, 1, 2]);

        assert_eq!(hub.sample(), [Some(40.0), Some(50.0), None]);
        assert_eq!(hub.value_of("b.temp"), Some(50.0));
        assert_eq!(hub.value_of("c.temp"), None);
        assert_eq!(hub.value_of("missing"), None);
    }

    #[test]
    fn duplicate_keys_are_made_unique() {
        let first = FakeProvider::temperatures(&[("x.temp", Some(1.0))]);
        let second = FakeProvider::temperatures(&[("x.temp", Some(2.0)), ("x.temp", Some(3.0))]);
        let hub = Hub::new(vec![Box::new(first), Box::new(second)], 1);
        let keys: Vec<_> = hub.list().sensors.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys, ["x.temp", "x.temp.2", "x.temp.3"]);
    }

    #[test]
    fn slow_provider_is_read_only_when_due() {
        let mut slow = FakeProvider::temperatures(&[("dimm.temp", Some(33.0))]);
        slow.period_secs = 5;
        let samples_taken = slow.samples_taken.clone();
        let mut hub = Hub::new(vec![Box::new(slow)], 1);

        for _ in 0..10 {
            assert_eq!(hub.sample(), [Some(33.0)], "the last value is repeated between reads");
        }
        assert_eq!(samples_taken.load(Ordering::Relaxed), 2, "ticks 0 and 5");
    }

    #[test]
    fn provider_returning_too_few_values_cannot_shift_later_sensors() {
        let mut short = FakeProvider::temperatures(&[("a.temp", Some(1.0)), ("b.temp", Some(2.0))]);
        short.values.pop();
        let after = FakeProvider::temperatures(&[("c.temp", Some(3.0))]);
        let mut hub = Hub::new(vec![Box::new(short), Box::new(after)], 1);
        assert_eq!(hub.sample(), [Some(1.0), None, Some(3.0)]);
    }

    #[test]
    fn samples_carry_sensor_ids() {
        let provider = FakeProvider::temperatures(&[("a.temp", Some(1.5)), ("b.temp", None)]);
        let mut hub = Hub::new(vec![Box::new(provider)], 1);
        hub.sample();
        let samples: Vec<_> = hub.samples().collect();
        assert_eq!(samples, [Sample { id: 0, value: Some(1.5) }, Sample { id: 1, value: None }]);
    }
}
