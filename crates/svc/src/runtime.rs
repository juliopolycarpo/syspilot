//! Assembles the real service from its parts.

use std::sync::Arc;

use syspilot_engine::os::{Hive, Registry, reg_type};
use syspilot_engine::win::{self, WinRegistry};
use syspilot_proto::Instance;

use crate::catalog;
use crate::core::Core;
use crate::events::EventQueue;
use crate::host::{Host, HostParts};
use crate::logger::SensorLogger;
use crate::pipe;
use crate::privilege::is_process_elevated;
use crate::storage::{DataDir, FileStore};
use crate::worker::WorkerHardware;

/// What a command that needs administrator rights says without them.
pub const NEEDS_ELEVATION: &str =
    "This needs administrator rights. Run it from an elevated terminal.";

/// Reads the Windows build number, e.g. 26100. Zero when it cannot be read.
pub fn os_build() -> u32 {
    let key = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
    let Ok(Some(value)) = WinRegistry.read(&Hive::Machine, key, "CurrentBuildNumber") else {
        return 0;
    };
    if value.kind != reg_type::SZ {
        return 0;
    }
    let units: Vec<u16> = value
        .data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .take_while(|unit| *unit != 0)
        .collect();
    String::from_utf16_lossy(&units).trim().parse().unwrap_or(0)
}

/// Runs the service on the calling thread until it is told to stop.
/// `on_ready` is called once the service accepts requests.
pub fn serve(
    instance: &Instance,
    console: bool,
    queue: Arc<EventQueue>,
    on_ready: impl FnOnce(),
) -> Result<(), String> {
    if !is_process_elevated() {
        return Err(NEEDS_ELEVATION.into());
    }
    let data = DataDir::for_instance(instance);
    data.secure()?;
    // Taking the pipe name is also what makes this the only running copy,
    // so it happens before anything is applied.
    pipe::start(instance, queue.clone()).map_err(|error| match error.kind() {
        std::io::ErrorKind::PermissionDenied => {
            format!("{} is already running.", instance.name())
        }
        _ => format!("create the pipe {}: {error}", instance.pipe_name()),
    })?;

    let catalog = catalog::builtin().map_err(|error| format!("built-in catalog: {error}"))?;
    let build = os_build();
    let core = Core::new(win::os(), FileStore::new(data.clone()), catalog, build, console);
    let logging = core.profile().logging.clone();
    let parts = HostParts {
        instance: instance.clone(),
        console,
        os_build: build,
        hardware: Box::new(WorkerHardware::machine()),
        logger: SensorLogger::new(data.logs(), logging),
        log_dir: data.logs().display().to_string(),
        // The state directory is the one only SYSTEM and administrators can
        // write, which is what the saved BIOS registers need.
        fan_store_directory: data.state().display().to_string(),
    };
    let mut host = Host::new(core, queue, parts)
        .map_err(|error| format!("create the sensor section: {error}"))?;
    on_ready();
    host.run();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_build_is_a_plausible_windows_build() {
        let build = os_build();
        assert!(build >= 10240, "expected a Windows 10 or later build number | received: {build}");
    }
}
