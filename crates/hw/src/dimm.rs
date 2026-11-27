//! DIMM temperatures and SPD facts over the platform SMBus.
//!
//! Memory modules answer on SMBus addresses `0x50..=0x57` (slot 0 to 7). Two
//! generations are told apart without writing anything:
//!
//! - DDR5 modules carry an SPD5118 hub whose registers MR0 and MR1 read
//!   `0x51 0x18`. The hub has the temperature sensor built in (register
//!   `0x31`). Its SPD contents sit behind a page register that is written,
//!   so only the hub registers are read.
//! - DDR3 and DDR4 modules are EEPROMs. DDR4 has two 256-byte pages. The
//!   optional temperature sensor is a separate chip at `0x18 | slot`.
//!
//! # No temperature read writes
//!
//! The JC-42.4 sensor registers and the SPD5118 hub registers are not paged,
//! so reading a temperature never needs the page select. The default build
//! therefore makes no SMBus write at all (decision 12), and reads only SPD
//! page 0, which holds the memory type, the geometry and the byte that
//! declares a thermal sensor (byte 14, bit 7).
//!
//! The cost is that a bus another program left on page 1 shows the wrong
//! bytes for those facts, so a DDR4 module can be listed as "not a memory
//! module" and its sensor missed until the page is back on 0 (the EEPROMs
//! power up on page 0). With the `spd-paging` feature the scan first selects
//! page 0, reads page 1 once for the module manufacturer and part number, and
//! selects page 0 again. That is the one SMBus write SysPilot has: a write to
//! `0x36` or `0x37`, which all modules on the bus obey together.
//!
//! The scan runs once, at probe. Serial numbers, manufacturing dates and
//! locations are never read: nothing here identifies one physical module.
//! Format references: JEDEC Standard 21-C Annex L (DDR4 SPD), JEDEC JESD300-5
//! (SPD5118), and the Linux `ee1004`, `jc42` and `spd5118` drivers for the
//! register maps.

use std::sync::Arc;

use syspilot_proto::message::InfoSection;
use syspilot_proto::sensor::{DeviceClass, SensorKind};

use crate::backoff::Backoff;
use crate::bus::{BusError, BusKind, BusLocks, Smbus};
use crate::hub::{Provider, SensorSpec};

/// SMBus address of slot 0.
pub const FIRST_ADDRESS: u8 = 0x50;
/// Slots a bus can address.
pub const SLOT_COUNT: u8 = 8;
/// Seconds between temperature reads. A module's temperature moves over
/// minutes, and every read is a transaction on a bus the platform shares.
pub const PERIOD_SECS: u32 = 5;
/// The longest pause after repeated bus errors, in skipped samples.
const MAXIMUM_BACKOFF_SAMPLES: u32 = 6;
/// Plausible module temperature. A word of all zeros or all ones is a bus
/// that returned nothing, not 0 degrees or -0.06 degrees.
const PLAUSIBLE_TEMPERATURE: std::ops::RangeInclusive<f64> = -40.0..=125.0;

/// DDR4 thermal sensor (JEDEC TSE2004, same register map as the JC-42.4
/// sensors): address `0x18 | slot`, temperature register `0x05`.
const DDR4_SENSOR_BASE_ADDRESS: u8 = 0x18;
const DDR4_SENSOR_TEMPERATURE_REGISTER: u8 = 0x05;
/// SPD5118 hub temperature register (MR49, low byte first).
const SPD5118_TEMPERATURE_REGISTER: u8 = 0x31;

/// What a module is, from its identification bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Generation {
    /// SPD byte 2 is `0x0B`.
    Ddr3,
    /// SPD byte 2 is `0x0C`.
    Ddr4,
    /// MR0/MR1 read `0x51 0x18`.
    Ddr5,
    /// Something answers at the address but is not a recognised module
    /// (a board EEPROM, for example). Carries SPD byte 2.
    Unknown(u8),
}

/// How a module's temperature is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sensor {
    /// TSE2004 at `address`, big-endian register.
    Ddr4 { address: u8 },
    /// SPD5118 hub at `address`, little-endian register, quarter degrees.
    Spd5118 { address: u8 },
}

impl Sensor {
    fn address(self) -> u8 {
        match self {
            Self::Ddr4 { address } | Self::Spd5118 { address } => address,
        }
    }

    fn read(self, smbus: &mut dyn Smbus) -> Result<Option<f64>, BusError> {
        Ok(match self {
            Self::Ddr4 { address } => decode_ddr4_temperature(
                smbus.read_word_data(address, DDR4_SENSOR_TEMPERATURE_REGISTER)?,
            ),
            Self::Spd5118 { address } => decode_spd5118_temperature(
                smbus.read_word_data(address, SPD5118_TEMPERATURE_REGISTER)?,
            ),
        })
    }
}

/// One module found on the bus.
#[derive(Debug, Clone, PartialEq)]
pub struct Module {
    /// Slot, 0 to 7: the SMBus address minus `0x50`.
    pub slot: u8,
    /// What it is.
    pub generation: Generation,
    /// The facts read from its SPD, as display rows.
    pub section: InfoSection,
    sensor: Option<Sensor>,
}

impl Module {
    /// The device name sensors are grouped under.
    pub fn device_name(&self) -> String {
        format!("DIMM {}", self.slot)
    }

    /// Whether the module has a temperature sensor that read plausibly.
    pub fn has_temperature_sensor(&self) -> bool {
        self.sensor.is_some()
    }
}

/// What a scan of the bus found.
#[derive(Debug, Clone, PartialEq)]
pub struct Survey {
    /// Modules that acknowledged, in slot order.
    pub modules: Vec<Module>,
    /// Why the scan stopped before it finished, if it did: the first bus
    /// error that was not a missing device.
    pub stopped: Option<String>,
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

/// DDR4 thermal sensor register: 13 bits, bit 12 the sign, 0.0625 degrees
/// per count. The register is sent high byte first, and an SMBus word read
/// returns the first byte as the low byte, so the bytes are swapped back.
pub fn decode_ddr4_temperature(wire_word: u16) -> Option<f64> {
    if wire_word == 0x0000 || wire_word == 0xFFFF {
        return None;
    }
    let register = wire_word.swap_bytes() & 0x1FFF;
    let counts =
        if register & 0x1000 != 0 { i32::from(register) - 0x2000 } else { i32::from(register) };
    let degrees = f64::from(counts) * 0.0625;
    PLAUSIBLE_TEMPERATURE.contains(&degrees).then_some(degrees)
}

/// SPD5118 temperature register: little-endian, bits 12:2 hold an 11-bit
/// two's-complement value of 0.25 degrees per count (the Linux `spd5118`
/// driver reads it as `sign_extend32(reg >> 2, 10) * 250` millidegrees).
pub fn decode_spd5118_temperature(word: u16) -> Option<f64> {
    if word == 0x0000 || word == 0xFFFF {
        return None;
    }
    let field = i32::from((word >> 2) & 0x7FF);
    let counts = if field & 0x400 != 0 { field - 0x800 } else { field };
    let degrees = f64::from(counts) * 0.25;
    PLAUSIBLE_TEMPERATURE.contains(&degrees).then_some(degrees)
}

/// Data rate in MT/s from the DDR4 SPD minimum cycle time: byte 18 in units
/// of 125 ps plus the signed fine offset of byte 125 in picoseconds.
/// Rounded to a standard bin when within 1.5 percent of one.
pub fn ddr4_data_rate(tck_medium_timebase: u8, tck_fine_offset: u8) -> Option<u32> {
    let tck_ps = i32::from(tck_medium_timebase) * 125 + i32::from(tck_fine_offset as i8);
    if tck_ps <= 0 {
        return None;
    }
    let rate = 2_000_000.0 / f64::from(tck_ps);
    const BINS: [u32; 12] =
        [1600, 1866, 2133, 2400, 2666, 2933, 3200, 3600, 3733, 4000, 4266, 4800];
    let nearest = BINS
        .iter()
        .copied()
        .min_by(|a, b| (f64::from(*a) - rate).abs().total_cmp(&(f64::from(*b) - rate).abs()))?;
    if (f64::from(nearest) - rate).abs() <= f64::from(nearest) * 0.015 {
        Some(nearest)
    } else {
        Some(rate.round() as u32)
    }
}

/// Module capacity in MiB from the DDR4 SPD geometry bytes, or `None` when a
/// field is out of range.
///
/// `density_code` is the low nibble of byte 4, `width_code` and `rank_bits`
/// come from byte 12, `bus_code` from byte 13, and `die_count` from byte 6
/// for stacked packages (1 for monolithic ones). Capacity is the per-die
/// density times the dies per package times the devices per rank times the
/// ranks, as `decode-dimms` computes it.
pub fn ddr4_capacity_mib(
    density_code: u8,
    die_count: u8,
    width_code: u8,
    rank_bits: u8,
    bus_code: u8,
) -> Option<u64> {
    let density_megabits: u64 = match density_code {
        0..=7 => 256u64 << density_code,
        8 => 12 * 1024,
        9 => 24 * 1024,
        _ => return None,
    };
    if width_code > 3 || bus_code > 3 {
        return None;
    }
    let device_width = 4u64 << width_code;
    let bus_width = 8u64 << bus_code;
    let ranks = u64::from(rank_bits) + 1;
    let devices_per_rank = bus_width / device_width;
    Some(density_megabits / 8 * u64::from(die_count) * devices_per_rank * ranks)
}

/// Name of a JEDEC manufacturer from the continuation count (bank) and the
/// code with its parity bit, for the three vendors common enough to name;
/// the raw identifier otherwise.
pub fn jedec_manufacturer(continuation: u8, code: u8) -> String {
    let bank = (continuation & 0x7F) + 1;
    match (bank, code) {
        (1, 0x2C) => "Micron".to_owned(),
        (1, 0xCE) => "Samsung".to_owned(),
        (1, 0xAD) => "SK hynix".to_owned(),
        _ => format!("JEDEC bank {bank}, code {code:#04x}"),
    }
}

/// An ASCII field with trailing blanks and NULs removed. Non-printable bytes
/// show as `?`; a field of all `0x00` or all `0xFF` is "not programmed".
pub fn ascii_field(bytes: &[u8]) -> String {
    if bytes.iter().all(|&b| b == 0x00) || bytes.iter().all(|&b| b == 0xFF) {
        return "not programmed".to_owned();
    }
    let text: String = bytes
        .iter()
        .map(|&b| {
            if (0x20..0x7F).contains(&b) {
                b as char
            } else if b == 0 {
                ' '
            } else {
                '?'
            }
        })
        .collect();
    text.trim_end().to_owned()
}

fn ddr4_form_factor(module_type_byte: u8) -> String {
    match module_type_byte & 0x0F {
        0x01 => "RDIMM".to_owned(),
        0x02 => "UDIMM".to_owned(),
        0x03 => "SO-DIMM".to_owned(),
        0x04 => "LRDIMM".to_owned(),
        0x05 => "Mini-RDIMM".to_owned(),
        0x06 => "Mini-UDIMM".to_owned(),
        0x08 => "72b-SO-RDIMM".to_owned(),
        0x09 => "72b-SO-UDIMM".to_owned(),
        0x0C => "16b-SO-DIMM".to_owned(),
        0x0D => "32b-SO-DIMM".to_owned(),
        other => format!("module type {other:#x}"),
    }
}

// ---------------------------------------------------------------------------
// Reading the bus
// ---------------------------------------------------------------------------

/// Leaves the SPD page selected as the modules expect it, page 0, however the
/// reads in between ended. All DDR4 modules on the bus switch page together,
/// so a page left on 1 would make every other tool read the wrong bytes.
#[cfg(feature = "spd-paging")]
struct PageOne<'a> {
    smbus: &'a mut dyn Smbus,
    restored: bool,
}

#[cfg(feature = "spd-paging")]
impl<'a> PageOne<'a> {
    /// Selects page 1. A module that does not acknowledge the select still
    /// switches on some controllers; that is not an error, anything else is.
    fn select(smbus: &'a mut dyn Smbus) -> Result<Self, BusError> {
        let mut guard = Self { smbus, restored: false };
        match guard.smbus.select_spd_page(1) {
            Ok(()) => Ok(guard),
            Err(error) if error.is_no_device() => Ok(guard),
            Err(error) => {
                // The select may have reached some modules: put page 0 back.
                let _ = guard.restore();
                Err(error)
            }
        }
    }

    /// Selects page 0 again and reports a failure.
    fn restore(&mut self) -> Result<(), BusError> {
        self.restored = true;
        match self.smbus.select_spd_page(0) {
            Err(error) if !error.is_no_device() => Err(error),
            _ => Ok(()),
        }
    }
}

#[cfg(feature = "spd-paging")]
impl Drop for PageOne<'_> {
    fn drop(&mut self) {
        if !self.restored {
            let _ = self.restore();
        }
    }
}

#[cfg(feature = "spd-paging")]
fn read_bytes(
    smbus: &mut dyn Smbus,
    address: u8,
    first: u8,
    count: u8,
) -> Result<Vec<u8>, BusError> {
    (0..count).map(|offset| smbus.read_byte_data(address, first + offset)).collect()
}

/// What a probe read from one address before the module is classified.
struct Identification {
    slot: u8,
    byte_0: u8,
    byte_1: u8,
}

fn is_ddr5_hub(identification: &Identification) -> bool {
    identification.byte_0 == 0x51 && identification.byte_1 == 0x18
}

/// Scans the eight slots and reads what each module reports about itself.
/// The caller holds the SMBus mutex for the whole scan.
///
/// The first address is probed with a byte read: a driver error other than
/// "nothing acknowledged" (a wrong request shape, a held bus) ends the scan
/// right there. Later, a timeout or any other bus error stops it too, and the
/// reason is returned in [`Survey::stopped`]; nothing is retried.
pub fn survey(smbus: &mut dyn Smbus) -> Survey {
    let mut stopped = None;
    let mut present = Vec::new();
    for slot in 0..SLOT_COUNT {
        let address = FIRST_ADDRESS + slot;
        match smbus.read_byte_data(address, 0) {
            Ok(byte_0) => match smbus.read_byte_data(address, 1) {
                Ok(byte_1) => present.push(Identification { slot, byte_0, byte_1 }),
                Err(error) if error.is_no_device() => {}
                Err(error) => {
                    stopped = Some(format!("slot {slot}: {error}"));
                    break;
                }
            },
            Err(error) if error.is_no_device() => {}
            Err(error) => {
                stopped = Some(format!("slot {slot}: {error}"));
                break;
            }
        }
    }

    let mut modules = Vec::new();
    if stopped.is_none() && !present.is_empty() {
        // DDR5 hubs need no page select; anything else might be a DDR4
        // module that another tool left on page 1. Without the `spd-paging`
        // feature that case is not handled (see the module documentation).
        #[cfg(feature = "spd-paging")]
        if present.iter().any(|module| !is_ddr5_hub(module))
            && let Err(error) = smbus.select_spd_page(0)
            && !error.is_no_device()
        {
            return Survey { modules, stopped: Some(format!("page select: {error}")) };
        }
        match read_modules(smbus, &present) {
            Ok(read) => modules = read,
            Err((read, error)) => {
                modules = read;
                stopped = Some(error.to_string());
            }
        }
    }
    Survey { modules, stopped }
}

type PartialRead = (Vec<Module>, BusError);

fn read_modules(
    smbus: &mut dyn Smbus,
    present: &[Identification],
) -> Result<Vec<Module>, PartialRead> {
    // Page-0 facts first for every module, then page 1 once for all of them.
    let mut staged: Vec<Staged> = Vec::new();
    for identification in present {
        match read_page_zero(smbus, identification) {
            Ok(module) => staged.push(module),
            Err(error) => return Err((finish(staged, &[]), error)),
        }
    }
    let (page_one, failure) = read_page_one(smbus, &staged);
    let modules = finish(staged, &page_one);
    match failure {
        Some(error) => Err((modules, error)),
        None => Ok(modules),
    }
}

/// Bytes 320..=351 of the SPD of every DDR4 module (manufacturer, part
/// number, revision, DRAM manufacturer), by slot, and the error that cut the
/// reads short, if any. Bytes 322..=328 (location, date and serial number)
/// are skipped on purpose. Needs the page select.
#[cfg(feature = "spd-paging")]
fn read_page_one(
    smbus: &mut dyn Smbus,
    staged: &[Staged],
) -> (Vec<(u8, Vec<u8>)>, Option<BusError>) {
    let ddr4_slots: Vec<u8> = staged
        .iter()
        .filter(|module| matches!(module.generation, Generation::Ddr4))
        .map(|module| module.slot)
        .collect();
    let mut page_one: Vec<(u8, Vec<u8>)> = Vec::new();
    if ddr4_slots.is_empty() {
        return (page_one, None);
    }
    let mut guard = match PageOne::select(smbus) {
        Ok(guard) => guard,
        Err(error) => return (page_one, Some(error)),
    };
    for slot in ddr4_slots {
        match read_bytes(guard.smbus, FIRST_ADDRESS + slot, 64, 2).and_then(|head| {
            let tail = read_bytes(guard.smbus, FIRST_ADDRESS + slot, 73, 23)?;
            Ok([head, tail].concat())
        }) {
            Ok(bytes) => page_one.push((slot, bytes)),
            Err(error) => {
                let _ = guard.restore();
                return (page_one, Some(error));
            }
        }
    }
    let failure = guard.restore().err();
    (page_one, failure)
}

/// Without the `spd-paging` feature page 1 is not read: it needs a bus write.
#[cfg(not(feature = "spd-paging"))]
fn read_page_one(_: &mut dyn Smbus, _: &[Staged]) -> (Vec<(u8, Vec<u8>)>, Option<BusError>) {
    (Vec::new(), None)
}

/// A module whose page-0 facts are read, waiting for the page-1 ones.
struct Staged {
    slot: u8,
    generation: Generation,
    rows: Vec<(String, String)>,
    sensor: Option<Sensor>,
}

fn finish(staged: Vec<Staged>, page_one: &[(u8, Vec<u8>)]) -> Vec<Module> {
    staged
        .into_iter()
        .map(|mut module| {
            let bytes = page_one
                .iter()
                .find(|(slot, _)| *slot == module.slot)
                .map(|(_, bytes)| bytes.as_slice());
            if bytes.is_none() && module.generation == Generation::Ddr4 {
                module.rows.push((
                    "Manufacturer and part number".to_owned(),
                    "not read (they sit on SPD page 1, which needs a bus write)".to_owned(),
                ));
            }
            if let Some(bytes) = bytes {
                // bytes: [320, 321, 329..=351]
                let (manufacturer, rest) = bytes.split_at(2);
                let (part_number, tail) = rest.split_at(20);
                module.rows.push((
                    "Module manufacturer".to_owned(),
                    jedec_manufacturer(manufacturer[0], manufacturer[1]),
                ));
                module.rows.push(("Part number".to_owned(), ascii_field(part_number)));
                module.rows.push(("Module revision".to_owned(), format!("{:#04x}", tail[0])));
                module
                    .rows
                    .push(("DRAM manufacturer".to_owned(), jedec_manufacturer(tail[1], tail[2])));
            }
            Module {
                slot: module.slot,
                generation: module.generation,
                section: InfoSection { title: format!("DIMM {}", module.slot), rows: module.rows },
                sensor: module.sensor,
            }
        })
        .collect()
}

fn read_page_zero(
    smbus: &mut dyn Smbus,
    identification: &Identification,
) -> Result<Staged, BusError> {
    let address = FIRST_ADDRESS + identification.slot;
    let location =
        ("SMBus address".to_owned(), format!("{address:#04x} (slot {})", identification.slot));
    if is_ddr5_hub(identification) {
        return read_ddr5_hub(smbus, identification.slot, location);
    }
    let byte_2 = smbus.read_byte_data(address, 2)?;
    match byte_2 {
        0x0C => read_ddr4(smbus, identification.slot, location),
        0x0B => Ok(Staged {
            slot: identification.slot,
            generation: Generation::Ddr3,
            rows: vec![
                ("Type".to_owned(), "DDR3 SDRAM".to_owned()),
                ("Details".to_owned(), "SPD not decoded (no temperature support)".to_owned()),
                location,
            ],
            sensor: None,
        }),
        other => Ok(Staged {
            slot: identification.slot,
            generation: Generation::Unknown(other),
            rows: vec![
                ("Type".to_owned(), format!("not a memory module (SPD byte 2 reads {other:#04x})")),
                location,
            ],
            sensor: None,
        }),
    }
}

fn read_ddr4(
    smbus: &mut dyn Smbus,
    slot: u8,
    location: (String, String),
) -> Result<Staged, BusError> {
    let address = FIRST_ADDRESS + slot;
    let mut byte = |offset: u8| smbus.read_byte_data(address, offset);
    let revision = byte(1)?;
    let module_type = byte(3)?;
    let density = byte(4)?;
    let package = byte(6)?;
    let organisation = byte(12)?;
    let bus_width = byte(13)?;
    let thermal = byte(14)?;
    let tck = byte(18)?;
    let tck_fine = byte(125)?;

    let ranks = ((organisation >> 3) & 0x07) + 1;
    let device_width = 4u32 << (organisation & 0x07).min(7);
    // Byte 6 bit 7 marks a non-monolithic package; bits 6:4 count its dies
    // minus one.
    let die_count = if package & 0x80 != 0 { ((package >> 4) & 0x07) + 1 } else { 1 };
    let capacity = ddr4_capacity_mib(
        density & 0x0F,
        die_count,
        organisation & 0x07,
        (organisation >> 3) & 0x07,
        bus_width & 0x07,
    );
    let ecc = (bus_width >> 3) & 0x03 == 1;

    let mut rows = vec![
        ("Type".to_owned(), "DDR4 SDRAM".to_owned()),
        ("Form factor".to_owned(), ddr4_form_factor(module_type)),
        (
            "Capacity".to_owned(),
            capacity.map_or_else(
                || format!("not decoded (density code {:#x})", density & 0x0F),
                |mib| format!("{} GiB", mib / 1024),
            ),
        ),
        (
            "Organisation".to_owned(),
            format!(
                "{ranks} rank{}, x{device_width} devices{}",
                if ranks == 1 { "" } else { "s" },
                if ecc { ", ECC" } else { "" }
            ),
        ),
        (
            "Speed".to_owned(),
            ddr4_data_rate(tck, tck_fine)
                .map_or_else(|| "not decoded".to_owned(), |rate| format!("DDR4-{rate}")),
        ),
        ("SPD revision".to_owned(), format!("{}.{}", revision >> 4, revision & 0x0F)),
    ];

    // Byte 14 bit 7: the module carries a thermal sensor.
    let mut sensor = None;
    if thermal & 0x80 != 0 {
        let candidate = Sensor::Ddr4 { address: DDR4_SENSOR_BASE_ADDRESS | slot };
        match candidate.read(smbus) {
            Ok(Some(_)) => {
                sensor = Some(candidate);
                rows.push((
                    "Thermal sensor".to_owned(),
                    format!("TSE2004 at {:#04x}", candidate.address()),
                ));
            }
            Ok(None) => rows.push((
                "Thermal sensor".to_owned(),
                format!(
                    "declared, at {:#04x}, but the reading is not plausible",
                    candidate.address()
                ),
            )),
            Err(error) if error.is_no_device() => rows.push((
                "Thermal sensor".to_owned(),
                format!("declared, but nothing answers at {:#04x}", candidate.address()),
            )),
            Err(error) => return Err(error),
        }
    } else {
        rows.push(("Thermal sensor".to_owned(), "none declared".to_owned()));
    }
    rows.push(location);
    Ok(Staged { slot, generation: Generation::Ddr4, rows, sensor })
}

fn read_ddr5_hub(
    smbus: &mut dyn Smbus,
    slot: u8,
    location: (String, String),
) -> Result<Staged, BusError> {
    let address = FIRST_ADDRESS + slot;
    // SPD5118 registers (JESD300-5; the Linux spd5118 driver names them):
    // MR2 revision, MR3:MR4 vendor ID, MR5 capability (bit 1: temperature
    // sensor supported), MR26 sensor configuration (bit 0: sensor disabled).
    let revision = smbus.read_byte_data(address, 0x02)?;
    let vendor_continuation = smbus.read_byte_data(address, 0x03)?;
    let vendor_code = smbus.read_byte_data(address, 0x04)?;
    let capability = smbus.read_byte_data(address, 0x05)?;
    let configuration = smbus.read_byte_data(address, 0x1A)?;

    let mut rows = vec![
        ("Type".to_owned(), "DDR5 SDRAM, SPD5118 hub".to_owned()),
        ("Hub manufacturer".to_owned(), jedec_manufacturer(vendor_continuation, vendor_code)),
        ("Hub revision".to_owned(), format!("{:#04x}", revision)),
        (
            "SPD contents".to_owned(),
            "not read (they sit behind a page register that has to be written)".to_owned(),
        ),
    ];
    let mut sensor = None;
    if capability & 0x02 == 0 {
        rows.push(("Thermal sensor".to_owned(), "not supported by this hub".to_owned()));
    } else if configuration & 0x01 != 0 {
        rows.push(("Thermal sensor".to_owned(), "disabled in the hub configuration".to_owned()));
    } else {
        let candidate = Sensor::Spd5118 { address };
        match candidate.read(smbus) {
            Ok(Some(_)) => {
                sensor = Some(candidate);
                rows.push(("Thermal sensor".to_owned(), format!("in the hub at {address:#04x}")));
            }
            Ok(None) => rows.push((
                "Thermal sensor".to_owned(),
                "in the hub, but the reading is not plausible".to_owned(),
            )),
            Err(error) if error.is_no_device() => {
                rows.push(("Thermal sensor".to_owned(), "the hub stopped answering".to_owned()))
            }
            Err(error) => return Err(error),
        }
    }
    rows.push(location);
    Ok(Staged { slot, generation: Generation::Ddr5, rows, sensor })
}

/// Names the SMBus controller from the three cells of `ioctl_identity`,
/// which `SmbusI801` and `SmbusPIIX4` fill the same way: a controller type
/// string packed little-endian (`i801`, `PIIX4`), the I/O base of the
/// controller, and the PCI vendor/device dword with the subsystem dword
/// above it.
pub fn describe_controller(identity: [u64; 3]) -> String {
    let kind: String = identity[0]
        .to_le_bytes()
        .iter()
        .take_while(|&&byte| byte != 0)
        .map(|&byte| if byte.is_ascii_graphic() { byte as char } else { '?' })
        .collect();
    let vendor = identity[2] & 0xFFFF;
    let device = (identity[2] >> 16) & 0xFFFF;
    format!("{kind} SMBus controller {vendor:04X}:{device:04X} at I/O {:#x}", identity[1])
}

// ---------------------------------------------------------------------------
// The provider
// ---------------------------------------------------------------------------

/// Why DIMM sensing could not start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DimmProbeError {
    /// The SMBus mutex is held by another program, or could not be opened.
    Bus(BusError),
    /// The first transfer failed for a reason other than an empty address:
    /// the controller or the request shape is not what was expected, and
    /// nothing further was sent.
    FirstTransferFailed(BusError),
}

impl std::fmt::Display for DimmProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bus(error) => error.fmt(f),
            Self::FirstTransferFailed(error) => write!(
                f,
                "(dimm): expected the first SMBus read to answer or find no device | received: \
                 {error}"
            ),
        }
    }
}

impl std::error::Error for DimmProbeError {}

/// The outcome of the one-time scan.
pub struct DimmProbe {
    /// Temperature provider, present when at least one module has a sensor.
    pub provider: Option<DimmProvider>,
    /// One section per module found.
    pub info: Vec<InfoSection>,
    /// One line for the feature detail.
    pub summary: String,
    /// Modules that acknowledged.
    pub modules: usize,
}

struct SensorEntry {
    slot: u8,
    sensor: Sensor,
}

/// Temperatures of the memory modules on the bus.
pub struct DimmProvider {
    smbus: Box<dyn Smbus>,
    locks: Arc<dyn BusLocks>,
    sensors: Vec<SensorEntry>,
    backoff: Backoff,
    last_problem: Option<String>,
}

impl DimmProvider {
    /// Scans the bus once under the SMBus mutex and prepares a provider for
    /// the modules that have a sensor. `controller` names the SMBus
    /// controller in the summary (for example `Intel PCH (SmbusI801)`).
    pub fn probe(
        mut smbus: Box<dyn Smbus>,
        locks: Arc<dyn BusLocks>,
        controller: &str,
    ) -> Result<DimmProbe, DimmProbeError> {
        let survey = {
            let _bus = locks.acquire(BusKind::Smbus).map_err(DimmProbeError::Bus)?;
            // The first transfer decides whether the request shape works at
            // all: an empty address is the expected answer on a machine with
            // no module there, anything else stops everything.
            match smbus.read_byte_data(FIRST_ADDRESS, 0) {
                Ok(_) => {}
                Err(error) if error.is_no_device() => {}
                Err(error) => return Err(DimmProbeError::FirstTransferFailed(error)),
            }
            survey(smbus.as_mut())
        };

        let info: Vec<InfoSection> = survey.modules.iter().map(|m| m.section.clone()).collect();
        let sensors: Vec<SensorEntry> = survey
            .modules
            .iter()
            .filter_map(|module| {
                module.sensor.map(|sensor| SensorEntry { slot: module.slot, sensor })
            })
            .collect();
        let summary = summarise(controller, &survey, sensors.len());
        let modules = survey.modules.len();
        let provider = (!sensors.is_empty()).then(|| Self {
            smbus,
            locks,
            sensors,
            backoff: Backoff::new(MAXIMUM_BACKOFF_SAMPLES),
            last_problem: None,
        });
        Ok(DimmProbe { provider, info, summary, modules })
    }

    fn read_all(&mut self) -> Result<Vec<Option<f64>>, BusError> {
        let locks = Arc::clone(&self.locks);
        let _bus = locks.acquire(BusKind::Smbus)?;
        let mut values = Vec::with_capacity(self.sensors.len());
        for entry in &self.sensors {
            match entry.sensor.read(self.smbus.as_mut()) {
                Ok(value) => values.push(value),
                // A module that stops answering reads as no value; the others
                // are still read.
                Err(error) if error.is_no_device() => values.push(None),
                Err(error) => return Err(error),
            }
        }
        Ok(values)
    }
}

fn summarise(controller: &str, survey: &Survey, sensors: usize) -> String {
    match (survey.modules.is_empty(), &survey.stopped) {
        (true, None) => format!(
            "{controller}: nothing acknowledged at SMBus addresses 0x50-0x57, so no memory \
             module is reachable through this controller"
        ),
        (true, Some(reason)) => {
            format!("{controller}: the scan of SMBus addresses 0x50-0x57 stopped: {reason}")
        }
        (false, stopped) => {
            let slots: Vec<String> = survey.modules.iter().map(|m| m.slot.to_string()).collect();
            let mut text = format!(
                "{controller}: {} SPD device(s) in slot(s) {}; {sensors} with a temperature \
                 sensor",
                survey.modules.len(),
                slots.join(", ")
            );
            if let Some(reason) = stopped {
                text.push_str(&format!("; the scan stopped early: {reason}"));
            }
            text
        }
    }
}

impl Provider for DimmProvider {
    fn sensors(&self) -> Vec<SensorSpec> {
        self.sensors
            .iter()
            .map(|entry| SensorSpec {
                device: format!("DIMM {}", entry.slot),
                class: DeviceClass::Memory,
                key: format!("memory.dimm{}.temp", entry.slot),
                label: "Temperature".to_owned(),
                kind: SensorKind::Temperature,
            })
            .collect()
    }

    fn sample(&mut self, out: &mut Vec<Option<f64>>) {
        if self.backoff.skip_this_sample() {
            out.extend(std::iter::repeat_n(None, self.sensors.len()));
            return;
        }
        match self.read_all() {
            Ok(values) => {
                self.backoff.record_success();
                self.last_problem = None;
                out.extend(values);
            }
            Err(error) => {
                if !error.is_contended() {
                    self.backoff.record_failure();
                }
                self.last_problem = Some(error.to_string());
                out.extend(std::iter::repeat_n(None, self.sensors.len()));
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
    use std::sync::PoisonError;

    use super::*;
    use crate::bus::replay::{ReplaySmbus, smbus_byte_key, smbus_word_key};
    use crate::bus::{FakeBusLocks, Fixture};

    // -- decoding -----------------------------------------------------------

    #[test]
    fn ddr4_temperature_swaps_the_wire_bytes_and_scales_by_a_sixteenth() {
        // Register 0x0194 (25.25 degrees) arrives high byte first, so the
        // word read returns 0x9401.
        assert_eq!(decode_ddr4_temperature(0x9401), Some(25.25));
        // 0x0280 = 40.0 degrees.
        assert_eq!(decode_ddr4_temperature(0x8002), Some(40.0));
    }

    #[test]
    fn ddr4_temperature_masks_the_alarm_flags_above_bit_12() {
        // 0xA194: bit 15 (critical) and bit 13 (upper limit) set around 25.25.
        assert_eq!(decode_ddr4_temperature(0x94A1), Some(25.25));
    }

    #[test]
    fn ddr4_temperature_is_negative_when_bit_12_is_set() {
        // 0x1FFC is -0.25: -4 counts.
        assert_eq!(decode_ddr4_temperature(0xFC1F), Some(-0.25));
        // 0x1E70 is -25.0 degrees: 0x1E70 - 0x2000 = -400 counts.
        assert_eq!(decode_ddr4_temperature(0x701E), Some(-25.0));
    }

    #[test]
    fn a_word_of_all_zeros_or_ones_is_no_reading() {
        assert_eq!(decode_ddr4_temperature(0x0000), None);
        assert_eq!(decode_ddr4_temperature(0xFFFF), None);
        assert_eq!(decode_spd5118_temperature(0x0000), None);
        assert_eq!(decode_spd5118_temperature(0xFFFF), None);
    }

    #[test]
    fn implausible_temperatures_are_dropped() {
        // 0x0FA0 = 250 degrees.
        assert_eq!(decode_ddr4_temperature(0xA00F), None);
        // 0x0300 = 48 degrees is fine; 0x0D00 = 208 degrees is not.
        assert_eq!(decode_ddr4_temperature(0x0003), Some(48.0));
        assert_eq!(decode_ddr4_temperature(0x000D), None);
    }

    #[test]
    fn spd5118_temperature_is_an_11_bit_value_in_quarter_degrees_from_bit_2() {
        // 25.0 degrees = 100 counts, shifted left 2.
        assert_eq!(decode_spd5118_temperature(100 << 2), Some(25.0));
        // 25.25 degrees.
        assert_eq!(decode_spd5118_temperature(101 << 2), Some(25.25));
        // The two low bits are not part of the value.
        assert_eq!(decode_spd5118_temperature((100 << 2) | 0b11), Some(25.0));
    }

    #[test]
    fn spd5118_temperature_sign_extends_from_bit_12() {
        // -0.25: -1 in 11 bits is 0x7FF, shifted left 2 is 0x1FFC.
        assert_eq!(decode_spd5118_temperature(0x1FFC), Some(-0.25));
        // -10 degrees = -40 counts = 0x7D8 in 11 bits.
        assert_eq!(decode_spd5118_temperature(0x7D8 << 2), Some(-10.0));
    }

    #[test]
    fn the_ddr4_data_rate_follows_the_cycle_time_and_snaps_to_a_bin() {
        assert_eq!(ddr4_data_rate(0x05, 0x00), Some(3200), "625 ps");
        assert_eq!(ddr4_data_rate(0x06, 0x00), Some(2666), "750 ps = 2666.7 MT/s");
        // 875 ps - 42 ps = 833 ps = 2401 MT/s.
        assert_eq!(ddr4_data_rate(0x07, 0xD6), Some(2400));
        assert_eq!(ddr4_data_rate(0x0A, 0x00), Some(1600), "1250 ps");
    }

    #[test]
    fn an_odd_cycle_time_gives_the_computed_rate_and_a_zero_one_gives_none() {
        // 1000 ps = 2000 MT/s is not a DDR4 bin; no snapping.
        assert_eq!(ddr4_data_rate(0x08, 0x00), Some(2000));
        assert_eq!(ddr4_data_rate(0x00, 0x00), None);
        // A fine offset that cancels the whole base.
        assert_eq!(ddr4_data_rate(0x01, 0x83), None, "125 ps - 125 ps");
    }

    #[test]
    fn capacity_is_density_times_dies_times_devices_times_ranks() {
        // 8 Gbit dies, monolithic, x8 devices on a 64-bit bus, 1 rank: 8 GiB.
        assert_eq!(ddr4_capacity_mib(5, 1, 1, 0, 3), Some(8 * 1024));
        // 8 Gbit, x8, 2 ranks: 16 GiB.
        assert_eq!(ddr4_capacity_mib(5, 1, 1, 1, 3), Some(16 * 1024));
        // 16 Gbit dies, x16 devices: 4 devices per rank, 1 rank: 8 GiB.
        assert_eq!(ddr4_capacity_mib(6, 1, 2, 0, 3), Some(8 * 1024));
        // A dual-die package doubles it.
        assert_eq!(ddr4_capacity_mib(5, 2, 1, 0, 3), Some(16 * 1024));
    }

    #[test]
    fn capacity_refuses_codes_outside_the_spec() {
        assert_eq!(ddr4_capacity_mib(0x0A, 1, 1, 0, 3), None, "density code 10");
        assert_eq!(ddr4_capacity_mib(5, 1, 4, 0, 3), None, "device width code 4");
        assert_eq!(ddr4_capacity_mib(5, 1, 1, 0, 4), None, "bus width code 4");
        // A 12 Gbit die is 1.5 GiB; eight of them are 12 GiB.
        assert_eq!(ddr4_capacity_mib(8, 1, 1, 0, 3), Some(12 * 1024), "12 Gbit die");
    }

    #[test]
    fn the_three_common_dram_makers_are_named_and_others_are_raw() {
        assert_eq!(jedec_manufacturer(0x80, 0xCE), "Samsung");
        assert_eq!(jedec_manufacturer(0x80, 0xAD), "SK hynix");
        assert_eq!(jedec_manufacturer(0x00, 0x2C), "Micron");
        assert_eq!(jedec_manufacturer(0x01, 0x98), "JEDEC bank 2, code 0x98");
    }

    #[test]
    fn ascii_fields_are_trimmed_and_unprintable_bytes_marked() {
        assert_eq!(ascii_field(b"KVR24N17S8/8       "), "KVR24N17S8/8");
        assert_eq!(ascii_field(&[b'A', 0x01, b'B', 0x20, 0x20]), "A?B");
        assert_eq!(ascii_field(&[0x00; 20]), "not programmed");
        assert_eq!(ascii_field(&[0xFF; 20]), "not programmed");
        assert_eq!(ascii_field(&[b'X', 0x00, 0x00]), "X");
    }

    #[test]
    fn form_factors_name_the_common_module_types() {
        assert_eq!(ddr4_form_factor(0x02), "UDIMM");
        assert_eq!(ddr4_form_factor(0x03), "SO-DIMM");
        assert_eq!(ddr4_form_factor(0x01), "RDIMM");
        assert_eq!(ddr4_form_factor(0x82), "UDIMM", "the hybrid bit is ignored");
        assert_eq!(ddr4_form_factor(0x0F), "module type 0xf");
    }

    // -- the bus ------------------------------------------------------------

    /// A DDR4 UDIMM in slot `slot`: 8 GiB, x8, one rank, DDR4-2400, with a
    /// thermal sensor reading 25.25 degrees. Page 1 holds Micron and a part
    /// number.
    fn add_ddr4_module(fixture: &mut Fixture, slot: u8, temperature_wire_word: u64) {
        let address = FIRST_ADDRESS + slot;
        let page_zero = [
            (0, 0x23),
            (1, 0x11),
            (2, 0x0C),
            (3, 0x02),
            (4, 0x85), // density code 5 (8 Gbit); bank bits in the upper nibble
            (6, 0x00),
            (12, 0x01), // 1 rank, x8
            (13, 0x03), // 64-bit bus, no ECC
            (14, 0x80), // thermal sensor present
            (18, 0x07),
            (125, 0xD6),
        ];
        for (offset, value) in page_zero {
            fixture.set(smbus_byte_key(address, 0, offset), value);
        }
        // JEDEC manufacturer: zero continuation codes (0x80 with the odd
        // parity bit), then 0x2C.
        fixture.set(smbus_byte_key(address, 1, 64), 0x80);
        fixture.set(smbus_byte_key(address, 1, 65), 0x2C);
        for (index, character) in b"4ATF1G64AZ-2G3E1    ".iter().enumerate() {
            fixture.set(smbus_byte_key(address, 1, 73 + index as u8), u64::from(*character));
        }
        fixture.set(smbus_byte_key(address, 1, 93), 0x31);
        fixture.set(smbus_byte_key(address, 1, 94), 0x80);
        fixture.set(smbus_byte_key(address, 1, 95), 0x2C);
        fixture.set(smbus_word_key(DDR4_SENSOR_BASE_ADDRESS | slot, 5), temperature_wire_word);
    }

    fn add_ddr5_module(fixture: &mut Fixture, slot: u8, temperature: u64) {
        let address = FIRST_ADDRESS + slot;
        for (register, value) in [
            (0x00, 0x51),
            (0x01, 0x18),
            (0x02, 0x12),
            (0x03, 0x80),
            (0x04, 0xCE),
            (0x05, 0x02),
            (0x1A, 0x00),
        ] {
            fixture.set(smbus_byte_key(address, 0, register), value);
        }
        fixture.set(smbus_word_key(address, 0x31), temperature);
    }

    fn one_ddr4_module() -> Fixture {
        let mut fixture = Fixture::new();
        add_ddr4_module(&mut fixture, 0, 0x9401);
        fixture
    }

    #[test]
    fn a_ddr4_module_s_sensor_and_page_zero_facts_are_read_without_a_page_select() {
        let mut bus = ReplaySmbus::new(one_ddr4_module());
        let survey = survey(&mut bus);
        assert_eq!(survey.stopped, None);
        let [module] = survey.modules.as_slice() else { panic!("{:?}", survey.modules) };
        assert!(module.has_temperature_sensor());
        let rows: std::collections::BTreeMap<_, _> = module.section.rows.iter().cloned().collect();
        assert_eq!(rows["Capacity"], "8 GiB");
        assert_eq!(rows["Speed"], "DDR4-2400");
        assert_eq!(rows["Thermal sensor"], "TSE2004 at 0x18");
        if cfg!(feature = "spd-paging") {
            assert_eq!(rows["Module manufacturer"], "Micron");
            assert!(!rows.contains_key("Manufacturer and part number"));
        } else {
            assert_eq!(
                rows["Manufacturer and part number"],
                "not read (they sit on SPD page 1, which needs a bus write)"
            );
            assert!(!rows.contains_key("Module manufacturer"), "{rows:?}");
            assert!(!rows.contains_key("Part number"), "{rows:?}");
        }
    }

    #[cfg(feature = "spd-paging")]
    #[test]
    fn a_ddr4_module_is_described_from_its_two_spd_pages() {
        let mut bus = ReplaySmbus::new(one_ddr4_module());
        let survey = survey(&mut bus);
        assert_eq!(survey.stopped, None);
        let [module] = survey.modules.as_slice() else { panic!("{:?}", survey.modules) };
        assert_eq!(module.generation, Generation::Ddr4);
        assert!(module.has_temperature_sensor());
        assert_eq!(module.device_name(), "DIMM 0");
        let rows: std::collections::BTreeMap<_, _> = module.section.rows.iter().cloned().collect();
        assert_eq!(module.section.title, "DIMM 0");
        assert_eq!(rows["Type"], "DDR4 SDRAM");
        assert_eq!(rows["Form factor"], "UDIMM");
        assert_eq!(rows["Capacity"], "8 GiB");
        assert_eq!(rows["Organisation"], "1 rank, x8 devices");
        assert_eq!(rows["Speed"], "DDR4-2400");
        assert_eq!(rows["SPD revision"], "1.1");
        assert_eq!(rows["Module manufacturer"], "Micron");
        assert_eq!(rows["Part number"], "4ATF1G64AZ-2G3E1");
        assert_eq!(rows["Module revision"], "0x31");
        assert_eq!(rows["DRAM manufacturer"], "Micron");
        assert_eq!(rows["Thermal sensor"], "TSE2004 at 0x18");
        assert_eq!(rows["SMBus address"], "0x50 (slot 0)");
    }

    #[cfg(feature = "spd-paging")]
    #[test]
    fn the_survey_never_reads_the_serial_number_date_or_location_bytes() {
        let mut bus = ReplaySmbus::new(one_ddr4_module());
        let log = bus.page_select_log();
        survey(&mut bus);
        // Bytes 322..=328 are page 1 offsets 66..=72; a replay of a fixture
        // without them would have failed with a missing key if they were read.
        assert_eq!(
            *log.lock().unwrap(),
            [0, 1, 0],
            "page 0 for the scan, page 1 once, page 0 again"
        );
    }

    #[cfg(feature = "spd-paging")]
    #[test]
    fn the_page_is_back_on_zero_after_a_survey() {
        let mut bus = ReplaySmbus::new(one_ddr4_module());
        let log = bus.page_select_log();
        survey(&mut bus);
        assert_eq!(log.lock().unwrap().last(), Some(&0));
    }

    #[cfg(feature = "spd-paging")]
    #[test]
    fn a_fault_while_page_one_is_selected_still_restores_page_zero() {
        let mut fixture = one_ddr4_module();
        // Page 1 is missing a byte the survey reads: a rejection, not a NAK.
        let address = FIRST_ADDRESS;
        let mut incomplete = Fixture::new();
        for (key, value) in fixture.entries() {
            if key != smbus_byte_key(address, 1, 80) {
                incomplete.set(key, value);
            }
        }
        fixture = incomplete;
        let mut bus = ReplaySmbus::new(fixture);
        let log = bus.page_select_log();
        let survey = survey(&mut bus);
        assert!(survey.stopped.is_some(), "the scan reports why it stopped");
        assert_eq!(*log.lock().unwrap(), [0, 1, 0], "page 0 is restored after the failed read");
        // The module's page-0 facts were read before the failure and are kept.
        assert_eq!(survey.modules.len(), 1);
    }

    #[cfg(feature = "spd-paging")]
    #[test]
    fn an_unacknowledged_page_select_is_tolerated() {
        let mut bus = ReplaySmbus::new(one_ddr4_module()).without_page_select_acknowledge();
        let survey = survey(&mut bus);
        assert_eq!(survey.stopped, None);
        assert_eq!(survey.modules.len(), 1);
    }

    #[test]
    fn a_ddr5_hub_is_read_without_any_page_select() {
        let mut fixture = Fixture::new();
        add_ddr5_module(&mut fixture, 1, 100 << 2);
        let mut bus = ReplaySmbus::new(fixture);
        #[cfg(feature = "spd-paging")]
        let log = bus.page_select_log();
        let survey = survey(&mut bus);
        assert_eq!(survey.stopped, None);
        let [module] = survey.modules.as_slice() else { panic!("{:?}", survey.modules) };
        assert_eq!(module.generation, Generation::Ddr5);
        assert_eq!(module.slot, 1);
        assert!(module.has_temperature_sensor());
        let rows: std::collections::BTreeMap<_, _> = module.section.rows.iter().cloned().collect();
        assert_eq!(rows["Type"], "DDR5 SDRAM, SPD5118 hub");
        assert_eq!(rows["Hub manufacturer"], "Samsung");
        assert_eq!(rows["Thermal sensor"], "in the hub at 0x51");
        #[cfg(feature = "spd-paging")]
        assert!(log.lock().unwrap().is_empty(), "no write of any kind for DDR5");
    }

    #[test]
    fn a_ddr5_hub_with_its_sensor_disabled_or_unsupported_has_no_sensor() {
        for (capability, configuration, expected) in [
            (0x00, 0x00, "not supported by this hub"),
            (0x02, 0x01, "disabled in the hub configuration"),
        ] {
            let mut fixture = Fixture::new();
            add_ddr5_module(&mut fixture, 0, 100 << 2);
            fixture.set(smbus_byte_key(FIRST_ADDRESS, 0, 0x05), capability);
            fixture.set(smbus_byte_key(FIRST_ADDRESS, 0, 0x1A), configuration);
            let survey = survey(&mut ReplaySmbus::new(fixture));
            assert!(!survey.modules[0].has_temperature_sensor());
            assert!(
                survey.modules[0]
                    .section
                    .rows
                    .contains(&("Thermal sensor".into(), expected.into())),
                "{:?}",
                survey.modules[0].section.rows
            );
        }
    }

    #[test]
    fn a_non_module_at_a_memory_address_is_listed_and_never_sampled() {
        // A board EEPROM at 0x52 that is neither DDR3, DDR4 nor an SPD5118.
        let mut fixture = Fixture::new();
        for (offset, value) in [(0, 0x55), (1, 0xAA), (2, 0x7E)] {
            fixture.set(smbus_byte_key(0x52, 0, offset), value);
        }
        let survey = survey(&mut ReplaySmbus::new(fixture));
        let [module] = survey.modules.as_slice() else { panic!("{:?}", survey.modules) };
        assert_eq!(module.generation, Generation::Unknown(0x7E));
        assert!(!module.has_temperature_sensor());
        assert_eq!(module.section.rows[0].1, "not a memory module (SPD byte 2 reads 0x7e)");
    }

    #[test]
    fn a_ddr3_module_is_named_and_not_decoded() {
        let mut fixture = Fixture::new();
        for (offset, value) in [(0, 0x92), (1, 0x10), (2, 0x0B)] {
            fixture.set(smbus_byte_key(0x50, 0, offset), value);
        }
        let survey = survey(&mut ReplaySmbus::new(fixture));
        assert_eq!(survey.modules[0].generation, Generation::Ddr3);
        assert_eq!(survey.modules[0].section.rows[0], ("Type".into(), "DDR3 SDRAM".into()));
        assert!(!survey.modules[0].has_temperature_sensor());
    }

    #[test]
    fn modules_in_several_slots_share_one_page_one_pass() {
        let mut fixture = Fixture::new();
        add_ddr4_module(&mut fixture, 0, 0x9401);
        add_ddr4_module(&mut fixture, 2, 0x8002);
        let mut bus = ReplaySmbus::new(fixture);
        #[cfg(feature = "spd-paging")]
        let log = bus.page_select_log();
        let survey = survey(&mut bus);
        let slots: Vec<u8> = survey.modules.iter().map(|m| m.slot).collect();
        assert_eq!(slots, [0, 2]);
        #[cfg(feature = "spd-paging")]
        assert_eq!(*log.lock().unwrap(), [0, 1, 0], "one page 1 pass for both modules");
    }

    #[test]
    fn an_empty_bus_finds_no_module_and_selects_no_page() {
        let mut bus = ReplaySmbus::new(Fixture::new());
        #[cfg(feature = "spd-paging")]
        let log = bus.page_select_log();
        let survey = survey(&mut bus);
        assert_eq!(survey, Survey { modules: vec![], stopped: None });
        #[cfg(feature = "spd-paging")]
        assert!(log.lock().unwrap().is_empty());
    }

    /// An SMBus whose every transfer fails with `error`.
    struct FailingSmbus {
        error: BusError,
        transfers: Arc<std::sync::Mutex<u32>>,
    }

    impl Smbus for FailingSmbus {
        fn read_byte_data(&mut self, _: u8, _: u8) -> Result<u8, BusError> {
            *self.transfers.lock().unwrap_or_else(PoisonError::into_inner) += 1;
            Err(self.error.clone())
        }
        fn read_word_data(&mut self, _: u8, _: u8) -> Result<u16, BusError> {
            *self.transfers.lock().unwrap_or_else(PoisonError::into_inner) += 1;
            Err(self.error.clone())
        }
        #[cfg(feature = "spd-paging")]
        fn select_spd_page(&mut self, _: u8) -> Result<(), BusError> {
            *self.transfers.lock().unwrap_or_else(PoisonError::into_inner) += 1;
            Err(self.error.clone())
        }
    }

    fn timeout() -> BusError {
        BusError::Driver(syspilot_pawnio::Error::Timeout { function: "ioctl_smbus_xfer".into() })
    }

    #[test]
    fn a_timeout_stops_the_scan_at_the_first_transfer() {
        let transfers = Arc::new(std::sync::Mutex::new(0));
        let mut bus = FailingSmbus { error: timeout(), transfers: transfers.clone() };
        let survey = survey(&mut bus);
        assert_eq!(*transfers.lock().unwrap(), 1, "nothing is retried and no other slot is tried");
        assert!(survey.modules.is_empty());
        assert!(survey.stopped.unwrap().starts_with("slot 0: "));
    }

    // -- the provider -------------------------------------------------------

    fn probe(fixture: Fixture, locks: Arc<FakeBusLocks>) -> Result<DimmProbe, DimmProbeError> {
        DimmProvider::probe(Box::new(ReplaySmbus::new(fixture)), locks, "Intel PCH (SmbusI801)")
    }

    #[test]
    fn probe_declares_one_temperature_per_module_with_a_sensor() {
        let mut fixture = Fixture::new();
        add_ddr4_module(&mut fixture, 0, 0x9401);
        add_ddr4_module(&mut fixture, 3, 0x8002);
        let locks = Arc::new(FakeBusLocks::free());
        let probed = probe(fixture, locks.clone()).unwrap();
        assert_eq!(probed.modules, 2);
        assert_eq!(probed.info.len(), 2);
        assert_eq!(
            probed.summary,
            "Intel PCH (SmbusI801): 2 SPD device(s) in slot(s) 0, 3; 2 with a temperature sensor"
        );
        let provider = probed.provider.expect("a provider");
        let specs = provider.sensors();
        let keys: Vec<&str> = specs.iter().map(|spec| spec.key.as_str()).collect();
        assert_eq!(keys, ["memory.dimm0.temp", "memory.dimm3.temp"]);
        assert!(
            specs
                .iter()
                .all(|s| s.class == DeviceClass::Memory && s.kind == SensorKind::Temperature)
        );
        assert_eq!(specs[1].device, "DIMM 3");
        assert_eq!(provider.period_secs(), 5);
        assert!(provider.needs_driver());
        assert_eq!(locks.live_guards(), 0);
        assert_eq!(locks.acquired(), [BusKind::Smbus]);
    }

    #[test]
    fn samples_decode_each_module_and_take_the_smbus_mutex_once() {
        let mut fixture = Fixture::new();
        add_ddr4_module(&mut fixture, 0, 0x9401);
        add_ddr5_module(&mut fixture, 1, 101 << 2);
        let locks = Arc::new(FakeBusLocks::free());
        let mut provider = probe(fixture, locks.clone()).unwrap().provider.unwrap();
        let mut out = Vec::new();
        provider.sample(&mut out);
        assert_eq!(out, [Some(25.25), Some(25.25)]);
        assert_eq!(
            locks.acquired(),
            [BusKind::Smbus, BusKind::Smbus],
            "one for the probe, one for the sample"
        );
        assert_eq!(locks.live_guards(), 0);
    }

    #[cfg(feature = "spd-paging")]
    #[test]
    fn a_sample_makes_no_write_and_does_not_touch_the_spd_page() {
        let mut fixture = one_ddr4_module();
        add_ddr5_module(&mut fixture, 4, 100 << 2);
        let bus = ReplaySmbus::new(fixture);
        let log = bus.page_select_log();
        let locks = Arc::new(FakeBusLocks::free());
        let mut provider =
            DimmProvider::probe(Box::new(bus), locks, "x").unwrap().provider.unwrap();
        log.lock().unwrap().clear();
        provider.sample(&mut Vec::new());
        assert!(log.lock().unwrap().is_empty(), "(sample): expected no page select");
    }

    #[test]
    fn a_held_bus_skips_the_sample_with_none_values() {
        let locks = Arc::new(FakeBusLocks::free());
        let mut provider = probe(one_ddr4_module(), locks).unwrap().provider.unwrap();
        provider.locks = Arc::new(FakeBusLocks::held_by_another_program(&[BusKind::Smbus]));
        let mut out = Vec::new();
        provider.sample(&mut out);
        assert_eq!(out, [None]);
        assert_eq!(provider.backoff.failures(), 0, "contention is not a fault");
        assert!(provider.last_problem().unwrap().contains("held by another program"));
    }

    #[test]
    fn a_bus_fault_during_a_sample_gives_none_and_backs_off() {
        let locks = Arc::new(FakeBusLocks::free());
        let mut provider = probe(one_ddr4_module(), locks.clone()).unwrap().provider.unwrap();
        let transfers = Arc::new(std::sync::Mutex::new(0));
        provider.smbus = Box::new(FailingSmbus { error: timeout(), transfers: transfers.clone() });
        let mut out = Vec::new();
        provider.sample(&mut out);
        assert_eq!(out, [None]);
        assert_eq!(locks.live_guards(), 0);
        assert_eq!(provider.backoff.failures(), 1);
        // The next sample is skipped: no transfer is attempted.
        let before = *transfers.lock().unwrap();
        provider.sample(&mut Vec::new());
        assert_eq!(*transfers.lock().unwrap(), before);
    }

    #[test]
    fn a_module_that_stops_answering_reads_none_and_the_others_still_read() {
        let mut fixture = Fixture::new();
        add_ddr4_module(&mut fixture, 0, 0x9401);
        add_ddr4_module(&mut fixture, 1, 0x8002);
        let locks = Arc::new(FakeBusLocks::free());
        let mut provider = probe(fixture.clone(), locks).unwrap().provider.unwrap();
        // The first module's sensor disappears from the bus.
        let mut gone = Fixture::new();
        for (key, value) in fixture.entries() {
            if key != smbus_word_key(0x18, 5) {
                gone.set(key, value);
            }
        }
        provider.smbus = Box::new(ReplaySmbus::new(gone));
        let mut out = Vec::new();
        provider.sample(&mut out);
        assert_eq!(out, [None, Some(40.0)]);
        assert_eq!(provider.backoff.failures(), 0);
    }

    #[test]
    fn an_empty_bus_reports_exactly_what_was_scanned_and_makes_no_provider() {
        let locks = Arc::new(FakeBusLocks::free());
        let probed = probe(Fixture::new(), locks).unwrap();
        assert!(probed.provider.is_none());
        assert_eq!(probed.modules, 0);
        assert!(probed.info.is_empty());
        assert_eq!(
            probed.summary,
            "Intel PCH (SmbusI801): nothing acknowledged at SMBus addresses 0x50-0x57, so no \
             memory module is reachable through this controller"
        );
    }

    #[test]
    fn a_module_without_a_sensor_is_listed_but_gets_no_provider() {
        let mut fixture = Fixture::new();
        add_ddr4_module(&mut fixture, 0, 0x9401);
        fixture.set(smbus_byte_key(FIRST_ADDRESS, 0, 14), 0x00);
        let probed = probe(fixture, Arc::new(FakeBusLocks::free())).unwrap();
        assert!(probed.provider.is_none());
        assert_eq!(probed.modules, 1);
        assert_eq!(probed.info.len(), 1);
        assert!(probed.summary.contains("0 with a temperature sensor"), "{}", probed.summary);
        assert!(probed.info[0].rows.contains(&("Thermal sensor".into(), "none declared".into())));
    }

    #[test]
    fn a_declared_sensor_that_reads_nonsense_is_not_sampled() {
        let mut fixture = Fixture::new();
        add_ddr4_module(&mut fixture, 0, 0xFFFF);
        let probed = probe(fixture, Arc::new(FakeBusLocks::free())).unwrap();
        assert!(probed.provider.is_none());
        let rows = &probed.info[0].rows;
        assert!(
            rows.contains(&(
                "Thermal sensor".into(),
                "declared, at 0x18, but the reading is not plausible".into()
            )),
            "{rows:?}"
        );
    }

    #[test]
    fn a_held_bus_at_probe_is_an_error_and_nothing_is_sent() {
        let locks = Arc::new(FakeBusLocks::held_by_another_program(&[BusKind::Smbus]));
        let Err(error) = probe(one_ddr4_module(), locks) else { panic!("expected an error") };
        assert_eq!(error, DimmProbeError::Bus(BusError::Contended(BusKind::Smbus)));
    }

    #[test]
    fn a_first_transfer_that_is_not_a_no_device_error_stops_everything() {
        let transfers = Arc::new(std::sync::Mutex::new(0));
        let invalid = BusError::Driver(syspilot_pawnio::Error::InvalidParameter {
            function: "ioctl_smbus_xfer".into(),
        });
        let smbus = FailingSmbus { error: invalid.clone(), transfers: transfers.clone() };
        let locks = Arc::new(FakeBusLocks::free());
        let Err(error) = DimmProvider::probe(Box::new(smbus), locks.clone(), "x") else {
            panic!("expected an error");
        };
        assert_eq!(error, DimmProbeError::FirstTransferFailed(invalid));
        assert_eq!(*transfers.lock().unwrap(), 1, "exactly one transfer was attempted");
        assert_eq!(locks.live_guards(), 0);
        assert!(error.to_string().starts_with(
            "(dimm): expected the first SMBus read to answer or find no device | received: "
        ));
    }

    #[test]
    fn the_intel_controller_identity_names_the_pci_device_and_io_base() {
        // As read from the reference machine through SmbusI801.
        assert_eq!(
            describe_controller([0x3130_3869, 0xF000, 0x7270_8086_8C22_8086]),
            "i801 SMBus controller 8086:8C22 at I/O 0xf000"
        );
    }

    #[test]
    fn the_amd_controller_identity_uses_the_five_character_type() {
        // The layout SmbusPIIX4 documents: "PIIX4", base 0xB00, FCH SMBus 1022:790B.
        let kind = u64::from_le_bytes([b'P', b'I', b'I', b'X', b'4', 0, 0, 0]);
        assert_eq!(
            // The PCI dword at offset 0 is device ID in the high half, vendor ID in the low one.
            describe_controller([kind, 0x0B00, 0x790B_1022]),
            "PIIX4 SMBus controller 1022:790B at I/O 0xb00"
        );
    }

    #[test]
    fn an_unprintable_type_string_is_marked_not_trusted() {
        assert_eq!(
            describe_controller([0x0000_0000_0001_4141, 0, 0]),
            "AA? SMBus controller 0000:0000 at I/O 0x0"
        );
    }

    #[test]
    fn a_scan_that_stopped_before_any_module_does_not_claim_the_bus_is_empty() {
        let survey = Survey { modules: vec![], stopped: Some("slot 3: boom".into()) };
        assert_eq!(
            summarise("ctl", &survey, 0),
            "ctl: the scan of SMBus addresses 0x50-0x57 stopped: slot 3: boom"
        );
    }

    #[test]
    fn a_scan_that_stopped_after_finding_a_module_says_so() {
        let mut fixture = Fixture::new();
        add_ddr5_module(&mut fixture, 2, 100 << 2);
        let found = survey(&mut ReplaySmbus::new(fixture));
        let survey = Survey { stopped: Some("page select: boom".into()), ..found };
        assert_eq!(
            summarise("ctl", &survey, 1),
            "ctl: 1 SPD device(s) in slot(s) 2; 1 with a temperature sensor; the scan stopped \
             early: page select: boom"
        );
    }
}
