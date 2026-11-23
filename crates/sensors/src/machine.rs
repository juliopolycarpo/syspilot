//! What the sensor worker does on the real machine.

use std::path::PathBuf;

use syspilot_proto::message::DriverState;

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

/// The driver state as far as it can be told without opening the driver.
fn installed_driver_state() -> DriverState {
    match syspilot_pawnio::installed_version() {
        Some(version) => DriverState::Ready { version },
        None => DriverState::Missing,
    }
}

/// Looks at the machine: Windows' own sensors, the inventory, and what the
/// PawnIO driver adds when it is installed.
pub fn probe() -> Machine {
    let _ = modules_directory();
    Machine {
        driver: installed_driver_state(),
        features: Vec::new(),
        system: syspilot_hw::sysinfo::collect(),
        providers: syspilot_hw::os::probe(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
