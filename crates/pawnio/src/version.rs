//! Driver version numbers.

use std::fmt;

/// The oldest driver this crate accepts.
///
/// The ABI was verified against 2.2.0 only. 2.0.0 is the floor of that major
/// series; raise it if an earlier 2.x release turns out to differ.
pub const MIN_DRIVER_VERSION: Version = Version::new(2, 0, 0);

/// A `major.minor.patch` version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    /// Major component.
    pub major: u8,
    /// Minor component.
    pub minor: u8,
    /// Patch component.
    pub patch: u8,
}

impl Version {
    /// A version from its components.
    pub const fn new(major: u8, minor: u8, patch: u8) -> Self {
        Self { major, minor, patch }
    }

    /// Decodes the value `IOCTL_PIO_VERSION` returns:
    /// `(major << 16) | (minor << 8) | patch`. Bits above 23 are ignored.
    pub const fn from_raw(raw: u32) -> Self {
        Self::new((raw >> 16) as u8, (raw >> 8) as u8, raw as u8)
    }

    /// The raw `IOCTL_PIO_VERSION` encoding.
    pub const fn to_raw(self) -> u32 {
        ((self.major as u32) << 16) | ((self.minor as u32) << 8) | self.patch as u32
    }

    /// Parses `2.2.0` or the four-part `2.2.0.0` form that the uninstall
    /// registry key uses. Missing trailing parts count as zero; the fourth
    /// part is ignored. Returns `None` for anything else.
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.trim().split('.');
        let mut next = |required: bool| -> Option<u8> {
            match parts.next() {
                Some(part) => part.parse().ok(),
                None if required => None,
                None => Some(0),
            }
        };
        let major = next(true)?;
        let minor = next(false)?;
        let patch = next(false)?;
        // The fourth component must still be numeric when present.
        if let Some(build) = parts.next() {
            build.parse::<u32>().ok()?;
        }
        if parts.next().is_some() {
            return None;
        }
        Some(Self::new(major, minor, patch))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_value_of_2_2_0_decodes_to_2_2_0() {
        assert_eq!(Version::from_raw(0x0002_0200), Version::new(2, 2, 0));
    }

    #[test]
    fn raw_encoding_round_trips() {
        let version = Version::new(2, 17, 5);
        assert_eq!(Version::from_raw(version.to_raw()), version);
    }

    #[test]
    fn bits_above_the_patch_triple_are_ignored() {
        assert_eq!(Version::from_raw(0xFF02_0100), Version::new(2, 1, 0));
    }

    #[test]
    fn parse_accepts_three_and_four_part_versions() {
        assert_eq!(Version::parse("2.2.0"), Some(Version::new(2, 2, 0)));
        assert_eq!(Version::parse(" 2.2.0.0 "), Some(Version::new(2, 2, 0)));
        assert_eq!(Version::parse("2.1"), Some(Version::new(2, 1, 0)));
        assert_eq!(Version::parse("3"), Some(Version::new(3, 0, 0)));
    }

    #[test]
    fn parse_rejects_malformed_text() {
        for text in ["", "x", "2.x.0", "2.2.0.x", "2.2.0.0.0", "256.0.0", "-1.0.0"] {
            assert_eq!(Version::parse(text), None, "{text:?} must not parse as a version");
        }
    }

    #[test]
    fn versions_order_by_component() {
        assert!(Version::new(2, 2, 0) > MIN_DRIVER_VERSION);
        assert!(Version::new(1, 9, 9) < MIN_DRIVER_VERSION);
        assert!(Version::new(2, 0, 1) > Version::new(2, 0, 0));
    }

    #[test]
    fn display_uses_three_components() {
        assert_eq!(Version::new(2, 2, 0).to_string(), "2.2.0");
    }
}
