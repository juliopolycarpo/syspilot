//! What the service says about the fans.
//!
//! Three things make the status: what the board allows (the worker's probe),
//! what the profile asks (its curves) and what the controller is doing (the
//! worker's frames). [`merge`] joins them; [`Watch`] decides when a change is
//! worth telling clients about.

use syspilot_proto::message::{FanAvailability, FanChannelStatus, FanPhase, FanRuntime, FanStatus};
use syspilot_proto::profile::{FanCurve, Profile};

/// The curves the controller is to follow: the enabled ones.
pub fn enabled_curves(profile: &Profile) -> Vec<FanCurve> {
    profile.fans.iter().filter(|curve| curve.enabled).cloned().collect()
}

/// Joins the board's `layout`, the profile's `curves` and the controller's
/// `runtime` into the status clients see.
///
/// `fan_control` is whether this service accepts curves. A worker that was
/// built with fan control while the service was not never gets a plan, so
/// "controllable" would be a promise nobody keeps; it reads as not in the
/// build.
pub fn merge(
    layout: &FanStatus,
    curves: &[FanCurve],
    runtime: &[FanRuntime],
    fan_control: bool,
) -> FanStatus {
    let availability = match &layout.availability {
        FanAvailability::Controllable if !fan_control => FanAvailability::NotInBuild,
        other => other.clone(),
    };
    let controllable = availability == FanAvailability::Controllable;
    let curve_of = |channel: &str| curves.iter().find(|curve| curve.channel == channel);
    let runtime_of = |channel: &str| runtime.iter().find(|entry| entry.channel == channel);

    let fill = |mut status: FanChannelStatus| {
        if let Some(curve) = curve_of(&status.channel) {
            status.curve = curve.points.clone();
            status.sources = curve.sources.clone();
            status.active = controllable && curve.enabled;
        }
        // Only a controllable board has a controller whose word counts.
        if let Some(entry) = runtime_of(&status.channel).filter(|_| controllable) {
            status.phase = entry.phase;
            status.target_duty_pct = entry.target_duty_pct;
            status.problem = entry.problem.clone();
        }
        status
    };

    let mut channels: Vec<FanChannelStatus> = layout.channels.iter().cloned().map(fill).collect();
    // A curve on a channel the entry does not list is shown with what the
    // controller said about it, so the profile's mistake has a place to
    // appear. A board with no channels has nowhere to put them.
    if !layout.channels.is_empty() {
        for curve in curves {
            if layout.channels.iter().any(|known| known.channel == curve.channel) {
                continue;
            }
            channels.push(fill(FanChannelStatus {
                channel: curve.channel.clone(),
                label: curve.channel.clone(),
                phase: FanPhase::Bios,
                ..FanChannelStatus::default()
            }));
        }
    }
    FanStatus { availability, board: layout.board.clone(), channels }
}

/// Remembers what clients were last told about the controller and says
/// whether a new report changes anything they show.
///
/// The target duty is compared in steps of five points: a curve follows the
/// temperature by the degree, and a status event for every percent would
/// wake every client for nothing a person can read.
#[derive(Debug, Default)]
pub struct Watch {
    told: Vec<(String, FanPhase, Option<u8>, Option<String>)>,
}

/// Width of the steps in which a changing target counts as a change.
const TARGET_STEP: u8 = 5;

impl Watch {
    /// Takes `runtime` as the new report. `true` when it differs from the
    /// last one in a channel, its phase, its problem or its rounded target.
    pub fn changed(&mut self, runtime: &[FanRuntime]) -> bool {
        let seen: Vec<_> = runtime
            .iter()
            .map(|entry| {
                (
                    entry.channel.clone(),
                    entry.phase,
                    entry.target_duty_pct.map(|duty| duty / TARGET_STEP),
                    entry.problem.clone(),
                )
            })
            .collect();
        if seen == self.told {
            return false;
        }
        self.told = seen;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syspilot_proto::profile::CurvePoint;

    fn channel(pwm: u8, label: &str) -> FanChannelStatus {
        FanChannelStatus {
            channel: format!("board.pwm{pwm}"),
            label: label.into(),
            tach_sensor: Some(format!("board.fan{pwm}")),
            min_duty_pct: 20,
            ..FanChannelStatus::default()
        }
    }

    fn layout(availability: FanAvailability) -> FanStatus {
        FanStatus {
            availability,
            board: "ACME B550".into(),
            channels: vec![channel(1, "CPU fan"), channel(2, "Case fan")],
        }
    }

    fn curve(pwm: u8, enabled: bool) -> FanCurve {
        FanCurve {
            channel: format!("board.pwm{pwm}"),
            enabled,
            sources: vec!["cpu.package.temp".into()],
            points: vec![CurvePoint(30, 20), CurvePoint(80, 100)],
            hysteresis_c: 2,
            min_duty_pct: 0,
            ramp_pct_per_sec: 10,
        }
    }

    fn running(pwm: u8, phase: FanPhase, target: Option<u8>) -> FanRuntime {
        FanRuntime {
            channel: format!("board.pwm{pwm}"),
            phase,
            target_duty_pct: target,
            problem: None,
        }
    }

    #[test]
    fn a_controllable_board_shows_curve_state_and_target_per_channel() {
        let status = merge(
            &layout(FanAvailability::Controllable),
            &[curve(1, true)],
            &[running(1, FanPhase::Manual, Some(55))],
            true,
        );
        assert_eq!(status.availability, FanAvailability::Controllable);
        assert_eq!(status.board, "ACME B550");
        let cpu = &status.channels[0];
        assert_eq!(cpu.curve.len(), 2);
        assert_eq!(cpu.sources, ["cpu.package.temp"]);
        assert!(cpu.active);
        assert_eq!((cpu.phase, cpu.target_duty_pct), (FanPhase::Manual, Some(55)));
        let case = &status.channels[1];
        assert!(!case.active && case.curve.is_empty(), "no curve for the second channel");
        assert_eq!((case.phase, case.target_duty_pct), (FanPhase::Bios, None));
    }

    #[test]
    fn a_disabled_curve_is_shown_and_not_active() {
        let status = merge(&layout(FanAvailability::Controllable), &[curve(1, false)], &[], true);
        assert!(!status.channels[0].active);
        assert_eq!(status.channels[0].curve.len(), 2, "the curve is still drawn");
    }

    #[test]
    fn a_read_only_board_keeps_its_reason_and_ignores_any_controller_report() {
        let reason = "the board's entry has not been validated".to_owned();
        let read_only = FanAvailability::ReadOnly { reason: reason.clone() };
        let status = merge(
            &layout(read_only.clone()),
            &[curve(1, true)],
            &[running(1, FanPhase::Manual, Some(80))],
            true,
        );
        assert_eq!(status.availability, read_only);
        let cpu = &status.channels[0];
        assert!(!cpu.active, "a read-only board follows nothing");
        assert_eq!((cpu.phase, cpu.target_duty_pct), (FanPhase::Bios, None));
        assert_eq!(cpu.curve.len(), 2, "the curve is still shown");
    }

    #[test]
    fn a_service_without_the_feature_never_calls_a_board_controllable() {
        let status = merge(&layout(FanAvailability::Controllable), &[], &[], false);
        assert_eq!(status.availability, FanAvailability::NotInBuild);
        assert!(status.channels.iter().all(|channel| !channel.active));
    }

    #[test]
    fn a_curve_on_a_channel_the_entry_lacks_appears_with_the_controllers_problem() {
        let missing = FanRuntime {
            problem: Some("(fan curve): expected a channel of the board's entry".into()),
            ..running(9, FanPhase::Bios, None)
        };
        let status =
            merge(&layout(FanAvailability::Controllable), &[curve(9, true)], &[missing], true);
        assert_eq!(status.channels.len(), 3);
        let extra = &status.channels[2];
        assert_eq!(extra.channel, "board.pwm9");
        assert!(extra.problem.as_deref().unwrap().contains("board's entry"), "{extra:?}");
    }

    #[test]
    fn a_board_without_channels_has_nowhere_to_show_a_curve() {
        let none = FanStatus { channels: Vec::new(), ..layout(FanAvailability::Controllable) };
        let status = merge(&none, &[curve(1, true)], &[], true);
        assert!(status.channels.is_empty());
    }

    #[test]
    fn only_enabled_curves_go_to_the_controller() {
        let profile = Profile { fans: vec![curve(1, true), curve(2, false)], ..Profile::default() };
        let enabled = enabled_curves(&profile);
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0].channel, "board.pwm1");
    }

    #[test]
    fn the_watch_reports_phase_and_problem_changes_and_rounded_target_steps_only() {
        let mut watch = Watch::default();
        assert!(!watch.changed(&[]), "nothing reported, nothing changed");
        assert!(watch.changed(&[running(1, FanPhase::Bios, None)]), "the first report");
        assert!(!watch.changed(&[running(1, FanPhase::Bios, None)]));

        assert!(watch.changed(&[running(1, FanPhase::Manual, Some(40))]), "phase");
        assert!(!watch.changed(&[running(1, FanPhase::Manual, Some(41))]), "within a step");
        assert!(!watch.changed(&[running(1, FanPhase::Manual, Some(44))]), "within a step");
        assert!(watch.changed(&[running(1, FanPhase::Manual, Some(45))]), "a new step");

        let failing = FanRuntime {
            problem: Some("sensor lost".into()),
            ..running(1, FanPhase::FailSafe, Some(100))
        };
        assert!(watch.changed(std::slice::from_ref(&failing)), "fail-safe with its problem");
        let cleared = FanRuntime { problem: None, ..failing };
        assert!(watch.changed(&[cleared]), "the problem went away");
        assert!(watch.changed(&[]), "the controller stopped reporting");
    }
}
