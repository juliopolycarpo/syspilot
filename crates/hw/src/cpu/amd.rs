//! AMD Zen (families 17h to 1Ah): package and CCD temperatures, RAPL power.
//!
//! **Validated by replay only.** No AMD machine has run this code; the
//! reference machine is an Intel Xeon. The register layout comes from the
//! Linux `k10temp` driver, whose author derived the CCD registers from the
//! zenpower project by experiment and notes that AMD's public documentation
//! does not confirm them, and from the `AMDFamily17` module's MSR list.
//! Treat a reading on a new model as unconfirmed until a recording from
//! that model sits next to the others in `tests/fixtures`.
//!
//! | Register | Meaning | Used as |
//! |---|---|---|
//! | SMN `0x59800` (`THM_TCON_CUR_TMP`) | bits 31:21 reported temperature in 0.125 degrees; bit 19 or bits 17:16 both set mean the range is shifted by -49 | Tctl |
//! | SMN `0x59800 + offset + 4 * n` | bit 11 valid, bits 10:0 in 0.125 degrees, always shifted by -49 | CCD `n` temperature |
//! | MSR `0xC0010299` | bits 12:8 energy unit, 1/2^n J | energy per count |
//! | MSR `0xC001029B` | package energy, 32-bit wrapping counter | power = delta * unit / time |
//!
//! Tctl is a control value, not a temperature: some first and second
//! generation parts report it up to 27 degrees above the die. `k10temp`
//! keeps a table of those models by brand string and this module does too,
//! so the package sensor is Tdie (Tctl minus the offset) and Tctl is only
//! declared where it differs.
//!
//! SMN reads go through the PCI configuration window and take the shared PCI
//! mutex. Only the first package is read: `ioctl_read_smn` addresses node 0.

use std::fmt::Write as _;
use std::sync::Arc;

use syspilot_proto::sensor::{DeviceClass, SensorKind};

use super::identity::{CpuIdentity, Vendor};
use super::rapl::{Clock, EnergyMeter};
use super::topology::CpuTopology;
use crate::backoff::Backoff;
use crate::bus::{BusError, BusKind, BusLocks, LogicalCpu, Msr, Smn};
use crate::hub::{Provider, SensorSpec};

/// `THM_TCON_CUR_TMP`: the reported temperature (Tctl).
const SMN_REPORTED_TEMPERATURE: u32 = 0x0005_9800;
const MSR_RAPL_POWER_UNIT: u32 = 0xC001_0299;
const MSR_PKG_ENERGY_STATUS: u32 = 0xC001_029B;

/// The reported temperature sits in bits 31:21.
const TEMPERATURE_SHIFT: u32 = 21;
/// Bit 19: the reading uses the -49 degree range.
const RANGE_SELECT: u32 = 1 << 19;
/// Bits 17:16 both set also select the -49 degree range.
const TJ_SELECT_MASK: u32 = 0b11 << 16;
/// CCD register bit 11: the CCD exists and the reading is valid.
const CCD_VALID: u32 = 1 << 11;
/// CCD register bits 10:0: the temperature.
const CCD_TEMPERATURE_MASK: u32 = 0x7FF;
/// Degrees per count of both temperature fields.
const DEGREES_PER_COUNT: f64 = 0.125;
/// The shifted range subtracts this many degrees.
const RANGE_SHIFT_DEGREES: f64 = 49.0;
/// Temperatures outside this range are not a measurement.
const PLAUSIBLE_TEMPERATURE: std::ops::RangeInclusive<f64> = -10.0..=150.0;
/// The energy counters are 32 bits wide; the upper bits are reserved.
const COUNTER_MASK: u64 = 0xFFFF_FFFF;
/// Seconds between samples.
const PERIOD_SECS: u32 = 2;
/// The longest pause after repeated bus errors, in skipped samples.
const MAXIMUM_BACKOFF_SAMPLES: u32 = 8;

/// Where a model keeps its CCD temperatures and how many it may have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CcdLayout {
    /// Added to the SMN thermal base `0x59800`.
    pub offset: u32,
    /// How many consecutive registers are probed.
    pub limit: u32,
}

/// The CCD register layout of a model, or `None` for models this module
/// has no layout for (they get the package temperature only).
///
/// From the `k10temp` probe function. Models that have no CCDs (APUs, first
/// generation parts) are in the table too: the valid bit of each register
/// decides which exist.
pub fn ccd_layout(family: u32, model: u32) -> Option<CcdLayout> {
    let layout = |offset, limit| Some(CcdLayout { offset, limit });
    match (family, model) {
        (0x17, 0x01 | 0x08 | 0x11 | 0x18) => layout(0x154, 4),
        (0x17, 0x31 | 0x47 | 0x60 | 0x68 | 0x71) => layout(0x154, 8),
        (0x17, 0xA0..=0xAF) => layout(0x300, 8),
        (0x19, 0x00 | 0x01 | 0x08 | 0x21 | 0x50..=0x5F) => layout(0x154, 8),
        (0x19, 0x40..=0x4F) => layout(0x300, 8),
        (0x19, 0x60..=0x7F) => layout(0x308, 8),
        (0x19, 0x10..=0x1F | 0xA0..=0xAF) => layout(0x300, 12),
        (0x1A, 0x00..=0x1F) => layout(0x1F0, 16),
        (0x1A, 0x40..=0x4F) => layout(0x308, 8),
        _ => None,
    }
}

/// First and second generation parts whose Tctl runs above the die
/// temperature, by the start of the brand string, and by how much. From
/// the `k10temp` table.
const TCTL_OFFSETS: [(&str, f64); 6] = [
    ("AMD Ryzen 5 1600X", 20.0),
    ("AMD Ryzen 7 1700X", 20.0),
    ("AMD Ryzen 7 1800X", 20.0),
    ("AMD Ryzen 7 2700X", 10.0),
    // 1900X, 1920X, 1950X.
    ("AMD Ryzen Threadripper 19", 27.0),
    // 2920X, 2950X, 2970WX, 2990WX.
    ("AMD Ryzen Threadripper 29", 27.0),
];

/// Degrees Tctl runs above Tdie on this processor. Only family 17h has such
/// parts.
pub fn tctl_offset(identity: &CpuIdentity) -> f64 {
    if identity.family != 0x17 {
        return 0.0;
    }
    TCTL_OFFSETS
        .iter()
        .find(|(prefix, _)| identity.brand.starts_with(prefix))
        .map_or(0.0, |&(_, offset)| offset)
}

/// Tctl in degrees from `THM_TCON_CUR_TMP`, or `None` for a register that
/// reads all ones (nothing answered) or a value outside a plausible range.
pub fn tctl_celsius(register: u32) -> Option<f64> {
    if register == u32::MAX {
        return None;
    }
    let mut degrees = f64::from(register >> TEMPERATURE_SHIFT) * DEGREES_PER_COUNT;
    if register & RANGE_SELECT != 0 || register & TJ_SELECT_MASK == TJ_SELECT_MASK {
        degrees -= RANGE_SHIFT_DEGREES;
    }
    PLAUSIBLE_TEMPERATURE.contains(&degrees).then_some(degrees)
}

/// A CCD temperature in degrees, or `None` when the CCD is not there (valid
/// bit clear), nothing answered (all ones) or the value is implausible.
pub fn ccd_celsius(register: u32) -> Option<f64> {
    if register == u32::MAX || register & CCD_VALID == 0 {
        return None;
    }
    let degrees =
        f64::from(register & CCD_TEMPERATURE_MASK) * DEGREES_PER_COUNT - RANGE_SHIFT_DEGREES;
    PLAUSIBLE_TEMPERATURE.contains(&degrees).then_some(degrees)
}

/// Why no AMD provider was created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AmdProbeError {
    /// The processor is not AMD.
    NotAmd {
        /// What it is.
        vendor: Vendor,
    },
    /// `AMDFamily17` loads on families 17h to 1Ah only.
    UnsupportedFamily {
        /// The family.
        family: u32,
    },
    /// The OS reported no cores.
    NoCores,
    /// The reported temperature could not be read or made no sense.
    NoTemperature {
        /// Why.
        cause: BusError,
    },
}

impl std::fmt::Display for AmdProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAmd { vendor } => {
                write!(f, "(amd cpu): expected an AMD processor | received: {vendor:?}")
            }
            Self::UnsupportedFamily { family } => {
                write!(f, "(amd cpu): expected family 0x17 to 0x1a | received: family {family:#x}")
            }
            Self::NoCores => {
                write!(f, "(amd cpu): expected at least one core in the topology | received: none")
            }
            Self::NoTemperature { cause } => write!(
                f,
                "(amd cpu): expected a readable reported temperature (SMN {SMN_REPORTED_TEMPERATURE:#x}) | \
                 received: {cause}"
            ),
        }
    }
}

impl std::error::Error for AmdProbeError {}

struct Ccd {
    /// Number shown to the user, from 1.
    number: u32,
    address: u32,
}

/// Temperatures and power of one AMD processor package.
pub struct AmdCpuProvider {
    device: String,
    smn: Box<dyn Smn>,
    msr: Box<dyn Msr>,
    locks: Arc<dyn BusLocks>,
    clock: Box<dyn Clock>,
    /// Package-scope MSRs are read on the first core.
    package_cpu: LogicalCpu,
    tctl_offset: f64,
    ccds: Vec<Ccd>,
    power: Option<EnergyMeter>,
    backoff: Backoff,
    last_problem: Option<String>,
    summary: String,
}

impl AmdCpuProvider {
    /// Probes the first package of `topology`.
    ///
    /// Reads the reported temperature once under the PCI mutex (failing the
    /// probe when it cannot), finds the CCDs whose registers are valid, and
    /// reads the RAPL unit and package counter once to decide whether power
    /// exists. The first [`Provider::sample`] takes the energy baseline and
    /// reports no power.
    ///
    /// # Errors
    ///
    /// See [`AmdProbeError`]. A bus the PCI mutex is contended on at probe
    /// time is [`AmdProbeError::NoTemperature`] with the cause
    /// [`BusError::Contended`]; the caller may try again later.
    pub fn probe(
        identity: &CpuIdentity,
        topology: &CpuTopology,
        mut smn: Box<dyn Smn>,
        mut msr: Box<dyn Msr>,
        locks: Arc<dyn BusLocks>,
        clock: Box<dyn Clock>,
    ) -> Result<Self, AmdProbeError> {
        if identity.vendor != Vendor::Amd {
            return Err(AmdProbeError::NotAmd { vendor: identity.vendor });
        }
        if !(0x17..=0x1A).contains(&identity.family) {
            return Err(AmdProbeError::UnsupportedFamily { family: identity.family });
        }
        let Some(package) = topology.packages.first().filter(|package| !package.cores.is_empty())
        else {
            return Err(AmdProbeError::NoCores);
        };
        let package_cpu = package.cores[0];

        let mut ccds = Vec::new();
        let mut notes = Vec::new();
        {
            let _pci = locks
                .acquire(BusKind::Pci)
                .map_err(|cause| AmdProbeError::NoTemperature { cause })?;
            let register = smn
                .read(SMN_REPORTED_TEMPERATURE)
                .map_err(|cause| AmdProbeError::NoTemperature { cause })?;
            if tctl_celsius(register).is_none() {
                return Err(AmdProbeError::NoTemperature {
                    cause: BusError::rejected(
                        "reported temperature",
                        "a plausible value (-10 to 150 degrees)",
                        format!("register {register:#010x}"),
                    ),
                });
            }
            match ccd_layout(identity.family, identity.model) {
                Some(layout) => {
                    for index in 0..layout.limit {
                        let address = SMN_REPORTED_TEMPERATURE + layout.offset + 4 * index;
                        match smn.read(address) {
                            Ok(register) if ccd_celsius(register).is_some() => {
                                ccds.push(Ccd { number: index + 1, address });
                            }
                            Ok(_) => {}
                            Err(error) => {
                                notes.push(format!(
                                    "CCD scan stopped at CCD {}: {error}",
                                    index + 1
                                ));
                                break;
                            }
                        }
                    }
                }
                None => notes.push(format!(
                    "no CCD layout for family {:#x} model {:#x}",
                    identity.family, identity.model
                )),
            }
        }

        let offset = tctl_offset(identity);
        let mut summary = format!(
            "family {:#x} model {:#x} (replay-validated only): {}",
            identity.family,
            identity.model,
            if offset == 0.0 {
                "Tctl equals Tdie".to_owned()
            } else {
                format!("Tdie is Tctl minus {offset} °C")
            }
        );
        let _ = write!(summary, ", {} CCD(s)", ccds.len());

        let power = match package_energy_unit(msr.as_mut(), package_cpu) {
            Some(unit) => match msr.read(package_cpu, MSR_PKG_ENERGY_STATUS) {
                // A counter that has run since power-on is never exactly zero.
                Ok(counter) if counter & COUNTER_MASK != 0 => {
                    summary.push_str("; power: package");
                    Some(EnergyMeter::new(unit))
                }
                Ok(_) => {
                    notes.push("package power (counter reads 0)".to_owned());
                    None
                }
                Err(error) => {
                    notes.push(format!("package power ({error})"));
                    None
                }
            },
            None => {
                notes.push("package power (no usable energy unit)".to_owned());
                None
            }
        };
        if !notes.is_empty() {
            let _ = write!(summary, "; not available: {}", notes.join(", "));
        }

        Ok(Self {
            device: identity.brand.clone(),
            smn,
            msr,
            locks,
            clock,
            package_cpu,
            tctl_offset: offset,
            ccds,
            power,
            backoff: Backoff::new(MAXIMUM_BACKOFF_SAMPLES),
            last_problem: None,
            summary,
        })
    }

    /// One-line description of what was found, for the feature detail.
    pub fn summary(&self) -> &str {
        &self.summary
    }

    /// Tctl and the CCD temperatures, read under one hold of the PCI mutex.
    fn read_temperatures(&mut self) -> Result<(Option<f64>, Vec<Option<f64>>), BusError> {
        let locks = Arc::clone(&self.locks);
        let _pci = locks.acquire(BusKind::Pci)?;
        let tctl = tctl_celsius(self.smn.read(SMN_REPORTED_TEMPERATURE)?);
        let mut ccds = Vec::with_capacity(self.ccds.len());
        for ccd in &self.ccds {
            ccds.push(ccd_celsius(self.smn.read(ccd.address)?));
        }
        Ok((tctl, ccds))
    }

    fn tdie(&self, tctl: f64) -> Option<f64> {
        let tdie = tctl - self.tctl_offset;
        PLAUSIBLE_TEMPERATURE.contains(&tdie).then_some(tdie)
    }

    fn declares_tctl(&self) -> bool {
        self.tctl_offset != 0.0
    }
}

/// Joules per count of the package energy counter, from bits 12:8 of
/// `MSR_RAPL_PWR_UNIT`. `None` when the register is unreadable or its energy
/// field is zero or implausibly large.
fn package_energy_unit(msr: &mut dyn Msr, cpu: LogicalCpu) -> Option<f64> {
    let raw = msr.read(cpu, MSR_RAPL_POWER_UNIT).ok()?;
    let exponent = (raw >> 8) & 0x1F;
    (1..=24).contains(&exponent).then(|| 1.0 / (1u64 << exponent) as f64)
}

impl Provider for AmdCpuProvider {
    fn sensors(&self) -> Vec<SensorSpec> {
        let spec = |key: String, label: String, kind| SensorSpec {
            device: self.device.clone(),
            class: DeviceClass::Cpu,
            key,
            label,
            kind,
        };
        let mut sensors =
            vec![spec("cpu.package.temp".into(), "Package".into(), SensorKind::Temperature)];
        if self.declares_tctl() {
            sensors.push(spec("cpu.package.tctl".into(), "Tctl".into(), SensorKind::Temperature));
        }
        for ccd in &self.ccds {
            sensors.push(spec(
                format!("cpu.ccd{}.temp", ccd.number),
                format!("CCD {}", ccd.number),
                SensorKind::Temperature,
            ));
        }
        if self.power.is_some() {
            sensors.push(spec(
                "cpu.package.power".into(),
                "Package power".into(),
                SensorKind::Power,
            ));
        }
        sensors
    }

    fn sample(&mut self, out: &mut Vec<Option<f64>>) {
        let temperature_values = 1 + usize::from(self.declares_tctl()) + self.ccds.len();
        if self.backoff.skip_this_sample() {
            out.extend(std::iter::repeat_n(None, temperature_values));
        } else {
            match self.read_temperatures() {
                Ok((tctl, ccds)) => {
                    self.backoff.record_success();
                    self.last_problem = None;
                    out.push(tctl.and_then(|tctl| self.tdie(tctl)));
                    if self.declares_tctl() {
                        out.push(tctl);
                    }
                    out.extend(ccds);
                }
                Err(error) => {
                    // Another program holding the bus is routine and says
                    // nothing about the processor; anything else backs off.
                    if !error.is_contended() {
                        self.backoff.record_failure();
                    }
                    self.last_problem = Some(error.to_string());
                    out.extend(std::iter::repeat_n(None, temperature_values));
                }
            }
        }

        // Power comes from an MSR, which needs no bus mutex and no backoff.
        if let Some(meter) = &mut self.power {
            let now = self.clock.now();
            let watts = self
                .msr
                .read(self.package_cpu, MSR_PKG_ENERGY_STATUS)
                .ok()
                .and_then(|counter| meter.watts(now, counter));
            out.push(watts);
        }
    }

    fn period_secs(&self) -> u32 {
        PERIOD_SECS
    }

    fn needs_driver(&self) -> bool {
        true
    }

    /// Why the most recent temperature read had no values, if it had none.
    fn last_problem(&self) -> Option<&str> {
        self.last_problem.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use super::*;
    use crate::bus::replay::{ReplayMsr, ReplaySmn};
    use crate::bus::{FakeBusLocks, is_permitted_smn_address};
    use crate::cpu::rapl::ManualClock;
    use crate::cpu::topology::Package;

    fn cpu(index: u8) -> LogicalCpu {
        LogicalCpu { group: 0, index }
    }

    fn identity(family: u32, model: u32, brand: &str) -> CpuIdentity {
        CpuIdentity { vendor: Vendor::Amd, family, model, stepping: 0, brand: brand.into() }
    }

    fn zen3() -> CpuIdentity {
        identity(0x19, 0x21, "AMD Ryzen 9 5950X 16-Core Processor")
    }

    fn topology() -> CpuTopology {
        CpuTopology { packages: vec![Package { cores: (0..4).map(|i| cpu(i * 2)).collect() }] }
    }

    /// `THM_TCON_CUR_TMP` reporting `degrees` (a multiple of 0.125) in the
    /// unshifted range.
    fn tctl_register(degrees: f64) -> u32 {
        ((degrees / DEGREES_PER_COUNT) as u32) << TEMPERATURE_SHIFT
    }

    /// A CCD register that is valid and reads `degrees` (above -49).
    fn ccd_register(degrees: f64) -> u32 {
        CCD_VALID | (((degrees + RANGE_SHIFT_DEGREES) / DEGREES_PER_COUNT) as u32)
    }

    /// Energy unit 2^-16 J (exponent 16, the usual Zen value), power unit
    /// exponent 3, time unit 10.
    const POWER_UNIT_2_16: u64 = 0x000A_1003;

    /// A Zen 3 replay: Tctl 55, two CCDs at 52 and 58, 2^-16 J energy unit.
    fn replay_smn() -> ReplaySmn {
        let mut smn = ReplaySmn::new();
        smn.set(SMN_REPORTED_TEMPERATURE, tctl_register(55.0));
        smn.set(0x0005_9954, ccd_register(52.0));
        smn.set(0x0005_9958, ccd_register(58.0));
        // The remaining registers of the scan exist and are not valid.
        for index in 2..8 {
            smn.set(0x0005_9954 + 4 * index, 0);
        }
        smn
    }

    fn replay_msr() -> ReplayMsr {
        let mut msr = ReplayMsr::new();
        msr.set(cpu(0), MSR_RAPL_POWER_UNIT, POWER_UNIT_2_16);
        msr.set(cpu(0), MSR_PKG_ENERGY_STATUS, 5);
        msr
    }

    struct Fixtures {
        provider: AmdCpuProvider,
        locks: Arc<FakeBusLocks>,
        clock: ManualClock,
    }

    fn provider_with(identity: &CpuIdentity, smn: ReplaySmn, msr: ReplayMsr) -> Fixtures {
        let locks = Arc::new(FakeBusLocks::free());
        let clock = ManualClock::new();
        let provider = AmdCpuProvider::probe(
            identity,
            &topology(),
            Box::new(smn),
            Box::new(msr),
            locks.clone(),
            Box::new(clock.clone()),
        )
        .unwrap();
        Fixtures { provider, locks, clock }
    }

    fn zen3_fixtures() -> Fixtures {
        provider_with(&zen3(), replay_smn(), replay_msr())
    }

    fn sampled(provider: &mut AmdCpuProvider) -> Vec<Option<f64>> {
        let mut out = Vec::new();
        provider.sample(&mut out);
        out
    }

    fn keys(provider: &AmdCpuProvider) -> Vec<String> {
        provider.sensors().into_iter().map(|spec| spec.key).collect()
    }

    // -- decoding -----------------------------------------------------------

    #[test]
    fn tctl_is_the_upper_eleven_bits_in_eighths_of_a_degree() {
        assert_eq!(tctl_celsius(0x3200_0000), Some(50.0));
        assert_eq!(tctl_celsius(tctl_register(42.375)), Some(42.375));
    }

    #[test]
    fn the_range_select_bit_shifts_tctl_down_by_49_degrees() {
        // 149 degrees raw with bit 19: 100 degrees.
        assert_eq!(tctl_celsius(tctl_register(149.0) | RANGE_SELECT), Some(100.0));
    }

    #[test]
    fn both_tj_select_bits_also_shift_tctl_but_one_alone_does_not() {
        let raw = tctl_register(120.0);
        assert_eq!(tctl_celsius(raw | (0b11 << 16)), Some(71.0));
        assert_eq!(tctl_celsius(raw | (0b01 << 16)), Some(120.0));
        assert_eq!(tctl_celsius(raw | (0b10 << 16)), Some(120.0));
    }

    #[test]
    fn a_register_of_all_ones_or_an_implausible_value_is_no_reading() {
        assert_eq!(tctl_celsius(u32::MAX), None);
        assert_eq!(tctl_celsius(tctl_register(200.0)), None);
        // Raw 3 degrees with the shift: -46 degrees.
        assert_eq!(tctl_celsius(tctl_register(3.0) | RANGE_SELECT), None);
    }

    #[test]
    fn a_ccd_temperature_is_eighths_of_a_degree_above_minus_49() {
        // 0x368 = 872 counts = 109 degrees raw, 60 degrees after the shift.
        assert_eq!(ccd_celsius(CCD_VALID | 0x368), Some(60.0));
        assert_eq!(ccd_celsius(ccd_register(33.125)), Some(33.125));
    }

    #[test]
    fn a_ccd_without_the_valid_bit_or_all_ones_is_no_reading() {
        assert_eq!(ccd_celsius(0x368), None, "valid bit clear");
        assert_eq!(ccd_celsius(0), None);
        assert_eq!(ccd_celsius(u32::MAX), None);
    }

    #[test]
    fn bits_above_the_ccd_field_are_ignored() {
        assert_eq!(ccd_celsius(0xABC0_0000 | ccd_register(60.0)), Some(60.0));
    }

    #[test]
    fn the_ccd_layouts_follow_the_k10temp_table() {
        assert_eq!(ccd_layout(0x17, 0x71), Some(CcdLayout { offset: 0x154, limit: 8 }));
        assert_eq!(ccd_layout(0x17, 0x01), Some(CcdLayout { offset: 0x154, limit: 4 }));
        assert_eq!(ccd_layout(0x19, 0x21), Some(CcdLayout { offset: 0x154, limit: 8 }));
        assert_eq!(ccd_layout(0x19, 0x61), Some(CcdLayout { offset: 0x308, limit: 8 }));
        assert_eq!(ccd_layout(0x19, 0x11), Some(CcdLayout { offset: 0x300, limit: 12 }));
        assert_eq!(ccd_layout(0x1A, 0x08), Some(CcdLayout { offset: 0x1F0, limit: 16 }));
        assert_eq!(ccd_layout(0x1A, 0x44), Some(CcdLayout { offset: 0x308, limit: 8 }));
        assert_eq!(ccd_layout(0x17, 0x02), None);
        assert_eq!(ccd_layout(0x1A, 0x20), None);
        assert_eq!(ccd_layout(0x16, 0x00), None);
    }

    #[test]
    fn every_ccd_register_of_every_layout_is_inside_the_permitted_smn_block() {
        for family in 0x17..=0x1A {
            for model in 0..=0xFF {
                let Some(layout) = ccd_layout(family, model) else { continue };
                for index in 0..layout.limit {
                    let address = SMN_REPORTED_TEMPERATURE + layout.offset + 4 * index;
                    assert!(
                        is_permitted_smn_address(address),
                        "(ccd layout): expected an address in the thermal block | received: \
                         {address:#x} for family {family:#x} model {model:#x}"
                    );
                }
            }
        }
    }

    #[test]
    fn only_the_listed_first_and_second_generation_models_have_a_tctl_offset() {
        let offset = |family, brand: &str| tctl_offset(&identity(family, 1, brand));
        assert_eq!(offset(0x17, "AMD Ryzen 7 1700X Eight-Core Processor"), 20.0);
        assert_eq!(offset(0x17, "AMD Ryzen 5 1600X Six-Core Processor"), 20.0);
        assert_eq!(offset(0x17, "AMD Ryzen 7 2700X Eight-Core Processor"), 10.0);
        assert_eq!(offset(0x17, "AMD Ryzen Threadripper 1950X 16-Core Processor"), 27.0);
        assert_eq!(offset(0x17, "AMD Ryzen Threadripper 2990WX 32-Core Processor"), 27.0);
        assert_eq!(offset(0x17, "AMD Ryzen 7 1700 Eight-Core Processor"), 0.0, "no X, no offset");
        assert_eq!(offset(0x17, "AMD Ryzen 7 3700X 8-Core Processor"), 0.0);
        assert_eq!(offset(0x19, "AMD Ryzen 7 1700X Eight-Core Processor"), 0.0, "family 17h only");
    }

    // -- probing ------------------------------------------------------------

    #[test]
    fn probe_finds_the_valid_ccds_and_the_package_power() {
        let Fixtures { provider, .. } = zen3_fixtures();
        assert_eq!(
            keys(&provider),
            ["cpu.package.temp", "cpu.ccd1.temp", "cpu.ccd2.temp", "cpu.package.power"]
        );
        assert_eq!(
            provider.summary(),
            "family 0x19 model 0x21 (replay-validated only): Tctl equals Tdie, 2 CCD(s); power: \
             package"
        );
    }

    #[test]
    fn labels_and_classes_follow_the_naming_scheme() {
        let Fixtures { provider, .. } = zen3_fixtures();
        let specs = provider.sensors();
        let pairs: Vec<_> = specs.iter().map(|s| (s.label.as_str(), s.kind)).collect();
        assert_eq!(
            pairs,
            [
                ("Package", SensorKind::Temperature),
                ("CCD 1", SensorKind::Temperature),
                ("CCD 2", SensorKind::Temperature),
                ("Package power", SensorKind::Power),
            ]
        );
        assert!(
            specs.iter().all(|s| s.device == "AMD Ryzen 9 5950X 16-Core Processor"
                && s.class == DeviceClass::Cpu)
        );
        assert_eq!(provider.period_secs(), 2);
        assert!(provider.needs_driver());
    }

    #[test]
    fn a_first_generation_part_declares_tctl_and_reports_tdie_as_the_package() {
        let brand = "AMD Ryzen 7 1700X Eight-Core Processor";
        let mut smn = ReplaySmn::new();
        smn.set(SMN_REPORTED_TEMPERATURE, tctl_register(65.0));
        for index in 0..4 {
            smn.set(0x0005_9954 + 4 * index, 0);
        }
        let Fixtures { mut provider, .. } =
            provider_with(&identity(0x17, 0x01, brand), smn, replay_msr());
        assert_eq!(keys(&provider)[..2], ["cpu.package.temp", "cpu.package.tctl"]);
        assert_eq!(sampled(&mut provider)[..2], [Some(45.0), Some(65.0)]);
        assert!(provider.summary().contains("Tdie is Tctl minus 20 °C"), "{}", provider.summary());
    }

    #[test]
    fn a_model_without_a_layout_reports_the_package_temperature_only_and_says_so() {
        let mut smn = ReplaySmn::new();
        smn.set(SMN_REPORTED_TEMPERATURE, tctl_register(50.0));
        let Fixtures { mut provider, .. } =
            provider_with(&identity(0x1A, 0x20, "AMD Test"), smn, replay_msr());
        assert_eq!(keys(&provider), ["cpu.package.temp", "cpu.package.power"]);
        assert_eq!(sampled(&mut provider)[0], Some(50.0));
        assert!(
            provider.summary().ends_with("not available: no CCD layout for family 0x1a model 0x20"),
            "{}",
            provider.summary()
        );
    }

    #[test]
    fn a_ccd_scan_that_hits_a_driver_error_keeps_the_ccds_found_so_far() {
        // Registers for CCD 1 only: the read of CCD 2 has no recording.
        let mut smn = ReplaySmn::new();
        smn.set(SMN_REPORTED_TEMPERATURE, tctl_register(55.0));
        smn.set(0x0005_9954, ccd_register(52.0));
        let Fixtures { provider, .. } = provider_with(&zen3(), smn, replay_msr());
        assert_eq!(keys(&provider)[..2], ["cpu.package.temp", "cpu.ccd1.temp"]);
        assert!(
            provider.summary().contains("CCD scan stopped at CCD 2: "),
            "{}",
            provider.summary()
        );
    }

    #[test]
    fn power_is_not_declared_for_a_zero_counter_or_a_missing_unit() {
        let mut zero = replay_msr();
        zero.set(cpu(0), MSR_PKG_ENERGY_STATUS, 0);
        let Fixtures { provider, .. } = provider_with(&zen3(), replay_smn(), zero);
        assert!(!keys(&provider).contains(&"cpu.package.power".to_owned()));
        assert!(provider.summary().ends_with("not available: package power (counter reads 0)"));

        let mut no_unit = replay_msr();
        no_unit.set(cpu(0), MSR_RAPL_POWER_UNIT, 0x000A_0003);
        let Fixtures { provider, .. } = provider_with(&zen3(), replay_smn(), no_unit);
        assert!(!keys(&provider).contains(&"cpu.package.power".to_owned()));
        assert!(provider.summary().contains("package power (no usable energy unit)"));
    }

    #[test]
    fn probe_refuses_other_vendors_families_empty_topologies_and_unreadable_temperatures() {
        let probe = |identity: &CpuIdentity, topology: &CpuTopology, smn: ReplaySmn| {
            AmdCpuProvider::probe(
                identity,
                topology,
                Box::new(smn),
                Box::new(replay_msr()),
                Arc::new(FakeBusLocks::free()),
                Box::new(ManualClock::new()),
            )
            .err()
        };
        let mut intel = zen3();
        intel.vendor = Vendor::Intel;
        assert_eq!(
            probe(&intel, &topology(), replay_smn()),
            Some(AmdProbeError::NotAmd { vendor: Vendor::Intel })
        );
        assert_eq!(
            probe(&identity(0x16, 0, "AMD Jaguar"), &topology(), replay_smn()),
            Some(AmdProbeError::UnsupportedFamily { family: 0x16 })
        );
        assert_eq!(
            probe(&identity(0x1B, 0, "AMD Future"), &topology(), replay_smn()),
            Some(AmdProbeError::UnsupportedFamily { family: 0x1B })
        );
        assert_eq!(
            probe(&zen3(), &CpuTopology { packages: vec![] }, replay_smn()),
            Some(AmdProbeError::NoCores)
        );
        let error = probe(&zen3(), &topology(), ReplaySmn::new());
        assert!(matches!(error, Some(AmdProbeError::NoTemperature { .. })), "{error:?}");
    }

    #[test]
    fn a_reported_temperature_of_all_ones_fails_the_probe_with_the_register_value() {
        let mut smn = ReplaySmn::new();
        smn.set(SMN_REPORTED_TEMPERATURE, u32::MAX);
        let error = AmdCpuProvider::probe(
            &zen3(),
            &topology(),
            Box::new(smn),
            Box::new(replay_msr()),
            Arc::new(FakeBusLocks::free()),
            Box::new(ManualClock::new()),
        )
        .err()
        .unwrap();
        assert_eq!(
            error.to_string(),
            "(amd cpu): expected a readable reported temperature (SMN 0x59800) | received: \
             (reported temperature): expected a plausible value (-10 to 150 degrees) | received: \
             register 0xffffffff"
        );
    }

    #[test]
    fn a_held_pci_bus_at_probe_time_is_reported_as_the_cause() {
        let error = AmdCpuProvider::probe(
            &zen3(),
            &topology(),
            Box::new(replay_smn()),
            Box::new(replay_msr()),
            Arc::new(FakeBusLocks::held_by_another_program(&[BusKind::Pci])),
            Box::new(ManualClock::new()),
        )
        .err();
        assert_eq!(
            error,
            Some(AmdProbeError::NoTemperature { cause: BusError::Contended(BusKind::Pci) })
        );
    }

    // -- sampling -----------------------------------------------------------

    #[test]
    fn a_sample_reports_tdie_the_ccds_and_takes_the_pci_mutex_once() {
        let Fixtures { mut provider, locks, .. } = zen3_fixtures();
        let probed = locks.acquired().len();
        let values = sampled(&mut provider);
        assert_eq!(values, [Some(55.0), Some(52.0), Some(58.0), None], "power needs two samples");
        assert_eq!(locks.acquired()[probed..], [BusKind::Pci]);
        assert_eq!(locks.live_guards(), 0);
    }

    #[test]
    fn power_is_energy_over_time_in_the_unit_the_register_names() {
        let mut msr = replay_msr();
        // 2^-16 J per count: 32 J in 2 s is 16 W. The first value is the probe's.
        msr.set_sequence(cpu(0), MSR_PKG_ENERGY_STATUS, &[5, 1000, 1000 + 32 * 65536]);
        let Fixtures { mut provider, clock, .. } = provider_with(&zen3(), replay_smn(), msr);
        assert_eq!(sampled(&mut provider)[3], None);
        clock.advance(Duration::from_secs(2));
        let watts = sampled(&mut provider)[3].unwrap();
        assert!((watts - 16.0).abs() < 1e-9, "{watts}");
    }

    #[test]
    fn a_held_pci_bus_skips_the_temperatures_but_not_the_power_and_does_not_back_off() {
        let mut msr = replay_msr();
        msr.set_sequence(cpu(0), MSR_PKG_ENERGY_STATUS, &[5, 1000, 1000 + 32 * 65536]);
        let Fixtures { mut provider, clock, .. } = provider_with(&zen3(), replay_smn(), msr);
        provider.locks = Arc::new(FakeBusLocks::held_by_another_program(&[BusKind::Pci]));
        assert_eq!(sampled(&mut provider), [None, None, None, None]);
        clock.advance(Duration::from_secs(2));
        let values = sampled(&mut provider);
        assert_eq!(values[..3], [None, None, None]);
        assert!(values[3].is_some(), "the MSR counter needs no bus mutex: {values:?}");
        assert_eq!(provider.backoff.failures(), 0);
        assert!(provider.last_problem().unwrap().contains("held by another program"));
    }

    /// An SMN that records every address and can be made to fail.
    struct ScriptedSmn {
        inner: ReplaySmn,
        reads: Arc<Mutex<Vec<u32>>>,
        failing: Arc<Mutex<bool>>,
    }

    impl Smn for ScriptedSmn {
        fn read(&mut self, address: u32) -> Result<u32, BusError> {
            self.reads.lock().unwrap().push(address);
            if *self.failing.lock().unwrap() {
                return Err(BusError::Driver(syspilot_pawnio::Error::Timeout {
                    function: "ioctl_read_smn".into(),
                }));
            }
            self.inner.read(address)
        }
    }

    /// A provider over a [`ScriptedSmn`] with handles to what it records.
    struct ScriptedProvider {
        provider: AmdCpuProvider,
        reads: Arc<Mutex<Vec<u32>>>,
        failing: Arc<Mutex<bool>>,
        locks: Arc<FakeBusLocks>,
    }

    fn scripted_provider() -> ScriptedProvider {
        let reads = Arc::new(Mutex::new(Vec::new()));
        let failing = Arc::new(Mutex::new(false));
        let locks = Arc::new(FakeBusLocks::free());
        let provider = AmdCpuProvider::probe(
            &zen3(),
            &topology(),
            Box::new(ScriptedSmn {
                inner: replay_smn(),
                reads: reads.clone(),
                failing: failing.clone(),
            }),
            Box::new(replay_msr()),
            locks.clone(),
            Box::new(ManualClock::new()),
        )
        .unwrap();
        ScriptedProvider { provider, reads, failing, locks }
    }

    #[test]
    fn a_bus_fault_gives_none_releases_the_mutex_backs_off_and_recovers() {
        let ScriptedProvider { mut provider, reads, failing, locks } = scripted_provider();
        *failing.lock().unwrap() = true;
        reads.lock().unwrap().clear();
        let values = sampled(&mut provider);
        assert_eq!(values[..3], [None, None, None]);
        assert_eq!(reads.lock().unwrap().len(), 1, "the first failed read ends the sample");
        assert_eq!(locks.live_guards(), 0, "the mutex is released on the error path");
        assert!(provider.last_problem().unwrap().contains("ioctl_read_smn"));

        // The next sample is skipped without touching the bus.
        reads.lock().unwrap().clear();
        let acquired = locks.acquired().len();
        assert_eq!(sampled(&mut provider)[..3], [None, None, None]);
        assert!(reads.lock().unwrap().is_empty());
        assert_eq!(locks.acquired().len(), acquired);

        // The bus recovers; the pause ends and values come back.
        *failing.lock().unwrap() = false;
        let values = sampled(&mut provider);
        assert_eq!(values[..3], [Some(55.0), Some(52.0), Some(58.0)]);
        assert_eq!(provider.backoff.failures(), 0);
        assert_eq!(provider.last_problem(), None);
    }

    #[test]
    fn only_thermal_registers_and_the_rapl_msrs_are_read() {
        /// The part of AMDFamily17's MSR read allow-list this provider uses.
        const ALLOWED_MSRS: [u32; 2] = [0xC001_0299, 0xC001_029B];
        let ScriptedProvider { mut provider, reads, .. } = scripted_provider();
        for _ in 0..3 {
            sampled(&mut provider);
        }
        let reads = reads.lock().unwrap();
        assert!(!reads.is_empty());
        for address in reads.iter() {
            assert!(
                is_permitted_smn_address(*address),
                "(amd smn): expected an address in the thermal block | received: {address:#x}"
            );
        }

        let msr = replay_msr();
        let log = msr.read_log();
        let Fixtures { mut provider, .. } = provider_with(&zen3(), replay_smn(), msr);
        sampled(&mut provider);
        for (reader, register) in log.lock().unwrap().iter() {
            assert!(
                ALLOWED_MSRS.contains(register),
                "(amd msr): expected an allow-listed register | received: {register:#x} on {reader:?}"
            );
            assert_eq!(*reader, cpu(0), "package-scope registers are read on the first core");
        }
    }

    #[test]
    fn a_sample_has_exactly_one_value_per_sensor_in_every_state() {
        let Fixtures { mut provider, .. } = zen3_fixtures();
        assert_eq!(sampled(&mut provider).len(), provider.sensors().len());

        let ScriptedProvider { provider: mut failing_provider, failing, .. } = scripted_provider();
        *failing.lock().unwrap() = true;
        let declared = failing_provider.sensors().len();
        for _ in 0..3 {
            assert_eq!(sampled(&mut failing_provider).len(), declared);
        }
    }
}
