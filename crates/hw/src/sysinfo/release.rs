//! The installed Windows release: name, version, build and architecture.

use windows_sys::Win32::System::SystemInformation::{
    GetNativeSystemInfo, PROCESSOR_ARCHITECTURE_AMD64, PROCESSOR_ARCHITECTURE_ARM,
    PROCESSOR_ARCHITECTURE_ARM64, PROCESSOR_ARCHITECTURE_INTEL, SYSTEM_INFO,
};

use crate::os::registry;

const CURRENT_VERSION_KEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
/// First build number of Windows 11. Its registry `ProductName` still says
/// "Windows 10", so the generation is read from the build.
const FIRST_WINDOWS_11_BUILD: u32 = 22_000;

/// What the registry and the OS say about the installed release.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct OperatingSystem {
    /// The registry `ProductName`, e.g. `Windows 10 Pro`.
    pub product_name: Option<String>,
    /// The feature update, e.g. `25H2`.
    pub display_version: Option<String>,
    pub build: Option<u32>,
    /// The cumulative-update revision that follows the build number.
    pub revision: Option<u32>,
    pub architecture: Option<&'static str>,
}

/// The product name with the generation corrected: builds from 22000 on are
/// Windows 11 even though the registry calls them "Windows 10 ...".
pub(crate) fn windows_name(product_name: &str, build: Option<u32>) -> String {
    let is_windows_11 = build.is_some_and(|build| build >= FIRST_WINDOWS_11_BUILD);
    match product_name.strip_prefix("Windows 10") {
        Some(edition) if is_windows_11 => format!("Windows 11{edition}"),
        _ => product_name.to_owned(),
    }
}

/// `26300.10075`, or just `26300` when the revision is unknown.
pub(crate) fn build_text(build: u32, revision: Option<u32>) -> String {
    match revision {
        Some(revision) => format!("{build}.{revision}"),
        None => build.to_string(),
    }
}

/// `PROCESSOR_ARCHITECTURE_*` values and their names. A table instead of a
/// `match`: the constants are not upper case, which patterns reject.
const ARCHITECTURE_NAMES: [(u16, &str); 4] = [
    (PROCESSOR_ARCHITECTURE_INTEL, "x86"),
    (PROCESSOR_ARCHITECTURE_AMD64, "x64"),
    (PROCESSOR_ARCHITECTURE_ARM, "ARM"),
    (PROCESSOR_ARCHITECTURE_ARM64, "ARM64"),
];

pub(crate) fn architecture_name(code: u16) -> Option<&'static str> {
    ARCHITECTURE_NAMES.iter().find(|&&(value, _)| value == code).map(|&(_, name)| name)
}

fn native_architecture() -> Option<&'static str> {
    let mut info = SYSTEM_INFO::default();
    // SAFETY: `info` is a valid, writable `SYSTEM_INFO`; the call cannot fail.
    unsafe { GetNativeSystemInfo(&mut info) };
    // SAFETY: the union is plain integers and was filled by the call above;
    // `wProcessorArchitecture` is its meaningful view on every release.
    architecture_name(unsafe { info.Anonymous.Anonymous.wProcessorArchitecture })
}

/// Reads the installed release.
pub(crate) fn read_operating_system() -> OperatingSystem {
    let text = |name| registry::read_string(CURRENT_VERSION_KEY, name);
    OperatingSystem {
        product_name: text("ProductName"),
        // `DisplayVersion` replaced `ReleaseId` in 20H2.
        display_version: text("DisplayVersion").or_else(|| text("ReleaseId")),
        build: text("CurrentBuildNumber").and_then(|build| build.parse().ok()),
        revision: registry::read_u32(CURRENT_VERSION_KEY, "UBR"),
        architecture: native_architecture(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_11_builds_are_renamed_from_the_registry_windows_10() {
        assert_eq!(windows_name("Windows 10 Pro", Some(26100)), "Windows 11 Pro");
        assert_eq!(
            windows_name("Windows 10 Home Single Language", Some(22000)),
            "Windows 11 Home Single Language"
        );
    }

    #[test]
    fn windows_10_builds_keep_their_name() {
        assert_eq!(windows_name("Windows 10 Pro", Some(19045)), "Windows 10 Pro");
        assert_eq!(windows_name("Windows 10 Pro", Some(21999)), "Windows 10 Pro");
    }

    #[test]
    fn name_is_kept_when_the_build_is_unknown_or_the_name_is_not_windows_10() {
        assert_eq!(windows_name("Windows 10 Pro", None), "Windows 10 Pro");
        assert_eq!(
            windows_name("Windows Server 2025 Standard", Some(26100)),
            "Windows Server 2025 Standard"
        );
        assert_eq!(windows_name("Windows 11 Pro", Some(26100)), "Windows 11 Pro");
    }

    #[test]
    fn build_text_appends_the_revision_when_known() {
        assert_eq!(build_text(26300, Some(10075)), "26300.10075");
        assert_eq!(build_text(26300, None), "26300");
    }

    #[test]
    fn architectures_have_names() {
        assert_eq!(architecture_name(PROCESSOR_ARCHITECTURE_AMD64), Some("x64"));
        assert_eq!(architecture_name(PROCESSOR_ARCHITECTURE_ARM64), Some("ARM64"));
        assert_eq!(architecture_name(PROCESSOR_ARCHITECTURE_INTEL), Some("x86"));
        assert_eq!(architecture_name(0xFFFF), None, "PROCESSOR_ARCHITECTURE_UNKNOWN");
    }

    #[test]
    fn this_machine_reports_a_windows_release() {
        let release = read_operating_system();
        let name = release.product_name.as_deref().unwrap_or_default();
        assert!(
            name.starts_with("Windows"),
            "ProductName: expected \"Windows ...\" | received: {name:?}"
        );
        assert!(
            release.build.is_some_and(|build| build >= 19_041),
            "build: expected >= 19041 (Windows 10 2004) | received: {:?}",
            release.build
        );
        assert!(
            release.architecture.is_some(),
            "architecture: expected a known name | received: None"
        );
    }
}
