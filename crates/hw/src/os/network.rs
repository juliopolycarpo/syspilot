//! Download and upload throughput of every connected physical network
//! adapter.
//!
//! "Physical" means the interface table flags the interface as a hardware
//! device and not a filter, and its type is Ethernet, Wi-Fi or mobile
//! broadband. That excludes loopback, tunnels, VPN and WAN miniports, and
//! the virtual adapters of Hyper-V and similar: their traffic is already
//! counted on the physical adapter underneath. The adapter set is fixed at
//! probe time; an adapter that is unplugged then reads no value until it is
//! plugged back in.

use std::ptr;
use std::time::Instant;

use syspilot_proto::sensor::{DeviceClass, SensorKind};
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetIfEntry2, GetIfTable2, IF_TYPE_ETHERNET_CSMACD, IF_TYPE_IEEE80211,
    IF_TYPE_WWANPP, IF_TYPE_WWANPP2, MIB_IF_ROW2, MIB_IF_TABLE2,
};
use windows_sys::Win32::NetworkManagement::Ndis::{IfOperStatusUp, MediaConnectStateConnected};

use super::rate::RateTracker;
use super::text::{clean_utf16, number_repeated_names};
use crate::hub::{Provider, SensorSpec};

/// `HardwareInterface`, bit 0 of `MIB_IF_ROW2::InterfaceAndOperStatusFlags`:
/// the interface is a physical device.
const HARDWARE_INTERFACE: u8 = 1 << 0;
/// `FilterInterface`, bit 1: a filter driver bound over another interface.
const FILTER_INTERFACE: u8 = 1 << 1;

/// What the interface table says about one interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Interface {
    /// Locally unique identifier, stable until reboot; used to look the
    /// interface up again.
    pub luid: u64,
    /// `IfIndex`, used to order adapters.
    pub index: u32,
    /// Driver description, e.g. `Realtek PCIe GbE Family Controller`.
    pub description: String,
    /// `IANAifType`.
    pub if_type: u32,
    pub hardware: bool,
    pub filter: bool,
    /// Operational status is up and the media is connected.
    pub connected: bool,
}

/// Cumulative bytes through an interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Octets {
    pub received: u64,
    pub sent: u64,
}

/// Where the interface list and its counters come from.
pub(crate) trait InterfaceSource: Send {
    /// Every interface of the machine, physical or not.
    fn interfaces(&mut self) -> Vec<Interface>;
    /// Current byte counters of the interface, `None` if it is gone.
    fn octets(&mut self, luid: u64) -> Option<Octets>;
}

/// Whether `interface` is a connected physical adapter worth a sensor.
fn is_connected_physical(interface: &Interface) -> bool {
    let carries_traffic =
        [IF_TYPE_ETHERNET_CSMACD, IF_TYPE_IEEE80211, IF_TYPE_WWANPP, IF_TYPE_WWANPP2]
            .contains(&interface.if_type);
    interface.hardware && !interface.filter && interface.connected && carries_traffic
}

/// Converts a row of the interface table.
fn interface_from_row(row: &MIB_IF_ROW2) -> Interface {
    let flags = row.InterfaceAndOperStatusFlags._bitfield;
    let description = clean_utf16(&row.Description);
    Interface {
        // SAFETY: both members of the LUID union are plain integers of the
        // same size, valid for any bit pattern.
        luid: unsafe { row.InterfaceLuid.Value },
        index: row.InterfaceIndex,
        description: if description.is_empty() {
            format!("Network adapter {}", row.InterfaceIndex)
        } else {
            description
        },
        if_type: row.Type,
        hardware: flags & HARDWARE_INTERFACE != 0,
        filter: flags & FILTER_INTERFACE != 0,
        connected: row.OperStatus == IfOperStatusUp
            && row.MediaConnectState == MediaConnectStateConnected,
    }
}

/// Frees a table returned by `GetIfTable2`.
struct MibTable(*mut MIB_IF_TABLE2);

impl Drop for MibTable {
    fn drop(&mut self) {
        // SAFETY: the pointer came from a successful `GetIfTable2` and is
        // freed exactly once, here.
        unsafe { FreeMibTable(self.0.cast()) };
    }
}

/// [`InterfaceSource`] backed by the IP Helper API.
struct WindowsInterfaces;

impl InterfaceSource for WindowsInterfaces {
    fn interfaces(&mut self) -> Vec<Interface> {
        let mut table: *mut MIB_IF_TABLE2 = ptr::null_mut();
        // SAFETY: `table` is a valid out-pointer.
        let status = unsafe { GetIfTable2(&mut table) };
        if status != ERROR_SUCCESS || table.is_null() {
            return Vec::new();
        }
        let table = MibTable(table);
        // SAFETY: a successful call returns a table of `NumEntries` rows laid
        // out contiguously from `Table`; the pointer is derived from the
        // table pointer itself so it covers every row, and `table` keeps the
        // memory alive until the rows have been converted.
        let rows = unsafe {
            let first = ptr::addr_of!((*table.0).Table).cast::<MIB_IF_ROW2>();
            std::slice::from_raw_parts(first, (*table.0).NumEntries as usize)
        };
        rows.iter().map(interface_from_row).collect()
    }

    fn octets(&mut self, luid: u64) -> Option<Octets> {
        let mut row = MIB_IF_ROW2::default();
        row.InterfaceLuid.Value = luid;
        // SAFETY: `row` is a valid, initialised row whose LUID selects the
        // interface; the call fills the rest.
        let status = unsafe { GetIfEntry2(&mut row) };
        (status == ERROR_SUCCESS).then_some(Octets { received: row.InOctets, sent: row.OutOctets })
    }
}

struct Adapter {
    luid: u64,
    device: String,
    down: RateTracker,
    up: RateTracker,
}

/// Throughput of the connected physical adapters.
pub struct NetworkProvider {
    source: Box<dyn InterfaceSource>,
    adapters: Vec<Adapter>,
}

impl NetworkProvider {
    /// Builds the provider for the connected physical adapters `source` lists,
    /// or `None` if there are none.
    fn from_source(mut source: Box<dyn InterfaceSource>) -> Option<Self> {
        let mut found: Vec<Interface> =
            source.interfaces().into_iter().filter(is_connected_physical).collect();
        found.sort_by_key(|interface| interface.index);
        let mut names: Vec<String> = found.iter().map(|i| i.description.clone()).collect();
        number_repeated_names(&mut names);
        let adapters: Vec<Adapter> = found
            .iter()
            .zip(names)
            .map(|(interface, device)| Adapter {
                luid: interface.luid,
                device,
                down: RateTracker::default(),
                up: RateTracker::default(),
            })
            .collect();
        (!adapters.is_empty()).then_some(Self { source, adapters })
    }

    /// One sample taken at `now`, so tests control the elapsed time.
    fn sample_at(&mut self, now: Instant, out: &mut Vec<Option<f64>>) {
        for adapter in &mut self.adapters {
            match self.source.octets(adapter.luid) {
                Some(octets) => {
                    out.push(adapter.down.update(now, octets.received));
                    out.push(adapter.up.update(now, octets.sent));
                }
                None => out.extend([None, None]),
            }
        }
    }
}

impl Provider for NetworkProvider {
    fn sensors(&self) -> Vec<SensorSpec> {
        let mut specs = Vec::with_capacity(self.adapters.len() * 2);
        for (number, adapter) in self.adapters.iter().enumerate() {
            for (suffix, label) in [("down", "Download"), ("up", "Upload")] {
                specs.push(SensorSpec {
                    device: adapter.device.clone(),
                    class: DeviceClass::Network,
                    key: format!("net{number}.{suffix}"),
                    label: label.to_owned(),
                    kind: SensorKind::Throughput,
                });
            }
        }
        specs
    }

    fn sample(&mut self, out: &mut Vec<Option<f64>>) {
        self.sample_at(Instant::now(), out);
    }
}

/// The network provider, if the machine has a connected physical adapter.
pub fn probe() -> Vec<Box<dyn Provider>> {
    match NetworkProvider::from_source(Box::new(WindowsInterfaces)) {
        Some(provider) => vec![Box::new(provider)],
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;
    use windows_sys::Win32::NetworkManagement::Ndis::{
        IfOperStatusDown, MediaConnectStateDisconnected,
    };

    const ONE_SECOND: Duration = Duration::from_secs(1);

    /// A machine with scripted interfaces whose counters are replayed one
    /// frame per `octets` call (the last frame repeats).
    struct FakeInterfaces {
        interfaces: Vec<Interface>,
        frames: HashMap<u64, Vec<Option<Octets>>>,
        reads: HashMap<u64, usize>,
    }

    impl FakeInterfaces {
        fn new(interfaces: Vec<Interface>) -> Self {
            Self { interfaces, frames: HashMap::new(), reads: HashMap::new() }
        }

        fn with_frames(mut self, luid: u64, frames: Vec<Option<Octets>>) -> Self {
            self.frames.insert(luid, frames);
            self
        }
    }

    impl InterfaceSource for FakeInterfaces {
        fn interfaces(&mut self) -> Vec<Interface> {
            self.interfaces.clone()
        }

        fn octets(&mut self, luid: u64) -> Option<Octets> {
            let frames = self.frames.get(&luid)?;
            let read = self.reads.entry(luid).or_default();
            let frame = frames[(*read).min(frames.len() - 1)];
            *read += 1;
            frame
        }
    }

    fn interface(luid: u64, index: u32, description: &str) -> Interface {
        Interface {
            luid,
            index,
            description: description.to_owned(),
            if_type: IF_TYPE_ETHERNET_CSMACD,
            hardware: true,
            filter: false,
            connected: true,
        }
    }

    fn octets(received: u64, sent: u64) -> Option<Octets> {
        Some(Octets { received, sent })
    }

    fn row_with(flags: u8, if_type: u32, oper_up: bool, media_connected: bool) -> MIB_IF_ROW2 {
        let mut row = MIB_IF_ROW2::default();
        row.InterfaceLuid.Value = 0xABCD;
        row.InterfaceIndex = 9;
        row.Type = if_type;
        row.InterfaceAndOperStatusFlags._bitfield = flags;
        row.OperStatus = if oper_up { IfOperStatusUp } else { IfOperStatusDown };
        row.MediaConnectState = if media_connected {
            MediaConnectStateConnected
        } else {
            MediaConnectStateDisconnected
        };
        for (slot, unit) in row.Description.iter_mut().zip("  Realtek GbE \0junk".encode_utf16()) {
            *slot = unit;
        }
        row
    }

    // ---- classification --------------------------------------------------

    #[test]
    fn a_connected_hardware_ethernet_adapter_is_physical() {
        assert!(is_connected_physical(&interface(1, 9, "Realtek")));
    }

    #[test]
    fn wifi_and_mobile_broadband_count_as_physical() {
        for if_type in [IF_TYPE_IEEE80211, IF_TYPE_WWANPP, IF_TYPE_WWANPP2] {
            let adapter = Interface { if_type, ..interface(1, 9, "Radio") };
            assert!(is_connected_physical(&adapter), "IANAifType {if_type}");
        }
    }

    #[test]
    fn virtual_filter_tunnel_loopback_and_disconnected_interfaces_are_not() {
        let base = interface(1, 9, "x");
        let cases = [
            ("virtual", Interface { hardware: false, ..base.clone() }),
            ("filter", Interface { filter: true, ..base.clone() }),
            ("disconnected", Interface { connected: false, ..base.clone() }),
            ("tunnel", Interface { if_type: 131, ..base.clone() }),
            ("loopback", Interface { if_type: 24, ..base.clone() }),
            ("ppp", Interface { if_type: 23, ..base.clone() }),
        ];
        for (name, adapter) in cases {
            assert!(!is_connected_physical(&adapter), "{name}: expected not physical");
        }
    }

    // ---- table rows ------------------------------------------------------------

    #[test]
    fn row_conversion_reads_identity_flags_and_cleans_the_description() {
        let row = row_with(HARDWARE_INTERFACE, IF_TYPE_ETHERNET_CSMACD, true, true);
        let converted = interface_from_row(&row);
        assert_eq!(
            converted,
            Interface {
                luid: 0xABCD,
                index: 9,
                description: "Realtek GbE".to_owned(),
                if_type: IF_TYPE_ETHERNET_CSMACD,
                hardware: true,
                filter: false,
                connected: true,
            }
        );
    }

    #[test]
    fn row_flag_bits_map_to_hardware_and_filter_independently() {
        let filter_only = interface_from_row(&row_with(FILTER_INTERFACE, 6, true, true));
        assert_eq!((filter_only.hardware, filter_only.filter), (false, true));
        // Bits above the two used (ConnectorPresent, Paused...) change nothing.
        let noisy = interface_from_row(&row_with(HARDWARE_INTERFACE | 0b1111_1100, 6, true, true));
        assert_eq!((noisy.hardware, noisy.filter), (true, false));
    }

    #[test]
    fn row_is_connected_only_when_operationally_up_and_media_connected() {
        for (oper_up, media, expected) in
            [(true, true, true), (false, true, false), (true, false, false), (false, false, false)]
        {
            let row = row_with(HARDWARE_INTERFACE, 6, oper_up, media);
            assert_eq!(
                interface_from_row(&row).connected,
                expected,
                "oper up {oper_up}, media connected {media}"
            );
        }
    }

    #[test]
    fn row_without_a_description_gets_a_name_from_its_index() {
        let mut row = row_with(HARDWARE_INTERFACE, 6, true, true);
        row.Description = [0; 257];
        assert_eq!(interface_from_row(&row).description, "Network adapter 9");
    }

    // ---- provider ----------------------------------------------------------------

    #[test]
    fn only_connected_physical_adapters_get_sensors_ordered_by_interface_index() {
        let source = FakeInterfaces::new(vec![
            interface(30, 12, "Second NIC"),
            Interface { hardware: false, ..interface(40, 5, "vEthernet") },
            interface(10, 9, "First NIC"),
            Interface { connected: false, ..interface(50, 3, "Unplugged") },
        ]);
        let provider = NetworkProvider::from_source(Box::new(source)).unwrap();
        let specs = provider.sensors();
        let summary: Vec<_> = specs.iter().map(|s| (s.key.as_str(), s.device.as_str())).collect();
        assert_eq!(
            summary,
            [
                ("net0.down", "First NIC"),
                ("net0.up", "First NIC"),
                ("net1.down", "Second NIC"),
                ("net1.up", "Second NIC"),
            ]
        );
        assert!(specs.iter().all(|s| s.class == DeviceClass::Network));
        assert!(specs.iter().all(|s| s.kind == SensorKind::Throughput));
        assert_eq!((specs[0].label.as_str(), specs[1].label.as_str()), ("Download", "Upload"));
    }

    #[test]
    fn machine_without_a_connected_physical_adapter_has_no_provider() {
        let source = FakeInterfaces::new(vec![
            Interface { hardware: false, ..interface(1, 1, "Hyper-V") },
            Interface { connected: false, ..interface(2, 2, "Unplugged") },
        ]);
        assert!(NetworkProvider::from_source(Box::new(source)).is_none());
        assert!(NetworkProvider::from_source(Box::new(FakeInterfaces::new(Vec::new()))).is_none());
    }

    #[test]
    fn identical_adapters_stay_separate_devices() {
        let source =
            FakeInterfaces::new(vec![interface(1, 1, "Same NIC"), interface(2, 2, "Same NIC")]);
        let provider = NetworkProvider::from_source(Box::new(source)).unwrap();
        let devices: Vec<_> = provider.sensors().into_iter().map(|s| s.device).collect();
        assert_eq!(devices, ["Same NIC", "Same NIC", "Same NIC (2)", "Same NIC (2)"]);
    }

    #[test]
    fn rates_start_empty_then_use_the_real_elapsed_time() {
        let source = FakeInterfaces::new(vec![interface(7, 1, "NIC")])
            .with_frames(7, vec![octets(1_000, 100), octets(3_000, 100), octets(3_000, 1_100)]);
        let mut provider = NetworkProvider::from_source(Box::new(source)).unwrap();
        let start = Instant::now();
        let mut out = Vec::new();

        provider.sample_at(start, &mut out);
        assert_eq!(out, [None, None], "no baseline on the first read");

        out.clear();
        provider.sample_at(start + ONE_SECOND, &mut out);
        assert_eq!(out, [Some(2_000.0), Some(0.0)]);

        out.clear();
        provider.sample_at(start + 3 * ONE_SECOND, &mut out);
        assert_eq!(out, [Some(0.0), Some(500.0)], "1 000 bytes over 2 s");
    }

    #[test]
    fn adapter_that_disappears_reports_none_and_recovers_with_its_baseline() {
        let source = FakeInterfaces::new(vec![interface(7, 1, "NIC")])
            .with_frames(7, vec![octets(0, 0), None, octets(4_000, 0)]);
        let mut provider = NetworkProvider::from_source(Box::new(source)).unwrap();
        let start = Instant::now();
        let mut out = Vec::new();
        provider.sample_at(start, &mut out);
        out.clear();
        provider.sample_at(start + ONE_SECOND, &mut out);
        assert_eq!(out, [None, None], "interface gone");
        out.clear();
        provider.sample_at(start + 2 * ONE_SECOND, &mut out);
        assert_eq!(out, [Some(2_000.0), Some(0.0)], "baseline kept across the gap");
    }

    #[test]
    fn counter_reset_gives_none_not_a_negative_rate() {
        let source = FakeInterfaces::new(vec![interface(7, 1, "NIC")])
            .with_frames(7, vec![octets(9_000, 9_000), octets(10, 10)]);
        let mut provider = NetworkProvider::from_source(Box::new(source)).unwrap();
        let start = Instant::now();
        let mut out = Vec::new();
        provider.sample_at(start, &mut out);
        out.clear();
        provider.sample_at(start + ONE_SECOND, &mut out);
        assert_eq!(out, [None, None]);
    }
}
