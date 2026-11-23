//! Tweak catalog schema.
//!
//! A tweak is a documented, reversible setting made of one or more
//! [`Action`]s. The catalog is written in TOML (`catalog/tweaks/*.toml`),
//! validated by [`Catalog::parse`], and sent to the UI over IPC.

use std::collections::HashSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::action::{Action, InvalidAction, Trigger, validate_triggers};

/// Which UI level an entry is shown at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Level {
    /// Safe, reversible options in plain language.
    Simple,
    /// Everything, with full technical detail.
    Advanced,
}

/// What an entry is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Category {
    /// Responsiveness, latency and throughput.
    Performance,
    /// Data sent to Microsoft or exposed to apps.
    Privacy,
    /// Attack surface and exploit mitigations.
    Security,
    /// Stability, recoverability and update behaviour.
    Reliability,
    /// Interface and quality-of-life adjustments.
    Other,
}

impl Category {
    /// Every category, in display order.
    pub const ALL: [Self; 5] =
        [Self::Performance, Self::Privacy, Self::Security, Self::Reliability, Self::Other];
}

/// Effect of an entry on security or reliability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Direction {
    /// Makes the system safer or more robust.
    Hardens,
    /// Trades safety or robustness for something else.
    Weakens,
}

/// What must happen before a change is fully in effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Reboot {
    /// Effective immediately.
    #[default]
    None,
    /// Effective after signing out and in again.
    SignOut,
    /// Effective after a restart.
    Restart,
}

/// One catalog entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Tweak {
    /// Stable identifier, e.g. `privacy.advertising-id`.
    pub id: String,
    /// Short name in sentence case.
    pub title: String,
    /// Plain-language text: what the user will notice. Shown at both levels.
    pub summary: String,
    /// Technical explanation of the mechanism. Shown at the Advanced level.
    #[serde(default)]
    pub detail: String,
    /// Level the entry is shown at.
    pub level: Level,
    /// Category the entry belongs to.
    pub category: Category,
    /// Whether the entry hardens or weakens the system. Mandatory for
    /// security entries.
    pub direction: Option<Direction>,
    /// The risk in plain words. Mandatory for entries that weaken.
    pub risk: Option<String>,
    /// Known side effects, empty when there are none.
    #[serde(default)]
    pub side_effects: String,
    /// What is needed before the change takes effect.
    #[serde(default)]
    pub reboot: Reboot,
    /// Primary source documenting the exact setting and its semantics.
    pub source: String,
    /// First Windows build the entry applies to.
    pub min_build: Option<u32>,
    /// Detect-only entry: reported, never applied. Used for facts such as
    /// Secure Boot that Windows does not let software change.
    #[serde(default)]
    pub audit: bool,
    /// The settings that make up the tweak.
    #[serde(rename = "action")]
    pub actions: Vec<Action>,
    /// When the tweak is re-evaluated. Empty means the defaults for its
    /// actions.
    #[serde(default)]
    pub triggers: Vec<Trigger>,
}

/// One catalog file.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Catalog {
    /// Entries in display order.
    #[serde(rename = "tweak", default)]
    pub tweaks: Vec<Tweak>,
}

/// Why a catalog was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogError {
    /// The file is not valid TOML or does not match the schema.
    Syntax(String),
    /// An entry breaks a catalog rule.
    Entry {
        /// ID of the offending entry (may itself be the invalid part).
        id: String,
        /// The broken rule.
        problem: EntryProblem,
    },
}

/// A catalog rule broken by one entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryProblem {
    /// The ID is malformed.
    InvalidId,
    /// Another entry already uses the ID.
    DuplicateId,
    /// A mandatory text is empty.
    MissingText(&'static str),
    /// The source is not an `https://` URL.
    InvalidSource,
    /// The entry has no actions.
    NoActions,
    /// One of the actions is malformed.
    Action(InvalidAction),
    /// A security entry does not say whether it hardens or weakens.
    SecurityWithoutDirection,
    /// An entry weakens the system without describing the risk.
    WeakensWithoutRisk,
    /// A Simple-level entry weakens security or reliability.
    SimpleWeakens,
    /// A Simple-level entry cannot be reverted exactly.
    SimpleNotReversible,
}

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax(message) => write!(f, "(catalog): {message}"),
            Self::Entry { id, problem } => write!(f, "(catalog entry {id:?}): {problem}"),
        }
    }
}

impl fmt::Display for EntryProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidId => {
                f.write_str("expected an id of dot-separated [a-z0-9-] words, like privacy.ad-id")
            }
            Self::DuplicateId => f.write_str("expected a unique id | received: a duplicate"),
            Self::MissingText(field) => write!(f, "expected non-empty {field} | received: \"\""),
            Self::InvalidSource => f.write_str("expected source to be an https:// URL"),
            Self::NoActions => f.write_str("expected at least one action | received: none"),
            Self::Action(error) => error.fmt(f),
            Self::SecurityWithoutDirection => {
                f.write_str("expected direction (hardens | weakens) on a security entry")
            }
            Self::WeakensWithoutRisk => {
                f.write_str("expected a risk text on an entry that weakens the system")
            }
            Self::SimpleWeakens => {
                f.write_str("expected level advanced for an entry that weakens the system")
            }
            Self::SimpleNotReversible => {
                f.write_str("expected level advanced for an entry that cannot be reverted")
            }
        }
    }
}

impl std::error::Error for CatalogError {}

impl Tweak {
    /// Whether every action can be reverted to its original state.
    pub fn is_reversible(&self) -> bool {
        self.actions.iter().all(Action::is_reversible)
    }

    /// Whether the entry trades security or reliability for something else.
    pub fn weakens(&self) -> bool {
        self.direction == Some(Direction::Weakens)
    }

    /// Triggers that apply to the action at `index`: the declared ones, or
    /// the defaults for that action.
    pub fn triggers_for(&self, index: usize) -> Vec<Trigger> {
        if self.triggers.is_empty() {
            self.actions.get(index).map(Action::default_triggers).unwrap_or_default()
        } else {
            self.triggers.clone()
        }
    }

    fn validate(&self) -> Result<(), EntryProblem> {
        let id_ok = !self.id.is_empty()
            && self.id.len() <= 80
            && self.id.contains('.')
            && self.id.split('.').all(|word| {
                !word.is_empty()
                    && word
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            });
        if !id_ok {
            return Err(EntryProblem::InvalidId);
        }
        for (field, text) in [("title", &self.title), ("summary", &self.summary)] {
            if text.trim().is_empty() {
                return Err(EntryProblem::MissingText(field));
            }
        }
        if !self.source.starts_with("https://") || self.source.contains(char::is_whitespace) {
            return Err(EntryProblem::InvalidSource);
        }
        if self.actions.is_empty() {
            return Err(EntryProblem::NoActions);
        }
        for action in &self.actions {
            action.validate().map_err(EntryProblem::Action)?;
        }
        validate_triggers(&self.triggers).map_err(EntryProblem::Action)?;
        if self.category == Category::Security && self.direction.is_none() {
            return Err(EntryProblem::SecurityWithoutDirection);
        }
        if self.weakens() && self.risk.as_deref().is_none_or(|risk| risk.trim().is_empty()) {
            return Err(EntryProblem::WeakensWithoutRisk);
        }
        if self.level == Level::Simple {
            if self.weakens() {
                return Err(EntryProblem::SimpleWeakens);
            }
            if !self.is_reversible() {
                return Err(EntryProblem::SimpleNotReversible);
            }
        }
        Ok(())
    }
}

impl Catalog {
    /// Parses and validates one catalog file.
    pub fn parse(text: &str) -> Result<Self, CatalogError> {
        let catalog: Self =
            toml::from_str(text).map_err(|error| CatalogError::Syntax(error.to_string()))?;
        catalog.validate()?;
        Ok(catalog)
    }

    /// Parses several files into one catalog, rejecting IDs that repeat
    /// across files.
    pub fn parse_all<'a>(texts: impl IntoIterator<Item = &'a str>) -> Result<Self, CatalogError> {
        let mut merged = Self::default();
        for text in texts {
            merged.tweaks.extend(Self::parse(text)?.tweaks);
        }
        merged.validate()?;
        Ok(merged)
    }

    /// Checks every entry and that IDs are unique.
    pub fn validate(&self) -> Result<(), CatalogError> {
        let mut seen = HashSet::new();
        for tweak in &self.tweaks {
            let entry = |problem| CatalogError::Entry { id: tweak.id.clone(), problem };
            tweak.validate().map_err(entry)?;
            if !seen.insert(tweak.id.as_str()) {
                return Err(entry(EntryProblem::DuplicateId));
            }
        }
        Ok(())
    }

    /// Looks an entry up by ID.
    pub fn get(&self, id: &str) -> Option<&Tweak> {
        self.tweaks.iter().find(|tweak| tweak.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
[[tweak]]
id = "privacy.advertising-id"
title = "Turn off the advertising ID"
summary = "Apps can no longer use your advertising ID to personalise ads."
level = "simple"
category = "privacy"
source = "https://learn.microsoft.com/windows/privacy/manage-connections"

[[tweak.action]]
registry = { scope = "user", key = 'Software\Microsoft\Windows\CurrentVersion\AdvertisingInfo', name = "Enabled", data = { dword = 0 } }
"#;

    /// Returns the valid sample with one line replaced or appended, so each
    /// test changes exactly one property.
    fn with_change(from: &str, to: &str) -> String {
        assert!(VALID.contains(from), "fixture no longer contains {from:?}");
        VALID.replacen(from, to, 1)
    }

    fn problem_of(text: &str) -> EntryProblem {
        match Catalog::parse(text) {
            Err(CatalogError::Entry { problem, .. }) => problem,
            other => panic!("expected an entry problem | received: {other:?}"),
        }
    }

    #[test]
    fn valid_entry_parses_with_defaults() {
        let catalog = Catalog::parse(VALID).unwrap();
        let tweak = catalog.get("privacy.advertising-id").unwrap();
        assert_eq!(tweak.level, Level::Simple);
        assert_eq!(tweak.reboot, Reboot::None);
        assert!(tweak.is_reversible());
        assert_eq!(tweak.triggers_for(0), [Trigger::Logon, Trigger::Change]);
    }

    #[test]
    fn syntax_errors_are_reported() {
        let error = Catalog::parse("[[tweak]]\nid = 3").unwrap_err();
        assert!(matches!(error, CatalogError::Syntax(_)), "{error}");
    }

    #[test]
    fn simple_entry_that_weakens_is_rejected() {
        let text = with_change(
            "category = \"privacy\"",
            "category = \"security\"\ndirection = \"weakens\"\nrisk = \"Removes a mitigation.\"",
        );
        assert_eq!(problem_of(&text), EntryProblem::SimpleWeakens);
    }

    #[test]
    fn simple_entry_that_is_not_reversible_is_rejected() {
        let text = with_change(
            "registry = { scope = \"user\", key = 'Software\\Microsoft\\Windows\\CurrentVersion\\AdvertisingInfo', name = \"Enabled\", data = { dword = 0 } }",
            "command = { apply = { program = 'C:\\x.exe' }, run-as = \"system\", timeout-secs = 5 }",
        );
        assert_eq!(problem_of(&text), EntryProblem::SimpleNotReversible);
    }

    #[test]
    fn security_entry_needs_a_direction() {
        let text = with_change("category = \"privacy\"", "category = \"security\"");
        assert_eq!(problem_of(&text), EntryProblem::SecurityWithoutDirection);
    }

    #[test]
    fn weakening_entry_needs_a_risk_text() {
        let text = with_change(
            "level = \"simple\"",
            "level = \"advanced\"\ndirection = \"weakens\"\nrisk = \"  \"",
        );
        assert_eq!(problem_of(&text), EntryProblem::WeakensWithoutRisk);
    }

    #[test]
    fn source_must_be_https() {
        let text = with_change("https://learn", "http://learn");
        assert_eq!(problem_of(&text), EntryProblem::InvalidSource);
    }

    #[test]
    fn malformed_ids_are_rejected() {
        for id in ["Privacy.Ad", "privacy", "privacy..ad", "privacy.ad id"] {
            let text = with_change("privacy.advertising-id", id);
            assert_eq!(problem_of(&text), EntryProblem::InvalidId, "{id}");
        }
    }

    #[test]
    fn invalid_action_names_the_field() {
        let text = with_change("key = 'Software", "key = 'HKCU\\Software");
        match problem_of(&text) {
            EntryProblem::Action(error) => assert_eq!(error.field, "registry.key"),
            other => panic!("expected an action problem | received: {other:?}"),
        }
    }

    #[test]
    fn duplicate_ids_across_files_are_rejected() {
        let error = Catalog::parse_all([VALID, VALID]).unwrap_err();
        assert_eq!(
            error,
            CatalogError::Entry {
                id: "privacy.advertising-id".into(),
                problem: EntryProblem::DuplicateId
            }
        );
    }

    #[test]
    fn catalog_survives_the_wire_encoding() {
        let catalog = Catalog::parse(VALID).unwrap();
        let wire = postcard::to_stdvec(&catalog).unwrap();
        assert_eq!(postcard::from_bytes::<Catalog>(&wire).unwrap(), catalog);
    }
}
