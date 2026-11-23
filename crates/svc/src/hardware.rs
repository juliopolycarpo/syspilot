//! The service's view of the machine's hardware.
//!
//! The event loop talks to hardware only through [`Hardware`], so it can be
//! exercised with [`FakeHardware`] and never touches a device in tests. The
//! real implementation is [`crate::worker::WorkerHardware`].

use syspilot_hw::Provider;
use syspilot_proto::message::{DriverState, FanRuntime, FanStatus, Feature, SystemInfo};
use syspilot_proto::worker::FanPlan;

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

    /// Whether [`Hardware::driver`] and [`Hardware::system_info`] would have
    /// to wait for a look at the machine. Asking starts that look without
    /// waiting, so the event loop can do other work and ask again.
    fn report_pending(&mut self) -> bool {
        false
    }

    /// The same for [`Hardware::providers`].
    fn providers_pending(&mut self) -> bool {
        false
    }

    /// Lets go of what a finished look kept ready for
    /// [`Hardware::providers`], when nobody wanted the providers after all.
    fn release(&mut self) {}

    /// What the fans of this machine allow, from the look at the machine.
    /// Like [`Hardware::driver`], it may have to wait for that look.
    fn fan_layout(&mut self) -> FanStatus {
        FanStatus::default()
    }

    /// The same without waiting: `None` while no look has been made.
    fn known_fan_layout(&mut self) -> Option<FanStatus> {
        None
    }

    /// Sets the curves the fan controller follows, replacing any earlier
    /// plan. The sensor worker carries them out, so they reach it with the
    /// next sample request, and again with any worker that replaces it.
    fn set_fan_plan(&mut self, _plan: FanPlan) {}

    /// What the fan controller last reported for each channel. Empty when
    /// no worker is running, which means the BIOS holds every channel.
    fn fan_runtime(&mut self) -> Vec<FanRuntime> {
        Vec::new()
    }

    /// Tells the controller the machine sleeps (`true`) or woke (`false`).
    fn set_fans_suspended(&mut self, _suspended: bool) {}
}

#[cfg(test)]
pub use fake::FakeHardware;

#[cfg(test)]
mod fake {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};
    use syspilot_hw::{FakeProvider, SensorSpec};
    use syspilot_proto::message::SystemSummary;
    use syspilot_proto::sensor::{DeviceClass, SensorKind};

    /// What the fake fan controller was told and what it answers.
    #[derive(Default)]
    struct FakeFans {
        layout: FanStatus,
        runtime: Vec<FanRuntime>,
        plans: Vec<FanPlan>,
        suspensions: Vec<bool>,
    }

    /// Scripted hardware: one CPU temperature sensor, plus a fan sensor
    /// while the driver is "installed".
    #[derive(Clone)]
    pub struct FakeHardware {
        driver: Arc<Mutex<DriverState>>,
        probes: Arc<AtomicU32>,
        /// While set, a look at the machine is "under way".
        looking: Arc<AtomicBool>,
        fans: Arc<Mutex<FakeFans>>,
    }

    impl Default for FakeHardware {
        fn default() -> Self {
            Self {
                driver: Arc::new(Mutex::new(DriverState::Missing)),
                probes: Arc::new(AtomicU32::new(0)),
                looking: Arc::new(AtomicBool::new(false)),
                fans: Arc::default(),
            }
        }
    }

    impl FakeHardware {
        fn fans(&self) -> std::sync::MutexGuard<'_, FakeFans> {
            self.fans.lock().unwrap_or_else(PoisonError::into_inner)
        }

        /// What the board allows for the fans, from now on.
        pub fn set_fan_layout(&self, layout: FanStatus) {
            self.fans().layout = layout;
        }

        /// What the fake controller reports from now on.
        pub fn set_fan_runtime(&self, runtime: Vec<FanRuntime>) {
            self.fans().runtime = runtime;
        }

        /// Every plan the service handed over, oldest first.
        pub fn fan_plans(&self) -> Vec<FanPlan> {
            self.fans().plans.clone()
        }

        /// Every sleep (`true`) and wake (`false`) the service announced.
        pub fn fan_suspensions(&self) -> Vec<bool> {
            self.fans().suspensions.clone()
        }

        /// Changes what driver detection reports from now on.
        pub fn set_driver(&self, state: DriverState) {
            *self.driver.lock().unwrap_or_else(PoisonError::into_inner) = state;
        }

        /// Makes every question about the machine answer "later" (`true`)
        /// or lets the look end (`false`).
        pub fn set_looking(&self, looking: bool) {
            self.looking.store(looking, Ordering::Relaxed);
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

        fn report_pending(&mut self) -> bool {
            self.looking.load(Ordering::Relaxed)
        }

        fn providers_pending(&mut self) -> bool {
            self.looking.load(Ordering::Relaxed)
        }

        fn fan_layout(&mut self) -> FanStatus {
            self.fans().layout.clone()
        }

        fn known_fan_layout(&mut self) -> Option<FanStatus> {
            Some(self.fans().layout.clone())
        }

        fn set_fan_plan(&mut self, plan: FanPlan) {
            self.fans().plans.push(plan);
        }

        fn fan_runtime(&mut self) -> Vec<FanRuntime> {
            self.fans().runtime.clone()
        }

        fn set_fans_suspended(&mut self, suspended: bool) {
            self.fans().suspensions.push(suspended);
        }
    }
}
