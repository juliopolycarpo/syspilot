//! Used-space percentage of every fixed volume that has a drive letter.
//!
//! Volumes mounted only as folders have no letter and are not listed.

use syspilot_proto::sensor::{DeviceClass, SensorKind};
use syspilot_proto::win::wide;
use windows_sys::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives,
};
use windows_sys::Win32::System::WindowsProgramming::DRIVE_FIXED;

use crate::hub::{Provider, SensorSpec};

/// Space accounting of one volume, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VolumeSpace {
    pub total: u64,
    /// Free bytes on the volume, not the caller's quota-limited share.
    pub free: u64,
}

/// Where volume letters and their space come from.
pub(crate) trait VolumeSource: Send {
    /// Letters (`'C'`, `'D'`, ...) of the fixed volumes, in order.
    fn fixed_letters(&mut self) -> Vec<char>;
    /// Space of the volume, `None` if it cannot be read (locked, offline).
    fn space(&mut self, letter: char) -> Option<VolumeSpace>;
}

/// Letters whose bit is set in a `GetLogicalDrives` mask (bit 0 is `A`).
fn letters_from_mask(mask: u32) -> Vec<char> {
    ('A'..='Z').enumerate().filter(|(bit, _)| mask & (1 << bit) != 0).map(|(_, c)| c).collect()
}

/// Used share of a volume, in percent. `None` for an empty volume or
/// accounting that cannot be right (more free than total).
fn used_percent(space: VolumeSpace) -> Option<f64> {
    if space.total == 0 || space.free > space.total {
        return None;
    }
    Some((space.total - space.free) as f64 / space.total as f64 * 100.0)
}

/// [`VolumeSource`] backed by the Win32 volume APIs.
struct WindowsVolumes;

impl WindowsVolumes {
    fn root(letter: char) -> Vec<u16> {
        wide(&format!("{letter}:\\"))
    }
}

impl VolumeSource for WindowsVolumes {
    fn fixed_letters(&mut self) -> Vec<char> {
        // SAFETY: trivially safe; no arguments.
        let mask = unsafe { GetLogicalDrives() };
        letters_from_mask(mask)
            .into_iter()
            .filter(|&letter| {
                let root = Self::root(letter);
                // SAFETY: `root` is NUL-terminated and outlives the call.
                unsafe { GetDriveTypeW(root.as_ptr()) == DRIVE_FIXED }
            })
            .collect()
    }

    fn space(&mut self, letter: char) -> Option<VolumeSpace> {
        let root = Self::root(letter);
        let (mut available_to_caller, mut total, mut free) = (0u64, 0u64, 0u64);
        // SAFETY: `root` is NUL-terminated and outlives the call; the three
        // out-pointers are valid.
        let ok = unsafe {
            GetDiskFreeSpaceExW(root.as_ptr(), &mut available_to_caller, &mut total, &mut free)
        };
        (ok != 0).then_some(VolumeSpace { total, free })
    }
}

/// Used space of each fixed volume.
pub struct VolumesProvider {
    source: Box<dyn VolumeSource>,
    letters: Vec<char>,
}

impl VolumesProvider {
    /// Builds the provider from the volumes `source` lists and can read.
    fn from_source(mut source: Box<dyn VolumeSource>) -> Option<Self> {
        let letters: Vec<char> = source
            .fixed_letters()
            .into_iter()
            .filter(|&letter| source.space(letter).and_then(used_percent).is_some())
            .collect();
        (!letters.is_empty()).then_some(Self { source, letters })
    }
}

impl Provider for VolumesProvider {
    fn sensors(&self) -> Vec<SensorSpec> {
        self.letters
            .iter()
            .map(|letter| SensorSpec {
                device: "Volumes".to_owned(),
                class: DeviceClass::Storage,
                key: format!("volume.{}.used", letter.to_ascii_lowercase()),
                label: format!("{}: used", letter.to_ascii_uppercase()),
                kind: SensorKind::Level,
            })
            .collect()
    }

    fn sample(&mut self, out: &mut Vec<Option<f64>>) {
        for &letter in &self.letters {
            out.push(self.source.space(letter).and_then(used_percent));
        }
    }

    /// Used space moves slowly and each read walks file-system metadata.
    fn period_secs(&self) -> u32 {
        5
    }
}

/// The volumes provider, if the machine has a readable fixed volume.
pub fn probe() -> Vec<Box<dyn Provider>> {
    match VolumesProvider::from_source(Box::new(WindowsVolumes)) {
        Some(provider) => vec![Box::new(provider)],
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A machine with scripted volumes. A letter without space is "locked".
    struct FakeVolumes {
        fixed: Vec<char>,
        space: HashMap<char, VolumeSpace>,
    }

    impl VolumeSource for FakeVolumes {
        fn fixed_letters(&mut self) -> Vec<char> {
            self.fixed.clone()
        }

        fn space(&mut self, letter: char) -> Option<VolumeSpace> {
            self.space.get(&letter).copied()
        }
    }

    fn space(total: u64, free: u64) -> VolumeSpace {
        VolumeSpace { total, free }
    }

    fn fake(fixed: &[char], spaces: &[(char, VolumeSpace)]) -> Box<FakeVolumes> {
        Box::new(FakeVolumes { fixed: fixed.to_vec(), space: spaces.iter().copied().collect() })
    }

    #[test]
    fn mask_bits_map_to_drive_letters_from_a() {
        assert_eq!(letters_from_mask(0), Vec::<char>::new());
        assert_eq!(letters_from_mask(0b101), ['A', 'C']);
        assert_eq!(letters_from_mask(1 << 25), ['Z']);
        assert_eq!(
            letters_from_mask(u32::MAX),
            ('A'..='Z').collect::<Vec<_>>(),
            "bits 26..31 are ignored"
        );
    }

    #[test]
    fn used_percent_is_the_non_free_share_of_the_total() {
        assert_eq!(used_percent(space(1_000, 250)), Some(75.0));
        assert_eq!(used_percent(space(1_000, 1_000)), Some(0.0));
        assert_eq!(used_percent(space(1_000, 0)), Some(100.0));
    }

    #[test]
    fn impossible_space_accounting_has_no_percentage() {
        assert_eq!(used_percent(space(0, 0)), None, "empty volume");
        assert_eq!(used_percent(space(1_000, 1_001)), None, "more free than total");
    }

    #[test]
    fn provider_lists_each_readable_fixed_volume_with_lowercase_keys() {
        let source = fake(&['C', 'D'], &[('C', space(100, 40)), ('D', space(200, 200))]);
        let mut provider = VolumesProvider::from_source(source).unwrap();
        let specs = provider.sensors();
        let keys: Vec<_> = specs.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys, ["volume.c.used", "volume.d.used"]);
        assert!(specs.iter().all(|s| s.device == "Volumes" && s.kind == SensorKind::Level));
        assert_eq!(specs[0].label, "C: used");

        let mut out = Vec::new();
        provider.sample(&mut out);
        assert_eq!(out, [Some(60.0), Some(0.0)]);
    }

    #[test]
    fn locked_volume_is_not_declared() {
        let source = fake(&['C', 'E'], &[('C', space(100, 10))]);
        let provider = VolumesProvider::from_source(source).unwrap();
        assert_eq!(provider.letters, ['C']);
    }

    #[test]
    fn no_readable_fixed_volume_means_no_provider() {
        assert!(VolumesProvider::from_source(fake(&[], &[])).is_none());
        assert!(VolumesProvider::from_source(fake(&['C'], &[])).is_none());
    }

    #[test]
    fn volume_that_becomes_unreadable_reports_none_without_shifting_others() {
        let source = fake(&['C', 'D'], &[('C', space(100, 50)), ('D', space(100, 50))]);
        let mut provider = VolumesProvider::from_source(source).unwrap();
        // Replace the source with one where D has gone away (BitLocker lock).
        provider.source = fake(&['C', 'D'], &[('C', space(100, 25))]);
        let mut out = Vec::new();
        provider.sample(&mut out);
        assert_eq!(out, [Some(75.0), None]);
    }

    #[test]
    fn volumes_are_read_every_five_seconds() {
        let provider = VolumesProvider::from_source(fake(&['C'], &[('C', space(1, 0))])).unwrap();
        assert_eq!(provider.period_secs(), 5);
    }
}
