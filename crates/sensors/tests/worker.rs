//! The real worker executable, spoken to the way the service does.
//!
//! It reads this machine's sensors through Windows and the PawnIO driver;
//! nothing is written. With the `fan-control` cargo feature the executable can
//! write fan registers, and this test starts it on the real driver, so it does
//! not exist in that build: fan control is tested on the replay bus only.
#![cfg(not(feature = "fan-control"))]

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use syspilot_proto::frame::read_frame;
use syspilot_proto::worker::{SAMPLE_REQUEST, WorkerFrame};

#[test]
fn worker_reports_this_machine_answers_a_sample_and_exits_when_its_input_ends() {
    let mut worker = Command::new(env!("CARGO_BIN_EXE_syspilot-sensors"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("(worker): expected the executable to start");
    let mut requests = worker.stdin.take().expect("piped input");
    let mut frames = worker.stdout.take().expect("piped output");

    let WorkerFrame::Probed(probed) = read_frame(&mut frames).expect("a first frame") else {
        panic!("(worker): expected its report first | received: a sample");
    };
    let sensors = &probed.sensors.sensors;
    assert!(
        sensors.iter().any(|sensor| sensor.key == "cpu.load"),
        "(worker): expected at least the processor load sensor | received: {} sensors",
        sensors.len()
    );
    assert!(!probed.system.summary.os.is_empty(), "the Windows version is always known");
    let ids: Vec<u32> = sensors.iter().map(|sensor| sensor.id).collect();
    assert_eq!(ids, (0..sensors.len() as u32).collect::<Vec<_>>(), "IDs index the samples");

    requests.write_all(&[SAMPLE_REQUEST]).expect("a request written");
    requests.flush().expect("a request sent");
    let WorkerFrame::Sample(values) = read_frame(&mut frames).expect("an answer") else {
        panic!("(worker): expected a sample | received: a second report");
    };
    assert_eq!(values.len(), sensors.len(), "one value per sensor");
    assert!(values.iter().any(Option::is_some), "at least one sensor could be read");

    drop(requests);
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = worker.try_wait().expect("the worker's state") {
            break status;
        }
        if Instant::now() > deadline {
            let _ = worker.kill();
            panic!("(worker): expected it to exit when its input ended | received: still running");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success(), "(worker): expected exit code 0 | received: {status}");
}
