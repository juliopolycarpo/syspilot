//! The service's view of the machine's hardware.
//!
//! The event loop talks to hardware only through [`Hardware`], so it can be
//! exercised with [`FakeHardware`] and never touches a device in tests.

use syspilot_hw::Provider;
use syspilot_proto::message::{DriverState, Feature, SystemInfo};

/// Where the driver is obtained. SysPilot never ships or installs it.
pub const DRIVER_URL: &str = "https://pawnio.eu/";

/// The driver's state and what it makes available.
#[derive(Debug, Clone, PartialEq)]
pub struct DriverReport {
    /// Whether the driver is usable.
    pub state: DriverState,
    /// Capabilities that depend on the driver, each marked available or not.
    pub features: Vec<Feature>,
}

/// Access to sensors and inventory.
pub trait Hardware {
    /// Detects the driver. Called for every status request, so it must be
    /// cheap, and it must notice a driver installed after the service
    /// started.
    fn driver(&mut self) -> DriverReport;

    /// Opens every sensor provider that works on this machine. Called when
    /// sampling starts; the providers are dropped when it stops.
    fn providers(&mut self) -> Vec<Box<dyn Provider>>;

    /// Collects the hardware and software inventory.
    fn system_info(&mut self) -> SystemInfo;
}

/// The machine the service runs on.
#[derive(Default)]
pub struct MachineHardware;

impl MachineHardware {
    /// Nothing is probed until the first request needs it.
    pub fn new() -> Self {
        Self
    }
}

impl Hardware for MachineHardware {
    fn driver(&mut self) -> DriverReport {
        DriverReport { state: DriverState::Missing, features: Vec::new() }
    }

    fn providers(&mut self) -> Vec<Box<dyn Provider>> {
        Vec::new()
    }

    fn system_info(&mut self) -> SystemInfo {
        SystemInfo::default()
    }
}

#[cfg(test)]
pub use fake::FakeHardware;

#[cfg(test)]
mod fake {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};
    use syspilot_hw::{FakeProvider, SensorSpec};
    use syspilot_proto::message::SystemSummary;
    use syspilot_proto::sensor::{DeviceClass, SensorKind};

    /// Scripted hardware: one CPU temperature sensor, plus a fan sensor
    /// while the driver is "installed".
    #[derive(Clone)]
    pub struct FakeHardware {
        driver: Arc<Mutex<DriverState>>,
        probes: Arc<AtomicU32>,
    }

    impl Default for FakeHardware {
        fn default() -> Self {
            Self {
                driver: Arc::new(Mutex::new(DriverState::Missing)),
                probes: Arc::new(AtomicU32::new(0)),
            }
        }
    }

    impl FakeHardware {
        /// Changes what driver detection reports from now on.
        pub fn set_driver(&self, state: DriverState) {
            *self.driver.lock().unwrap_or_else(PoisonError::into_inner) = state;
        }

        /// How many times the providers were opened.
        pub fn probes(&self) -> u32 {
            self.probes.load(Ordering::Relaxed)
        }

        fn state(&self) -> DriverState {
            self.driver.lock().unwrap_or_else(PoisonError::into_inner).clone()
        }
    }

    /// A provider with one sensor that always reads `value`.
    fn single_sensor(
        key: &str,
        label: &str,
        class: DeviceClass,
        kind: SensorKind,
        value: f64,
    ) -> Box<dyn Provider> {
        let spec = SensorSpec {
            device: "Fake machine".into(),
            class,
            key: key.into(),
            label: label.into(),
            kind,
        };
        Box::new(FakeProvider {
            specs: vec![spec],
            values: vec![Some(value)],
            period_secs: 1,
            samples_taken: Default::default(),
        })
    }

    impl Hardware for FakeHardware {
        fn driver(&mut self) -> DriverReport {
            let state = self.state();
            let available = matches!(state, DriverState::Ready { .. });
            let features = vec![Feature {
                key: "fans".into(),
                title: "Fan speeds".into(),
                available,
                detail: String::new(),
            }];
            DriverReport { state, features }
        }

        fn providers(&mut self) -> Vec<Box<dyn Provider>> {
            self.probes.fetch_add(1, Ordering::Relaxed);
            let mut providers = vec![single_sensor(
                "cpu.temp",
                "CPU",
                DeviceClass::Cpu,
                SensorKind::Temperature,
                42.5,
            )];
            if matches!(self.state(), DriverState::Ready { .. }) {
                providers.push(single_sensor(
                    "board.fan1",
                    "Fan 1",
                    DeviceClass::Board,
                    SensorKind::Fan,
                    900.0,
                ));
            }
            providers
        }

        fn system_info(&mut self) -> SystemInfo {
            SystemInfo {
                summary: SystemSummary { computer: "FAKE-PC".into(), ..SystemSummary::default() },
                sections: vec![],
            }
        }
    }
}
