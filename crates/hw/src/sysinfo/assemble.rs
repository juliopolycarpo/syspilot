//! Turns what the sources report into the summary and the display sections.
//!
//! Everything here is pure: [`collect_from`] asks a [`SystemSource`] for the
//! raw facts and builds the answer, so the whole assembly is tested with a
//! fake source and no OS access.
//!
//! Serial numbers, the system UUID and asset tags are never put in a row,
//! even though the SMBIOS parser reads them.

use syspilot_proto::message::{InfoSection, SystemInfo, SystemSummary};

use super::format::{SizeUnits, format_megahertz, format_size, format_transfers, normalize_date};
use super::graphics::GraphicsAdapter;
use super::release::{OperatingSystem, build_text, windows_name};
use super::smbios::{
    self, BiosRecord, BoardRecord, MemoryDeviceRecord, MemorySize, ProcessorRecord, Smbios,
    SystemRecord, is_placeholder,
};
use super::topology::{CacheGroup, CacheKind, Topology};
use crate::os::storage::DriveIdentity;
use crate::os::text::number_repeated_names;

/// Where the raw facts come from. The live implementation reads the OS; tests
/// use a fake.
pub(crate) trait SystemSource {
    fn computer_name(&self) -> Option<String>;
    /// The reply of `GetSystemFirmwareTable('RSMB')`, header included.
    fn firmware_table(&self) -> Option<Vec<u8>>;
    /// The processor name from the registry, for machines whose SMBIOS table
    /// has none.
    fn processor_name(&self) -> Option<String>;
    fn topology(&self) -> Option<Topology>;
    /// Installed RAM in bytes, for machines whose SMBIOS table has no memory
    /// devices.
    fn installed_memory_bytes(&self) -> Option<u64>;
    fn graphics(&self) -> Vec<GraphicsAdapter>;
    fn drives(&self) -> Vec<DriveIdentity>;
    fn operating_system(&self) -> OperatingSystem;
}

/// Label/value rows of one section. Missing values add no row.
#[derive(Default)]
struct Rows(Vec<(String, String)>);

impl Rows {
    /// Adds a row for a firmware string, unless it is missing or a
    /// placeholder such as `Default string`.
    fn real(&mut self, label: &str, value: Option<&str>) {
        self.show(label, value.filter(|text| !is_placeholder(text)));
    }

    /// Adds a row for a value this crate computed.
    fn show(&mut self, label: &str, value: Option<impl Into<String>>) {
        if let Some(value) = value {
            self.0.push((label.to_owned(), value.into()));
        }
    }

    /// The section, or `None` when no row was added.
    fn into_section(self, title: impl Into<String>) -> Option<InfoSection> {
        (!self.0.is_empty()).then(|| InfoSection { title: title.into(), rows: self.0 })
    }
}

fn binary_size(bytes: u64) -> String {
    format_size(bytes, SizeUnits::Binary)
}

fn first_populated(smbios: &Smbios) -> Option<&ProcessorRecord> {
    smbios.processors.iter().find(|processor| processor.populated)
}

/// A firmware string, or `None` if it is missing or a placeholder.
fn real(text: Option<&str>) -> Option<&str> {
    text.filter(|text| !is_placeholder(text))
}

/// `"maker product"`, without repeating the maker when the product already
/// starts with it. Placeholders count as missing.
fn maker_and_product(maker: Option<&str>, product: Option<&str>) -> Option<String> {
    let product = real(product)?;
    Some(match real(maker) {
        Some(maker) if !product.to_lowercase().starts_with(&maker.to_lowercase()) => {
            format!("{maker} {product}")
        }
        _ => product.to_owned(),
    })
}

/// The machine model: the system product, or the board's when the system
/// strings are placeholders (common on retail boards).
fn model_name(system: Option<&SystemRecord>, board: Option<&BoardRecord>) -> String {
    let from_system = system.and_then(|record| {
        maker_and_product(record.manufacturer.as_deref(), record.product.as_deref())
    });
    let from_board = board.and_then(|record| {
        maker_and_product(record.manufacturer.as_deref(), record.product.as_deref())
    });
    from_system.or(from_board).unwrap_or_default()
}

fn joined(parts: impl IntoIterator<Item = Option<String>>, separator: &str) -> String {
    parts.into_iter().flatten().collect::<Vec<_>>().join(separator)
}

/// `"vendor version (date)"`.
fn bios_summary(bios: Option<&BiosRecord>) -> String {
    let Some(bios) = bios else { return String::new() };
    let text = |field: &Option<String>| real(field.as_deref()).map(str::to_owned);
    let date = text(&bios.release_date).map(|date| format!("({})", normalize_date(&date)));
    joined([text(&bios.vendor), text(&bios.version), date], " ")
}

/// `"Windows 11 Pro 25H2 (build 26300.10075)"`.
fn os_summary(os: &OperatingSystem) -> String {
    let name = os.product_name.as_deref().map(|name| windows_name(name, os.build));
    let build = os.build.map(|build| format!("(build {})", build_text(build, os.revision)));
    joined([name, os.display_version.clone(), build], " ")
}

fn system_section(computer: Option<&str>, system: Option<&SystemRecord>) -> Option<InfoSection> {
    let mut rows = Rows::default();
    rows.show("Computer name", computer);
    if let Some(system) = system {
        rows.real("Manufacturer", system.manufacturer.as_deref());
        rows.real("Product", system.product.as_deref());
        rows.real("Version", system.version.as_deref());
        rows.real("SKU", system.sku.as_deref());
        rows.real("Family", system.family.as_deref());
    }
    rows.into_section("System")
}

fn motherboard_section(board: Option<&BoardRecord>) -> Option<InfoSection> {
    let board = board?;
    let mut rows = Rows::default();
    rows.real("Manufacturer", board.manufacturer.as_deref());
    rows.real("Product", board.product.as_deref());
    rows.real("Version", board.version.as_deref());
    rows.into_section("Motherboard")
}

fn bios_section(
    bios: Option<&BiosRecord>,
    smbios_version: Option<(u8, u8)>,
) -> Option<InfoSection> {
    let mut rows = Rows::default();
    if let Some(bios) = bios {
        rows.real("Vendor", bios.vendor.as_deref());
        rows.real("Version", bios.version.as_deref());
        rows.show("Release date", bios.release_date.as_deref().map(normalize_date));
    }
    rows.show("SMBIOS version", smbios_version.map(|(major, minor)| format!("{major}.{minor}")));
    rows.into_section("BIOS")
}

fn cache_label(group: &CacheGroup) -> String {
    let level = group.level;
    match group.kind {
        CacheKind::Unified => format!("L{level} cache"),
        CacheKind::Instruction => format!("L{level} instruction cache"),
        CacheKind::Data => format!("L{level} data cache"),
        CacheKind::Trace => format!("L{level} trace cache"),
    }
}

/// `"32 KiB"` for one cache, `"14 x 32 KiB"` for several of the same size.
fn cache_value(group: &CacheGroup) -> String {
    let size = binary_size(u64::from(group.size_bytes));
    if group.count > 1 { format!("{} x {size}", group.count) } else { size }
}

/// A count from the OS topology, falling back to the SMBIOS total over the
/// populated sockets. Zero means unknown in both.
fn count_or_sum(
    from_topology: Option<u32>,
    smbios: &Smbios,
    field: impl Fn(&ProcessorRecord) -> Option<u32>,
) -> Option<u32> {
    from_topology.filter(|&count| count > 0).or_else(|| {
        let total: u32 = smbios.processors.iter().filter(|p| p.populated).filter_map(field).sum();
        (total > 0).then_some(total)
    })
}

fn processor_name(smbios: &Smbios, registry_name: Option<&str>) -> Option<String> {
    first_populated(smbios)
        .and_then(|processor| processor.version.clone())
        .filter(|name| !is_placeholder(name))
        .or_else(|| registry_name.filter(|name| !is_placeholder(name)).map(str::to_owned))
}

fn processor_section(
    name: Option<&str>,
    smbios: &Smbios,
    topology: Option<&Topology>,
) -> Option<InfoSection> {
    let record = first_populated(smbios);
    let mut rows = Rows::default();
    rows.show("Name", name);
    let sockets = match smbios.processors.iter().filter(|p| p.populated).count() {
        0 => topology.map(|t| t.packages as usize).filter(|&count| count > 0),
        count => Some(count),
    };
    rows.show("Sockets", sockets.map(|count| count.to_string()));
    rows.show(
        "Cores",
        count_or_sum(topology.map(|t| t.cores), smbios, |p| p.cores_enabled.or(p.core_count))
            .map(|count| count.to_string()),
    );
    rows.show(
        "Logical processors",
        count_or_sum(topology.map(|t| t.logical_processors), smbios, |p| p.thread_count)
            .map(|count| count.to_string()),
    );
    rows.show("Speed", record.and_then(|p| p.current_speed_mhz).map(format_megahertz));
    rows.show("Max speed", record.and_then(|p| p.max_speed_mhz).map(format_megahertz));
    for group in topology.map(|t| t.caches.as_slice()).unwrap_or_default() {
        rows.show(&cache_label(group), Some(cache_value(group)));
    }
    rows.into_section("Processor")
}

/// One section per installed module, numbered from 1. Empty slots are left
/// out.
fn memory_sections(devices: &[MemoryDeviceRecord]) -> Vec<InfoSection> {
    devices
        .iter()
        .filter(|device| device.size != MemorySize::NotInstalled)
        .enumerate()
        .filter_map(|(index, device)| {
            let mut rows = Rows::default();
            rows.real("Slot", device.locator.as_deref());
            rows.real("Bank", device.bank.as_deref());
            if let MemorySize::Bytes(bytes) = device.size {
                rows.show("Size", Some(binary_size(bytes)));
            }
            rows.show("Type", device.memory_type);
            rows.show("Form factor", device.form_factor);
            // The speed it runs at, with the rated one only when they differ.
            let running = device.configured_speed_mts.or(device.speed_mts);
            rows.show("Speed", running.map(format_transfers));
            if device.speed_mts != running {
                rows.show("Rated speed", device.speed_mts.map(format_transfers));
            }
            rows.real("Manufacturer", device.manufacturer.as_deref());
            rows.real("Part number", device.part_number.as_deref());
            rows.show("Ranks", device.rank.map(|rank| rank.to_string()));
            rows.into_section(format!("Memory module {}", index + 1))
        })
        .collect()
}

/// A section per adapter: `Graphics` for one, `Graphics 1`, `Graphics 2`...
/// for several.
fn graphics_sections(adapters: &[GraphicsAdapter]) -> Vec<InfoSection> {
    adapters
        .iter()
        .enumerate()
        .filter_map(|(index, adapter)| {
            let mut rows = Rows::default();
            rows.real("Name", Some(&adapter.name));
            rows.show("Dedicated memory", adapter.dedicated_bytes.map(binary_size));
            rows.show("Shared memory", adapter.shared_bytes.map(binary_size));
            rows.real("Driver version", adapter.driver_version.as_deref());
            rows.show("Driver date", adapter.driver_date.as_deref().map(normalize_date));
            rows.real("Driver provider", adapter.driver_provider.as_deref());
            let title = if adapters.len() == 1 {
                "Graphics".to_owned()
            } else {
                format!("Graphics {}", index + 1)
            };
            rows.into_section(title)
        })
        .collect()
}

/// One row per drive: the model, then capacity, bus and firmware.
fn storage_section(drives: &[DriveIdentity]) -> Option<InfoSection> {
    let mut models: Vec<String> = drives.iter().map(|drive| drive.model.clone()).collect();
    number_repeated_names(&mut models);
    let mut rows = Rows::default();
    for (model, drive) in models.iter().zip(drives) {
        let size =
            (drive.size_bytes > 0).then(|| format_size(drive.size_bytes, SizeUnits::Decimal));
        let firmware = Some(drive.firmware.as_str())
            .filter(|firmware| !firmware.is_empty())
            .map(|firmware| format!("firmware {firmware}"));
        rows.show(model, Some(joined([size, Some(drive.bus.to_owned()), firmware], ", ")));
    }
    rows.into_section("Storage")
}

fn os_section(os: &OperatingSystem) -> Option<InfoSection> {
    let mut rows = Rows::default();
    rows.show("Name", os.product_name.as_deref().map(|name| windows_name(name, os.build)));
    rows.show("Version", os.display_version.as_deref());
    rows.show("Build", os.build.map(|build| build_text(build, os.revision)));
    rows.show("Architecture", os.architecture);
    rows.into_section("Operating system")
}

/// Installed memory: the SMBIOS modules added up, or the OS's figure when the
/// table lists none (or none with a known size).
fn installed_memory(smbios: &Smbios, fallback: Option<u64>) -> u64 {
    let from_modules: u64 = smbios
        .memory_devices
        .iter()
        .filter_map(|device| match device.size {
            MemorySize::Bytes(bytes) => Some(bytes),
            MemorySize::NotInstalled | MemorySize::Unknown => None,
        })
        .sum();
    if from_modules > 0 { from_modules } else { fallback.unwrap_or(0) }
}

/// Reads every fact from `source` and builds the answer. A source that
/// cannot supply something leaves the matching rows out; nothing fails.
pub(crate) fn collect_from(source: &dyn SystemSource) -> SystemInfo {
    let firmware = source.firmware_table();
    let table = firmware.as_deref().and_then(|buffer| smbios::split_firmware_table(buffer).ok());
    // A damaged table still yields the structures read before the damage.
    let tables = table.map(|table| smbios::parse(table.data).tables).unwrap_or_default();
    let smbios_version = table.map(|table| (table.major, table.minor));

    let computer = source.computer_name();
    let topology = source.topology();
    let graphics = source.graphics();
    let drives = source.drives();
    let os = source.operating_system();
    let cpu_name = processor_name(&tables, source.processor_name().as_deref());

    let summary = SystemSummary {
        computer: computer.clone().unwrap_or_default(),
        model: model_name(tables.system.as_ref(), tables.board.as_ref()),
        cpu: cpu_name.clone().unwrap_or_default(),
        gpus: graphics.iter().map(|adapter| adapter.name.clone()).collect(),
        memory_bytes: installed_memory(&tables, source.installed_memory_bytes()),
        os: os_summary(&os),
        bios: bios_summary(tables.bios.as_ref()),
    };

    let mut sections = Vec::new();
    sections.extend(system_section(computer.as_deref(), tables.system.as_ref()));
    sections.extend(motherboard_section(tables.board.as_ref()));
    sections.extend(bios_section(tables.bios.as_ref(), smbios_version));
    sections.extend(processor_section(cpu_name.as_deref(), &tables, topology.as_ref()));
    sections.extend(memory_sections(&tables.memory_devices));
    sections.extend(graphics_sections(&graphics));
    sections.extend(storage_section(&drives));
    sections.extend(os_section(&os));
    SystemInfo { summary, sections }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &[u8] = include_bytes!("fixtures/smbios-sanitized.bin");
    const GIB: u64 = 1 << 30;

    /// A `GetSystemFirmwareTable` reply for a table.
    fn firmware_reply(major: u8, minor: u8, table: &[u8]) -> Vec<u8> {
        let mut reply = vec![0, major, minor, 0];
        reply.extend((table.len() as u32).to_le_bytes());
        reply.extend(table);
        reply
    }

    /// A source that answers from fields, standing in for the OS.
    #[derive(Default)]
    struct FakeSystem {
        computer_name: Option<String>,
        firmware_table: Option<Vec<u8>>,
        processor_name: Option<String>,
        topology: Option<Topology>,
        installed_memory_bytes: Option<u64>,
        graphics: Vec<GraphicsAdapter>,
        drives: Vec<DriveIdentity>,
        operating_system: OperatingSystem,
    }

    impl SystemSource for FakeSystem {
        fn computer_name(&self) -> Option<String> {
            self.computer_name.clone()
        }
        fn firmware_table(&self) -> Option<Vec<u8>> {
            self.firmware_table.clone()
        }
        fn processor_name(&self) -> Option<String> {
            self.processor_name.clone()
        }
        fn topology(&self) -> Option<Topology> {
            self.topology.clone()
        }
        fn installed_memory_bytes(&self) -> Option<u64> {
            self.installed_memory_bytes
        }
        fn graphics(&self) -> Vec<GraphicsAdapter> {
            self.graphics.clone()
        }
        fn drives(&self) -> Vec<DriveIdentity> {
            self.drives.clone()
        }
        fn operating_system(&self) -> OperatingSystem {
            self.operating_system.clone()
        }
    }

    fn drive(model: &str, bus: &'static str, size_bytes: u64, firmware: &str) -> DriveIdentity {
        DriveIdentity { model: model.into(), firmware: firmware.into(), bus, size_bytes }
    }

    fn adapter(name: &str) -> GraphicsAdapter {
        GraphicsAdapter { name: name.into(), ..GraphicsAdapter::default() }
    }

    fn release() -> OperatingSystem {
        OperatingSystem {
            product_name: Some("Windows 10 Pro".into()),
            display_version: Some("25H2".into()),
            build: Some(26200),
            revision: Some(1234),
            architecture: Some("x64"),
        }
    }

    fn topology() -> Topology {
        Topology {
            packages: 1,
            cores: 14,
            logical_processors: 28,
            caches: vec![
                CacheGroup {
                    level: 1,
                    kind: CacheKind::Instruction,
                    size_bytes: 32 << 10,
                    count: 14,
                },
                CacheGroup { level: 1, kind: CacheKind::Data, size_bytes: 32 << 10, count: 14 },
                CacheGroup { level: 2, kind: CacheKind::Unified, size_bytes: 256 << 10, count: 14 },
                CacheGroup { level: 3, kind: CacheKind::Unified, size_bytes: 35 << 20, count: 1 },
            ],
        }
    }

    /// The machine the fixture table came from, as far as the table goes.
    fn fixture_machine() -> FakeSystem {
        FakeSystem {
            computer_name: Some("TEST-PC".into()),
            firmware_table: Some(firmware_reply(3, 0, FIXTURE)),
            topology: Some(topology()),
            graphics: vec![GraphicsAdapter {
                name: "Radeon RX 550".into(),
                dedicated_bytes: Some(4 * GIB),
                shared_bytes: Some(32 * GIB),
                driver_version: Some("31.0.21925.1001".into()),
                driver_date: Some("5-20-2026".into()),
                driver_provider: Some("Advanced Micro Devices, Inc.".into()),
            }],
            drives: vec![
                drive("CT500BX500SSD1", "SATA", 500_107_862_016, "M6CR023"),
                drive("Asgard AN4+ 1TNVMe-M.2/80", "NVMe", 1_000_204_886_016, ""),
            ],
            operating_system: release(),
            ..FakeSystem::default()
        }
    }

    fn section<'a>(info: &'a SystemInfo, title: &str) -> &'a InfoSection {
        info.sections.iter().find(|section| section.title == title).unwrap_or_else(|| {
            let titles: Vec<_> = info.sections.iter().map(|s| s.title.as_str()).collect();
            panic!("section {title:?}: expected present | received titles: {titles:?}")
        })
    }

    fn row<'a>(section: &'a InfoSection, label: &str) -> Option<&'a str> {
        section.rows.iter().find(|(name, _)| name == label).map(|(_, value)| value.as_str())
    }

    fn titles(info: &SystemInfo) -> Vec<&str> {
        info.sections.iter().map(|section| section.title.as_str()).collect()
    }

    // ---- the whole answer for the fixture machine -----------------------------------

    #[test]
    fn sections_come_in_display_order() {
        let info = collect_from(&fixture_machine());
        assert_eq!(
            titles(&info),
            [
                "System",
                "Motherboard",
                "BIOS",
                "Processor",
                "Memory module 1",
                "Memory module 2",
                "Memory module 3",
                "Memory module 4",
                "Graphics",
                "Storage",
                "Operating system",
            ]
        );
    }

    #[test]
    fn summary_of_the_fixture_machine() {
        let summary = collect_from(&fixture_machine()).summary;
        assert_eq!(summary.computer, "TEST-PC");
        assert_eq!(
            summary.model, "MACHINIST E5-MR9A PRO",
            "system strings are placeholders: the board names the model"
        );
        assert_eq!(summary.cpu, "Intel(R) Xeon(R) CPU E5-2680 v4 @ 2.40GHz");
        assert_eq!(summary.gpus, ["Radeon RX 550"]);
        assert_eq!(summary.memory_bytes, 64 * GIB);
        assert_eq!(summary.os, "Windows 11 Pro 25H2 (build 26200.1234)");
        assert_eq!(summary.bios, "American Megatrends Inc. 5.11 (2023-03-22)");
    }

    #[test]
    fn system_section_drops_placeholder_strings_and_keeps_the_computer_name() {
        let info = collect_from(&fixture_machine());
        assert_eq!(
            section(&info, "System").rows,
            [("Computer name".to_owned(), "TEST-PC".to_owned())]
        );
    }

    #[test]
    fn motherboard_and_bios_sections_of_the_fixture() {
        let info = collect_from(&fixture_machine());
        let board = section(&info, "Motherboard");
        assert_eq!(row(board, "Manufacturer"), Some("MACHINIST"));
        assert_eq!(row(board, "Product"), Some("E5-MR9A PRO"));
        assert_eq!(row(board, "Version"), Some("V1.2"));
        let bios = section(&info, "BIOS");
        assert_eq!(row(bios, "Vendor"), Some("American Megatrends Inc."));
        assert_eq!(row(bios, "Version"), Some("5.11"));
        assert_eq!(row(bios, "Release date"), Some("2023-03-22"));
        assert_eq!(row(bios, "SMBIOS version"), Some("3.0"));
    }

    #[test]
    fn processor_section_combines_smbios_and_the_topology() {
        let info = collect_from(&fixture_machine());
        let cpu = section(&info, "Processor");
        assert_eq!(row(cpu, "Name"), Some("Intel(R) Xeon(R) CPU E5-2680 v4 @ 2.40GHz"));
        assert_eq!(row(cpu, "Sockets"), Some("1"));
        assert_eq!(row(cpu, "Cores"), Some("14"));
        assert_eq!(row(cpu, "Logical processors"), Some("28"));
        assert_eq!(row(cpu, "Speed"), Some("2.4 GHz"));
        assert_eq!(row(cpu, "Max speed"), Some("4 GHz"));
        assert_eq!(row(cpu, "L1 data cache"), Some("14 x 32 KiB"));
        assert_eq!(row(cpu, "L1 instruction cache"), Some("14 x 32 KiB"));
        assert_eq!(row(cpu, "L2 cache"), Some("14 x 256 KiB"));
        assert_eq!(row(cpu, "L3 cache"), Some("35 MiB"));
    }

    #[test]
    fn memory_modules_are_numbered_from_one_with_their_details() {
        let info = collect_from(&fixture_machine());
        let module = section(&info, "Memory module 3");
        assert_eq!(row(module, "Slot"), Some("DIMM_C1"));
        assert_eq!(row(module, "Bank"), Some("NODE 2"));
        assert_eq!(row(module, "Size"), Some("16 GiB"));
        assert_eq!(row(module, "Type"), Some("DDR4"));
        assert_eq!(row(module, "Form factor"), Some("DIMM"));
        assert_eq!(row(module, "Speed"), Some("2400 MT/s"));
        assert_eq!(row(module, "Rated speed"), None, "configured speed equals the rated one");
        assert_eq!(row(module, "Manufacturer"), Some("Micron"));
        assert_eq!(row(module, "Part number"), Some("36ASF2G72PZ-2G1A2"));
        assert_eq!(row(module, "Ranks"), Some("2"));
    }

    #[test]
    fn graphics_storage_and_operating_system_sections() {
        let info = collect_from(&fixture_machine());
        let graphics = section(&info, "Graphics");
        assert_eq!(row(graphics, "Name"), Some("Radeon RX 550"));
        assert_eq!(row(graphics, "Dedicated memory"), Some("4 GiB"));
        assert_eq!(row(graphics, "Shared memory"), Some("32 GiB"));
        assert_eq!(row(graphics, "Driver version"), Some("31.0.21925.1001"));
        assert_eq!(row(graphics, "Driver date"), Some("2026-05-20"));
        assert_eq!(row(graphics, "Driver provider"), Some("Advanced Micro Devices, Inc."));
        let storage = section(&info, "Storage");
        assert_eq!(row(storage, "CT500BX500SSD1"), Some("500 GB, SATA, firmware M6CR023"));
        assert_eq!(row(storage, "Asgard AN4+ 1TNVMe-M.2/80"), Some("1 TB, NVMe"));
        let os = section(&info, "Operating system");
        assert_eq!(row(os, "Name"), Some("Windows 11 Pro"));
        assert_eq!(row(os, "Version"), Some("25H2"));
        assert_eq!(row(os, "Build"), Some("26200.1234"));
        assert_eq!(row(os, "Architecture"), Some("x64"));
    }

    // ---- identifiers are never shown ---------------------------------------------------

    #[test]
    fn no_serial_number_asset_tag_or_uuid_appears_anywhere_in_the_answer() {
        // The fixture's identifiers are `FAKE-...` strings and the UUID
        // 11111111-2222-3333-4444-555555555555; the parser reads all of them
        // (see its tests), so any of them showing up here is a leak.
        let info = collect_from(&fixture_machine());
        let everything = format!("{info:?}");
        for forbidden in ["FAKE-", "11111111"] {
            assert!(
                !everything.contains(forbidden),
                "answer: expected no identifier | received {forbidden:?} in {everything}"
            );
        }
    }

    // ---- degraded sources -----------------------------------------------------------------

    #[test]
    fn without_a_firmware_table_the_other_sources_still_fill_what_they_can() {
        let source = FakeSystem {
            computer_name: Some("TEST-PC".into()),
            processor_name: Some("Registry CPU @ 3.0GHz".into()),
            installed_memory_bytes: Some(16 * GIB),
            topology: Some(topology()),
            operating_system: release(),
            ..FakeSystem::default()
        };
        let info = collect_from(&source);
        assert_eq!(titles(&info), ["System", "Processor", "Operating system"]);
        assert_eq!(info.summary.cpu, "Registry CPU @ 3.0GHz");
        assert_eq!(info.summary.memory_bytes, 16 * GIB);
        assert_eq!((info.summary.model.as_str(), info.summary.bios.as_str()), ("", ""));
    }

    #[test]
    fn nothing_known_gives_an_empty_answer() {
        let info = collect_from(&FakeSystem::default());
        assert_eq!(info, SystemInfo::default());
    }

    #[test]
    fn garbage_firmware_buffer_is_survived_and_contributes_nothing() {
        for buffer in [vec![1, 2, 3], vec![0xFF; 64], firmware_reply(3, 0, &[0xEE; 40])] {
            let source = FakeSystem { firmware_table: Some(buffer), ..FakeSystem::default() };
            let info = collect_from(&source);
            assert_eq!(info.summary.model, "");
        }
    }

    #[test]
    fn firmware_table_cut_in_the_middle_keeps_the_structures_before_the_cut() {
        // The last 112 bytes of the fixture are the processor structure and
        // the end marker; cutting 120 bytes off lands in the cache structure
        // before them. BIOS and board survive, the processor does not.
        let cut = FIXTURE.len() - 120;
        let source = FakeSystem {
            firmware_table: Some(firmware_reply(3, 0, &FIXTURE[..cut])),
            ..FakeSystem::default()
        };
        let info = collect_from(&source);
        assert_eq!(section(&info, "BIOS").rows.len(), 4);
        assert!(info.sections.iter().all(|section| section.title != "Processor"));
    }

    // ---- model, summaries -------------------------------------------------------------------

    fn system_record(maker: &str, product: &str) -> SystemRecord {
        SystemRecord {
            manufacturer: Some(maker.into()),
            product: Some(product.into()),
            ..SystemRecord::default()
        }
    }

    #[test]
    fn model_prefers_the_system_and_prepends_the_maker_once() {
        let board = BoardRecord { product: Some("Z790 Board".into()), ..BoardRecord::default() };
        assert_eq!(model_name(Some(&system_record("LENOVO", "20XW")), Some(&board)), "LENOVO 20XW");
        assert_eq!(
            model_name(Some(&system_record("Dell Inc.", "Dell Inc. XPS")), None),
            "Dell Inc. XPS"
        );
        assert_eq!(
            model_name(
                Some(&system_record("To be filled by O.E.M.", "Default string")),
                Some(&board)
            ),
            "Z790 Board"
        );
        assert_eq!(model_name(None, None), "");
    }

    #[test]
    fn bios_summary_skips_missing_and_placeholder_parts() {
        let bios = BiosRecord {
            vendor: Some("Vendor".into()),
            version: Some("Default string".into()),
            release_date: Some("01/02/2024".into()),
        };
        assert_eq!(bios_summary(Some(&bios)), "Vendor (2024-01-02)");
        assert_eq!(bios_summary(None), "");
    }

    #[test]
    fn os_summary_with_only_some_facts() {
        let only_build = OperatingSystem { build: Some(19045), ..OperatingSystem::default() };
        assert_eq!(os_summary(&only_build), "(build 19045)");
        assert_eq!(os_summary(&OperatingSystem::default()), "");
    }

    // ---- memory -------------------------------------------------------------------------------

    fn module(slot: &str, size: MemorySize) -> MemoryDeviceRecord {
        MemoryDeviceRecord { locator: Some(slot.into()), size, ..MemoryDeviceRecord::default() }
    }

    #[test]
    fn empty_slots_get_no_section_and_numbering_skips_them() {
        let devices = [
            module("A1", MemorySize::Bytes(8 * GIB)),
            module("A2", MemorySize::NotInstalled),
            module("B1", MemorySize::Bytes(8 * GIB)),
        ];
        let sections = memory_sections(&devices);
        let titles: Vec<_> = sections.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["Memory module 1", "Memory module 2"]);
        assert_eq!(row(&sections[1], "Slot"), Some("B1"));
    }

    #[test]
    fn module_of_unknown_size_is_listed_without_a_size_row() {
        let sections = memory_sections(&[module("A1", MemorySize::Unknown)]);
        assert_eq!(row(&sections[0], "Size"), None);
        assert_eq!(row(&sections[0], "Slot"), Some("A1"));
    }

    #[test]
    fn module_running_below_its_rating_shows_both_speeds() {
        let device = MemoryDeviceRecord {
            size: MemorySize::Bytes(GIB),
            speed_mts: Some(3200),
            configured_speed_mts: Some(2933),
            ..MemoryDeviceRecord::default()
        };
        let sections = memory_sections(&[device]);
        assert_eq!(row(&sections[0], "Speed"), Some("2933 MT/s"));
        assert_eq!(row(&sections[0], "Rated speed"), Some("3200 MT/s"));
    }

    #[test]
    fn module_with_only_a_rated_speed_shows_it_as_the_speed() {
        let device = MemoryDeviceRecord {
            size: MemorySize::Bytes(GIB),
            speed_mts: Some(2133),
            ..MemoryDeviceRecord::default()
        };
        let sections = memory_sections(&[device]);
        assert_eq!(row(&sections[0], "Speed"), Some("2133 MT/s"));
        assert_eq!(row(&sections[0], "Rated speed"), None);
    }

    #[test]
    fn installed_memory_adds_known_modules_and_falls_back_when_there_are_none() {
        let mut smbios = Smbios::default();
        assert_eq!(installed_memory(&smbios, Some(8 * GIB)), 8 * GIB);
        assert_eq!(installed_memory(&smbios, None), 0);
        smbios.memory_devices = vec![
            module("A1", MemorySize::Bytes(4 * GIB)),
            module("A2", MemorySize::NotInstalled),
            module("A3", MemorySize::Unknown),
            module("A4", MemorySize::Bytes(4 * GIB)),
        ];
        assert_eq!(installed_memory(&smbios, Some(GIB)), 8 * GIB);
    }

    // ---- processor ----------------------------------------------------------------------------

    fn socket(populated: bool, cores: u32, threads: u32) -> ProcessorRecord {
        ProcessorRecord {
            populated,
            cores_enabled: Some(cores),
            thread_count: Some(threads),
            version: Some("CPU".into()),
            ..ProcessorRecord::default()
        }
    }

    #[test]
    fn without_a_topology_core_and_thread_counts_add_up_the_populated_sockets() {
        let smbios = Smbios {
            processors: vec![socket(true, 8, 16), socket(true, 8, 16), socket(false, 8, 16)],
            ..Smbios::default()
        };
        let section = processor_section(Some("CPU"), &smbios, None).unwrap();
        assert_eq!(row(&section, "Sockets"), Some("2"));
        assert_eq!(row(&section, "Cores"), Some("16"));
        assert_eq!(row(&section, "Logical processors"), Some("32"));
    }

    #[test]
    fn topology_counts_win_over_smbios_ones_and_zero_means_unknown() {
        let smbios = Smbios { processors: vec![socket(true, 8, 16)], ..Smbios::default() };
        let measured = Topology { packages: 1, cores: 6, logical_processors: 12, caches: vec![] };
        let section = processor_section(None, &smbios, Some(&measured)).unwrap();
        assert_eq!(row(&section, "Cores"), Some("6"));
        let unknown = Topology { cores: 0, logical_processors: 0, ..measured };
        let section = processor_section(None, &smbios, Some(&unknown)).unwrap();
        assert_eq!(row(&section, "Cores"), Some("8"), "a zero count falls back to SMBIOS");
    }

    #[test]
    fn processor_name_prefers_smbios_over_the_registry_and_skips_placeholders() {
        let smbios = Smbios { processors: vec![socket(true, 4, 8)], ..Smbios::default() };
        assert_eq!(processor_name(&smbios, Some("Registry")).as_deref(), Some("CPU"));
        let mut placeholder = socket(true, 4, 8);
        placeholder.version = Some("To Be Filled By O.E.M.".into());
        let smbios = Smbios { processors: vec![placeholder], ..Smbios::default() };
        assert_eq!(processor_name(&smbios, Some("Registry")).as_deref(), Some("Registry"));
        assert_eq!(processor_name(&Smbios::default(), None), None);
    }

    #[test]
    fn cache_rows_say_what_each_cache_holds() {
        let group = |level, kind, count| CacheGroup { level, kind, size_bytes: 1 << 20, count };
        assert_eq!(cache_label(&group(2, CacheKind::Unified, 1)), "L2 cache");
        assert_eq!(cache_label(&group(1, CacheKind::Data, 1)), "L1 data cache");
        assert_eq!(cache_label(&group(1, CacheKind::Instruction, 1)), "L1 instruction cache");
        assert_eq!(cache_label(&group(0, CacheKind::Trace, 1)), "L0 trace cache");
        assert_eq!(cache_value(&group(2, CacheKind::Unified, 1)), "1 MiB");
        assert_eq!(cache_value(&group(2, CacheKind::Unified, 4)), "4 x 1 MiB");
    }

    // ---- graphics and storage -------------------------------------------------------------------

    #[test]
    fn several_adapters_get_numbered_sections_and_one_stays_plain() {
        let one = graphics_sections(&[adapter("Card A")]);
        assert_eq!(one[0].title, "Graphics");
        let two = graphics_sections(&[adapter("Card A"), adapter("Card B")]);
        let titles: Vec<_> = two.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["Graphics 1", "Graphics 2"]);
        assert_eq!(row(&two[1], "Name"), Some("Card B"));
    }

    #[test]
    fn identical_drives_are_numbered_so_their_rows_stay_distinct() {
        let drives = [
            drive("Disk X", "SATA", 1_000_000_000_000, ""),
            drive("Disk X", "SATA", 1_000_000_000_000, ""),
        ];
        let section = storage_section(&drives).unwrap();
        let labels: Vec<_> = section.rows.iter().map(|(label, _)| label.as_str()).collect();
        assert_eq!(labels, ["Disk X", "Disk X (2)"]);
    }

    #[test]
    fn drive_with_unknown_capacity_shows_only_what_it_reports() {
        let section = storage_section(&[drive("Odd", "USB", 0, "")]).unwrap();
        assert_eq!(row(&section, "Odd"), Some("USB"));
    }

    #[test]
    fn machine_without_drives_or_adapters_has_neither_section() {
        assert!(storage_section(&[]).is_none());
        assert!(graphics_sections(&[]).is_empty());
    }
}
