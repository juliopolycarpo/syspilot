//! Static facts about the machine: firmware tables, processor, memory,
//! graphics, storage and operating system.
//!
//! [`collect`] answers the service's system-information request. It reads
//! the SMBIOS table, the processor topology, the display driver's adapter
//! list, drive identities and the registry, and nothing else: no WMI, no
//! COM, no child process. A fact the machine cannot supply is left out of
//! the answer instead of failing it.
//!
//! Serial numbers, the system UUID and asset tags identify one machine and
//! are never part of the answer.

mod assemble;
mod format;
mod graphics;
mod live;
mod release;
pub mod smbios;
mod topology;

use syspilot_proto::message::SystemInfo;

/// Reads this machine and builds the summary and the display sections.
pub fn collect() -> SystemInfo {
    assemble::collect_from(&live::LiveSystem)
}
