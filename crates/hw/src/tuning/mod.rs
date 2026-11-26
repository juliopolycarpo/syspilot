//! Hardware tuning: changing what a device's own driver lets a program
//! change (power limit, clocks, voltages), within the ranges that driver
//! reports.
//!
//! A [`TuningBackend`] is one vendor interface. Today there is one,
//! [`radeon`], for AMD graphics cards through the driver's ADL library; a
//! backend for another vendor or for a processor implements the same trait
//! and is added to [`system_backends`]. [`run`] carries out one
//! [`TuneJob`] over a set of backends and reports every device with the
//! values then in effect.
//!
//! Nothing here talks to PawnIO or to a register: every write goes through
//! the vendor's documented interface, which checks it again.

pub mod adl;
pub mod radeon;

use std::collections::BTreeMap;

use syspilot_proto::tuning::{TuneJob, TuneReport, TunedDevice};

/// One vendor interface for tuning.
pub trait TuningBackend {
    /// Every device this interface reaches, with its controls and the
    /// values in effect now.
    fn devices(&mut self) -> Vec<TunedDevice>;

    /// Puts `values` in effect on the device with `key`, and the default on
    /// every control `values` does not list. Nothing is written when a value
    /// does not fit.
    fn apply(&mut self, key: &str, values: &BTreeMap<String, i32>) -> Result<(), String>;

    /// Puts every control of the device with `key` back to its default:
    /// a setting with no values.
    fn reset(&mut self, key: &str) -> Result<(), String> {
        self.apply(key, &BTreeMap::new())
    }
}

/// A borrowed backend is a backend, so one that somebody else keeps can be
/// handed to [`run`].
impl<T: TuningBackend + ?Sized> TuningBackend for &mut T {
    fn devices(&mut self) -> Vec<TunedDevice> {
        (**self).devices()
    }

    fn apply(&mut self, key: &str, values: &BTreeMap<String, i32>) -> Result<(), String> {
        (**self).apply(key, values)
    }

    // Forwarded too: the default would bypass a backend's own reset.
    fn reset(&mut self, key: &str) -> Result<(), String> {
        (**self).reset(key)
    }
}

/// The tuning interfaces of this machine. A machine without the vendor's
/// driver has none, which is not a problem to report.
pub fn system_backends() -> Vec<Box<dyn TuningBackend>> {
    let mut backends: Vec<Box<dyn TuningBackend>> = Vec::new();
    if let Ok(library) = adl::AdlLibrary::load() {
        backends.push(Box::new(radeon::Radeon::new(library)));
    }
    backends
}

/// Carries out `job` and reports every device.
///
/// Resets come first, so a device that is both reset and given a setting
/// ends with the setting. A setting or a reset for a device nobody has is
/// skipped: the report then lacks the device, which is how the caller learns
/// it is not there. A setting that is not enabled is skipped too.
pub fn run(job: &TuneJob, backends: &mut [Box<dyn TuningBackend + '_>]) -> TuneReport {
    let mut problems: BTreeMap<String, String> = BTreeMap::new();
    // A look only reads, so it skips the reading this needs to find devices.
    if !job.is_look() {
        for backend in backends.iter_mut() {
            let known: Vec<String> =
                backend.devices().into_iter().map(|device| device.key).collect();
            for key in job.reset.iter().filter(|key| known.contains(key)) {
                if let Err(problem) = backend.reset(key) {
                    problems.insert(key.clone(), problem);
                }
            }
            let wanted = job.apply.iter().filter(|setting| setting.enabled);
            for setting in wanted.filter(|setting| known.contains(&setting.device)) {
                match backend.apply(&setting.device, &setting.values) {
                    Ok(()) => _ = problems.remove(&setting.device),
                    Err(problem) => _ = problems.insert(setting.device.clone(), problem),
                }
            }
        }
    }
    let mut devices: Vec<TunedDevice> =
        backends.iter_mut().flat_map(|backend| backend.devices()).collect();
    for device in &mut devices {
        // What the job did outranks what reading the device back says.
        if let Some(problem) = problems.remove(&device.key) {
            device.problem = Some(problem);
        }
    }
    TuneReport { devices }
}

/// A scripted backend: devices held in memory, with every call recorded.
#[derive(Debug, Default)]
pub struct FakeTuningBackend {
    /// The devices, as [`TuningBackend::devices`] reports them.
    pub devices: Vec<TunedDevice>,
    /// Keys of devices whose apply and reset fail, with the reason.
    pub failing: BTreeMap<String, String>,
    /// Every apply, in order, with the device key and the values. A reset is
    /// an apply with no values.
    pub calls: Vec<(String, BTreeMap<String, i32>)>,
}

impl FakeTuningBackend {
    fn device(&mut self, key: &str) -> Result<&mut TunedDevice, String> {
        if let Some(problem) = self.failing.get(key) {
            return Err(problem.clone());
        }
        self.devices
            .iter_mut()
            .find(|device| device.key == key)
            .ok_or_else(|| format!("(tuning device): expected a known device | received: {key:?}"))
    }
}

impl TuningBackend for FakeTuningBackend {
    fn devices(&mut self) -> Vec<TunedDevice> {
        self.devices.clone()
    }

    fn apply(&mut self, key: &str, values: &BTreeMap<String, i32>) -> Result<(), String> {
        self.calls.push((key.to_owned(), values.clone()));
        let device = self.device(key)?;
        syspilot_proto::tuning::check_values(&device.controls, values)
            .map_err(|problem| problem.to_string())?;
        for control in &mut device.controls {
            control.current = control.value_in(values);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syspilot_proto::tuning::{TuningControl, TuningKind, TuningSetting, TuningUnit};

    const CARD: &str = "gpu.amd-699f.bus-3";
    const OTHER: &str = "gpu.amd-67df.bus-4";

    fn power_limit(current: i32) -> TuningControl {
        TuningControl {
            key: "power-limit".into(),
            kind: TuningKind::PowerLimit,
            state: None,
            unit: TuningUnit::Percent,
            min: -75,
            max: 75,
            step: 1,
            default: 0,
            current,
            wanted: None,
        }
    }

    fn card(key: &str, current: i32) -> TunedDevice {
        TunedDevice {
            key: key.into(),
            name: "Radeon".into(),
            interface: "Fake".into(),
            controls: vec![power_limit(current)],
            problem: None,
        }
    }

    fn setting(device: &str, limit: i32) -> TuningSetting {
        TuningSetting {
            device: device.into(),
            enabled: true,
            values: [("power-limit".to_owned(), limit)].into(),
        }
    }

    fn backends(devices: Vec<TunedDevice>) -> Vec<Box<dyn TuningBackend>> {
        vec![Box::new(FakeTuningBackend { devices, ..FakeTuningBackend::default() })]
    }

    fn limit_of(report: &TuneReport, key: &str) -> i32 {
        let device = report.devices.iter().find(|device| device.key == key);
        device
            .unwrap_or_else(|| panic!("(report): expected the device {key} | received: none"))
            .controls[0]
            .current
    }

    #[test]
    fn an_empty_job_reports_every_device_and_changes_nothing() {
        let mut backends = backends(vec![card(CARD, -10), card(OTHER, 0)]);
        let report = run(&TuneJob::default(), &mut backends);
        assert_eq!(report.devices.len(), 2);
        assert_eq!((limit_of(&report, CARD), limit_of(&report, OTHER)), (-10, 0));
    }

    #[test]
    fn a_setting_is_applied_and_the_report_shows_the_values_then_in_effect() {
        let mut backends = backends(vec![card(CARD, 0), card(OTHER, 5)]);
        let job = TuneJob { apply: vec![setting(CARD, -20)], reset: vec![OTHER.into()] };
        let report = run(&job, &mut backends);
        assert_eq!(limit_of(&report, CARD), -20);
        assert_eq!(limit_of(&report, OTHER), 0, "the other card went back to its default");
        assert!(
            report.devices.iter().all(|device| device.problem.is_none()),
            "(problems): expected none | received: {report:?}"
        );
    }

    #[test]
    fn a_device_that_is_both_reset_and_set_ends_with_the_setting() {
        let mut backends = backends(vec![card(CARD, 30)]);
        let job = TuneJob { apply: vec![setting(CARD, -5)], reset: vec![CARD.into()] };
        assert_eq!(limit_of(&run(&job, &mut backends), CARD), -5);
    }

    #[test]
    fn a_value_that_does_not_fit_changes_nothing_and_is_the_devices_problem() {
        let mut backends = backends(vec![card(CARD, 7)]);
        let report = run(&TuneJob { apply: vec![setting(CARD, 90)], reset: vec![] }, &mut backends);
        assert_eq!(limit_of(&report, CARD), 7);
        assert_eq!(
            report.devices[0].problem.as_deref(),
            Some("(tuning.values.power-limit): expected -75..=75 | received: 90")
        );
    }

    #[test]
    fn a_setting_for_a_device_nobody_has_and_one_that_is_off_are_skipped() {
        let off = TuningSetting { enabled: false, ..setting(CARD, -20) };
        let job = TuneJob {
            apply: vec![setting("gpu.amd-0000.bus-9", -20), off],
            reset: vec!["gpu.amd-0000.bus-9".into()],
        };
        let mut backends = backends(vec![card(CARD, 0)]);
        let report = run(&job, &mut backends);
        assert_eq!(report.devices.len(), 1, "the report lists what exists");
        assert_eq!(limit_of(&report, CARD), 0, "a setting that is off is not applied");
    }

    #[test]
    fn a_backend_that_fails_names_the_reason_on_its_device() {
        let failing =
            [(CARD.to_owned(), "(driver): expected ADL_OK (0) | received: -1".to_owned())];
        let mut backends: Vec<Box<dyn TuningBackend>> = vec![Box::new(FakeTuningBackend {
            devices: vec![card(CARD, 0)],
            failing: failing.into(),
            ..FakeTuningBackend::default()
        })];
        let report =
            run(&TuneJob { apply: vec![setting(CARD, -20)], reset: vec![] }, &mut backends);
        assert_eq!(
            report.devices[0].problem.as_deref(),
            Some("(driver): expected ADL_OK (0) | received: -1")
        );
    }

    #[test]
    fn a_machine_without_tuning_interfaces_reports_no_device() {
        assert_eq!(run(&TuneJob::default(), &mut []), TuneReport::default());
    }

    #[test]
    fn the_fake_backend_records_what_it_was_asked() {
        let mut backend =
            FakeTuningBackend { devices: vec![card(CARD, 0)], ..FakeTuningBackend::default() };
        backend.reset(CARD).unwrap();
        backend.apply(CARD, &[("power-limit".to_owned(), 3)].into()).unwrap();
        let unknown = backend.apply("gpu.none", &BTreeMap::new());
        assert!(unknown.is_err(), "(gpu.none): expected an error | received: {unknown:?}");
        let keys: Vec<_> =
            backend.calls.iter().map(|(key, values)| (key.as_str(), values.len())).collect();
        assert_eq!(keys, [(CARD, 0), (CARD, 1), ("gpu.none", 0)]);
    }

    #[test]
    fn a_borrowed_backend_keeps_what_the_job_did() {
        let mut kept =
            FakeTuningBackend { devices: vec![card(CARD, 0)], ..FakeTuningBackend::default() };
        let job = TuneJob { apply: vec![setting(CARD, -20)], reset: vec![] };
        let report = run(&job, &mut [Box::new(&mut kept)]);
        assert_eq!(limit_of(&report, CARD), -20);
        assert_eq!(kept.devices[0].controls[0].current, -20);
        assert_eq!(kept.calls.len(), 1);
    }

    /// A backend with a reset of its own, which a borrow must not bypass.
    #[derive(Default)]
    struct OwnResetBackend {
        resets: Vec<String>,
    }

    impl TuningBackend for OwnResetBackend {
        fn devices(&mut self) -> Vec<TunedDevice> {
            vec![card(CARD, 0)]
        }

        fn apply(&mut self, key: &str, _values: &BTreeMap<String, i32>) -> Result<(), String> {
            Err(format!("(apply): expected no call for a reset | received: one for {key}"))
        }

        fn reset(&mut self, key: &str) -> Result<(), String> {
            self.resets.push(key.to_owned());
            Ok(())
        }
    }

    #[test]
    fn a_borrowed_backend_resets_with_its_own_reset() {
        let mut own = OwnResetBackend::default();
        let report =
            run(&TuneJob { apply: vec![], reset: vec![CARD.into()] }, &mut [Box::new(&mut own)]);
        assert_eq!(own.resets, [CARD], "(resets): expected the backend's own reset");
        assert_eq!(report.devices[0].problem, None, "(problem): expected none");
    }
}
