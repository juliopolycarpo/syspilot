//! Physical memory use and commit charge from `GlobalMemoryStatusEx`.

use syspilot_proto::sensor::{DeviceClass, SensorKind};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

use crate::hub::{Provider, SensorSpec};

/// The figures of one `GlobalMemoryStatusEx` call that the sensors use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MemoryStatus {
    /// Installed physical memory the OS can use, in bytes.
    pub total_physical: u64,
    /// Physical memory available to processes, in bytes.
    pub available_physical: u64,
    /// The system commit limit (RAM plus page files), in bytes.
    pub commit_limit: u64,
    /// Commit limit minus the committed bytes, in bytes.
    pub commit_available: u64,
}

/// The four values in sensor order: load, used, available, committed.
/// `None` for all of them if the totals are impossible (zero or smaller than
/// what is reported available).
fn memory_values(status: &MemoryStatus) -> [Option<f64>; 4] {
    if status.total_physical == 0 || status.available_physical > status.total_physical {
        return [None; 4];
    }
    let used = status.total_physical - status.available_physical;
    let load = used as f64 / status.total_physical as f64 * 100.0;
    let committed = status.commit_limit.checked_sub(status.commit_available);
    [
        Some(load),
        Some(used as f64),
        Some(status.available_physical as f64),
        committed.map(|bytes| bytes as f64),
    ]
}

fn read_status() -> Option<MemoryStatus> {
    let mut raw = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        dwMemoryLoad: 0,
        ullTotalPhys: 0,
        ullAvailPhys: 0,
        ullTotalPageFile: 0,
        ullAvailPageFile: 0,
        ullTotalVirtual: 0,
        ullAvailVirtual: 0,
        ullAvailExtendedVirtual: 0,
    };
    // SAFETY: `raw` is a valid `MEMORYSTATUSEX` with `dwLength` set as the
    // API requires.
    if unsafe { GlobalMemoryStatusEx(&mut raw) } == 0 {
        return None;
    }
    Some(MemoryStatus {
        total_physical: raw.ullTotalPhys,
        available_physical: raw.ullAvailPhys,
        commit_limit: raw.ullTotalPageFile,
        commit_available: raw.ullAvailPageFile,
    })
}

/// Load, used, available and committed memory.
pub struct MemoryProvider;

impl Provider for MemoryProvider {
    fn sensors(&self) -> Vec<SensorSpec> {
        let spec = |key: &str, label: &str, kind| SensorSpec {
            device: "Memory".into(),
            class: DeviceClass::Memory,
            key: key.into(),
            label: label.into(),
            kind,
        };
        vec![
            spec("memory.load", "Load", SensorKind::Load),
            spec("memory.used", "Used", SensorKind::Data),
            spec("memory.available", "Available", SensorKind::Data),
            spec("memory.commit", "Committed", SensorKind::Data),
        ]
    }

    fn sample(&mut self, out: &mut Vec<Option<f64>>) {
        let values = read_status().map_or([None; 4], |status| memory_values(&status));
        out.extend(values);
    }
}

/// The memory provider, if the OS answers `GlobalMemoryStatusEx`.
pub fn probe() -> Vec<Box<dyn Provider>> {
    match read_status() {
        Some(_) => vec![Box::new(MemoryProvider)],
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    fn status(total: u64, available: u64) -> MemoryStatus {
        MemoryStatus {
            total_physical: total,
            available_physical: available,
            commit_limit: 96 * GIB,
            commit_available: 80 * GIB,
        }
    }

    #[test]
    fn used_is_total_minus_available_and_load_is_its_share() {
        let values = memory_values(&status(64 * GIB, 48 * GIB));
        assert_eq!(values[0], Some(25.0));
        assert_eq!(values[1], Some((16 * GIB) as f64));
        assert_eq!(values[2], Some((48 * GIB) as f64));
    }

    #[test]
    fn commit_charge_is_the_limit_minus_what_is_still_available() {
        assert_eq!(memory_values(&status(64 * GIB, 48 * GIB))[3], Some((16 * GIB) as f64));
    }

    #[test]
    fn commit_that_exceeds_its_limit_is_dropped_not_negative() {
        let mut broken = status(64 * GIB, 48 * GIB);
        broken.commit_available = broken.commit_limit + 1;
        let values = memory_values(&broken);
        assert_eq!(values[3], None);
        assert!(values[0].is_some(), "the physical figures stay valid");
    }

    #[test]
    fn impossible_totals_give_no_values() {
        assert_eq!(memory_values(&status(0, 0)), [None; 4], "no memory reported");
        assert_eq!(memory_values(&status(GIB, 2 * GIB)), [None; 4], "available exceeds total");
    }

    #[test]
    fn fully_used_memory_is_one_hundred_percent() {
        assert_eq!(memory_values(&status(GIB, 0))[0], Some(100.0));
    }

    #[test]
    fn sensors_have_unique_keys_and_the_declared_count_is_sampled() {
        let mut provider = MemoryProvider;
        let specs = provider.sensors();
        let mut keys: Vec<_> = specs.iter().map(|spec| spec.key.clone()).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), specs.len());
        let mut out = Vec::new();
        provider.sample(&mut out);
        assert_eq!(out.len(), specs.len());
    }
}
