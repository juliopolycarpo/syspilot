//! The operating-system surface the engine needs, as traits.
//!
//! Each trait is one narrow capability. The Win32 implementations live in
//! [`crate::win`]; the in-memory ones in [`crate::fake`] let the whole
//! enforcement logic run in tests without touching a real system.

use std::fmt;
use std::time::Duration;

use syspilot_proto::action::{CommandLine, RegData, RunAs};

/// Why an OS operation failed, reduced to what the engine reacts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsErrorKind {
    /// Windows refused the operation. For a setting this means it is
    /// protected and the engine must report it instead of working around it.
    AccessDenied,
    /// The target (key, service, hive, session) does not exist.
    NotFound,
    /// A command ran longer than its timeout and was terminated.
    Timeout,
    /// Anything else.
    Other,
}

/// A failed OS operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OsError {
    /// What the engine should do about it.
    pub kind: OsErrorKind,
    /// The operation and its target, e.g. `write HKLM\SOFTWARE\X\Value`.
    pub context: String,
    /// Win32 error code, or 0 when there is none.
    pub code: u32,
}

impl OsError {
    /// Builds an error from its parts.
    pub fn new(kind: OsErrorKind, context: impl Into<String>, code: u32) -> Self {
        Self { kind, context: context.into(), code }
    }
}

impl fmt::Display for OsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            OsErrorKind::AccessDenied => "access denied",
            OsErrorKind::NotFound => "not found",
            OsErrorKind::Timeout => "timed out",
            OsErrorKind::Other => "failed",
        };
        write!(f, "{}: {kind}", self.context)?;
        if self.code != 0 {
            write!(f, " (error {})", self.code)?;
        }
        Ok(())
    }
}

impl std::error::Error for OsError {}

/// A registry hive the service can reach.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Hive {
    /// `HKEY_LOCAL_MACHINE`.
    Machine,
    /// `HKEY_USERS\<SID>`: the loaded hive of one user.
    User(String),
}

impl fmt::Display for Hive {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Machine => f.write_str("HKLM"),
            Self::User(sid) => write!(f, "HKU\\{sid}"),
        }
    }
}

/// Registry value type codes used by the engine.
pub mod reg_type {
    /// `REG_SZ`.
    pub const SZ: u32 = 1;
    /// `REG_EXPAND_SZ`.
    pub const EXPAND_SZ: u32 = 2;
    /// `REG_BINARY`.
    pub const BINARY: u32 = 3;
    /// `REG_DWORD`.
    pub const DWORD: u32 = 4;
    /// `REG_MULTI_SZ`.
    pub const MULTI_SZ: u32 = 7;
    /// `REG_QWORD`.
    pub const QWORD: u32 = 11;
}

/// A registry value exactly as stored: type code and raw bytes. Keeping the
/// raw form is what makes a revert exact, whatever the original type was.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RegValue {
    /// `REG_*` type code.
    pub kind: u32,
    /// Raw data.
    pub data: Vec<u8>,
}

impl RegValue {
    /// The stored form of `data`, or `None` for [`RegData::Absent`].
    pub fn from_data(data: &RegData) -> Option<Self> {
        let utf16 = |text: &str| -> Vec<u8> {
            text.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect()
        };
        let (kind, data) = match data {
            RegData::Dword(value) => (reg_type::DWORD, value.to_le_bytes().to_vec()),
            RegData::Qword(value) => (reg_type::QWORD, value.to_le_bytes().to_vec()),
            RegData::Sz(text) => (reg_type::SZ, utf16(text)),
            RegData::ExpandSz(text) => (reg_type::EXPAND_SZ, utf16(text)),
            RegData::MultiSz(texts) => {
                let mut bytes: Vec<u8> = texts.iter().flat_map(|text| utf16(text)).collect();
                bytes.extend([0, 0]);
                (reg_type::MULTI_SZ, bytes)
            }
            RegData::Binary(bytes) => (reg_type::BINARY, bytes.clone()),
            RegData::Absent => return None,
        };
        Some(Self { kind, data })
    }

    /// Whether this stored value holds `desired`. Strings compare without
    /// their terminators, because writers disagree on whether to store them.
    pub fn matches(&self, desired: &RegData) -> bool {
        let Some(expected) = Self::from_data(desired) else { return false };
        if self.kind != expected.kind {
            return false;
        }
        match self.kind {
            reg_type::SZ | reg_type::EXPAND_SZ | reg_type::MULTI_SZ => {
                self.strings() == expected.strings()
            }
            _ => self.data == expected.data,
        }
    }

    /// Decodes string data into its elements (one for `REG_SZ`).
    fn strings(&self) -> Vec<String> {
        let units: Vec<u16> =
            self.data.as_chunks::<2>().0.iter().map(|pair| u16::from_le_bytes(*pair)).collect();
        let mut strings: Vec<String> =
            units.split(|unit| *unit == 0).map(String::from_utf16_lossy).collect();
        while strings.last().is_some_and(String::is_empty) {
            strings.pop();
        }
        if self.kind != reg_type::MULTI_SZ {
            // An embedded NUL ends a plain string.
            strings.truncate(1);
            if strings.is_empty() {
                strings.push(String::new());
            }
        }
        strings
    }

    /// Display form matching [`RegData`]'s, e.g. `dword:1`.
    pub fn describe(&self) -> String {
        let number = |width: usize| -> Option<u64> {
            (self.data.len() == width).then(|| {
                let mut bytes = [0u8; 8];
                bytes[..width].copy_from_slice(&self.data);
                u64::from_le_bytes(bytes)
            })
        };
        match self.kind {
            reg_type::DWORD => number(4).map(|value| format!("dword:{value}")),
            reg_type::QWORD => number(8).map(|value| format!("qword:{value}")),
            reg_type::SZ => Some(format!("sz:{:?}", self.strings()[0])),
            reg_type::EXPAND_SZ => Some(format!("expand_sz:{:?}", self.strings()[0])),
            reg_type::MULTI_SZ => Some(format!("multi_sz:{:?}", self.strings())),
            reg_type::BINARY => Some(RegData::Binary(self.data.clone()).to_string()),
            _ => None,
        }
        .unwrap_or_else(|| format!("type {} ({} bytes)", self.kind, self.data.len()))
    }
}

/// Registry access.
pub trait Registry {
    /// Reads a value. `Ok(None)` when the value or its key does not exist.
    fn read(&self, hive: &Hive, key: &str, name: &str) -> Result<Option<RegValue>, OsError>;
    /// Writes a value, creating the key and any missing parents.
    fn write(&self, hive: &Hive, key: &str, name: &str, value: &RegValue) -> Result<(), OsError>;
    /// Deletes a value. Succeeds when it is already gone.
    fn delete_value(&self, hive: &Hive, key: &str, name: &str) -> Result<(), OsError>;
    /// Whether a key exists. A hive that is not loaded reports `false`.
    fn key_exists(&self, hive: &Hive, key: &str) -> Result<bool, OsError>;
    /// Deletes a key if it has no values and no subkeys. Returns whether it
    /// was deleted. Succeeds with `false` when the key is gone or not empty.
    fn delete_key_if_empty(&self, hive: &Hive, key: &str) -> Result<bool, OsError>;
}

/// Service start type as Windows stores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServiceStart {
    /// Loaded by the boot loader (drivers only).
    Boot,
    /// Loaded during kernel initialisation (drivers only).
    System,
    /// Started at boot.
    Automatic,
    /// Started shortly after boot.
    AutomaticDelayed,
    /// Started on demand.
    Manual,
    /// Cannot be started.
    Disabled,
}

impl fmt::Display for ServiceStart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Boot => "boot",
            Self::System => "system",
            Self::Automatic => "automatic",
            Self::AutomaticDelayed => "automatic (delayed)",
            Self::Manual => "manual",
            Self::Disabled => "disabled",
        })
    }
}

/// Service configuration access.
pub trait Services {
    /// The start type of `service`, or `Ok(None)` when it does not exist.
    fn start_type(&self, service: &str) -> Result<Option<ServiceStart>, OsError>;
    /// Changes the start type of `service`.
    fn set_start_type(&self, service: &str, start: ServiceStart) -> Result<(), OsError>;
}

/// AC and DC value indexes of one power setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PowerValues {
    /// Value index on AC power.
    pub ac: u32,
    /// Value index on battery.
    pub dc: u32,
}

/// Power settings of the active scheme. GUIDs are passed in the textual
/// form already validated by `syspilot_proto::action::parse_guid`.
pub trait Power {
    /// Reads a setting, or `Ok(None)` when the active scheme has no such
    /// setting.
    fn read(&self, subgroup: &str, setting: &str) -> Result<Option<PowerValues>, OsError>;
    /// Writes the given indexes and makes them effective. `None` leaves
    /// that power source untouched.
    fn write(
        &self,
        subgroup: &str,
        setting: &str,
        ac: Option<u32>,
        dc: Option<u32>,
    ) -> Result<(), OsError>;
}

/// Runs user-defined commands.
pub trait Launcher {
    /// Runs `command` to completion and returns its exit code. A command
    /// still running after `timeout` is terminated and reported as
    /// [`OsErrorKind::Timeout`]. [`RunAs::User`] without a logged-on user is
    /// [`OsErrorKind::NotFound`].
    fn run(&self, command: &CommandLine, run_as: RunAs, timeout: Duration) -> Result<i32, OsError>;
}

/// Logged-on users.
pub trait Sessions {
    /// SIDs of the users whose registry hives are loaded because they are
    /// logged on interactively.
    fn interactive_user_sids(&self) -> Vec<String>;
}

/// Time source.
pub trait Clock {
    /// Monotonic time since an arbitrary fixed point.
    fn monotonic(&self) -> Duration;
    /// Wall-clock seconds since the Unix epoch.
    fn unix_secs(&self) -> u64;
}

/// Everything the engine needs from the operating system.
pub struct Os {
    /// Registry access.
    pub registry: Box<dyn Registry>,
    /// Service configuration.
    pub services: Box<dyn Services>,
    /// Power settings.
    pub power: Box<dyn Power>,
    /// Command execution.
    pub launcher: Box<dyn Launcher>,
    /// Logged-on users.
    pub sessions: Box<dyn Sessions>,
    /// Time.
    pub clock: Box<dyn Clock>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_converts_to_the_stored_form() {
        let dword = RegValue::from_data(&RegData::Dword(0x0102_0304)).unwrap();
        assert_eq!(dword, RegValue { kind: reg_type::DWORD, data: vec![4, 3, 2, 1] });

        let text = RegValue::from_data(&RegData::Sz("ab".into())).unwrap();
        assert_eq!(text.data, [0x61, 0, 0x62, 0, 0, 0]);

        let multi = RegValue::from_data(&RegData::MultiSz(vec!["a".into(), "b".into()])).unwrap();
        assert_eq!(multi.data, [0x61, 0, 0, 0, 0x62, 0, 0, 0, 0, 0]);

        assert_eq!(RegValue::from_data(&RegData::Absent), None);
    }

    #[test]
    fn matching_requires_the_same_type() {
        let stored = RegValue { kind: reg_type::QWORD, data: 1u64.to_le_bytes().to_vec() };
        assert!(stored.matches(&RegData::Qword(1)));
        assert!(!stored.matches(&RegData::Dword(1)));
        assert!(!stored.matches(&RegData::Absent));
    }

    #[test]
    fn strings_match_with_or_without_a_terminator() {
        let unterminated = RegValue { kind: reg_type::SZ, data: vec![0x61, 0] };
        let terminated = RegValue { kind: reg_type::SZ, data: vec![0x61, 0, 0, 0] };
        assert!(unterminated.matches(&RegData::Sz("a".into())));
        assert!(terminated.matches(&RegData::Sz("a".into())));
        assert!(!terminated.matches(&RegData::Sz("b".into())));
        assert!(!terminated.matches(&RegData::ExpandSz("a".into())));
    }

    #[test]
    fn empty_string_matches_empty_data() {
        let empty = RegValue { kind: reg_type::SZ, data: vec![] };
        assert!(empty.matches(&RegData::Sz(String::new())));
    }

    #[test]
    fn multi_strings_compare_element_wise() {
        let stored = RegValue::from_data(&RegData::MultiSz(vec!["a".into(), "b".into()])).unwrap();
        assert!(stored.matches(&RegData::MultiSz(vec!["a".into(), "b".into()])));
        assert!(!stored.matches(&RegData::MultiSz(vec!["a".into()])));
    }

    #[test]
    fn describe_matches_the_desired_data_notation() {
        for data in [
            RegData::Dword(3),
            RegData::Qword(9),
            RegData::Sz("x y".into()),
            RegData::ExpandSz("%TEMP%".into()),
            RegData::MultiSz(vec!["a".into(), "b".into()]),
            RegData::Binary(vec![0xde, 0xad]),
        ] {
            let stored = RegValue::from_data(&data).unwrap();
            assert_eq!(stored.describe(), data.to_string());
        }
    }

    #[test]
    fn malformed_or_unknown_values_are_described_by_type_and_size() {
        let short_dword = RegValue { kind: reg_type::DWORD, data: vec![1, 2] };
        assert_eq!(short_dword.describe(), "type 4 (2 bytes)");
        let link = RegValue { kind: 6, data: vec![0; 10] };
        assert_eq!(link.describe(), "type 6 (10 bytes)");
    }

    #[test]
    fn error_display_names_the_operation_and_code() {
        let error = OsError::new(OsErrorKind::AccessDenied, r"write HKLM\SOFTWARE\X\Value", 5);
        assert_eq!(error.to_string(), r"write HKLM\SOFTWARE\X\Value: access denied (error 5)");
    }
}
