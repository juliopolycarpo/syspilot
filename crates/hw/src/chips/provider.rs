//! The [`Provider`] over one detected Super I/O chip.
//!
//! Everything happens inside [`Provider::sample`], on the caller's thread:
//! take the ISA bus mutex with a bounded wait, read the chip, release. A
//! bus that another program holds skips the sample; a bus error skips the
//! next samples with a growing pause instead of hammering a port that just
//! failed.

use std::fmt;
use std::sync::Arc;

use syspilot_proto::sensor::{DeviceClass, SensorKind};

use crate::backoff::Backoff;
use crate::bus::{BusError, BusKind, BusLocks, SuperIo, SuperIoSlot, Unlock};
use crate::hub::{Provider, SensorSpec};

use super::detect::{ConfigMode, Detected};
use super::it87xx::It87xx;
use super::nct67xx::Nct67xx;
use super::{Channel, MonitorChip};

/// Seconds between reads. Fans and board temperatures move slowly, and every
/// pass is a few hundred port accesses on a bus other tools share.
pub const PERIOD_SECS: u32 = 2;

/// The longest pause after repeated bus errors, in skipped samples.
const MAXIMUM_BACKOFF_SAMPLES: u32 = 8;

/// Why a detected chip yielded no provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderError {
    /// The chip was found but this crate does not read it.
    Unsupported {
        /// What was found and why it is not read.
        description: String,
    },
    /// A bus access failed while setting up.
    Bus(BusError),
    /// The chip answered but no register held a plausible reading.
    NoReadings {
        /// The chip.
        chip: String,
    },
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported { description } => f.write_str(description),
            Self::Bus(error) => error.fmt(f),
            Self::NoReadings { chip } => write!(
                f,
                "(superio): expected at least one plausible reading from {chip} | received: none"
            ),
        }
    }
}

impl std::error::Error for ProviderError {}

impl From<BusError> for ProviderError {
    fn from(error: BusError) -> Self {
        Self::Bus(error)
    }
}

/// Temperatures, voltages, fans and fan duty of one Super I/O chip.
pub struct SuperIoProvider {
    chip_name: String,
    summary: String,
    /// Dedicated to this chip: the driver module remembers the selected slot
    /// and the I/O ranges it learned, so two chips cannot share one.
    io: Box<dyn SuperIo>,
    locks: Arc<dyn BusLocks>,
    chip: Box<dyn MonitorChip>,
    channels: Vec<Channel>,
    backoff: Backoff,
    last_problem: Option<String>,
}

impl SuperIoProvider {
    /// Opens the provider for `detected` on `io`, which must be a fresh
    /// handle on the LpcIO module.
    ///
    /// Re-enters configuration mode once so that this handle's module learns
    /// the chip's I/O ranges, then reads the chip once to decide which
    /// channels exist. Takes the ISA bus mutex itself.
    pub fn open(
        detected: &Detected,
        io: Box<dyn SuperIo>,
        locks: Arc<dyn BusLocks>,
    ) -> Result<Self, ProviderError> {
        let (slot, unlock, chip): (_, _, Box<dyn MonitorChip>) = match detected {
            Detected::Nuvoton { slot, chip, base, .. } => {
                (*slot, Unlock::WinbondNuvoton, Box::new(Nct67xx::new(*chip, *base)))
            }
            Detected::Ite { slot, chip, version, base } => {
                (*slot, Unlock::Ite, Box::new(It87xx::new(*chip, *version, *base)))
            }
            Detected::Unsupported { .. } => {
                return Err(ProviderError::Unsupported { description: detected.describe() });
            }
        };
        Self::open_chip(slot, unlock, chip, detected.describe(), io, locks)
    }

    /// [`SuperIoProvider::open`] for a chip reader already built.
    fn open_chip(
        slot: SuperIoSlot,
        unlock: Unlock,
        mut chip: Box<dyn MonitorChip>,
        description: String,
        mut io: Box<dyn SuperIo>,
        locks: Arc<dyn BusLocks>,
    ) -> Result<Self, ProviderError> {
        let channels = {
            let _bus = locks.acquire(BusKind::Isa)?;
            io.select_slot(slot)?;
            let mut mode = ConfigMode::enter(io.as_mut(), unlock)?;
            let ranges = mode.discover_io_ranges();
            let left = mode.leave();
            ranges?;
            left?;
            chip.discover(io.as_mut())?
        };
        let chip_name = chip.name();
        if channels.is_empty() {
            return Err(ProviderError::NoReadings { chip: chip_name });
        }
        let summary = format!("{description} ({})", count_by_kind(&channels));
        Ok(Self {
            chip_name,
            summary,
            io,
            locks,
            chip,
            channels,
            backoff: Backoff::new(MAXIMUM_BACKOFF_SAMPLES),
            last_problem: None,
        })
    }

    /// The chip's name.
    pub fn chip_name(&self) -> &str {
        &self.chip_name
    }

    /// What was found, for the feature detail.
    pub fn summary(&self) -> &str {
        &self.summary
    }

    /// Reads the chip once under the bus mutex.
    fn read_once(&mut self) -> Result<Vec<Option<f64>>, BusError> {
        let locks = Arc::clone(&self.locks);
        let _bus = locks.acquire(BusKind::Isa)?;
        let values = self.chip.read(self.io.as_mut())?;
        if values.len() != self.channels.len() {
            return Err(BusError::rejected(
                format!("{} read", self.chip_name),
                format!("{} values, one per channel", self.channels.len()),
                format!("{} values", values.len()),
            ));
        }
        Ok(values)
    }
}

/// "5 temperatures, 11 voltages, 5 fans, 5 duty".
fn count_by_kind(channels: &[Channel]) -> String {
    let count = |kind: SensorKind| channels.iter().filter(|channel| channel.kind == kind).count();
    format!(
        "{} temperatures, {} voltages, {} fans, {} duty",
        count(SensorKind::Temperature),
        count(SensorKind::Voltage),
        count(SensorKind::Fan),
        count(SensorKind::Duty)
    )
}

impl Provider for SuperIoProvider {
    fn sensors(&self) -> Vec<SensorSpec> {
        self.channels
            .iter()
            .map(|channel| SensorSpec {
                device: self.chip_name.clone(),
                class: DeviceClass::Board,
                key: channel.key.clone(),
                label: channel.label.clone(),
                kind: channel.kind,
            })
            .collect()
    }

    fn sample(&mut self, out: &mut Vec<Option<f64>>) {
        if self.backoff.skip_this_sample() {
            out.extend(std::iter::repeat_n(None, self.channels.len()));
            return;
        }
        match self.read_once() {
            Ok(values) => {
                self.backoff.record_success();
                self.last_problem = None;
                out.extend(values);
            }
            Err(error) => {
                // Another program holding the bus is routine and says nothing
                // about the chip; anything else is a reason to back off.
                if !error.is_contended() {
                    self.backoff.record_failure();
                }
                self.last_problem = Some(error.to_string());
                out.extend(std::iter::repeat_n(None, self.channels.len()));
            }
        }
    }

    fn period_secs(&self) -> u32 {
        PERIOD_SECS
    }

    fn needs_driver(&self) -> bool {
        true
    }

    fn last_problem(&self) -> Option<&str> {
        self.last_problem.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::bus::replay::{
        ReplaySuperIo, SuperIoWrite, superio_config_key, superio_hwm_key, superio_unlock_key,
        unlock_code,
    };
    use crate::bus::{FakeBusLocks, Fixture};
    use crate::chips::it87xx;
    use crate::chips::nct67xx::{self, reference_registers};

    const BASE: u16 = 0x0A20;
    const PRIMARY: SuperIoSlot = SuperIoSlot::Primary;

    /// The NCT6779D of the reference machine on the primary slot.
    fn reference_fixture() -> Fixture {
        let mut fixture = Fixture::new();
        fixture.set(superio_unlock_key(PRIMARY), unlock_code(Unlock::WinbondNuvoton));
        fixture.set(superio_config_key(PRIMARY, 0, 0x20), 0xC5);
        fixture.set(superio_config_key(PRIMARY, 0, 0x21), 0x62);
        for (address, value) in reference_registers() {
            fixture.set(
                superio_hwm_key(PRIMARY, (address >> 8) as u8, address as u8),
                u64::from(value),
            );
        }
        fixture
    }

    fn nct6779d() -> Detected {
        Detected::Nuvoton {
            slot: PRIMARY,
            chip: nct67xx::Chip::Nct6779D,
            id_and_revision: 0xC562,
            base: BASE,
            lock_register: 0,
        }
    }

    /// While the switch is on, every data-port read times out, like a bus
    /// that stopped answering.
    #[derive(Clone, Default)]
    struct FaultSwitch(Arc<AtomicBool>);

    impl FaultSwitch {
        fn trip(&self) {
            self.0.store(true, Ordering::SeqCst);
        }

        fn clear(&self) {
            self.0.store(false, Ordering::SeqCst);
        }
    }

    struct FaultableSuperIo {
        inner: ReplaySuperIo,
        fault: FaultSwitch,
    }

    fn timeout() -> BusError {
        BusError::Driver(syspilot_pawnio::Error::Timeout { function: "ioctl_pio_inb".into() })
    }

    impl SuperIo for FaultableSuperIo {
        fn select_slot(&mut self, slot: SuperIoSlot) -> Result<(), BusError> {
            self.inner.select_slot(slot)
        }
        fn enter_config_mode(&mut self, unlock: Unlock) -> Result<(), BusError> {
            self.inner.enter_config_mode(unlock)
        }
        fn exit_config_mode(&mut self, unlock: Unlock) -> Result<(), BusError> {
            self.inner.exit_config_mode(unlock)
        }
        fn select_logical_device(&mut self, device: u8) -> Result<(), BusError> {
            self.inner.select_logical_device(device)
        }
        fn config_read(&mut self, register: u8) -> Result<u8, BusError> {
            self.inner.config_read(register)
        }
        fn discover_io_ranges(&mut self) -> Result<(), BusError> {
            self.inner.discover_io_ranges()
        }
        fn hwm_set_index(&mut self, base: u16, index: u8) -> Result<(), BusError> {
            self.inner.hwm_set_index(base, index)
        }
        fn hwm_write_bank(&mut self, base: u16, bank: u8) -> Result<(), BusError> {
            self.inner.hwm_write_bank(base, bank)
        }
        fn hwm_read_index(&mut self, base: u16) -> Result<u8, BusError> {
            self.inner.hwm_read_index(base)
        }
        fn hwm_read_data(&mut self, base: u16) -> Result<u8, BusError> {
            if self.fault.0.load(Ordering::SeqCst) {
                return Err(timeout());
            }
            self.inner.hwm_read_data(base)
        }
    }

    struct Opened {
        provider: SuperIoProvider,
        locks: Arc<FakeBusLocks>,
        log: Arc<Mutex<Vec<SuperIoWrite>>>,
        fault: FaultSwitch,
    }

    fn open_reference() -> Opened {
        let inner = ReplaySuperIo::new(reference_fixture());
        let log = inner.write_log();
        let fault = FaultSwitch::default();
        let io = FaultableSuperIo { inner, fault: fault.clone() };
        let locks = Arc::new(FakeBusLocks::free());
        let provider = SuperIoProvider::open(&nct6779d(), Box::new(io), locks.clone())
            .unwrap_or_else(|error| panic!("open failed: {error}"));
        Opened { provider, locks, log, fault }
    }

    fn sampled(provider: &mut SuperIoProvider) -> Vec<Option<f64>> {
        let mut out = Vec::new();
        provider.sample(&mut out);
        out
    }

    fn value_of(provider: &SuperIoProvider, values: &[Option<f64>], key: &str) -> Option<f64> {
        let position = provider.sensors().iter().position(|spec| spec.key == key);
        values[position.unwrap_or_else(|| panic!("no sensor `{key}`"))]
    }

    #[test]
    fn the_provider_declares_the_reference_channels_under_the_board_class() {
        let Opened { provider, locks, .. } = open_reference();
        let specs = provider.sensors();
        // 5 temperatures, 11 voltages, 5 fans and 5 duty outputs.
        assert_eq!(specs.len(), 26, "{specs:#?}");
        assert!(specs.iter().all(|s| s.device == "NCT6779D" && s.class == DeviceClass::Board));
        let key_kind = |key: &str| specs.iter().find(|s| s.key == key).map(|s| s.kind);
        assert_eq!(key_kind("board.temp.peci0"), Some(SensorKind::Temperature));
        assert_eq!(key_kind("board.vin2"), Some(SensorKind::Voltage));
        assert_eq!(key_kind("board.fan2"), Some(SensorKind::Fan));
        assert_eq!(key_kind("board.pwm2"), Some(SensorKind::Duty));
        assert_eq!(provider.period_secs(), 2);
        assert!(provider.needs_driver());
        assert_eq!(locks.live_guards(), 0, "open released the bus mutex");
        assert_eq!(locks.acquired(), [BusKind::Isa]);
    }

    #[test]
    fn open_learns_the_io_ranges_and_leaves_configuration_mode() {
        let Opened { log, .. } = open_reference();
        let writes = log.lock().unwrap().clone();
        assert_eq!(
            writes[..4],
            [
                SuperIoWrite::SelectSlot(PRIMARY),
                SuperIoWrite::Enter(Unlock::WinbondNuvoton),
                SuperIoWrite::DiscoverRanges,
                SuperIoWrite::Exit(Unlock::WinbondNuvoton),
            ]
        );
    }

    #[test]
    fn a_sample_returns_one_value_per_sensor_and_releases_the_mutex() {
        let Opened { mut provider, locks, .. } = open_reference();
        let values = sampled(&mut provider);
        assert_eq!(values.len(), provider.sensors().len());
        assert_eq!(value_of(&provider, &values, "board.temp.peci0"), Some(34.0));
        assert_eq!(value_of(&provider, &values, "board.temp.systin"), Some(48.0));
        assert_eq!(value_of(&provider, &values, "board.fan1"), Some(0.0), "a stopped fan is 0 rpm");
        assert_eq!(locks.live_guards(), 0);
        assert_eq!(locks.acquired(), [BusKind::Isa, BusKind::Isa]);
        assert_eq!(provider.last_problem(), None);
    }

    #[test]
    fn a_sample_writes_only_bank_selects_and_indices() {
        let Opened { mut provider, log, .. } = open_reference();
        log.lock().unwrap().clear();
        sampled(&mut provider);
        let writes = log.lock().unwrap().clone();
        assert!(!writes.is_empty());
        for write in writes {
            assert!(
                matches!(write, SuperIoWrite::HwmIndex { .. } | SuperIoWrite::HwmBank { .. }),
                "(nct6779d sample): expected only index and bank writes | received: {write:?}"
            );
        }
    }

    #[test]
    fn a_held_bus_skips_the_sample_without_touching_the_chip_or_backing_off() {
        let Opened { mut provider, log, .. } = open_reference();
        let channels = provider.sensors().len();
        provider.locks = Arc::new(FakeBusLocks::held_by_another_program(&[BusKind::Isa]));
        log.lock().unwrap().clear();

        let values = sampled(&mut provider);
        assert_eq!(values, vec![None; channels]);
        assert!(log.lock().unwrap().is_empty(), "no port was written while the bus was held");
        assert_eq!(
            provider.last_problem(),
            Some(
                "(isa bus): expected the shared bus mutex within the wait limit | received: held by another program"
            )
        );

        // The next sample tries again straight away: contention is not a fault.
        provider.locks = Arc::new(FakeBusLocks::free());
        let values = sampled(&mut provider);
        assert!(values.iter().any(Option::is_some), "{values:?}");
        assert_eq!(provider.last_problem(), None);
    }

    #[test]
    fn a_bus_error_gives_none_releases_the_mutex_and_backs_off() {
        let Opened { mut provider, locks, fault, .. } = open_reference();
        let channels = provider.sensors().len();
        fault.trip();
        let values = sampled(&mut provider);
        assert_eq!(values, vec![None; channels]);
        assert_eq!(locks.live_guards(), 0, "the mutex is released on the error path");
        let problem = provider.last_problem().unwrap().to_owned();
        assert!(problem.contains("ioctl_pio_inb"), "{problem}");

        // One failure: the next sample is skipped without taking the bus.
        let acquired = locks.acquired().len();
        let values = sampled(&mut provider);
        assert_eq!(values, vec![None; channels]);
        assert_eq!(locks.acquired().len(), acquired, "a backed-off sample does not take the bus");
    }

    #[test]
    fn repeated_failures_double_the_pause_up_to_the_cap() {
        let Opened { mut provider, locks, fault, .. } = open_reference();
        fault.trip();
        let mut pauses = Vec::new();
        for _ in 0..6 {
            // Sit out the pause, then fail once more.
            while provider.backoff.pending_skips() > 0 {
                sampled(&mut provider);
            }
            sampled(&mut provider);
            pauses.push(provider.backoff.pending_skips());
        }
        assert_eq!(pauses, [1, 2, 4, 8, 8, 8]);
        assert_eq!(locks.live_guards(), 0);
    }

    #[test]
    fn a_successful_sample_resets_the_backoff() {
        let Opened { mut provider, fault, .. } = open_reference();
        fault.trip();
        sampled(&mut provider);
        assert_eq!(provider.backoff.failures(), 1);
        fault.clear();
        while provider.backoff.pending_skips() > 0 {
            sampled(&mut provider);
        }
        let values = sampled(&mut provider);
        assert!(values.iter().any(Option::is_some), "the bus recovered: {values:?}");
        assert_eq!((provider.backoff.failures(), provider.backoff.pending_skips()), (0, 0));
    }

    /// A chip reader with scripted answers.
    struct ScriptedChip {
        channels: Vec<Channel>,
        values: Vec<Option<f64>>,
    }

    impl MonitorChip for ScriptedChip {
        fn name(&self) -> String {
            "SCRIPTED".into()
        }
        fn discover(&mut self, _: &mut dyn SuperIo) -> Result<Vec<Channel>, BusError> {
            Ok(self.channels.clone())
        }
        fn read(&mut self, _: &mut dyn SuperIo) -> Result<Vec<Option<f64>>, BusError> {
            Ok(self.values.clone())
        }
    }

    fn open_scripted(chip: ScriptedChip) -> Result<SuperIoProvider, ProviderError> {
        SuperIoProvider::open_chip(
            PRIMARY,
            Unlock::WinbondNuvoton,
            Box::new(chip),
            "scripted".into(),
            Box::new(ReplaySuperIo::new(reference_fixture())),
            Arc::new(FakeBusLocks::free()),
        )
    }

    #[test]
    fn a_chip_returning_the_wrong_number_of_values_is_reported_not_trusted() {
        let mut provider = open_scripted(ScriptedChip {
            channels: vec![Channel::new("board.temp.a", "A", SensorKind::Temperature)],
            values: vec![],
        })
        .unwrap();
        assert_eq!(sampled(&mut provider), [None]);
        assert_eq!(
            provider.last_problem(),
            Some("(SCRIPTED read): expected 1 values, one per channel | received: 0 values")
        );
    }

    #[test]
    fn a_chip_with_no_channels_is_refused() {
        let Err(error) = open_scripted(ScriptedChip { channels: vec![], values: vec![] }) else {
            panic!("a chip without readings must not open");
        };
        assert_eq!(error, ProviderError::NoReadings { chip: "SCRIPTED".into() });
        assert_eq!(
            error.to_string(),
            "(superio): expected at least one plausible reading from SCRIPTED | received: none"
        );
    }

    #[test]
    fn an_unsupported_chip_is_refused_with_its_description_and_nothing_is_touched() {
        let detected = Detected::Unsupported {
            slot: PRIMARY,
            raw_id: 0xE711,
            name: None,
            reason: "(superio): expected a supported Nuvoton chip id/revision | received: 0xe711"
                .into(),
        };
        let locks = Arc::new(FakeBusLocks::free());
        let io = ReplaySuperIo::new(Fixture::new());
        let log = io.write_log();
        let Err(error) = SuperIoProvider::open(&detected, Box::new(io), locks.clone()) else {
            panic!("an unsupported chip must not open");
        };
        assert_eq!(
            error.to_string(),
            "unknown chip (id 0xe711) at ports 0x2e/0x2f is not read: (superio): expected a \
             supported Nuvoton chip id/revision | received: 0xe711"
        );
        assert!(locks.acquired().is_empty());
        assert!(log.lock().unwrap().is_empty());
    }

    #[test]
    fn open_with_a_held_bus_reports_contention_and_writes_nothing() {
        let io = ReplaySuperIo::new(reference_fixture());
        let log = io.write_log();
        let locks = Arc::new(FakeBusLocks::held_by_another_program(&[BusKind::Isa]));
        let Err(error) = SuperIoProvider::open(&nct6779d(), Box::new(io), locks) else {
            panic!("a held bus must not open");
        };
        assert_eq!(error, ProviderError::Bus(BusError::Contended(BusKind::Isa)));
        assert!(log.lock().unwrap().is_empty());
    }

    #[test]
    fn an_ite_chip_opens_through_the_ite_sequence() {
        let slot = SuperIoSlot::Secondary;
        let mut fixture = Fixture::new();
        fixture.set(superio_unlock_key(slot), unlock_code(Unlock::Ite));
        for register in 0..=0xFFu8 {
            fixture.set(superio_hwm_key(slot, 0, register), 0);
        }
        // IT8686E: input 0 at register 0x20 reads 100 counts of 12 mV.
        fixture.set(superio_hwm_key(slot, 0, 0x20), 100);
        let detected =
            Detected::Ite { slot, chip: it87xx::Chip::It8686E, version: 5, base: 0x0290 };
        let io = ReplaySuperIo::new(fixture);
        let log = io.write_log();
        let locks = Arc::new(FakeBusLocks::free());
        let mut provider = SuperIoProvider::open(&detected, Box::new(io), locks).unwrap();
        assert_eq!(provider.chip_name(), "IT8686E");
        let values = sampled(&mut provider);
        assert!((value_of(&provider, &values, "board.vin0").unwrap() - 1.2).abs() < 1e-9);
        let writes = log.lock().unwrap().clone();
        assert_eq!(writes[1], SuperIoWrite::Enter(Unlock::Ite));
        assert_eq!(writes[3], SuperIoWrite::Exit(Unlock::Ite));
    }

    #[test]
    fn the_summary_names_the_chip_where_it_answers_and_the_channel_counts() {
        let Opened { provider, .. } = open_reference();
        assert_eq!(
            provider.summary(),
            "NCT6779D (id/revision 0xc562) at ports 0x2e/0x2f, monitor at 0x0a20 (5 temperatures, \
             11 voltages, 5 fans, 5 duty)"
        );
    }
}
