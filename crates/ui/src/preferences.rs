//! Per-user choices about the app itself: level and theme.
//!
//! They live in a small TOML file under the user's local application data.
//! Nothing in it affects the system, so the app owns it without the service.

use std::path::{Path, PathBuf};
use std::{fs, io};

use serde::{Deserialize, Serialize};

use crate::model::ViewLevel;

/// File name inside the app's local data directory.
const FILE_NAME: &str = "app.toml";

/// Which colours the app uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Theme {
    /// Whatever Windows is set to.
    #[default]
    System,
    /// Light.
    Light,
    /// Dark.
    Dark,
}

impl Theme {
    /// Every theme, in display order.
    pub const ALL: [Self; 3] = [Self::System, Self::Light, Self::Dark];

    /// Display name.
    pub fn name(self) -> &'static str {
        match self {
            Self::System => "Use Windows setting",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }
}

/// The user's choices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct Preferences {
    /// How much of the product is shown.
    pub level: ViewLevel,
    /// Whether the user has read the note shown before the Advanced level
    /// is used for the first time.
    pub advanced_acknowledged: bool,
    /// Colours.
    pub theme: Theme,
}

impl Default for Preferences {
    fn default() -> Self {
        Self { level: ViewLevel::Simple, advanced_acknowledged: false, theme: Theme::System }
    }
}

/// Where the preferences of `instance_name` are stored, given the value of
/// `%LOCALAPPDATA%`.
pub fn file_in(local_app_data: &Path, instance_name: &str) -> PathBuf {
    local_app_data.join(instance_name).join(FILE_NAME)
}

/// Reads the preferences. A missing or unreadable file gives the defaults:
/// losing them costs the user two clicks, so it is not worth an error.
pub fn load(file: &Path) -> Preferences {
    fs::read_to_string(file).ok().and_then(|text| toml::from_str(&text).ok()).unwrap_or_default()
}

/// Writes the preferences, creating the directory if needed.
pub fn save(file: &Path, preferences: &Preferences) -> io::Result<()> {
    if let Some(directory) = file.parent() {
        fs::create_dir_all(directory)?;
    }
    let text = toml::to_string(preferences).map_err(io::Error::other)?;
    fs::write(file, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory under the system temp directory, removed on drop.
    struct ScratchDirectory(PathBuf);

    impl ScratchDirectory {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("SysPilot-Test.preferences-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            Self(path)
        }
    }

    impl Drop for ScratchDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn preferences_survive_a_save_and_load() {
        let scratch = ScratchDirectory::new("round-trip");
        let file = file_in(&scratch.0, "SysPilot-Test.ui");
        assert!(file.ends_with(r"SysPilot-Test.ui\app.toml"), "{file:?}");

        let chosen = Preferences {
            level: ViewLevel::Advanced,
            advanced_acknowledged: true,
            theme: Theme::Dark,
        };
        save(&file, &chosen).unwrap();
        assert_eq!(load(&file), chosen);
        let text = fs::read_to_string(&file).unwrap();
        assert!(text.contains("level = \"advanced\""), "{text}");
    }

    #[test]
    fn missing_or_damaged_file_gives_the_defaults() {
        let scratch = ScratchDirectory::new("damaged");
        let file = file_in(&scratch.0, "SysPilot-Test.ui");
        assert_eq!(load(&file), Preferences::default());

        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "level = 7\n").unwrap();
        assert_eq!(load(&file), Preferences::default());
        assert_eq!(Preferences::default().level, ViewLevel::Simple);
    }

    #[test]
    fn unknown_keys_from_a_newer_version_are_ignored() {
        let scratch = ScratchDirectory::new("newer");
        let file = file_in(&scratch.0, "SysPilot-Test.ui");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "theme = \"light\"\nsomething-new = true\n").unwrap();
        assert_eq!(load(&file).theme, Theme::Light);
    }
}
