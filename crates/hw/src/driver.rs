//! What the PawnIO driver gives this machine, decided in one place.
//!
//! [`probe`] checks the driver, then tries each driver-backed feature on its
//! own: processor sensors, motherboard sensors, memory module temperatures and
//! the chipset temperature. A feature that cannot work ends as an unavailable
//! [`Feature`] with the reason in plain words; it never stops another one.
//! Nothing here writes to hardware: the bus layer has no write beyond the
//! index writes a read needs (see [`crate::bus`]).
//!
//! The decisions run against the [`Platform`] trait, so the tests replay
//! recorded registers and need no driver. [`SystemPlatform`] is the real one.
//! Panics are not caught: release builds abort on panic, so nothing here may
//! panic, and the bus layer is written to return errors instead.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use syspilot_pawnio::{Error, Executor, INSTALL_URL, PawnIo, Version, status};
use syspilot_proto::message::{DriverState, Feature, InfoSection};

use crate::bus::pawn::{PawnMsr, PawnPchThermal, PawnSmbus, PawnSmn, PawnSuperIo};
use crate::bus::{BusKind, BusLocks, Msr, PchThermal, Smbus, Smn, SuperIo, SystemBusLocks};
use crate::chips::{Detected, SuperIoProvider, detect};
use crate::cpu::{
    AmdCpuProvider, Clock, CpuIdentity, CpuTopology, IntelCpuProvider, SystemClock, Vendor,
};
use crate::dimm::{DimmProvider, describe_controller};
use crate::hub::Provider;
use crate::pch::{PchThermalProvider, describe_identity};

/// Feature key of the processor sensors.
pub const CPU_SENSORS: &str = "cpu-sensors";
/// Feature key of the motherboard sensors.
pub const BOARD_SENSORS: &str = "board-sensors";
/// Feature key of the memory module sensors.
pub const MEMORY_SENSORS: &str = "memory-sensors";
/// Feature key of the chipset temperature.
pub const CHIPSET_SENSORS: &str = "chipset-sensors";
/// Feature key of fan control.
pub const FAN_CONTROL: &str = "fan-control";

/// A PawnIO module SysPilot loads, one per handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Module {
    /// Intel model-specific registers.
    IntelMsr,
    /// AMD Zen model-specific registers and the SMN.
    AmdFamily17,
    /// Super I/O chips.
    LpcIo,
    /// The Intel PCH SMBus controller.
    SmbusI801,
    /// The Intel PCH thermal sensor.
    IntelPchThermal,
}

impl Module {
    /// The release file name, e.g. `IntelMSR.bin`.
    pub fn file_name(self) -> &'static str {
        match self {
            Self::IntelMsr => "IntelMSR.bin",
            Self::AmdFamily17 => "AMDFamily17.bin",
            Self::LpcIo => "LpcIO.bin",
            Self::SmbusI801 => "SmbusI801.bin",
            Self::IntelPchThermal => "IntelPCHThermal.bin",
        }
    }
}

/// Why a module could not be put on a fresh handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleError {
    /// The module file is not in the modules directory.
    FileMissing {
        /// The module.
        module: Module,
    },
    /// The file exists but could not be read.
    Unreadable {
        /// The module.
        module: Module,
        /// The operating system's reason.
        reason: String,
    },
    /// The module's `main()` refused this machine: wrong vendor or no
    /// supported controller.
    Refused {
        /// The module.
        module: Module,
    },
    /// The driver rejected the file: not signed by the key it trusts, or
    /// damaged.
    Rejected {
        /// The module.
        module: Module,
        /// The driver's error.
        reason: String,
    },
    /// Any other driver failure while opening a handle or loading.
    Failed {
        /// The module.
        module: Module,
        /// The driver's error.
        reason: String,
    },
}

impl ModuleError {
    /// The cause in plain words, as it appears in a feature's detail.
    pub fn plain(&self) -> String {
        match self {
            Self::FileMissing { module } => format!(
                "{} is missing from the modules folder; run `cargo xtask fetch-modules`",
                module.file_name()
            ),
            Self::Unreadable { module, reason } => {
                format!("{} could not be read: {reason}", module.file_name())
            }
            // This module refuses for two reasons the user can tell apart
            // from "unsupported": no thermal function, or one that is off.
            Self::Refused { module: Module::IntelPchThermal } => format!(
                "PawnIO's {} found no supported chipset thermal sensor, or its function is not \
                 decoding memory",
                Module::IntelPchThermal.file_name()
            ),
            Self::Refused { module } => {
                format!("PawnIO's {} does not support this machine", module.file_name())
            }
            Self::Rejected { module, reason } => format!(
                "PawnIO refused {} ({reason}); run `cargo xtask fetch-modules` to replace it",
                module.file_name()
            ),
            Self::Failed { module, reason } => {
                format!("PawnIO could not load {}: {reason}", module.file_name())
            }
        }
    }
}

/// An SMBus handle and the name of the controller behind it.
pub struct SmbusAccess {
    /// The bus.
    pub bus: Box<dyn Smbus>,
    /// Controller description for the feature detail.
    pub controller: String,
}

/// The chipset temperature register and what the module found.
pub struct PchAccess {
    /// The register.
    pub sensor: Box<dyn PchThermal>,
    /// The chipset and the register window, for the feature detail.
    pub description: String,
}

/// What the probe needs from the machine. Every method that returns a bus
/// gives a fresh handle with the module loaded; the Super I/O one is called
/// twice, because the module remembers the chip it scanned.
pub trait Platform {
    /// Opens the driver and returns its version.
    fn open_driver(&self) -> Result<Version, Error>;
    /// The processor identity.
    fn cpu_identity(&self) -> CpuIdentity;
    /// The processor's cores, or why they could not be read.
    fn cpu_topology(&self) -> Result<CpuTopology, String>;
    /// The clock the power meters use.
    fn clock(&self) -> Box<dyn Clock>;
    /// The shared bus mutexes.
    fn locks(&self) -> Arc<dyn BusLocks>;
    /// Model-specific registers through `IntelMsr` or `AmdFamily17`.
    fn msr(&self, module: Module) -> Result<Box<dyn Msr>, ModuleError>;
    /// The AMD SMN through `AmdFamily17`.
    fn smn(&self) -> Result<Box<dyn Smn>, ModuleError>;
    /// The Super I/O ports through `LpcIo`.
    fn super_io(&self) -> Result<Box<dyn SuperIo>, ModuleError>;
    /// The SMBus through `SmbusI801`.
    fn smbus(&self) -> Result<SmbusAccess, ModuleError>;
    /// The chipset temperature register through `IntelPchThermal`. Loading
    /// reads PCI configuration space, so the caller holds the PCI mutex.
    fn pch_thermal(&self) -> Result<PchAccess, ModuleError>;
}

/// The result of [`probe`].
pub struct DriverProbe {
    /// The driver's state.
    pub state: DriverState,
    /// The five features in a fixed order: processor, board, memory, chipset,
    /// fan control.
    pub features: Vec<Feature>,
    /// The providers of the available features, ready to put in a hub.
    pub providers: Vec<Box<dyn Provider>>,
    /// One section per memory module found.
    pub info: Vec<InfoSection>,
}

/// Probes the real machine with the modules in `modules_dir`.
pub fn probe(modules_dir: &Path) -> DriverProbe {
    probe_with(&SystemPlatform::new(modules_dir))
}

/// [`probe`] against any [`Platform`].
pub fn probe_with(platform: &dyn Platform) -> DriverProbe {
    let state = driver_state(platform.open_driver());
    let outcomes = if matches!(state, DriverState::Ready { .. }) {
        [
            (CPU_SENSORS, "Processor temperature and power", probe_cpu(platform)),
            (BOARD_SENSORS, "Motherboard fans and temperatures", probe_board(platform)),
            (MEMORY_SENSORS, "Memory module temperatures", probe_memory(platform)),
            (CHIPSET_SENSORS, "Chipset temperature", probe_chipset(platform)),
        ]
    } else {
        let detail = needs_driver(&state);
        [
            (CPU_SENSORS, "Processor temperature and power", Outcome::unavailable(&detail)),
            (BOARD_SENSORS, "Motherboard fans and temperatures", Outcome::unavailable(&detail)),
            (MEMORY_SENSORS, "Memory module temperatures", Outcome::unavailable(&detail)),
            (CHIPSET_SENSORS, "Chipset temperature", Outcome::unavailable(&detail)),
        ]
    };

    let mut features = Vec::with_capacity(5);
    let mut providers = Vec::new();
    let mut info = Vec::new();
    for (key, title, outcome) in outcomes {
        features.push(Feature {
            key: key.to_owned(),
            title: title.to_owned(),
            available: outcome.available,
            detail: outcome.detail,
        });
        providers.extend(outcome.providers);
        info.extend(outcome.info);
    }
    features.push(Feature {
        key: FAN_CONTROL.to_owned(),
        title: "Fan control".to_owned(),
        available: false,
        detail: "Not part of this version".to_owned(),
    });
    DriverProbe { state, features, providers, info }
}

/// What opening the driver says about its state.
fn driver_state(opened: Result<Version, Error>) -> DriverState {
    match opened {
        Ok(version) => DriverState::Ready { version: version.to_string() },
        Err(Error::DriverAbsent { .. }) => DriverState::Missing,
        Err(Error::DriverTooOld { found, required }) => {
            DriverState::TooOld { version: found.to_string(), required: required.to_string() }
        }
        Err(other) => DriverState::Failed { error: other.to_string() },
    }
}

/// The detail of a feature that cannot work because the driver cannot.
fn needs_driver(state: &DriverState) -> String {
    match state {
        DriverState::Missing => {
            format!("Needs PawnIO, which is not installed. Get it from {INSTALL_URL}")
        }
        DriverState::TooOld { version, required } => format!(
            "Needs PawnIO {required} or newer, and {version} is installed. Update it from \
             {INSTALL_URL}"
        ),
        DriverState::Failed { error } => {
            format!("Needs PawnIO, which could not be opened: {error}")
        }
        DriverState::Ready { .. } => String::new(),
    }
}

/// What one feature found.
struct Outcome {
    available: bool,
    detail: String,
    providers: Vec<Box<dyn Provider>>,
    info: Vec<InfoSection>,
}

impl Outcome {
    fn unavailable(detail: &str) -> Self {
        Self {
            available: false,
            detail: detail.to_owned(),
            providers: Vec::new(),
            info: Vec::new(),
        }
    }

    fn found(provider: impl Provider + 'static, detail: &str) -> Self {
        Self {
            available: true,
            detail: detail.to_owned(),
            providers: vec![Box::new(provider)],
            info: Vec::new(),
        }
    }
}

fn probe_cpu(platform: &dyn Platform) -> Outcome {
    let identity = platform.cpu_identity();
    let topology = match platform.cpu_topology() {
        Ok(topology) => topology,
        Err(reason) => {
            return Outcome::unavailable(&format!(
                "The processor layout could not be read: {reason}"
            ));
        }
    };
    match identity.vendor {
        Vendor::Intel => {
            let msr = match platform.msr(Module::IntelMsr) {
                Ok(msr) => msr,
                Err(error) => return Outcome::unavailable(&error.plain()),
            };
            match IntelCpuProvider::probe(&identity, &topology, msr, platform.clock()) {
                Ok(provider) => {
                    let detail = provider.summary().to_owned();
                    Outcome::found(provider, &detail)
                }
                Err(error) => Outcome::unavailable(&format!(
                    "The processor's sensors could not be read: {error}"
                )),
            }
        }
        Vendor::Amd => {
            let msr = match platform.msr(Module::AmdFamily17) {
                Ok(msr) => msr,
                Err(error) => return Outcome::unavailable(&error.plain()),
            };
            let smn = match platform.smn() {
                Ok(smn) => smn,
                Err(error) => return Outcome::unavailable(&error.plain()),
            };
            match AmdCpuProvider::probe(
                &identity,
                &topology,
                smn,
                msr,
                platform.locks(),
                platform.clock(),
            ) {
                Ok(provider) => {
                    let detail = provider.summary().to_owned();
                    Outcome::found(provider, &detail)
                }
                Err(error) => Outcome::unavailable(&format!(
                    "The processor's sensors could not be read: {error}"
                )),
            }
        }
        Vendor::Other => Outcome::unavailable(&format!(
            "Only Intel and AMD processors are supported, and this one is {}",
            identity.brand
        )),
    }
}

fn probe_board(platform: &dyn Platform) -> Outcome {
    let locks = platform.locks();
    let mut scanner = match platform.super_io() {
        Ok(io) => io,
        Err(error) => return Outcome::unavailable(&error.plain()),
    };
    let detection = {
        let _isa = match locks.acquire(BusKind::Isa) {
            Ok(guard) => guard,
            Err(error) => {
                return Outcome::unavailable(&format!(
                    "The motherboard's chip could not be scanned: {error}"
                ));
            }
        };
        detect(scanner.as_mut())
    };
    let Some(chip) =
        detection.chips.iter().find(|chip| !matches!(chip, Detected::Unsupported { .. }))
    else {
        let detail = if !detection.chips.is_empty() {
            let found: Vec<String> = detection.chips.iter().map(Detected::describe).collect();
            format!("No supported sensor chip. Found: {}", found.join("; "))
        } else if !detection.problems.is_empty() {
            format!("The scan for a sensor chip failed: {}", detection.problems.join("; "))
        } else {
            "No sensor chip answered on the standard ports".to_owned()
        };
        return Outcome::unavailable(&detail);
    };
    // The module remembers the slot and the I/O ranges it learned, so the
    // provider gets a handle of its own.
    let io = match platform.super_io() {
        Ok(io) => io,
        Err(error) => return Outcome::unavailable(&error.plain()),
    };
    match SuperIoProvider::open(chip, io, locks) {
        Ok(provider) => {
            let detail = provider.summary().to_owned();
            Outcome::found(provider, &detail)
        }
        Err(error) => Outcome::unavailable(&format!(
            "The motherboard's sensor chip could not be read: {error}"
        )),
    }
}

fn probe_memory(platform: &dyn Platform) -> Outcome {
    let SmbusAccess { bus, controller } = match platform.smbus() {
        Ok(access) => access,
        Err(error) => return Outcome::unavailable(&error.plain()),
    };
    match DimmProvider::probe(bus, platform.locks(), &controller) {
        Ok(probed) => {
            let mut outcome = match probed.provider {
                Some(provider) => Outcome::found(provider, &probed.summary),
                // The summary says what was scanned and why nothing was found.
                None => Outcome::unavailable(&probed.summary),
            };
            outcome.info = probed.info;
            outcome
        }
        Err(error) => {
            Outcome::unavailable(&format!("The memory bus could not be scanned: {error}"))
        }
    }
}

fn probe_chipset(platform: &dyn Platform) -> Outcome {
    let locks = platform.locks();
    let access = {
        // Loading the module reads PCI configuration space to find the
        // thermal function.
        let _pci = match locks.acquire(BusKind::Pci) {
            Ok(guard) => guard,
            Err(error) => {
                return Outcome::unavailable(&format!(
                    "The chipset's thermal sensor could not be looked up: {error}"
                ));
            }
        };
        match platform.pch_thermal() {
            Ok(access) => access,
            Err(error) => return Outcome::unavailable(&error.plain()),
        }
    };
    match PchThermalProvider::probe(access.sensor, &access.description) {
        Ok(provider) => {
            let detail = provider.summary().to_owned();
            Outcome::found(provider, &detail)
        }
        Err(error) => {
            Outcome::unavailable(&format!("The chipset temperature could not be read: {error}"))
        }
    }
}

// ---------------------------------------------------------------------------
// The real machine
// ---------------------------------------------------------------------------

/// The real driver, processor and bus mutexes, with modules read from a
/// directory.
pub struct SystemPlatform {
    modules_dir: PathBuf,
    locks: Arc<SystemBusLocks>,
}

impl SystemPlatform {
    /// A platform that reads module files from `modules_dir`.
    pub fn new(modules_dir: &Path) -> Self {
        Self { modules_dir: modules_dir.to_owned(), locks: Arc::new(SystemBusLocks::new()) }
    }

    /// A fresh handle with `module` loaded. The file is read first, so a
    /// missing file never reaches the driver.
    fn load(&self, module: Module) -> Result<Arc<dyn Executor>, ModuleError> {
        let path = self.modules_dir.join(module.file_name());
        let blob = std::fs::read(&path).map_err(|error| match error.kind() {
            ErrorKind::NotFound => ModuleError::FileMissing { module },
            _ => ModuleError::Unreadable { module, reason: error.to_string() },
        })?;
        let device = PawnIo::open()
            .map_err(|error| ModuleError::Failed { module, reason: error.to_string() })?;
        device.load(&blob).map_err(|error| classify_load_error(module, error))?;
        Ok(Arc::new(device))
    }
}

/// How a failed load is told to the user.
fn classify_load_error(module: Module, error: Error) -> ModuleError {
    match error {
        Error::NotSupported { function: None } => ModuleError::Refused { module },
        // A module whose `main()` finds none of the devices it supports (the
        // PCH thermal one does) fails with STATUS_NO_SUCH_DEVICE instead.
        Error::Status { operation, status: status::NO_SUCH_DEVICE } if operation == "load" => {
            ModuleError::Refused { module }
        }
        Error::SignatureRefused { .. } | Error::ModuleMalformed { .. } => {
            ModuleError::Rejected { module, reason: error.to_string() }
        }
        other => ModuleError::Failed { module, reason: other.to_string() },
    }
}

impl Platform for SystemPlatform {
    fn open_driver(&self) -> Result<Version, Error> {
        PawnIo::open()?.driver_version()
    }

    fn cpu_identity(&self) -> CpuIdentity {
        CpuIdentity::read()
    }

    fn cpu_topology(&self) -> Result<CpuTopology, String> {
        CpuTopology::read().map_err(|error| error.to_string())
    }

    fn clock(&self) -> Box<dyn Clock> {
        Box::new(SystemClock)
    }

    fn locks(&self) -> Arc<dyn BusLocks> {
        self.locks.clone()
    }

    fn msr(&self, module: Module) -> Result<Box<dyn Msr>, ModuleError> {
        Ok(Box::new(PawnMsr::new(self.load(module)?)))
    }

    fn smn(&self) -> Result<Box<dyn Smn>, ModuleError> {
        Ok(Box::new(PawnSmn::new(self.load(Module::AmdFamily17)?)))
    }

    fn super_io(&self) -> Result<Box<dyn SuperIo>, ModuleError> {
        Ok(Box::new(PawnSuperIo::new(self.load(Module::LpcIo)?)))
    }

    fn smbus(&self) -> Result<SmbusAccess, ModuleError> {
        let module = self.load(Module::SmbusI801)?;
        // The identity is informational: three cells the module filled at load.
        let mut identity = [0u64; 3];
        let controller = match module.execute("ioctl_identity", &[], &mut identity) {
            Ok(()) => describe_controller(identity),
            Err(_) => "SMBus controller".to_owned(),
        };
        Ok(SmbusAccess { bus: Box::new(PawnSmbus::new(module)), controller })
    }

    fn pch_thermal(&self) -> Result<PchAccess, ModuleError> {
        let module = self.load(Module::IntelPchThermal)?;
        // The identity is informational: five cells the module filled at load.
        let mut identity = [0u64; 5];
        let description = match module.execute("ioctl_identity", &[], &mut identity) {
            Ok(()) => describe_identity(identity),
            Err(_) => "Intel PCH thermal sensor".to_owned(),
        };
        Ok(PchAccess { sensor: Box::new(PawnPchThermal::new(module)), description })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::bus::replay::{
        ReplayMsr, ReplayPchThermal, ReplaySmbus, ReplaySmn, ReplaySuperIo, smbus_byte_key,
        smbus_word_key, smn_key, superio_config_key, superio_unlock_key, unlock_code,
    };
    use crate::bus::{FakeBusLocks, Fixture, LogicalCpu, PchRoute, SuperIoSlot, Unlock};
    use crate::cpu::{ManualClock, Package};

    const NCT6779D_FIXTURE: &str = include_str!("../tests/fixtures/nct6779d.txt");
    const INTEL_FIXTURE: &str = include_str!("../tests/fixtures/intel-msr.txt");

    fn fixture(text: &str) -> Fixture {
        Fixture::parse(text)
            .unwrap_or_else(|error| panic!("(fixture): expected valid text | {error}"))
    }

    /// A machine described by recorded registers. `calls` names every
    /// module a probe asked for, in order.
    struct ReplayPlatform {
        driver: Result<Version, Error>,
        identity: CpuIdentity,
        topology: Result<CpuTopology, String>,
        msr: Fixture,
        smn: Fixture,
        super_io: Fixture,
        smbus: Fixture,
        /// The chipset thermal register: its layout, the raw values it reads
        /// in turn, and the description the module's identity gives.
        pch: (PchRoute, Vec<u32>, String),
        refused: Vec<(Module, ModuleError)>,
        locks: Arc<FakeBusLocks>,
        calls: Mutex<Vec<&'static str>>,
    }

    impl ReplayPlatform {
        /// The reference machine: a Xeon E5-2680 v4, an NCT6779D and an
        /// SMBus with no memory module on it.
        fn reference() -> Self {
            Self {
                driver: Ok(Version::new(2, 2, 0)),
                identity: CpuIdentity {
                    vendor: Vendor::Intel,
                    family: 6,
                    model: 0x4F,
                    stepping: 1,
                    brand: "Intel(R) Xeon(R) CPU E5-2680 v4 @ 2.40GHz".to_owned(),
                },
                topology: Ok(CpuTopology {
                    packages: vec![Package {
                        cores: (0..14)
                            .map(|core| LogicalCpu { group: 0, index: core * 2 })
                            .collect(),
                    }],
                }),
                msr: fixture(INTEL_FIXTURE),
                smn: Fixture::new(),
                super_io: fixture(NCT6779D_FIXTURE),
                smbus: Fixture::new(),
                pch: (
                    PchRoute::PciBar0,
                    // What this machine's `ioctl_read_raw` and `ioctl_identity`
                    // returned: 49.0 degrees.
                    vec![0x00C6],
                    describe_identity([1, 0x8C24_8086, 0x1F06, 0x383F_FFF1_4000, 0x100]),
                ),
                refused: Vec::new(),
                locks: Arc::new(FakeBusLocks::free()),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn with_module_error(mut self, module: Module, error: ModuleError) -> Self {
            self.refused.push((module, error));
            self
        }

        fn gate(&self, module: Module) -> Result<(), ModuleError> {
            self.calls.lock().unwrap().push(module.file_name());
            match self.refused.iter().find(|(refused, _)| *refused == module) {
                Some((_, error)) => Err(error.clone()),
                None => Ok(()),
            }
        }

        fn loaded(&self) -> Vec<&'static str> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Platform for ReplayPlatform {
        fn open_driver(&self) -> Result<Version, Error> {
            self.driver.clone()
        }
        fn cpu_identity(&self) -> CpuIdentity {
            self.identity.clone()
        }
        fn cpu_topology(&self) -> Result<CpuTopology, String> {
            self.topology.clone()
        }
        fn clock(&self) -> Box<dyn Clock> {
            Box::new(ManualClock::new())
        }
        fn locks(&self) -> Arc<dyn BusLocks> {
            self.locks.clone()
        }
        fn msr(&self, module: Module) -> Result<Box<dyn Msr>, ModuleError> {
            self.gate(module)?;
            Ok(Box::new(ReplayMsr::from_fixture(&self.msr)))
        }
        fn smn(&self) -> Result<Box<dyn Smn>, ModuleError> {
            self.gate(Module::AmdFamily17)?;
            Ok(Box::new(ReplaySmn::from_fixture(&self.smn)))
        }
        fn super_io(&self) -> Result<Box<dyn SuperIo>, ModuleError> {
            self.gate(Module::LpcIo)?;
            Ok(Box::new(ReplaySuperIo::new(self.super_io.clone())))
        }
        fn smbus(&self) -> Result<SmbusAccess, ModuleError> {
            self.gate(Module::SmbusI801)?;
            Ok(SmbusAccess {
                bus: Box::new(ReplaySmbus::new(self.smbus.clone())),
                controller: "i801 SMBus controller 8086:8C22 at I/O 0xf000".to_owned(),
            })
        }
        fn pch_thermal(&self) -> Result<PchAccess, ModuleError> {
            self.gate(Module::IntelPchThermal)?;
            let (route, raws, description) = &self.pch;
            Ok(PchAccess {
                sensor: Box::new(ReplayPchThermal::new(*route, raws)),
                description: description.clone(),
            })
        }
    }

    fn feature<'a>(probe: &'a DriverProbe, key: &str) -> &'a Feature {
        probe.features.iter().find(|feature| feature.key == key).unwrap_or_else(|| {
            panic!("(features): expected `{key}` | received: {:?}", probe.features)
        })
    }

    fn keys(probe: &DriverProbe) -> Vec<&str> {
        probe.features.iter().map(|feature| feature.key.as_str()).collect()
    }

    /// Sensors of every provider and one sample of each, as the live test
    /// does, checking the one-value-per-sensor contract.
    fn sample_all(probe: &mut DriverProbe) -> usize {
        let mut total = 0;
        for provider in &mut probe.providers {
            let mut values = Vec::new();
            provider.sample(&mut values);
            assert_eq!(
                values.len(),
                provider.sensors().len(),
                "(provider): expected one value per sensor"
            );
            total += values.len();
        }
        total
    }

    fn assert_fan_control_is_out(probe: &DriverProbe) {
        let fan = feature(probe, FAN_CONTROL);
        assert!(!fan.available, "(fan-control): expected unavailable | received: {fan:?}");
        assert_eq!(fan.title, "Fan control");
        assert_eq!(fan.detail, "Not part of this version");
    }

    // -- the driver ---------------------------------------------------------

    #[test]
    fn the_five_features_are_always_listed_in_the_same_order_with_their_titles() {
        for driver in [
            Ok(Version::new(2, 2, 0)),
            Err(Error::DriverAbsent { win32: 2 }),
            Err(Error::NotElevated),
        ] {
            let platform = ReplayPlatform { driver, ..ReplayPlatform::reference() };
            let probe = probe_with(&platform);
            assert_eq!(
                keys(&probe),
                [
                    "cpu-sensors",
                    "board-sensors",
                    "memory-sensors",
                    "chipset-sensors",
                    "fan-control"
                ]
            );
            let titles: Vec<&str> = probe.features.iter().map(|f| f.title.as_str()).collect();
            assert_eq!(
                titles,
                [
                    "Processor temperature and power",
                    "Motherboard fans and temperatures",
                    "Memory module temperatures",
                    "Chipset temperature",
                    "Fan control"
                ]
            );
            assert_fan_control_is_out(&probe);
        }
    }

    #[test]
    fn a_missing_driver_makes_every_sensor_feature_need_pawnio_and_loads_nothing() {
        let platform = ReplayPlatform {
            driver: Err(Error::DriverAbsent { win32: 2 }),
            ..ReplayPlatform::reference()
        };
        let probe = probe_with(&platform);
        assert_eq!(probe.state, DriverState::Missing);
        assert!(probe.providers.is_empty());
        assert!(probe.info.is_empty());
        for key in [CPU_SENSORS, BOARD_SENSORS, MEMORY_SENSORS, CHIPSET_SENSORS] {
            let found = feature(&probe, key);
            assert!(!found.available, "({key}): expected unavailable");
            assert!(
                found.detail.starts_with("Needs PawnIO") && found.detail.contains(INSTALL_URL),
                "({key}): expected a detail naming PawnIO and {INSTALL_URL} | received: {}",
                found.detail
            );
        }
        assert_eq!(platform.loaded(), Vec::<&str>::new(), "no module is loaded without a driver");
    }

    #[test]
    fn a_driver_that_is_too_old_says_both_versions() {
        let platform = ReplayPlatform {
            driver: Err(Error::DriverTooOld {
                found: Version::new(1, 9, 0),
                required: Version::new(2, 0, 0),
            }),
            ..ReplayPlatform::reference()
        };
        let probe = probe_with(&platform);
        assert_eq!(
            probe.state,
            DriverState::TooOld { version: "1.9.0".into(), required: "2.0.0".into() }
        );
        let detail = &feature(&probe, CPU_SENSORS).detail;
        assert!(
            detail.contains("2.0.0") && detail.contains("1.9.0"),
            "(cpu-sensors): expected both versions | received: {detail}"
        );
        assert!(probe.providers.is_empty());
    }

    #[test]
    fn a_driver_that_cannot_be_opened_is_failed_with_the_reason() {
        let platform =
            ReplayPlatform { driver: Err(Error::NotElevated), ..ReplayPlatform::reference() };
        let probe = probe_with(&platform);
        let DriverState::Failed { error } = &probe.state else {
            panic!("(driver state): expected Failed | received: {:?}", probe.state)
        };
        assert_eq!(error, &Error::NotElevated.to_string());
        assert!(feature(&probe, BOARD_SENSORS).detail.contains(error.as_str()));
        assert!(probe.providers.is_empty());
    }

    #[test]
    fn every_open_error_maps_to_a_driver_state() {
        assert_eq!(
            driver_state(Ok(Version::new(2, 2, 0))),
            DriverState::Ready { version: "2.2.0".into() }
        );
        assert_eq!(driver_state(Err(Error::DriverAbsent { win32: 3 })), DriverState::Missing);
        assert!(matches!(
            driver_state(Err(Error::OpenFailed { win32: 5 })),
            DriverState::Failed { .. }
        ));
        assert!(matches!(
            driver_state(Err(Error::Status { operation: "version".into(), status: 0xC000_0001 })),
            DriverState::Failed { .. }
        ));
    }

    // -- the reference machine ----------------------------------------------

    #[test]
    fn the_reference_machine_gets_processor_board_and_chipset_sensors_and_an_empty_memory_bus() {
        let platform = ReplayPlatform::reference();
        let mut probe = probe_with(&platform);
        assert_eq!(probe.state, DriverState::Ready { version: "2.2.0".into() });

        let cpu = feature(&probe, CPU_SENSORS);
        assert!(cpu.available, "{cpu:?}");
        assert!(cpu.detail.starts_with("14 cores"), "(cpu-sensors): {}", cpu.detail);

        let board = feature(&probe, BOARD_SENSORS);
        assert!(board.available, "{board:?}");
        assert!(board.detail.contains("NCT6779D"), "(board-sensors): {}", board.detail);

        let memory = feature(&probe, MEMORY_SENSORS);
        assert!(!memory.available, "{memory:?}");
        assert!(
            memory.detail.contains("nothing acknowledged at SMBus addresses 0x50-0x57"),
            "(memory-sensors): {}",
            memory.detail
        );

        let chipset = feature(&probe, CHIPSET_SENSORS);
        assert!(chipset.available, "{chipset:?}");
        assert_eq!(
            chipset.detail,
            "Intel PCH 8086:8C24 at PCI 00:1f.6, PCI thermal BAR0 at 0x383ffff14000 (256 \
             bytes), reads 49.0 °C"
        );
        assert_fan_control_is_out(&probe);

        assert_eq!(probe.providers.len(), 3);
        assert!(probe.info.is_empty());
        assert!(sample_all(&mut probe) > 20, "(sensors): expected the CPU and board sensors");
        assert!(probe.providers.iter().all(|provider| provider.needs_driver()));
        assert_eq!(platform.locks.live_guards(), 0, "every bus mutex is released");
        assert_eq!(
            platform.loaded(),
            ["IntelMSR.bin", "LpcIO.bin", "LpcIO.bin", "SmbusI801.bin", "IntelPCHThermal.bin"],
            "one handle per module, two for the Super I/O"
        );
    }

    // -- processor ----------------------------------------------------------

    /// A Zen 3 machine: Tctl 50 degrees, an energy unit of 2^-16 J.
    fn amd_platform() -> ReplayPlatform {
        let mut smn = Fixture::new();
        smn.set(smn_key(0x0005_9800), 0x3200_0000);
        let mut msr = Fixture::new();
        msr.set("msr.0.0.0x606", 0x000A_1003);
        msr.set("msr.0.0.0x611", 5);
        ReplayPlatform {
            identity: CpuIdentity {
                vendor: Vendor::Amd,
                family: 0x19,
                model: 0x21,
                stepping: 0,
                brand: "AMD Ryzen 9 5950X 16-Core Processor".to_owned(),
            },
            topology: Ok(CpuTopology {
                packages: vec![Package {
                    cores: (0..4).map(|core| LogicalCpu { group: 0, index: core * 2 }).collect(),
                }],
            }),
            msr,
            smn,
            ..ReplayPlatform::reference()
        }
    }

    #[test]
    fn an_amd_processor_uses_amdfamily17_for_both_registers_and_the_smn() {
        let platform = amd_platform();
        let probe = probe_with(&platform);
        let cpu = feature(&probe, CPU_SENSORS);
        assert!(cpu.available, "{cpu:?}");
        assert!(cpu.detail.starts_with("family 0x19 model 0x21"), "(cpu-sensors): {}", cpu.detail);
        let loaded = platform.loaded();
        assert!(
            loaded.starts_with(&["AMDFamily17.bin", "AMDFamily17.bin"]),
            "(modules): expected AMDFamily17 for the MSRs and the SMN | received: {loaded:?}"
        );
        assert!(!loaded.contains(&"IntelMSR.bin"), "{loaded:?}");
    }

    #[test]
    fn a_processor_of_another_vendor_is_unsupported_and_loads_no_cpu_module() {
        let platform = ReplayPlatform {
            identity: CpuIdentity {
                vendor: Vendor::Other,
                family: 6,
                model: 1,
                stepping: 0,
                brand: "Example Processor".to_owned(),
            },
            ..ReplayPlatform::reference()
        };
        let probe = probe_with(&platform);
        let cpu = feature(&probe, CPU_SENSORS);
        assert!(!cpu.available);
        assert_eq!(
            cpu.detail,
            "Only Intel and AMD processors are supported, and this one is Example Processor"
        );
        assert!(!platform.loaded().iter().any(|file| file.contains("MSR") || file.contains("AMD")));
        assert!(feature(&probe, BOARD_SENSORS).available, "the other features are unaffected");
    }

    #[test]
    fn an_unreadable_processor_layout_is_explained() {
        let platform = ReplayPlatform {
            topology: Err("the OS refused".to_owned()),
            ..ReplayPlatform::reference()
        };
        let probe = probe_with(&platform);
        assert_eq!(
            feature(&probe, CPU_SENSORS).detail,
            "The processor layout could not be read: the OS refused"
        );
    }

    #[test]
    fn registers_that_cannot_be_read_name_the_processor_probe_error() {
        let platform = ReplayPlatform { msr: Fixture::new(), ..ReplayPlatform::reference() };
        let probe = probe_with(&platform);
        let cpu = feature(&probe, CPU_SENSORS);
        assert!(!cpu.available);
        assert!(
            cpu.detail.starts_with("The processor's sensors could not be read: (intel cpu)"),
            "(cpu-sensors): {}",
            cpu.detail
        );
        assert!(feature(&probe, BOARD_SENSORS).available);
    }

    // -- module files -------------------------------------------------------

    #[test]
    fn a_missing_module_file_names_the_file_and_the_fetch_command_and_spares_the_rest() {
        let platform = ReplayPlatform::reference().with_module_error(
            Module::IntelMsr,
            ModuleError::FileMissing { module: Module::IntelMsr },
        );
        let probe = probe_with(&platform);
        let cpu = feature(&probe, CPU_SENSORS);
        assert!(!cpu.available);
        assert_eq!(
            cpu.detail,
            "IntelMSR.bin is missing from the modules folder; run `cargo xtask fetch-modules`"
        );
        assert!(feature(&probe, BOARD_SENSORS).available, "a failure never takes down another");
        assert!(feature(&probe, CHIPSET_SENSORS).available, "a failure never takes down another");
        assert_eq!(probe.providers.len(), 2);
    }

    #[test]
    fn a_module_that_refuses_the_machine_is_told_apart_from_a_missing_one() {
        let platform = ReplayPlatform::reference()
            .with_module_error(
                Module::SmbusI801,
                ModuleError::Refused { module: Module::SmbusI801 },
            )
            .with_module_error(Module::LpcIo, ModuleError::Refused { module: Module::LpcIo });
        let probe = probe_with(&platform);
        assert_eq!(
            feature(&probe, MEMORY_SENSORS).detail,
            "PawnIO's SmbusI801.bin does not support this machine"
        );
        assert_eq!(
            feature(&probe, BOARD_SENSORS).detail,
            "PawnIO's LpcIO.bin does not support this machine"
        );
        assert!(feature(&probe, CPU_SENSORS).available);
    }

    #[test]
    fn a_module_the_driver_rejects_asks_for_a_fresh_download() {
        let platform = ReplayPlatform::reference().with_module_error(
            Module::AmdFamily17,
            ModuleError::Rejected { module: Module::AmdFamily17, reason: "signature".into() },
        );
        let platform = ReplayPlatform { identity: amd_platform().identity, ..platform };
        let probe = probe_with(&platform);
        let detail = &feature(&probe, CPU_SENSORS).detail;
        assert_eq!(
            detail,
            "PawnIO refused AMDFamily17.bin (signature); run `cargo xtask fetch-modules` to \
             replace it"
        );
    }

    #[test]
    fn load_failures_are_classified_by_cause() {
        let module = Module::LpcIo;
        assert_eq!(
            classify_load_error(module, Error::NotSupported { function: None }),
            ModuleError::Refused { module }
        );
        // A module that finds none of its devices (IntelPCHThermal) says so with
        // STATUS_NO_SUCH_DEVICE, which is a refusal too.
        assert_eq!(
            classify_load_error(
                module,
                Error::Status { operation: "load".into(), status: status::NO_SUCH_DEVICE }
            ),
            ModuleError::Refused { module }
        );
        // The same status from a function is not a load refusal.
        assert!(matches!(
            classify_load_error(
                module,
                Error::Status { operation: "ioctl_x".into(), status: status::NO_SUCH_DEVICE }
            ),
            ModuleError::Failed { .. }
        ));
        for rejected in [
            Error::SignatureRefused { status: 0xC000_A000 },
            Error::ModuleMalformed { status: 0xC000_000D },
        ] {
            assert!(
                matches!(classify_load_error(module, rejected), ModuleError::Rejected { .. }),
                "(load error): expected Rejected for a refused or malformed file"
            );
        }
        // A function that declines is not a load refusal.
        assert!(matches!(
            classify_load_error(module, Error::NotSupported { function: Some("ioctl_x".into()) }),
            ModuleError::Failed { .. }
        ));
        assert!(matches!(
            classify_load_error(module, Error::AlreadyLoaded),
            ModuleError::Failed { .. }
        ));
    }

    #[test]
    fn every_module_error_reads_as_a_sentence_naming_the_file() {
        let module = Module::SmbusI801;
        let unreadable = ModuleError::Unreadable { module, reason: "access denied".into() };
        let failed = ModuleError::Failed { module, reason: "busy".into() };
        assert_eq!(unreadable.plain(), "SmbusI801.bin could not be read: access denied");
        assert_eq!(failed.plain(), "PawnIO could not load SmbusI801.bin: busy");
        let files: Vec<&str> =
            [Module::IntelMsr, Module::AmdFamily17, Module::LpcIo, module, Module::IntelPchThermal]
                .map(Module::file_name)
                .to_vec();
        assert_eq!(
            files,
            [
                "IntelMSR.bin",
                "AMDFamily17.bin",
                "LpcIO.bin",
                "SmbusI801.bin",
                "IntelPCHThermal.bin"
            ]
        );
    }

    #[test]
    fn the_real_platform_reports_a_missing_module_file_without_touching_the_driver() {
        let platform = SystemPlatform::new(Path::new("a-directory-that-does-not-exist"));
        let Err(error) = platform.load(Module::LpcIo) else {
            panic!("(module load): expected an error | received: a loaded module")
        };
        assert_eq!(error, ModuleError::FileMissing { module: Module::LpcIo });
    }

    #[test]
    fn probing_the_real_machine_without_module_files_finds_no_provider() {
        // Whatever the machine has installed, a directory without modules
        // yields features that are all unavailable and nothing to sample.
        let probe = probe(Path::new("a-directory-that-does-not-exist"));
        assert_eq!(
            keys(&probe),
            ["cpu-sensors", "board-sensors", "memory-sensors", "chipset-sensors", "fan-control"]
        );
        assert!(probe.providers.is_empty());
        assert!(
            probe.features.iter().all(|feature| !feature.available && !feature.detail.is_empty())
        );
    }

    // -- motherboard --------------------------------------------------------

    #[test]
    fn a_super_io_chip_this_reader_does_not_know_is_reported_not_read() {
        let mut super_io = Fixture::new();
        super_io.set(superio_unlock_key(SuperIoSlot::Primary), unlock_code(Unlock::WinbondNuvoton));
        super_io.set(superio_config_key(SuperIoSlot::Primary, 0, 0x20), 0x12);
        super_io.set(superio_config_key(SuperIoSlot::Primary, 0, 0x21), 0x34);
        let platform = ReplayPlatform { super_io, ..ReplayPlatform::reference() };
        let probe = probe_with(&platform);
        let board = feature(&probe, BOARD_SENSORS);
        assert!(!board.available);
        assert!(
            board.detail.starts_with("No supported sensor chip. Found: unknown chip (id 0x1234)"),
            "(board-sensors): {}",
            board.detail
        );
        assert!(feature(&probe, CPU_SENSORS).available);
    }

    #[test]
    fn a_machine_with_no_super_io_chip_says_nothing_answered() {
        // Both slots read all ones, as a port nothing decodes does.
        let mut super_io = Fixture::new();
        for slot in SuperIoSlot::ALL {
            super_io.set(superio_unlock_key(slot), 0);
            super_io.set(superio_config_key(slot, 0, 0x20), 0xFF);
            super_io.set(superio_config_key(slot, 0, 0x21), 0xFF);
        }
        let platform = ReplayPlatform { super_io, ..ReplayPlatform::reference() };
        let probe = probe_with(&platform);
        assert_eq!(
            feature(&probe, BOARD_SENSORS).detail,
            "No sensor chip answered on the standard ports"
        );
    }

    #[test]
    fn a_held_isa_bus_leaves_the_board_unavailable_and_the_rest_alone() {
        let platform = ReplayPlatform {
            locks: Arc::new(FakeBusLocks::held_by_another_program(&[BusKind::Isa])),
            ..ReplayPlatform::reference()
        };
        let probe = probe_with(&platform);
        let board = feature(&probe, BOARD_SENSORS);
        assert!(!board.available);
        assert!(
            board.detail.starts_with("The motherboard's chip could not be scanned: "),
            "(board-sensors): {}",
            board.detail
        );
        assert!(feature(&probe, CPU_SENSORS).available);
        assert_eq!(platform.locks.live_guards(), 0);
    }

    // -- memory -------------------------------------------------------------

    /// A DDR5 module in slot 0 whose hub reads 25 degrees.
    fn ddr5_smbus() -> Fixture {
        let mut smbus = Fixture::new();
        for (register, value) in [
            (0x00, 0x51),
            (0x01, 0x18),
            (0x02, 0x12),
            (0x03, 0x80),
            (0x04, 0xCE),
            (0x05, 0x02),
            (0x1A, 0),
        ] {
            smbus.set(smbus_byte_key(0x50, 0, register), value);
        }
        smbus.set(smbus_word_key(0x50, 0x31), 100 << 2);
        smbus
    }

    #[test]
    fn a_memory_module_with_a_sensor_makes_the_feature_available_with_its_info() {
        let platform = ReplayPlatform { smbus: ddr5_smbus(), ..ReplayPlatform::reference() };
        let mut probe = probe_with(&platform);
        let memory = feature(&probe, MEMORY_SENSORS);
        assert!(memory.available, "{memory:?}");
        assert_eq!(
            memory.detail,
            "i801 SMBus controller 8086:8C22 at I/O 0xf000: 1 SPD device(s) in slot(s) 0; 1 with \
             a temperature sensor"
        );
        assert_eq!(probe.info.len(), 1);
        assert_eq!(probe.info[0].title, "DIMM 0");
        assert_eq!(probe.providers.len(), 4);
        let declared: usize = probe.providers.iter().map(|provider| provider.sensors().len()).sum();
        assert_eq!(sample_all(&mut probe), declared);
        // Processor, board, memory, chipset.
        let specs = probe.providers[2].sensors();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].key, "memory.dimm0.temp");
    }

    #[test]
    fn an_empty_memory_bus_reports_what_was_scanned_and_keeps_the_info_empty() {
        let probe = probe_with(&ReplayPlatform::reference());
        let memory = feature(&probe, MEMORY_SENSORS);
        assert!(!memory.available);
        assert_eq!(
            memory.detail,
            "i801 SMBus controller 8086:8C22 at I/O 0xf000: nothing acknowledged at SMBus \
             addresses 0x50-0x57, so no memory module is reachable through this controller"
        );
        assert!(probe.info.is_empty());
    }

    #[test]
    fn a_module_without_a_sensor_is_unavailable_but_still_listed_in_the_info() {
        let mut smbus = ddr5_smbus();
        // Capability register: no temperature sensor in the hub.
        smbus.set(smbus_byte_key(0x50, 0, 0x05), 0x00);
        let platform = ReplayPlatform { smbus, ..ReplayPlatform::reference() };
        let probe = probe_with(&platform);
        let memory = feature(&probe, MEMORY_SENSORS);
        assert!(!memory.available);
        assert!(memory.detail.contains("0 with a temperature sensor"), "{}", memory.detail);
        assert_eq!(probe.info.len(), 1);
        assert_eq!(probe.providers.len(), 3);
    }

    #[test]
    fn a_held_smbus_leaves_memory_unavailable_and_the_rest_alone() {
        let platform = ReplayPlatform {
            locks: Arc::new(FakeBusLocks::held_by_another_program(&[BusKind::Smbus])),
            ..ReplayPlatform::reference()
        };
        let probe = probe_with(&platform);
        let memory = feature(&probe, MEMORY_SENSORS);
        assert!(!memory.available);
        assert!(
            memory.detail.starts_with("The memory bus could not be scanned: "),
            "(memory-sensors): {}",
            memory.detail
        );
        assert!(feature(&probe, BOARD_SENSORS).available);
    }

    // -- chipset ------------------------------------------------------------

    #[test]
    fn the_chipset_provider_is_last_and_declares_one_chipset_temperature() {
        let probe = probe_with(&ReplayPlatform::reference());
        let chipset = probe.providers.last().expect("the chipset provider is last");
        let specs = chipset.sensors();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].key, "chipset.temp");
        assert_eq!(specs[0].device, "Chipset");
    }

    #[test]
    fn a_chipset_on_the_pwrm_route_reads_whole_degrees() {
        let platform = ReplayPlatform {
            pch: (
                PchRoute::FixedPwrm,
                vec![0x8000_0231],
                describe_identity([2, 0x7F0C_8086, 0x1F00, 0xFE00_0000, 0x2000]),
            ),
            ..ReplayPlatform::reference()
        };
        let probe = probe_with(&platform);
        let chipset = feature(&probe, CHIPSET_SENSORS);
        assert!(chipset.available, "{chipset:?}");
        assert_eq!(
            chipset.detail,
            "Intel PCH 8086:7F0C at PCI 00:1f.0, fixed PWRM window at 0xfe000000 (8192 bytes), \
             reads 49.0 °C"
        );
    }

    #[test]
    fn a_machine_without_a_chipset_thermal_function_says_so_and_spares_the_rest() {
        let platform = ReplayPlatform::reference().with_module_error(
            Module::IntelPchThermal,
            ModuleError::Refused { module: Module::IntelPchThermal },
        );
        let probe = probe_with(&platform);
        let chipset = feature(&probe, CHIPSET_SENSORS);
        assert!(!chipset.available);
        assert_eq!(
            chipset.detail,
            "PawnIO's IntelPCHThermal.bin found no supported chipset thermal sensor, or its \
             function is not decoding memory"
        );
        assert!(feature(&probe, CPU_SENSORS).available);
        assert!(feature(&probe, BOARD_SENSORS).available);
        assert_eq!(probe.providers.len(), 2);
    }

    #[test]
    fn a_missing_chipset_module_file_names_the_file_and_the_fetch_command() {
        let platform = ReplayPlatform::reference().with_module_error(
            Module::IntelPchThermal,
            ModuleError::FileMissing { module: Module::IntelPchThermal },
        );
        let probe = probe_with(&platform);
        assert_eq!(
            feature(&probe, CHIPSET_SENSORS).detail,
            "IntelPCHThermal.bin is missing from the modules folder; run `cargo xtask \
             fetch-modules`"
        );
    }

    #[test]
    fn a_chipset_register_that_holds_no_reading_is_unavailable_and_names_the_raw_value() {
        let platform = ReplayPlatform {
            pch: (PchRoute::PciBar0, vec![0xFFFF], "Intel PCH example".to_owned()),
            ..ReplayPlatform::reference()
        };
        let probe = probe_with(&platform);
        let chipset = feature(&probe, CHIPSET_SENSORS);
        assert!(!chipset.available);
        assert_eq!(
            chipset.detail,
            "The chipset temperature could not be read: (PCH thermal): expected a register value \
             that decodes to 0..=125 °C | received: 0xffff on the PCI thermal BAR0 route"
        );
        assert_eq!(probe.providers.len(), 2);
    }

    #[test]
    fn a_held_pci_bus_leaves_the_chipset_unavailable_and_the_rest_alone() {
        let platform = ReplayPlatform {
            locks: Arc::new(FakeBusLocks::held_by_another_program(&[BusKind::Pci])),
            ..ReplayPlatform::reference()
        };
        let probe = probe_with(&platform);
        let chipset = feature(&probe, CHIPSET_SENSORS);
        assert!(!chipset.available);
        assert!(
            chipset.detail.starts_with("The chipset's thermal sensor could not be looked up: "),
            "(chipset-sensors): {}",
            chipset.detail
        );
        assert!(
            !platform.loaded().contains(&"IntelPCHThermal.bin"),
            "the module is not loaded without the PCI mutex"
        );
        assert!(feature(&probe, BOARD_SENSORS).available);
        assert_eq!(platform.locks.live_guards(), 0);
    }
}
