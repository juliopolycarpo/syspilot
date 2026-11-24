//! Release versions: three numbers, compared in order.

use std::fmt;

/// A release version such as `0.2.1`.
///
/// The fields are compared in declaration order, which is what the derived
/// ordering does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    major: u16,
    minor: u16,
    patch: u16,
}

impl Version {
    /// Parses `major.minor.patch`, digits only. Anything else is `None`:
    /// the text comes from the network or from a package, and what is
    /// accepted here ends up in a URL and in front of the user.
    pub fn parse(text: &str) -> Option<Self> {
        let mut numbers = text.split('.').map(|part| {
            let plain =
                !part.is_empty() && part.len() <= 5 && part.bytes().all(|b| b.is_ascii_digit());
            plain.then(|| part.parse::<u16>().ok()).flatten()
        });
        let version =
            Self { major: numbers.next()??, minor: numbers.next()??, patch: numbers.next()?? };
        numbers.next().is_none().then_some(version)
    }

    /// Parses a release tag: the version with a `v` in front.
    pub fn from_tag(tag: &str) -> Option<Self> {
        Self::parse(tag.strip_prefix('v')?)
    }

    /// The release tag of this version, e.g. `v0.2.1`.
    pub fn tag(self) -> String {
        format!("v{self}")
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

    fn version(text: &str) -> Version {
        Version::parse(text).unwrap_or_else(|| panic!("expected {text:?} to be a version"))
    }

    #[test]
    fn versions_parse_and_print_the_same() {
        for text in ["0.1.1", "0.2.0", "12.34.5678", "65535.0.0"] {
            assert_eq!(version(text).to_string(), text);
        }
        assert_eq!(Version::from_tag("v0.2.1"), Some(version("0.2.1")));
        assert_eq!(version("0.2.1").tag(), "v0.2.1");
    }

    #[test]
    fn versions_compare_number_by_number() {
        assert!(version("0.2.0") > version("0.1.9"));
        assert!(version("0.10.0") > version("0.9.9"), "numbers, not text");
        assert!(version("1.0.0") > version("0.99.99"));
        assert!(version("0.1.1") == version("0.1.1"));
        assert!(version("0.1.2") > version("0.1.1"));
    }

    #[test]
    fn anything_else_is_refused() {
        for text in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "v1.2.3",
            "1.2.x",
            "1.2.-3",
            "1.2.+3",
            " 1.2.3",
            "1.2.3 ",
            "1..3",
            "1.2.3-rc1",
            "65536.0.0",
            "000001.2.3",
            "1.2.3/../x",
        ] {
            assert_eq!(Version::parse(text), None, "(version): expected {text:?} to be refused");
        }
        for tag in ["0.2.1", "V0.2.1", "vv0.2.1", "v", "latest"] {
            assert_eq!(Version::from_tag(tag), None, "(tag): expected {tag:?} to be refused");
        }
    }
}
