//! Graphics adapters: what the display driver reports, joined with the
//! driver's version and date from the display class registry key.
//!
//! The kernel adapter list has the names and memory; the registry has the
//! driver version and date, one `0000`, `0001`... subkey per installed
//! display device. A subkey belongs to an adapter when its `DriverDesc` is
//! the adapter's name.

use crate::os::gpu::{self, SegmentSizes};
use crate::os::registry;

/// The display adapter device setup class.
const DISPLAY_CLASS_KEY: &str =
    r"SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}";

/// One graphics adapter, ready to show.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct GraphicsAdapter {
    pub name: String,
    /// On-board memory in bytes.
    pub dedicated_bytes: Option<u64>,
    /// System memory the adapter may borrow, in bytes.
    pub shared_bytes: Option<u64>,
    pub driver_version: Option<String>,
    /// As the driver registers it (`M-D-YYYY`).
    pub driver_date: Option<String>,
    pub driver_provider: Option<String>,
}

/// A display class subkey.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct DisplayDriver {
    pub description: String,
    pub version: Option<String>,
    pub date: Option<String>,
    pub provider: Option<String>,
    /// `HardwareInformation.qwMemorySize`: the memory the driver detected.
    pub memory_bytes: Option<u64>,
}

/// The driver entry for the adapter called `name`. Several entries can share
/// a description (identical cards); the first one wins, as the entries of
/// identical cards carry the same version and date.
fn driver_for<'a>(name: &str, drivers: &'a [DisplayDriver]) -> Option<&'a DisplayDriver> {
    drivers.iter().find(|driver| driver.description.eq_ignore_ascii_case(name))
}

/// The adapter named `name` with its memory and driver details.
///
/// Dedicated memory is the driver-declared video memory; the registry's
/// detected size covers drivers that report zero there, and the system
/// memory carved out at boot covers integrated adapters that have no video
/// memory at all.
pub(crate) fn describe_adapter(
    name: &str,
    sizes: Option<SegmentSizes>,
    drivers: &[DisplayDriver],
) -> GraphicsAdapter {
    let driver = driver_for(name, drivers);
    let nonzero = |bytes: u64| (bytes != 0).then_some(bytes);
    let dedicated_bytes = sizes
        .and_then(|sizes| nonzero(sizes.dedicated_video))
        .or_else(|| driver.and_then(|driver| driver.memory_bytes).and_then(nonzero))
        .or_else(|| sizes.and_then(|sizes| nonzero(sizes.dedicated_system)));
    GraphicsAdapter {
        name: name.to_owned(),
        dedicated_bytes,
        shared_bytes: sizes.and_then(|sizes| nonzero(sizes.shared_system)),
        driver_version: driver.and_then(|driver| driver.version.clone()),
        driver_date: driver.and_then(|driver| driver.date.clone()),
        driver_provider: driver.and_then(|driver| driver.provider.clone()),
    }
}

/// Whether a subkey name is one of the numbered device entries (`0000`);
/// the class key also holds `Configuration` and `Properties`.
fn is_device_entry(name: &str) -> bool {
    name.len() == 4 && name.bytes().all(|byte| byte.is_ascii_digit())
}

/// The driver entries registered for display devices.
pub(crate) fn display_drivers() -> Vec<DisplayDriver> {
    registry::subkey_names(DISPLAY_CLASS_KEY)
        .into_iter()
        .filter(|name| is_device_entry(name))
        .filter_map(|name| {
            let key = format!(r"{DISPLAY_CLASS_KEY}\{name}");
            Some(DisplayDriver {
                description: registry::read_string(&key, "DriverDesc")?,
                version: registry::read_string(&key, "DriverVersion"),
                date: registry::read_string(&key, "DriverDate"),
                provider: registry::read_string(&key, "ProviderName"),
                memory_bytes: registry::read_u64(&key, "HardwareInformation.qwMemorySize"),
            })
        })
        .collect()
}

/// Every hardware graphics adapter of this machine. Software rasterisers
/// (the Basic Render Driver) are left out.
pub(crate) fn graphics_adapters() -> Vec<GraphicsAdapter> {
    let drivers = display_drivers();
    gpu::adapters()
        .iter()
        .filter(|adapter| !adapter.software)
        .map(|adapter| describe_adapter(&adapter.name, adapter.segment_sizes(), &drivers))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    fn driver(description: &str, version: &str, memory: Option<u64>) -> DisplayDriver {
        DisplayDriver {
            description: description.to_owned(),
            version: Some(version.to_owned()),
            date: Some("5-20-2026".to_owned()),
            provider: Some("Vendor, Inc.".to_owned()),
            memory_bytes: memory,
        }
    }

    fn sizes(video: u64, system: u64, shared: u64) -> SegmentSizes {
        SegmentSizes { dedicated_video: video, dedicated_system: system, shared_system: shared }
    }

    #[test]
    fn adapter_takes_memory_from_the_kernel_and_driver_details_from_the_registry() {
        let drivers = [driver("Radeon RX 550", "31.0.1", Some(4 * GIB))];
        let adapter = describe_adapter("Radeon RX 550", Some(sizes(4 * GIB, 0, 8 * GIB)), &drivers);
        assert_eq!(adapter.dedicated_bytes, Some(4 * GIB));
        assert_eq!(adapter.shared_bytes, Some(8 * GIB));
        assert_eq!(adapter.driver_version.as_deref(), Some("31.0.1"));
        assert_eq!(adapter.driver_date.as_deref(), Some("5-20-2026"));
        assert_eq!(adapter.driver_provider.as_deref(), Some("Vendor, Inc."));
    }

    #[test]
    fn driver_is_matched_by_description_ignoring_case() {
        let drivers = [driver("Other Card", "1", None), driver("RADEON rx 550", "2", None)];
        let adapter = describe_adapter("Radeon RX 550", None, &drivers);
        assert_eq!(adapter.driver_version.as_deref(), Some("2"));
    }

    #[test]
    fn adapter_without_a_registry_entry_has_no_driver_details() {
        let adapter =
            describe_adapter("Mystery", Some(sizes(GIB, 0, 0)), &[driver("Other", "1", None)]);
        assert_eq!(adapter.driver_version, None);
        assert_eq!(adapter.driver_date, None);
        assert_eq!(adapter.dedicated_bytes, Some(GIB));
    }

    #[test]
    fn dedicated_memory_falls_back_to_the_registry_then_to_carved_out_system_memory() {
        let drivers = [driver("Card", "1", Some(2 * GIB))];
        assert_eq!(
            describe_adapter("Card", Some(sizes(0, GIB, 0)), &drivers).dedicated_bytes,
            Some(2 * GIB),
            "driver reports no video memory: the registry's detected size is used"
        );
        assert_eq!(
            describe_adapter("Card", Some(sizes(0, GIB, 0)), &[driver("Card", "1", None)])
                .dedicated_bytes,
            Some(GIB),
            "no registry size either: the boot-time carve-out is used"
        );
        assert_eq!(describe_adapter("Card", None, &[]).dedicated_bytes, None);
    }

    #[test]
    fn zero_sizes_are_unknown_not_zero_bytes() {
        let adapter =
            describe_adapter("Card", Some(sizes(0, 0, 0)), &[driver("Card", "1", Some(0))]);
        assert_eq!((adapter.dedicated_bytes, adapter.shared_bytes), (None, None));
    }

    #[test]
    fn only_numbered_entries_of_the_class_key_are_devices() {
        for name in ["0000", "0001", "0012"] {
            assert!(is_device_entry(name), "{name}: expected a device entry");
        }
        for name in ["Configuration", "Properties", "000", "00000", "00a1", ""] {
            assert!(!is_device_entry(name), "{name:?}: expected not a device entry");
        }
    }

    #[test]
    fn every_hardware_adapter_found_has_a_name() {
        // Machines without a display adapter (a build server) give an empty list.
        for adapter in graphics_adapters() {
            assert!(
                !adapter.name.is_empty(),
                "adapter name: expected non-empty | received: {adapter:?}"
            );
        }
    }
}
