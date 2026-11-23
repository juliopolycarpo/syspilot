//! Nuvoton NCT6771F to NCT6799D hardware monitors.
//!
//! The monitor is a bank-switched register file behind an index/data port
//! pair at `base + 5` / `base + 6`. A 16-bit register address is
//! `bank << 8 | index`: the bank goes to index `0x4E`, then the index to the
//! address port and the value is read from the data port. Chips of one
//! family differ in where the registers are; the layouts below hold that.
//!
//! Temperatures are routed: each temperature register (a *slot*) shows the
//! source selected in its own source register (CPUTIN, SYSTIN, PECI agent 0,
//! ...), so a reading is keyed by the source the slot currently selects, not
//! by the slot. A source no slot selects can still have an integer-only
//! *alternate* register.

use syspilot_proto::sensor::SensorKind;

use crate::bus::{BusError, SuperIo};

use super::{Channel, MonitorChip};

/// Logical device of the hardware monitor.
pub const HWM_LOGICAL_DEVICE: u8 = 0x0B;
/// The vendor ID every Nuvoton hardware monitor reports.
pub const VENDOR_ID: u16 = 0x5CA3;

/// Index of the bank-select register.
const BANK_SELECT_INDEX: u8 = 0x4E;
/// Index of the vendor ID register; bank `0x80` reads its high byte.
const VENDOR_ID_INDEX: u8 = 0x4F;
const HIGH_BYTE_BANK: u8 = 0x80;
/// Fan duty registers hold 0..=255 for 0..=100 %.
const DUTY_COUNTS_PER_PERCENT: f64 = 2.55;
/// Source registers use the low five bits; the rest are flags.
const SOURCE_MASK: u8 = 0x1F;
/// Temperatures outside this range are the chip's way of saying "no sensor".
const PLAUSIBLE_TEMPERATURE: std::ops::RangeInclusive<f64> = -55.0..=125.0;
/// Fan tachometer constant: counts per minute of the 13-bit counter.
const FAN_COUNTS_PER_MINUTE: f64 = 1.35e6;
/// A 13-bit count at or above this means the fan is stopped or absent.
const FAN_COUNT_STOPPED: u16 = 0x1FFF;
/// A smaller count would not fit the 16-bit RPM registers.
const FAN_COUNT_MINIMUM: u16 = 0x15;
/// Raw value of a voltage input at the ADC's full scale: clipped, so the
/// real voltage is unknowable.
const VOLTAGE_CLIPPED: u8 = 0xFF;
/// Volts per count at the pin.
const VOLTS_PER_COUNT: f64 = 0.008;
/// Inputs with the chip's internal 2:1 divider (AVCC, 3VCC, 3VSB, VBAT):
/// the rail is twice the pin voltage.
const DIVIDED_INPUTS: [usize; 4] = [2, 3, 7, 8];
/// The battery input, valid only while its monitor is enabled.
const BATTERY_INPUT: usize = 8;
const BATTERY_MONITOR_REGISTER: u16 = 0x005D;

/// The chips this module reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Chip {
    /// NCT6771F.
    Nct6771F,
    /// NCT6776F.
    Nct6776F,
    /// NCT6779D.
    Nct6779D,
    /// NCT6791D.
    Nct6791D,
    /// NCT6792D.
    Nct6792D,
    /// NCT6792D-A.
    Nct6792DA,
    /// NCT6793D.
    Nct6793D,
    /// NCT6795D.
    Nct6795D,
    /// NCT6796D.
    Nct6796D,
    /// NCT6796D-R.
    Nct6796DR,
    /// NCT6797D.
    Nct6797D,
    /// NCT6798D.
    Nct6798D,
    /// NCT6799D.
    Nct6799D,
}

impl Chip {
    /// Every supported chip.
    pub const ALL: [Chip; 13] = [
        Chip::Nct6771F,
        Chip::Nct6776F,
        Chip::Nct6779D,
        Chip::Nct6791D,
        Chip::Nct6792D,
        Chip::Nct6792DA,
        Chip::Nct6793D,
        Chip::Nct6795D,
        Chip::Nct6796D,
        Chip::Nct6796DR,
        Chip::Nct6797D,
        Chip::Nct6798D,
        Chip::Nct6799D,
    ];

    /// The marketing name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Nct6771F => "NCT6771F",
            Self::Nct6776F => "NCT6776F",
            Self::Nct6779D => "NCT6779D",
            Self::Nct6791D => "NCT6791D",
            Self::Nct6792D => "NCT6792D",
            Self::Nct6792DA => "NCT6792D-A",
            Self::Nct6793D => "NCT6793D",
            Self::Nct6795D => "NCT6795D",
            Self::Nct6796D => "NCT6796D",
            Self::Nct6796DR => "NCT6796D-R",
            Self::Nct6797D => "NCT6797D",
            Self::Nct6798D => "NCT6798D",
            Self::Nct6799D => "NCT6799D",
        }
    }

    /// Whether the chip has the hardware-monitor I/O space lock (bit 4 of
    /// configuration register `0x28`) that a BIOS can set to hide the
    /// monitor from the OS. The NCT679x family has it; older chips do not.
    pub fn has_io_space_lock(self) -> bool {
        !matches!(self, Self::Nct6771F | Self::Nct6776F | Self::Nct6779D)
    }

    /// The chip with marketing name `name` (as [`Chip::name`] writes it).
    pub fn from_name(name: &str) -> Option<Chip> {
        Self::ALL.into_iter().find(|chip| chip.name() == name)
    }

    /// How many fan duty (PWM) outputs the chip has: the duty registers this
    /// module reads back.
    pub fn pwm_channels(self) -> usize {
        layout(self).duty.len()
    }

    /// The registers that read back each duty output's current value, the ones
    /// [`MonitorChip::read`] reports as duty (`REG_PWM_READ` in the Linux
    /// `nct6775` driver). Not the registers a fan write targets.
    #[cfg(feature = "fan-control")]
    pub(crate) fn duty_read_registers(self) -> &'static [u16] {
        layout(self).duty
    }

    /// How many fan tachometer inputs the chip has.
    pub fn fan_tachometers(self) -> usize {
        match layout(self).fans {
            Fans::Counter13(registers) => registers.len(),
            Fans::Rpm16 { registers, .. } => registers.len(),
        }
    }
}

/// The chip behind chip ID `id` and revision `revision` (configuration
/// registers `0x20` and `0x21`).
pub fn identify(id: u8, revision: u8) -> Option<Chip> {
    Some(match (id, revision) {
        (0xB4, r) if r & 0xF0 == 0x70 => Chip::Nct6771F,
        (0xC3, r) if r & 0xF0 == 0x30 => Chip::Nct6776F,
        (0xC5, r) if r & 0xF0 == 0x60 => Chip::Nct6779D,
        (0xC8, 0x03) => Chip::Nct6791D,
        (0xC9, 0x11) => Chip::Nct6792D,
        (0xC9, 0x13) => Chip::Nct6792DA,
        (0xD1, 0x21) => Chip::Nct6793D,
        (0xD3, 0x52) => Chip::Nct6795D,
        (0xD4, 0x23) => Chip::Nct6796D,
        (0xD4, 0x2A) => Chip::Nct6796DR,
        (0xD4, 0x51) => Chip::Nct6797D,
        (0xD4, 0x2B) => Chip::Nct6798D,
        (0xD8, 0x02) => Chip::Nct6799D,
        _ => return None,
    })
}

/// Nuvoton chips this module recognises but does not read: their monitor
/// lives in a different register space (an embedded controller).
pub fn recognised_but_unsupported(id: u8, revision: u8) -> Option<&'static str> {
    Some(match (id, revision) {
        (0xC4, r) if r & 0xF0 == 0x50 => "NCT610XD",
        (0xC7, 0x32) => "NCT6683D",
        (0xD4, 0x40 | 0x41) => "NCT6686D",
        (0xD5, 0x92) => "NCT6687D",
        (0xD8, 0x06) => "NCT6701D",
        _ => return None,
    })
}

/// Reads the vendor ID through the bank-select protocol: bank `0x80` for the
/// high byte of register `0x4F`, bank `0` for the low byte. Leaves bank `0`
/// selected, as the chip comes out of reset.
pub fn read_vendor_id(io: &mut dyn SuperIo, base: u16) -> Result<u16, BusError> {
    let mut byte = |bank: u8| -> Result<u8, BusError> {
        io.hwm_set_index(base, BANK_SELECT_INDEX)?;
        io.hwm_write_bank(base, bank)?;
        io.hwm_set_index(base, VENDOR_ID_INDEX)?;
        io.hwm_read_data(base)
    };
    let high = byte(HIGH_BYTE_BANK)?;
    let low = byte(0)?;
    Ok(u16::from_be_bytes([high, low]))
}

// ---------------------------------------------------------------------------
// Register layouts
// ---------------------------------------------------------------------------

/// Where a temperature slot's source comes from.
#[derive(Debug, Clone, Copy)]
enum Select {
    /// The slot shows whatever source this register selects.
    Register(u16),
    /// The slot always shows this source.
    Fixed(u8),
}

/// One temperature register and its extras.
#[derive(Debug, Clone, Copy)]
struct Slot {
    /// Integer degrees, two's complement.
    value: u16,
    /// Register and bit of the half-degree.
    half: Option<(u16, u8)>,
    select: Select,
}

/// A temperature source a layout reports.
#[derive(Debug, Clone, Copy)]
struct Named {
    source: u8,
    key: &'static str,
    label: &'static str,
    /// Integer-degree register that shows this source directly, used when
    /// no slot routes it.
    alternate: Option<u16>,
}

#[derive(Debug, Clone, Copy)]
enum Fans {
    /// 13-bit counters in `(high << 5) | (low & 0x1F)` form at `register`
    /// and `register + 1`.
    Counter13(&'static [u16]),
    /// 16-bit RPM registers (high, low); `minimum_rpm` and below is "stopped".
    Rpm16 { registers: &'static [u16], minimum_rpm: u16 },
}

#[derive(Debug, Clone, Copy)]
struct Layout {
    slots: &'static [Slot],
    named: &'static [Named],
    voltages: &'static [u16],
    fans: Fans,
    duty: &'static [u16],
}

const fn slot(value: u16, half: Option<(u16, u8)>, select: u16) -> Slot {
    Slot { value, half, select: Select::Register(select) }
}

const fn fixed_slot(value: u16, source: u8) -> Slot {
    Slot { value, half: None, select: Select::Fixed(source) }
}

const fn named(
    source: u8,
    key: &'static str,
    label: &'static str,
    alternate: Option<u16>,
) -> Named {
    Named { source, key, label, alternate }
}

const SYSTIN: u8 = 1;
const CPUTIN: u8 = 2;

const MODERN_VOLTAGES: [u16; 16] = [
    0x480, 0x481, 0x482, 0x483, 0x484, 0x485, 0x486, 0x487, 0x488, 0x489, 0x48A, 0x48B, 0x48C,
    0x48D, 0x48E, 0x48F,
];
const LEGACY_VOLTAGES: [u16; 9] = [0x020, 0x021, 0x022, 0x023, 0x024, 0x025, 0x026, 0x550, 0x551];
// Statics, not consts: the layouts borrow prefixes of these, which a const
// cannot lend for `'static`.
static MODERN_FAN_COUNTERS: [u16; 7] = [0x4B0, 0x4B2, 0x4B4, 0x4B6, 0x4B8, 0x4BA, 0x4CC];
static LEGACY_FAN_RPM: [u16; 5] = [0x656, 0x658, 0x65A, 0x65C, 0x65E];
static DUTY_STANDARD: [u16; 7] = [0x001, 0x003, 0x011, 0x013, 0x015, 0x017, 0x029];
/// NCT6797D and later moved the sixth and seventh duty registers.
static DUTY_RELOCATED: [u16; 7] = [0x001, 0x003, 0x011, 0x013, 0x015, 0xA09, 0xB09];

/// NCT6771F and NCT6776F. The PECI source number differs between the two.
const fn legacy_named(peci_source: u8) -> [Named; 4] {
    [
        named(peci_source, "peci0", "PECI agent 0", None),
        named(CPUTIN, "cputin", "CPUTIN", None),
        named(3, "auxtin", "AUXTIN", None),
        named(SYSTIN, "systin", "SYSTIN", None),
    ]
}
const LEGACY_NAMED_6771: [Named; 4] = legacy_named(5);
const LEGACY_NAMED_6776: [Named; 4] = legacy_named(12);
const LEGACY_SLOTS: [Slot; 9] = [
    slot(0x027, None, 0x621),
    slot(0x073, Some((0x074, 7)), 0x100),
    slot(0x075, Some((0x076, 7)), 0x200),
    slot(0x077, Some((0x078, 7)), 0x300),
    slot(0x150, Some((0x151, 7)), 0x622),
    slot(0x250, Some((0x251, 7)), 0x623),
    slot(0x62B, None, 0x624),
    slot(0x62C, None, 0x625),
    slot(0x62D, None, 0x626),
];

/// NCT6779D.
const NCT6779D_SLOTS: [Slot; 7] = [
    slot(0x027, None, 0x621),
    slot(0x073, Some((0x074, 7)), 0x100),
    slot(0x075, Some((0x076, 7)), 0x200),
    slot(0x077, Some((0x078, 7)), 0x300),
    slot(0x079, Some((0x07A, 7)), 0x800),
    slot(0x07B, Some((0x07C, 7)), 0x900),
    slot(0x150, Some((0x151, 7)), 0x622),
];
const NCT6779D_NAMED: [Named; 7] = [
    named(16, "peci0", "PECI agent 0", None),
    named(CPUTIN, "cputin", "CPUTIN", Some(0x491)),
    named(SYSTIN, "systin", "SYSTIN", Some(0x490)),
    named(3, "auxtin0", "AUXTIN0", Some(0x492)),
    named(4, "auxtin1", "AUXTIN1", Some(0x493)),
    named(5, "auxtin2", "AUXTIN2", Some(0x494)),
    named(6, "auxtin3", "AUXTIN3", Some(0x495)),
];

/// NCT6791D to NCT6799D.
const NCT679X_SLOTS: [Slot; 19] = [
    slot(0x073, Some((0x074, 7)), 0x100),
    slot(0x075, Some((0x076, 7)), 0x200),
    slot(0x077, Some((0x078, 7)), 0x300),
    slot(0x079, Some((0x07A, 7)), 0x800),
    slot(0x07B, Some((0x07C, 7)), 0x900),
    slot(0x07D, Some((0x07E, 7)), 0xA00),
    slot(0x4A0, Some((0x49E, 6)), 0xB00),
    slot(0x027, None, 0x621),
    slot(0x150, Some((0x151, 7)), 0x622),
    slot(0x670, None, 0xC26),
    slot(0x672, None, 0xC27),
    slot(0x674, None, 0xC28),
    slot(0x676, None, 0xC29),
    slot(0x678, None, 0xC2A),
    slot(0x67A, None, 0xC2B),
    fixed_slot(0x405, 22),
    fixed_slot(0x406, 23),
    fixed_slot(0x407, 24),
    fixed_slot(0x408, 25),
];
/// NCT6798D and NCT6799D add the TSENSOR input.
const NCT679X_D_SLOTS: [Slot; 20] = {
    let mut slots = [fixed_slot(0, 0); 20];
    let mut index = 0;
    while index < NCT679X_SLOTS.len() {
        slots[index] = NCT679X_SLOTS[index];
        index += 1;
    }
    slots[19] = slot(0x4A2, Some((0x4A1, 7)), 0xC00);
    slots
};
const NCT679X_NAMED: [Named; 18] = [
    named(16, "peci0", "PECI agent 0", None),
    named(CPUTIN, "cputin", "CPUTIN", Some(0x491)),
    named(SYSTIN, "systin", "SYSTIN", Some(0x490)),
    named(3, "auxtin0", "AUXTIN0", Some(0x492)),
    named(4, "auxtin1", "AUXTIN1", Some(0x493)),
    named(5, "auxtin2", "AUXTIN2", Some(0x494)),
    named(6, "auxtin3", "AUXTIN3", Some(0x495)),
    named(7, "auxtin4", "AUXTIN4", None),
    named(8, "smbusmaster0", "SMBus master 0", None),
    named(9, "smbusmaster1", "SMBus master 1", None),
    named(17, "peci1", "PECI agent 1", None),
    named(18, "pch-cpu-max", "PCH CPU max", Some(0x400)),
    named(19, "pch-chip", "PCH chip", Some(0x401)),
    named(20, "pch-cpu", "PCH CPU", Some(0x402)),
    named(21, "pch-mch", "PCH MCH", Some(0x404)),
    named(22, "agent0-dimm0", "Agent 0 DIMM 0", None),
    named(23, "agent0-dimm1", "Agent 0 DIMM 1", None),
    named(24, "agent1-dimm0", "Agent 1 DIMM 0", None),
];
const NCT679X_D_NAMED: [Named; 19] = {
    let mut list = [named(0, "", "", None); 19];
    let mut index = 0;
    while index < NCT679X_NAMED.len() {
        list[index] = NCT679X_NAMED[index];
        index += 1;
    }
    list[18] = named(10, "tsensor", "TSENSOR", Some(0x496));
    list
};

fn layout(chip: Chip) -> Layout {
    match chip {
        Chip::Nct6771F => Layout {
            slots: &LEGACY_SLOTS,
            named: &LEGACY_NAMED_6771,
            voltages: &LEGACY_VOLTAGES,
            fans: Fans::Rpm16 { registers: &LEGACY_FAN_RPM[..4], minimum_rpm: 20 },
            duty: &DUTY_STANDARD[..3],
        },
        Chip::Nct6776F => Layout {
            slots: &LEGACY_SLOTS,
            named: &LEGACY_NAMED_6776,
            voltages: &LEGACY_VOLTAGES,
            fans: Fans::Rpm16 { registers: &LEGACY_FAN_RPM, minimum_rpm: 164 },
            duty: &DUTY_STANDARD[..3],
        },
        Chip::Nct6779D => Layout {
            slots: &NCT6779D_SLOTS,
            named: &NCT6779D_NAMED,
            voltages: &MODERN_VOLTAGES,
            fans: Fans::Counter13(&MODERN_FAN_COUNTERS[..5]),
            duty: &DUTY_STANDARD[..5],
        },
        Chip::Nct6791D
        | Chip::Nct6792D
        | Chip::Nct6792DA
        | Chip::Nct6793D
        | Chip::Nct6795D
        | Chip::Nct6796D => Layout {
            slots: &NCT679X_SLOTS,
            named: &NCT679X_NAMED,
            voltages: &MODERN_VOLTAGES,
            fans: Fans::Counter13(&MODERN_FAN_COUNTERS[..6]),
            duty: &DUTY_STANDARD[..6],
        },
        Chip::Nct6796DR => Layout {
            slots: &NCT679X_SLOTS,
            named: &NCT679X_NAMED,
            voltages: &MODERN_VOLTAGES,
            fans: Fans::Counter13(&MODERN_FAN_COUNTERS),
            duty: &DUTY_STANDARD,
        },
        Chip::Nct6797D => Layout {
            slots: &NCT679X_SLOTS,
            named: &NCT679X_NAMED,
            voltages: &MODERN_VOLTAGES,
            fans: Fans::Counter13(&MODERN_FAN_COUNTERS),
            duty: &DUTY_RELOCATED,
        },
        Chip::Nct6798D | Chip::Nct6799D => Layout {
            slots: &NCT679X_D_SLOTS,
            named: &NCT679X_D_NAMED,
            voltages: &MODERN_VOLTAGES,
            fans: Fans::Counter13(&MODERN_FAN_COUNTERS),
            duty: &DUTY_RELOCATED,
        },
    }
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// What one pass over the registers produced.
struct Snapshot {
    /// Indexed by source number.
    temperatures: [Option<f64>; 32],
    voltages: Vec<Option<f64>>,
    fans: Vec<Option<f64>>,
    duty: Vec<Option<f64>>,
}

/// What a declared channel reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Temperature(u8),
    Voltage(usize),
    Fan(usize),
    Duty(usize),
}

/// A detected Nuvoton chip.
#[derive(Debug)]
pub struct Nct67xx {
    chip: Chip,
    base: u16,
    layout: Layout,
    declared: Vec<Source>,
    /// The bank last selected during the current pass; `None` before the
    /// first read of a pass.
    bank: Option<u8>,
}

impl Nct67xx {
    /// A reader for `chip` whose monitor ports start at `base`.
    pub fn new(chip: Chip, base: u16) -> Self {
        Self { chip, base, layout: layout(chip), declared: Vec::new(), bank: None }
    }

    /// Reads one byte at the 16-bit `address`, selecting its bank first
    /// when it is not the bank the previous read used. The caller holds the
    /// ISA bus guard for the whole pass, so the selection stays valid.
    fn read_register(&mut self, io: &mut dyn SuperIo, address: u16) -> Result<u8, BusError> {
        let bank = (address >> 8) as u8;
        if self.bank != Some(bank) {
            io.hwm_set_index(self.base, BANK_SELECT_INDEX)?;
            io.hwm_write_bank(self.base, bank)?;
            self.bank = Some(bank);
        }
        io.hwm_set_index(self.base, address as u8)?;
        io.hwm_read_data(self.base)
    }

    fn read_temperatures(&mut self, io: &mut dyn SuperIo) -> Result<[Option<f64>; 32], BusError> {
        let mut found = [None; 32];
        let layout = self.layout;
        for slot in layout.slots {
            let source = match slot.select {
                Select::Register(register) => self.read_register(io, register)? & SOURCE_MASK,
                Select::Fixed(source) => source,
            };
            // The first slot with a plausible reading wins; slots routed
            // to sources this layout reports nothing for are not read.
            let wanted = layout.named.iter().any(|entry| entry.source == source);
            if !wanted || found[usize::from(source)].is_some() {
                continue;
            }
            let integer = self.read_register(io, slot.value)? as i8;
            let half = match slot.half {
                Some((register, bit)) => (self.read_register(io, register)? >> bit) & 1,
                None => 0,
            };
            let temperature = f64::from((i16::from(integer) << 1) | i16::from(half)) / 2.0;
            if PLAUSIBLE_TEMPERATURE.contains(&temperature) {
                found[usize::from(source)] = Some(temperature);
            }
        }
        for entry in layout.named {
            let Some(register) = entry.alternate else { continue };
            if found[usize::from(entry.source)].is_some() {
                continue;
            }
            // Integer degrees only; 0 and below mean "not connected".
            let degrees = self.read_register(io, register)? as i8;
            if (1..=125).contains(&degrees) {
                found[usize::from(entry.source)] = Some(f64::from(degrees));
            }
        }
        Ok(found)
    }

    fn read_voltages(&mut self, io: &mut dyn SuperIo) -> Result<Vec<Option<f64>>, BusError> {
        let layout = self.layout;
        let mut values = Vec::with_capacity(layout.voltages.len());
        for (index, &register) in layout.voltages.iter().enumerate() {
            let raw = self.read_register(io, register)?;
            let mut value =
                (raw != 0 && raw != VOLTAGE_CLIPPED).then(|| f64::from(raw) * VOLTS_PER_COUNT);
            if index == BATTERY_INPUT {
                let monitored = self.read_register(io, BATTERY_MONITOR_REGISTER)? & 1 != 0;
                if !monitored {
                    value = None;
                }
            }
            if DIVIDED_INPUTS.contains(&index) {
                value = value.map(|volts| volts * 2.0);
            }
            values.push(value);
        }
        Ok(values)
    }

    fn read_fans(&mut self, io: &mut dyn SuperIo) -> Result<Vec<Option<f64>>, BusError> {
        let layout = self.layout;
        let mut values = Vec::new();
        match layout.fans {
            Fans::Counter13(registers) => {
                for &register in registers {
                    let high = u16::from(self.read_register(io, register)?);
                    let low = u16::from(self.read_register(io, register + 1)?);
                    let count = (high << 5) | (low & 0x1F);
                    values.push(if count >= FAN_COUNT_STOPPED {
                        Some(0.0)
                    } else if count >= FAN_COUNT_MINIMUM {
                        Some(FAN_COUNTS_PER_MINUTE / f64::from(count))
                    } else {
                        None
                    });
                }
            }
            Fans::Rpm16 { registers, minimum_rpm } => {
                for &register in registers {
                    let high = u16::from(self.read_register(io, register)?);
                    let low = u16::from(self.read_register(io, register + 1)?);
                    let rpm = (high << 8) | low;
                    values.push(Some(if rpm > minimum_rpm { f64::from(rpm) } else { 0.0 }));
                }
            }
        }
        Ok(values)
    }

    fn read_duty(&mut self, io: &mut dyn SuperIo) -> Result<Vec<Option<f64>>, BusError> {
        let layout = self.layout;
        let mut values = Vec::with_capacity(layout.duty.len());
        for &register in layout.duty {
            let raw = self.read_register(io, register)?;
            values.push(Some(f64::from(raw) / DUTY_COUNTS_PER_PERCENT));
        }
        Ok(values)
    }

    fn read_all(&mut self, io: &mut dyn SuperIo) -> Result<Snapshot, BusError> {
        Ok(Snapshot {
            temperatures: self.read_temperatures(io)?,
            voltages: self.read_voltages(io)?,
            fans: self.read_fans(io)?,
            duty: self.read_duty(io)?,
        })
    }

    fn snapshot(&mut self, io: &mut dyn SuperIo) -> Result<Snapshot, BusError> {
        // Whatever bank the last pass or another program left is unknown.
        self.bank = None;
        let result = self.read_all(io);
        // Leave bank 0 selected, the state after reset, even when a read failed.
        let restored = if self.bank.is_some_and(|bank| bank != 0) {
            io.hwm_set_index(self.base, BANK_SELECT_INDEX)
                .and_then(|()| io.hwm_write_bank(self.base, 0))
        } else {
            Ok(())
        };
        self.bank = None;
        let snapshot = result?;
        restored?;
        Ok(snapshot)
    }

    fn value_of(&self, snapshot: &Snapshot, source: Source) -> Option<f64> {
        match source {
            Source::Temperature(number) => snapshot.temperatures[usize::from(number)],
            Source::Voltage(index) => snapshot.voltages.get(index).copied().flatten(),
            Source::Fan(index) => snapshot.fans.get(index).copied().flatten(),
            Source::Duty(index) => snapshot.duty.get(index).copied().flatten(),
        }
    }

    /// The label of voltage input `index`.
    fn voltage_label(index: usize) -> String {
        match index {
            2 => "VIN2 (AVCC)".to_owned(),
            3 => "VIN3 (3VCC)".to_owned(),
            7 => "VIN7 (3VSB)".to_owned(),
            8 => "VIN8 (VBAT)".to_owned(),
            other => format!("VIN{other}"),
        }
    }
}

impl MonitorChip for Nct67xx {
    fn name(&self) -> String {
        self.chip.name().to_owned()
    }

    fn discover(&mut self, io: &mut dyn SuperIo) -> Result<Vec<Channel>, BusError> {
        let snapshot = self.snapshot(io)?;
        let mut declared = Vec::new();
        let mut channels = Vec::new();
        for entry in self.layout.named {
            if snapshot.temperatures[usize::from(entry.source)].is_some() {
                declared.push(Source::Temperature(entry.source));
                channels.push(Channel::new(
                    format!("board.temp.{}", entry.key),
                    entry.label,
                    SensorKind::Temperature,
                ));
            }
        }
        for (index, value) in snapshot.voltages.iter().enumerate() {
            if value.is_some() {
                declared.push(Source::Voltage(index));
                channels.push(Channel::new(
                    format!("board.vin{index}"),
                    Self::voltage_label(index),
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
        Ok(self.declared.iter().map(|&source| self.value_of(&snapshot, source)).collect())
    }
}

/// The registers the reference machine (NCT6779D) held when recorded: every
/// register one read pass touches.
#[cfg(test)]
pub(crate) fn reference_registers() -> Vec<(u16, u8)> {
    let mut registers = vec![
        // slot value, half, source select
        (0x027, 0x30),
        (0x621, 0x01),
        (0x073, 0x00),
        (0x074, 0x00),
        (0x100, 0x11),
        (0x075, 0x22),
        (0x076, 0x00),
        (0x200, 0x10),
        (0x077, 0xC5),
        (0x078, 0x00),
        (0x300, 0x03),
        (0x079, 0xC5),
        (0x07A, 0x00),
        (0x800, 0x03),
        (0x07B, 0xC5),
        (0x07C, 0x00),
        (0x900, 0x03),
        (0x150, 0x22),
        (0x151, 0x00),
        (0x622, 0x10),
        // alternates
        (0x490, 0x30),
        (0x491, 0x83),
        (0x492, 0xC5),
        (0x493, 0x68),
        (0x494, 0x6D),
        (0x495, 0x18),
        // battery monitor enable
        (0x05D, 0x47),
        // fans: stopped, 621 rpm, stopped, stopped, stopped
        (0x4B0, 0xFF),
        (0x4B1, 0x1F),
        (0x4B2, 0x43),
        (0x4B3, 0x1C),
        (0x4B4, 0xFF),
        (0x4B5, 0x1F),
        (0x4B6, 0xFF),
        (0x4B7, 0x1F),
        (0x4B8, 0xFF),
        (0x4B9, 0x1F),
        // duty
        (0x001, 0xFF),
        (0x003, 0x11),
        (0x011, 0x7F),
        (0x013, 0xFF),
        (0x015, 0xFF),
    ];
    let volts = [
        0x27, 0x00, 0xCE, 0xCE, 0x00, 0x00, 0xFD, 0xD2, 0xCD, 0x84, 0x12, 0x10, 0xFF, 0x9A, 0x15,
        0x00,
    ];
    registers.extend(volts.iter().enumerate().map(|(i, &v)| (0x480 + i as u16, v)));
    registers
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::bus::replay::{ReplaySuperIo, SuperIoWrite, superio_hwm_key};
    use crate::bus::{Fixture, SuperIoSlot, Unlock, is_permitted_bank};

    const BASE: u16 = 0x0A20;

    /// A replay holding `registers` (16-bit address, value) in the monitor,
    /// with the I/O range discovered as the driver requires.
    fn replay(registers: &[(u16, u8)]) -> ReplaySuperIo {
        let mut fixture = Fixture::new();
        for &(address, value) in registers {
            fixture.set(
                superio_hwm_key(SuperIoSlot::Primary, (address >> 8) as u8, address as u8),
                u64::from(value),
            );
        }
        let mut io = ReplaySuperIo::new(fixture);
        io.select_slot(SuperIoSlot::Primary).unwrap();
        io.enter_config_mode(Unlock::WinbondNuvoton).unwrap();
        io.discover_io_ranges().unwrap();
        io.exit_config_mode(Unlock::WinbondNuvoton).unwrap();
        io
    }

    fn discover_reference() -> (Nct67xx, ReplaySuperIo, Vec<Channel>) {
        let mut io = replay(&reference_registers());
        let mut chip = Nct67xx::new(Chip::Nct6779D, BASE);
        let channels = chip.discover(&mut io).unwrap();
        (chip, io, channels)
    }

    fn keys(channels: &[Channel]) -> Vec<&str> {
        channels.iter().map(|c| c.key.as_str()).collect()
    }

    #[test]
    fn identify_maps_every_published_id_and_revision() {
        assert_eq!(identify(0xC5, 0x62), Some(Chip::Nct6779D), "revision low nibble is ignored");
        assert_eq!(identify(0xB4, 0x70), Some(Chip::Nct6771F));
        assert_eq!(identify(0xC3, 0x3F), Some(Chip::Nct6776F));
        assert_eq!(identify(0xC8, 0x03), Some(Chip::Nct6791D));
        assert_eq!(identify(0xC9, 0x11), Some(Chip::Nct6792D));
        assert_eq!(identify(0xC9, 0x13), Some(Chip::Nct6792DA));
        assert_eq!(identify(0xD1, 0x21), Some(Chip::Nct6793D));
        assert_eq!(identify(0xD3, 0x52), Some(Chip::Nct6795D));
        assert_eq!(identify(0xD4, 0x23), Some(Chip::Nct6796D));
        assert_eq!(identify(0xD4, 0x2A), Some(Chip::Nct6796DR));
        assert_eq!(identify(0xD4, 0x51), Some(Chip::Nct6797D));
        assert_eq!(identify(0xD4, 0x2B), Some(Chip::Nct6798D));
        assert_eq!(identify(0xD8, 0x02), Some(Chip::Nct6799D));
    }

    #[test]
    fn identify_refuses_a_known_id_with_another_revision() {
        assert_eq!(identify(0xC8, 0x04), None, "NCT6791D needs revision exactly 0x03");
        assert_eq!(identify(0xC5, 0x50), None, "NCT6779D needs revision 0x6x");
        assert_eq!(identify(0xFF, 0xFF), None);
    }

    #[test]
    fn a_chip_is_found_again_by_its_marketing_name() {
        for chip in Chip::ALL {
            assert_eq!(Chip::from_name(chip.name()), Some(chip));
        }
        assert_eq!(Chip::from_name("NCT6779d"), None, "names are matched exactly");
        assert_eq!(Chip::from_name(""), None);
    }

    #[test]
    fn the_chip_reports_how_many_pwm_outputs_and_tachometers_it_has() {
        assert_eq!((Chip::Nct6776F.pwm_channels(), Chip::Nct6776F.fan_tachometers()), (3, 5));
        assert_eq!((Chip::Nct6779D.pwm_channels(), Chip::Nct6779D.fan_tachometers()), (5, 5));
        assert_eq!((Chip::Nct6791D.pwm_channels(), Chip::Nct6791D.fan_tachometers()), (6, 6));
        assert_eq!((Chip::Nct6798D.pwm_channels(), Chip::Nct6798D.fan_tachometers()), (7, 7));
    }

    #[test]
    fn embedded_controller_chips_are_recognised_but_not_read() {
        assert_eq!(recognised_but_unsupported(0xD5, 0x92), Some("NCT6687D"));
        assert_eq!(recognised_but_unsupported(0xD4, 0x41), Some("NCT6686D"));
        assert_eq!(recognised_but_unsupported(0xC5, 0x62), None);
        for (id, revision) in [(0xD5u8, 0x92u8), (0xD4, 0x40), (0xC7, 0x32), (0xD8, 0x06)] {
            assert_eq!(identify(id, revision), None, "{id:#x}/{revision:#x} must not be read");
        }
    }

    #[test]
    fn only_the_679x_family_has_the_io_space_lock() {
        let locked: Vec<_> =
            Chip::ALL.iter().filter(|c| c.has_io_space_lock()).map(|c| c.name()).collect();
        assert_eq!(locked.len(), 10, "{locked:?}");
        assert!(!locked.contains(&"NCT6779D"));
        assert!(locked.contains(&"NCT6791D"));
    }

    #[test]
    fn the_vendor_id_is_read_in_two_banks_and_bank_zero_is_left_selected() {
        let mut io = replay(&[(0x804F, 0x5C), (0x004F, 0xA3)]);
        let log = io.write_log();
        assert_eq!(read_vendor_id(&mut io, BASE), Ok(VENDOR_ID));
        let writes = log.lock().unwrap().clone();
        let banks: Vec<u8> = writes
            .iter()
            .filter_map(|w| match w {
                SuperIoWrite::HwmBank { bank, .. } => Some(*bank),
                _ => None,
            })
            .collect();
        assert_eq!(banks, [0x80, 0x00]);
    }

    #[test]
    fn a_wrong_vendor_id_is_reported_as_read() {
        let mut io = replay(&[(0x804F, 0xFF), (0x004F, 0xFF)]);
        assert_eq!(read_vendor_id(&mut io, BASE), Ok(0xFFFF));
    }

    #[test]
    fn the_reference_machine_declares_what_its_registers_support() {
        let (_, _, channels) = discover_reference();
        assert_eq!(
            keys(&channels),
            [
                "board.temp.peci0",
                "board.temp.systin",
                "board.temp.auxtin1",
                "board.temp.auxtin2",
                "board.temp.auxtin3",
                "board.vin0",
                "board.vin2",
                "board.vin3",
                "board.vin6",
                "board.vin7",
                "board.vin8",
                "board.vin9",
                "board.vin10",
                "board.vin11",
                "board.vin13",
                "board.vin14",
                "board.fan1",
                "board.fan2",
                "board.fan3",
                "board.fan4",
                "board.fan5",
                "board.pwm1",
                "board.pwm2",
                "board.pwm3",
                "board.pwm4",
                "board.pwm5",
            ]
        );
    }

    #[test]
    fn the_reference_machine_values_decode_as_documented() {
        let (mut chip, mut io, channels) = discover_reference();
        let values = chip.read(&mut io).unwrap();
        let by_key = |key: &str| {
            let index = channels.iter().position(|c| c.key == key).expect(key);
            values[index].unwrap_or_else(|| panic!("{key}: expected a value | received: None"))
        };
        // PECI agent 0 is routed through 0x075 (source 0x10).
        assert_eq!(by_key("board.temp.peci0"), 34.0);
        // SYSTIN is routed through 0x027 (source 0x01).
        assert_eq!(by_key("board.temp.systin"), 48.0);
        // No slot routes AUXTIN1; its alternate register is used.
        assert_eq!(by_key("board.temp.auxtin1"), 104.0);
        assert_eq!(by_key("board.temp.auxtin3"), 24.0);
        // AVCC and the 3.3 V rails sit behind the internal 2:1 divider.
        assert!((by_key("board.vin2") - 3.296).abs() < 1e-9);
        assert!((by_key("board.vin3") - 3.296).abs() < 1e-9);
        assert!((by_key("board.vin7") - 3.36).abs() < 1e-9);
        assert!((by_key("board.vin8") - 3.28).abs() < 1e-9);
        // An input with no divider is the pin voltage.
        assert!((by_key("board.vin0") - 0.312).abs() < 1e-9);
        // 13-bit counter 0x43 << 5 | 0x1C = 2172 -> 1.35e6 / 2172 rpm.
        assert!((by_key("board.fan2") - 1_350_000.0 / 2172.0).abs() < 1e-9);
        assert_eq!(by_key("board.fan1"), 0.0, "count 0x1FFF is a stopped fan");
        assert!((by_key("board.pwm2") - 17.0 / 2.55).abs() < 1e-9);
        assert_eq!(by_key("board.pwm1"), 100.0);
    }

    #[test]
    fn a_pass_leaves_bank_zero_selected() {
        let (mut chip, mut io, _) = discover_reference();
        let log = io.write_log();
        log.lock().unwrap().clear();
        chip.read(&mut io).unwrap();
        let writes = log.lock().unwrap().clone();
        let last_bank = writes.iter().rev().find_map(|w| match w {
            SuperIoWrite::HwmBank { bank, .. } => Some(*bank),
            _ => None,
        });
        assert_eq!(last_bank, Some(0));
    }

    #[test]
    fn a_pass_writes_only_indices_and_banks() {
        let (mut chip, mut io, _) = discover_reference();
        let log = io.write_log();
        log.lock().unwrap().clear();
        chip.read(&mut io).unwrap();
        for write in log.lock().unwrap().iter() {
            assert!(
                matches!(write, SuperIoWrite::HwmIndex { .. } | SuperIoWrite::HwmBank { .. }),
                "(nct67xx read): expected only index and bank writes | received: {write:?}"
            );
            if let SuperIoWrite::HwmBank { bank, .. } = write {
                assert!(is_permitted_bank(*bank), "bank {bank:#x}");
            }
        }
    }

    #[test]
    fn the_bank_is_selected_again_only_when_it_changes() {
        let mut io = replay(&[(0x073, 0x20), (0x074, 0x00), (0x075, 0x00)]);
        let log = io.write_log();
        log.lock().unwrap().clear();
        let mut chip = Nct67xx::new(Chip::Nct6779D, BASE);
        // Two reads in bank 0, then bank 1.
        chip.bank = None;
        assert_eq!(chip.read_register(&mut io, 0x073), Ok(0x20));
        assert_eq!(chip.read_register(&mut io, 0x075), Ok(0x00));
        let banks = log
            .lock()
            .unwrap()
            .iter()
            .filter(|w| matches!(w, SuperIoWrite::HwmBank { .. }))
            .count();
        assert_eq!(banks, 1);
    }

    /// NCT6779D temperatures with every slot unrouted and every alternate
    /// empty, then `overrides` (16-bit address, value) applied on top.
    fn temperatures_of(overrides: &[(u16, u8)]) -> [Option<f64>; 32] {
        let mut registers: Vec<(u16, u8)> = [
            // slot values and half-degree registers
            0x027, 0x073, 0x074, 0x075, 0x076, 0x077, 0x078, 0x079, 0x07A, 0x07B, 0x07C, 0x150,
            0x151, // source selects
            0x621, 0x100, 0x200, 0x300, 0x800, 0x900, 0x622, // alternates
            0x490, 0x491, 0x492, 0x493, 0x494, 0x495,
        ]
        .iter()
        .map(|&address| (address, 0u8))
        .collect();
        registers.retain(|(address, _)| !overrides.iter().any(|(set, _)| set == address));
        registers.extend_from_slice(overrides);
        let mut io = replay(&registers);
        Nct67xx::new(Chip::Nct6779D, BASE).read_temperatures(&mut io).unwrap()
    }

    #[test]
    fn half_degrees_and_negative_temperatures_decode_from_two_registers() {
        // CPUTIN (source 2) is routed through slot 0x079 by select 0x800.
        // The slot holds -3 degrees and bit 7 of 0x07A adds 0.5: -2.5.
        let found = temperatures_of(&[(0x800, 0x02), (0x079, 0xFD), (0x07A, 0x80)]);
        assert_eq!(found[usize::from(CPUTIN)], Some(-2.5));
    }

    #[test]
    fn a_clear_half_bit_adds_nothing() {
        let found = temperatures_of(&[(0x800, 0x02), (0x079, 0x2A), (0x07A, 0x7F)]);
        assert_eq!(found[usize::from(CPUTIN)], Some(42.0));
    }

    #[test]
    fn a_temperature_outside_minus_55_to_125_is_not_a_reading() {
        // 0xC8 is -56 degrees, below the range; 0x7E is 126, above it.
        for raw in [0xC8u8, 0x7E] {
            let found = temperatures_of(&[(0x621, 0x01), (0x027, raw)]);
            assert_eq!(found[usize::from(SYSTIN)], None, "raw {raw:#x}");
        }
    }

    #[test]
    fn the_range_edges_are_readings() {
        for (raw, expected) in [(0xC9u8, -55.0), (0x7D, 125.0)] {
            let found = temperatures_of(&[(0x621, 0x01), (0x027, raw)]);
            assert_eq!(found[usize::from(SYSTIN)], Some(expected), "raw {raw:#x}");
        }
    }

    #[test]
    fn the_first_plausible_slot_wins_when_two_slots_select_one_source() {
        // Slots 0x075 and 0x150 both select PECI agent 0 (16); 0x075 comes
        // first and is implausible, so 0x150 supplies the value.
        let found = temperatures_of(&[(0x200, 0x10), (0x075, 0x80), (0x622, 0x10), (0x150, 0x21)]);
        assert_eq!(found[16], Some(33.0));
    }

    #[test]
    fn the_first_slot_is_kept_when_both_are_plausible() {
        let found = temperatures_of(&[(0x200, 0x10), (0x075, 0x22), (0x622, 0x10), (0x150, 0x30)]);
        assert_eq!(found[16], Some(34.0));
    }

    #[test]
    fn an_alternate_register_must_hold_one_to_125_degrees() {
        for (raw, expected) in
            [(0x00u8, None), (0x01, Some(1.0)), (0x7D, Some(125.0)), (0x7E, None), (0x83, None)]
        {
            let found = temperatures_of(&[(0x491, raw)]);
            assert_eq!(found[usize::from(CPUTIN)], expected, "alternate {raw:#x}");
        }
    }

    #[test]
    fn a_routed_source_does_not_use_its_alternate_when_the_slot_value_is_valid() {
        // SYSTIN is routed through 0x027 at 48; its alternate says 99.
        let found = temperatures_of(&[(0x621, 0x01), (0x027, 0x30), (0x490, 99)]);
        assert_eq!(found[usize::from(SYSTIN)], Some(48.0));
    }

    #[test]
    fn a_routed_source_with_an_implausible_slot_value_falls_back_to_its_alternate() {
        let found = temperatures_of(&[(0x621, 0x01), (0x027, 0x80), (0x490, 40)]);
        assert_eq!(found[usize::from(SYSTIN)], Some(40.0));
    }

    #[test]
    fn the_source_flags_above_bit_four_are_ignored() {
        // Select value 0xE1 is source 1 with flag bits set.
        let found = temperatures_of(&[(0x621, 0xE1), (0x027, 0x30)]);
        assert_eq!(found[usize::from(SYSTIN)], Some(48.0));
    }

    #[test]
    fn a_source_the_layout_does_not_name_is_not_read() {
        // Source 17 (PECI agent 1) is routed through slot 0x073 and holds a
        // plausible 32 degrees, but NCT6779D has no sensor for it.
        let found = temperatures_of(&[(0x100, 0x11), (0x073, 0x20)]);
        assert!(found.iter().all(Option::is_none), "{found:?}");
    }

    fn fan_replay(registers: &[(u16, u8)]) -> (Nct67xx, ReplaySuperIo) {
        (Nct67xx::new(Chip::Nct6779D, BASE), replay(registers))
    }

    #[test]
    fn a_13_bit_count_converts_to_rpm_and_the_edges_are_handled() {
        // counts: 0x15 (smallest valid), 0x14 (too small), 0x1FFE, 0x1FFF.
        let (mut chip, mut io) = fan_replay(&[
            (0x4B0, 0x00),
            (0x4B1, 0x15),
            (0x4B2, 0x00),
            (0x4B3, 0x14),
            (0x4B4, 0xFF),
            (0x4B5, 0x1E),
            (0x4B6, 0xFF),
            (0x4B7, 0x1F),
            (0x4B8, 0x00),
            (0x4B9, 0x00),
        ]);
        let fans = chip.read_fans(&mut io).unwrap();
        assert_eq!(fans[0], Some(1_350_000.0 / 21.0));
        assert_eq!(fans[1], None, "a count below 0x15 cannot be a tachometer value");
        assert_eq!(fans[2], Some(1_350_000.0 / f64::from(0x1FFE)));
        assert_eq!(fans[3], Some(0.0));
        assert_eq!(fans[4], None, "count 0 is below the minimum");
    }

    #[test]
    fn the_low_byte_of_a_fan_count_keeps_only_five_bits() {
        let (mut chip, mut io) = fan_replay(&[
            (0x4B0, 0x01),
            (0x4B1, 0xE5),
            (0x4B2, 0),
            (0x4B3, 0),
            (0x4B4, 0),
            (0x4B5, 0),
            (0x4B6, 0),
            (0x4B7, 0),
            (0x4B8, 0),
            (0x4B9, 0),
        ]);
        // 0xE5 & 0x1F = 5; count = 1 << 5 | 5 = 37.
        assert_eq!(chip.read_fans(&mut io).unwrap()[0], Some(1_350_000.0 / 37.0));
    }

    #[test]
    fn legacy_chips_read_a_16_bit_rpm() {
        let mut io = replay(&[
            (0x656, 0x04),
            (0x657, 0xB0),
            (0x658, 0x00),
            (0x659, 0x10),
            (0x65A, 0x00),
            (0x65B, 0x00),
            (0x65C, 0x00),
            (0x65D, 0x00),
        ]);
        let mut chip = Nct67xx::new(Chip::Nct6771F, BASE);
        let fans = chip.read_fans(&mut io).unwrap();
        assert_eq!(fans.len(), 4, "NCT6771F has four fan inputs");
        assert_eq!(fans[0], Some(1200.0));
        assert_eq!(fans[1], Some(0.0), "16 rpm is below the 20 rpm floor");
    }

    #[test]
    fn nct6776f_has_five_fans_and_a_higher_rpm_floor() {
        let mut io = replay(&[
            (0x656, 0x00),
            (0x657, 0xA4),
            (0x658, 0x00),
            (0x659, 0xA5),
            (0x65A, 0),
            (0x65B, 0),
            (0x65C, 0),
            (0x65D, 0),
            (0x65E, 0),
            (0x65F, 0),
        ]);
        let mut chip = Nct67xx::new(Chip::Nct6776F, BASE);
        let fans = chip.read_fans(&mut io).unwrap();
        assert_eq!(fans.len(), 5);
        assert_eq!(fans[0], Some(0.0), "164 rpm is not above the floor");
        assert_eq!(fans[1], Some(165.0));
    }

    #[test]
    fn a_clipped_or_zero_voltage_input_is_not_a_reading() {
        let mut registers: Vec<(u16, u8)> = (0x480..=0x48F).map(|r| (r, 0u8)).collect();
        registers[1].1 = 0xFF;
        registers[4].1 = 0xFE;
        registers.push((0x05D, 0x01));
        let mut io = replay(&registers);
        let mut chip = Nct67xx::new(Chip::Nct6779D, BASE);
        let volts = chip.read_voltages(&mut io).unwrap();
        assert_eq!(volts[0], None, "zero");
        assert_eq!(volts[1], None, "full scale is clipped");
        assert!((volts[4].unwrap() - 0xFE as f64 * 0.008).abs() < 1e-9);
    }

    #[test]
    fn the_battery_input_needs_its_monitor_enabled() {
        for (enable, present) in [(0x00u8, false), (0x01, true), (0xFE, false)] {
            let mut registers: Vec<(u16, u8)> = (0x480..=0x48F).map(|r| (r, 0x10u8)).collect();
            registers.push((0x05D, enable));
            let mut io = replay(&registers);
            let mut chip = Nct67xx::new(Chip::Nct6779D, BASE);
            let volts = chip.read_voltages(&mut io).unwrap();
            assert_eq!(volts[8].is_some(), present, "monitor register {enable:#x}");
            assert!(volts[7].is_some(), "other inputs do not depend on it");
        }
    }

    #[test]
    fn duty_converts_counts_to_percent() {
        let mut io = replay(&[(0x001, 0), (0x003, 255), (0x011, 128), (0x013, 51), (0x015, 1)]);
        let mut chip = Nct67xx::new(Chip::Nct6779D, BASE);
        let duty = chip.read_duty(&mut io).unwrap();
        assert_eq!(duty[0], Some(0.0));
        assert_eq!(duty[1], Some(100.0));
        assert!((duty[2].unwrap() - 128.0 / 2.55).abs() < 1e-9);
        assert!((duty[3].unwrap() - 20.0).abs() < 1e-9);
    }

    #[test]
    fn channel_counts_follow_the_chip() {
        for (chip, fans, duty) in [
            (Chip::Nct6771F, 4, 3),
            (Chip::Nct6776F, 5, 3),
            (Chip::Nct6779D, 5, 5),
            (Chip::Nct6791D, 6, 6),
            (Chip::Nct6796D, 6, 6),
            (Chip::Nct6796DR, 7, 7),
            (Chip::Nct6797D, 7, 7),
            (Chip::Nct6799D, 7, 7),
        ] {
            let layout = layout(chip);
            let fan_count = match layout.fans {
                Fans::Counter13(registers) => registers.len(),
                Fans::Rpm16 { registers, .. } => registers.len(),
            };
            assert_eq!((fan_count, layout.duty.len()), (fans, duty), "{}", chip.name());
        }
    }

    #[test]
    fn the_sixth_and_seventh_duty_registers_move_from_nct6797d_on() {
        assert_eq!(layout(Chip::Nct6796DR).duty[5], 0x017);
        assert_eq!(layout(Chip::Nct6797D).duty[5], 0xA09);
        assert_eq!(layout(Chip::Nct6799D).duty[6], 0xB09);
    }

    #[test]
    fn only_the_newest_chips_have_the_tsensor_input() {
        let has = |chip| layout(chip).named.iter().any(|n| n.key == "tsensor");
        assert!(has(Chip::Nct6798D) && has(Chip::Nct6799D));
        assert!(!has(Chip::Nct6797D) && !has(Chip::Nct6779D));
        assert_eq!(layout(Chip::Nct6798D).slots.len(), 20);
        assert_eq!(layout(Chip::Nct6797D).slots.len(), 19);
    }

    #[test]
    fn every_register_of_every_layout_lies_in_a_permitted_bank() {
        for chip in Chip::ALL {
            let layout = layout(chip);
            let mut addresses: Vec<u16> = Vec::new();
            for slot in layout.slots {
                addresses.push(slot.value);
                if let Some((register, bit)) = slot.half {
                    addresses.push(register);
                    assert!(bit < 8, "{}: half bit {bit}", chip.name());
                }
                if let Select::Register(register) = slot.select {
                    addresses.push(register);
                }
            }
            addresses.extend(layout.named.iter().filter_map(|n| n.alternate));
            addresses.extend(layout.voltages);
            addresses.extend(layout.duty);
            match layout.fans {
                Fans::Counter13(registers) => {
                    addresses.extend(registers.iter().flat_map(|r| [*r, r + 1]))
                }
                Fans::Rpm16 { registers, .. } => {
                    addresses.extend(registers.iter().flat_map(|r| [*r, r + 1]))
                }
            }
            for address in addresses {
                assert!(
                    is_permitted_bank((address >> 8) as u8),
                    "({}): expected a register in banks 0..=0xf | received: {address:#x}",
                    chip.name()
                );
            }
        }
    }

    #[test]
    fn source_numbers_and_keys_are_unique_within_a_layout() {
        for chip in Chip::ALL {
            let named = layout(chip).named;
            let sources: BTreeSet<_> = named.iter().map(|n| n.source).collect();
            let keys: BTreeSet<_> = named.iter().map(|n| n.key).collect();
            assert_eq!(sources.len(), named.len(), "{}: duplicate source number", chip.name());
            assert_eq!(keys.len(), named.len(), "{}: duplicate key", chip.name());
            assert!(
                sources.iter().all(|s| *s <= SOURCE_MASK),
                "{}: source above 0x1f",
                chip.name()
            );
        }
    }

    #[test]
    fn a_register_read_error_fails_the_pass_without_a_partial_result() {
        // Only one register recorded: the replay reports a missing recording.
        let mut io = replay(&[(0x621, 0x01)]);
        let mut chip = Nct67xx::new(Chip::Nct6779D, BASE);
        let error = chip.read(&mut io).unwrap_err();
        assert!(error.to_string().contains("no recording"), "{error}");
    }

    #[test]
    fn a_failed_pass_still_restores_bank_zero() {
        // 0x800 is in bank 8; the next register (0x079... in bank 0) is
        // recorded, but 0x900 is not, so the pass fails after leaving bank 9.
        let mut io =
            replay(&[(0x621, 0x00), (0x100, 0x00), (0x200, 0x00), (0x300, 0x00), (0x800, 0x00)]);
        let log = io.write_log();
        log.lock().unwrap().clear();
        let mut chip = Nct67xx::new(Chip::Nct6779D, BASE);
        assert!(chip.read_temperatures(&mut io).is_err());
        assert!(chip.snapshot(&mut io).is_err());
        let last_bank = log.lock().unwrap().iter().rev().find_map(|w| match w {
            SuperIoWrite::HwmBank { bank, .. } => Some(*bank),
            _ => None,
        });
        assert_eq!(last_bank, Some(0), "bank 0 is selected again after the failure");
    }

    #[test]
    fn voltage_labels_name_the_chip_inputs_with_a_divider() {
        assert_eq!(Nct67xx::voltage_label(0), "VIN0");
        assert_eq!(Nct67xx::voltage_label(2), "VIN2 (AVCC)");
        assert_eq!(Nct67xx::voltage_label(8), "VIN8 (VBAT)");
        assert_eq!(Nct67xx::voltage_label(14), "VIN14");
    }

    #[test]
    fn the_chip_names_itself() {
        assert_eq!(Nct67xx::new(Chip::Nct6779D, BASE).name(), "NCT6779D");
        assert_eq!(Nct67xx::new(Chip::Nct6796DR, BASE).name(), "NCT6796D-R");
    }
}
