//! Driver tests: they read the real hardware through PawnIO.
//!
//! They run only with `SYSPILOT_TEST_PAWNIO=1`, from an elevated shell, on a
//! machine with PawnIO installed and the modules fetched by
//! `cargo xtask fetch-modules`; otherwise each test prints why it is skipped
//! and passes. Every access is a read, apart from the index writes and the
//! SPD page select the bus layer documents.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use syspilot_hw::Provider;
use syspilot_hw::bus::pawn::PawnMsr;
use syspilot_hw::cpu::{CpuIdentity, CpuTopology, IntelCpuProvider, SystemClock, Vendor};
use syspilot_pawnio::{Error, Executor, PawnIo};

/// Whether the driver tests are enabled; prints the reason when not.
fn enabled(test: &str) -> bool {
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
