//! Physical drive temperature, wear and throughput.
//!
//! Drives are opened as `\\.\PhysicalDriveN` with **zero desired access**,
//! which is enough for every query used here (`IOCTL_STORAGE_QUERY_PROPERTY`,
//! `IOCTL_DISK_PERFORMANCE`, `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX`) and means a
//! drive is never opened for reading data or writing.
//!
//! Throughput is a pure kernel counter read on every sample. Temperature and
//! wear send commands to the device (a SMART or NVMe log read), which can
//! wake a sleeping disk, so they are read at most every
//! [`HEALTH_MIN_INTERVAL`] and the last result is repeated in between.

use std::mem::offset_of;
use std::ptr;
use std::time::{Duration, Instant};

use syspilot_proto::sensor::{DeviceClass, SensorKind};
use syspilot_proto::win::{OwnedHandle, wide};
use windows_sys::Win32::Storage::FileSystem::{
    BusType1394, BusTypeAta, BusTypeAtapi, BusTypeFibre, BusTypeFileBackedVirtual, BusTypeMmc,
    BusTypeNvme, BusTypeSas, BusTypeSata, BusTypeScsi, BusTypeSd, BusTypeSpaces, BusTypeUfs,
    BusTypeUsb, BusTypeVirtual, BusTypeiScsi, CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    AtaDataTypeLogPage, DISK_GEOMETRY_EX, DISK_PERFORMANCE, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
    IOCTL_DISK_PERFORMANCE, IOCTL_STORAGE_QUERY_PROPERTY, NVMeDataTypeLogPage,
    PropertyStandardQuery, ProtocolTypeAta, ProtocolTypeNvme, STORAGE_DEVICE_DESCRIPTOR,
    STORAGE_PROPERTY_QUERY, STORAGE_PROTOCOL_DATA_DESCRIPTOR, STORAGE_PROTOCOL_SPECIFIC_DATA,
    STORAGE_TEMPERATURE_DATA_DESCRIPTOR, STORAGE_TEMPERATURE_INFO, StorageDeviceProperty,
    StorageDeviceProtocolSpecificProperty, StorageDeviceTemperatureProperty,
};

use super::rate::RateTracker;
use super::text::{clean_text, number_repeated_names};
use crate::hub::{Provider, SensorSpec};

/// Highest `PhysicalDriveN` index probed, exclusive. Drive numbers can have
/// gaps (a removed USB disk keeps its number free), so every index is tried.
const MAX_PHYSICAL_DRIVES: u32 = 32;
/// Shortest time between temperature and wear reads. Slightly under five
/// seconds so that one-second ticks that arrive a little early do not push a
/// read to the following tick.
const HEALTH_MIN_INTERVAL: Duration = Duration::from_millis(4_500);
/// Desired access for every drive handle: none. Query IOCTLs do not need
/// any, and not asking for read or write access means a drive can be opened
/// even when a file system holds it exclusively.
const DRIVE_DESIRED_ACCESS: u32 = 0;
/// Size of the log pages read: the NVMe health log (`NVME_HEALTH_INFO_LOG`)
/// and an ATA log page are both 512 bytes.
const LOG_PAGE_BYTES: usize = 512;
/// A log-page query's reply: the protocol descriptor, then the page.
const LOG_PAGE_REPLY_BYTES: usize = size_of::<STORAGE_PROTOCOL_DATA_DESCRIPTOR>() + LOG_PAGE_BYTES;
/// `NVME_LOG_PAGE_HEALTH_INFO` (`nvme.h`): the SMART / health log page.
const NVME_LOG_PAGE_HEALTH_INFO: u32 = 2;
/// ATA log address of the ACS-3 Device Statistics log.
const ATA_LOG_DEVICE_STATISTICS: u32 = 0x04;
/// Device Statistics page 5: temperature statistics.
const ATA_STATISTICS_TEMPERATURE_PAGE: u32 = 5;
/// Byte offset of "current temperature" in that page (first statistic after
/// the 8-byte header).
const ATA_CURRENT_TEMPERATURE_OFFSET: usize = 8;
/// Flag bits in the top byte of a statistic: "supported" (bit 7) and "valid"
/// (bit 6). Both must be set for the value to mean anything.
const ATA_STATISTIC_USABLE: u8 = 0xC0;
/// Bounds on a believable drive temperature, in degrees Celsius.
const PLAUSIBLE_CELSIUS: std::ops::RangeInclusive<f64> = -20.0..=125.0;

// Offsets used by the byte parsers below, checked against the bindings so a
// layout mistake fails the build instead of misreading a buffer.
const _: () = {
    assert!(offset_of!(STORAGE_DEVICE_DESCRIPTOR, Size) == 4);
    assert!(offset_of!(STORAGE_DEVICE_DESCRIPTOR, VendorIdOffset) == 12);
    assert!(offset_of!(STORAGE_DEVICE_DESCRIPTOR, ProductIdOffset) == 16);
    assert!(offset_of!(STORAGE_DEVICE_DESCRIPTOR, ProductRevisionOffset) == 20);
    assert!(offset_of!(STORAGE_DEVICE_DESCRIPTOR, BusType) == 28);
    assert!(offset_of!(STORAGE_TEMPERATURE_DATA_DESCRIPTOR, InfoCount) == 12);
    assert!(offset_of!(STORAGE_TEMPERATURE_DATA_DESCRIPTOR, TemperatureInfo) == 24);
    assert!(size_of::<STORAGE_TEMPERATURE_INFO>() == 16);
    assert!(offset_of!(STORAGE_TEMPERATURE_INFO, Temperature) == 2);
    assert!(offset_of!(STORAGE_PROPERTY_QUERY, AdditionalParameters) == 8);
    assert!(size_of::<STORAGE_PROPERTY_QUERY>() == 12);
    assert!(size_of::<STORAGE_PROTOCOL_SPECIFIC_DATA>() == 40);
    assert!(size_of::<STORAGE_PROTOCOL_DATA_DESCRIPTOR>() == 48);
    assert!(offset_of!(STORAGE_PROTOCOL_DATA_DESCRIPTOR, ProtocolSpecificData) == 8);
    assert!(offset_of!(STORAGE_PROTOCOL_SPECIFIC_DATA, ProtocolDataOffset) == 16);
    assert!(offset_of!(STORAGE_PROTOCOL_SPECIFIC_DATA, ProtocolDataLength) == 20);
    assert!(offset_of!(DISK_PERFORMANCE, BytesRead) == 0);
    assert!(offset_of!(DISK_PERFORMANCE, BytesWritten) == 8);
    assert!(offset_of!(DISK_GEOMETRY_EX, DiskSize) == 24);
};

/// Little-endian `u16` at `offset`, if the buffer is long enough.
fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?))
}

/// Little-endian `u32` at `offset`, if the buffer is long enough.
fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?))
}

/// Little-endian `u64` at `offset`, if the buffer is long enough.
fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(offset..offset.checked_add(8)?)?.try_into().ok()?))
}

/// What a drive reports about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DriveIdentity {
    /// Vendor and product, e.g. `CT500BX500SSD1`.
    pub model: String,
    /// Firmware revision.
    pub firmware: String,
    /// Bus name, e.g. `NVMe` or `SATA`.
    pub bus: &'static str,
    /// Capacity in bytes; zero if unknown.
    pub size_bytes: u64,
}

/// Cumulative bytes moved since the disk was brought online.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IoCounters {
    pub bytes_read: u64,
    pub bytes_written: u64,
}

/// Temperature and wear. A field is `None` when the drive does not report it.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Health {
    /// Degrees Celsius.
    pub temperature: Option<f64>,
    /// Percent of rated endurance left (NVMe: 100 minus "percentage used").
    pub life_remaining: Option<f64>,
}

/// The operations a drive answers.
pub(crate) trait DriveBackend: Send {
    /// Cumulative transfer counters; `None` if the read failed.
    fn io_counters(&mut self) -> Option<IoCounters>;
    /// Temperature and wear.
    fn health(&mut self) -> Health;
}

/// Opens drives by number.
pub(crate) trait DriveOpener {
    /// The drive with this `PhysicalDrive` number, or `None` if there is
    /// none (or it cannot be opened).
    fn open(&mut self, index: u32) -> Option<(DriveIdentity, Box<dyn DriveBackend>)>;
}

/// An opened physical drive.
pub(crate) struct Drive {
    /// The `PhysicalDriveN` number.
    pub index: u32,
    pub identity: DriveIdentity,
    backend: Box<dyn DriveBackend>,
}

/// Opens every drive `opener` has, trying all numbers: gaps are normal.
pub(crate) fn enumerate_drives(opener: &mut dyn DriveOpener) -> Vec<Drive> {
    (0..MAX_PHYSICAL_DRIVES)
        .filter_map(|index| {
            let (identity, backend) = opener.open(index)?;
            Some(Drive { index, identity, backend })
        })
        .collect()
}

/// Every physical drive of this machine.
pub(crate) fn drives() -> Vec<Drive> {
    enumerate_drives(&mut WindowsDrives)
}

// ---------------------------------------------------------------------
// Parsing of IOCTL buffers (pure)
// ---------------------------------------------------------------------

/// `STORAGE_BUS_TYPE` values and the names shown for them. A table instead
/// of a `match`: the bindings' constants are CamelCase, which Rust rejects as
/// patterns.
const BUS_TYPE_NAMES: [(i32, &str); 16] = [
    (BusTypeScsi, "SCSI"),
    (BusTypeAtapi, "ATAPI"),
    (BusTypeAta, "ATA"),
    (BusType1394, "IEEE 1394"),
    (BusTypeFibre, "Fibre Channel"),
    (BusTypeUsb, "USB"),
    (BusTypeiScsi, "iSCSI"),
    (BusTypeSas, "SAS"),
    (BusTypeSata, "SATA"),
    (BusTypeSd, "SD"),
    (BusTypeMmc, "MMC"),
    (BusTypeVirtual, "Virtual"),
    (BusTypeFileBackedVirtual, "Virtual"),
    (BusTypeSpaces, "Storage Spaces"),
    (BusTypeNvme, "NVMe"),
    (BusTypeUfs, "UFS"),
];

/// Name of a `STORAGE_BUS_TYPE` value.
fn bus_type_name(bus: u32) -> &'static str {
    BUS_TYPE_NAMES
        .iter()
        .find(|(value, _)| u32::try_from(*value) == Ok(bus))
        .map_or("Unknown", |&(_, name)| name)
}

/// The text of the NUL-terminated ASCII string at `offset` in `bytes`.
/// Offset zero means "absent" in `STORAGE_DEVICE_DESCRIPTOR`.
fn descriptor_string(bytes: &[u8], offset: u32) -> Option<String> {
    let start = usize::try_from(offset).ok().filter(|&start| start != 0)?;
    let tail = bytes.get(start..)?;
    let length = tail.iter().position(|&byte| byte == 0).unwrap_or(tail.len());
    Some(clean_text(&String::from_utf8_lossy(&tail[..length])))
}

/// `"vendor product"`, except that the vendor is dropped when it only names
/// the transport: SATA drives behind the SCSI translation layer report
/// `ATA`, and NVMe drives report `NVMe`.
fn drive_model(vendor: &str, product: &str) -> String {
    let transport_vendor = ["", "ATA", "NVMe"].iter().any(|v| v.eq_ignore_ascii_case(vendor));
    if transport_vendor { product.to_owned() } else { clean_text(&format!("{vendor} {product}")) }
}

/// Decodes a `STORAGE_DEVICE_DESCRIPTOR` (`valid` is the number of bytes the
/// IOCTL returned). The serial number is deliberately not read.
fn parse_device_descriptor(bytes: &[u8], valid: usize) -> Option<(String, String, &'static str)> {
    let bytes = bytes.get(..valid.min(bytes.len()))?;
    let declared = read_u32(bytes, offset_of!(STORAGE_DEVICE_DESCRIPTOR, Size))? as usize;
    // Strings are addressed from the start of the descriptor, and must lie
    // inside what the driver says it filled.
    let bytes = bytes.get(..declared.min(bytes.len()))?;
    let field = |offset: usize| descriptor_string(bytes, read_u32(bytes, offset)?);
    let vendor = field(offset_of!(STORAGE_DEVICE_DESCRIPTOR, VendorIdOffset)).unwrap_or_default();
    let product = field(offset_of!(STORAGE_DEVICE_DESCRIPTOR, ProductIdOffset)).unwrap_or_default();
    let firmware =
        field(offset_of!(STORAGE_DEVICE_DESCRIPTOR, ProductRevisionOffset)).unwrap_or_default();
    let bus = bus_type_name(read_u32(bytes, offset_of!(STORAGE_DEVICE_DESCRIPTOR, BusType))?);
    Some((drive_model(&vendor, &product), firmware, bus))
}

/// A temperature if it is believable for a drive.
fn plausible_celsius(celsius: f64) -> Option<f64> {
    PLAUSIBLE_CELSIUS.contains(&celsius).then_some(celsius)
}

/// The composite temperature from a `STORAGE_TEMPERATURE_DATA_DESCRIPTOR`:
/// the entry with sensor index 0, else the first valid entry.
fn parse_temperature_descriptor(bytes: &[u8], valid: usize) -> Option<f64> {
    let bytes = bytes.get(..valid.min(bytes.len()))?;
    let count = read_u16(bytes, offset_of!(STORAGE_TEMPERATURE_DATA_DESCRIPTOR, InfoCount))?;
    let first = offset_of!(STORAGE_TEMPERATURE_DATA_DESCRIPTOR, TemperatureInfo);
    let entries = (0..usize::from(count)).filter_map(|entry| {
        let base = first + entry * size_of::<STORAGE_TEMPERATURE_INFO>();
        let index = read_u16(bytes, base)?;
        let celsius = read_u16(bytes, base + offset_of!(STORAGE_TEMPERATURE_INFO, Temperature))?;
        Some((index, plausible_celsius(f64::from(celsius as i16))?))
    });
    let mut best: Option<(u16, f64)> = None;
    for (index, celsius) in entries {
        if best.is_none_or(|(best_index, _)| index < best_index) {
            best = Some((index, celsius));
        }
    }
    best.map(|(_, celsius)| celsius)
}

/// The two figures read from an NVMe health log.
#[derive(Debug, Clone, Copy, PartialEq)]
struct NvmeHealth {
    temperature: Option<f64>,
    life_remaining: f64,
}

/// The log page inside the reply to a protocol-specific query: a
/// `STORAGE_PROTOCOL_DATA_DESCRIPTOR` followed by the page, `valid` bytes in
/// all. Limited to what the driver says it returned and to what is there.
fn protocol_log_page(bytes: &[u8], valid: usize) -> Option<&[u8]> {
    let bytes = bytes.get(..valid.min(bytes.len()))?;
    let protocol = offset_of!(STORAGE_PROTOCOL_DATA_DESCRIPTOR, ProtocolSpecificData);
    let data_offset =
        read_u32(bytes, protocol + offset_of!(STORAGE_PROTOCOL_SPECIFIC_DATA, ProtocolDataOffset))?;
    let data_length =
        read_u32(bytes, protocol + offset_of!(STORAGE_PROTOCOL_SPECIFIC_DATA, ProtocolDataLength))?;
    // The page lives `ProtocolDataOffset` bytes after the protocol block.
    let start = protocol.checked_add(usize::try_from(data_offset).ok()?)?;
    let end = start.checked_add(usize::try_from(data_length).ok()?)?;
    bytes.get(start..end.min(bytes.len()))
}

/// Decodes the NVMe health log page (log 2).
fn parse_nvme_health(bytes: &[u8], valid: usize) -> Option<NvmeHealth> {
    let log = protocol_log_page(bytes, valid)?;
    // Bytes 0..6 of the page: critical warning, composite temperature (two
    // bytes, Kelvin), available spare, spare threshold, percentage used.
    if log.len() < 6 {
        return None;
    }
    let kelvin = read_u16(log, 1)?;
    let temperature = (kelvin != 0).then(|| f64::from(kelvin) - 273.0).and_then(plausible_celsius);
    let percentage_used = f64::from(log[5]);
    Some(NvmeHealth { temperature, life_remaining: (100.0 - percentage_used).clamp(0.0, 100.0) })
}

/// A `STORAGE_PROPERTY_QUERY` for `property_id`, followed by `extra` bytes
/// of query-specific parameters.
///
/// The input is padded to the full size of the structure (its one-byte
/// `AdditionalParameters` array included): the storage stack rejects a
/// shorter buffer with `ERROR_INVALID_PARAMETER`.
fn property_query(property_id: i32, extra: &[u8]) -> Vec<u8> {
    let header = offset_of!(STORAGE_PROPERTY_QUERY, AdditionalParameters);
    let mut query = Vec::with_capacity(header + extra.len());
    query.extend_from_slice(&property_id.to_le_bytes());
    query.extend_from_slice(&PropertyStandardQuery.to_le_bytes());
    query.extend_from_slice(extra);
    query.resize(query.len().max(size_of::<STORAGE_PROPERTY_QUERY>()), 0);
    query
}

/// A `StorageDeviceProtocolSpecificProperty` query for one 512-byte log page.
/// `address` and `page` are the protocol's log identifier and page number.
fn log_page_query(protocol: i32, data_type: i32, address: u32, page: u32) -> Vec<u8> {
    let fields: [u32; 10] = [
        protocol as u32,
        data_type as u32,
        address,                                            // ProtocolDataRequestValue
        page,                                               // ProtocolDataRequestSubValue
        size_of::<STORAGE_PROTOCOL_SPECIFIC_DATA>() as u32, // ProtocolDataOffset
        LOG_PAGE_BYTES as u32,                              // ProtocolDataLength
        0,                                                  // FixedProtocolReturnData
        0,                                                  // ProtocolDataRequestSubValue2
        0,                                                  // ProtocolDataRequestSubValue3
        0,                                                  // ProtocolDataRequestSubValue4
    ];
    let extra: Vec<u8> = fields.iter().flat_map(|field| field.to_le_bytes()).collect();
    let mut query = property_query(StorageDeviceProtocolSpecificProperty, &extra);
    // The driver wants the input at least as large as the reply.
    query.resize(query.len().max(LOG_PAGE_REPLY_BYTES), 0);
    query
}

/// The query that asks an NVMe drive for its health log page.
fn nvme_health_query() -> Vec<u8> {
    log_page_query(ProtocolTypeNvme, NVMeDataTypeLogPage, NVME_LOG_PAGE_HEALTH_INFO, 0)
}

/// The query that asks a SATA drive for its Device Statistics temperature
/// page.
fn ata_temperature_query() -> Vec<u8> {
    log_page_query(
        ProtocolTypeAta,
        AtaDataTypeLogPage,
        ATA_LOG_DEVICE_STATISTICS,
        ATA_STATISTICS_TEMPERATURE_PAGE,
    )
}

/// Decodes the ATA Device Statistics temperature page: the current
/// temperature of the drive. `None` when the page is not the one asked for or
/// the drive flags the value as unsupported or invalid.
fn parse_ata_temperature(bytes: &[u8], valid: usize) -> Option<f64> {
    let log = protocol_log_page(bytes, valid)?;
    // Header qword: revision in bits 15:0, page number in bits 23:16.
    if u32::from(*log.get(2)?) != ATA_STATISTICS_TEMPERATURE_PAGE {
        return None;
    }
    // Statistic qword 1: the value in bits 7:0 (signed degrees Celsius) and
    // flags in the top byte, bit 7 "supported" and bit 6 "valid".
    let flags = *log.get(ATA_CURRENT_TEMPERATURE_OFFSET + 7)?;
    if flags & ATA_STATISTIC_USABLE != ATA_STATISTIC_USABLE {
        return None;
    }
    plausible_celsius(f64::from(*log.get(ATA_CURRENT_TEMPERATURE_OFFSET)? as i8))
}

// ---------------------------------------------------------------------
// The Windows backend
// ---------------------------------------------------------------------

/// Sends `code` to `handle`. Returns the number of output bytes.
fn ioctl(handle: &OwnedHandle, code: u32, input: &[u8], output: &mut [u8]) -> Option<usize> {
    let mut returned = 0u32;
    let input_pointer = if input.is_empty() { ptr::null() } else { input.as_ptr().cast() };
    // SAFETY: `handle` is a valid open handle; `input` and `output` are valid
    // for reads and writes of the lengths passed, which fit in `u32` because
    // every buffer in this module is a few hundred bytes; `returned` is a
    // valid out-pointer; the call is synchronous (no overlapped structure).
    let ok = unsafe {
        DeviceIoControl(
            handle.raw(),
            code,
            input_pointer,
            input.len() as u32,
            output.as_mut_ptr().cast(),
            output.len() as u32,
            &mut returned,
            ptr::null_mut(),
        )
    };
    (ok != 0).then_some(returned as usize)
}

struct PhysicalDrive {
    handle: OwnedHandle,
    is_nvme: bool,
}

impl PhysicalDrive {
    /// The generic temperature property. Drivers that do not implement it
    /// (the SATA AHCI driver among them) fail with `ERROR_INVALID_FUNCTION`.
    fn temperature_property(&self) -> Option<f64> {
        let query = property_query(StorageDeviceTemperatureProperty, &[]);
        let mut output = [0u8; 256];
        let valid = ioctl(&self.handle, IOCTL_STORAGE_QUERY_PROPERTY, &query, &mut output)?;
        parse_temperature_descriptor(&output, valid)
    }

    /// Sends a log-page query and returns the reply and its valid length.
    fn read_log_page(&self, query: &[u8]) -> Option<([u8; LOG_PAGE_REPLY_BYTES], usize)> {
        let mut output = [0u8; LOG_PAGE_REPLY_BYTES];
        let valid = ioctl(&self.handle, IOCTL_STORAGE_QUERY_PROPERTY, query, &mut output)?;
        Some((output, valid))
    }

    fn nvme_health(&self) -> Option<NvmeHealth> {
        let (output, valid) = self.read_log_page(&nvme_health_query())?;
        parse_nvme_health(&output, valid)
    }

    /// Temperature from the ACS-3 Device Statistics log, for SATA drives
    /// whose driver has no temperature property. Drives older than ACS-3
    /// fail the read.
    fn ata_temperature(&self) -> Option<f64> {
        let (output, valid) = self.read_log_page(&ata_temperature_query())?;
        parse_ata_temperature(&output, valid)
    }
}

impl DriveBackend for PhysicalDrive {
    fn io_counters(&mut self) -> Option<IoCounters> {
        let mut output = [0u8; size_of::<DISK_PERFORMANCE>()];
        let valid = ioctl(&self.handle, IOCTL_DISK_PERFORMANCE, &[], &mut output)?;
        let bytes = output.get(..valid)?;
        Some(IoCounters {
            bytes_read: read_u64(bytes, offset_of!(DISK_PERFORMANCE, BytesRead))?,
            bytes_written: read_u64(bytes, offset_of!(DISK_PERFORMANCE, BytesWritten))?,
        })
    }

    fn health(&mut self) -> Health {
        let nvme = if self.is_nvme { self.nvme_health() } else { None };
        // Cheapest source first; every fallback is one more command sent to
        // the drive, so stop at the first that answers.
        let temperature = nvme
            .and_then(|health| health.temperature)
            .or_else(|| self.temperature_property())
            .or_else(|| if self.is_nvme { None } else { self.ata_temperature() });
        Health { temperature, life_remaining: nvme.map(|health| health.life_remaining) }
    }
}

/// Opens `\\.\PhysicalDrive<index>` and identifies the drive.
struct WindowsDrives;

impl DriveOpener for WindowsDrives {
    fn open(&mut self, index: u32) -> Option<(DriveIdentity, Box<dyn DriveBackend>)> {
        let path = wide(&format!(r"\\.\PhysicalDrive{index}"));
        // SAFETY: `path` is NUL-terminated and outlives the call; the other
        // arguments are plain values or null.
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                DRIVE_DESIRED_ACCESS,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                OPEN_EXISTING,
                0,
                ptr::null_mut(),
            )
        };
        let handle = OwnedHandle::new(handle).ok()?;
        let identity = read_identity(&handle, index);
        let is_nvme = identity.bus == "NVMe";
        Some((identity, Box::new(PhysicalDrive { handle, is_nvme })))
    }
}

fn read_identity(handle: &OwnedHandle, index: u32) -> DriveIdentity {
    let query = property_query(StorageDeviceProperty, &[]);
    let mut descriptor = [0u8; 1024];
    let described = ioctl(handle, IOCTL_STORAGE_QUERY_PROPERTY, &query, &mut descriptor)
        .and_then(|valid| parse_device_descriptor(&descriptor, valid));
    let (model, firmware, bus) = described.unwrap_or_default();

    let mut geometry = [0u8; 128];
    let size_bytes = ioctl(handle, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, &[], &mut geometry)
        .and_then(|valid| read_u64(geometry.get(..valid)?, offset_of!(DISK_GEOMETRY_EX, DiskSize)))
        .unwrap_or(0);

    let model = if model.is_empty() { format!("Physical drive {index}") } else { model };
    DriveIdentity { model, firmware, bus: if bus.is_empty() { "Unknown" } else { bus }, size_bytes }
}

// ---------------------------------------------------------------------
// Providers
// ---------------------------------------------------------------------

/// A drive as the provider samples it.
struct SampledDrive {
    index: u32,
    device: String,
    backend: Box<dyn DriveBackend>,
    /// Whether the drive answers the transfer counter query.
    throughput: bool,
    /// Whether the drive reports a temperature / remaining life.
    temperature: bool,
    life: bool,
    read: RateTracker,
    write: RateTracker,
    /// Result of the last health read, repeated until the next one.
    health: Health,
}

impl SampledDrive {
    fn has_health_sensors(&self) -> bool {
        self.temperature || self.life
    }
}

/// Throughput, temperature and remaining life of the physical drives.
pub struct DriveProvider {
    drives: Vec<SampledDrive>,
    /// When the drives were last asked for temperature and wear.
    last_health_read: Instant,
}

impl DriveProvider {
    /// Builds the provider from `drives`, or `None` if no drive answers any
    /// sensor query. Which sensors a drive has is decided by probing it once
    /// at `now`; that first health read is kept as the first value.
    fn new(drives: Vec<Drive>, now: Instant) -> Option<Self> {
        let mut names: Vec<String> = drives.iter().map(|d| d.identity.model.clone()).collect();
        number_repeated_names(&mut names);

        let mut sampled = Vec::new();
        for (mut drive, device) in drives.into_iter().zip(names) {
            let throughput = drive.backend.io_counters().is_some();
            let health = drive.backend.health();
            let (temperature, life) =
                (health.temperature.is_some(), health.life_remaining.is_some());
            if !(throughput || temperature || life) {
                continue;
            }
            sampled.push(SampledDrive {
                index: drive.index,
                device,
                backend: drive.backend,
                throughput,
                temperature,
                life,
                read: RateTracker::default(),
                write: RateTracker::default(),
                health,
            });
        }
        (!sampled.is_empty()).then_some(Self { drives: sampled, last_health_read: now })
    }

    /// One sample taken at `now`, so tests control the elapsed time.
    fn sample_at(&mut self, now: Instant, out: &mut Vec<Option<f64>>) {
        let health_due =
            now.saturating_duration_since(self.last_health_read) >= HEALTH_MIN_INTERVAL;
        if health_due {
            self.last_health_read = now;
        }
        for drive in &mut self.drives {
            if drive.throughput {
                match drive.backend.io_counters() {
                    Some(counters) => {
                        out.push(drive.read.update(now, counters.bytes_read));
                        out.push(drive.write.update(now, counters.bytes_written));
                    }
                    None => out.extend([None, None]),
                }
            }
            if health_due && drive.has_health_sensors() {
                drive.health = drive.backend.health();
            }
            if drive.temperature {
                out.push(drive.health.temperature);
            }
            if drive.life {
                out.push(drive.health.life_remaining);
            }
        }
    }
}

impl Provider for DriveProvider {
    fn sensors(&self) -> Vec<SensorSpec> {
        let mut specs = Vec::new();
        for drive in &self.drives {
            let mut add = |suffix: &str, label: &str, kind| {
                specs.push(SensorSpec {
                    device: drive.device.clone(),
                    class: DeviceClass::Storage,
                    key: format!("disk{}.{suffix}", drive.index),
                    label: label.to_owned(),
                    kind,
                });
            };
            if drive.throughput {
                add("read", "Read", SensorKind::Throughput);
                add("write", "Write", SensorKind::Throughput);
            }
            if drive.temperature {
                add("temp", "Temperature", SensorKind::Temperature);
            }
            if drive.life {
                add("life", "Life remaining", SensorKind::Level);
            }
        }
        specs
    }

    fn sample(&mut self, out: &mut Vec<Option<f64>>) {
        self.sample_at(Instant::now(), out);
    }
}

/// The drive provider, if any physical drive answers a sensor query.
pub fn probe() -> Vec<Box<dyn Provider>> {
    match DriveProvider::new(drives(), Instant::now()) {
        Some(provider) => vec![Box::new(provider)],
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    const ONE_SECOND: Duration = Duration::from_secs(1);

    // ---- buffer builders -------------------------------------------------

    fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    /// A `STORAGE_DEVICE_DESCRIPTOR` with the strings appended after the
    /// 36-byte header, as drivers lay it out.
    fn device_descriptor(vendor: &str, product: &str, firmware: &str, bus: u32) -> Vec<u8> {
        let mut bytes = vec![0u8; 36];
        for (field, text) in [(12, vendor), (16, product), (20, firmware)] {
            if text.is_empty() {
                continue;
            }
            let string_offset = bytes.len() as u32;
            put_u32(&mut bytes, field, string_offset);
            bytes.extend_from_slice(text.as_bytes());
            bytes.push(0);
        }
        put_u32(&mut bytes, 28, bus);
        let size = bytes.len() as u32;
        put_u32(&mut bytes, 4, size);
        bytes
    }

    /// A `STORAGE_TEMPERATURE_DATA_DESCRIPTOR` with `(index, celsius)` entries.
    fn temperature_descriptor(entries: &[(u16, i16)]) -> Vec<u8> {
        let mut bytes = vec![0u8; 24];
        bytes[12..14].copy_from_slice(&(entries.len() as u16).to_le_bytes());
        for &(index, celsius) in entries {
            let mut entry = [0u8; 16];
            entry[0..2].copy_from_slice(&index.to_le_bytes());
            entry[2..4].copy_from_slice(&celsius.to_le_bytes());
            bytes.extend_from_slice(&entry);
        }
        bytes
    }

    /// The reply to a log-page query: the protocol descriptor, then a
    /// zeroed 512-byte page.
    fn log_reply() -> Vec<u8> {
        let mut bytes = vec![0u8; LOG_PAGE_REPLY_BYTES];
        put_u32(&mut bytes, 24, 40); // ProtocolDataOffset
        put_u32(&mut bytes, 28, LOG_PAGE_BYTES as u32); // ProtocolDataLength
        bytes
    }

    /// The reply to the NVMe health-log query.
    fn nvme_reply(kelvin: u16, percentage_used: u8) -> Vec<u8> {
        let mut bytes = log_reply();
        bytes[48 + 1..48 + 3].copy_from_slice(&kelvin.to_le_bytes());
        bytes[48 + 5] = percentage_used;
        bytes
    }

    /// The reply to an ATA Device Statistics query for `page`, whose current
    /// temperature statistic holds `celsius` with `flags` in its top byte.
    fn ata_reply(page: u8, celsius: u8, flags: u8) -> Vec<u8> {
        let mut bytes = log_reply();
        bytes[48] = 1; // revision
        bytes[48 + 2] = page;
        bytes[48 + 8] = celsius;
        bytes[48 + 15] = flags;
        bytes
    }

    // ---- descriptor parsing ---------------------------------------------

    #[test]
    fn sata_drive_model_drops_the_transport_vendor() {
        let bytes = device_descriptor("ATA     ", "CT500BX500SSD1", "M6CR061", BusTypeSata as u32);
        let (model, firmware, bus) = parse_device_descriptor(&bytes, bytes.len()).unwrap();
        assert_eq!((model.as_str(), firmware.as_str(), bus), ("CT500BX500SSD1", "M6CR061", "SATA"));
    }

    #[test]
    fn real_vendor_is_kept_in_the_model() {
        assert_eq!(drive_model("Samsung", "SSD 970 EVO"), "Samsung SSD 970 EVO");
        assert_eq!(drive_model("NVMe", "Asgard AN4+"), "Asgard AN4+");
        assert_eq!(drive_model("", "Disk"), "Disk");
        assert_eq!(drive_model("ata", "Disk"), "Disk", "the transport check ignores case");
    }

    #[test]
    fn descriptor_without_strings_has_empty_text_and_still_names_the_bus() {
        let bytes = device_descriptor("", "", "", BusTypeUsb as u32);
        assert_eq!(
            parse_device_descriptor(&bytes, bytes.len()),
            Some((String::new(), String::new(), "USB"))
        );
    }

    #[test]
    fn descriptor_string_pointing_outside_the_buffer_is_ignored() {
        let mut bytes = device_descriptor("", "Disk", "", BusTypeNvme as u32);
        put_u32(&mut bytes, 16, 5_000); // ProductIdOffset beyond the data
        let (model, _, bus) = parse_device_descriptor(&bytes, bytes.len()).unwrap();
        assert_eq!((model.as_str(), bus), ("", "NVMe"));
    }

    #[test]
    fn truncated_descriptors_do_not_panic() {
        let bytes = device_descriptor("ATA", "CT500BX500SSD1", "M6CR061", BusTypeSata as u32);
        for valid in 0..bytes.len() {
            // Must neither panic nor invent text beyond the valid bytes.
            let parsed = parse_device_descriptor(&bytes, valid);
            if let Some((model, firmware, _)) = parsed {
                assert!(model.len() <= "CT500BX500SSD1".len() && firmware.len() <= "M6CR061".len());
            }
        }
        assert_eq!(parse_device_descriptor(&bytes, 0), None, "no bytes returned");
        assert_eq!(parse_device_descriptor(&[], 99), None, "empty buffer");
    }

    #[test]
    fn descriptor_size_field_larger_than_the_buffer_is_clamped() {
        let mut bytes = device_descriptor("", "Disk", "", BusTypeSata as u32);
        put_u32(&mut bytes, 4, 1_000_000);
        let (model, _, _) = parse_device_descriptor(&bytes, bytes.len()).unwrap();
        assert_eq!(model, "Disk");
    }

    #[test]
    fn bus_type_values_have_names() {
        let cases = [(0, "Unknown"), (7, "USB"), (11, "SATA"), (17, "NVMe"), (12345, "Unknown")];
        for (value, name) in cases {
            assert_eq!(bus_type_name(value), name, "STORAGE_BUS_TYPE {value}");
        }
    }

    // ---- temperature descriptor -----------------------------------------

    #[test]
    fn temperature_descriptor_gives_the_composite_entry() {
        let bytes = temperature_descriptor(&[(0, 38)]);
        assert_eq!(parse_temperature_descriptor(&bytes, bytes.len()), Some(38.0));
    }

    #[test]
    fn temperature_descriptor_prefers_sensor_index_zero_over_later_sensors() {
        let bytes = temperature_descriptor(&[(2, 60), (0, 41), (1, 55)]);
        assert_eq!(parse_temperature_descriptor(&bytes, bytes.len()), Some(41.0));
    }

    #[test]
    fn temperature_descriptor_skips_invalid_entries() {
        let bytes = temperature_descriptor(&[(0, i16::MIN), (1, 0x7FFF), (2, 44)]);
        assert_eq!(parse_temperature_descriptor(&bytes, bytes.len()), Some(44.0));
        let all_bad = temperature_descriptor(&[(0, 500)]);
        assert_eq!(parse_temperature_descriptor(&all_bad, all_bad.len()), None);
    }

    #[test]
    fn temperature_descriptor_that_claims_more_entries_than_it_holds_is_safe() {
        let mut bytes = temperature_descriptor(&[(0, 36)]);
        bytes[12..14].copy_from_slice(&500u16.to_le_bytes());
        assert_eq!(parse_temperature_descriptor(&bytes, bytes.len()), Some(36.0));
        assert_eq!(parse_temperature_descriptor(&bytes, 20), None, "too short for the first entry");
        assert_eq!(parse_temperature_descriptor(&[], 0), None);
    }

    // ---- NVMe health log --------------------------------------------------

    #[test]
    fn nvme_health_gives_celsius_from_kelvin_and_life_from_percentage_used() {
        let bytes = nvme_reply(313, 7);
        let health = parse_nvme_health(&bytes, bytes.len()).unwrap();
        assert_eq!(health, NvmeHealth { temperature: Some(40.0), life_remaining: 93.0 });
    }

    #[test]
    fn nvme_wear_beyond_the_rated_endurance_leaves_zero_life_not_a_negative_one() {
        let bytes = nvme_reply(300, 255);
        assert_eq!(parse_nvme_health(&bytes, bytes.len()).unwrap().life_remaining, 0.0);
    }

    #[test]
    fn nvme_temperature_of_zero_kelvin_is_absent_but_life_is_kept() {
        let bytes = nvme_reply(0, 10);
        let health = parse_nvme_health(&bytes, bytes.len()).unwrap();
        assert_eq!((health.temperature, health.life_remaining), (None, 90.0));
    }

    #[test]
    fn nvme_reply_with_a_data_offset_outside_the_buffer_is_rejected() {
        let mut bytes = nvme_reply(313, 7);
        put_u32(&mut bytes, 24, 100_000);
        assert_eq!(parse_nvme_health(&bytes, bytes.len()), None);
        let mut short_log = nvme_reply(313, 7);
        put_u32(&mut short_log, 28, 3); // ProtocolDataLength too small for the fields read
        assert_eq!(parse_nvme_health(&short_log, short_log.len()), None);
    }

    #[test]
    fn truncated_nvme_replies_do_not_panic() {
        let bytes = nvme_reply(313, 7);
        for valid in [0, 1, 7, 8, 31, 47, 48, 49, 53, 54] {
            let parsed = parse_nvme_health(&bytes, valid);
            assert!(
                parsed.is_none() || valid >= 54,
                "valid={valid}: expected None | received: {parsed:?}"
            );
        }
    }

    // ---- ATA Device Statistics ----------------------------------------------

    #[test]
    fn ata_statistics_give_the_current_temperature() {
        let bytes = ata_reply(5, 46, 0xC0);
        assert_eq!(parse_ata_temperature(&bytes, bytes.len()), Some(46.0));
    }

    #[test]
    fn ata_statistic_not_flagged_supported_and_valid_is_ignored() {
        for flags in [0x00, 0x40, 0x80] {
            let bytes = ata_reply(5, 46, flags);
            assert_eq!(
                parse_ata_temperature(&bytes, bytes.len()),
                None,
                "flags {flags:#04x}: expected None | received a temperature"
            );
        }
    }

    #[test]
    fn ata_page_other_than_the_temperature_page_is_ignored() {
        let bytes = ata_reply(1, 46, 0xC0);
        assert_eq!(parse_ata_temperature(&bytes, bytes.len()), None);
    }

    #[test]
    fn ata_temperature_is_signed_and_implausible_values_are_dropped() {
        let below_zero = ata_reply(5, (-5i8) as u8, 0xC0);
        assert_eq!(parse_ata_temperature(&below_zero, below_zero.len()), Some(-5.0));
        let too_cold = ata_reply(5, (-100i8) as u8, 0xC0);
        assert_eq!(parse_ata_temperature(&too_cold, too_cold.len()), None);
    }

    #[test]
    fn truncated_ata_replies_do_not_panic() {
        let bytes = ata_reply(5, 46, 0xC0);
        // Header (8 bytes), then the log page; the statistic ends at page
        // offset 16, that is reply offset 48 + 16 = 64.
        for valid in 0..bytes.len() {
            let parsed = parse_ata_temperature(&bytes, valid);
            assert!(
                parsed.is_none() || valid >= 64,
                "valid={valid}: expected None | received: {parsed:?}"
            );
        }
    }

    // ---- query construction ----------------------------------------------

    #[test]
    fn property_query_is_the_property_id_the_standard_query_type_then_parameters() {
        let query = property_query(52, &[9, 8, 7, 6]);
        assert_eq!(query, [52, 0, 0, 0, 0, 0, 0, 0, 9, 8, 7, 6]);
    }

    #[test]
    fn property_query_without_parameters_is_padded_to_the_structure_size() {
        // A shorter input is rejected by the storage stack.
        let query = property_query(0, &[]);
        assert_eq!(query.len(), size_of::<STORAGE_PROPERTY_QUERY>());
        assert_eq!(query.len(), 12);
        assert!(query[8..].iter().all(|&byte| byte == 0));
    }

    #[test]
    fn nvme_health_query_requests_log_page_2_of_512_bytes() {
        let query = nvme_health_query();
        assert_eq!(query.len(), LOG_PAGE_REPLY_BYTES, "input as large as the reply");
        let word = |offset: usize| read_u32(&query, offset).unwrap();
        assert_eq!(word(0), 50, "StorageDeviceProtocolSpecificProperty");
        assert_eq!(word(4), 0, "PropertyStandardQuery");
        assert_eq!(word(8), 3, "ProtocolTypeNvme");
        assert_eq!(word(12), 2, "NVMeDataTypeLogPage");
        assert_eq!(word(16), 2, "health information log page");
        assert_eq!(word(24), 40, "ProtocolDataOffset");
        assert_eq!(word(28), 512, "ProtocolDataLength");
    }

    #[test]
    fn ata_temperature_query_requests_page_5_of_the_device_statistics_log() {
        let query = ata_temperature_query();
        let word = |offset: usize| read_u32(&query, offset).unwrap();
        assert_eq!(word(0), 50, "StorageDeviceProtocolSpecificProperty");
        assert_eq!(word(8), 2, "ProtocolTypeAta");
        assert_eq!(word(12), 2, "AtaDataTypeLogPage");
        assert_eq!(word(16), 4, "Device Statistics log address");
        assert_eq!(word(20), 5, "temperature statistics page");
        assert_eq!(word(28), 512, "ProtocolDataLength");
    }

    #[test]
    fn drives_are_opened_without_any_access_right() {
        assert_eq!(DRIVE_DESIRED_ACCESS, 0);
    }

    // ---- fakes ---------------------------------------------------------------

    /// Replays a script, then repeats its last entry forever.
    struct Script<T: Copy>(VecDeque<T>);

    impl<T: Copy> Script<T> {
        fn next(&mut self) -> T {
            let entry = if self.0.len() > 1 { self.0.pop_front() } else { self.0.front().copied() };
            entry.expect("a script has at least one entry")
        }
    }

    /// A drive that replays scripted counter and health reads and counts how
    /// many times it was asked for health.
    struct ScriptedDrive {
        counters: Script<Option<IoCounters>>,
        health: Script<Health>,
        health_reads: Arc<AtomicU32>,
    }

    impl DriveBackend for ScriptedDrive {
        fn io_counters(&mut self) -> Option<IoCounters> {
            self.counters.next()
        }

        fn health(&mut self) -> Health {
            self.health_reads.fetch_add(1, Ordering::Relaxed);
            self.health.next()
        }
    }

    /// A machine whose drives are the scripted ones, keyed by drive number.
    #[derive(Default)]
    struct FakeDisks {
        present: BTreeMap<u32, (DriveIdentity, ScriptedDrive)>,
    }

    impl FakeDisks {
        /// Adds a drive and returns the counter of its health reads.
        fn add(
            &mut self,
            index: u32,
            model: &str,
            counters: &[Option<IoCounters>],
            healths: &[Health],
        ) -> Arc<AtomicU32> {
            let health_reads = Arc::new(AtomicU32::new(0));
            let identity = DriveIdentity {
                model: model.to_owned(),
                firmware: "FW1".to_owned(),
                bus: "SATA",
                size_bytes: 1 << 40,
            };
            let drive = ScriptedDrive {
                counters: Script(counters.iter().copied().collect()),
                health: Script(healths.iter().copied().collect()),
                health_reads: health_reads.clone(),
            };
            self.present.insert(index, (identity, drive));
            health_reads
        }

        /// The provider for every drive added so far, built at `now`.
        fn provider(&mut self, now: Instant) -> Option<DriveProvider> {
            DriveProvider::new(enumerate_drives(self), now)
        }
    }

    impl DriveOpener for FakeDisks {
        fn open(&mut self, index: u32) -> Option<(DriveIdentity, Box<dyn DriveBackend>)> {
            let (identity, drive) = self.present.remove(&index)?;
            Some((identity, Box::new(drive)))
        }
    }

    fn counters(bytes_read: u64, bytes_written: u64) -> Option<IoCounters> {
        Some(IoCounters { bytes_read, bytes_written })
    }

    fn health(temperature: Option<f64>, life_remaining: Option<f64>) -> Health {
        Health { temperature, life_remaining }
    }

    /// One sample at `now`.
    fn sample(provider: &mut DriveProvider, now: Instant) -> Vec<Option<f64>> {
        let mut out = Vec::new();
        provider.sample_at(now, &mut out);
        out
    }

    // ---- enumeration -------------------------------------------------------

    #[test]
    fn enumeration_finds_drives_across_gaps_in_the_numbering() {
        let mut disks = FakeDisks::default();
        for index in [0, 2, 31, 32] {
            disks.add(index, "Disk", &[None], &[Health::default()]);
        }
        let found: Vec<_> = enumerate_drives(&mut disks).iter().map(|d| d.index).collect();
        assert_eq!(found, [0, 2, 31], "drive 32 is beyond MAX_PHYSICAL_DRIVES");
    }

    #[test]
    fn machine_without_drives_has_no_provider() {
        assert!(FakeDisks::default().provider(Instant::now()).is_none());
    }

    #[test]
    fn drive_that_answers_nothing_gets_no_sensors_and_no_provider() {
        let mut disks = FakeDisks::default();
        disks.add(0, "Mute", &[None], &[Health::default()]);
        assert!(disks.provider(Instant::now()).is_none());
    }

    // ---- provider ----------------------------------------------------------------

    #[test]
    fn sensors_are_keyed_by_physical_drive_number_and_named_by_model() {
        let mut disks = FakeDisks::default();
        disks.add(1, "XrayDisk 1TB SSD", &[counters(0, 0)], &[health(Some(35.0), None)]);
        disks.add(2, "Asgard", &[counters(0, 0)], &[health(Some(40.0), Some(98.0))]);
        let specs = disks.provider(Instant::now()).unwrap().sensors();
        let keys: Vec<_> = specs.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "disk1.read",
                "disk1.write",
                "disk1.temp",
                "disk2.read",
                "disk2.write",
                "disk2.temp",
                "disk2.life"
            ]
        );
        assert_eq!(specs[0].device, "XrayDisk 1TB SSD");
        assert_eq!(specs[0].kind, SensorKind::Throughput);
        assert_eq!(specs[2].kind, SensorKind::Temperature);
        assert_eq!(specs[6].kind, SensorKind::Level);
        assert!(specs.iter().all(|s| s.class == DeviceClass::Storage));
    }

    #[test]
    fn drive_without_counters_still_gets_its_health_sensors() {
        let mut disks = FakeDisks::default();
        disks.add(0, "NoCounters", &[None], &[health(Some(30.0), None)]);
        let mut provider = disks.provider(Instant::now()).unwrap();
        let keys: Vec<_> = provider.sensors().into_iter().map(|s| s.key).collect();
        assert_eq!(keys, ["disk0.temp"]);
        assert_eq!(sample(&mut provider, Instant::now()), [Some(30.0)]);
    }

    #[test]
    fn drives_of_the_same_model_stay_separate_devices() {
        let mut disks = FakeDisks::default();
        disks.add(0, "Same", &[counters(0, 0)], &[Health::default()]);
        disks.add(1, "Same", &[counters(0, 0)], &[Health::default()]);
        let devices: Vec<_> = disks
            .provider(Instant::now())
            .unwrap()
            .sensors()
            .into_iter()
            .map(|s| s.device)
            .collect();
        assert_eq!(devices, ["Same", "Same", "Same (2)", "Same (2)"]);
    }

    #[test]
    fn throughput_has_no_value_on_the_first_sample_then_uses_the_elapsed_time() {
        // The first frame is consumed by the probe.
        let script =
            [counters(0, 0), counters(1_000, 500), counters(5_000, 500), counters(5_000, 2_500)];
        let mut disks = FakeDisks::default();
        disks.add(0, "D", &script, &[Health::default()]);
        let start = Instant::now();
        let mut provider = disks.provider(start).unwrap();

        assert_eq!(sample(&mut provider, start), [None, None], "no baseline yet");
        assert_eq!(
            sample(&mut provider, start + ONE_SECOND),
            [Some(4_000.0), Some(0.0)],
            "bytes per second over the one second between the reads"
        );
        assert_eq!(
            sample(&mut provider, start + 3 * ONE_SECOND),
            [Some(0.0), Some(1_000.0)],
            "2 000 bytes over 2 s, from the real elapsed time and not an assumed tick"
        );
    }

    #[test]
    fn failed_counter_read_gives_none_for_that_drive_only_and_keeps_the_baseline() {
        let script = [
            counters(0, 0),
            counters(0, 0),
            None, // transient failure
            counters(3_000, 0),
        ];
        let mut disks = FakeDisks::default();
        disks.add(0, "Flaky", &script, &[Health::default()]);
        disks.add(1, "Steady", &[counters(0, 0)], &[Health::default()]);
        let start = Instant::now();
        let mut provider = disks.provider(start).unwrap();

        assert_eq!(sample(&mut provider, start), [None, None, None, None]);
        assert_eq!(
            sample(&mut provider, start + ONE_SECOND),
            [None, None, Some(0.0), Some(0.0)],
            "the failing drive has no values, the other is unaffected"
        );
        assert_eq!(
            sample(&mut provider, start + 2 * ONE_SECOND),
            [Some(1_500.0), Some(0.0), Some(0.0), Some(0.0)],
            "the baseline from before the failure is used with the full elapsed time"
        );
    }

    #[test]
    fn counter_reset_after_a_disk_restart_gives_none_not_a_negative_rate() {
        let script = [counters(0, 0), counters(9_000, 9_000), counters(100, 100)];
        let mut disks = FakeDisks::default();
        disks.add(0, "Restarted", &script, &[Health::default()]);
        let start = Instant::now();
        let mut provider = disks.provider(start).unwrap();
        sample(&mut provider, start);
        assert_eq!(sample(&mut provider, start + ONE_SECOND), [None, None]);
    }

    #[test]
    fn health_is_read_once_at_probe_time_and_then_at_most_every_four_and_a_half_seconds() {
        let healths = [
            health(Some(30.0), Some(99.0)),
            health(Some(31.0), Some(98.0)),
            health(Some(32.0), Some(97.0)),
        ];
        let mut disks = FakeDisks::default();
        let reads = disks.add(0, "Warm", &[counters(0, 0)], &healths);
        let start = Instant::now();
        let mut provider = disks.provider(start).unwrap();
        let health_reads = || reads.load(Ordering::Relaxed);
        assert_eq!(health_reads(), 1, "the probe read");

        // The probe's reading is the first value; no second device command.
        assert_eq!(sample(&mut provider, start)[2..], [Some(30.0), Some(99.0)]);
        assert_eq!(health_reads(), 1);

        assert_eq!(sample(&mut provider, start + 4 * ONE_SECOND)[2..], [Some(30.0), Some(99.0)]);
        assert_eq!(health_reads(), 1, "4 s is under the minimum interval");

        assert_eq!(
            sample(&mut provider, start + Duration::from_millis(4_500))[2..],
            [Some(31.0), Some(98.0)]
        );
        assert_eq!(health_reads(), 2);

        // The interval counts from the last read, not from the probe.
        assert_eq!(sample(&mut provider, start + 8 * ONE_SECOND)[2..], [Some(31.0), Some(98.0)]);
        assert_eq!(health_reads(), 2);
        assert_eq!(sample(&mut provider, start + 9 * ONE_SECOND)[2..], [Some(32.0), Some(97.0)]);
        assert_eq!(health_reads(), 3);
    }

    #[test]
    fn health_that_stops_being_reported_gives_none() {
        let healths = [health(Some(30.0), None), Health::default()];
        let mut disks = FakeDisks::default();
        disks.add(0, "Sleepy", &[None], &healths);
        let start = Instant::now();
        let mut provider = disks.provider(start).unwrap();
        assert_eq!(sample(&mut provider, start), [Some(30.0)]);
        assert_eq!(sample(&mut provider, start + 5 * ONE_SECOND), [None]);
    }

    #[test]
    fn a_clock_that_steps_back_does_not_trigger_a_health_read() {
        let mut disks = FakeDisks::default();
        let reads = disks.add(0, "D", &[counters(0, 0)], &[health(Some(30.0), None)]);
        let start = Instant::now() + 10 * ONE_SECOND;
        let mut provider = disks.provider(start).unwrap();
        sample(&mut provider, start - 5 * ONE_SECOND);
        assert_eq!(reads.load(Ordering::Relaxed), 1);
    }
}
