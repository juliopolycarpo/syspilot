//! SysPilot tuner.
//!
//! Started by the service, as LocalSystem, for one job: it reads one
//! [`TuneJob`] frame from its standard input, carries it out through the
//! vendor interfaces of this machine and writes one [`TuneReport`] frame to
//! its standard output. An empty job only reports. See
//! [`syspilot_proto::tuning`].
//!
//! It is a separate executable so that the resident service loads no vendor
//! library, and so that a driver call that hangs can be ended from outside:
//! the service stops waiting after a few seconds and terminates the process.
//!
//! It takes no arguments. Every value it writes goes through the vendor's
//! own interface and is checked first against the range that interface
//! reports; see `syspilot_hw::tuning`.

use std::io::{Read, Write};
use std::process::ExitCode;

use syspilot_hw::tuning::{TuningBackend, run, system_backends};
use syspilot_proto::frame::{FrameError, read_frame, write_frame};
use syspilot_proto::tuning::TuneJob;

/// One job: read it, do it, report.
///
/// The job is read before `backends` loads anything. The service writes the
/// job before it starts counting the time the tuner gets, so a vendor
/// library that hangs while loading must not keep the job from being taken.
fn serve(
    mut job: impl Read,
    mut report: impl Write,
    backends: impl FnOnce() -> Vec<Box<dyn TuningBackend>>,
) -> Result<(), FrameError> {
    let job: TuneJob = read_frame(&mut job)?;
    write_frame(&mut report, &run(&job, &mut backends()))
}

fn main() -> ExitCode {
    if std::env::args_os().len() > 1 {
        eprintln!(
            "(arguments): expected none | received: some. The service starts this program and \
             gives it its job on standard input."
        );
        return ExitCode::from(2);
    }
    // Standard output carries the report frame, so nothing else may print
    // there.
    let (job, report) = (std::io::stdin().lock(), std::io::stdout().lock());
    match serve(job, report, system_backends) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use syspilot_hw::tuning::FakeTuningBackend;
    use syspilot_proto::tuning::{
        TuneReport, TunedDevice, TuningControl, TuningKind, TuningSetting, TuningUnit,
    };

    const CARD: &str = "gpu.amd-699f.bus-3";

    fn fake_card() -> Vec<Box<dyn TuningBackend>> {
        let limit = TuningControl {
            key: "power-limit".into(),
            kind: TuningKind::PowerLimit,
            state: None,
            unit: TuningUnit::Percent,
            min: -75,
            max: 75,
            step: 1,
            default: 0,
            current: 0,
            wanted: None,
        };
        let card = TunedDevice {
            key: CARD.into(),
            name: "Radeon RX 550 Series".into(),
            interface: "Fake".into(),
            controls: vec![limit],
            problem: None,
        };
        vec![Box::new(FakeTuningBackend { devices: vec![card], ..FakeTuningBackend::default() })]
    }

    fn served(job: &TuneJob) -> TuneReport {
        let mut input = Vec::new();
        write_frame(&mut input, job).unwrap();
        let mut output = Vec::new();
        serve(Cursor::new(input), &mut output, fake_card).unwrap();
        let mut output = Cursor::new(output);
        let report = read_frame(&mut output).unwrap();
        assert!(
            read_frame::<_, TuneReport>(&mut output).is_err(),
            "(tuner output): expected one frame and nothing after it"
        );
        report
    }

    #[test]
    fn an_empty_job_is_answered_with_the_devices_as_they_are() {
        let report = served(&TuneJob::default());
        assert_eq!(report.devices.len(), 1);
        assert_eq!(report.devices[0].controls[0].current, 0);
    }

    #[test]
    fn a_setting_is_carried_out_and_the_report_shows_it() {
        let setting = TuningSetting {
            device: CARD.into(),
            enabled: true,
            values: [("power-limit".to_owned(), -20)].into(),
        };
        let report = served(&TuneJob { apply: vec![setting], reset: Vec::new() });
        assert_eq!(report.devices[0].controls[0].current, -20);
        assert_eq!(report.devices[0].problem, None);
    }

    #[test]
    fn without_a_job_nothing_is_done_and_nothing_is_reported() {
        // No vendor library is loaded for a job that never came.
        let never = || -> Vec<Box<dyn TuningBackend>> {
            panic!("(backends): expected none loaded without a job | received: a load")
        };
        let mut output = Vec::new();
        let outcome = serve(std::io::empty(), &mut output, never);
        assert!(matches!(outcome, Err(FrameError::Closed)), "{outcome:?}");
        assert!(output.is_empty(), "(tuner output): expected nothing | received: {output:?}");

        // Bytes that are no job end the same way: nothing is guessed.
        let garbage = [3u8, 0, 0, 0, 0xFF, 0xFF, 0xFF];
        assert!(serve(Cursor::new(garbage), &mut output, never).is_err());
        assert!(output.is_empty());
    }
}
