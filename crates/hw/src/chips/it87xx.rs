//! ITE IT87xx hardware monitors.
//!
//! The monitor is a flat 8-bit register file behind an index/data pair at
//! `base + 5` / `base + 6`. After writing an index the address port reads
//! back the index it holds; a mismatch means the access did not take, and
//! the value is dropped (the IT8688E does not read the index back, so it is
//! exempt).
//!
//! Chips whose temperature and fan registers sit behind the bank-select
//! register (`0x06`: IT8655E and IT8665E) are detected but not read: reading
//! them correctly means writing that register on every pass, and SysPilot
//! writes no register that is not an index, a bank for a read or an SPD page.

use syspilot_proto::sensor::SensorKind;

use crate::bus::{BusError, SuperIo};

use super::{Channel, MonitorChip};

/// Logical device of the environment controller.
pub const HWM_LOGICAL_DEVICE: u8 = 0x04;
/// Configuration register holding the chip version in its low nibble.
pub const VERSION_REGISTER: u8 = 0x22;

const VENDOR_REGISTER: u8 = 0x58;
/// What the vendor register holds on ITE monitors.
const VENDOR_IDS: [u8; 2] = [0x90, 0x7F];
const CONFIGURATION_REGISTER: u8 = 0x00;
/// Bit that is always set in the configuration register of a live monitor.
const CONFIGURATION_ALIVE: u8 = 0x10;
const TEMPERATURE_BASE: u8 = 0x29;
/// Inputs 0..8 are `0x20..=0x28`; the rest are scattered.
const VOLTAGE_REGISTERS: [u8; 13] =
    [0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x2F, 0x2C, 0x2D, 0x2E];
const TACHOMETER: [u8; 6] = [0x0D, 0x0E, 0x0F, 0x80, 0x82, 0x4C];
const TACHOMETER_EXTENDED: [u8; 6] = [0x18, 0x19, 0x1A, 0x81, 0x83, 0x4D];
/// IT8625E and IT8665E keep the sixth tachometer elsewhere.
const TACHOMETER_ALT_SIXTH: (u8, u8) = (0x93, 0x94);
const FAN_16_BIT_REGISTER: u8 = 0x0C;
const FAN_16_BIT_REGISTER_IT8665E: u8 = 0x0B;
const FAN_DIVISOR_REGISTER: u8 = 0x0B;
const DUTY_CONTROL: [u8; 6] = [0x15, 0x16, 0x17, 0x7F, 0xA7, 0xAF];
const DUTY_CONTROL_ALT: [u8; 6] = [0x15, 0x16, 0x17, 0x1E, 0x1F, 0x92];
const DUTY_EXTENDED: [u8; 6] = [0x63, 0x6B, 0x73, 0x7B, 0xA3, 0xAB];
/// Fan tachometer constant.
const FAN_COUNTS_PER_MINUTE: f64 = 1.35e6;
/// A voltage register at full scale is clipped.
const VOLTAGE_CLIPPED: u8 = 0xFF;

/// The chips this module knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Chip {
    It8613E,
    It8620E,
    It8625E,
    It8628E,
    It8631E,
    It8638E,
    It8655E,
    It8665E,
    It8686E,
    It8688E,
    It8689E,
    It8696E,
    It8705F,
    It8712F,
    It8716F,
    It8718F,
    It8720F,
    It8721F,
    It8726F,
    It8728F,
    It8771E,
    It8772E,
    It8790E,
    It8792E,
    It87952E,
}

/// What distinguishes the chips: how many inputs they have and how the
/// duty registers work.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Spec {
    voltages: usize,
    temperatures: usize,
    fans: usize,
    controls: usize,
    /// Duty is read from an 8-bit extended register, not the 7-bit field of
    /// the control register.
    extended_duty: bool,
    /// Volts per count.
    gain: f64,
    /// IT8625E and IT8665E move the sixth duty and tachometer registers.
    alternate_sixth: bool,
}

const fn spec(
    voltages: usize,
    temperatures: usize,
    fans: usize,
    controls: usize,
    extended_duty: bool,
    gain: f64,
) -> Spec {
    Spec { voltages, temperatures, fans, controls, extended_duty, gain, alternate_sixth: false }
}

impl Chip {
    /// Every known chip.
    pub const ALL: [Chip; 25] = [
        Chip::It8613E,
        Chip::It8620E,
        Chip::It8625E,
        Chip::It8628E,
        Chip::It8631E,
        Chip::It8638E,
        Chip::It8655E,
        Chip::It8665E,
        Chip::It8686E,
        Chip::It8688E,
        Chip::It8689E,
        Chip::It8696E,
        Chip::It8705F,
        Chip::It8712F,
        Chip::It8716F,
        Chip::It8718F,
        Chip::It8720F,
        Chip::It8721F,
        Chip::It8726F,
        Chip::It8728F,
        Chip::It8771E,
        Chip::It8772E,
        Chip::It8790E,
        Chip::It8792E,
        Chip::It87952E,
    ];

    /// The marketing name.
    pub fn name(self) -> &'static str {
        match self {
            Self::It8613E => "IT8613E",
            Self::It8620E => "IT8620E",
            Self::It8625E => "IT8625E",
            Self::It8628E => "IT8628E",
            Self::It8631E => "IT8631E",
            Self::It8638E => "IT8638E",
            Self::It8655E => "IT8655E",
            Self::It8665E => "IT8665E",
            Self::It8686E => "IT8686E",
            Self::It8688E => "IT8688E",
            Self::It8689E => "IT8689E",
            Self::It8696E => "IT8696E",
            Self::It8705F => "IT8705F",
            Self::It8712F => "IT8712F",
            Self::It8716F => "IT8716F",
            Self::It8718F => "IT8718F",
            Self::It8720F => "IT8720F",
            Self::It8721F => "IT8721F",
            Self::It8726F => "IT8726F",
            Self::It8728F => "IT8728F",
            Self::It8771E => "IT8771E",
            Self::It8772E => "IT8772E",
            Self::It8790E => "IT8790E",
            Self::It8792E => "IT8792E",
            Self::It87952E => "IT87952E",
        }
    }

    /// Whether reading the chip needs the bank-select register written.
    pub fn needs_bank_select(self) -> bool {
        matches!(self, Self::It8655E | Self::It8665E)
    }

    fn spec(self) -> Spec {
        match self {
            Self::It8613E => spec(10, 4, 5, 4, true, 0.012),
            Self::It8620E => spec(9, 3, 5, 5, true, 0.012),
            Self::It8625E => Spec { alternate_sixth: true, ..spec(7, 3, 6, 6, true, 0.011) },
            Self::It8628E => spec(10, 6, 6, 6, true, 0.012),
            Self::It8631E | Self::It8638E => spec(9, 2, 2, 2, true, 0.012),
            Self::It8655E => spec(9, 6, 3, 3, true, 0.0109),
            Self::It8665E => Spec { alternate_sixth: true, ..spec(9, 6, 6, 6, true, 0.0109) },
            Self::It8686E => spec(10, 7, 6, 5, true, 0.012),
            Self::It8688E => spec(11, 6, 6, 5, true, 0.012),
            Self::It8689E | Self::It8696E => spec(10, 6, 6, 6, true, 0.012),
            Self::It8705F => spec(9, 3, 3, 3, false, 0.016),
            Self::It8721F | Self::It8728F | Self::It8772E => spec(9, 3, 5, 3, true, 0.012),
            Self::It8771E => spec(9, 3, 5, 3, false, 0.012),
            Self::It8792E | Self::It87952E => spec(9, 3, 3, 3, true, 0.011),
            Self::It8712F
            | Self::It8716F
            | Self::It8718F
            | Self::It8720F
            | Self::It8726F
            | Self::It8790E => spec(9, 3, 5, 3, false, 0.016),
        }
    }

    /// Whether the chip's index register does not read back what was
    /// written, which disables the read-back check.
    fn index_is_not_readable(self) -> bool {
        self == Self::It8688E
    }

    /// The duty control registers of the chip.
    fn duty_registers(self) -> &'static [u8] {
        match self {
            Self::It8665E | Self::It8625E => &DUTY_CONTROL_ALT,
            Self::It8792E => &DUTY_CONTROL[..3],
            _ => &DUTY_CONTROL,
        }
    }
}

/// The chip behind the 16-bit ID in configuration registers `0x20`/`0x21`.
pub fn identify(id: u16) -> Option<Chip> {
    Some(match id {
        0x8613 => Chip::It8613E,
        0x8620 => Chip::It8620E,
        0x8625 => Chip::It8625E,
        0x8628 => Chip::It8628E,
        0x8631 => Chip::It8631E,
        0x8638 => Chip::It8638E,
        0x8655 => Chip::It8655E,
        0x8665 => Chip::It8665E,
        0x8686 => Chip::It8686E,
        0x8688 => Chip::It8688E,
        0x8689 => Chip::It8689E,
        0x8696 => Chip::It8696E,
        0x8705 => Chip::It8705F,
        0x8712 => Chip::It8712F,
        0x8716 => Chip::It8716F,
        0x8718 => Chip::It8718F,
        0x8720 => Chip::It8720F,
        0x8721 => Chip::It8721F,
        0x8726 => Chip::It8726F,
        0x8728 => Chip::It8728F,
        0x8771 => Chip::It8771E,
        0x8772 => Chip::It8772E,
        0x8790 => Chip::It8790E,
        // These two carry the ID of the family member they extend.
        0x8733 => Chip::It8792E,
        0x8695 => Chip::It87952E,
        _ => return None,
    })
}

/// Reads one monitor register. `None` when the address port did not read
/// back the index (the access did not take).
fn read_checked(
    io: &mut dyn SuperIo,
    base: u16,
    chip: Chip,
    register: u8,
) -> Result<Option<u8>, BusError> {
    io.hwm_set_index(base, register)?;
    let value = io.hwm_read_data(base)?;
    let accepted = chip.index_is_not_readable() || io.hwm_read_index(base)? == register;
    Ok(accepted.then_some(value))
}

/// Whether the monitor behind `base` is a live ITE monitor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verification {
    /// Vendor ID and configuration register are as expected.
    Verified,
    /// Not an ITE monitor, or not alive; the text names the value found.
    Rejected(String),
}

/// Checks the vendor ID (`0x58`) and that the configuration register's
/// "alive" bit is set. The IT8655E and IT8665E do not set that bit.
pub fn verify(io: &mut dyn SuperIo, base: u16, chip: Chip) -> Result<Verification, BusError> {
    let Some(vendor) = read_checked(io, base, chip, VENDOR_REGISTER)? else {
        return Ok(Verification::Rejected(
            "(it87xx): expected the address port to read back the vendor index | received: a \
             different index"
                .to_owned(),
        ));
    };
    if !VENDOR_IDS.contains(&vendor) {
        return Ok(Verification::Rejected(format!(
            "(it87xx): expected vendor id 0x90 or 0x7f | received: {vendor:#04x}"
        )));
    }
    let exempt = matches!(chip, Chip::It8655E | Chip::It8665E);
    let configuration = read_checked(io, base, chip, CONFIGURATION_REGISTER)?;
    match configuration {
        Some(value) if value & CONFIGURATION_ALIVE != 0 || exempt => Ok(Verification::Verified),
        Some(value) => Ok(Verification::Rejected(format!(
            "(it87xx): expected configuration register bit 4 set | received: {value:#04x}"
        ))),
        None => Ok(Verification::Rejected(
            "(it87xx): expected the address port to read back the configuration index | \
             received: a different index"
                .to_owned(),
        )),
    }
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Temperature(usize),
    Voltage(usize),
    Fan(usize),
    Duty(usize),
}

struct Snapshot {
    temperatures: Vec<Option<f64>>,
    voltages: Vec<Option<f64>>,
    fans: Vec<Option<f64>>,
    duty: Vec<Option<f64>>,
}

/// A detected ITE chip.
#[derive(Debug)]
pub struct It87xx {
    chip: Chip,
    base: u16,
    spec: Spec,
    /// Whether the tachometers are 16 bit. Early IT8705F and IT8712F
    /// revisions have 8-bit counters with a divisor.
    sixteen_bit_fans: bool,
    /// Fan inputs that are not enabled for 16-bit counting.
    fans_disabled: Vec<bool>,
    declared: Vec<Source>,
}

impl It87xx {
    /// A reader for `chip` of chip `version` (the low nibble of
    /// configuration register `0x22`) whose monitor ports start at `base`.
    pub fn new(chip: Chip, version: u8, base: u16) -> Self {
        let spec = chip.spec();
        let sixteen_bit_fans =
            (chip != Chip::It8705F || version >= 3) && (chip != Chip::It8712F || version >= 8);
        Self {
            chip,
            base,
            spec,
            sixteen_bit_fans,
            fans_disabled: vec![false; spec.fans],
            declared: Vec::new(),
        }
    }

    fn read_register(&self, io: &mut dyn SuperIo, register: u8) -> Result<Option<u8>, BusError> {
        read_checked(io, self.base, self.chip, register)
    }

    /// Which fan inputs count with 16 bits: inputs 4 and 5 and (on bigger
    /// chips) 6 have an enable bit.
    fn read_fan_enables(&mut self, io: &mut dyn SuperIo) -> Result<(), BusError> {
        self.fans_disabled = vec![false; self.spec.fans];
        if !self.sixteen_bit_fans {
            return Ok(());
        }
        let Some(modes) = self.read_register(io, FAN_16_BIT_REGISTER)? else { return Ok(()) };
        if self.spec.fans >= 5 {
            self.fans_disabled[3] = modes & (1 << 4) == 0;
            self.fans_disabled[4] = modes & (1 << 5) == 0;
        }
        if self.spec.fans >= 6 {
            self.fans_disabled[5] = if self.chip == Chip::It8665E {
                match self.read_register(io, FAN_16_BIT_REGISTER_IT8665E)? {
                    Some(alternate) => alternate & (1 << 3) == 0,
                    None => false,
                }
            } else {
                modes & (1 << 2) == 0
            };
        }
        Ok(())
    }

    fn read_voltages(&self, io: &mut dyn SuperIo) -> Result<Vec<Option<f64>>, BusError> {
        let mut values = Vec::with_capacity(self.spec.voltages);
        for &register in &VOLTAGE_REGISTERS[..self.spec.voltages] {
            values.push(
                self.read_register(io, register)?
                    .filter(|&raw| raw != 0 && raw != VOLTAGE_CLIPPED)
                    .map(|raw| f64::from(raw) * self.spec.gain),
            );
        }
        Ok(values)
    }

    fn read_temperatures(&self, io: &mut dyn SuperIo) -> Result<Vec<Option<f64>>, BusError> {
        let mut values = Vec::with_capacity(self.spec.temperatures);
        for index in 0..self.spec.temperatures {
            let register = TEMPERATURE_BASE + index as u8;
            // 0 and 127 and above are "no sensor"; negative values do not occur.
            values.push(
                self.read_register(io, register)?
                    .map(|raw| raw as i8)
                    .filter(|&degrees| degrees > 0 && degrees < i8::MAX)
                    .map(f64::from),
            );
        }
        Ok(values)
    }

    fn read_fans(&self, io: &mut dyn SuperIo) -> Result<Vec<Option<f64>>, BusError> {
        let mut values = Vec::with_capacity(self.spec.fans);
        for index in 0..self.spec.fans {
            if self.fans_disabled[index] {
                values.push(None);
                continue;
            }
            values.push(if self.sixteen_bit_fans {
                self.read_fan_16_bit(io, index)?
            } else {
                self.read_fan_8_bit(io, index)?
            });
        }
        Ok(values)
    }

    fn read_fan_16_bit(&self, io: &mut dyn SuperIo, index: usize) -> Result<Option<f64>, BusError> {
        let (low_register, high_register) = if self.spec.alternate_sixth && index == 5 {
            TACHOMETER_ALT_SIXTH
        } else {
            (TACHOMETER[index], TACHOMETER_EXTENDED[index])
        };
        let Some(low) = self.read_register(io, low_register)? else { return Ok(None) };
        let Some(high) = self.read_register(io, high_register)? else { return Ok(None) };
        let count = u32::from(low) | (u32::from(high) << 8);
        Ok(if count > 0x3F {
            // 0xFFFF is the counter's overflow: stopped or absent.
            Some(if count < 0xFFFF { FAN_COUNTS_PER_MINUTE / f64::from(count * 2) } else { 0.0 })
        } else {
            None
        })
    }

    fn read_fan_8_bit(&self, io: &mut dyn SuperIo, index: usize) -> Result<Option<f64>, BusError> {
        let Some(count) = self.read_register(io, TACHOMETER[index])? else { return Ok(None) };
        // Fans 1 and 2 have a programmable divisor, three bits each; the
        // rest divide by 2.
        let divisor = if index < 2 {
            let Some(divisors) = self.read_register(io, FAN_DIVISOR_REGISTER)? else {
                return Ok(None);
            };
            1u32 << ((divisors >> (3 * index)) & 0x7)
        } else {
            2
        };
        Ok(match count {
            0 => None,
            0xFF => Some(0.0),
            count => Some(FAN_COUNTS_PER_MINUTE / f64::from(u32::from(count) * divisor)),
        })
    }

    fn read_duty(&self, io: &mut dyn SuperIo) -> Result<Vec<Option<f64>>, BusError> {
        let registers = self.chip.duty_registers();
        let mut values = Vec::with_capacity(self.spec.controls);
        for index in 0..self.spec.controls {
            let Some(control) = self.read_register(io, registers[index])? else {
                values.push(None);
                continue;
            };
            // Bit 7 set: the chip's automatic mode owns the output and the
            // register does not hold the duty.
            if control & 0x80 != 0 {
                values.push(None);
            } else if self.spec.extended_duty {
                values.push(
                    self.read_register(io, DUTY_EXTENDED[index])?
                        .map(|raw| (f64::from(raw) * 100.0 / 255.0).round()),
                );
            } else {
                values.push(Some((f64::from(control & 0x7F) * 100.0 / 127.0).round()));
            }
        }
        Ok(values)
    }

    fn snapshot(&self, io: &mut dyn SuperIo) -> Result<Snapshot, BusError> {
        Ok(Snapshot {
            temperatures: self.read_temperatures(io)?,
            voltages: self.read_voltages(io)?,
            fans: self.read_fans(io)?,
            duty: self.read_duty(io)?,
        })
    }

    fn value_of(snapshot: &Snapshot, source: Source) -> Option<f64> {
        match source {
            Source::Temperature(index) => snapshot.temperatures.get(index).copied().flatten(),
            Source::Voltage(index) => snapshot.voltages.get(index).copied().flatten(),
            Source::Fan(index) => snapshot.fans.get(index).copied().flatten(),
            Source::Duty(index) => snapshot.duty.get(index).copied().flatten(),
        }
    }
}

impl MonitorChip for It87xx {
    fn name(&self) -> String {
        self.chip.name().to_owned()
    }

    fn discover(&mut self, io: &mut dyn SuperIo) -> Result<Vec<Channel>, BusError> {
        self.read_fan_enables(io)?;
        let snapshot = self.snapshot(io)?;
        let mut declared = Vec::new();
        let mut channels = Vec::new();
        for (index, value) in snapshot.temperatures.iter().enumerate() {
            if value.is_some() {
                declared.push(Source::Temperature(index));
                channels.push(Channel::new(
                    format!("board.temp.temp{}", index + 1),
                    format!("Temperature {}", index + 1),
                    SensorKind::Temperature,
                ));
            }
        }
        for (index, value) in snapshot.voltages.iter().enumerate() {
            if value.is_some() {
                declared.push(Source::Voltage(index));
                channels.push(Channel::new(
                    format!("board.vin{index}"),
                    format!("VIN{index}"),
                    SensorKind::Voltage,
                ));
            }
        }
        for (index, value) in snapshot.fans.iter().enumerate() {
            if value.is_some() {
                declared.push(Source::Fan(index));
                channels.push(Channel::new(
                    format!("board.fan{}", index + 1),
                    format!("Fan {}", index + 1),
                    SensorKind::Fan,
                ));
            }
        }
        for (index, value) in snapshot.duty.iter().enumerate() {
            if value.is_some() {
                declared.push(Source::Duty(index));
                channels.push(Channel::new(
                    format!("board.pwm{}", index + 1),
                    format!("Fan {} duty", index + 1),
                    SensorKind::Duty,
                ));
            }
        }
        self.declared = declared;
        Ok(channels)
    }

    fn read(&mut self, io: &mut dyn SuperIo) -> Result<Vec<Option<f64>>, BusError> {
        let snapshot = self.snapshot(io)?;
        Ok(self.declared.iter().map(|&source| Self::value_of(&snapshot, source)).collect())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::bus::replay::{ReplaySuperIo, SuperIoWrite, superio_hwm_key};
    use crate::bus::{Fixture, SuperIoSlot, Unlock};

    const BASE: u16 = 0x0290;

    fn replay(registers: &[(u8, u8)]) -> ReplaySuperIo {
        let mut fixture = Fixture::new();
        for &(register, value) in registers {
            fixture.set(superio_hwm_key(SuperIoSlot::Primary, 0, register), u64::from(value));
        }
        let mut io = ReplaySuperIo::new(fixture);
        io.select_slot(SuperIoSlot::Primary).unwrap();
        io.enter_config_mode(Unlock::Ite).unwrap();
        io.discover_io_ranges().unwrap();
        io.exit_config_mode(Unlock::Ite).unwrap();
        io
    }

    /// A chip whose address port does not read back the index it was given,
    /// as a stuck bus would behave.
    struct StaleIndexSuperIo(ReplaySuperIo);

    impl SuperIo for StaleIndexSuperIo {
        fn select_slot(&mut self, slot: SuperIoSlot) -> Result<(), BusError> {
            self.0.select_slot(slot)
        }
        fn enter_config_mode(&mut self, unlock: Unlock) -> Result<(), BusError> {
            self.0.enter_config_mode(unlock)
        }
        fn exit_config_mode(&mut self, unlock: Unlock) -> Result<(), BusError> {
            self.0.exit_config_mode(unlock)
        }
        fn select_logical_device(&mut self, device: u8) -> Result<(), BusError> {
            self.0.select_logical_device(device)
        }
        fn config_read(&mut self, register: u8) -> Result<u8, BusError> {
            self.0.config_read(register)
        }
        fn discover_io_ranges(&mut self) -> Result<(), BusError> {
            self.0.discover_io_ranges()
        }
        fn hwm_set_index(&mut self, base: u16, index: u8) -> Result<(), BusError> {
            self.0.hwm_set_index(base, index)
        }
        fn hwm_write_bank(&mut self, base: u16, bank: u8) -> Result<(), BusError> {
            self.0.hwm_write_bank(base, bank)
        }
        fn hwm_read_index(&mut self, _base: u16) -> Result<u8, BusError> {
            Ok(0xFF)
        }
        fn hwm_read_data(&mut self, base: u16) -> Result<u8, BusError> {
            self.0.hwm_read_data(base)
        }
    }

    /// All-zero registers for a chip with `spec`, overlaid with `overrides`.
    fn registers(overrides: &[(u8, u8)]) -> Vec<(u8, u8)> {
        let mut all: Vec<(u8, u8)> = (0..=0xFFu8).map(|register| (register, 0)).collect();
        for &(register, value) in overrides {
            all[usize::from(register)].1 = value;
        }
        all
    }

    #[test]
    fn identify_maps_the_published_ids() {
        assert_eq!(identify(0x8686), Some(Chip::It8686E));
        assert_eq!(identify(0x8733), Some(Chip::It8792E));
        assert_eq!(identify(0x8695), Some(Chip::It87952E));
        assert_eq!(identify(0x8790), Some(Chip::It8790E));
        assert_eq!(identify(0x0000), None);
        assert_eq!(identify(0xFFFF), None);
        assert_eq!(identify(0x8883), None, "the ID a bugged IT8792E reports");
    }

    #[test]
    fn every_chip_has_one_id_and_one_name() {
        let ids: Vec<u16> =
            (0..=0xFFFFu32).map(|id| id as u16).filter(|id| identify(*id).is_some()).collect();
        assert_eq!(ids.len(), Chip::ALL.len());
        let chips: BTreeSet<_> = ids.iter().map(|id| identify(*id).unwrap().name()).collect();
        assert_eq!(chips.len(), Chip::ALL.len(), "names are unique");
        assert!(Chip::ALL.iter().all(|chip| ids.iter().any(|id| identify(*id) == Some(*chip))));
    }

    #[test]
    fn only_the_two_banked_chips_are_refused() {
        let refused: Vec<_> =
            Chip::ALL.iter().filter(|c| c.needs_bank_select()).map(|c| c.name()).collect();
        assert_eq!(refused, ["IT8655E", "IT8665E"]);
    }

    #[test]
    fn channel_counts_follow_the_chip() {
        let counts = |chip: Chip| {
            let spec = chip.spec();
            (spec.voltages, spec.temperatures, spec.fans, spec.controls)
        };
        assert_eq!(counts(Chip::It8613E), (10, 4, 5, 4));
        assert_eq!(counts(Chip::It8686E), (10, 7, 6, 5));
        assert_eq!(counts(Chip::It8688E), (11, 6, 6, 5));
        assert_eq!(counts(Chip::It8792E), (9, 3, 3, 3));
        assert_eq!(counts(Chip::It8712F), (9, 3, 5, 3));
        for chip in Chip::ALL {
            let spec = chip.spec();
            assert!(spec.voltages <= VOLTAGE_REGISTERS.len(), "{}", chip.name());
            assert!(spec.fans <= TACHOMETER.len() && spec.controls <= DUTY_CONTROL.len());
            assert!(chip.duty_registers().len() >= spec.controls, "{}", chip.name());
        }
    }

    #[test]
    fn the_voltage_gain_follows_the_chip_generation() {
        assert_eq!(Chip::It8686E.spec().gain, 0.012);
        assert_eq!(Chip::It8625E.spec().gain, 0.011);
        assert_eq!(Chip::It8665E.spec().gain, 0.0109);
        assert_eq!(Chip::It8712F.spec().gain, 0.016);
    }

    #[test]
    fn a_live_monitor_verifies() {
        let mut io = replay(&registers(&[(0x58, 0x90), (0x00, 0x1B)]));
        assert_eq!(verify(&mut io, BASE, Chip::It8686E), Ok(Verification::Verified));
        let mut io = replay(&registers(&[(0x58, 0x7F), (0x00, 0x10)]));
        assert_eq!(verify(&mut io, BASE, Chip::It8686E), Ok(Verification::Verified));
    }

    #[test]
    fn a_foreign_vendor_id_is_rejected_with_the_value_found() {
        let mut io = replay(&registers(&[(0x58, 0x23), (0x00, 0x1B)]));
        let Ok(Verification::Rejected(reason)) = verify(&mut io, BASE, Chip::It8686E) else {
            panic!("expected a rejection");
        };
        assert_eq!(reason, "(it87xx): expected vendor id 0x90 or 0x7f | received: 0x23");
    }

    #[test]
    fn a_monitor_without_the_alive_bit_is_rejected_unless_the_chip_is_exempt() {
        let dead = registers(&[(0x58, 0x90), (0x00, 0x00)]);
        let mut io = replay(&dead);
        let Ok(Verification::Rejected(reason)) = verify(&mut io, BASE, Chip::It8686E) else {
            panic!("expected a rejection");
        };
        assert!(reason.contains("bit 4 set | received: 0x00"), "{reason}");
        let mut io = replay(&dead);
        assert_eq!(verify(&mut io, BASE, Chip::It8665E), Ok(Verification::Verified));
        let mut io = replay(&dead);
        assert_eq!(verify(&mut io, BASE, Chip::It8655E), Ok(Verification::Verified));
    }

    #[test]
    fn an_index_that_does_not_read_back_fails_verification() {
        let mut io = StaleIndexSuperIo(replay(&registers(&[(0x58, 0x90), (0x00, 0x1B)])));
        let Ok(Verification::Rejected(reason)) = verify(&mut io, BASE, Chip::It8686E) else {
            panic!("expected a rejection");
        };
        assert!(reason.contains("read back the vendor index"), "{reason}");
    }

    #[test]
    fn the_it8688e_is_exempt_from_the_index_read_back() {
        let mut io = StaleIndexSuperIo(replay(&registers(&[(0x58, 0x90), (0x00, 0x1B)])));
        assert_eq!(verify(&mut io, BASE, Chip::It8688E), Ok(Verification::Verified));
    }

    #[test]
    fn temperatures_need_a_value_between_1_and_126() {
        let chip = It87xx::new(Chip::It8686E, 0, BASE);
        let mut io = replay(&registers(&[
            (0x29, 40),
            (0x2A, 0),
            (0x2B, 127),
            (0x2C, 0x80),
            (0x2D, 126),
            (0x2E, 1),
            (0x2F, 0xFF),
        ]));
        let temperatures = chip.read_temperatures(&mut io).unwrap();
        assert_eq!(
            temperatures,
            [Some(40.0), None, None, None, Some(126.0), Some(1.0), None],
            "0, 127 and negative values are not readings"
        );
    }

    #[test]
    fn voltages_scale_by_the_chip_gain_and_drop_zero_and_clipped() {
        let chip = It87xx::new(Chip::It8686E, 0, BASE);
        let mut io = replay(&registers(&[(0x20, 100), (0x21, 0), (0x22, 0xFF), (0x2F, 255 - 1)]));
        let volts = chip.read_voltages(&mut io).unwrap();
        assert_eq!(volts.len(), 10);
        assert!((volts[0].unwrap() - 1.2).abs() < 1e-9);
        assert_eq!(volts[1], None);
        assert_eq!(volts[2], None);
        // Input 9 is register 0x2F, not 0x29.
        assert!((volts[9].unwrap() - 254.0 * 0.012).abs() < 1e-9);
    }

    #[test]
    fn sixteen_bit_fans_convert_the_count_and_flag_overflow_and_tiny_counts() {
        let chip = It87xx::new(Chip::It8686E, 0, BASE);
        let mut io = replay(&registers(&[
            // fan 1: count 0x0258 = 600 -> 1.35e6 / 1200 = 1125 rpm
            (0x0D, 0x58),
            (0x18, 0x02),
            // fan 2: overflow
            (0x0E, 0xFF),
            (0x19, 0xFF),
            // fan 3: count 0x3F, too small to be a tachometer value
            (0x0F, 0x3F),
            (0x1A, 0x00),
            // fan 4: count 0x40, the smallest valid
            (0x80, 0x40),
            (0x81, 0x00),
        ]));
        let fans = chip.read_fans(&mut io).unwrap();
        assert_eq!(fans[0], Some(1125.0));
        assert_eq!(fans[1], Some(0.0));
        assert_eq!(fans[2], None);
        assert_eq!(fans[3], Some(1_350_000.0 / 128.0));
    }

    #[test]
    fn fans_four_to_six_are_skipped_unless_enabled_for_16_bit_counting() {
        let mut chip = It87xx::new(Chip::It8686E, 0, BASE);
        // Bits 4 and 5 enable fans 4 and 5; bit 2 enables fan 6.
        let mut io = replay(&registers(&[(0x0C, 0b0011_0000)]));
        chip.read_fan_enables(&mut io).unwrap();
        assert_eq!(chip.fans_disabled, [false, false, false, false, false, true]);
        let mut io = replay(&registers(&[(0x0C, 0b0000_0100)]));
        chip.read_fan_enables(&mut io).unwrap();
        assert_eq!(chip.fans_disabled, [false, false, false, true, true, false]);
    }

    #[test]
    fn the_it8665e_reads_its_sixth_fan_enable_elsewhere() {
        let mut chip = It87xx::new(Chip::It8665E, 0, BASE);
        let mut io = replay(&registers(&[(0x0C, 0b0011_0100), (0x0B, 0b0000_1000)]));
        chip.read_fan_enables(&mut io).unwrap();
        assert!(!chip.fans_disabled[5], "bit 3 of 0x0B enables it");
        let mut io = replay(&registers(&[(0x0C, 0b0011_0100), (0x0B, 0b0000_0000)]));
        chip.read_fan_enables(&mut io).unwrap();
        assert!(chip.fans_disabled[5]);
    }

    #[test]
    fn the_alternate_sixth_tachometer_is_used_on_the_it8625e() {
        let chip = It87xx::new(Chip::It8625E, 0, BASE);
        let mut io = replay(&registers(&[(0x93, 0x58), (0x94, 0x02), (0x4C, 0x11), (0x4D, 0x11)]));
        assert_eq!(chip.read_fan_16_bit(&mut io, 5), Ok(Some(1125.0)));
    }

    #[test]
    fn eight_bit_fans_use_the_divisor_of_fans_one_and_two() {
        // IT8705F below version 3: 8-bit counters. Divisor register 0x0B:
        // fan 1 field 2 -> /4, fan 2 field 0 -> /1.
        let chip = It87xx::new(Chip::It8705F, 2, BASE);
        assert!(!chip.sixteen_bit_fans);
        let mut io =
            replay(&registers(&[(0x0B, 0b0000_0010), (0x0D, 100), (0x0E, 100), (0x0F, 100)]));
        let fans = chip.read_fans(&mut io).unwrap();
        assert_eq!(fans[0], Some(1_350_000.0 / 400.0));
        assert_eq!(fans[1], Some(1_350_000.0 / 100.0));
        assert_eq!(fans[2], Some(1_350_000.0 / 200.0), "fan 3 divides by 2");
    }

    #[test]
    fn an_eight_bit_count_of_zero_is_invalid_and_of_0xff_is_a_stopped_fan() {
        let chip = It87xx::new(Chip::It8705F, 2, BASE);
        let mut io = replay(&registers(&[(0x0B, 0), (0x0D, 0), (0x0E, 0xFF), (0x0F, 0)]));
        let fans = chip.read_fans(&mut io).unwrap();
        assert_eq!(fans, [None, Some(0.0), None]);
    }

    #[test]
    fn the_16_bit_counter_threshold_depends_on_the_chip_version() {
        assert!(!It87xx::new(Chip::It8705F, 2, BASE).sixteen_bit_fans);
        assert!(It87xx::new(Chip::It8705F, 3, BASE).sixteen_bit_fans);
        assert!(!It87xx::new(Chip::It8712F, 7, BASE).sixteen_bit_fans);
        assert!(It87xx::new(Chip::It8712F, 8, BASE).sixteen_bit_fans);
        assert!(It87xx::new(Chip::It8686E, 0, BASE).sixteen_bit_fans);
    }

    #[test]
    fn duty_in_automatic_mode_is_not_readable() {
        let chip = It87xx::new(Chip::It8686E, 0, BASE);
        // Control 1 in automatic mode (bit 7), control 2 manual with the
        // extended duty register at 0xFF -> 100 %, control 3 manual at 0x80.
        let mut io = replay(&registers(&[
            (0x15, 0x80),
            (0x63, 0x55),
            (0x16, 0x00),
            (0x6B, 0xFF),
            (0x17, 0x00),
            (0x73, 0x80),
        ]));
        let duty = chip.read_duty(&mut io).unwrap();
        assert_eq!(duty[0], None);
        assert_eq!(duty[1], Some(100.0));
        assert_eq!(duty[2], Some(50.0));
    }

    #[test]
    fn chips_without_the_extended_register_use_the_seven_bit_field() {
        let chip = It87xx::new(Chip::It8705F, 3, BASE);
        let mut io = replay(&registers(&[(0x15, 0x7F), (0x16, 0x40), (0x17, 0x00)]));
        let duty = chip.read_duty(&mut io).unwrap();
        assert_eq!(duty, [Some(100.0), Some(50.0), Some(0.0)]);
    }

    #[test]
    fn declared_channels_follow_what_reads_at_probe_and_read_matches_them() {
        let mut chip = It87xx::new(Chip::It8613E, 0, BASE);
        let mut io = replay(&registers(&[
            (0x29, 33),
            (0x2B, 45),
            (0x20, 120),
            (0x0C, 0xFF),
            (0x0D, 0x58),
            (0x18, 0x02),
            (0x63, 0xFF),
        ]));
        let channels = chip.discover(&mut io).unwrap();
        let keys: Vec<&str> = channels.iter().map(|c| c.key.as_str()).collect();
        // Temperatures 2 and 4 read 0 (no sensor); fans 2 to 5 have a count
        // of 0 (no tachometer); all four duty controls are in manual mode, so
        // they read, three of them as 0 %.
        assert_eq!(
            keys,
            [
                "board.temp.temp1",
                "board.temp.temp3",
                "board.vin0",
                "board.fan1",
                "board.pwm1",
                "board.pwm2",
                "board.pwm3",
                "board.pwm4",
            ]
        );
        let values = chip.read(&mut io).unwrap();
        assert_eq!(
            values,
            [
                Some(33.0),
                Some(45.0),
                Some(1.44),
                Some(1125.0),
                Some(100.0),
                Some(0.0),
                Some(0.0),
                Some(0.0)
            ]
        );
    }

    #[test]
    fn a_pass_writes_only_indices() {
        let mut chip = It87xx::new(Chip::It8686E, 0, BASE);
        let mut io = replay(&registers(&[(0x29, 33), (0x20, 100), (0x0C, 0xFF), (0x0D, 0x58)]));
        let log = io.write_log();
        log.lock().unwrap().clear();
        chip.discover(&mut io).unwrap();
        chip.read(&mut io).unwrap();
        for write in log.lock().unwrap().iter() {
            assert!(
                matches!(write, SuperIoWrite::HwmIndex { .. }),
                "(it87xx read): expected only index writes | received: {write:?}"
            );
        }
    }

    #[test]
    fn a_value_whose_index_did_not_read_back_is_none() {
        let chip = It87xx::new(Chip::It8686E, 0, BASE);
        let mut io = StaleIndexSuperIo(replay(&registers(&[(0x29, 40)])));
        assert!(chip.read_temperatures(&mut io).unwrap().iter().all(Option::is_none));
        // The same bus on an IT8688E, which cannot be checked, yields the value.
        let chip = It87xx::new(Chip::It8688E, 0, BASE);
        let mut io = StaleIndexSuperIo(replay(&registers(&[(0x29, 40)])));
        assert_eq!(chip.read_temperatures(&mut io).unwrap()[0], Some(40.0));
    }

    #[test]
    fn the_chip_names_itself() {
        assert_eq!(It87xx::new(Chip::It87952E, 0, BASE).name(), "IT87952E");
    }
}
