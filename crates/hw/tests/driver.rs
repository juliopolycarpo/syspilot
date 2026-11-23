//! Driver tests: they read the real hardware through PawnIO.
//!
//! They run only with `SYSPILOT_TEST_PAWNIO=1`, from an elevated shell, on a
//! machine with PawnIO installed and the modules fetched by
//! `cargo xtask fetch-modules`; otherwise each test prints why it is skipped
//! and passes. Every access is a read, apart from the index writes the bus
//! layer documents. Never run them with the `spd-paging` feature on a machine
//! that has memory modules on the SMBus: that feature adds an SMBus write. With
//! the `fan-control` feature on they skip even when the variable is set.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use syspilot_hw::Provider;
use syspilot_hw::bus::pawn::{PawnMsr, PawnPchThermal, PawnSmbus, PawnSuperIo};
use syspilot_hw::bus::{BusKind, BusLocks, PchThermal, SystemBusLocks};
use syspilot_hw::chips::{Detected, SuperIoProvider, detect};
use syspilot_hw::cpu::{CpuIdentity, CpuTopology, IntelCpuProvider, SystemClock, Vendor};
use syspilot_hw::dimm::{DimmProvider, describe_controller};
use syspilot_hw::driver;
use syspilot_hw::pch::{PchThermalProvider, describe_identity};
use syspilot_pawnio::{Error, Executor, PawnIo};
use syspilot_proto::message::DriverState;
use syspilot_proto::sensor::SensorKind;

/// Whether the driver tests are enabled; prints the reason when not.
fn enabled(test: &str) -> bool {
    // A build with the fan-register write code must never open the real
    // driver from a test, whatever the environment says.
    if cfg!(feature = "fan-control") {
        eprintln!(
            "skipping {test}: the `fan-control` feature is on, driver tests never run with it"
        );
        return false;
    }
    if std::env::var("SYSPILOT_TEST_PAWNIO").as_deref() == Ok("1") {
        return true;
    }
    eprintln!("skipping {test}: set SYSPILOT_TEST_PAWNIO=1 (elevated, PawnIO installed) to run it");
    false
}

fn modules_dir() -> PathBuf {
    match std::env::var_os("SYSPILOT_PAWNIO_MODULES") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../pawnio-modules"),
    }
}

/// A handle with `file` loaded, or `None` (after printing why) when the
/// module refuses this machine.
fn load_module(test: &str, file: &str) -> Option<Arc<dyn Executor>> {
    let path = modules_dir().join(file);
    let blob = std::fs::read(&path).unwrap_or_else(|error| {
        panic!(
            "(driver test): expected module {} | received: {error}; run `cargo xtask fetch-modules`",
            path.display()
        )
    });
    let device = PawnIo::open().unwrap_or_else(|error| {
        panic!("(driver test): expected an open driver | received: {error}")
    });
    match device.load(&blob) {
        Ok(()) => Some(Arc::new(device)),
        Err(Error::NotSupported { function: None }) => {
            eprintln!("skipping {test}: {file} refused this machine");
            None
        }
        Err(other) => panic!("(driver test): expected {file} to load | received: {other}"),
    }
}

#[test]
fn the_super_io_provider_reads_plausible_board_sensors() {
    let test = "the_super_io_provider_reads_plausible_board_sensors";
    if !enabled(test) {
        return;
    }
    let Some(module) = load_module(test, "LpcIO.bin") else { return };
    let locks: Arc<dyn BusLocks> = Arc::new(SystemBusLocks::new());
    let mut scanner = PawnSuperIo::new(module);
    let detection = {
        let _isa = locks.acquire(BusKind::Isa).expect("(driver test): expected the ISA bus mutex");
        detect(&mut scanner)
    };
    assert_eq!(detection.problems, Vec::<String>::new(), "(detection): expected a clean scan");
    for chip in &detection.chips {
        eprintln!("detected: {}", chip.describe());
    }
    let Some(chip) =
        detection.chips.iter().find(|chip| !matches!(chip, Detected::Unsupported { .. }))
    else {
        eprintln!("skipping {test}: no readable Super I/O chip on this machine");
        return;
    };
    // The provider needs its own handle: the module remembers the slot and the I/O ranges.
    let Some(own_module) = load_module(test, "LpcIO.bin") else { return };
    let mut provider = SuperIoProvider::open(chip, Box::new(PawnSuperIo::new(own_module)), locks)
        .unwrap_or_else(|error| {
            panic!("(super io provider): expected to open | received: {error}")
        });
    eprintln!("probe: {}", provider.summary());

    let sensors = provider.sensors();
    let mut values = Vec::new();
    provider.sample(&mut values);
    assert_eq!(values.len(), sensors.len(), "one value per sensor");
    let mut plausible_temperatures = 0;
    for (sensor, value) in sensors.iter().zip(&values) {
        eprintln!("{:<24} {:<16} {:?}", sensor.key, sensor.label, value);
        let Some(value) = *value else { continue };
        let range = match sensor.kind {
            SensorKind::Temperature => -55.0..=125.0,
            SensorKind::Voltage => 0.0..=20.0,
            SensorKind::Fan => 0.0..=30_000.0,
            SensorKind::Duty => 0.0..=100.0,
            other => {
                panic!("(super io provider): expected a board sensor kind | received: {other:?}")
            }
        };
        assert!(range.contains(&value), "({}): expected {range:?} | received: {value}", sensor.key);
        if sensor.kind == SensorKind::Temperature {
            plausible_temperatures += 1;
        }
    }
    assert!(plausible_temperatures > 0, "(super io provider): expected at least one temperature");
    assert_eq!(provider.last_problem(), None);
}

#[test]
fn the_dimm_scan_reports_what_the_platform_smbus_holds() {
    let test = "the_dimm_scan_reports_what_the_platform_smbus_holds";
    if !enabled(test) {
        return;
    }
    let Some(module) = load_module(test, "SmbusI801.bin") else { return };
    // The controller's identity is informational: three cells the module
    // fills with what it found at load time.
    let mut identity = [0u64; 3];
    module.execute("ioctl_identity", &[], &mut identity).unwrap_or_else(|error| {
        panic!("(dimm scan): expected ioctl_identity to answer | received: {error}")
    });
    let controller = describe_controller(identity);
    eprintln!("controller: {controller} (raw {identity:#x?})");
    let locks: Arc<dyn BusLocks> = Arc::new(SystemBusLocks::new());
    let probed = DimmProvider::probe(Box::new(PawnSmbus::new(module)), locks, &controller)
        .unwrap_or_else(|error| panic!("(dimm scan): expected a scan | received: {error}"));
    eprintln!("summary: {}", probed.summary);
    for section in &probed.info {
        eprintln!("{}", section.title);
        for (label, value) in &section.rows {
            eprintln!("    {label:<22} {value}");
        }
    }
    assert_eq!(probed.modules, probed.info.len(), "one section per module");
    let Some(mut provider) = probed.provider else {
        eprintln!("no DIMM temperature sensor on this bus; nothing to sample");
        return;
    };
    let sensors = provider.sensors();
    let mut values = Vec::new();
    provider.sample(&mut values);
    assert_eq!(values.len(), sensors.len(), "one value per sensor");
    for (sensor, value) in sensors.iter().zip(&values) {
        eprintln!("{:<22} {:<14} {:?}", sensor.key, sensor.device, value);
        let temperature = value
            .unwrap_or_else(|| panic!("({}): expected a temperature | received: None", sensor.key));
        assert!(
            (5.0..=95.0).contains(&temperature),
            "({}): expected 5..=95 degrees C | received: {temperature}",
            sensor.key
        );
    }
    assert_eq!(provider.last_problem(), None);
}

#[test]
fn the_chipset_provider_reads_a_plausible_temperature() {
    let test = "the_chipset_provider_reads_a_plausible_temperature";
    if !enabled(test) {
        return;
    }
    let Some(module) = load_module(test, "IntelPCHThermal.bin") else { return };
    // The identity is informational: five cells the module filled at load.
    let mut identity = [0u64; 5];
    module.execute("ioctl_identity", &[], &mut identity).unwrap_or_else(|error| {
        panic!("(chipset provider): expected ioctl_identity to answer | received: {error}")
    });
    let description = describe_identity(identity);
    eprintln!("identity: {description} (raw {identity:#x?})");

    let mut raw_reader = PawnPchThermal::new(module.clone());
    let reading = raw_reader.read_raw().unwrap_or_else(|error| {
        panic!("(chipset provider): expected a raw reading | received: {error}")
    });
    eprintln!("raw: {:#x} on {:?}", reading.raw, reading.route);

    let mut provider =
        PchThermalProvider::probe(Box::new(PawnPchThermal::new(module)), &description)
            .unwrap_or_else(|error| {
                panic!("(chipset provider): expected a probe | received: {error}")
            });
    eprintln!("probe: {}", provider.summary());
    let mut values = Vec::new();
    for _ in 0..3 {
        provider.sample(&mut values);
        std::thread::sleep(Duration::from_millis(500));
    }
    eprintln!("values: {values:?}");
    assert_eq!(values.len(), 3, "one value per sample");
    for value in values {
        let temperature = value.unwrap_or_else(|| {
            panic!("(chipset provider): expected a temperature | received: None")
        });
        assert!(
            (10.0..=100.0).contains(&temperature),
            "(chipset provider): expected 10..=100 degrees C | received: {temperature}"
        );
    }
    assert_eq!(provider.last_problem(), None);
}

#[test]
fn the_intel_provider_reads_plausible_temperatures_and_power() {
    let test = "the_intel_provider_reads_plausible_temperatures_and_power";
    if !enabled(test) {
        return;
    }
    let identity = CpuIdentity::read();
    if identity.vendor != Vendor::Intel {
        eprintln!("skipping {test}: the processor is {:?}", identity.vendor);
        return;
    }
    let Some(module) = load_module(test, "IntelMSR.bin") else { return };
    let topology = CpuTopology::read().expect("(topology): expected the OS topology");
    let mut provider = IntelCpuProvider::probe(
        &identity,
        &topology,
        Box::new(PawnMsr::new(module)),
        Box::new(SystemClock),
    )
    .unwrap_or_else(|error| panic!("(intel provider): expected a probe | received: {error}"));
    eprintln!("probe: {}", provider.summary());

    let sensors = provider.sensors();
    let mut values = Vec::new();
    provider.sample(&mut values);
    assert_eq!(values.len(), sensors.len(), "one value per sensor");
    std::thread::sleep(Duration::from_millis(2_200));
    values.clear();
    provider.sample(&mut values);

    for (sensor, value) in sensors.iter().zip(&values) {
        eprintln!("{:<22} {:<14} {:?}", sensor.key, sensor.label, value);
        if sensor.key.ends_with(".temp") {
            let temperature = value.unwrap_or_else(|| {
                panic!(
                    "(intel provider): expected a temperature for {} | received: None",
                    sensor.key
                )
            });
            assert!(
                (15.0..=110.0).contains(&temperature),
                "({}): expected 15..=110 degrees C | received: {temperature}",
                sensor.key
            );
        }
        if sensor.key == "cpu.package.power" {
            let watts = value.unwrap_or_else(|| {
                panic!(
                    "(intel provider): expected package power on the second sample | received: None"
                )
            });
            assert!(
                (0.0..=1000.0).contains(&watts),
                "({}): expected 0..=1000 W | received: {watts}",
                sensor.key
            );
        }
    }
    let package = sensors.iter().position(|s| s.key == "cpu.package.temp").expect("package sensor");
    let hottest = sensors.iter().position(|s| s.key == "cpu.core-max.temp").expect("max sensor");
    let core_values: Vec<f64> = sensors
        .iter()
        .zip(&values)
        .filter(|(s, _)| {
            s.key.starts_with("cpu.core")
                && s.key.ends_with(".temp")
                && s.key != "cpu.core-max.temp"
        })
        .filter_map(|(_, value)| *value)
        .collect();
    assert_eq!(
        values[hottest],
        core_values.iter().copied().reduce(f64::max),
        "(cpu.core-max.temp): expected the maximum of the core temperatures"
    );
    assert!(values[package].is_some());
}

/// The plausible range of a sensor's value, by kind.
fn plausible_range(kind: SensorKind) -> std::ops::RangeInclusive<f64> {
    match kind {
        SensorKind::Temperature => -55.0..=125.0,
        SensorKind::Voltage => 0.0..=20.0,
        SensorKind::Fan => 0.0..=30_000.0,
        SensorKind::Duty => 0.0..=100.0,
        SensorKind::Power => 0.0..=1000.0,
        other => panic!("(driver probe): expected a hardware sensor kind | received: {other:?}"),
    }
}

#[test]
fn the_probe_samples_every_sensor_once_a_second_without_a_problem() {
    let test = "the_probe_samples_every_sensor_once_a_second_without_a_problem";
    if !enabled(test) {
        return;
    }
    const SECONDS: usize = 10;
    let mut probe = driver::probe(&modules_dir());
    eprintln!("state: {:?}", probe.state);
    for feature in &probe.features {
        eprintln!(
            "feature {:<15} {:<12} {}",
            feature.key,
            if feature.available { "available" } else { "unavailable" },
            feature.detail
        );
    }
    for section in &probe.info {
        eprintln!("info: {}", section.title);
        for (label, value) in &section.rows {
            eprintln!("    {label:<22} {value}");
        }
    }
    assert!(
        matches!(probe.state, DriverState::Ready { .. }),
        "(driver probe): expected a ready driver | received: {:?}",
        probe.state
    );
    assert_eq!(probe.features.len(), 5, "(driver probe): expected five features");
    assert!(!probe.providers.is_empty(), "(driver probe): expected at least one provider");

    let sensors: Vec<_> = probe.providers.iter().map(|provider| provider.sensors()).collect();
    let mut keys: Vec<&str> = sensors.iter().flatten().map(|sensor| sensor.key.as_str()).collect();
    let declared = keys.len();
    keys.sort_unstable();
    keys.dedup();
    assert_eq!(keys.len(), declared, "(driver probe): expected unique sensor keys");

    // Per provider, per sensor: every value seen in the window.
    let mut seen: Vec<Vec<Vec<Option<f64>>>> =
        sensors.iter().map(|specs| vec![Vec::new(); specs.len()]).collect();
    for second in 0..SECONDS {
        for (index, provider) in probe.providers.iter_mut().enumerate() {
            let mut values = Vec::new();
            provider.sample(&mut values);
            assert_eq!(
                values.len(),
                sensors[index].len(),
                "(provider {index}, second {second}): expected one value per sensor"
            );
            for (sensor_index, value) in values.into_iter().enumerate() {
                seen[index][sensor_index].push(value);
            }
            assert_eq!(
                provider.last_problem(),
                None,
                "(provider {index}, second {second}): expected no problem"
            );
        }
        if second + 1 < SECONDS {
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    for (specs, per_sensor) in sensors.iter().zip(&seen) {
        for (spec, values) in specs.iter().zip(per_sensor) {
            let present: Vec<f64> = values.iter().copied().flatten().collect();
            eprintln!(
                "{:<26} {:<22} {:>2}/{SECONDS} values, min {:>9.2}, max {:>9.2}, last {:?}",
                spec.key,
                spec.label,
                present.len(),
                present.iter().copied().fold(f64::INFINITY, f64::min),
                present.iter().copied().fold(f64::NEG_INFINITY, f64::max),
                values.last().copied().flatten()
            );
            assert!(
                !present.is_empty(),
                "({}): expected at least one value in {SECONDS} seconds | received: none",
                spec.key
            );
            let range = plausible_range(spec.kind);
            for value in present {
                assert!(
                    range.contains(&value),
                    "({}): expected {range:?} | received: {value}",
                    spec.key
                );
            }
        }
    }
}
