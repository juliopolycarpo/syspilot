//! Parser for the SMBIOS structure table that firmware hands to Windows.
//!
//! The table is a sequence of structures. Each starts with a 4-byte header
//! (type, length of the formatted area, handle), then the formatted area,
//! then a set of NUL-terminated strings closed by an empty string (two NULs
//! in a row; a structure with no strings has just those two NULs). Fields in
//! the formatted area refer to the strings by 1-based index, 0 meaning "none".
//!
//! Five structure types are decoded: BIOS (0), system (1), baseboard (2),
//! processor (4) and memory device (17). Every other type is skipped by its
//! length. The parser is a pure function of bytes and never panics on bad
//! input: a structure that is shorter than the spec's version of it simply
//! leaves the later fields `None`, and a table that is damaged beyond that
//! stops the walk with a [`SmbiosError`] while keeping what was read before.
//!
//! Serial numbers, asset tags and the system UUID are parsed so that callers
//! and tests can see they were understood, but nothing in this crate shows
//! them: they identify one machine.

use std::fmt;

use crate::os::bytes::{read_u8, read_u16, read_u32};
use crate::os::text::clean_text;

/// Smallest possible structure: the header alone.
const HEADER_BYTES: usize = 4;
/// Structure type that ends the table.
const TYPE_END_OF_TABLE: u8 = 127;

/// Shortest formatted areas this parser accepts for each type: the length
/// of the oldest revision of the structure that still holds the fields the
/// decoder reads unconditionally.
const MIN_BIOS_BYTES: usize = 0x12;
const MIN_SYSTEM_BYTES: usize = 0x08;
const MIN_BOARD_BYTES: usize = 0x08;
const MIN_PROCESSOR_BYTES: usize = 0x1A;
const MIN_MEMORY_DEVICE_BYTES: usize = 0x15;

/// Everything decoded from a table.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Smbios {
    pub bios: Option<BiosRecord>,
    pub system: Option<SystemRecord>,
    pub board: Option<BoardRecord>,
    /// Every processor socket, populated or not.
    pub processors: Vec<ProcessorRecord>,
    /// Every memory slot, installed or not.
    pub memory_devices: Vec<MemoryDeviceRecord>,
}

/// The result of [`parse`]: the structures read, and the problem that ended
/// the walk early, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub tables: Smbios,
    pub error: Option<SmbiosError>,
}

/// Type 0: BIOS information.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BiosRecord {
    pub vendor: Option<String>,
    pub version: Option<String>,
    /// As the firmware writes it, usually `MM/DD/YYYY`.
    pub release_date: Option<String>,
}

/// Type 1: system information.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SystemRecord {
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub version: Option<String>,
    /// Identifies the machine; never shown.
    pub serial: Option<String>,
    /// Identifies the machine; never shown.
    pub uuid: Option<[u8; 16]>,
    pub sku: Option<String>,
    pub family: Option<String>,
}

/// Type 2: baseboard (motherboard) information.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BoardRecord {
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub version: Option<String>,
    /// Identifies the board; never shown.
    pub serial: Option<String>,
    /// Identifies the board; never shown.
    pub asset_tag: Option<String>,
}

/// Type 4: processor information.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProcessorRecord {
    pub socket: Option<String>,
    pub manufacturer: Option<String>,
    /// The processor's name as the firmware reads it from the part.
    pub version: Option<String>,
    /// Whether a processor is installed in the socket.
    pub populated: bool,
    pub max_speed_mhz: Option<u32>,
    pub current_speed_mhz: Option<u32>,
    pub core_count: Option<u32>,
    pub cores_enabled: Option<u32>,
    pub thread_count: Option<u32>,
    /// Identifies the part; never shown.
    pub serial: Option<String>,
    /// Identifies the part; never shown.
    pub asset_tag: Option<String>,
}

/// What a memory slot holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MemorySize {
    /// The slot is empty.
    NotInstalled,
    /// A module is there but the firmware does not know its size.
    #[default]
    Unknown,
    Bytes(u64),
}

/// Type 17: memory device (a slot, with its module if one is installed).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MemoryDeviceRecord {
    /// Slot name, e.g. `DIMM_A1`.
    pub locator: Option<String>,
    pub bank: Option<String>,
    pub size: MemorySize,
    pub form_factor: Option<&'static str>,
    pub memory_type: Option<&'static str>,
    /// Rated speed in MT/s.
    pub speed_mts: Option<u32>,
    /// Speed the module runs at now, in MT/s.
    pub configured_speed_mts: Option<u32>,
    pub manufacturer: Option<String>,
    /// Identifies the module; never shown.
    pub serial: Option<String>,
    /// Identifies the module; never shown.
    pub asset_tag: Option<String>,
    pub part_number: Option<String>,
    /// Number of ranks, if reported.
    pub rank: Option<u8>,
}

/// Why the walk over the table stopped early.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SmbiosError {
    /// Fewer than 4 bytes are left, not enough for a structure header.
    TruncatedHeader { offset: usize, available: usize },
    /// The header's length byte is below the size of the header itself.
    StructureLength { offset: usize, length: u8 },
    /// The formatted area runs past the end of the table.
    TruncatedStructure { offset: usize, length: u8, available: usize },
    /// The table ends inside a structure's string set.
    UnterminatedStrings { offset: usize },
    /// A firmware-table buffer is shorter than its own header.
    FirmwareHeader { available: usize },
    /// A firmware-table header declares more table bytes than the buffer has.
    FirmwareLength { declared: usize, available: usize },
}

impl fmt::Display for SmbiosError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TruncatedHeader { offset, available } => write!(
                f,
                "(smbios): expected structure header of {HEADER_BYTES} bytes at offset {offset} | received: {available} bytes"
            ),
            Self::StructureLength { offset, length } => write!(
                f,
                "(smbios): expected structure length >= {HEADER_BYTES} at offset {offset} | received: {length}"
            ),
            Self::TruncatedStructure { offset, length, available } => write!(
                f,
                "(smbios): expected {length} bytes of structure data at offset {offset} | received: {available}"
            ),
            Self::UnterminatedStrings { offset } => write!(
                f,
                "(smbios): expected a string set ending in two NUL bytes for the structure at offset {offset} | received: end of table"
            ),
            Self::FirmwareHeader { available } => write!(
                f,
                "(smbios): expected firmware table header of {FIRMWARE_HEADER_BYTES} bytes | received: {available} bytes"
            ),
            Self::FirmwareLength { declared, available } => write!(
                f,
                "(smbios): expected {declared} bytes of table data after the {FIRMWARE_HEADER_BYTES}-byte header | received: {available}"
            ),
        }
    }
}

impl std::error::Error for SmbiosError {}

/// Size of the `RAW_SMBIOS_TABLE` header that `GetSystemFirmwareTable`
/// puts before the structures: `Used20CallingMethod`, major and minor
/// version, DMI revision (one byte each), then the data length (`u32`).
const FIRMWARE_HEADER_BYTES: usize = 8;

/// The SMBIOS structures inside a `GetSystemFirmwareTable('RSMB')` reply,
/// with the version the firmware declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FirmwareTable<'a> {
    pub major: u8,
    pub minor: u8,
    /// The structure table itself, ready for [`parse`].
    pub data: &'a [u8],
}

/// Separates the `RAW_SMBIOS_TABLE` header from the structure table.
pub fn split_firmware_table(buffer: &[u8]) -> Result<FirmwareTable<'_>, SmbiosError> {
    let Some(header) = buffer.get(..FIRMWARE_HEADER_BYTES) else {
        return Err(SmbiosError::FirmwareHeader { available: buffer.len() });
    };
    let declared = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
    let available = buffer.len() - FIRMWARE_HEADER_BYTES;
    let Some(data) = buffer[FIRMWARE_HEADER_BYTES..].get(..declared) else {
        return Err(SmbiosError::FirmwareLength { declared, available });
    };
    Ok(FirmwareTable { major: header[1], minor: header[2], data })
}

/// One structure of the table, with typed access to its fields.
struct Structure<'a> {
    kind: u8,
    /// Header and formatted area; field offsets in the spec index into this.
    formatted: &'a [u8],
    strings: Vec<&'a [u8]>,
}

impl Structure<'_> {
    fn u8_at(&self, offset: usize) -> Option<u8> {
        read_u8(self.formatted, offset)
    }

    fn u16_at(&self, offset: usize) -> Option<u16> {
        read_u16(self.formatted, offset)
    }

    fn u32_at(&self, offset: usize) -> Option<u32> {
        read_u32(self.formatted, offset)
    }

    /// The string whose index is stored at `offset`. `None` for index 0, an
    /// index outside the string set (damaged firmware), a field that is not
    /// in this revision of the structure, and text that is empty once
    /// cleaned.
    fn text_at(&self, offset: usize) -> Option<String> {
        let index = usize::from(self.u8_at(offset)?);
        let raw = self.strings.get(index.checked_sub(1)?)?;
        Some(clean_text(&String::from_utf8_lossy(raw))).filter(|text| !text.is_empty())
    }
}

/// Splits the table into structures and decodes the ones this module knows.
pub fn parse(table: &[u8]) -> Parsed {
    let mut tables = Smbios::default();
    let mut offset = 0;
    let mut error = None;
    while offset < table.len() {
        match read_structure(table, offset) {
            Ok((structure, next)) => {
                if structure.kind == TYPE_END_OF_TABLE {
                    break;
                }
                decode(&structure, &mut tables);
                offset = next;
            }
            Err(problem) => {
                error = Some(problem);
                break;
            }
        }
    }
    Parsed { tables, error }
}

/// Reads the structure at `offset` and returns it with the offset of the
/// next one.
fn read_structure(table: &[u8], offset: usize) -> Result<(Structure<'_>, usize), SmbiosError> {
    let available = table.len() - offset;
    if available < HEADER_BYTES {
        return Err(SmbiosError::TruncatedHeader { offset, available });
    }
    let kind = table[offset];
    let length = table[offset + 1];
    if usize::from(length) < HEADER_BYTES {
        return Err(SmbiosError::StructureLength { offset, length });
    }
    let strings_start = offset + usize::from(length);
    if strings_start > table.len() {
        return Err(SmbiosError::TruncatedStructure { offset, length, available });
    }
    let formatted = &table[offset..strings_start];

    let mut strings = Vec::new();
    let mut cursor = strings_start;
    loop {
        let Some(len) = table[cursor.min(table.len())..].iter().position(|&byte| byte == 0) else {
            return Err(SmbiosError::UnterminatedStrings { offset });
        };
        if len == 0 {
            // The empty string closes the set. A structure without strings
            // has a second NUL right after this one.
            cursor += 1;
            if strings.is_empty() {
                if table.get(cursor) != Some(&0) {
                    return Err(SmbiosError::UnterminatedStrings { offset });
                }
                cursor += 1;
            }
            break;
        }
        strings.push(&table[cursor..cursor + len]);
        cursor += len + 1;
    }
    Ok((Structure { kind, formatted, strings }, cursor))
}

fn decode(structure: &Structure<'_>, tables: &mut Smbios) {
    let long_enough = |minimum: usize| structure.formatted.len() >= minimum;
    match structure.kind {
        0 if long_enough(MIN_BIOS_BYTES) && tables.bios.is_none() => {
            tables.bios = Some(decode_bios(structure));
        }
        1 if long_enough(MIN_SYSTEM_BYTES) && tables.system.is_none() => {
            tables.system = Some(decode_system(structure));
        }
        2 if long_enough(MIN_BOARD_BYTES) && tables.board.is_none() => {
            tables.board = Some(decode_board(structure));
        }
        4 if long_enough(MIN_PROCESSOR_BYTES) => {
            tables.processors.push(decode_processor(structure));
        }
        17 if long_enough(MIN_MEMORY_DEVICE_BYTES) => {
            tables.memory_devices.push(decode_memory_device(structure));
        }
        _ => {}
    }
}

fn decode_bios(s: &Structure<'_>) -> BiosRecord {
    BiosRecord { vendor: s.text_at(0x04), version: s.text_at(0x05), release_date: s.text_at(0x08) }
}

fn decode_system(s: &Structure<'_>) -> SystemRecord {
    let uuid = s.formatted.get(0x08..0x18).and_then(|bytes| <[u8; 16]>::try_from(bytes).ok());
    SystemRecord {
        manufacturer: s.text_at(0x04),
        product: s.text_at(0x05),
        version: s.text_at(0x06),
        serial: s.text_at(0x07),
        uuid,
        sku: s.text_at(0x19),
        family: s.text_at(0x1A),
    }
}

fn decode_board(s: &Structure<'_>) -> BoardRecord {
    BoardRecord {
        manufacturer: s.text_at(0x04),
        product: s.text_at(0x05),
        version: s.text_at(0x06),
        serial: s.text_at(0x07),
        asset_tag: s.text_at(0x08),
    }
}

/// A count stored in one byte, where 0xFF means "see the 16-bit field" (SMBIOS
/// 3.0) and 0 means unknown.
fn count_with_extension(s: &Structure<'_>, byte_offset: usize, word_offset: usize) -> Option<u32> {
    let count = match s.u8_at(byte_offset)? {
        0xFF => s.u16_at(word_offset).map(u32::from)?,
        small => u32::from(small),
    };
    (count != 0).then_some(count)
}

/// A speed in MHz or MT/s where 0 means unknown.
fn nonzero(value: u16) -> Option<u32> {
    (value != 0).then_some(u32::from(value))
}

fn decode_processor(s: &Structure<'_>) -> ProcessorRecord {
    // Bit 6 of the status byte says whether the socket is populated. A
    // structure too old to have the byte is taken to describe a processor.
    let populated = s.u8_at(0x18).is_none_or(|status| status & 0x40 != 0);
    ProcessorRecord {
        socket: s.text_at(0x04),
        manufacturer: s.text_at(0x07),
        version: s.text_at(0x10),
        populated,
        max_speed_mhz: s.u16_at(0x14).and_then(nonzero),
        current_speed_mhz: s.u16_at(0x16).and_then(nonzero),
        core_count: count_with_extension(s, 0x23, 0x2A),
        cores_enabled: count_with_extension(s, 0x24, 0x2C),
        thread_count: count_with_extension(s, 0x25, 0x2E),
        serial: s.text_at(0x20),
        asset_tag: s.text_at(0x21),
    }
}

/// Decodes the size field of a memory device (`raw` at 0x0C, extended size
/// at 0x1C).
fn memory_size(s: &Structure<'_>) -> MemorySize {
    const MEBIBYTE: u64 = 1 << 20;
    const KIBIBYTE: u64 = 1 << 10;
    match s.u16_at(0x0C) {
        None | Some(0xFFFF) => MemorySize::Unknown,
        Some(0) => MemorySize::NotInstalled,
        // 0x7FFF: the real size, in MiB, is in the 31-bit extended field.
        Some(0x7FFF) => match s.u32_at(0x1C).map(|extended| u64::from(extended & 0x7FFF_FFFF)) {
            Some(mebibytes) if mebibytes != 0 => MemorySize::Bytes(mebibytes * MEBIBYTE),
            _ => MemorySize::Unknown,
        },
        // Bit 15 selects the unit: set means KiB, clear means MiB.
        Some(raw) if raw & 0x8000 != 0 => MemorySize::Bytes(u64::from(raw & 0x7FFF) * KIBIBYTE),
        Some(raw) => MemorySize::Bytes(u64::from(raw) * MEBIBYTE),
    }
}

/// A speed field that holds 0xFFFF when the real value is in a 32-bit field.
fn memory_speed(s: &Structure<'_>, word_offset: usize, extended_offset: usize) -> Option<u32> {
    match s.u16_at(word_offset)? {
        0 => None,
        0xFFFF => s.u32_at(extended_offset).filter(|&speed| speed != 0),
        speed => Some(u32::from(speed)),
    }
}

fn decode_memory_device(s: &Structure<'_>) -> MemoryDeviceRecord {
    MemoryDeviceRecord {
        locator: s.text_at(0x10),
        bank: s.text_at(0x11),
        size: memory_size(s),
        form_factor: s.u8_at(0x0E).and_then(form_factor_name),
        memory_type: s.u8_at(0x12).and_then(memory_type_name),
        speed_mts: memory_speed(s, 0x15, 0x54),
        configured_speed_mts: memory_speed(s, 0x20, 0x58),
        manufacturer: s.text_at(0x17),
        serial: s.text_at(0x18),
        asset_tag: s.text_at(0x19),
        part_number: s.text_at(0x1A),
        rank: s.u8_at(0x1B).map(|attributes| attributes & 0x0F).filter(|&rank| rank != 0),
    }
}

/// Name of an SMBIOS memory form factor code. `None` for "other", "unknown"
/// and codes this table does not have.
fn form_factor_name(code: u8) -> Option<&'static str> {
    Some(match code {
        0x03 => "SIMM",
        0x04 => "SIP",
        0x05 => "Chip",
        0x06 => "DIP",
        0x07 => "ZIP",
        0x08 => "Proprietary card",
        0x09 => "DIMM",
        0x0A => "TSOP",
        0x0B => "Row of chips",
        0x0C => "RIMM",
        0x0D => "SODIMM",
        0x0E => "SRIMM",
        0x0F => "FB-DIMM",
        0x10 => "Die",
        _ => return None,
    })
}

/// Name of an SMBIOS memory type code. `None` for "other", "unknown" and
/// codes this table does not have.
fn memory_type_name(code: u8) -> Option<&'static str> {
    Some(match code {
        0x03 => "DRAM",
        0x04 => "EDRAM",
        0x05 => "VRAM",
        0x06 => "SRAM",
        0x07 => "RAM",
        0x08 => "ROM",
        0x09 => "Flash",
        0x0A => "EEPROM",
        0x0B => "FEPROM",
        0x0C => "EPROM",
        0x0D => "CDRAM",
        0x0E => "3DRAM",
        0x0F => "SDRAM",
        0x10 => "SGRAM",
        0x11 => "RDRAM",
        0x12 => "DDR",
        0x13 => "DDR2",
        0x14 => "DDR2 FB-DIMM",
        0x18 => "DDR3",
        0x19 => "FBD2",
        0x1A => "DDR4",
        0x1B => "LPDDR",
        0x1C => "LPDDR2",
        0x1D => "LPDDR3",
        0x1E => "LPDDR4",
        0x1F => "Logical non-volatile device",
        0x20 => "HBM",
        0x21 => "HBM2",
        0x22 => "DDR5",
        0x23 => "LPDDR5",
        0x24 => "HBM3",
        _ => return None,
    })
}

/// Strings that firmware leaves in fields the manufacturer never filled in.
const PLACEHOLDERS: &[&str] = &[
    "default string",
    "to be filled by o.e.m.",
    "to be filled by oem",
    "not specified",
    "not applicable",
    "n/a",
    "none",
    "unknown",
    "o.e.m.",
    "oem",
    "system manufacturer",
    "system product name",
    "system version",
    "system serial number",
    "base board manufacturer",
    "base board product name",
    "base board version",
    "type1productconfigid",
];

/// Whether `text` is a placeholder rather than a real value, ignoring case.
pub fn is_placeholder(text: &str) -> bool {
    let text = text.trim();
    text.is_empty() || PLACEHOLDERS.iter().any(|placeholder| placeholder.eq_ignore_ascii_case(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The structure table of a real machine (AMI BIOS, an E5-2680 v4 and
    /// four 16 GiB DDR4 modules), reduced to the structure types this parser
    /// reads and a few neighbours it must skip (chassis, caches, memory
    /// arrays and mapped addresses). Every serial number, asset tag and the
    /// system UUID is replaced by an obviously fake value of the same
    /// length: serials read `FAKE-<type>-<field offset>-0000...` and the UUID
    /// is 11111111-2222-3333-4444-555555555555. The firmware left several
    /// text fields at the placeholder `Default string`.
    const FIXTURE: &[u8] = include_bytes!("fixtures/smbios-sanitized.bin");

    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;

    fn parsed_fixture() -> Smbios {
        let parsed = parse(FIXTURE);
        assert_eq!(parsed.error, None, "fixture table: expected a clean parse");
        parsed.tables
    }

    // ---- hand-built structures ---------------------------------------------

    /// A structure: header, formatted area up to `length` bytes (zero-padded
    /// from `fields`, which start at offset 4), then the string set.
    fn structure(kind: u8, length: u8, fields: &[(usize, &[u8])], strings: &[&str]) -> Vec<u8> {
        let mut bytes = vec![0u8; usize::from(length)];
        bytes[0] = kind;
        bytes[1] = length;
        bytes[2..4].copy_from_slice(&0x1234u16.to_le_bytes());
        for &(offset, value) in fields {
            bytes[offset..offset + value.len()].copy_from_slice(value);
        }
        for text in strings {
            bytes.extend_from_slice(text.as_bytes());
            bytes.push(0);
        }
        if strings.is_empty() {
            bytes.push(0);
        }
        bytes.push(0);
        bytes
    }

    fn end_of_table() -> Vec<u8> {
        structure(127, 4, &[], &[])
    }

    /// A memory device of the SMBIOS 3.3 length (0x5C), which includes the
    /// extended speed fields at 0x54 and 0x58.
    fn memory_device(fields: &[(usize, &[u8])], strings: &[&str]) -> Vec<u8> {
        structure(17, 0x5C, fields, strings)
    }

    fn with_size(raw: u16, extended: u32) -> Vec<u8> {
        memory_device(&[(0x0C, &raw.to_le_bytes()), (0x1C, &extended.to_le_bytes())], &[])
    }

    fn size_of_device(table: &[u8]) -> MemorySize {
        let parsed = parse(table);
        assert_eq!(parsed.error, None);
        parsed.tables.memory_devices[0].size
    }

    // ---- the real table ------------------------------------------------------

    #[test]
    fn fixture_bios_is_decoded() {
        let bios = parsed_fixture().bios.unwrap();
        assert_eq!(bios.vendor.as_deref(), Some("American Megatrends Inc."));
        assert_eq!(bios.version.as_deref(), Some("5.11"));
        assert_eq!(bios.release_date.as_deref(), Some("03/22/2023"));
    }

    #[test]
    fn fixture_system_keeps_placeholders_and_parses_but_does_not_expose_identifiers() {
        let system = parsed_fixture().system.unwrap();
        assert_eq!(system.manufacturer.as_deref(), Some("Default string"));
        assert!(is_placeholder(system.manufacturer.as_deref().unwrap()));
        assert_eq!(system.serial.as_deref(), Some("FAKE-1-7-00000"));
        let fake_uuid = [
            0x11, 0x11, 0x11, 0x11, 0x22, 0x22, 0x33, 0x33, 0x44, 0x44, 0x55, 0x55, 0x55, 0x55,
            0x55, 0x55,
        ];
        assert_eq!(system.uuid, Some(fake_uuid));
    }

    #[test]
    fn fixture_board_is_decoded() {
        let board = parsed_fixture().board.unwrap();
        assert_eq!(board.manufacturer.as_deref(), Some("MACHINIST"));
        assert_eq!(board.product.as_deref(), Some("E5-MR9A PRO"));
        assert_eq!(board.version.as_deref(), Some("V1.2"));
        assert_eq!(board.serial.as_deref(), Some("FAKE-2-7-000000"));
        assert_eq!(board.asset_tag.as_deref(), Some("FAKE-2-8-00000"));
    }

    #[test]
    fn fixture_processor_is_decoded() {
        let tables = parsed_fixture();
        assert_eq!(tables.processors.len(), 1);
        let cpu = &tables.processors[0];
        assert_eq!(cpu.socket.as_deref(), Some("SOCKET 0"));
        assert_eq!(cpu.manufacturer.as_deref(), Some("Intel"));
        assert_eq!(cpu.version.as_deref(), Some("Intel(R) Xeon(R) CPU E5-2680 v4 @ 2.40GHz"));
        assert!(cpu.populated);
        assert_eq!(cpu.max_speed_mhz, Some(4000));
        assert_eq!(cpu.current_speed_mhz, Some(2400));
        assert_eq!(
            (cpu.core_count, cpu.cores_enabled, cpu.thread_count),
            (Some(14), Some(14), Some(28))
        );
        assert_eq!(cpu.serial, None, "the firmware has no processor serial");
    }

    #[test]
    fn fixture_memory_modules_are_decoded() {
        let tables = parsed_fixture();
        let slots: Vec<_> = tables.memory_devices.iter().map(|m| m.locator.as_deref()).collect();
        assert_eq!(slots, [Some("DIMM_A1"), Some("DIMM_B1"), Some("DIMM_C1"), Some("DIMM_D1")]);
        for module in &tables.memory_devices {
            assert_eq!(module.size, MemorySize::Bytes(16 * GIB));
            assert_eq!(module.memory_type, Some("DDR4"));
            assert_eq!(module.form_factor, Some("DIMM"));
            assert_eq!((module.speed_mts, module.configured_speed_mts), (Some(2400), Some(2400)));
            assert_eq!(module.manufacturer.as_deref(), Some("Micron"));
            assert_eq!(module.part_number.as_deref(), Some("36ASF2G72PZ-2G1A2"));
            assert_eq!(module.rank, Some(2));
            assert!(module.serial.as_deref().is_some_and(|serial| serial.starts_with("FAKE-17-")));
        }
        assert_eq!(tables.memory_devices[0].bank.as_deref(), Some("NODE 1"));
        assert_eq!(tables.memory_devices[2].bank.as_deref(), Some("NODE 2"));
    }

    #[test]
    fn fixture_contains_no_identifier_that_looks_real() {
        // The serial and asset-tag fields of every decoded structure carry
        // the `FAKE-` marker or are a placeholder. A real value there means
        // the fixture was regenerated without sanitising.
        let tables = parsed_fixture();
        let mut identifiers: Vec<&str> = Vec::new();
        let system = tables.system.as_ref().unwrap();
        let board = tables.board.as_ref().unwrap();
        identifiers.extend(system.serial.as_deref());
        identifiers.extend(board.serial.as_deref());
        identifiers.extend(board.asset_tag.as_deref());
        for cpu in &tables.processors {
            identifiers.extend(cpu.serial.as_deref());
            identifiers.extend(cpu.asset_tag.as_deref());
        }
        for module in &tables.memory_devices {
            identifiers.extend(module.serial.as_deref());
            identifiers.extend(module.asset_tag.as_deref());
        }
        for identifier in identifiers {
            assert!(
                identifier.starts_with("FAKE-") || is_placeholder(identifier),
                "fixture identifier: expected a FAKE- value or placeholder | received: {identifier:?}"
            );
        }
        assert_eq!(
            system.uuid,
            Some([
                0x11, 0x11, 0x11, 0x11, 0x22, 0x22, 0x33, 0x33, 0x44, 0x44, 0x55, 0x55, 0x55, 0x55,
                0x55, 0x55
            ])
        );
    }

    // ---- sizes -------------------------------------------------------------------

    #[test]
    fn memory_size_in_mebibytes() {
        assert_eq!(size_of_device(&with_size(8192, 0)), MemorySize::Bytes(8192 * MIB));
    }

    #[test]
    fn memory_size_with_the_kibibyte_flag() {
        // 0x8000 | 512: 512 KiB.
        assert_eq!(size_of_device(&with_size(0x8000 | 512, 0)), MemorySize::Bytes(512 * 1024));
    }

    #[test]
    fn memory_size_in_the_extended_field() {
        // 0x7FFF says "look at the extended size": 65 536 MiB = 64 GiB.
        assert_eq!(size_of_device(&with_size(0x7FFF, 65_536)), MemorySize::Bytes(64 * GIB));
        // Bit 31 of the extended field is reserved.
        assert_eq!(
            size_of_device(&with_size(0x7FFF, 0x8000_0000 | 32_768)),
            MemorySize::Bytes(32 * GIB)
        );
    }

    #[test]
    fn extended_size_marker_without_an_extended_size_is_unknown() {
        assert_eq!(size_of_device(&with_size(0x7FFF, 0)), MemorySize::Unknown);
        // A structure of the pre-2.7 length (0x1A) has no extended field to read.
        let short = structure(17, 0x1A, &[(0x0C, &0x7FFFu16.to_le_bytes())], &[]);
        assert_eq!(size_of_device(&short), MemorySize::Unknown);
    }

    #[test]
    fn empty_slot_is_not_installed_and_unreported_size_is_unknown() {
        assert_eq!(size_of_device(&with_size(0, 0)), MemorySize::NotInstalled);
        assert_eq!(size_of_device(&with_size(0xFFFF, 0)), MemorySize::Unknown);
    }

    #[test]
    fn empty_slot_keeps_its_name_so_it_can_be_counted() {
        let mut table = memory_device(&[(0x0C, &0u16.to_le_bytes()), (0x10, &[1])], &["DIMM_E1"]);
        table.extend(end_of_table());
        let device = &parse(&table).tables.memory_devices[0];
        assert_eq!(
            (device.size, device.locator.as_deref()),
            (MemorySize::NotInstalled, Some("DIMM_E1"))
        );
    }

    // ---- speeds -------------------------------------------------------------------

    #[test]
    fn speed_of_0xffff_is_read_from_the_extended_fields() {
        let table = memory_device(
            &[
                (0x0C, &1024u16.to_le_bytes()),
                (0x15, &0xFFFFu16.to_le_bytes()),
                (0x20, &0xFFFFu16.to_le_bytes()),
                (0x54, &8400u32.to_le_bytes()),
                (0x58, &8000u32.to_le_bytes()),
            ],
            &[],
        );
        let device = &parse(&table).tables.memory_devices[0];
        assert_eq!((device.speed_mts, device.configured_speed_mts), (Some(8400), Some(8000)));
    }

    #[test]
    fn zero_speed_is_unknown() {
        let table = memory_device(&[(0x0C, &1024u16.to_le_bytes())], &[]);
        let device = &parse(&table).tables.memory_devices[0];
        assert_eq!((device.speed_mts, device.configured_speed_mts), (None, None));
    }

    // ---- processor ---------------------------------------------------------------

    #[test]
    fn processor_counts_above_255_come_from_the_sixteen_bit_fields() {
        let table = structure(
            4,
            0x30,
            &[
                (0x18, &[0x41]),
                (0x23, &[0xFF]),
                (0x24, &[0xFF]),
                (0x25, &[0xFF]),
                (0x2A, &256u16.to_le_bytes()),
                (0x2C, &128u16.to_le_bytes()),
                (0x2E, &512u16.to_le_bytes()),
            ],
            &[],
        );
        let cpu = &parse(&table).tables.processors[0];
        assert_eq!(
            (cpu.core_count, cpu.cores_enabled, cpu.thread_count),
            (Some(256), Some(128), Some(512))
        );
    }

    #[test]
    fn processor_from_an_old_revision_has_no_counts_but_is_still_listed() {
        // SMBIOS 2.0: 0x1A bytes, no status-independent counts.
        let table = structure(4, 0x1A, &[(0x18, &[0x41]), (0x10, &[1])], &["Old CPU"]);
        let cpu = &parse(&table).tables.processors[0];
        assert_eq!(cpu.version.as_deref(), Some("Old CPU"));
        assert_eq!((cpu.core_count, cpu.thread_count), (None, None));
    }

    #[test]
    fn empty_socket_is_not_populated() {
        let table = structure(4, 0x30, &[(0x18, &[0x00]), (0x04, &[1])], &["SOCKET 1"]);
        let cpu = &parse(&table).tables.processors[0];
        assert!(!cpu.populated);
        assert_eq!(cpu.socket.as_deref(), Some("SOCKET 1"));
    }

    #[test]
    fn unreported_processor_speed_is_none() {
        let table = structure(4, 0x30, &[(0x18, &[0x41])], &[]);
        let cpu = &parse(&table).tables.processors[0];
        assert_eq!((cpu.max_speed_mhz, cpu.current_speed_mhz), (None, None));
    }

    // ---- strings -------------------------------------------------------------------

    #[test]
    fn string_fields_are_trimmed_and_padding_collapsed() {
        let table = memory_device(
            &[(0x0C, &1024u16.to_le_bytes()), (0x1A, &[1])],
            &["  M393A2K43   BB1-CRC  "],
        );
        let device = &parse(&table).tables.memory_devices[0];
        assert_eq!(device.part_number.as_deref(), Some("M393A2K43 BB1-CRC"));
    }

    #[test]
    fn string_index_zero_or_out_of_range_is_none() {
        let table = memory_device(
            &[(0x0C, &1024u16.to_le_bytes()), (0x10, &[0]), (0x11, &[9]), (0x17, &[2])],
            &["only one"],
        );
        let device = &parse(&table).tables.memory_devices[0];
        assert_eq!((device.locator.as_deref(), device.bank.as_deref()), (None, None));
        assert_eq!(device.manufacturer, None, "index 2 of a one-string set");
    }

    #[test]
    fn blank_string_is_none_and_non_utf8_bytes_do_not_fail() {
        let mut table = structure(2, 0x0F, &[(0x04, &[1]), (0x05, &[2])], &["   ", "caf\u{e9}"]);
        // Replace the UTF-8 "é" with a lone Latin-1 byte.
        let position = table.windows(2).position(|w| w == [0xC3, 0xA9]).unwrap();
        table.splice(position..position + 2, [0xE9]);
        let board = parse(&table).tables.board.unwrap();
        assert_eq!(board.manufacturer, None);
        assert_eq!(board.product.as_deref(), Some("caf\u{FFFD}"));
    }

    // ---- structure walking ----------------------------------------------------------

    #[test]
    fn end_of_table_stops_the_walk_even_with_bytes_after_it() {
        let mut table = structure(2, 0x0F, &[(0x04, &[1])], &["Board"]);
        table.extend(end_of_table());
        table.extend(structure(0, 0x18, &[(0x04, &[1])], &["Ignored"]));
        let parsed = parse(&table);
        assert_eq!(parsed.error, None);
        assert!(parsed.tables.board.is_some());
        assert!(parsed.tables.bios.is_none(), "structures after type 127 are not read");
    }

    #[test]
    fn table_without_an_end_marker_is_accepted() {
        let table = structure(2, 0x0F, &[(0x04, &[1])], &["Board"]);
        let parsed = parse(&table);
        assert_eq!(parsed.error, None);
        assert!(parsed.tables.board.is_some());
    }

    #[test]
    fn unknown_and_short_structures_are_skipped_not_errors() {
        let mut table = structure(200, 0x08, &[], &["vendor blob"]);
        table.extend(structure(17, 0x06, &[], &[])); // below the minimum for type 17
        table.extend(structure(2, 0x0F, &[(0x04, &[1])], &["Board"]));
        let parsed = parse(&table);
        assert_eq!(parsed.error, None);
        assert!(parsed.tables.memory_devices.is_empty());
        assert_eq!(parsed.tables.board.unwrap().manufacturer.as_deref(), Some("Board"));
    }

    #[test]
    fn only_the_first_bios_system_and_board_are_kept() {
        let mut table = structure(2, 0x0F, &[(0x04, &[1])], &["First"]);
        table.extend(structure(2, 0x0F, &[(0x04, &[1])], &["Second"]));
        assert_eq!(parse(&table).tables.board.unwrap().manufacturer.as_deref(), Some("First"));
    }

    #[test]
    fn empty_table_parses_to_nothing_without_error() {
        let parsed = parse(&[]);
        assert_eq!(parsed, Parsed { tables: Smbios::default(), error: None });
    }

    // ---- damaged tables ---------------------------------------------------------------

    #[test]
    fn structure_length_below_the_header_size_is_reported_with_its_offset() {
        let mut table = structure(2, 0x0F, &[(0x04, &[1])], &["Board"]);
        let bad_offset = table.len();
        table.extend([0x11, 0x02, 0x00, 0x00, 0x00, 0x00]);
        let parsed = parse(&table);
        let error = parsed.error.expect("a length of 2 must be an error");
        assert_eq!(error, SmbiosError::StructureLength { offset: bad_offset, length: 2 });
        assert_eq!(
            error.to_string(),
            format!(
                "(smbios): expected structure length >= 4 at offset {bad_offset} | received: 2"
            )
        );
        assert!(parsed.tables.board.is_some(), "structures before the damage are kept");
    }

    #[test]
    fn table_cut_inside_a_header_is_reported() {
        let mut table = structure(2, 0x0F, &[(0x04, &[1])], &["Board"]);
        let offset = table.len();
        table.extend([0x11, 0x14]);
        let parsed = parse(&table);
        assert_eq!(parsed.error, Some(SmbiosError::TruncatedHeader { offset, available: 2 }));
        assert_eq!(
            parsed.error.unwrap().to_string(),
            format!(
                "(smbios): expected structure header of 4 bytes at offset {offset} | received: 2 bytes"
            )
        );
    }

    #[test]
    fn table_cut_inside_the_formatted_area_is_reported() {
        let table = structure(2, 0x0F, &[(0x04, &[1])], &["Board"]);
        let cut = &table[..10];
        let parsed = parse(cut);
        assert_eq!(
            parsed.error,
            Some(SmbiosError::TruncatedStructure { offset: 0, length: 0x0F, available: 10 })
        );
        assert_eq!(
            parsed.error.unwrap().to_string(),
            "(smbios): expected 15 bytes of structure data at offset 0 | received: 10"
        );
        assert!(parsed.tables.board.is_none());
    }

    #[test]
    fn table_cut_inside_the_string_set_is_reported() {
        let table = structure(2, 0x0F, &[(0x04, &[1])], &["Board"]);
        // Drop the closing NULs and half of the text.
        let cut = &table[..table.len() - 4];
        let parsed = parse(cut);
        assert_eq!(parsed.error, Some(SmbiosError::UnterminatedStrings { offset: 0 }));
        assert!(parsed.tables.board.is_none(), "a structure with a damaged string set is dropped");
    }

    #[test]
    fn structure_without_strings_but_one_missing_nul_is_unterminated() {
        // Header + 11 formatted bytes + a single NUL.
        let mut bytes = vec![0u8; 15];
        bytes[0] = 2;
        bytes[1] = 15;
        bytes.push(0);
        assert_eq!(parse(&bytes).error, Some(SmbiosError::UnterminatedStrings { offset: 0 }));
    }

    #[test]
    fn every_truncation_of_the_real_table_is_survived_with_partial_results() {
        let full = parse(FIXTURE).tables;
        for length in 0..FIXTURE.len() {
            let parsed = parse(&FIXTURE[..length]);
            // Whatever was read is a prefix of the full table's structures,
            // so a cut never invents or reorders anything.
            let modules = &parsed.tables.memory_devices;
            assert_eq!(modules[..], full.memory_devices[..modules.len()], "cut at {length}");
            let sockets = &parsed.tables.processors;
            assert_eq!(sockets[..], full.processors[..sockets.len()], "cut at {length}");
            if let Some(bios) = &parsed.tables.bios {
                assert_eq!(Some(bios), full.bios.as_ref(), "cut at {length}");
            }
        }
        // The table cut after the BIOS structure still yields the BIOS.
        let after_bios = parse(&FIXTURE[..66]);
        assert_eq!(after_bios.error, None);
        assert_eq!(after_bios.tables.bios, full.bios);
    }

    #[test]
    fn every_byte_flip_of_the_real_table_is_survived() {
        // Damage must produce an error or odd values, never a panic.
        for index in 0..FIXTURE.len() {
            let mut damaged = FIXTURE.to_vec();
            damaged[index] ^= 0xFF;
            let _ = parse(&damaged);
        }
    }

    #[test]
    fn random_garbage_is_survived() {
        // A small deterministic generator (xorshift) instead of a dependency.
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        for round in 0..2_000 {
            let length = round % 300;
            let garbage: Vec<u8> = (0..length)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state as u8
                })
                .collect();
            let _ = parse(&garbage);
        }
    }

    // ---- firmware table wrapper ---------------------------------------------------------

    /// What `GetSystemFirmwareTable` returns: the 8-byte header, then the table.
    fn firmware_buffer(major: u8, minor: u8, data: &[u8]) -> Vec<u8> {
        let mut buffer = vec![0, major, minor, 0];
        buffer.extend((data.len() as u32).to_le_bytes());
        buffer.extend(data);
        buffer
    }

    #[test]
    fn firmware_buffer_is_split_into_version_and_table() {
        let buffer = firmware_buffer(3, 0, FIXTURE);
        let table = split_firmware_table(&buffer).unwrap();
        assert_eq!((table.major, table.minor), (3, 0));
        assert_eq!(table.data, FIXTURE);
    }

    #[test]
    fn firmware_buffer_ignores_padding_after_the_declared_length() {
        let mut buffer = firmware_buffer(2, 8, FIXTURE);
        buffer.extend([0xAA; 7]);
        assert_eq!(split_firmware_table(&buffer).unwrap().data, FIXTURE);
    }

    #[test]
    fn firmware_buffer_shorter_than_its_header_is_a_typed_error() {
        let error = split_firmware_table(&[0, 3, 0]).unwrap_err();
        assert_eq!(error, SmbiosError::FirmwareHeader { available: 3 });
        assert_eq!(
            error.to_string(),
            "(smbios): expected firmware table header of 8 bytes | received: 3 bytes"
        );
    }

    #[test]
    fn firmware_buffer_cut_inside_the_table_is_a_typed_error() {
        let buffer = firmware_buffer(3, 0, FIXTURE);
        let error = split_firmware_table(&buffer[..buffer.len() - 1]).unwrap_err();
        assert_eq!(
            error,
            SmbiosError::FirmwareLength { declared: FIXTURE.len(), available: FIXTURE.len() - 1 }
        );
        assert_eq!(
            error.to_string(),
            format!(
                "(smbios): expected {} bytes of table data after the 8-byte header | received: {}",
                FIXTURE.len(),
                FIXTURE.len() - 1
            )
        );
    }

    #[test]
    fn firmware_buffer_declaring_a_huge_length_does_not_overflow() {
        let mut buffer = vec![0, 3, 0, 0];
        buffer.extend(u32::MAX.to_le_bytes());
        assert!(matches!(
            split_firmware_table(&buffer),
            Err(SmbiosError::FirmwareLength { declared, available: 0 }) if declared == u32::MAX as usize
        ));
    }

    // ---- names and placeholders ---------------------------------------------------------

    #[test]
    fn memory_types_and_form_factors_have_names() {
        let types =
            [(0x18, "DDR3"), (0x1A, "DDR4"), (0x22, "DDR5"), (0x1E, "LPDDR4"), (0x23, "LPDDR5")];
        for (code, name) in types {
            assert_eq!(memory_type_name(code), Some(name), "memory type {code:#04x}");
        }
        assert_eq!(memory_type_name(0x02), None, "unknown");
        assert_eq!(memory_type_name(0xFE), None, "not in the table");
        assert_eq!(form_factor_name(0x09), Some("DIMM"));
        assert_eq!(form_factor_name(0x0D), Some("SODIMM"));
        assert_eq!(form_factor_name(0x02), None);
    }

    #[test]
    fn placeholders_are_recognised_whatever_their_case() {
        for text in
            ["Default string", "TO BE FILLED BY O.E.M.", " System Product Name ", "", "None"]
        {
            assert!(is_placeholder(text), "{text:?}: expected a placeholder");
        }
        for text in ["MACHINIST", "E5-MR9A PRO", "Default string 2"] {
            assert!(!is_placeholder(text), "{text:?}: expected a real value");
        }
    }
}
