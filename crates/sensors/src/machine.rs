//! What the sensor worker does on the real machine.

use std::path::{Path, PathBuf};

use syspilot_hw::chips::Detected;
use syspilot_hw::driver::FAN_CONTROL;
#[cfg(feature = "fan-control")]
use syspilot_hw::driver::SystemPlatform;
use syspilot_proto::message::{FanAvailability, FanStatus, Feature, SystemInfo};

use crate::worker::Machine;

/// Directory, next to the executable, that holds the PawnIO modules.
const MODULES_DIRECTORY: &str = "pawnio-modules";

/// Where the PawnIO modules are expected: beside the executable, where a
/// release puts them as separate files.
fn modules_directory() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|executable| {
            executable.parent().map(|directory| directory.join(MODULES_DIRECTORY))
        })
        .unwrap_or_else(|| PathBuf::from(MODULES_DIRECTORY))
}

/// Looks at the machine: Windows' own sensors, the inventory, and what the
/// PawnIO driver adds when it is installed. Windows' sensors come first,
/// so the readings every machine has lead the list.
pub fn probe() -> Machine {
    let modules = modules_directory();
    let mut driver = syspilot_hw::driver::probe(&modules);
    let mut system = syspilot_hw::sysinfo::collect();
    system.sections.append(&mut driver.info);
    let mut providers = syspilot_hw::os::probe();
    providers.append(&mut driver.providers);

    let fans = syspilot_hw::fan::layout::assess_shipped(
        &system,
        driver.super_io_chip.as_ref(),
        &driver.state,
        cfg!(feature = "fan-control"),
    );
    // The feature list carries the same answer in words for the features view.
    if let Some(feature) = driver.features.iter_mut().find(|feature| feature.key == FAN_CONTROL) {
        describe_fan_feature(feature, &fans);
    }
    let fan_access = fan_access(&modules, &system, &driver.super_io_chip, &fans);
    Machine { driver: driver.state, features: driver.features, system, providers, fans, fan_access }
}

/// Puts what the fan status decided into the "Fan control" feature row.
fn describe_fan_feature(feature: &mut Feature, fans: &FanStatus) {
    match &fans.availability {
        FanAvailability::Controllable => {
            feature.available = true;
            feature.detail = format!("Curves can run on {}", fans.board);
        }
        FanAvailability::NotInBuild => {
            feature.available = false;
            feature.detail =
                format!("{} is validated for fan control; this build leaves it out", fans.board);
        }
        FanAvailability::ReadOnly { reason } => {
            feature.available = false;
            feature.detail = format!("Fans are read-only: {reason}");
        }
    }
}

/// Without the feature there is nothing that could write, so no access.
#[cfg(not(feature = "fan-control"))]
fn fan_access(
    _modules: &Path,
    _system: &SystemInfo,
    _chip: &Option<Detected>,
    _fans: &FanStatus,
) -> Option<crate::fans::Access> {
    None
}

/// Access to the fan controller only for a board the status found
/// controllable, which means a validated catalog entry for the chip found.
#[cfg(feature = "fan-control")]
fn fan_access(
    modules: &Path,
    system: &SystemInfo,
    chip: &Option<Detected>,
    fans: &FanStatus,
) -> Option<crate::fans::Access> {
    if fans.availability != FanAvailability::Controllable {
        return None;
    }
    let (manufacturer, product) = syspilot_hw::fan::layout::board_identity(system)?;
    Some(Box::new(real::PlatformFanAccess {
        platform: SystemPlatform::new(modules),
        manufacturer,
        product,
        chip: chip.clone()?,
    }))
}

#[cfg(feature = "fan-control")]
mod real {
    use std::path::Path;

    use syspilot_hw::chips::Detected;
    use syspilot_hw::driver::{Platform, SystemPlatform};
    use syspilot_hw::fan::board::BoardCatalog;
    use syspilot_hw::fan::session::FanSession;
    use syspilot_hw::fan::store::FileOriginalStore;
    use syspilot_proto::profile::FanCurve;

    use crate::fans::FanAccess;

    /// Opens a fan session on the real machine. Every call asks the shipped
    /// catalog for a grant again, so nothing here can write on a board the
    /// catalog does not allow.
    pub struct PlatformFanAccess {
        pub platform: SystemPlatform,
        pub manufacturer: String,
        pub product: String,
        pub chip: Detected,
    }

    impl FanAccess for PlatformFanAccess {
        fn open(
            &mut self,
            curves: &[FanCurve],
            store_directory: &Path,
        ) -> Result<FanSession, String> {
            let catalog = BoardCatalog::shipped().map_err(|error| error.to_string())?;
            let Detected::Nuvoton { chip, .. } = &self.chip else {
                return Err(format!(
                    "(fan control): expected a Nuvoton chip | received: {}",
                    self.chip.describe()
                ));
            };
            let grant = catalog
                .offer(&self.manufacturer, &self.product, *chip)
                .map_err(|reason| reason.to_string())?;
            let io = self.platform.super_io().map_err(|error| error.plain())?;
            let store = FileOriginalStore::in_directory(store_directory)
                .map_err(|error| error.to_string())?;
            FanSession::start(
                &grant,
                &self.chip,
                curves,
                io,
                self.platform.locks(),
                Box::new(store),
            )
            .map_err(|error| error.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(not(feature = "fan-control"))]
    use syspilot_proto::message::DriverState;

    #[test]
    fn modules_are_looked_for_beside_the_executable() {
        let directory = modules_directory();
        assert!(directory.ends_with(MODULES_DIRECTORY), "{directory:?}");
        let executable = std::env::current_exe().unwrap();
        assert_eq!(directory.parent(), executable.parent());
    }

    fn fan_row() -> Feature {
        Feature {
            key: FAN_CONTROL.into(),
            title: "Fan control".into(),
            available: false,
            detail: String::new(),
        }
    }

    #[test]
    fn the_fan_feature_row_says_what_the_fan_status_decided() {
        let status = |availability| FanStatus {
            availability,
            board: "ACME B550".into(),
            channels: Vec::new(),
        };
        let mut row = fan_row();
        describe_fan_feature(&mut row, &status(FanAvailability::Controllable));
        assert!(row.available, "{row:?}");
        assert_eq!(row.detail, "Curves can run on ACME B550");

        describe_fan_feature(&mut row, &status(FanAvailability::NotInBuild));
        assert!(!row.available, "{row:?}");
        assert!(row.detail.contains("this build leaves it out"), "{}", row.detail);

        let reason = "the board's entry has not been validated".to_owned();
        describe_fan_feature(&mut row, &status(FanAvailability::ReadOnly { reason }));
        assert!(!row.available, "{row:?}");
        assert_eq!(row.detail, "Fans are read-only: the board's entry has not been validated");
    }

    // Probing opens the real driver. With the feature on, that is a build that
    // can write to fans, which no test here may run against this machine.
    #[cfg(not(feature = "fan-control"))]
    #[test]
    fn probing_this_machine_finds_windows_sensors_and_its_name() {
        let machine = probe();
        assert!(
            !machine.providers.is_empty(),
            "(machine probe): expected at least the processor and memory providers | received: none"
        );
        assert!(!machine.system.summary.os.is_empty(), "the Windows version is always known");
        assert_eq!(
            matches!(machine.driver, DriverState::Missing),
            syspilot_pawnio::installed_version().is_none()
        );
        let keys: Vec<&str> = machine.features.iter().map(|feature| feature.key.as_str()).collect();
        assert_eq!(
            keys,
            ["cpu-sensors", "board-sensors", "memory-sensors", "chipset-sensors", "fan-control"]
        );
        assert!(
            !machine.features[4].available,
            "(machine probe): expected fan control to be unavailable without the feature"
        );
        assert!(
            machine.fan_access.is_none(),
            "(machine probe): expected no fan access in a build without the feature"
        );
        assert!(
            !matches!(machine.fans.availability, FanAvailability::Controllable),
            "(machine probe): expected fans never controllable without the feature: {:?}",
            machine.fans.availability
        );
    }
}
