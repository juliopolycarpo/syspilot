//! The [`SystemSource`] that reads this machine.
//!
//! Every call is a read-only query: registry values, kernel adapter
//! information, drive identity (a drive handle opened with no access rights)
//! and the firmware table. Nothing is cached; [`collect`](super::collect) is
//! meant to be called on request, not sampled.

use windows_sys::Win32::System::SystemInformation::{
    ComputerNameDnsHostname, GetComputerNameExW, GetPhysicallyInstalledSystemMemory,
    GetSystemFirmwareTable,
};

use super::assemble::SystemSource;
use super::graphics::{GraphicsAdapter, graphics_adapters};
use super::release::{OperatingSystem, read_operating_system};
use super::topology::{Topology, read_topology};
use crate::os::cpu::PROCESSOR_KEY;
use crate::os::registry;
use crate::os::storage::{self, DriveIdentity};
use crate::os::text::clean_utf16;

/// `'RSMB'`: the raw SMBIOS firmware table provider.
const RAW_SMBIOS_PROVIDER: u32 = u32::from_be_bytes(*b"RSMB");
/// Longest DNS host name, plus the terminator.
const HOST_NAME_UNITS: usize = 64;

/// This machine, read live.
pub(crate) struct LiveSystem;

impl SystemSource for LiveSystem {
    fn computer_name(&self) -> Option<String> {
        let mut units = [0u16; HOST_NAME_UNITS];
        let mut length = HOST_NAME_UNITS as u32;
        // SAFETY: `units` is valid for writes of `length` UTF-16 units and
        // `length` is a valid in/out pointer.
        let ok =
            unsafe { GetComputerNameExW(ComputerNameDnsHostname, units.as_mut_ptr(), &mut length) };
        if ok == 0 {
            return None;
        }
        // On success `length` is the number of units written, without the NUL.
        let name = clean_utf16(units.get(..length as usize)?);
        (!name.is_empty()).then_some(name)
    }

    fn firmware_table(&self) -> Option<Vec<u8>> {
        // SAFETY: a null buffer with size zero asks for the required size.
        let needed =
            unsafe { GetSystemFirmwareTable(RAW_SMBIOS_PROVIDER, 0, std::ptr::null_mut(), 0) };
        if needed == 0 {
            return None;
        }
        let mut buffer = vec![0u8; needed as usize];
        // SAFETY: `buffer` is valid for writes of `needed` bytes.
        let written =
            unsafe { GetSystemFirmwareTable(RAW_SMBIOS_PROVIDER, 0, buffer.as_mut_ptr(), needed) };
        // Zero is failure; more than `needed` is the API asking for a bigger
        // buffer because the table changed in between.
        if written == 0 || written > needed {
            return None;
        }
        buffer.truncate(written as usize);
        Some(buffer)
    }

    fn processor_name(&self) -> Option<String> {
        registry::read_string(PROCESSOR_KEY, "ProcessorNameString")
    }

    fn topology(&self) -> Option<Topology> {
        read_topology()
    }

    fn installed_memory_bytes(&self) -> Option<u64> {
        let mut kibibytes = 0u64;
        // SAFETY: `kibibytes` is a valid out-pointer.
        let ok = unsafe { GetPhysicallyInstalledSystemMemory(&mut kibibytes) };
        (ok != 0 && kibibytes > 0).then(|| kibibytes.saturating_mul(1024))
    }

    fn graphics(&self) -> Vec<GraphicsAdapter> {
        graphics_adapters()
    }

    fn drives(&self) -> Vec<DriveIdentity> {
        storage::drives().into_iter().map(|drive| drive.identity).collect()
    }

    fn operating_system(&self) -> OperatingSystem {
        read_operating_system()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_signature_spells_rsmb() {
        assert_eq!(RAW_SMBIOS_PROVIDER, 0x5253_4D42);
    }

    #[test]
    fn this_machine_has_a_computer_name_that_matches_the_environment() {
        let name = LiveSystem.computer_name().expect("GetComputerNameExW failed");
        let environment = std::env::var("COMPUTERNAME").unwrap_or_default();
        assert!(
            name.eq_ignore_ascii_case(&environment),
            "computer name: expected {environment:?} (COMPUTERNAME, ignoring case) | received: {name:?}"
        );
    }

    #[test]
    fn this_machine_has_a_firmware_table_that_parses_to_a_header_and_structures() {
        let Some(buffer) = LiveSystem.firmware_table() else {
            // Virtual machines without SMBIOS and some CI images have none.
            return;
        };
        let table = crate::sysinfo::smbios::split_firmware_table(&buffer)
            .unwrap_or_else(|error| panic!("firmware table header: {error}"));
        assert!(
            table.major >= 2,
            "SMBIOS major version: expected >= 2 | received: {}",
            table.major
        );
        let parsed = crate::sysinfo::smbios::parse(table.data);
        assert_eq!(parsed.error, None, "this machine's own table: expected a clean parse");
    }

    /// Every serial number, asset tag and UUID text this machine's table
    /// holds, as strings a leak would contain. Lengths only are ever printed.
    fn live_identifiers() -> Vec<(&'static str, String)> {
        use crate::sysinfo::smbios::{is_placeholder, parse, split_firmware_table};
        let Some(buffer) = LiveSystem.firmware_table() else { return Vec::new() };
        let Ok(table) = split_firmware_table(&buffer) else { return Vec::new() };
        let tables = parse(table.data).tables;
        let mut candidates: Vec<(&'static str, Option<String>)> = Vec::new();
        if let Some(system) = &tables.system {
            candidates.push(("system serial", system.serial.clone()));
            if let Some(uuid) = system.uuid {
                // The two common spellings: the hex bytes in order, and the
                // GUID layout with the first three fields byte-swapped.
                let hex =
                    |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
                candidates.push(("system UUID", Some(hex(&uuid))));
                let guid = format!(
                    "{:08x}-{:04x}-{:04x}-{}-{}",
                    u32::from_le_bytes([uuid[0], uuid[1], uuid[2], uuid[3]]),
                    u16::from_le_bytes([uuid[4], uuid[5]]),
                    u16::from_le_bytes([uuid[6], uuid[7]]),
                    hex(&uuid[8..10]),
                    hex(&uuid[10..]),
                );
                candidates.push(("system UUID", Some(guid)));
            }
        }
        if let Some(board) = &tables.board {
            candidates.push(("board serial", board.serial.clone()));
            candidates.push(("board asset tag", board.asset_tag.clone()));
        }
        for processor in &tables.processors {
            candidates.push(("processor serial", processor.serial.clone()));
            candidates.push(("processor asset tag", processor.asset_tag.clone()));
        }
        for module in &tables.memory_devices {
            candidates.push(("memory serial", module.serial.clone()));
            candidates.push(("memory asset tag", module.asset_tag.clone()));
        }
        // Placeholders and very short values (`0`, `None`) would match
        // unrelated text.
        candidates
            .into_iter()
            .filter_map(|(kind, value)| Some((kind, value?)))
            .filter(|(_, value)| value.len() >= 4 && !is_placeholder(value))
            .collect()
    }

    #[test]
    fn answer_for_this_machine_contains_none_of_its_identifiers() {
        let answer =
            format!("{:?}", super::super::assemble::collect_from(&LiveSystem)).to_lowercase();
        for (kind, identifier) in live_identifiers() {
            assert!(
                !answer.contains(&identifier.to_lowercase()),
                "answer: expected no {kind} | received a {}-character {kind} in it",
                identifier.len()
            );
        }
    }

    #[test]
    fn this_machine_reports_installed_memory_and_a_processor_name() {
        let memory =
            LiveSystem.installed_memory_bytes().expect("GetPhysicallyInstalledSystemMemory failed");
        assert!(memory >= 256 << 20, "installed memory: expected >= 256 MiB | received: {memory}");
        let name = LiveSystem.processor_name().expect("registry ProcessorNameString missing");
        assert!(!name.is_empty());
    }
}
