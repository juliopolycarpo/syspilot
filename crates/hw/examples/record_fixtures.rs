//! Records what this machine's hardware returns, as fixtures the replay
//! tests read.
//!
//! ```text
//! cargo run -p syspilot-hw --example record_fixtures -- <output-dir> superio intel
//! ```
//!
//! Run from an elevated shell with PawnIO installed and the modules fetched
//! by `cargo xtask fetch-modules` (or point `SYSPILOT_PAWNIO_MODULES` at
//! them). Everything is read-only; the only writes are the index and
//! configuration-mode writes the bus layer documents. A recording keeps no
//! serial numbers or other per-unit identifiers: each target removes them
//! before saving.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use syspilot_hw::Provider;
use syspilot_hw::bus::pawn::{PawnMsr, PawnSuperIo};
use syspilot_hw::bus::replay::{RecordingMsr, RecordingSuperIo, SharedFixture};
use syspilot_hw::bus::{BusKind, BusLocks, Fixture, SystemBusLocks};
use syspilot_hw::chips::{Detected, SuperIoProvider, detect};
use syspilot_hw::cpu::{CpuIdentity, CpuTopology, IntelCpuProvider, SystemClock, Vendor};
use syspilot_pawnio::{Executor, PawnIo};

fn modules_dir() -> PathBuf {
    match std::env::var_os("SYSPILOT_PAWNIO_MODULES") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../pawnio-modules"),
    }
}

fn load_module(file: &str) -> Arc<dyn Executor> {
    let path = modules_dir().join(file);
    let blob = std::fs::read(&path).unwrap_or_else(|error| {
        panic!("(record_fixtures): expected module {} | received: {error}", path.display())
    });
    let device = PawnIo::open()
        .unwrap_or_else(|error| panic!("(record_fixtures): expected an open driver | {error}"));
    device
        .load(&blob)
        .unwrap_or_else(|error| panic!("(record_fixtures): expected {file} to load | {error}"));
    Arc::new(device)
}

fn save(directory: &Path, name: &str, fixture: &SharedFixture, comments: &[&str]) {
    let mut fixture = fixture.lock().expect("fixture lock").clone();
    for comment in comments {
        fixture = fixture.with_comment(*comment);
    }
    let path = directory.join(name);
    std::fs::write(&path, fixture.to_text()).unwrap_or_else(|error| {
        panic!("(record_fixtures): expected to write {} | {error}", path.display())
    });
    println!("wrote {} ({} values)", path.display(), fixture.len());
}

/// The Super I/O chip: detection on both slots, the provider's probe and one
/// sample, every register read recorded.
fn record_superio(directory: &Path) {
    let shared: SharedFixture = Arc::new(Mutex::new(Fixture::new()));
    let locks: Arc<dyn BusLocks> = Arc::new(SystemBusLocks::new());
    let mut recorder =
        RecordingSuperIo::new(PawnSuperIo::new(load_module("LpcIO.bin")), shared.clone());

    let detection = {
        let _isa =
            locks.acquire(BusKind::Isa).expect("(record_fixtures): expected the ISA bus mutex");
        detect(&mut recorder)
    };
    for chip in &detection.chips {
        println!("detected: {}", chip.describe());
    }
    for problem in &detection.problems {
        println!("problem: {problem}");
    }
    let Some(chip) =
        detection.chips.iter().find(|chip| !matches!(chip, Detected::Unsupported { .. }))
    else {
        println!("no readable Super I/O chip; nothing recorded");
        return;
    };
    let mut provider = SuperIoProvider::open(chip, Box::new(recorder), locks)
        .unwrap_or_else(|error| panic!("(record_fixtures): expected a provider | {error}"));
    let mut values = Vec::new();
    provider.sample(&mut values);
    for (sensor, value) in provider.sensors().iter().zip(&values) {
        println!("{:<24} {:<16} {value:?}", sensor.key, sensor.label);
    }
    let name = format!("{}.txt", provider.chip_name().to_lowercase());
    save(
        directory,
        &name,
        &shared,
        &[
            &format!(
                "{} Super I/O hardware monitor, recorded from the reference machine.",
                provider.chip_name()
            ),
            "Detection on both slots, the provider's probe and one sample; read-only.",
            "Recorded by `cargo run -p syspilot-hw --example record_fixtures`.",
        ],
    );
}

/// The Intel package: probe and two samples of every register.
fn record_intel(directory: &Path) {
    let identity = CpuIdentity::read();
    if identity.vendor != Vendor::Intel {
        println!("the processor is {:?}, not Intel; nothing recorded", identity.vendor);
        return;
    }
    let topology = CpuTopology::read().expect("(record_fixtures): expected the OS topology");
    let shared: SharedFixture = Arc::new(Mutex::new(Fixture::new()));
    let recorder = RecordingMsr::new(PawnMsr::new(load_module("IntelMSR.bin")), shared.clone());
    let mut provider =
        IntelCpuProvider::probe(&identity, &topology, Box::new(recorder), Box::new(SystemClock))
            .unwrap_or_else(|error| panic!("(record_fixtures): expected a probe | {error}"));
    println!("probe: {}", provider.summary());
    let mut values = Vec::new();
    provider.sample(&mut values);
    std::thread::sleep(Duration::from_millis(2_200));
    values.clear();
    provider.sample(&mut values);
    for (sensor, value) in provider.sensors().iter().zip(&values) {
        println!("{:<24} {:<16} {value:?}", sensor.key, sensor.label);
    }
    let family = format!(
        "family {}, model {:#x}, stepping {}; {} cores",
        identity.family,
        identity.model,
        identity.stepping,
        topology.packages.first().map_or(0, |package| package.cores.len())
    );
    save(
        directory,
        "intel-msr.txt",
        &shared,
        &[
            &format!("{}: {family}.", identity.brand),
            "MSR values of the first logical processor of each core; the last value of each register.",
            "Recorded by `cargo run -p syspilot-hw --example record_fixtures`.",
        ],
    );
}

fn main() {
    let mut arguments = std::env::args().skip(1);
    let directory = PathBuf::from(
        arguments.next().expect("usage: record_fixtures <output-dir> [superio] [intel]"),
    );
    std::fs::create_dir_all(&directory)
        .expect("(record_fixtures): expected to create the output directory");
    let targets: Vec<String> = arguments.collect();
    for target in &targets {
        match target.as_str() {
            "superio" => record_superio(&directory),
            "intel" => record_intel(&directory),
            other => {
                panic!("(record_fixtures): expected one of superio, intel | received: {other}")
            }
        }
    }
}
