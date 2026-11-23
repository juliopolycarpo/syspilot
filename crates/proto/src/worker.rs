//! What the service and its sensor worker say to each other.
//!
//! The worker is a separate executable the service starts while someone
//! needs sensor values. They talk over the worker's standard streams: the
//! worker first writes one [`WorkerFrame::Probed`], then answers every
//! [`SAMPLE_REQUEST`] byte on its input with one [`WorkerFrame::Sample`].
//! The end of its input tells it to exit. Frames use [`crate::frame`].

use serde::{Deserialize, Serialize};

use crate::message::{DriverState, Feature, SystemInfo};
use crate::sensor::SensorList;

/// File name of the worker, expected next to the service executable.
pub const WORKER_EXECUTABLE: &str = "syspilot-sensors.exe";
/// The byte that asks the worker for one sample.
pub const SAMPLE_REQUEST: u8 = b's';

/// What a worker found on the machine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Probed {
    /// State of the PawnIO driver.
    pub driver: DriverState,
    /// What depends on the driver, each marked available or not.
    pub features: Vec<Feature>,
    /// Hardware and software inventory.
    pub system: SystemInfo,
    /// The sensors, in the order samples list their values. The
    /// generation is not meaningful: the service numbers sensor sets.
    pub sensors: SensorList,
}

impl Probed {
    /// The report when nothing could look at the machine: only the driver
    /// state is known.
    pub fn nothing(driver: DriverState) -> Self {
        Self {
            driver,
            features: Vec::new(),
            system: SystemInfo::default(),
            sensors: SensorList::default(),
        }
    }
}

/// What travels from the worker to the service.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WorkerFrame {
    /// Sent once, first.
    Probed(Box<Probed>),
    /// One value per sensor, answering one request. `None` is a reading
    /// that failed.
    Sample(Vec<Option<f64>>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{read_frame, write_frame};
    use crate::sensor::{DeviceClass, DeviceInfo, SensorInfo, SensorKind};

    #[test]
    fn frames_survive_the_wire_including_failed_readings() {
        let sensors = SensorList {
            generation: 0,
            devices: vec![DeviceInfo { name: "Processor".into(), class: DeviceClass::Cpu }],
            sensors: vec![SensorInfo {
                id: 0,
                device: 0,
                key: "cpu.load".into(),
                label: "Total".into(),
                kind: SensorKind::Load,
                driver: false,
            }],
        };
        let probed = Probed { sensors, ..Probed::nothing(DriverState::Missing) };
        let sent =
            [WorkerFrame::Probed(Box::new(probed)), WorkerFrame::Sample(vec![Some(12.5), None])];
        let mut wire = Vec::new();
        for frame in &sent {
            write_frame(&mut wire, frame).unwrap();
        }
        let mut wire = std::io::Cursor::new(wire);
        for frame in &sent {
            assert_eq!(&read_frame::<_, WorkerFrame>(&mut wire).unwrap(), frame);
        }
    }

    #[test]
    fn an_empty_report_knows_only_the_driver_state() {
        let report = Probed::nothing(DriverState::Ready { version: "2.2.0".into() });
        assert_eq!(report.driver, DriverState::Ready { version: "2.2.0".into() });
        assert!(report.features.is_empty() && report.sensors.sensors.is_empty());
        assert_eq!(report.system, SystemInfo::default());
    }
}
