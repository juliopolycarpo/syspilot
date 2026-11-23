//! Smoke tests of the driverless providers against the machine running the
//! tests. They check contracts (unique keys, one value per sensor, plausible
//! ranges), not exact readings, so they pass on any Windows 11 machine.
//!
//! Providers are called directly instead of through `Hub`: the hub pads
//! missing values and renames duplicate keys, which would hide exactly the
//! bugs these tests look for.

use std::collections::HashSet;
use std::time::Duration;

use syspilot_hw::os;
use syspilot_hw::{Provider, SensorSpec};
use syspilot_proto::sensor::{DeviceClass, SensorKind};

type Probe = fn() -> Vec<Box<dyn Provider>>;

/// Every module's probe, by name, so a failure says which provider broke.
const PROBES: &[(&str, Probe)] =
    &[("cpu", os::cpu::probe), ("memory", os::memory::probe), ("gpu", os::gpu::probe)];

/// Samples taken per provider, and the wait between them. Rates need two
/// reads at least `MIN_INTERVAL` (50 ms) apart, and PDH counters need two
/// collections, so the wait cannot be skipped; 3 x 500 ms keeps the whole
/// file near one second because all providers share the same waits.
const SAMPLES: usize = 3;
const SAMPLE_GAP: Duration = Duration::from_millis(500);

fn is_lowercase_dotted_key(key: &str) -> bool {
    let mut segments = key.split('.');
    let first = segments.next().unwrap_or_default();
    let rest: Vec<_> = segments.collect();
    let valid_segment = |segment: &str| {
        !segment.is_empty()
            && segment.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    };
    valid_segment(first) && !rest.is_empty() && rest.iter().all(|segment| valid_segment(segment))
}

/// The range a reading of this kind may take on a working machine.
fn plausible_range(kind: SensorKind) -> std::ops::RangeInclusive<f64> {
    match kind {
        SensorKind::Load | SensorKind::Duty | SensorKind::Level => 0.0..=100.0,
        SensorKind::Temperature => -20.0..=125.0,
        SensorKind::Frequency => 1.0..=10_000.0,
        SensorKind::Fan => 0.0..=30_000.0,
        SensorKind::Voltage => 0.0..=100.0,
        SensorKind::Power => 0.0..=5_000.0,
        SensorKind::Data => 0.0..=(1u64 << 50) as f64,
        SensorKind::Throughput => 0.0..=(1u64 << 40) as f64,
    }
}

struct Probed {
    name: &'static str,
    provider: Box<dyn Provider>,
    specs: Vec<SensorSpec>,
}

fn probe_everything() -> Vec<Probed> {
    let mut probed = Vec::new();
    for &(name, probe) in PROBES {
        for provider in probe() {
            let specs = provider.sensors();
            probed.push(Probed { name, provider, specs });
        }
    }
    probed
}

#[test]
fn probing_twice_gives_the_same_sensor_set() {
    // Dropping a provider must release its handles, and a second probe must
    // see the same hardware.
    let keys = |providers: &[Probed]| -> Vec<String> {
        providers.iter().flat_map(|p| p.specs.iter().map(|s| s.key.clone())).collect()
    };
    let first = probe_everything();
    let first_keys = keys(&first);
    drop(first);
    assert_eq!(keys(&probe_everything()), first_keys);
}

#[test]
fn os_probe_matches_the_per_module_probes() {
    let all: usize = os::probe().iter().map(|provider| provider.sensors().len()).sum();
    let per_module: usize = probe_everything().iter().map(|p| p.specs.len()).sum();
    assert_eq!(all, per_module, "os::probe sensors vs sum of module probes");
}

#[test]
fn memory_and_cpu_are_always_present() {
    let probed = probe_everything();
    for expected in [DeviceClass::Cpu, DeviceClass::Memory] {
        assert!(
            probed.iter().any(|p| p.specs.iter().any(|s| s.class == expected)),
            "device class {expected:?}: expected a provider on every Windows machine | received none"
        );
    }
}

#[test]
fn providers_declare_valid_keys_and_report_plausible_values_over_three_samples() {
    let mut probed = probe_everything();

    let mut seen_keys = HashSet::new();
    for p in &probed {
        assert!(
            !p.specs.is_empty(),
            "{}: a probe that finds nothing must return no provider",
            p.name
        );
        for spec in &p.specs {
            assert!(
                is_lowercase_dotted_key(&spec.key),
                "{}: expected a lowercase dot-separated key | received: {:?}",
                p.name,
                spec.key
            );
            assert!(seen_keys.insert(spec.key.clone()), "{}: duplicate key {:?}", p.name, spec.key);
            assert!(
                !spec.device.is_empty(),
                "{}: sensor {:?} has an empty device name",
                p.name,
                spec.key
            );
            assert!(!spec.label.is_empty(), "{}: sensor {:?} has an empty label", p.name, spec.key);
        }
    }

    // samples[provider][round] = values
    let mut samples: Vec<Vec<Vec<Option<f64>>>> = vec![Vec::new(); probed.len()];
    for round in 0..SAMPLES {
        if round > 0 {
            std::thread::sleep(SAMPLE_GAP);
        }
        for (index, p) in probed.iter_mut().enumerate() {
            let mut out = Vec::new();
            p.provider.sample(&mut out);
            samples[index].push(out);
        }
    }

    for (p, rounds) in probed.iter().zip(&samples) {
        for (round, values) in rounds.iter().enumerate() {
            assert_eq!(
                values.len(),
                p.specs.len(),
                "{} sample {round}: expected one value per sensor | received {} values for {} sensors",
                p.name,
                values.len(),
                p.specs.len()
            );
            for (spec, value) in p.specs.iter().zip(values) {
                let Some(value) = value else { continue };
                let range = plausible_range(spec.kind);
                assert!(
                    value.is_finite() && range.contains(value),
                    "{} sample {round}, {}: expected a value in {range:?} | received: {value}",
                    p.name,
                    spec.key
                );
            }
        }
        // Rates have no value on the first sample and must have one on the
        // second and third.
        for (index, spec) in p.specs.iter().enumerate() {
            if is_always_available_rate(&spec.key) {
                assert_eq!(
                    rounds[0][index], None,
                    "{}: first sample of a rate must be None",
                    spec.key
                );
                for (round, values) in rounds.iter().enumerate().skip(1) {
                    assert!(
                        values[index].is_some(),
                        "{} sample {round}: expected a value | received: None",
                        spec.key
                    );
                }
            }
        }
    }
}

/// Rate sensors that every machine can compute from the second sample on.
fn is_always_available_rate(key: &str) -> bool {
    key == "cpu.load"
}
