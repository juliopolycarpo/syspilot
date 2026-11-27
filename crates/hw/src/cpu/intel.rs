//! Intel Core and Xeon: package and per-core temperatures, RAPL power.
//!
//! Register semantics are from the Intel SDM (volume 4, model-specific
//! registers) and cross-checked on a Broadwell-EP machine; see
//! `docs/research/reference-machine.md`.
//!
//! | MSR | Meaning | Used as |
//! |---|---|---|
//! | `0x1A2` `IA32_TEMPERATURE_TARGET` | bits 23:16 = TjMax in °C | temperature = TjMax - readout |
//! | `0x19C` `IA32_THERM_STATUS` (per core) | bit 31 = reading valid, bits 22:16 = digital readout (degrees below TjMax) | core temperature |
//! | `0x1B1` `IA32_PACKAGE_THERM_STATUS` | same layout, package scope | package temperature |
//! | `0x606` `MSR_RAPL_POWER_UNIT` | bits 3:0 = power unit, 1/2^n W; bits 12:8 = energy unit, 1/2^n J | energy per count |
//! | `0x611` / `0x639` / `0x619` | package / cores (PP0) / DRAM energy, 32-bit wrapping counters | power = delta * unit / time |
//! | `0x614` `MSR_PKG_POWER_INFO` | bits 14:0 = thermal spec power (TDP), in power units | floor for the plausibility check below |
//!
//! # An untrustworthy package meter
//!
//! The package counter of the Xeon E5-2680 v4 on the reference machine runs
//! at the right rate (one update per millisecond, in the unit the unit
//! register declares) and follows load, but reads 0.8 W idle and 2.6 W with
//! every core busy for a 120 W part. The cause is in the board's firmware or
//! the part, not in this code (see `docs/research/reference-machine.md`). A
//! meter like that would mislead, so on the models whose datasheet lists
//! package C-state power ([`idle_power_floor_watts`]) the probe watches the
//! counter for a moment and does not offer package power when it reads below
//! the floor.

use std::fmt::Write as _;
use std::time::Duration;

use syspilot_proto::sensor::{DeviceClass, SensorKind};

use super::identity::{CpuIdentity, Vendor};
use super::rapl::{Clock, EnergyMeter};
use super::topology::CpuTopology;
use crate::bus::{BusError, LogicalCpu, Msr};
use crate::hub::{Provider, SensorSpec};

const MSR_THERM_STATUS: u32 = 0x19C;
const MSR_TEMPERATURE_TARGET: u32 = 0x1A2;
const MSR_PACKAGE_THERM_STATUS: u32 = 0x1B1;
const MSR_RAPL_POWER_UNIT: u32 = 0x606;
const MSR_PKG_POWER_INFO: u32 = 0x614;
const MSR_PKG_ENERGY_STATUS: u32 = 0x611;
const MSR_DRAM_ENERGY_STATUS: u32 = 0x619;
const MSR_PP0_ENERGY_STATUS: u32 = 0x639;

/// Used when `IA32_TEMPERATURE_TARGET` cannot be read or holds nonsense.
/// 100 °C is the TjMax of most Core and Xeon parts.
const FALLBACK_TJMAX: f64 = 100.0;
/// Range of TjMax values taken at face value.
const PLAUSIBLE_TJMAX: std::ops::RangeInclusive<u64> = 70..=125;
/// The energy counters are 32 bits wide; the upper bits are reserved.
const COUNTER_MASK: u64 = 0xFFFF_FFFF;
/// Digital readout valid bit in `IA32_THERM_STATUS`.
const READING_VALID: u64 = 1 << 31;
/// Temperatures outside this range are not a measurement.
const PLAUSIBLE_TEMPERATURE: std::ops::RangeInclusive<f64> = -10.0..=150.0;
/// Seconds between samples.
const PERIOD_SECS: u32 = 2;
/// How long the probe watches the package counter. At least the energy
/// meter's minimum interval; at the floor of a 120 W part it counts about
/// 25 000 times in this window, so quantisation does not matter.
const PLAUSIBILITY_WINDOW: Duration = Duration::from_millis(250);
/// The share of its TDP that a processor on a model covered by
/// [`idle_power_floor_watts`] is assumed to draw at least.
///
/// The datasheet's package C6 figures (the deepest idle state) are 12 to 14 W
/// for Xeon E5 v3 and v4 parts of 120 to 160 W, at least 8.1 % of the TDP
/// (the E5-2683 v3 row is 20 W, 16.7 %), and they are maximums. 5 % is below
/// the lowest of them and well above the 0.7 % the reference machine's meter
/// reads at idle. Typical package C6 power on a healthy part was not
/// measured, so the margin for a healthy meter is an expectation.
const IDLE_POWER_FLOOR_OF_TDP: f64 = 0.05;

/// Where the DRAM energy counter's unit comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DramUnit {
    /// The unit in `MSR_RAPL_POWER_UNIT`, like the package counter.
    PackageUnit,
    /// A fixed 2^-16 J (15.3 µJ): Haswell-EP and later server parts ignore
    /// the unit register for the DRAM domain.
    Fixed,
    /// The DRAM domain is not declared on this model.
    Absent,
}

/// What RAPL offers on a processor model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RaplDomains {
    /// The cores (PP0) domain is meaningful.
    pub cores: bool,
    /// The DRAM domain and its unit.
    pub dram: DramUnit,
}

/// RAPL capabilities by Intel family 6 model number. Models not listed get
/// the package domain only: a wrong unit would be worse than no sensor.
pub fn rapl_domains(model: u32) -> RaplDomains {
    match model {
        // Sandy Bridge(-EP), Ivy Bridge(-EP), Haswell, Broadwell, Skylake
        // and later client cores, all measured in the package unit.
        0x2A | 0x2D | 0x3A | 0x3E | 0x3C | 0x45 | 0x46 | 0x3D | 0x47 | 0x4E | 0x5E | 0x8E
        | 0x9E | 0xA5 | 0xA6 | 0x7D | 0x7E | 0x9D | 0xA7 | 0x8C | 0x8D | 0x97 | 0x9A | 0xB7
        | 0xBA | 0xBF | 0xAA | 0xAC | 0xC5 | 0xC6 | 0xBD => {
            RaplDomains { cores: true, dram: DramUnit::PackageUnit }
        }
        // Haswell-EP, Broadwell-EP, Broadwell-DE, Skylake-SP, Knights
        // Landing and Mill: no PP0 domain, DRAM in a fixed unit.
        0x3F | 0x4F | 0x56 | 0x55 | 0x57 | 0x85 => {
            RaplDomains { cores: false, dram: DramUnit::Fixed }
        }
        _ => RaplDomains { cores: false, dram: DramUnit::Absent },
    }
}

/// Why no Intel provider was created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntelProbeError {
    /// The processor is not Intel.
    NotIntel {
        /// What it is.
        vendor: Vendor,
    },
    /// The OS reported no cores.
    NoCores,
    /// No thermal register could be read.
    NoThermalRegisters {
        /// The error of the package register read.
        first_error: BusError,
    },
}

impl std::fmt::Display for IntelProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotIntel { vendor } => {
                write!(f, "(intel cpu): expected an Intel processor | received: {vendor:?}")
            }
            Self::NoCores => {
                write!(
                    f,
                    "(intel cpu): expected at least one core in the topology | received: none"
                )
            }
            Self::NoThermalRegisters { first_error } => write!(
                f,
                "(intel cpu): expected a readable thermal status register | received: {first_error}"
            ),
        }
    }
}

impl std::error::Error for IntelProbeError {}

struct Core {
    cpu: LogicalCpu,
    tjmax: f64,
}

struct PowerDomain {
    key: &'static str,
    label: &'static str,
    register: u32,
    meter: EnergyMeter,
}

/// Temperatures and power of one Intel processor package.
pub struct IntelCpuProvider {
    device: String,
    msr: Box<dyn Msr>,
    clock: Box<dyn Clock>,
    cores: Vec<Core>,
    package_tjmax: f64,
    /// Package-scope registers are read on the first core.
    package_cpu: LogicalCpu,
    domains: Vec<PowerDomain>,
    summary: String,
}

/// TjMax from `IA32_TEMPERATURE_TARGET`, or the fallback.
fn tjmax_from(raw: Result<u64, BusError>) -> f64 {
    match raw {
        Ok(value) if PLAUSIBLE_TJMAX.contains(&((value >> 16) & 0xFF)) => {
            ((value >> 16) & 0xFF) as f64
        }
        _ => FALLBACK_TJMAX,
    }
}

/// Degrees below TjMax from a thermal status register, when its valid bit
/// is set.
fn readout(status: u64) -> Option<f64> {
    (status & READING_VALID != 0).then_some(((status >> 16) & 0x7F) as f64)
}

impl IntelCpuProvider {
    /// Probes the first package of `topology`.
    ///
    /// Reads TjMax per core, the RAPL unit register and each domain's
    /// counter once to decide which sensors exist. On models covered by
    /// [`idle_power_floor_watts`] it also watches the package counter for
    /// `PLAUSIBILITY_WINDOW` (250 ms spent in [`Clock::wait`]) and does not
    /// declare package power when it reads below the floor. Energy baselines
    /// are taken by the first [`Provider::sample`], which therefore reports
    /// no power.
    pub fn probe(
        identity: &CpuIdentity,
        topology: &CpuTopology,
        mut msr: Box<dyn Msr>,
        clock: Box<dyn Clock>,
    ) -> Result<Self, IntelProbeError> {
        if identity.vendor != Vendor::Intel {
            return Err(IntelProbeError::NotIntel { vendor: identity.vendor });
        }
        let Some(package) = topology.packages.first().filter(|package| !package.cores.is_empty())
        else {
            return Err(IntelProbeError::NoCores);
        };
        let package_cpu = package.cores[0];
        if let Err(first_error) = msr.read(package_cpu, MSR_PACKAGE_THERM_STATUS) {
            // A part without the package register still has per-core
            // status; only give up when neither can be read.
            if let Err(core_error) = msr.read(package_cpu, MSR_THERM_STATUS) {
                return Err(IntelProbeError::NoThermalRegisters {
                    first_error: if first_error == core_error { first_error } else { core_error },
                });
            }
        }

        let cores: Vec<Core> = package
            .cores
            .iter()
            .map(|&cpu| Core { cpu, tjmax: tjmax_from(msr.read(cpu, MSR_TEMPERATURE_TARGET)) })
            .collect();
        let package_tjmax = cores[0].tjmax;

        let capabilities = rapl_domains(identity.model);
        let mut domains = Vec::new();
        let mut summary = format!(
            "{} cores, package and per-core temperature (TjMax {package_tjmax} °C)",
            cores.len()
        );
        let unit_register = msr.read(package_cpu, MSR_RAPL_POWER_UNIT).ok();
        if let Some(unit) = unit_register.and_then(energy_unit) {
            let mut power =
                vec![("cpu.package.power", "Package power", MSR_PKG_ENERGY_STATUS, unit)];
            if capabilities.cores {
                power.push(("cpu.cores.power", "Cores power", MSR_PP0_ENERGY_STATUS, unit));
            }
            match capabilities.dram {
                DramUnit::PackageUnit => {
                    power.push(("cpu.dram.power", "DRAM power", MSR_DRAM_ENERGY_STATUS, unit));
                }
                DramUnit::Fixed => {
                    power.push((
                        "cpu.dram.power",
                        "DRAM power",
                        MSR_DRAM_ENERGY_STATUS,
                        1.0 / 65536.0,
                    ));
                }
                DramUnit::Absent => {}
            }
            let mut found = Vec::new();
            let mut skipped = Vec::new();
            for (key, label, register, joules_per_count) in power {
                let name = label.trim_end_matches(" power").to_lowercase();
                match msr.read(package_cpu, register) {
                    // A live counter is never exactly zero: it has counted
                    // since power-on. Zero means the firmware does not
                    // enable the domain (the DRAM domain on the reference
                    // machine), and a constant 0 W would mislead.
                    Ok(counter) if counter & COUNTER_MASK != 0 => {
                        if register == MSR_PKG_ENERGY_STATUS
                            && let Some(reason) = untrustworthy_package_meter(
                                identity.model,
                                unit_register,
                                msr.as_mut(),
                                clock.as_ref(),
                                package_cpu,
                                (counter, joules_per_count),
                            )
                        {
                            skipped.push(format!("{name} ({reason})"));
                            continue;
                        }
                        found.push(name);
                        domains.push(PowerDomain {
                            key,
                            label,
                            register,
                            meter: EnergyMeter::new(joules_per_count),
                        });
                    }
                    Ok(_) => skipped.push(format!("{name} (counter reads 0)")),
                    Err(_) => skipped.push(format!("{name} (unreadable)")),
                }
            }
            if !found.is_empty() {
                let _ = write!(summary, "; power: {}", found.join(", "));
            }
            if !skipped.is_empty() {
                let _ = write!(summary, "; not available: {}", skipped.join(", "));
            }
        }

        Ok(Self {
            device: identity.brand.clone(),
            msr,
            clock,
            cores,
            package_tjmax,
            package_cpu,
            domains,
            summary,
        })
    }

    /// One-line description of what was found, for the feature detail.
    pub fn summary(&self) -> &str {
        &self.summary
    }
}

/// Joules per count of the package energy counter, from bits 12:8 of
/// `MSR_RAPL_POWER_UNIT`. `None` when its energy field is zero or
/// implausibly large.
fn energy_unit(unit_register: u64) -> Option<f64> {
    let exponent = (unit_register >> 8) & 0x1F;
    (1..=24).contains(&exponent).then(|| 1.0 / (1u64 << exponent) as f64)
}

/// The thermal spec power in watts: `MSR_PKG_POWER_INFO` bits 14:0, counted
/// in the power unit of bits 3:0 of `MSR_RAPL_POWER_UNIT`.
fn tdp_watts(power_info: u64, unit_register: u64) -> f64 {
    (power_info & 0x7FFF) as f64 / (1u64 << (unit_register & 0xF)) as f64
}

/// Whether [`idle_power_floor_watts`] has a rule for `model`.
fn has_idle_power_floor(model: u32) -> bool {
    matches!(model, 0x3F | 0x4F)
}

/// The package power below which a reading on `model` is not believed, given
/// the part's TDP; `None` for models without a source for it.
///
/// The sources are the E5-2600 v3 and v4 datasheets (volume 1, section 2.10,
/// package C-state power), which cover family 6 models `0x3F` (Haswell-EP) and
/// `0x4F` (Broadwell-EP). No source was consulted for the other models, and
/// client parts in deep package C-states may idle well below 5 % of their
/// TDP, so a rule for them needs its own source.
pub fn idle_power_floor_watts(model: u32, tdp_watts: f64) -> Option<f64> {
    has_idle_power_floor(model).then_some(tdp_watts * IDLE_POWER_FLOOR_OF_TDP)
}

/// Why the package energy counter of `model` is not to be believed, or
/// `None` when it is, or when there is no way to tell.
///
/// Watches the counter for [`PLAUSIBILITY_WINDOW`], starting from the
/// `(counter, joules_per_count)` already read, and compares the average
/// power with [`idle_power_floor_watts`].
fn untrustworthy_package_meter(
    model: u32,
    unit_register: Option<u64>,
    msr: &mut dyn Msr,
    clock: &dyn Clock,
    cpu: LogicalCpu,
    (first_counter, joules_per_count): (u64, f64),
) -> Option<String> {
    // Skip the TDP read where there is no rule to apply it to.
    if !has_idle_power_floor(model) {
        return None;
    }
    let unit_register = unit_register?;
    let tdp = tdp_watts(msr.read(cpu, MSR_PKG_POWER_INFO).ok()?, unit_register);
    let floor = idle_power_floor_watts(model, tdp)?;

    let mut meter = EnergyMeter::new(joules_per_count);
    meter.watts(clock.now(), first_counter);
    clock.wait(PLAUSIBILITY_WINDOW);
    let counter = msr.read(cpu, MSR_PKG_ENERGY_STATUS).ok()?;
    let watts = meter.watts(clock.now(), counter)?;
    (watts < floor)
        .then(|| format!("reads {watts:.2} W, below the {floor:.1} W floor for a {tdp:.0} W part"))
}

impl Provider for IntelCpuProvider {
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
        for index in 0..self.cores.len() {
            sensors.push(spec(
                format!("cpu.core{index}.temp"),
                format!("Core {index}"),
                SensorKind::Temperature,
            ));
        }
        sensors.push(spec(
            "cpu.core-max.temp".into(),
            "Hottest core".into(),
            SensorKind::Temperature,
        ));
        for domain in &self.domains {
            sensors.push(spec(domain.key.into(), domain.label.into(), SensorKind::Power));
        }
        sensors
    }

    fn sample(&mut self, out: &mut Vec<Option<f64>>) {
        let plausible =
            |temperature: f64| PLAUSIBLE_TEMPERATURE.contains(&temperature).then_some(temperature);

        let package = self
            .msr
            .read(self.package_cpu, MSR_PACKAGE_THERM_STATUS)
            .ok()
            .and_then(readout)
            .and_then(|degrees_below| plausible(self.package_tjmax - degrees_below));
        out.push(package);

        let mut hottest: Option<f64> = None;
        for core in &self.cores {
            let temperature = self
                .msr
                .read(core.cpu, MSR_THERM_STATUS)
                .ok()
                .and_then(readout)
                .and_then(|degrees_below| plausible(core.tjmax - degrees_below));
            hottest = match (hottest, temperature) {
                (Some(best), Some(this)) => Some(best.max(this)),
                (best, this) => best.or(this),
            };
            out.push(temperature);
        }
        out.push(hottest);

        let now = self.clock.now();
        for domain in &mut self.domains {
            let watts = self
                .msr
                .read(self.package_cpu, domain.register)
                .ok()
                .and_then(|counter| domain.meter.watts(now, counter));
            out.push(watts);
        }
    }

    fn period_secs(&self) -> u32 {
        PERIOD_SECS
    }

    fn needs_driver(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::replay::ReplayMsr;
    use crate::cpu::rapl::ManualClock;
    use crate::cpu::topology::Package;
    use std::time::Duration;

    fn cpu(index: u8) -> LogicalCpu {
        LogicalCpu { group: 0, index }
    }

    fn identity(model: u32) -> CpuIdentity {
        CpuIdentity {
            vendor: Vendor::Intel,
            family: 6,
            model,
            stepping: 1,
            brand: "Intel(R) Test CPU".into(),
        }
    }

    fn topology(cores: u8) -> CpuTopology {
        CpuTopology { packages: vec![Package { cores: (0..cores).map(|i| cpu(i * 2)).collect() }] }
    }

    /// `IA32_THERM_STATUS` with a valid reading `below` degrees under TjMax.
    fn status(below: u64) -> u64 {
        READING_VALID | (below << 16)
    }

    /// A broadwell-EP-like replay: TjMax 85, energy unit 2^-14, two cores.
    fn replay(cores: u8) -> ReplayMsr {
        let mut msr = ReplayMsr::new();
        msr.set(cpu(0), MSR_PACKAGE_THERM_STATUS, status(40));
        msr.set(cpu(0), MSR_RAPL_POWER_UNIT, 0x000A_0E03);
        for index in 0..cores {
            msr.set(cpu(index * 2), MSR_TEMPERATURE_TARGET, 0x0055_0000);
            msr.set(cpu(index * 2), MSR_THERM_STATUS, status(30 + u64::from(index)));
        }
        // Live counters: nonzero, as every counter that has run since power-on is.
        msr.set(cpu(0), MSR_PKG_ENERGY_STATUS, 7);
        msr.set(cpu(0), MSR_DRAM_ENERGY_STATUS, 7);
        msr.set(cpu(0), MSR_PP0_ENERGY_STATUS, 7);
        msr
    }

    fn provider(model: u32, msr: ReplayMsr) -> (IntelCpuProvider, ManualClock) {
        let clock = ManualClock::new();
        let provider = IntelCpuProvider::probe(
            &identity(model),
            &topology(2),
            Box::new(msr),
            Box::new(clock.clone()),
        )
        .unwrap();
        (provider, clock)
    }

    fn sample(provider: &mut IntelCpuProvider) -> Vec<Option<f64>> {
        let mut out = Vec::new();
        provider.sample(&mut out);
        out
    }

    fn keys(provider: &IntelCpuProvider) -> Vec<String> {
        provider.sensors().into_iter().map(|spec| spec.key).collect()
    }

    #[test]
    fn temperatures_are_tjmax_minus_the_digital_readout() {
        let (mut provider, _) = provider(0x4F, replay(2));
        // Package 85 - 40, cores 85 - 30 and 85 - 31, hottest core 55.
        assert_eq!(&sample(&mut provider)[..4], [Some(45.0), Some(55.0), Some(54.0), Some(55.0)]);
    }

    #[test]
    fn a_reading_without_the_valid_bit_is_none_and_skipped_by_the_maximum() {
        let mut msr = replay(2);
        msr.set(cpu(0), MSR_THERM_STATUS, 30 << 16); // valid bit clear
        let (mut provider, _) = provider(0x4F, msr);
        assert_eq!(&sample(&mut provider)[..4], [Some(45.0), None, Some(54.0), Some(54.0)]);
    }

    #[test]
    fn no_valid_core_gives_no_maximum() {
        let mut msr = replay(2);
        msr.set(cpu(0), MSR_THERM_STATUS, 0);
        msr.set(cpu(2), MSR_THERM_STATUS, 0);
        let (mut provider, _) = provider(0x4F, msr);
        assert_eq!(&sample(&mut provider)[1..4], [None, None, None]);
    }

    #[test]
    fn an_unreadable_core_register_is_none_not_a_failure() {
        let mut msr = replay(2);
        msr.remove(cpu(2), MSR_THERM_STATUS);
        let (mut provider, _) = provider(0x4F, msr);
        assert_eq!(&sample(&mut provider)[1..4], [Some(55.0), None, Some(55.0)]);
    }

    #[test]
    fn tjmax_falls_back_to_100_when_unreadable_or_implausible() {
        for target in [None, Some(0u64), Some(0x00FF_0000), Some(0x0005_0000)] {
            let mut msr = replay(1);
            match target {
                Some(value) => msr.set(cpu(0), MSR_TEMPERATURE_TARGET, value),
                None => msr.remove(cpu(0), MSR_TEMPERATURE_TARGET),
            };
            let clock = ManualClock::new();
            let mut provider = IntelCpuProvider::probe(
                &identity(0x4F),
                &topology(1),
                Box::new(msr),
                Box::new(clock),
            )
            .unwrap();
            // 100 - 30 degrees below TjMax.
            assert_eq!(sample(&mut provider)[1], Some(70.0), "target {target:?}");
        }
    }

    #[test]
    fn power_needs_two_samples_and_uses_the_energy_unit() {
        let mut msr = replay(2);
        // Package: 2^-14 J per count; 32 J in 2 s is 16 W. The first value
        // is consumed by the probe's existence check of the counter.
        msr.set_sequence(cpu(0), MSR_PKG_ENERGY_STATUS, &[7, 1000, 1000 + 32 * 16384]);
        let (mut provider, clock) = provider(0x4F, msr);
        let first = sample(&mut provider);
        assert_eq!(first[4], None, "no baseline yet");
        clock.advance(Duration::from_secs(2));
        let second = sample(&mut provider);
        let watts = second[4].unwrap();
        assert!((watts - 16.0).abs() < 1e-9, "{watts}");
    }

    #[test]
    fn the_dram_domain_of_a_server_part_uses_the_fixed_unit() {
        let mut msr = replay(2);
        // 2^-16 J per count: 20 W for 2 s is 40 J = 40 * 65536 counts.
        msr.set_sequence(cpu(0), MSR_DRAM_ENERGY_STATUS, &[7, 0, 40 * 65536]);
        let (mut provider, clock) = provider(0x4F, msr);
        sample(&mut provider);
        clock.advance(Duration::from_secs(2));
        let values = sample(&mut provider);
        let dram_at = keys(&provider).iter().position(|key| key == "cpu.dram.power").unwrap();
        let watts = values[dram_at].unwrap();
        assert!((watts - 20.0).abs() < 1e-9, "{watts}");
    }

    #[test]
    fn the_dram_domain_of_a_client_part_uses_the_package_unit() {
        let mut msr = replay(2);
        // Skylake client: DRAM in the package unit 2^-14 J: 10 W for 2 s.
        msr.set_sequence(cpu(0), MSR_DRAM_ENERGY_STATUS, &[7, 0, 20 * 16384]);
        let (mut provider, clock) = provider(0x5E, msr);
        sample(&mut provider);
        clock.advance(Duration::from_secs(2));
        let values = sample(&mut provider);
        let dram_at = keys(&provider).iter().position(|key| key == "cpu.dram.power").unwrap();
        assert!((values[dram_at].unwrap() - 10.0).abs() < 1e-9);
    }

    #[test]
    fn the_declared_power_domains_follow_the_model() {
        let (server, _) = provider(0x4F, replay(2));
        // Skip the four temperature sensors of a two-core provider.
        assert_eq!(
            keys(&server)[4..],
            ["cpu.package.power", "cpu.dram.power"],
            "server parts have no PP0 domain"
        );
        let (client, _) = provider(0x5E, replay(2));
        assert_eq!(keys(&client)[4..], ["cpu.package.power", "cpu.cores.power", "cpu.dram.power"]);
        let (unknown, _) = provider(0x99, replay(2));
        assert_eq!(keys(&unknown)[4..], ["cpu.package.power"]);
    }

    #[test]
    fn a_domain_whose_counter_cannot_be_read_is_not_declared() {
        let mut msr = ReplayMsr::new();
        msr.set(cpu(0), MSR_PACKAGE_THERM_STATUS, status(40));
        msr.set(cpu(0), MSR_THERM_STATUS, status(30));
        msr.set(cpu(0), MSR_TEMPERATURE_TARGET, 0x0055_0000);
        msr.set(cpu(0), MSR_RAPL_POWER_UNIT, 0x000A_0E03);
        msr.set(cpu(0), MSR_PKG_ENERGY_STATUS, 5);
        let clock = ManualClock::new();
        let provider =
            IntelCpuProvider::probe(&identity(0x4F), &topology(1), Box::new(msr), Box::new(clock))
                .unwrap();
        assert_eq!(keys(&provider).last().unwrap(), "cpu.package.power");
    }

    #[test]
    fn a_domain_whose_counter_reads_zero_is_not_declared_and_the_summary_says_why() {
        // The reference machine: the DRAM counter reads 0 and never moves.
        let mut msr = replay(2);
        msr.set(cpu(0), MSR_DRAM_ENERGY_STATUS, 0);
        let (provider, _) = provider(0x4F, msr);
        assert_eq!(keys(&provider).last().unwrap(), "cpu.package.power");
        assert_eq!(
            provider.summary(),
            "2 cores, package and per-core temperature (TjMax 85 °C); power: package; not \
             available: dram (counter reads 0)"
        );
    }

    #[test]
    fn a_zero_counter_in_the_reserved_upper_bits_still_counts_as_zero() {
        let mut msr = replay(2);
        msr.set(cpu(0), MSR_DRAM_ENERGY_STATUS, 0xABCD_0000_0000_0000);
        let (provider, _) = provider(0x4F, msr);
        assert!(!keys(&provider).contains(&"cpu.dram.power".to_owned()));
    }

    #[test]
    fn no_power_sensors_without_a_usable_energy_unit() {
        let mut msr = replay(2);
        msr.set(cpu(0), MSR_RAPL_POWER_UNIT, 0x0000_0003); // energy exponent 0
        let (provider, _) = provider(0x4F, msr);
        assert!(
            keys(&provider).iter().all(|key| !key.ends_with(".power")),
            "{:?}",
            keys(&provider)
        );
    }

    #[test]
    fn labels_and_keys_follow_the_naming_scheme() {
        let (provider, _) = provider(0x4F, replay(2));
        let specs = provider.sensors();
        let pairs: Vec<_> = specs.iter().map(|s| (s.key.as_str(), s.label.as_str())).collect();
        assert_eq!(
            pairs,
            [
                ("cpu.package.temp", "Package"),
                ("cpu.core0.temp", "Core 0"),
                ("cpu.core1.temp", "Core 1"),
                ("cpu.core-max.temp", "Hottest core"),
                ("cpu.package.power", "Package power"),
                ("cpu.dram.power", "DRAM power"),
            ]
        );
        assert!(
            specs.iter().all(|s| s.device == "Intel(R) Test CPU" && s.class == DeviceClass::Cpu)
        );
        assert!(specs[..4].iter().all(|s| s.kind == SensorKind::Temperature));
        assert!(specs[4..].iter().all(|s| s.kind == SensorKind::Power));
    }

    #[test]
    fn a_sample_has_exactly_one_value_per_sensor() {
        let (mut provider, _) = provider(0x4F, replay(2));
        assert_eq!(sample(&mut provider).len(), provider.sensors().len());
    }

    #[test]
    fn the_provider_declares_its_period_and_driver_need() {
        let (provider, _) = provider(0x4F, replay(2));
        assert_eq!(provider.period_secs(), 2);
        assert!(provider.needs_driver());
    }

    #[test]
    fn only_registers_on_the_module_read_allow_list_are_read() {
        /// The part of IntelMSR's read allow-list this provider uses.
        const ALLOWED: [u32; 8] = [0x19C, 0x1A2, 0x1B1, 0x606, 0x611, 0x614, 0x619, 0x639];
        // A model with a power floor, so that the TDP register is read too.
        let msr = metered(&[1_000, 1_000 + WINDOW_COUNTS_40_W], Some(POWER_INFO_120_W));
        let log = msr.read_log();
        let (mut provider, clock) = provider(0x4F, msr);
        for _ in 0..3 {
            sample(&mut provider);
            clock.advance(Duration::from_secs(2));
        }
        let reads = log.lock().unwrap();
        assert!(!reads.is_empty());
        for (cpu, register) in reads.iter() {
            assert!(
                ALLOWED.contains(register),
                "(intel msr): expected an allow-listed register | received: {register:#x} on {cpu:?}"
            );
        }
    }

    #[test]
    fn core_registers_are_read_on_each_cores_first_thread_and_package_registers_on_the_first() {
        let msr = replay(2);
        let log = msr.read_log();
        let (mut provider, _) = provider(0x4F, msr);
        log.lock().unwrap().clear();
        sample(&mut provider);
        let reads = log.lock().unwrap().clone();
        assert!(reads.contains(&(cpu(0), MSR_THERM_STATUS)));
        assert!(reads.contains(&(cpu(2), MSR_THERM_STATUS)));
        for (reader, register) in reads {
            if register != MSR_THERM_STATUS {
                assert_eq!(reader, cpu(0), "register {register:#x} is package scope");
            }
        }
    }

    #[test]
    fn probe_refuses_other_vendors_empty_topologies_and_unreadable_thermals() {
        let clock = || Box::new(ManualClock::new());
        let mut amd = identity(0x4F);
        amd.vendor = Vendor::Amd;
        let error = IntelCpuProvider::probe(&amd, &topology(1), Box::new(replay(1)), clock()).err();
        assert_eq!(error, Some(IntelProbeError::NotIntel { vendor: Vendor::Amd }));

        let empty = CpuTopology { packages: vec![] };
        let error =
            IntelCpuProvider::probe(&identity(0x4F), &empty, Box::new(replay(1)), clock()).err();
        assert_eq!(error, Some(IntelProbeError::NoCores));

        let error = IntelCpuProvider::probe(
            &identity(0x4F),
            &topology(1),
            Box::new(ReplayMsr::new()),
            clock(),
        )
        .err();
        assert!(matches!(error, Some(IntelProbeError::NoThermalRegisters { .. })), "{error:?}");
    }

    #[test]
    fn rapl_domain_table_covers_the_reference_machine() {
        assert_eq!(rapl_domains(0x4F), RaplDomains { cores: false, dram: DramUnit::Fixed });
        assert_eq!(rapl_domains(0x9E), RaplDomains { cores: true, dram: DramUnit::PackageUnit });
        assert_eq!(rapl_domains(0xFF), RaplDomains { cores: false, dram: DramUnit::Absent });
    }

    // ---- the plausibility floor of the package meter --------------------------

    /// `MSR_PKG_POWER_INFO` of a 120 W part: TDP 960 counts of 1/8 W.
    const POWER_INFO_120_W: u64 = 0x01D0_03C0;
    /// Counts of 2^-14 J in the 250 ms window for 40 W, 0.79 W and exactly the
    /// 6 W floor of a 120 W part.
    const WINDOW_COUNTS_40_W: u64 = 10 * 16_384;
    const WINDOW_COUNTS_0_79_W: u64 = 3_230;
    const WINDOW_COUNTS_6_W: u64 = 6 * 16_384 / 4;

    /// A replay for `model` whose package counter returns `counters` in turn:
    /// the first is the probe's existence read, the second ends its window.
    fn metered(counters: &[u64], power_info: Option<u64>) -> ReplayMsr {
        let mut msr = replay(2);
        msr.set_sequence(cpu(0), MSR_PKG_ENERGY_STATUS, counters);
        match power_info {
            Some(info) => msr.set(cpu(0), MSR_PKG_POWER_INFO, info),
            None => msr.remove(cpu(0), MSR_PKG_POWER_INFO),
        };
        msr
    }

    fn package_power_offered(provider: &IntelCpuProvider) -> bool {
        keys(provider).contains(&"cpu.package.power".to_owned())
    }

    #[test]
    fn a_meter_far_below_the_tdp_of_a_broadwell_ep_part_is_not_offered_and_the_summary_says_why() {
        // Issue 3: 0.79 W for a 120 W part.
        let msr = metered(&[1_000, 1_000 + WINDOW_COUNTS_0_79_W], Some(POWER_INFO_120_W));
        let (provider, _) = provider(0x4F, msr);
        assert!(!package_power_offered(&provider), "{:?}", keys(&provider));
        assert_eq!(
            provider.summary(),
            "2 cores, package and per-core temperature (TjMax 85 °C); power: dram; not \
             available: package (reads 0.79 W, below the 6.0 W floor for a 120 W part)"
        );
    }

    #[test]
    fn a_haswell_ep_part_gets_the_same_check() {
        let msr = metered(&[1_000, 1_000 + WINDOW_COUNTS_0_79_W], Some(POWER_INFO_120_W));
        let (provider, _) = provider(0x3F, msr);
        assert!(!package_power_offered(&provider));
    }

    #[test]
    fn a_standing_counter_is_below_the_floor() {
        let msr = metered(&[1_000, 1_000], Some(POWER_INFO_120_W));
        let (provider, _) = provider(0x4F, msr);
        assert!(!package_power_offered(&provider));
        assert!(provider.summary().contains("package (reads 0.00 W"), "{}", provider.summary());
    }

    #[test]
    fn a_meter_at_a_healthy_rate_is_offered_and_measures_normally() {
        let in_window = 1_000 + WINDOW_COUNTS_40_W;
        let msr = metered(
            &[1_000, in_window, in_window, in_window + 32 * 16_384],
            Some(POWER_INFO_120_W),
        );
        let (mut provider, clock) = provider(0x4F, msr);
        assert!(package_power_offered(&provider));
        assert_eq!(
            provider.summary(),
            "2 cores, package and per-core temperature (TjMax 85 °C); power: package, dram"
        );
        sample(&mut provider);
        clock.advance(Duration::from_secs(2));
        let watts = sample(&mut provider)[4].unwrap();
        assert!((watts - 16.0).abs() < 1e-9, "{watts}");
    }

    #[test]
    fn the_floor_is_five_percent_of_the_tdp_and_a_reading_on_it_passes() {
        // 6 W is 5 % of 120 W: 1.5 J in 250 ms. Just under it is withheld.
        let at_floor = metered(&[1_000, 1_000 + WINDOW_COUNTS_6_W], Some(POWER_INFO_120_W));
        assert!(package_power_offered(&provider(0x4F, at_floor).0));
        let under = metered(&[1_000, 1_000 + WINDOW_COUNTS_6_W - 1], Some(POWER_INFO_120_W));
        assert!(!package_power_offered(&provider(0x4F, under).0));
    }

    #[test]
    fn the_probe_waits_the_window_on_its_clock_and_not_in_real_time() {
        let msr = metered(&[1_000, 1_000 + WINDOW_COUNTS_40_W], Some(POWER_INFO_120_W));
        let clock = ManualClock::new();
        let before = clock.now();
        let started = std::time::Instant::now();
        IntelCpuProvider::probe(
            &identity(0x4F),
            &topology(2),
            Box::new(msr),
            Box::new(clock.clone()),
        )
        .unwrap();
        assert_eq!(clock.now() - before, PLAUSIBILITY_WINDOW);
        assert!(started.elapsed() < Duration::from_secs(5), "expected a manual clock to not sleep");
    }

    #[test]
    fn models_without_a_datasheet_floor_keep_their_power_sensor_whatever_it_reads() {
        // Skylake client, as the table is not a statement about it.
        let msr = metered(&[1_000, 1_000 + WINDOW_COUNTS_0_79_W], Some(POWER_INFO_120_W));
        let log = msr.read_log();
        let (provider, _) = provider(0x5E, msr);
        assert!(package_power_offered(&provider));
        assert!(
            !log.lock().unwrap().contains(&(cpu(0), MSR_PKG_POWER_INFO)),
            "no TDP read where there is no floor"
        );
    }

    #[test]
    fn without_a_readable_tdp_the_meter_cannot_be_judged_and_is_offered() {
        let msr = metered(&[1_000, 1_000 + WINDOW_COUNTS_0_79_W], None);
        let (provider, _) = provider(0x4F, msr);
        assert!(package_power_offered(&provider));
    }

    #[test]
    fn a_zero_tdp_gives_no_floor() {
        let msr = metered(&[1_000, 1_000], Some(0));
        let (provider, _) = provider(0x4F, msr);
        assert!(package_power_offered(&provider), "a floor of 0 W cannot be undercut");
    }

    #[test]
    fn the_tdp_is_the_low_fifteen_bits_in_the_power_unit_of_the_unit_register() {
        // 0x3C0 = 960 counts of 2^-3 W = 120 W; bits above 14 are other fields.
        assert_eq!(tdp_watts(0x002F_0780_01D0_03C0, 0x000A_0E03), 120.0);
        // Unit exponent 0: one count per watt.
        assert_eq!(tdp_watts(95, 0x000A_0E00), 95.0);
    }

    #[test]
    fn the_floor_table_covers_the_two_models_the_datasheets_cover() {
        assert_eq!(idle_power_floor_watts(0x4F, 120.0), Some(6.0));
        assert_eq!(idle_power_floor_watts(0x3F, 145.0), Some(7.25));
        assert_eq!(idle_power_floor_watts(0x55, 120.0), None, "Skylake-SP has no source here");
        assert_eq!(idle_power_floor_watts(0x5E, 65.0), None);
    }

    #[test]
    fn the_energy_unit_comes_from_bits_12_to_8_and_must_be_plausible() {
        assert_eq!(energy_unit(0x000A_0E03), Some(1.0 / 16384.0));
        assert_eq!(energy_unit(0x0000_0003), None, "exponent 0");
        assert_eq!(energy_unit(0x0000_1F03), None, "exponent 31");
    }

    #[test]
    fn the_summary_names_what_was_found() {
        let (provider, _) = provider(0x4F, replay(2));
        assert_eq!(
            provider.summary(),
            "2 cores, package and per-core temperature (TjMax 85 °C); power: package, dram"
        );
    }
}
