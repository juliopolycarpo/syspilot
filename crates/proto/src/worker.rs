//! What the service and its sensor worker say to each other.
//!
//! The worker is a separate executable the service starts while someone
//! needs sensor values. They talk over the worker's standard streams: the
//! worker first writes one [`WorkerFrame::Probed`], then answers every
//! [`SAMPLE_REQUEST`] byte on its input with one [`WorkerFrame::Sample`].
//! The end of its input tells it to exit. Frames use [`crate::frame`].
//!
//! Fan curves are followed inside the worker, next to the registers, and not
//! in the service: the worker sees the sensors it needs without a round
//! trip, and the end of its input (the service stopped or crashed) is its
//! cue to hand every fan back to the BIOS. The service sends the curves as a
//! [`FANS_REQUEST`] byte followed by a [`FanPlan`] frame, and the machine's
//! sleep and wake as [`SUSPEND_REQUEST`] and [`RESUME_REQUEST`]. The worker
//! reports what the controller does in [`WorkerFrame::Fans`].

use serde::{Deserialize, Serialize};

use crate::message::{DriverState, FanRuntime, FanStatus, Feature, SystemInfo};
use crate::profile::FanCurve;
use crate::sensor::SensorList;

/// File name of the worker, expected next to the service executable.
pub const WORKER_EXECUTABLE: &str = "syspilot-sensors.exe";
/// The byte that asks the worker for one sample.
pub const SAMPLE_REQUEST: u8 = b's';
/// The byte that announces a [`FanPlan`] frame, which replaces any earlier
/// plan.
pub const FANS_REQUEST: u8 = b'f';
/// The byte that tells the worker the machine is about to sleep: every fan
/// goes back to the BIOS and nothing is written until [`RESUME_REQUEST`].
pub const SUSPEND_REQUEST: u8 = b'z';
/// The byte that tells the worker the machine woke up.
pub const RESUME_REQUEST: u8 = b'r';

/// What the worker is asked to do with the fans.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FanPlan {
    /// The directory where the worker keeps the BIOS registers it saved
    /// before its first write, so that a crash cannot lose them.
    pub store_directory: String,
    /// The curves to follow. Empty follows nothing and hands back to the BIOS
    /// every channel the worker holds or the store still records.
    pub curves: Vec<FanCurve>,
}

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
    /// What the board's catalog entry and the build allow for the fans.
    pub fans: FanStatus,
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
            fans: FanStatus::default(),
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
    /// What the fan controller does with each channel. Sent when something
    /// in it changed, never more than once per control step.
    Fans(Vec<FanRuntime>),
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
    fn a_fan_plan_and_the_controllers_report_survive_the_wire() {
        use crate::message::FanPhase;
        use crate::profile::{CurvePoint, FanCurve};
        let plan = FanPlan {
            store_directory: r"C:\ProgramData\SysPilot".into(),
            curves: vec![FanCurve {
                channel: "board.pwm2".into(),
                enabled: true,
                sources: vec!["cpu.package.temp".into()],
                points: vec![CurvePoint(40, 30), CurvePoint(80, 100)],
                hysteresis_c: 3,
                min_duty_pct: 30,
                ramp_pct_per_sec: 10,
            }],
        };
        let runtime = WorkerFrame::Fans(vec![FanRuntime {
            channel: "board.pwm2".into(),
            phase: FanPhase::Manual,
            target_duty_pct: Some(30),
            problem: None,
        }]);
        let mut wire = Vec::new();
        write_frame(&mut wire, &plan).unwrap();
        write_frame(&mut wire, &runtime).unwrap();
        let mut wire = std::io::Cursor::new(wire);
        assert_eq!(read_frame::<_, FanPlan>(&mut wire).unwrap(), plan);
        assert_eq!(read_frame::<_, WorkerFrame>(&mut wire).unwrap(), runtime);
    }

    #[test]
    fn the_request_bytes_are_distinct() {
        let bytes = [SAMPLE_REQUEST, FANS_REQUEST, SUSPEND_REQUEST, RESUME_REQUEST];
        let unique: std::collections::BTreeSet<u8> = bytes.into_iter().collect();
        assert_eq!(
            unique.len(),
            bytes.len(),
            "(worker protocol): expected four distinct request bytes | received: {bytes:?}"
        );
    }

    #[test]
    fn an_empty_report_knows_only_the_driver_state() {
        let report = Probed::nothing(DriverState::Ready { version: "2.2.0".into() });
        assert_eq!(report.driver, DriverState::Ready { version: "2.2.0".into() });
        assert!(report.features.is_empty() && report.sensors.sensors.is_empty());
        assert_eq!(report.system, SystemInfo::default());
    }
}
