//! What the sensor worker does on the real machine.

use std::path::PathBuf;

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
    let driver = syspilot_hw::driver::probe(&modules_directory());
    let mut system = syspilot_hw::sysinfo::collect();
    system.sections.extend(driver.info);
    let mut providers = syspilot_hw::os::probe();
    providers.extend(driver.providers);
    Machine { driver: driver.state, features: driver.features, system, providers }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syspilot_proto::message::DriverState;

    #[test]
    fn modules_are_looked_for_beside_the_executable() {
        let directory = modules_directory();
        assert!(directory.ends_with(MODULES_DIRECTORY), "{directory:?}");
        let executable = std::env::current_exe().unwrap();
        assert_eq!(directory.parent(), executable.parent());
    }

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
        assert_eq!(keys, ["cpu-sensors", "board-sensors", "memory-sensors", "fan-control"]);
        assert!(
            !machine.features[3].available,
            "(machine probe): expected fan control to be unavailable in this version"
        );
    }
}
