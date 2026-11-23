//! The worker's side of the conversation with the service.
//!
//! Two threads: one reads the service's requests from standard input and
//! passes them on, the other owns the hardware. The split lets the worker
//! step the fan curves on its own timer when the service stops asking, and
//! notice the end of its input (the service stopped or crashed) at once, so
//! the fans go back to the BIOS in both cases.

use std::io::{Read, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use syspilot_hw::{Hub, Provider};
use syspilot_proto::frame::{FrameError, read_frame, write_frame};
use syspilot_proto::message::{DriverState, FanStatus, Feature, SystemInfo};
use syspilot_proto::worker::{
    FANS_REQUEST, FanPlan, Probed, RESUME_REQUEST, SAMPLE_REQUEST, SUSPEND_REQUEST, WorkerFrame,
};

use crate::fans::{Access, Fans, Sample};

/// How long the worker waits for a sample request while a curve holds a fan
/// before it reads the sensors and steps by itself. The service asks once a
/// second, so this only fires when the service stalls.
pub const STEP_INTERVAL: Duration = Duration::from_secs(2);

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
    /// What the board's catalog entry and the build allow for the fans.
    pub fans: FanStatus,
    /// What opens a fan session. `None` unless the build has fan control and
    /// the board is validated; a plan then opens nothing.
    pub fan_access: Option<Access>,
}

/// What the reading thread passes on.
enum Request {
    Sample,
    Plan(FanPlan),
    Suspend,
    Resume,
    /// The input ended or stopped making sense.
    Closed,
}

fn read_requests(mut requests: impl Read, forward: Sender<Request>) {
    // One byte at a time on purpose: each request is handled before the next
    // is looked at.
    let mut byte = [0u8; 1];
    loop {
        let request = match requests.read(&mut byte) {
            Ok(0) | Err(_) => Request::Closed,
            Ok(_) => match byte[0] {
                SAMPLE_REQUEST => Request::Sample,
                // A plan that does not decode means the two sides disagree;
                // handing the fans back and ending is the safe answer.
                FANS_REQUEST => match read_frame::<_, FanPlan>(&mut requests) {
                    Ok(plan) => Request::Plan(plan),
                    Err(_) => Request::Closed,
                },
                SUSPEND_REQUEST => Request::Suspend,
                RESUME_REQUEST => Request::Resume,
                // Anything else is not from this version of the service.
                _ => continue,
            },
        };
        let closed = matches!(request, Request::Closed);
        if forward.send(request).is_err() || closed {
            return;
        }
    }
}

/// The worker's whole life: probe, report, then answer requests until
/// `requests` ends. Every fan is back with the BIOS when this returns.
pub fn run(
    probe: impl FnOnce() -> Machine,
    requests: impl Read + Send + 'static,
    frames: impl Write,
) -> Result<(), FrameError> {
    run_every(probe, requests, frames, STEP_INTERVAL)
}

/// [`run`] with the self-step interval given, so a test need not wait seconds.
pub fn run_every(
    probe: impl FnOnce() -> Machine,
    requests: impl Read + Send + 'static,
    mut frames: impl Write,
    step_interval: Duration,
) -> Result<(), FrameError> {
    let machine = probe();
    let mut hub = Hub::new(machine.providers, 0);
    let keys: Vec<String> = hub.list().sensors.iter().map(|sensor| sensor.key.clone()).collect();
    let mut fans = Fans::new(machine.fan_access);
    let probed = Probed {
        driver: machine.driver,
        features: machine.features,
        system: machine.system,
        sensors: hub.list().clone(),
        fans: machine.fans,
    };
    write_frame(&mut frames, &WorkerFrame::Probed(Box::new(probed)))?;

    let (forward, incoming) = mpsc::channel();
    // Detached on purpose: it ends when the input does, and nothing waits
    // for it, because the input may stay open while a write has failed.
    std::thread::spawn(move || read_requests(requests, forward));

    let outcome = serve(&mut hub, &keys, &mut fans, &mut frames, &incoming, step_interval);
    // Whatever ended the loop, the fans go back first.
    fans.stop();
    outcome
}

fn serve(
    hub: &mut Hub,
    keys: &[String],
    fans: &mut Fans,
    frames: &mut impl Write,
    incoming: &Receiver<Request>,
    step_interval: Duration,
) -> Result<(), FrameError> {
    loop {
        let request = if fans.needs_steps() {
            match incoming.recv_timeout(step_interval) {
                Ok(request) => request,
                Err(RecvTimeoutError::Timeout) => {
                    let values = hub.sample().to_vec();
                    fans.tick(&Sample { keys, values: &values, at: Instant::now() });
                    send_changes(fans, frames)?;
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
        } else {
            match incoming.recv() {
                Ok(request) => request,
                Err(_) => return Ok(()),
            }
        };
        match request {
            Request::Closed => return Ok(()),
            Request::Sample => {
                let values = hub.sample().to_vec();
                write_frame(frames, &WorkerFrame::Sample(values.clone()))?;
                fans.tick(&Sample { keys, values: &values, at: Instant::now() });
                send_changes(fans, frames)?;
            }
            Request::Plan(plan) => {
                fans.set_plan(plan);
                send_changes(fans, frames)?;
            }
            Request::Suspend => {
                fans.suspend();
                send_changes(fans, frames)?;
            }
            Request::Resume => fans.resume(),
        }
    }
}

fn send_changes(fans: &mut Fans, frames: &mut impl Write) -> Result<(), FrameError> {
    match fans.take_changes() {
        Some(runtime) => write_frame(frames, &WorkerFrame::Fans(runtime)),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use syspilot_hw::FakeProvider;
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
            fans: FanStatus::default(),
            fan_access: None,
        }
    }

    #[test]
    fn worker_reports_first_then_answers_each_request_with_a_sample() {
        let requests = Cursor::new(vec![SAMPLE_REQUEST, b'?', SAMPLE_REQUEST]);
        let mut output = Vec::new();
        run(fake_machine, requests, &mut output).unwrap();

        let mut frames = Cursor::new(output);
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
        let mut frames = Cursor::new(output);
        assert!(matches!(read_frame(&mut frames).unwrap(), WorkerFrame::Probed(_)));
        assert!(read_frame::<_, WorkerFrame>(&mut frames).is_err());
    }

    #[test]
    fn the_probe_report_carries_the_fan_status_the_machine_decided() {
        let mut machine = fake_machine();
        machine.fans = FanStatus { board: "ACME BOARD".into(), ..FanStatus::default() };
        let mut output = Vec::new();
        run(move || machine, std::io::empty(), &mut output).unwrap();
        let WorkerFrame::Probed(probed) = read_frame(&mut Cursor::new(output)).unwrap() else {
            panic!("(worker): expected the report as the first frame");
        };
        assert_eq!(probed.fans.board, "ACME BOARD");
    }

    #[test]
    fn a_plan_the_build_cannot_run_is_ignored_and_the_samples_still_flow() {
        // Without a fan access (read-only board, or a build without the
        // feature) a plan opens nothing and sends no fan frame.
        let curve = syspilot_proto::profile::FanCurve {
            channel: "board.pwm1".into(),
            enabled: true,
            sources: vec!["cpu.temp".into()],
            points: vec![
                syspilot_proto::profile::CurvePoint(30, 20),
                syspilot_proto::profile::CurvePoint(60, 60),
            ],
            hysteresis_c: 0,
            min_duty_pct: 0,
            ramp_pct_per_sec: 100,
        };
        let mut wire = vec![FANS_REQUEST];
        write_frame(&mut wire, &FanPlan { store_directory: "unused".into(), curves: vec![curve] })
            .unwrap();
        wire.extend([SUSPEND_REQUEST, RESUME_REQUEST, SAMPLE_REQUEST]);
        let mut output = Vec::new();
        run(fake_machine, Cursor::new(wire), &mut output).unwrap();

        let mut frames = Cursor::new(output);
        assert!(matches!(read_frame(&mut frames).unwrap(), WorkerFrame::Probed(_)));
        assert_eq!(
            read_frame::<_, WorkerFrame>(&mut frames).unwrap(),
            WorkerFrame::Sample(vec![Some(41.0), None])
        );
        assert!(read_frame::<_, WorkerFrame>(&mut frames).is_err());
    }

    /// The worker on a validated, replayed board: fan control, end to end.
    #[cfg(feature = "fan-control")]
    mod fan_control {
        use super::*;
        use crate::fan_fakes::{BIOS_MODE, CPU, ReplayHandles, curve, replay_access};
        use std::collections::VecDeque;
        use std::sync::{Arc, Mutex};
        use syspilot_proto::message::{FanPhase, FanRuntime};

        fn replay_machine(temperature: f64) -> (Machine, ReplayHandles) {
            let (access, handles) = replay_access();
            let mut machine = fake_machine();
            machine.providers =
                vec![Box::new(FakeProvider::temperatures(&[(CPU, Some(temperature))]))];
            machine.fan_access = Some(access);
            (machine, handles)
        }

        fn plan_bytes(enabled: bool) -> Vec<u8> {
            let mut wire = vec![FANS_REQUEST];
            let plan = FanPlan { store_directory: "unused".into(), curves: vec![curve(enabled)] };
            write_frame(&mut wire, &plan).unwrap();
            wire
        }

        fn fan_frames(output: Vec<u8>) -> Vec<Vec<FanRuntime>> {
            let mut frames = Cursor::new(output);
            let mut reports = Vec::new();
            while let Ok(frame) = read_frame::<_, WorkerFrame>(&mut frames) {
                if let WorkerFrame::Fans(runtime) = frame {
                    reports.push(runtime);
                }
            }
            reports
        }

        #[test]
        fn a_plan_then_samples_take_the_fan_and_the_end_of_input_gives_it_back() {
            let (machine, handles) = replay_machine(45.0);
            let mut wire = plan_bytes(true);
            wire.extend([SAMPLE_REQUEST; 3]);
            let mut output = Vec::new();
            run(move || machine, Cursor::new(wire), &mut output).unwrap();

            let reports = fan_frames(output);
            assert_eq!(reports.len(), 2, "(fan frames): expected opened, then manual: {reports:?}");
            assert_eq!(reports[0][0].phase, FanPhase::Bios);
            assert_eq!(
                (reports[1][0].phase, reports[1][0].target_duty_pct),
                (FanPhase::Manual, Some(40))
            );
            let writes = handles.fan_writes();
            assert!(writes.contains(&(0x109, 102)), "40 % of 255 was written: {writes:?}");
            // The input ended: the mode register is back with the BIOS.
            assert_eq!(writes.last(), Some(&(0x102, BIOS_MODE)), "{writes:?}");
        }

        #[test]
        fn suspend_hands_the_fan_back_and_says_so() {
            let (machine, handles) = replay_machine(45.0);
            let mut wire = plan_bytes(true);
            wire.extend([SAMPLE_REQUEST; 3]);
            wire.push(SUSPEND_REQUEST);
            wire.extend([SAMPLE_REQUEST; 2]);
            let mut output = Vec::new();
            run(move || machine, Cursor::new(wire), &mut output).unwrap();

            let reports = fan_frames(output);
            let last = reports.last().unwrap();
            assert_eq!(last[0].phase, FanPhase::Bios, "{reports:?}");
            let writes = handles.fan_writes();
            let restores = writes.iter().filter(|write| **write == (0x102, BIOS_MODE)).count();
            assert_eq!(restores, 1, "restored once, not again at the end: {writes:?}");
            assert!(!writes.contains(&(0x109, 0xFF)), "no full speed after a suspend: {writes:?}");
        }

        #[test]
        fn a_plan_without_curves_after_a_running_one_restores_the_registers() {
            let (machine, handles) = replay_machine(45.0);
            let mut wire = plan_bytes(true);
            wire.extend([SAMPLE_REQUEST; 3]);
            wire.extend(plan_bytes(false));
            let mut output = Vec::new();
            run(move || machine, Cursor::new(wire), &mut output).unwrap();
            let reports = fan_frames(output);
            assert_eq!(reports.last().unwrap(), &Vec::<FanRuntime>::new());
            assert_eq!(handles.fan_writes().last(), Some(&(0x102, BIOS_MODE)));
        }

        /// A request stream the test feeds while the worker runs.
        struct FedReader {
            chunks: std::sync::mpsc::Receiver<Vec<u8>>,
            pending: VecDeque<u8>,
        }

        impl Read for FedReader {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                if self.pending.is_empty() {
                    match self.chunks.recv() {
                        Ok(chunk) => self.pending.extend(chunk),
                        // The sender is gone: the input ended.
                        Err(_) => return Ok(0),
                    }
                }
                match (self.pending.pop_front(), buffer.first_mut()) {
                    (Some(byte), Some(slot)) => {
                        *slot = byte;
                        Ok(1)
                    }
                    _ => Ok(0),
                }
            }
        }

        /// Frames the worker wrote, readable while it still runs.
        #[derive(Clone, Default)]
        struct SharedFrames(Arc<Mutex<Vec<u8>>>);

        impl Write for SharedFrames {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        #[test]
        fn a_stalled_service_does_not_stall_the_curve_and_a_closed_pipe_restores() {
            let (machine, handles) = replay_machine(45.0);
            let (feed, chunks) = std::sync::mpsc::channel();
            let frames = SharedFrames::default();
            let worker = {
                let frames = frames.clone();
                std::thread::spawn(move || {
                    run_every(
                        move || machine,
                        FedReader { chunks, pending: VecDeque::new() },
                        frames,
                        Duration::from_millis(10),
                    )
                })
            };
            // One plan and then silence: the worker steps by itself.
            feed.send(plan_bytes(true)).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while !handles.fan_writes().contains(&(0x109, 102)) {
                assert!(
                    Instant::now() < deadline,
                    "(self step): expected the curve to take the fan without a sample request | \
                     received: {:?}",
                    handles.fan_writes()
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            // The service dies: its end of the pipe closes.
            drop(feed);
            worker.join().unwrap().unwrap();
            let writes = handles.fan_writes();
            assert_eq!(writes.last(), Some(&(0x102, BIOS_MODE)), "{writes:?}");
            let output = frames.0.lock().unwrap().clone();
            assert!(
                fan_frames(output).iter().any(|report| report[0].phase == FanPhase::Manual),
                "the manual phase reached the service even without a sample request"
            );
        }
    }

    #[test]
    fn a_plan_that_does_not_decode_ends_the_worker_instead_of_guessing() {
        let mut wire = vec![FANS_REQUEST];
        wire.extend(3u32.to_le_bytes());
        wire.extend([0xFF, 0xFF, 0xFF]);
        wire.push(SAMPLE_REQUEST);
        let mut output = Vec::new();
        run(fake_machine, Cursor::new(wire), &mut output).unwrap();
        let mut frames = Cursor::new(output);
        assert!(matches!(read_frame(&mut frames).unwrap(), WorkerFrame::Probed(_)));
        assert!(
            read_frame::<_, WorkerFrame>(&mut frames).is_err(),
            "(worker): expected no sample after a plan that did not decode"
        );
    }
}
