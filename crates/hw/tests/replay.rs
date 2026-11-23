//! Replays of what the reference machine's hardware returned.
//!
//! The fixtures in `tests/fixtures` were recorded by
//! `cargo run -p syspilot-hw --example record_fixtures` on a MACHINIST
//! E5-MR9A PRO (X99) with a Xeon E5-2680 v4 and an NCT6779D Super I/O. These
//! tests need no driver and run everywhere; they pin what the providers
//! decode from real register contents and which writes they issue.

use std::sync::Arc;
use std::time::Duration;

use syspilot_hw::Provider;
use syspilot_hw::bus::replay::{ReplayMsr, ReplaySuperIo, SuperIoWrite};
use syspilot_hw::bus::{
    BusKind, FakeBusLocks, Fixture, LogicalCpu, SuperIoSlot, Unlock, is_permitted_bank,
};
use syspilot_hw::chips::nct67xx::Chip;
use syspilot_hw::chips::{Detected, SuperIoProvider, detect};
use syspilot_hw::cpu::{CpuIdentity, CpuTopology, IntelCpuProvider, ManualClock, Package, Vendor};

const NCT6779D_FIXTURE: &str = include_str!("fixtures/nct6779d.txt");
const INTEL_FIXTURE: &str = include_str!("fixtures/intel-msr.txt");

fn fixture(text: &str) -> Fixture {
    Fixture::parse(text).unwrap_or_else(|error| panic!("(fixture): expected valid text | {error}"))
}

fn value_of(provider: &dyn Provider, values: &[Option<f64>], key: &str) -> Option<f64> {
    let position = provider
        .sensors()
        .iter()
        .position(|sensor| sensor.key == key)
        .unwrap_or_else(|| panic!("(provider): expected a sensor `{key}` | received: none"));
    values[position]
}

fn close(actual: Option<f64>, expected: f64, key: &str) {
    let actual = actual.unwrap_or_else(|| panic!("({key}): expected {expected} | received: None"));
    assert!((actual - expected).abs() < 1e-6, "({key}): expected {expected} | received: {actual}");
}

// ---------------------------------------------------------------------------
// Super I/O: NCT6779D
// ---------------------------------------------------------------------------

#[test]
fn detection_finds_the_nct6779d_on_the_primary_slot_and_nothing_on_the_secondary() {
    let mut io = ReplaySuperIo::new(fixture(NCT6779D_FIXTURE));
    let detection = detect(&mut io);
    assert_eq!(detection.problems, Vec::<String>::new());
    assert_eq!(
        detection.chips,
        [Detected::Nuvoton {
            slot: SuperIoSlot::Primary,
            chip: Chip::Nct6779D,
            id_and_revision: 0xC562,
            base: 0x0A20,
            lock_register: 0x00,
        }],
        "the I/O space lock bit is clear on this chip, which has no such lock"
    );
}

#[test]
fn the_provider_decodes_the_recorded_registers() {
    let mut io = ReplaySuperIo::new(fixture(NCT6779D_FIXTURE));
    let detection = detect(&mut io);
    let locks = Arc::new(FakeBusLocks::free());
    let mut provider = SuperIoProvider::open(&detection.chips[0], Box::new(io), locks.clone())
        .unwrap_or_else(|error| panic!("(replay): expected a provider | {error}"));
    // The replay answers from the recording: a provider that read a register
    // the live pass did not would fail here with the missing key.
    let mut values = Vec::new();
    provider.sample(&mut values);
    assert_eq!(values.len(), provider.sensors().len());
    assert_eq!(locks.live_guards(), 0);

    let read = |key: &str| value_of(&provider, &values, key);
    close(read("board.temp.peci0"), 38.0, "board.temp.peci0");
    close(read("board.temp.systin"), 48.0, "board.temp.systin");
    // Unconnected thermistor inputs float and read a fixed value: the
    // plausibility range does not catch them.
    close(read("board.temp.auxtin1"), 103.0, "board.temp.auxtin1");
    close(read("board.temp.auxtin2"), 109.0, "board.temp.auxtin2");
    close(read("board.temp.auxtin3"), 22.0, "board.temp.auxtin3");
    // 0xCE counts of 8 mV, doubled by the input's 2:1 divider.
    close(read("board.vin2"), 3.296, "board.vin2");
    close(read("board.vin3"), 3.296, "board.vin3");
    close(read("board.vin7"), 3.36, "board.vin7");
    close(read("board.vin8"), 3.28, "board.vin8");
    close(read("board.vin9"), 1.056, "board.vin9");
    close(read("board.fan1"), 0.0, "board.fan1");
    // count (0x46 << 5) | (0x0C & 0x1F) = 2252 -> 1.35e6 / 2252 rpm
    close(read("board.fan2"), 1_350_000.0 / 2252.0, "board.fan2");
    close(read("board.pwm2"), f64::from(0x16u8) / 2.55, "board.pwm2");
    assert_eq!(
        read("board.pwm1"),
        Some(100.0),
        "a full-scale duty register is 100 percent, not an error"
    );
}

#[test]
fn the_voltage_inputs_the_chip_reports_as_zero_or_clipped_are_not_declared() {
    let mut io = ReplaySuperIo::new(fixture(NCT6779D_FIXTURE));
    let detection = detect(&mut io);
    let provider =
        SuperIoProvider::open(&detection.chips[0], Box::new(io), Arc::new(FakeBusLocks::free()))
            .expect("(replay): expected a provider");
    let keys: Vec<String> = provider.sensors().into_iter().map(|sensor| sensor.key).collect();
    // Inputs 1, 4 and 5 read 0x00 and input 12 reads the clipped 0xFF.
    for absent in ["board.vin1", "board.vin4", "board.vin5", "board.vin12"] {
        assert!(!keys.iter().any(|key| key == absent), "{absent} was declared: {keys:?}");
    }
    assert!(keys.iter().any(|key| key == "board.vin8"), "the battery input is declared");
}

#[test]
fn every_write_of_a_detection_open_and_sample_is_on_the_allowed_list() {
    let mut io = ReplaySuperIo::new(fixture(NCT6779D_FIXTURE));
    let log = io.write_log();
    let detection = detect(&mut io);
    let locks = Arc::new(FakeBusLocks::free());
    let mut provider = SuperIoProvider::open(&detection.chips[0], Box::new(io), locks)
        .expect("(replay): expected a provider");
    // `open` moved the replay; its log handle still sees the writes.
    provider.sample(&mut Vec::new());

    let writes = log.lock().expect("write log").clone();
    assert!(writes.len() > 50, "(write log): expected a full pass | received: {}", writes.len());
    // The sequence last entered and not yet left.
    let mut in_config_mode: Option<Unlock> = None;
    let mut last_index = None;
    for write in &writes {
        match *write {
            SuperIoWrite::SelectSlot(_) => {
                // The ITE unlock sequence on a slot no ITE chip answers on
                // enters nothing, and its leave sequence is deliberately not
                // written to a bus nothing answered on. Any other open
                // configuration mode is a leak.
                assert!(
                    matches!(in_config_mode, None | Some(Unlock::Ite)),
                    "(write log): slot selected inside {in_config_mode:?} configuration mode"
                );
                in_config_mode = None;
            }
            SuperIoWrite::Enter(unlock) => {
                assert_eq!(in_config_mode, None, "(write log): nested configuration mode");
                in_config_mode = Some(unlock);
            }
            SuperIoWrite::Exit(unlock) => {
                assert_eq!(
                    in_config_mode,
                    Some(unlock),
                    "(write log): expected an exit matching the enter sequence"
                );
                in_config_mode = None;
            }
            SuperIoWrite::LogicalDevice(device) => {
                assert!(in_config_mode.is_some());
                assert!(
                    [0x0B, 0x04].contains(&device),
                    "(logical device): expected the hardware monitor 0x0b or 0x04 | received: {device:#x}"
                );
            }
            SuperIoWrite::DiscoverRanges => assert!(in_config_mode.is_some()),
            SuperIoWrite::HwmIndex { index, .. } => last_index = Some(index),
            SuperIoWrite::HwmBank { bank, .. } => {
                assert_eq!(
                    last_index,
                    Some(0x4E),
                    "(bank write): expected the bank-select index just before it"
                );
                assert!(
                    is_permitted_bank(bank),
                    "(bank): expected 0..=0x0f or 0x80 | received: {bank:#x}"
                );
            }
            // The audit covers read-only scans: a fan write in one is a bug.
            #[cfg(feature = "fan-control")]
            SuperIoWrite::HwmFanRegister { address, value, .. } => panic!(
                "(write log): expected no fan register write in a read-only scan | received: {value:#04x} to {address:#06x}"
            ),
        }
    }
    assert_eq!(in_config_mode, None, "(write log): configuration mode left open");
}

#[test]
fn the_detection_scan_of_the_reference_machine_writes_exactly_this() {
    let mut io = ReplaySuperIo::new(fixture(NCT6779D_FIXTURE));
    let log = io.write_log();
    detect(&mut io);
    let primary = SuperIoSlot::Primary;
    let secondary = SuperIoSlot::Secondary;
    assert_eq!(
        *log.lock().expect("write log"),
        [
            SuperIoWrite::SelectSlot(primary),
            SuperIoWrite::Enter(Unlock::WinbondNuvoton),
            SuperIoWrite::DiscoverRanges,
            SuperIoWrite::LogicalDevice(0x0B),
            SuperIoWrite::Exit(Unlock::WinbondNuvoton),
            // Vendor ID: the high byte through bank 0x80, the low byte through bank 0.
            SuperIoWrite::HwmIndex { base: 0x0A20, index: 0x4E },
            SuperIoWrite::HwmBank { base: 0x0A20, bank: 0x80 },
            SuperIoWrite::HwmIndex { base: 0x0A20, index: 0x4F },
            SuperIoWrite::HwmIndex { base: 0x0A20, index: 0x4E },
            SuperIoWrite::HwmBank { base: 0x0A20, bank: 0x00 },
            SuperIoWrite::HwmIndex { base: 0x0A20, index: 0x4F },
            // Nothing answers on the second pair: the ITE sequence is written and
            // no leave sequence follows it.
            SuperIoWrite::SelectSlot(secondary),
            SuperIoWrite::Enter(Unlock::WinbondNuvoton),
            SuperIoWrite::Exit(Unlock::WinbondNuvoton),
            SuperIoWrite::Enter(Unlock::Ite),
        ]
    );
}

#[test]
fn a_sample_leaves_the_monitor_on_bank_zero() {
    let mut io = ReplaySuperIo::new(fixture(NCT6779D_FIXTURE));
    let log = io.write_log();
    let detection = detect(&mut io);
    let mut provider =
        SuperIoProvider::open(&detection.chips[0], Box::new(io), Arc::new(FakeBusLocks::free()))
            .expect("(replay): expected a provider");
    log.lock().expect("write log").clear();
    provider.sample(&mut Vec::new());
    let last_bank = log.lock().expect("write log").iter().rev().find_map(|write| match write {
        SuperIoWrite::HwmBank { bank, .. } => Some(*bank),
        _ => None,
    });
    assert_eq!(last_bank, Some(0), "(bank): expected a pass to end on bank 0");
}

#[test]
fn a_sample_takes_the_isa_bus_mutex_and_nothing_else() {
    let mut io = ReplaySuperIo::new(fixture(NCT6779D_FIXTURE));
    let detection = detect(&mut io);
    let locks = Arc::new(FakeBusLocks::free());
    let mut provider = SuperIoProvider::open(&detection.chips[0], Box::new(io), locks.clone())
        .expect("(replay): expected a provider");
    provider.sample(&mut Vec::new());
    assert_eq!(locks.acquired(), [BusKind::Isa, BusKind::Isa], "one for open, one for the sample");
    assert_eq!(locks.live_guards(), 0);
}

// ---------------------------------------------------------------------------
// Intel: Xeon E5-2680 v4
// ---------------------------------------------------------------------------

fn intel_identity() -> CpuIdentity {
    CpuIdentity {
        vendor: Vendor::Intel,
        family: 6,
        model: 0x4F,
        stepping: 1,
        brand: "Intel(R) Xeon(R) CPU E5-2680 v4 @ 2.40GHz".to_owned(),
    }
}

/// One package, the first logical processor of each of the 14 cores: 0, 2, ..., 26.
fn intel_topology() -> CpuTopology {
    CpuTopology {
        packages: vec![Package {
            cores: (0..14).map(|core| LogicalCpu { group: 0, index: core * 2 }).collect(),
        }],
    }
}

/// The recorded package energy counter and the rate it counted at.
const INTEL_BASELINE_COUNTER: u64 = 0x0FC8_A8E0;
/// Counts the reference machine's package counter advanced in the probe's
/// 250 ms window while idle (about 12 900 a second, measured 2026-11-23):
/// 0.79 W at the recorded unit of 2^-14 J, for a 120 W part.
const INTEL_IDLE_COUNTS_PER_WINDOW: u64 = 3_230;
/// The same with all 28 logical processors busy (about 42 700 a second):
/// 2.6 W.
const INTEL_BUSY_COUNTS_PER_WINDOW: u64 = 10_700;

/// A replay of the recording whose package counter, read by the probe once
/// for its existence and once after its watch window, then by two samples,
/// returns `counters` in turn.
fn intel_replay_with_counters(counters: &[u64]) -> ReplayMsr {
    let mut msr = ReplayMsr::from_fixture(&fixture(INTEL_FIXTURE));
    msr.set_sequence(LogicalCpu { group: 0, index: 0 }, 0x611, counters);
    msr
}

fn probe_intel(msr: ReplayMsr, clock: &ManualClock) -> IntelCpuProvider {
    IntelCpuProvider::probe(
        &intel_identity(),
        &intel_topology(),
        Box::new(msr),
        Box::new(clock.clone()),
    )
    .unwrap_or_else(|error| panic!("(replay): expected a probe | {error}"))
}

/// Issue 3: the package meter of the reference machine counts at 0.8 W idle
/// and 2.6 W busy for a 120 W part. The probe must not offer it, and must say
/// why.
#[test]
fn the_intel_probe_withholds_the_reference_machines_package_power_and_says_why() {
    for (load, counts, reading) in [
        ("idle", INTEL_IDLE_COUNTS_PER_WINDOW, "0.79"),
        ("busy", INTEL_BUSY_COUNTS_PER_WINDOW, "2.61"),
    ] {
        let msr =
            intel_replay_with_counters(&[INTEL_BASELINE_COUNTER, INTEL_BASELINE_COUNTER + counts]);
        let provider = probe_intel(msr, &ManualClock::new());
        assert_eq!(
            provider.summary(),
            format!(
                "14 cores, package and per-core temperature (TjMax 100 °C); not available: \
                 package (reads {reading} W, below the 6.0 W floor for a 120 W part), dram \
                 (counter reads 0)"
            ),
            "({load})"
        );
        let keys: Vec<String> = provider.sensors().into_iter().map(|sensor| sensor.key).collect();
        assert_eq!(keys.len(), 1 + 14 + 1, "({load}) package, 14 cores, hottest: {keys:?}");
        assert!(
            keys.iter().all(|key| !key.ends_with(".power")),
            "({load}) expected no power sensor | received: {keys:?}"
        );
    }
}

/// What the same recording yields from a meter that works: 40 W (10 J in the
/// 250 ms window) clears the floor, and the sensor follows the unit register.
#[test]
fn the_intel_probe_offers_package_power_from_a_meter_above_the_floor() {
    let baseline = INTEL_BASELINE_COUNTER;
    let in_window = baseline + 10 * 16_384;
    let clock = ManualClock::new();
    // Then 80 J over the 2 s between the two samples is 40 W again.
    let msr =
        intel_replay_with_counters(&[baseline, in_window, in_window, in_window + 80 * 16_384]);
    let mut provider = probe_intel(msr, &clock);
    assert_eq!(
        provider.summary(),
        "14 cores, package and per-core temperature (TjMax 100 °C); power: package; not available: \
         dram (counter reads 0)"
    );
    provider.sample(&mut Vec::new());
    clock.advance(Duration::from_secs(2));
    let mut values = Vec::new();
    provider.sample(&mut values);
    close(value_of(&provider, &values, "cpu.package.power"), 40.0, "cpu.package.power");
}

#[test]
fn the_intel_temperatures_decode_from_the_recorded_status_registers() {
    let mut provider = IntelCpuProvider::probe(
        &intel_identity(),
        &intel_topology(),
        Box::new(ReplayMsr::from_fixture(&fixture(INTEL_FIXTURE))),
        Box::new(ManualClock::new()),
    )
    .expect("(replay): expected a probe");
    let mut values = Vec::new();
    provider.sample(&mut values);
    let read = |key: &str| value_of(&provider, &values, key);
    // TjMax 100 minus the digital readout in bits 22:16 of each status register.
    close(read("cpu.package.temp"), 47.0, "cpu.package.temp");
    close(read("cpu.core0.temp"), 40.0, "cpu.core0.temp");
    close(read("cpu.core1.temp"), 41.0, "cpu.core1.temp");
    close(read("cpu.core13.temp"), 42.0, "cpu.core13.temp");
    close(read("cpu.core-max.temp"), 45.0, "cpu.core-max.temp");
}
