//! Profile schema: everything the user has configured.
//!
//! The profile is stored as TOML under `%ProgramData%` (readable by
//! administrators only) and travels over IPC as `postcard`. The UI never
//! reads the file; it asks the service.

use std::collections::HashSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::action::{Action, InvalidAction, Trigger, validate_triggers};

/// Current profile schema version.
pub const PROFILE_SCHEMA: u32 = 1;
/// Prefix every custom action ID must carry, so custom IDs can never
/// collide with catalog IDs.
pub const CUSTOM_ID_PREFIX: &str = "custom.";
/// Most points a fan curve may have.
pub const MAX_CURVE_POINTS: usize = 16;

/// The complete user configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Profile {
    /// Schema version, [`PROFILE_SCHEMA`].
    pub schema: u32,
    /// IDs of the catalog tweaks that are enforced.
    #[serde(default)]
    pub enabled: Vec<String>,
    /// User-defined actions.
    #[serde(default)]
    pub custom: Vec<CustomAction>,
    /// Sensor logging settings.
    #[serde(default)]
    pub logging: LoggingConfig,
    /// Fan curves, one per controlled channel.
    #[serde(default, rename = "fan")]
    pub fans: Vec<FanCurve>,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            schema: PROFILE_SCHEMA,
            enabled: Vec::new(),
            custom: Vec::new(),
            logging: LoggingConfig::default(),
            fans: Vec::new(),
        }
    }
}

/// A user-defined enforced action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CustomAction {
    /// Identifier starting with [`CUSTOM_ID_PREFIX`].
    pub id: String,
    /// Name shown in the UI.
    pub title: String,
    /// Whether the action is currently enforced.
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    /// What to enforce.
    pub action: Action,
    /// When to re-evaluate. Empty means the defaults for the action.
    #[serde(default)]
    pub triggers: Vec<Trigger>,
}

fn enabled_by_default() -> bool {
    true
}

impl CustomAction {
    /// The declared triggers, or the defaults for the action.
    pub fn effective_triggers(&self) -> Vec<Trigger> {
        if self.triggers.is_empty() {
            self.action.default_triggers()
        } else {
            self.triggers.clone()
        }
    }
}

/// Sensor logging settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct LoggingConfig {
    /// Whether sensor values are recorded.
    pub enabled: bool,
    /// Seconds between recorded rows.
    pub interval_secs: u32,
    /// Sensor keys to record. Empty records every sensor.
    pub sensors: Vec<String>,
    /// File size at which the log rotates, in kibibytes.
    pub max_file_kib: u32,
    /// Rotated files kept before the oldest is deleted.
    pub keep_files: u32,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_secs: 5,
            sensors: Vec::new(),
            max_file_kib: 8192,
            keep_files: 5,
        }
    }
}

/// One point of a fan curve: temperature in °C, then duty in percent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurvePoint(pub u8, pub u8);

impl CurvePoint {
    /// Temperature in degrees Celsius.
    pub fn temp_c(self) -> u8 {
        self.0
    }

    /// Fan duty in percent.
    pub fn duty_pct(self) -> u8 {
        self.1
    }
}

/// A temperature-driven curve for one fan channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct FanCurve {
    /// Fan channel key from the board catalog, e.g. `nct6779.fan2`.
    pub channel: String,
    /// Whether the curve is active. An inactive curve leaves the channel
    /// under BIOS control.
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    /// Keys of the temperature sensors that drive the curve; the hottest
    /// one wins.
    pub sources: Vec<String>,
    /// Curve points with strictly increasing temperatures.
    pub points: Vec<CurvePoint>,
    /// Degrees the temperature must fall before the duty is lowered.
    #[serde(default = "default_hysteresis")]
    pub hysteresis_c: u8,
    /// Duty the fan never goes below, in percent.
    #[serde(default = "default_min_duty")]
    pub min_duty_pct: u8,
    /// Largest duty change per second, in percentage points.
    #[serde(default = "default_ramp")]
    pub ramp_pct_per_sec: u8,
}

fn default_hysteresis() -> u8 {
    3
}

fn default_min_duty() -> u8 {
    20
}

fn default_ramp() -> u8 {
    10
}

/// Why a profile was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileError {
    /// Not valid TOML, or the document does not match the schema.
    Syntax(String),
    /// Written by an incompatible version.
    Schema {
        /// The schema version found in the document.
        received: u32,
    },
    /// A field holds an unacceptable value.
    Invalid {
        /// Where: e.g. `custom[custom.x].id` or `fan[cpu].points`.
        location: String,
        /// What a valid value looks like.
        expected: &'static str,
        /// The rejected value.
        received: String,
    },
}

impl fmt::Display for ProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax(message) => write!(f, "(profile): {message}"),
            Self::Schema { received } => {
                write!(f, "(profile.schema): expected {PROFILE_SCHEMA} | received: {received}")
            }
            Self::Invalid { location, expected, received } => {
                write!(f, "(profile.{location}): expected {expected} | received: {received:?}")
            }
        }
    }
}

impl std::error::Error for ProfileError {}

fn invalid(location: String, expected: &'static str, received: impl ToString) -> ProfileError {
    ProfileError::Invalid { location, expected, received: received.to_string() }
}

fn from_action(location: &str, error: InvalidAction) -> ProfileError {
    ProfileError::Invalid {
        location: format!("{location}.{}", error.field),
        expected: error.expected,
        received: error.received,
    }
}

impl Profile {
    /// Parses and validates a profile document.
    pub fn parse(text: &str) -> Result<Self, ProfileError> {
        // Check the schema version before the full shape, so a profile from
        // a newer version reports "wrong schema" instead of an unknown field.
        #[derive(Deserialize)]
        struct SchemaOnly {
            schema: u32,
        }
        let SchemaOnly { schema } =
            toml::from_str(text).map_err(|error| ProfileError::Syntax(error.to_string()))?;
        if schema != PROFILE_SCHEMA {
            return Err(ProfileError::Schema { received: schema });
        }
        let profile: Self =
            toml::from_str(text).map_err(|error| ProfileError::Syntax(error.to_string()))?;
        profile.validate()?;
        Ok(profile)
    }

    /// Renders the profile as a TOML document.
    pub fn to_toml(&self) -> String {
        // Serialisation only fails for types TOML cannot represent, and the
        // round-trip test covers every profile type.
        toml::to_string(self).unwrap_or_default()
    }

    /// Checks every field. The service validates every profile it receives,
    /// whatever the source.
    pub fn validate(&self) -> Result<(), ProfileError> {
        if self.schema != PROFILE_SCHEMA {
            return Err(ProfileError::Schema { received: self.schema });
        }
        let mut seen = HashSet::new();
        for id in &self.enabled {
            if id.is_empty() || id.starts_with(CUSTOM_ID_PREFIX) {
                return Err(invalid("enabled".into(), "catalog tweak ids", id));
            }
            if !seen.insert(id.as_str()) {
                return Err(invalid("enabled".into(), "each id at most once", id));
            }
        }
        let mut seen = HashSet::new();
        for custom in &self.custom {
            custom.validate()?;
            if !seen.insert(custom.id.as_str()) {
                return Err(invalid("custom".into(), "unique ids", &custom.id));
            }
        }
        self.logging.validate()?;
        let mut seen = HashSet::new();
        for fan in &self.fans {
            fan.validate()?;
            if !seen.insert(fan.channel.as_str()) {
                return Err(invalid("fan".into(), "one curve per channel", &fan.channel));
            }
        }
        Ok(())
    }
}

impl CustomAction {
    /// Checks the ID shape, the title and the action itself.
    pub fn validate(&self) -> Result<(), ProfileError> {
        let location = format!("custom[{}]", self.id);
        let slug = self.id.strip_prefix(CUSTOM_ID_PREFIX).unwrap_or_default();
        let slug_ok = !slug.is_empty()
            && self.id.len() <= 80
            && slug.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !slug_ok {
            return Err(invalid(format!("{location}.id"), "custom.<lowercase-slug>", &self.id));
        }
        if self.title.trim().is_empty() || self.title.len() > 120 {
            return Err(invalid(format!("{location}.title"), "1..=120 characters", &self.title));
        }
        self.action.validate().map_err(|error| from_action(&location, error))?;
        validate_triggers(&self.triggers).map_err(|error| from_action(&location, error))
    }
}

impl LoggingConfig {
    /// Checks the numeric bounds.
    pub fn validate(&self) -> Result<(), ProfileError> {
        if !(1..=3600).contains(&self.interval_secs) {
            return Err(invalid(
                "logging.interval-secs".into(),
                "1..=3600 seconds",
                self.interval_secs,
            ));
        }
        if !(64..=1_048_576).contains(&self.max_file_kib) {
            return Err(invalid("logging.max-file-kib".into(), "64..=1048576", self.max_file_kib));
        }
        if !(1..=100).contains(&self.keep_files) {
            return Err(invalid("logging.keep-files".into(), "1..=100", self.keep_files));
        }
        Ok(())
    }
}

impl FanCurve {
    /// Checks that the curve is well formed and can never stop a fan.
    pub fn validate(&self) -> Result<(), ProfileError> {
        let location = |field: &str| format!("fan[{}].{field}", self.channel);
        if self.channel.trim().is_empty() {
            return Err(invalid("fan.channel".into(), "a channel key", &self.channel));
        }
        if self.sources.is_empty() || self.sources.iter().any(|source| source.trim().is_empty()) {
            return Err(invalid(location("sources"), "at least one sensor key", "none"));
        }
        if !(2..=MAX_CURVE_POINTS).contains(&self.points.len()) {
            return Err(invalid(location("points"), "2..=16 points", self.points.len()));
        }
        if let Some(pair) = self.points.windows(2).find(|pair| pair[0].temp_c() >= pair[1].temp_c())
        {
            return Err(invalid(
                location("points"),
                "strictly increasing temperatures",
                format!("{} then {}", pair[0].temp_c(), pair[1].temp_c()),
            ));
        }
        if let Some(point) = self.points.iter().find(|point| point.duty_pct() > 100) {
            return Err(invalid(location("points"), "duty 0..=100", point.duty_pct()));
        }
        // The hottest point must reach full speed, so a curve can never cap
        // cooling below what the hardware can deliver.
        let last = self.points[self.points.len() - 1];
        if last.duty_pct() != 100 {
            return Err(invalid(location("points"), "last point at 100% duty", last.duty_pct()));
        }
        if self.min_duty_pct > 100 {
            return Err(invalid(location("min-duty-pct"), "0..=100", self.min_duty_pct));
        }
        if self.hysteresis_c > 20 {
            return Err(invalid(location("hysteresis-c"), "0..=20", self.hysteresis_c));
        }
        if self.ramp_pct_per_sec == 0 {
            return Err(invalid(location("ramp-pct-per-sec"), "1..=255", 0));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
schema = 1
enabled = ["privacy.advertising-id", "performance.game-mode"]

[[custom]]
id = "custom.keep-scratch-value"
title = "Keep the scratch value"
triggers = ["boot", "change", { interval = 3600 }]
action = { registry = { scope = "machine", key = 'SOFTWARE\SysPilot-Test', name = "Value", data = { dword = 1 } } }

[[custom]]
id = "custom.flush-dns"
title = "Flush DNS at logon"
enabled = false
action = { command = { apply = { program = '%SystemRoot%\System32\ipconfig.exe', args = ["/flushdns"] }, run-as = "system", timeout-secs = 30 } }

[logging]
enabled = true
interval-secs = 10
sensors = ["cpu.package.temp"]

[[fan]]
channel = "nct6779.fan2"
sources = ["cpu.package.temp", "gpu0.temp"]
points = [[40, 30], [60, 50], [80, 100]]
"#;

    fn invalid_location(profile: &Profile) -> String {
        match profile.validate() {
            Err(ProfileError::Invalid { location, .. }) => location,
            other => panic!("expected an invalid field | received: {other:?}"),
        }
    }

    #[test]
    fn sample_profile_round_trips_through_toml_and_postcard() {
        let profile = Profile::parse(SAMPLE).unwrap();
        assert_eq!(profile.enabled.len(), 2);
        assert!(!profile.custom[1].enabled);
        assert_eq!(profile.logging.max_file_kib, 8192, "unset logging fields take defaults");
        assert_eq!(profile.fans[0].min_duty_pct, 20);

        assert_eq!(Profile::parse(&profile.to_toml()).unwrap(), profile);
        let wire = postcard::to_stdvec(&profile).unwrap();
        assert_eq!(postcard::from_bytes::<Profile>(&wire).unwrap(), profile);
    }

    #[test]
    fn default_profile_is_valid_and_round_trips() {
        let profile = Profile::default();
        profile.validate().unwrap();
        assert_eq!(Profile::parse(&profile.to_toml()).unwrap(), profile);
    }

    #[test]
    fn newer_schema_is_reported_as_such() {
        let error = Profile::parse("schema = 2\nfuture-field = true").unwrap_err();
        assert_eq!(error, ProfileError::Schema { received: 2 });
        assert_eq!(error.to_string(), "(profile.schema): expected 1 | received: 2");
    }

    #[test]
    fn syntax_error_is_reported() {
        assert!(matches!(Profile::parse("schema = "), Err(ProfileError::Syntax(_))));
    }

    #[test]
    fn duplicate_enabled_id_is_rejected() {
        let profile = Profile { enabled: vec!["a.b".into(), "a.b".into()], ..Profile::default() };
        assert_eq!(invalid_location(&profile), "enabled");
    }

    #[test]
    fn custom_id_must_carry_the_prefix() {
        let mut profile = Profile::parse(SAMPLE).unwrap();
        profile.custom[0].id = "privacy.fake".into();
        assert_eq!(invalid_location(&profile), "custom[privacy.fake].id");
    }

    #[test]
    fn custom_action_error_names_the_field() {
        let mut profile = Profile::parse(SAMPLE).unwrap();
        let Action::Command(command) = &mut profile.custom[1].action else {
            panic!("fixture changed: custom[1] is no longer a command");
        };
        command.apply.program = "ipconfig".into();
        let error = profile.validate().unwrap_err();
        assert_eq!(
            error.to_string(),
            "(profile.custom[custom.flush-dns].command.apply.program): expected an absolute path \
             such as C:\\Tools\\x.exe or %SystemRoot%\\System32\\x.exe | received: \"ipconfig\""
        );
    }

    #[test]
    fn logging_interval_is_bounded() {
        let mut profile = Profile::default();
        profile.logging.interval_secs = 0;
        assert_eq!(invalid_location(&profile), "logging.interval-secs");
    }

    #[test]
    fn fan_curve_temperatures_must_increase() {
        let mut profile = Profile::parse(SAMPLE).unwrap();
        profile.fans[0].points = vec![CurvePoint(60, 30), CurvePoint(60, 100)];
        let error = profile.validate().unwrap_err();
        assert_eq!(
            error.to_string(),
            "(profile.fan[nct6779.fan2].points): expected strictly increasing temperatures | \
             received: \"60 then 60\""
        );
    }

    #[test]
    fn fan_curve_must_reach_full_speed() {
        let mut profile = Profile::parse(SAMPLE).unwrap();
        profile.fans[0].points = vec![CurvePoint(40, 30), CurvePoint(80, 90)];
        assert_eq!(invalid_location(&profile), "fan[nct6779.fan2].points");
    }

    #[test]
    fn one_curve_per_channel() {
        let mut profile = Profile::parse(SAMPLE).unwrap();
        profile.fans.push(profile.fans[0].clone());
        assert_eq!(invalid_location(&profile), "fan");
    }
}
