//! What the service and its updater exchange.
//!
//! The service never talks to the network and never runs an installer. When
//! it is time to look for a new version, or to install one, it starts
//! `syspilot-update.exe` from its own directory with one command and reads
//! one line from its standard output. Both always come from the same build,
//! so this carries no version.
//!
//! | Command               | Line on success                         |
//! | --------------------- | --------------------------------------- |
//! | `check`               | `current`, or `available <version>`     |
//! | `install <directory>` | `installing <version>`                  |
//!
//! A failure is a non-zero exit code and a sentence on standard error.

/// File name of the updater executable, next to the service's.
pub const UPDATER_EXECUTABLE: &str = "syspilot-update.exe";
/// The updater's command that looks for a new version.
pub const CHECK_COMMAND: &str = "check";
/// The updater's command that downloads, verifies and installs.
pub const INSTALL_COMMAND: &str = "install";

/// Longest version text accepted from the updater.
const VERSION_MAX: usize = 32;

/// What the updater found or did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdaterReport {
    /// The newest published version is not newer than this one.
    Current,
    /// A newer version is published and signed.
    Available(String),
    /// The new version was verified and Windows Installer has been started.
    Installing(String),
}

/// Whether `text` looks like a version: digits and dots, e.g. `0.2.1`. The
/// service shows it to the user, so nothing else may come through.
fn is_version(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= VERSION_MAX
        && text.split('.').all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

impl UpdaterReport {
    /// The line the updater prints.
    pub fn to_line(&self) -> String {
        match self {
            Self::Current => "current".to_owned(),
            Self::Available(version) => format!("available {version}"),
            Self::Installing(version) => format!("installing {version}"),
        }
    }

    /// Reads the updater's output. `None` for anything that is not exactly
    /// one of the lines above.
    pub fn parse_line(output: &str) -> Option<Self> {
        let mut words = output.trim().split(' ');
        let report = match (words.next()?, words.next()) {
            ("current", None) => Self::Current,
            ("available", Some(version)) if is_version(version) => {
                Self::Available(version.to_owned())
            }
            ("installing", Some(version)) if is_version(version) => {
                Self::Installing(version.to_owned())
            }
            _ => return None,
        };
        words.next().is_none().then_some(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_report_survives_its_line() {
        let reports = [
            UpdaterReport::Current,
            UpdaterReport::Available("0.2.1".into()),
            UpdaterReport::Installing("12.0.345".into()),
        ];
        for report in reports {
            let line = report.to_line();
            assert_eq!(UpdaterReport::parse_line(&format!("{line}\r\n")), Some(report), "{line}");
        }
    }

    #[test]
    fn anything_but_one_known_line_is_refused() {
        for output in [
            "",
            "current 0.2.1",
            "available",
            "available v0.2.1",
            "available 0.2.1 now",
            "available 0..1",
            "available 0.2.1\navailable 0.2.2",
            "installing <script>",
            "Available 0.2.1",
        ] {
            assert_eq!(
                UpdaterReport::parse_line(output),
                None,
                "(updater output): expected {output:?} to be refused"
            );
        }
        let long = format!("available {}", "1".repeat(VERSION_MAX + 1));
        assert_eq!(UpdaterReport::parse_line(&long), None);
    }
}
