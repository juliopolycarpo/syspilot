//! Named fakes for the worker's fan control: a validated board whose NCT6779D
//! is replayed from a fixture, and nothing that touches a real driver.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use syspilot_hw::bus::replay::{ReplaySuperIo, SuperIoWrite, superio_hwm_key};
use syspilot_hw::bus::{FakeBusLocks, Fixture, SuperIoSlot};
use syspilot_hw::chips::Detected;
use syspilot_hw::chips::nct67xx::Chip;
use syspilot_hw::fan::board::BoardCatalog;
use syspilot_hw::fan::failsafe::{ControlSnapshot, OriginalStore, StoreError};
use syspilot_hw::fan::session::FanSession;
use syspilot_proto::profile::{CurvePoint, FanCurve};

use crate::fans::{Access, FanAccess};

/// The BIOS' mode register on the replayed chip: SmartFan IV.
pub const BIOS_MODE: u8 = 0x45;
/// The BIOS' duty on the replayed chip.
pub const BIOS_DUTY: u8 = 0x66;
/// The sensor the test curves read.
pub const CPU: &str = "cpu.package.temp";

const BOARD: &str = r#"
[[board]]
manufacturer = "ACME"
product = "TEST BOARD"
chip = "NCT6779D"
validated = true
validated-by = "A. Tester"
validated-on = "2026-12-01"

[[board.channel]]
pwm = 1
tach = 1
label = "CPU fan"
min-duty = 20
may-stop = false
"#;

/// An original store that keeps nothing between sessions.
#[derive(Default)]
struct ForgetfulStore;

impl OriginalStore for ForgetfulStore {
    fn load(&mut self, _channel: &str) -> Result<Option<ControlSnapshot>, StoreError> {
        Ok(None)
    }
    fn save(&mut self, _channel: &str, _original: &ControlSnapshot) -> Result<(), StoreError> {
        Ok(())
    }
    fn clear(&mut self, _channel: &str) -> Result<(), StoreError> {
        Ok(())
    }
}

/// The writes one replayed chip received.
type WriteLog = Arc<Mutex<Vec<SuperIoWrite>>>;

/// What a test looks at after the fact.
#[derive(Clone, Default)]
pub struct ReplayHandles {
    logs: Arc<Mutex<Vec<WriteLog>>>,
    directories: Arc<Mutex<Vec<String>>>,
}

impl ReplayHandles {
    /// The fan registers written so far, over every session, as
    /// `(address, value)`.
    pub fn fan_writes(&self) -> Vec<(u16, u8)> {
        let logs = locked(&self.logs);
        logs.iter()
            .flat_map(|log| locked(log).clone())
            .filter_map(|write| match write {
                SuperIoWrite::HwmFanRegister { address, value, .. } => Some((address, value)),
                _ => None,
            })
            .collect()
    }

    /// The store directories the plans named, in order.
    pub fn directories(&self) -> Vec<String> {
        locked(&self.directories).clone()
    }
}

/// The guard of a fake's mutex; a poisoned one still holds what was logged.
fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Opens a session on a fresh replayed chip each time, or refuses with a
/// fixed reason.
struct ReplayAccess {
    handles: ReplayHandles,
    refuse: Option<String>,
}

impl FanAccess for ReplayAccess {
    fn open(&mut self, curves: &[FanCurve], store_directory: &Path) -> Result<FanSession, String> {
        locked(&self.handles.directories).push(store_directory.display().to_string());
        if let Some(refusal) = &self.refuse {
            return Err(refusal.clone());
        }
        let mut fixture = Fixture::new();
        for (address, value) in [(0x102u16, BIOS_MODE), (0x109, BIOS_DUTY), (0x001, BIOS_DUTY)] {
            fixture.set(
                superio_hwm_key(SuperIoSlot::Primary, (address >> 8) as u8, address as u8),
                u64::from(value),
            );
        }
        let io = ReplaySuperIo::new(fixture);
        locked(&self.handles.logs).push(io.write_log());
        let catalog = BoardCatalog::parse("test", BOARD).map_err(|error| error.to_string())?;
        let grant = catalog
            .offer("ACME", "TEST BOARD", Chip::Nct6779D)
            .map_err(|reason| reason.to_string())?;
        let detected = Detected::Nuvoton {
            slot: SuperIoSlot::Primary,
            chip: Chip::Nct6779D,
            id_and_revision: 0xC562,
            base: 0x0A20,
            lock_register: 0,
        };
        FanSession::start(
            &grant,
            &detected,
            curves,
            Box::new(io),
            Arc::new(FakeBusLocks::free()),
            Box::new(ForgetfulStore),
        )
        .map_err(|error| error.to_string())
    }
}

/// An access on the replayed validated board, and the handles to inspect it.
pub fn replay_access() -> (Access, ReplayHandles) {
    let handles = ReplayHandles::default();
    (Box::new(ReplayAccess { handles: handles.clone(), refuse: None }), handles)
}

/// An access that always refuses with `reason`, as a busy bus would.
pub fn refusing_access(reason: &str) -> (Access, ReplayHandles) {
    let handles = ReplayHandles::default();
    let access = ReplayAccess { handles: handles.clone(), refuse: Some(reason.to_owned()) };
    (Box::new(access), handles)
}

/// A three-point curve for `board.pwm1` driven by [`CPU`]: 20 % at 30 degrees,
/// 60 % at 60, 100 % at 80.
pub fn curve(enabled: bool) -> FanCurve {
    FanCurve {
        channel: "board.pwm1".into(),
        enabled,
        sources: vec![CPU.into()],
        points: vec![CurvePoint(30, 20), CurvePoint(60, 60), CurvePoint(80, 100)],
        hysteresis_c: 0,
        min_duty_pct: 0,
        ramp_pct_per_sec: 100,
    }
}
