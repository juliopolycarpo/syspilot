//! The worker's side of the conversation with the service.

use std::io::{Read, Write};

use syspilot_hw::{Hub, Provider};
use syspilot_proto::frame::{FrameError, write_frame};
use syspilot_proto::message::{DriverState, FanStatus, Feature, SystemInfo};
use syspilot_proto::worker::{Probed, SAMPLE_REQUEST, WorkerFrame};

/// The machine as the worker sees it: the inventory plus open providers.
pub struct Machine {
    /// State of the PawnIO driver.
    pub driver: DriverState,
    /// What depends on the driver.
    pub features: Vec<Feature>,
    /// Hardware and software inventory.
    pub system: SystemInfo,
    /// Every sensor provider that works here.
    pub providers: Vec<Box<dyn Provider>>,
}

/// The worker's whole life: probe, report, then answer sample requests
/// until `requests` ends.
pub fn run(
    probe: impl FnOnce() -> Machine,
    mut requests: impl Read,
    mut frames: impl Write,
) -> Result<(), FrameError> {
    let machine = probe();
    let mut hub = Hub::new(machine.providers, 0);
    let probed = Probed {
        driver: machine.driver,
        features: machine.features,
        system: machine.system,
        sensors: hub.list().clone(),
        fans: FanStatus::default(),
    };
    write_frame(&mut frames, &WorkerFrame::Probed(Box::new(probed)))?;
    // One byte at a time on purpose: each request is answered before the
    // next is looked at, and there is one a second.
    let mut request = [0u8; 1];
    loop {
        match requests.read(&mut request) {
            // The service closed its end: time to go.
            Ok(0) | Err(_) => return Ok(()),
            Ok(_) if request[0] == SAMPLE_REQUEST => {
                write_frame(&mut frames, &WorkerFrame::Sample(hub.sample().to_vec()))?;
            }
            // Anything else is not from this version of the service.
            Ok(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syspilot_hw::FakeProvider;
    use syspilot_proto::frame::read_frame;
    use syspilot_proto::message::SystemSummary;

    fn fake_machine() -> Machine {
        Machine {
            driver: DriverState::Ready { version: "2.2.0".into() },
            features: Vec::new(),
            system: SystemInfo {
                summary: SystemSummary { computer: "FAKE-PC".into(), ..SystemSummary::default() },
                sections: Vec::new(),
            },
            providers: vec![Box::new(FakeProvider::temperatures(&[
                ("cpu.temp", Some(41.0)),
                ("board.temp", None),
            ]))],
        }
    }

    #[test]
    fn worker_reports_first_then_answers_each_request_with_a_sample() {
        let requests = [SAMPLE_REQUEST, b'?', SAMPLE_REQUEST];
        let mut output = Vec::new();
        run(fake_machine, &requests[..], &mut output).unwrap();

        let mut frames = std::io::Cursor::new(output);
        let WorkerFrame::Probed(probed) = read_frame(&mut frames).unwrap() else {
            panic!("(worker): expected the report as the first frame");
        };
        assert_eq!(probed.driver, DriverState::Ready { version: "2.2.0".into() });
        assert_eq!(probed.sensors.sensors.len(), 2);
        assert_eq!(probed.system.summary.computer, "FAKE-PC");
        for _ in 0..2 {
            let sample: WorkerFrame = read_frame(&mut frames).unwrap();
            assert_eq!(sample, WorkerFrame::Sample(vec![Some(41.0), None]));
        }
        let end = read_frame::<_, WorkerFrame>(&mut frames).unwrap_err();
        assert!(matches!(end, FrameError::Closed), "an unknown request gets no answer: {end:?}");
    }

    #[test]
    fn worker_without_requests_reports_and_ends() {
        let mut output = Vec::new();
        run(fake_machine, std::io::empty(), &mut output).unwrap();
        let mut frames = std::io::Cursor::new(output);
        assert!(matches!(read_frame(&mut frames).unwrap(), WorkerFrame::Probed(_)));
        assert!(read_frame::<_, WorkerFrame>(&mut frames).is_err());
    }
}
