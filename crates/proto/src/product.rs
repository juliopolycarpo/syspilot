//! Product naming.
//!
//! The product name is defined here and in the workspace `Cargo.toml`
//! metadata, and nowhere else. Every OS-visible name (service, pipe,
//! shared-memory section, data directory) is derived from an [`Instance`],
//! so a rename is a mechanical change and tests can run a second copy under
//! a different name without colliding with an installed one.

use std::fmt;

/// Display name of the product.
pub const PRODUCT_NAME: &str = "SysPilot";

/// Version of the IPC protocol. Bumped on any incompatible change to the
/// message types or the sensor section layout.
pub const PROTOCOL_VERSION: u16 = 3;

/// File the app keeps its per-user choices in, inside a directory named
/// after the instance under the user's local application data. Defined here
/// because uninstalling, which is the service's job, removes it.
pub const APP_PREFERENCES_FILE: &str = "app.toml";

/// Longest accepted instance name.
const INSTANCE_NAME_MAX: usize = 48;

/// A named copy of the product on this machine.
///
/// The installed product uses [`Instance::product`]. Tests and development
/// builds use [`Instance::named`] so their service, pipe and data directory
/// never touch the installed copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    name: String,
}

/// An instance name that cannot be used for OS object names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidInstanceName(pub String);

impl fmt::Display for InvalidInstanceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "(instance): expected 1..={INSTANCE_NAME_MAX} characters of [A-Za-z0-9._-] | received: {:?}",
            self.0
        )
    }
}

impl std::error::Error for InvalidInstanceName {}

impl Instance {
    /// The installed product instance.
    pub fn product() -> Self {
        Self { name: PRODUCT_NAME.to_owned() }
    }

    /// A separately named instance, for tests and development.
    ///
    /// The name ends up in a service name, a pipe name and a directory name,
    /// so it is restricted to characters that are valid in all three.
    pub fn named(name: &str) -> Result<Self, InvalidInstanceName> {
        let valid = !name.is_empty()
            && name.len() <= INSTANCE_NAME_MAX
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
        if valid {
            Ok(Self { name: name.to_owned() })
        } else {
            Err(InvalidInstanceName(name.to_owned()))
        }
    }

    /// The instance name, also used as the Windows service name and as the
    /// directory name under `%ProgramData%`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether this is the installed product rather than a test copy.
    pub fn is_product(&self) -> bool {
        self.name == PRODUCT_NAME
    }

    /// Path of the request pipe.
    pub fn pipe_name(&self) -> String {
        format!(r"\\.\pipe\{}", self.name)
    }

    /// Name of the read-only shared-memory section holding sensor values.
    pub fn sensor_section_name(&self) -> String {
        format!(r"Global\{}.Sensors", self.name)
    }
}

impl fmt::Display for Instance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_instance_derives_every_os_name_from_the_product_name() {
        let instance = Instance::product();
        assert!(instance.is_product());
        assert_eq!(instance.name(), PRODUCT_NAME);
        assert_eq!(instance.pipe_name(), r"\\.\pipe\SysPilot");
        assert_eq!(instance.sensor_section_name(), r"Global\SysPilot.Sensors");
    }

    #[test]
    fn named_instance_is_distinct_from_the_product() {
        let instance = Instance::named("SysPilot-Test").unwrap();
        assert!(!instance.is_product());
        assert_eq!(instance.pipe_name(), r"\\.\pipe\SysPilot-Test");
    }

    #[test]
    fn names_that_would_escape_an_object_namespace_are_rejected() {
        for bad in ["", r"a\b", "a/b", "a b", "ünï", &"x".repeat(INSTANCE_NAME_MAX + 1)] {
            let error = Instance::named(bad).unwrap_err();
            assert_eq!(error, InvalidInstanceName(bad.to_owned()));
            assert!(error.to_string().contains("expected 1..=48 characters"), "{error}");
        }
    }
}
