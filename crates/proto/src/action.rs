//! The action model: one enforceable unit of system state.
//!
//! These types appear in three places: the tweak catalog (TOML), the
//! profile (TOML) and IPC frames (`postcard`). `postcard` is not
//! self-describing, so the types use only externally tagged enums and no
//! `skip_serializing_if`, `flatten` or `untagged` attributes.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Shortest allowed interval trigger. Anything faster is a polling loop,
/// which the overhead budget forbids.
pub const MIN_INTERVAL_SECS: u32 = 60;
/// Longest a command action may run before it is terminated.
pub const MAX_COMMAND_TIMEOUT_SECS: u32 = 3600;

/// One enforceable unit: what to detect, apply and revert.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    /// A registry value.
    Registry(RegistryAction),
    /// The start type of a Windows service.
    Service(ServiceAction),
    /// A power setting of the active power scheme.
    Power(PowerAction),
    /// A user-defined command.
    Command(CommandAction),
}

/// Whose registry hive a registry action targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Scope {
    /// `HKEY_LOCAL_MACHINE`.
    Machine,
    /// `HKEY_CURRENT_USER` of every interactively logged-on user, written by
    /// the service through `HKEY_USERS\<SID>`.
    User,
}

/// A registry value and the data it must hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryAction {
    /// Hive the key lives in.
    pub scope: Scope,
    /// Key path relative to the hive root, without a leading backslash.
    pub key: String,
    /// Value name. Empty means the key's default value.
    pub name: String,
    /// Desired data.
    pub data: RegData,
}

/// Desired content of a registry value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RegData {
    /// `REG_DWORD`.
    Dword(u32),
    /// `REG_QWORD`.
    Qword(u64),
    /// `REG_SZ`.
    Sz(String),
    /// `REG_EXPAND_SZ`.
    ExpandSz(String),
    /// `REG_MULTI_SZ`.
    MultiSz(Vec<String>),
    /// `REG_BINARY`.
    Binary(Vec<u8>),
    /// The value must not exist.
    Absent,
}

/// A service and the start type it must have.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceAction {
    /// Service key name (not the display name).
    pub service: String,
    /// Desired start type.
    pub start: StartType,
}

/// Start types an action may set. Boot and system start are driver-only and
/// deliberately not offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StartType {
    /// Started at boot.
    Automatic,
    /// Started shortly after boot.
    AutomaticDelayed,
    /// Started on demand.
    Manual,
    /// Cannot be started.
    Disabled,
}

/// A power setting, addressed by its subgroup and setting GUIDs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerAction {
    /// Subgroup GUID, in `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` form.
    pub subgroup: String,
    /// Setting GUID, same form.
    pub setting: String,
    /// Desired value index on AC power, if enforced.
    pub ac: Option<u32>,
    /// Desired value index on battery, if enforced.
    pub dc: Option<u32>,
}

/// Account a command runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunAs {
    /// LocalSystem, in session 0.
    System,
    /// The interactively logged-on user, in their session.
    User,
}

/// An executable with its arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandLine {
    /// Absolute path of the executable. A leading `%Variable%` is expanded
    /// by the service.
    pub program: String,
    /// Arguments, passed without shell interpretation.
    #[serde(default)]
    pub args: Vec<String>,
}

/// A command that reports whether the desired state already holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CommandCheck {
    /// Command to run.
    pub command: CommandLine,
    /// Exit code that means "already compliant".
    #[serde(default)]
    pub expect_exit: i32,
}

/// A user-defined command action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CommandAction {
    /// Command that brings the system into the desired state.
    pub apply: CommandLine,
    /// Account to run every command of this action under.
    pub run_as: RunAs,
    /// Seconds before a running command is terminated.
    pub timeout_secs: u32,
    /// Optional detection command. Without one the action is applied on
    /// every trigger and never reports drift.
    pub check: Option<CommandCheck>,
    /// Optional command that undoes `apply`, run on revert.
    pub undo: Option<CommandLine>,
}

/// When an action is (re-)evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Trigger {
    /// When the service starts, which is at boot.
    Boot,
    /// When a user logs on.
    Logon,
    /// When the machine resumes from sleep or hibernation.
    Resume,
    /// When the underlying state changes (registry notification).
    Change,
    /// Every this many seconds.
    Interval(u32),
}

/// Why an [`Action`] or trigger list is not acceptable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidAction {
    /// Dotted path of the offending field, e.g. `registry.key`.
    pub field: &'static str,
    /// What a valid value looks like.
    pub expected: &'static str,
    /// The rejected value.
    pub received: String,
}

impl fmt::Display for InvalidAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({}): expected {} | received: {:?}", self.field, self.expected, self.received)
    }
}

impl std::error::Error for InvalidAction {}

fn invalid(field: &'static str, expected: &'static str, received: &str) -> InvalidAction {
    InvalidAction { field, expected, received: received.to_owned() }
}

impl Action {
    /// Checks every field against the shape the service accepts.
    pub fn validate(&self) -> Result<(), InvalidAction> {
        match self {
            Self::Registry(action) => action.validate(),
            Self::Service(action) => action.validate(),
            Self::Power(action) => action.validate(),
            Self::Command(action) => action.validate(),
        }
    }

    /// Whether the action targets per-user state and therefore needs a
    /// logged-on user to be applied.
    pub fn is_per_user(&self) -> bool {
        match self {
            Self::Registry(action) => action.scope == Scope::User,
            Self::Command(action) => action.run_as == RunAs::User,
            Self::Service(_) | Self::Power(_) => false,
        }
    }

    /// Whether the original state can be restored exactly on revert.
    pub fn is_reversible(&self) -> bool {
        match self {
            Self::Registry(_) | Self::Service(_) | Self::Power(_) => true,
            Self::Command(action) => action.undo.is_some(),
        }
    }

    /// Triggers used when a definition does not list any: evaluate at boot
    /// (or logon for per-user state) and whenever the state changes.
    pub fn default_triggers(&self) -> Vec<Trigger> {
        match self {
            Self::Command(action) if action.run_as == RunAs::User => vec![Trigger::Logon],
            Self::Command(_) => vec![Trigger::Boot],
            _ if self.is_per_user() => vec![Trigger::Logon, Trigger::Change],
            _ => vec![Trigger::Boot, Trigger::Change],
        }
    }

    /// One-line technical description for the Advanced level, e.g.
    /// `HKLM\SOFTWARE\Policies\…\AllowTelemetry = dword:0`.
    pub fn describe(&self) -> String {
        match self {
            Self::Registry(action) => {
                let root = match action.scope {
                    Scope::Machine => "HKLM",
                    Scope::User => "HKCU",
                };
                let name = if action.name.is_empty() { "(Default)" } else { &action.name };
                format!(r"{root}\{}\{name} = {}", action.key, action.data)
            }
            Self::Service(action) => {
                format!("Service {} start type = {}", action.service, action.start)
            }
            Self::Power(action) => {
                let index = |value: Option<u32>| value.map_or("-".to_owned(), |v| v.to_string());
                format!(
                    "Power setting {}/{} AC = {}, DC = {}",
                    action.subgroup,
                    action.setting,
                    index(action.ac),
                    index(action.dc)
                )
            }
            Self::Command(action) => {
                let account = match action.run_as {
                    RunAs::System => "SYSTEM",
                    RunAs::User => "user",
                };
                format!("Run as {account}: {}", action.apply)
            }
        }
    }
}

impl fmt::Display for RegData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dword(value) => write!(f, "dword:{value}"),
            Self::Qword(value) => write!(f, "qword:{value}"),
            Self::Sz(value) => write!(f, "sz:{value:?}"),
            Self::ExpandSz(value) => write!(f, "expand_sz:{value:?}"),
            Self::MultiSz(values) => write!(f, "multi_sz:{values:?}"),
            Self::Binary(bytes) => {
                f.write_str("binary:")?;
                bytes.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
            }
            Self::Absent => f.write_str("(absent)"),
        }
    }
}

impl fmt::Display for StartType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Automatic => "automatic",
            Self::AutomaticDelayed => "automatic (delayed)",
            Self::Manual => "manual",
            Self::Disabled => "disabled",
        })
    }
}

impl fmt::Display for CommandLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.program)?;
        self.args.iter().try_for_each(|arg| write!(f, " {arg}"))
    }
}

impl RegistryAction {
    fn validate(&self) -> Result<(), InvalidAction> {
        let key = &self.key;
        let well_formed = !key.is_empty()
            && !key.starts_with('\\')
            && !key.ends_with('\\')
            && !key.contains(r"\\")
            && !key.contains('\0')
            && key.split('\\').all(|part| part.len() <= 255);
        if !well_formed {
            return Err(invalid(
                "registry.key",
                r"a key path relative to the hive, like SOFTWARE\Vendor\Key",
                key,
            ));
        }
        // A hive prefix means the author mixed up `scope` and `key`.
        let first = key.split('\\').next().unwrap_or_default().to_ascii_uppercase();
        if first.starts_with("HKEY_") || matches!(first.as_str(), "HKLM" | "HKCU" | "HKU" | "HKCR")
        {
            return Err(invalid("registry.key", "no hive prefix; the hive comes from scope", key));
        }
        if self.name.len() > 16383 || self.name.contains('\0') {
            return Err(invalid("registry.name", "a value name without NUL", &self.name));
        }
        let strings: &[String] = match &self.data {
            RegData::Sz(text) | RegData::ExpandSz(text) => std::slice::from_ref(text),
            RegData::MultiSz(texts) => texts,
            _ => &[],
        };
        if let Some(text) = strings.iter().find(|text| text.contains('\0')) {
            return Err(invalid("registry.data", "strings without NUL", text));
        }
        if let RegData::MultiSz(texts) = &self.data
            && let Some(empty) = texts.iter().find(|text| text.is_empty())
        {
            // An empty string terminates a REG_MULTI_SZ, so it cannot be an element.
            return Err(invalid("registry.data", "non-empty multi_sz elements", empty));
        }
        Ok(())
    }
}

impl ServiceAction {
    fn validate(&self) -> Result<(), InvalidAction> {
        let name = &self.service;
        let valid = !name.is_empty()
            && name.len() <= 256
            && !name.contains(['\\', '/', '\0'])
            && name.trim() == name;
        if valid {
            Ok(())
        } else {
            Err(invalid("service.service", "a service key name such as DiagTrack", name))
        }
    }
}

impl PowerAction {
    fn validate(&self) -> Result<(), InvalidAction> {
        if parse_guid(&self.subgroup).is_none() {
            return Err(invalid("power.subgroup", "a GUID", &self.subgroup));
        }
        if parse_guid(&self.setting).is_none() {
            return Err(invalid("power.setting", "a GUID", &self.setting));
        }
        if self.ac.is_none() && self.dc.is_none() {
            return Err(invalid("power", "at least one of ac and dc", "neither"));
        }
        Ok(())
    }
}

impl CommandLine {
    fn validate(&self, field: &'static str) -> Result<(), InvalidAction> {
        let program = &self.program;
        let bytes = program.as_bytes();
        let drive_absolute = bytes.len() > 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && bytes[2] == b'\\';
        let variable_rooted = program.starts_with('%')
            && program[1..].find('%').is_some_and(|end| program[end + 2..].starts_with('\\'));
        // The service runs commands as SYSTEM: a bare name would be resolved
        // through the search path, which other software can influence.
        if !(drive_absolute || variable_rooted) || program.contains(['"', '\0']) {
            return Err(invalid(
                field,
                r"an absolute path such as C:\Tools\x.exe or %SystemRoot%\System32\x.exe",
                program,
            ));
        }
        if let Some(arg) = self.args.iter().find(|arg| arg.contains('\0')) {
            return Err(invalid(field, "arguments without NUL", arg));
        }
        Ok(())
    }
}

impl CommandAction {
    fn validate(&self) -> Result<(), InvalidAction> {
        self.apply.validate("command.apply.program")?;
        if let Some(check) = &self.check {
            check.command.validate("command.check.command.program")?;
        }
        if let Some(undo) = &self.undo {
            undo.validate("command.undo.program")?;
        }
        if !(1..=MAX_COMMAND_TIMEOUT_SECS).contains(&self.timeout_secs) {
            return Err(invalid(
                "command.timeout_secs",
                "1..=3600 seconds",
                &self.timeout_secs.to_string(),
            ));
        }
        Ok(())
    }
}

/// Checks a trigger list: no duplicates and no sub-minute intervals.
pub fn validate_triggers(triggers: &[Trigger]) -> Result<(), InvalidAction> {
    for (index, trigger) in triggers.iter().enumerate() {
        if let Trigger::Interval(secs) = trigger
            && *secs < MIN_INTERVAL_SECS
        {
            return Err(invalid("triggers", "interval >= 60 seconds", &secs.to_string()));
        }
        let same_kind =
            |other: &Trigger| std::mem::discriminant(other) == std::mem::discriminant(trigger);
        if triggers[..index].iter().any(same_kind) {
            return Err(invalid(
                "triggers",
                "each trigger kind at most once",
                &format!("{trigger:?}"),
            ));
        }
    }
    Ok(())
}

/// Parses a GUID in `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` form (braces
/// optional) into the field layout of the Win32 `GUID` structure.
pub fn parse_guid(text: &str) -> Option<(u32, u16, u16, [u8; 8])> {
    let text = text.strip_prefix('{').and_then(|t| t.strip_suffix('}')).unwrap_or(text);
    let parts: Vec<&str> = text.split('-').collect();
    let lengths_match = parts.len() == 5
        && parts.iter().map(|part| part.len()).eq([8, 4, 4, 4, 12])
        && parts.iter().all(|part| part.bytes().all(|b| b.is_ascii_hexdigit()));
    if !lengths_match {
        return None;
    }
    let data1 = u32::from_str_radix(parts[0], 16).ok()?;
    let data2 = u16::from_str_radix(parts[1], 16).ok()?;
    let data3 = u16::from_str_radix(parts[2], 16).ok()?;
    let tail = format!("{}{}", parts[3], parts[4]);
    let mut data4 = [0u8; 8];
    for (index, byte) in data4.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&tail[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some((data1, data2, data3, data4))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry(key: &str, data: RegData) -> Action {
        Action::Registry(RegistryAction {
            scope: Scope::Machine,
            key: key.to_owned(),
            name: "Value".to_owned(),
            data,
        })
    }

    fn command(program: &str) -> CommandAction {
        CommandAction {
            apply: CommandLine { program: program.to_owned(), args: vec![] },
            run_as: RunAs::System,
            timeout_secs: 30,
            check: None,
            undo: None,
        }
    }

    #[test]
    fn well_formed_actions_validate() {
        registry(r"SOFTWARE\SysPilot-Test", RegData::Dword(1)).validate().unwrap();
        Action::Service(ServiceAction { service: "DiagTrack".into(), start: StartType::Disabled })
            .validate()
            .unwrap();
        Action::Power(PowerAction {
            subgroup: "54533251-82be-4824-96c1-47b60b740d00".into(),
            setting: "{0cc5b647-c1df-4637-891a-dec35c318583}".into(),
            ac: Some(100),
            dc: None,
        })
        .validate()
        .unwrap();
        Action::Command(command(r"%SystemRoot%\System32\ipconfig.exe")).validate().unwrap();
        Action::Command(command(r"C:\Tools\tool.exe")).validate().unwrap();
    }

    #[test]
    fn registry_key_with_hive_prefix_is_rejected() {
        for key in [r"HKLM\SOFTWARE\X", r"HKEY_CURRENT_USER\Software\X", r"hkcu\Software"] {
            let error = registry(key, RegData::Dword(1)).validate().unwrap_err();
            assert_eq!(error.field, "registry.key", "{key}");
            assert_eq!(error.received, key);
        }
    }

    #[test]
    fn malformed_registry_keys_are_rejected() {
        for key in ["", r"\SOFTWARE", r"SOFTWARE\", r"SOFTWARE\\X"] {
            let error = registry(key, RegData::Dword(1)).validate().unwrap_err();
            assert_eq!(error.field, "registry.key", "{key:?}");
        }
    }

    #[test]
    fn multi_sz_cannot_hold_an_empty_element() {
        let error = registry(r"SOFTWARE\X", RegData::MultiSz(vec!["a".into(), String::new()]))
            .validate()
            .unwrap_err();
        assert_eq!(error.field, "registry.data");
    }

    #[test]
    fn power_action_needs_valid_guids_and_a_value() {
        let mut action = PowerAction {
            subgroup: "not-a-guid".into(),
            setting: "0cc5b647-c1df-4637-891a-dec35c318583".into(),
            ac: Some(1),
            dc: None,
        };
        assert_eq!(action.validate().unwrap_err().field, "power.subgroup");
        action.subgroup = "54533251-82be-4824-96c1-47b60b740d00".into();
        action.ac = None;
        assert_eq!(action.validate().unwrap_err().field, "power");
    }

    #[test]
    fn command_program_must_be_absolute() {
        for program in ["cmd.exe", r"tools\x.exe", r"\\server\share\x.exe", "%SystemRoot%", ""] {
            let error = Action::Command(command(program)).validate().unwrap_err();
            assert_eq!(error.field, "command.apply.program", "{program:?}");
            assert_eq!(error.received, program);
        }
    }

    #[test]
    fn command_timeout_is_bounded() {
        for timeout_secs in [0, MAX_COMMAND_TIMEOUT_SECS + 1] {
            let action = CommandAction { timeout_secs, ..command(r"C:\x.exe") };
            let error = Action::Command(action).validate().unwrap_err();
            assert_eq!(error.field, "command.timeout_secs");
            assert_eq!(error.received, timeout_secs.to_string());
        }
    }

    #[test]
    fn triggers_reject_fast_intervals_and_duplicates() {
        validate_triggers(&[Trigger::Boot, Trigger::Change, Trigger::Interval(60)]).unwrap();
        let error = validate_triggers(&[Trigger::Interval(59)]).unwrap_err();
        assert_eq!(error.received, "59");
        let error = validate_triggers(&[Trigger::Boot, Trigger::Boot]).unwrap_err();
        assert_eq!(error.expected, "each trigger kind at most once");
    }

    #[test]
    fn default_triggers_follow_the_scope() {
        assert_eq!(
            registry(r"SOFTWARE\X", RegData::Dword(1)).default_triggers(),
            [Trigger::Boot, Trigger::Change]
        );
        let per_user = Action::Registry(RegistryAction {
            scope: Scope::User,
            key: r"Software\X".into(),
            name: String::new(),
            data: RegData::Absent,
        });
        assert!(per_user.is_per_user());
        assert_eq!(per_user.default_triggers(), [Trigger::Logon, Trigger::Change]);
    }

    #[test]
    fn command_is_reversible_only_with_an_undo() {
        let mut action = command(r"C:\x.exe");
        assert!(!Action::Command(action.clone()).is_reversible());
        action.undo = Some(CommandLine { program: r"C:\undo.exe".into(), args: vec![] });
        assert!(Action::Command(action).is_reversible());
    }

    #[test]
    fn guid_parses_into_win32_field_layout() {
        let guid = parse_guid("54533251-82be-4824-96c1-47b60b740d00").unwrap();
        assert_eq!(
            guid,
            (0x5453_3251, 0x82be, 0x4824, [0x96, 0xc1, 0x47, 0xb6, 0x0b, 0x74, 0x0d, 0x00])
        );
        assert_eq!(parse_guid("{54533251-82be-4824-96c1-47b60b740d00}"), Some(guid));
        assert_eq!(parse_guid("54533251-82be-4824-96c1-47b60b740d0"), None);
        assert_eq!(parse_guid("5453325g-82be-4824-96c1-47b60b740d00"), None);
    }

    #[test]
    fn describe_shows_the_exact_target() {
        let action = registry(r"SOFTWARE\Policies\X", RegData::Dword(0));
        assert_eq!(action.describe(), r"HKLM\SOFTWARE\Policies\X\Value = dword:0");
    }

    #[test]
    fn actions_survive_both_encodings() {
        let action = Action::Command(CommandAction {
            check: Some(CommandCheck { command: command(r"C:\c.exe").apply, expect_exit: 3 }),
            ..command(r"C:\x.exe")
        });
        let wire = postcard::to_stdvec(&action).unwrap();
        assert_eq!(postcard::from_bytes::<Action>(&wire).unwrap(), action);

        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Document {
            action: Action,
        }
        let document = Document { action };
        let text = toml::to_string(&document).unwrap();
        assert_eq!(toml::from_str::<Document>(&text).unwrap(), document);
    }
}
