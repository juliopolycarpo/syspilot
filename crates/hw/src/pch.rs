//! The chipset (Intel PCH) temperature.
//!
//! PawnIO's `IntelPCHThermal` module maps one of two register layouts and
//! returns the raw register; this file decides whether the value is a
//! reading and converts it. The module has no write function, so nothing here
//! can change the hardware.
//!
//! - [`PchRoute::PciBar0`]: BAR0 of the PCH thermal PCI function. The register
//!   at offset 0 holds the reading in bits 8:0, in 0.5 °C steps from -50 °C.
//!   Source: Linux `drivers/thermal/intel/intel_pch_thermal.c`, which masks the
//!   register with `WPT_TEMP_TSR` (`0x01FF`) and converts with `GET_WPT_TEMP`,
//!   `x * 1000 / 2 - 50000` millidegrees.
//! - [`PchRoute::FixedPwrm`]: the dword at `0xFE000000 + 0x1560` on recent
//!   client chipsets. Bit 9 says the reading is valid and bits 8:0 are whole
//!   degrees Celsius. Linux has no driver for this register; the layout is the
//!   one described by the author of the module (PawnIO.Modules pull request
//!   79, from the HWMonitor 1.65 analysis and one read on an Intel HM870
//!   platform, where `0x80000231` decoded to 49 °C). It is **unverified**
//!   against Intel documentation.

use std::fmt;

use syspilot_proto::sensor::{DeviceClass, SensorKind};

use crate::bus::{BusError, PchReading, PchRoute, PchThermal};
use crate::hub::{Provider, SensorSpec};

/// Seconds between reads. The chipset warms and cools over minutes.
pub const PERIOD_SECS: u32 = 2;

/// The device the sensor belongs to.
pub const DEVICE_NAME: &str = "Chipset";

/// Key of the one sensor.
pub const TEMPERATURE_KEY: &str = "chipset.temp";

/// Bits 8:0 of the register: the reading on both routes.
const FIELD_MASK: u32 = 0x1FF;

/// [`PchRoute::FixedPwrm`] only: the reading in the low bits is valid.
const PWRM_VALID_BIT: u32 = 1 << 9;

/// Readings outside this range are dropped. The lower bound is above the
/// register's own floor (-50 °C on the PCI route) because a PCH is never
/// colder than a room.
const PLAUSIBLE_CELSIUS: std::ops::RangeInclusive<f64> = 0.0..=125.0;

/// Converts a raw register value to °C, or `None` when it is not a reading.
///
/// A field of `0` (the register is not running) or `0x1FF` (all ones, which a
/// bus that does not answer reads as) is not a reading on either route; the
/// PWRM route also needs its valid bit. What remains is dropped when it falls
/// outside 0 to 125 °C.
pub fn decode_temperature(reading: PchReading) -> Option<f64> {
    let field = reading.raw & FIELD_MASK;
    if field == 0 || field == FIELD_MASK {
        return None;
    }
    let celsius = match reading.route {
        PchRoute::PciBar0 => f64::from(field) / 2.0 - 50.0,
        PchRoute::FixedPwrm => {
            if reading.raw & PWRM_VALID_BIT == 0 {
                return None;
            }
            f64::from(field)
        }
    };
    PLAUSIBLE_CELSIUS.contains(&celsius).then_some(celsius)
}

fn route_name(route: PchRoute) -> &'static str {
    match route {
        PchRoute::PciBar0 => "PCI thermal BAR0",
        PchRoute::FixedPwrm => "fixed PWRM window",
    }
}

/// A sentence fragment naming what the module found, from the five cells of
/// `ioctl_identity`: route, vendor/device dword, `bus << 16 | device << 8 |
/// function`, physical base and mapped size.
pub fn describe_identity(identity: [u64; 5]) -> String {
    let [route, didvid, address, base, size] = identity;
    let vendor = didvid & 0xFFFF;
    let device = (didvid >> 16) & 0xFFFF;
    let (bus, slot, function) = ((address >> 16) & 0xFF, (address >> 8) & 0xFF, address & 0xFF);
    let route = match PchRoute::from_cell(route) {
        Some(route) => route_name(route).to_owned(),
        None => format!("route {route}"),
    };
    format!(
        "Intel PCH {vendor:04X}:{device:04X} at PCI {bus:02x}:{slot:02x}.{function:x}, {route} at \
         {base:#x} ({size} bytes)"
    )
}

/// Why the chipset temperature could not start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PchProbeError {
    /// The first read failed.
    Bus(BusError),
    /// The register answered but the value is not a temperature.
    NoReading(PchReading),
}

impl fmt::Display for PchProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bus(error) => error.fmt(f),
            Self::NoReading(reading) => f.write_str(&no_reading(*reading)),
        }
    }
}

impl std::error::Error for PchProbeError {}

impl From<BusError> for PchProbeError {
    fn from(error: BusError) -> Self {
        Self::Bus(error)
    }
}

/// The message for a register value that is not a reading.
fn no_reading(reading: PchReading) -> String {
    format!(
        "(PCH thermal): expected a register value that decodes to {}..={} °C | received: {:#x} \
         on the {} route",
        PLAUSIBLE_CELSIUS.start(),
        PLAUSIBLE_CELSIUS.end(),
        reading.raw,
        route_name(reading.route)
    )
}

/// The chipset temperature: one sensor.
pub struct PchThermalProvider {
    summary: String,
    sensor: Box<dyn PchThermal>,
    last_problem: Option<String>,
}

impl PchThermalProvider {
    /// Reads the register once to see that it holds a reading, so a chipset
    /// whose register is dead gets no provider. `description` names what the
    /// module found (see [`describe_identity`]).
    pub fn probe(
        mut sensor: Box<dyn PchThermal>,
        description: &str,
    ) -> Result<Self, PchProbeError> {
        let reading = sensor.read_raw()?;
        let Some(celsius) = decode_temperature(reading) else {
            return Err(PchProbeError::NoReading(reading));
        };
        Ok(Self {
            summary: format!("{description}, reads {celsius:.1} °C"),
            sensor,
            last_problem: None,
        })
    }

    /// What was found, for the feature detail.
    pub fn summary(&self) -> &str {
        &self.summary
    }
}

impl Provider for PchThermalProvider {
    fn sensors(&self) -> Vec<SensorSpec> {
        vec![SensorSpec {
            device: DEVICE_NAME.to_owned(),
            class: DeviceClass::Board,
            key: TEMPERATURE_KEY.to_owned(),
            label: "PCH".to_owned(),
            kind: SensorKind::Temperature,
        }]
    }

    fn sample(&mut self, out: &mut Vec<Option<f64>>) {
        let value = match self.sensor.read_raw() {
            Ok(reading) => {
                let value = decode_temperature(reading);
                self.last_problem = value.is_none().then(|| no_reading(reading));
                value
            }
            Err(error) => {
                self.last_problem = Some(error.to_string());
                None
            }
        };
        out.push(value);
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
    use super::*;
    use crate::bus::replay::ReplayPchThermal;
    use syspilot_pawnio::Error as DriverError;

    fn pci(raw: u32) -> PchReading {
        PchReading { raw, route: PchRoute::PciBar0 }
    }

    fn pwrm(raw: u32) -> PchReading {
        PchReading { raw, route: PchRoute::FixedPwrm }
    }

    // -- decoding -------------------------------------------------------------

    #[test]
    fn the_pci_route_reads_half_degrees_from_minus_fifty() {
        // Field 0x0C8 is 200: 200 / 2 - 50 = 50 °C.
        assert_eq!(decode_temperature(pci(0x00C8)), Some(50.0));
        assert_eq!(decode_temperature(pci(0x00C9)), Some(50.5));
        assert_eq!(decode_temperature(pci(100)), Some(0.0), "the lowest plausible field");
        assert_eq!(decode_temperature(pci(300)), Some(100.0));
        assert_eq!(decode_temperature(pci(350)), Some(125.0), "the highest plausible field");
    }

    #[test]
    fn the_pci_route_masks_the_bits_above_the_field_as_linux_does() {
        assert_eq!(decode_temperature(pci(0xFE00 | 0x00C8)), Some(50.0));
    }

    #[test]
    fn the_pci_route_drops_a_register_that_is_off_or_a_bus_that_does_not_answer() {
        assert_eq!(decode_temperature(pci(0)), None, "zero: not running");
        assert_eq!(decode_temperature(pci(0xFFFF)), None, "all ones: bus fill");
        assert_eq!(decode_temperature(pci(0x01FF)), None, "field all ones");
        assert_eq!(decode_temperature(pci(0xFE00)), None, "field zero under set high bits");
    }

    #[test]
    fn the_pci_route_drops_readings_outside_zero_to_125_degrees() {
        assert_eq!(decode_temperature(pci(99)), None, "-0.5 °C");
        assert_eq!(decode_temperature(pci(1)), None, "-49.5 °C");
        assert_eq!(decode_temperature(pci(351)), None, "125.5 °C");
        assert_eq!(decode_temperature(pci(0x1FE)), None, "205 °C");
    }

    #[test]
    fn the_pwrm_route_needs_the_valid_bit_and_reads_whole_degrees() {
        // The value the module author read on an Intel HM870 platform.
        assert_eq!(decode_temperature(pwrm(0x8000_0231)), Some(49.0));
        assert_eq!(decode_temperature(pwrm(0x0000_0231)), Some(49.0));
        assert_eq!(decode_temperature(pwrm(0x0000_0031)), None, "valid bit clear");
        assert_eq!(decode_temperature(pwrm(0x8000_0031)), None, "valid bit clear");
    }

    #[test]
    fn the_pwrm_route_drops_zero_all_ones_and_out_of_range_values() {
        assert_eq!(decode_temperature(pwrm(0)), None);
        assert_eq!(decode_temperature(pwrm(0x200)), None, "valid but field zero");
        assert_eq!(decode_temperature(pwrm(0xFFFF_FFFF)), None, "bus fill");
        assert_eq!(decode_temperature(pwrm(0x3FF)), None, "field all ones");
        assert_eq!(decode_temperature(pwrm(0x200 | 125)), Some(125.0));
        assert_eq!(decode_temperature(pwrm(0x200 | 126)), None);
        assert_eq!(decode_temperature(pwrm(0x200 | 1)), Some(1.0), "a cold but possible value");
    }

    // -- identity -------------------------------------------------------------

    #[test]
    fn the_identity_names_the_device_the_function_and_the_window() {
        assert_eq!(
            describe_identity([1, 0x8C24_8086, 0x1F06, 0xF7E1_A000, 0x100]),
            "Intel PCH 8086:8C24 at PCI 00:1f.6, PCI thermal BAR0 at 0xf7e1a000 (256 bytes)"
        );
        assert_eq!(
            describe_identity([2, 0x7F0C_8086, 0x1F00, 0xFE00_0000, 0x2000]),
            "Intel PCH 8086:7F0C at PCI 00:1f.0, fixed PWRM window at 0xfe000000 (8192 bytes)"
        );
        assert!(describe_identity([9, 0, 0, 0, 0]).contains("route 9"));
    }

    // -- the provider ---------------------------------------------------------

    fn provider(route: PchRoute, replies: Vec<Result<u32, BusError>>) -> PchThermalProvider {
        PchThermalProvider::probe(
            Box::new(ReplayPchThermal::from_replies(route, replies)),
            "Intel PCH example",
        )
        .unwrap_or_else(|error| panic!("(pch provider): expected a probe | received: {error}"))
    }

    #[test]
    fn the_provider_declares_one_driver_backed_chipset_temperature() {
        let provider = provider(PchRoute::PciBar0, vec![Ok(0x00C8)]);
        let sensors = provider.sensors();
        assert_eq!(
            sensors,
            [SensorSpec {
                device: "Chipset".into(),
                class: DeviceClass::Board,
                key: "chipset.temp".into(),
                label: "PCH".into(),
                kind: SensorKind::Temperature,
            }]
        );
        assert!(provider.needs_driver());
        assert_eq!(provider.period_secs(), 2);
        assert_eq!(provider.summary(), "Intel PCH example, reads 50.0 °C");
    }

    #[test]
    fn a_sample_appends_exactly_one_value() {
        let mut provider = provider(PchRoute::FixedPwrm, vec![Ok(0x8000_0231), Ok(0x8000_0232)]);
        let mut values = Vec::new();
        provider.sample(&mut values);
        provider.sample(&mut values);
        assert_eq!(values, [Some(50.0), Some(50.0)], "the probe consumed the first reply");
        assert_eq!(provider.last_problem(), None);
    }

    #[test]
    fn a_failed_read_gives_no_value_and_says_why_until_the_next_good_one() {
        let timeout = BusError::from(DriverError::Timeout { function: "ioctl_read_raw".into() });
        let mut provider =
            provider(PchRoute::PciBar0, vec![Ok(0x00C8), Err(timeout.clone()), Ok(0x00CA)]);
        let mut values = Vec::new();
        provider.sample(&mut values);
        assert_eq!(values, [None]);
        assert_eq!(provider.last_problem(), Some(timeout.to_string().as_str()));
        values.clear();
        provider.sample(&mut values);
        assert_eq!(values, [Some(51.0)]);
        assert_eq!(provider.last_problem(), None);
    }

    #[test]
    fn a_value_that_is_not_a_temperature_is_dropped_with_the_raw_value_named() {
        let mut provider = provider(PchRoute::PciBar0, vec![Ok(0x00C8), Ok(0xFFFF)]);
        let mut values = Vec::new();
        provider.sample(&mut values);
        assert_eq!(values, [None]);
        assert_eq!(
            provider.last_problem(),
            Some(
                "(PCH thermal): expected a register value that decodes to 0..=125 °C | received: \
                 0xffff on the PCI thermal BAR0 route"
            )
        );
    }

    #[test]
    fn a_register_that_never_holds_a_reading_gets_no_provider() {
        let Err(error) = PchThermalProvider::probe(
            Box::new(ReplayPchThermal::new(PchRoute::FixedPwrm, &[0])),
            "Intel PCH example",
        ) else {
            panic!("(pch provider): expected no provider for a zero register | received: one")
        };
        assert_eq!(error, PchProbeError::NoReading(pwrm(0)));
        assert_eq!(
            error.to_string(),
            "(PCH thermal): expected a register value that decodes to 0..=125 °C | received: 0x0 \
             on the fixed PWRM window route"
        );
    }

    #[test]
    fn a_first_read_that_fails_keeps_the_bus_error() {
        let refused = BusError::from(DriverError::NotReady { function: "ioctl_read_raw".into() });
        let Err(error) = PchThermalProvider::probe(
            Box::new(ReplayPchThermal::from_replies(PchRoute::PciBar0, vec![Err(refused.clone())])),
            "Intel PCH example",
        ) else {
            panic!("(pch provider): expected an error | received: a provider")
        };
        assert_eq!(error, PchProbeError::Bus(refused));
    }
}
