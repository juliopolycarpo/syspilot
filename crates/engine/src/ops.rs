//! Detect, capture, apply and revert for each action kind.
//!
//! These functions are stateless: they translate one [`Action`] into calls
//! on the [`Os`] traits. Deciding *when* to call them is the enforcer's job.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use syspilot_proto::action::{
    Action, CommandAction, PowerAction, RegData, RegistryAction, RunAs, Scope, ServiceAction,
    StartType,
};

use crate::os::{Hive, Os, OsError, OsErrorKind, PowerValues, RegValue, ServiceStart};

/// Registry key under which Windows stores service configuration.
const SERVICES_KEY: &str = r"SYSTEM\CurrentControlSet\Services";
/// Registry key under which Windows stores power scheme values.
const POWER_SCHEMES_KEY: &str = r"SYSTEM\CurrentControlSet\Control\Power\User\PowerSchemes";

/// Whose state an action is evaluated against.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Target {
    /// Machine-wide state.
    Machine,
    /// The state of one logged-on user, by SID.
    User(String),
}

/// What [`detect`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detection {
    /// The system holds the desired state.
    Compliant {
        /// Current value in display form.
        current: String,
    },
    /// The system holds another state.
    Different {
        /// Current value in display form.
        current: String,
    },
    /// The action has no way to check (a command without a check command).
    Unverifiable,
    /// The target does not exist on this system.
    NotApplicable {
        /// What is missing.
        reason: String,
    },
}

/// The state an action replaced, kept so that a revert is exact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Snapshot {
    /// Original registry state.
    #[serde(rename_all = "kebab-case")]
    Registry {
        /// The original value, or `None` when it did not exist.
        value: Option<RegValue>,
        /// The shallowest key on the path that did not exist. Revert
        /// removes the keys the apply created, from the leaf up to this one.
        first_missing_key: Option<String>,
    },
    /// Original service start type.
    Service {
        /// The start type.
        start: ServiceStart,
    },
    /// Original power value indexes.
    Power(PowerValues),
    /// Commands have no state to capture.
    Command,
}

/// A registry key whose changes signal that an action may have drifted.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WatchSpec {
    /// Hive of the key.
    pub hive: Hive,
    /// Key path. May not exist yet; the caller watches the nearest existing
    /// ancestor in that case.
    pub key: String,
    /// Whether changes anywhere below the key count.
    pub subtree: bool,
}

fn hive_for(action: &RegistryAction, target: &Target) -> Hive {
    match (action.scope, target) {
        (Scope::User, Target::User(sid)) => Hive::User(sid.clone()),
        // A per-user action is only ever evaluated with a user target; the
        // machine hive is the safe reading of any other combination.
        _ => Hive::Machine,
    }
}

fn service_start(start: StartType) -> ServiceStart {
    match start {
        StartType::Automatic => ServiceStart::Automatic,
        StartType::AutomaticDelayed => ServiceStart::AutomaticDelayed,
        StartType::Manual => ServiceStart::Manual,
        StartType::Disabled => ServiceStart::Disabled,
    }
}

fn describe_value(value: Option<&RegValue>) -> String {
    value.map_or_else(|| RegData::Absent.to_string(), RegValue::describe)
}

fn describe_power(values: PowerValues) -> String {
    format!("AC {}, DC {}", values.ac, values.dc)
}

fn judge(compliant: bool, current: String) -> Detection {
    if compliant { Detection::Compliant { current } } else { Detection::Different { current } }
}

/// Reads the current state and compares it with the desired one.
pub fn detect(os: &Os, action: &Action, target: &Target) -> Result<Detection, OsError> {
    match action {
        Action::Registry(action) => {
            let value = os.registry.read(&hive_for(action, target), &action.key, &action.name)?;
            let compliant = match (&value, &action.data) {
                (None, RegData::Absent) => true,
                (Some(value), desired) => value.matches(desired),
                (None, _) => false,
            };
            Ok(judge(compliant, describe_value(value.as_ref())))
        }
        Action::Service(action) => match os.services.start_type(&action.service)? {
            None => Ok(Detection::NotApplicable {
                reason: format!("service {} is not installed", action.service),
            }),
            Some(ServiceStart::Boot | ServiceStart::System) => Ok(Detection::NotApplicable {
                reason: format!("{} is a boot-start driver and is not managed", action.service),
            }),
            Some(start) => Ok(judge(start == service_start(action.start), start.to_string())),
        },
        Action::Power(action) => match os.power.read(&action.subgroup, &action.setting)? {
            None => Ok(Detection::NotApplicable {
                reason: "the active power plan does not have this setting".into(),
            }),
            Some(values) => {
                let compliant = action.ac.is_none_or(|ac| ac == values.ac)
                    && action.dc.is_none_or(|dc| dc == values.dc);
                Ok(judge(compliant, describe_power(values)))
            }
        },
        Action::Command(action) => {
            let Some(check) = &action.check else { return Ok(Detection::Unverifiable) };
            let exit = os.launcher.run(&check.command, action.run_as, timeout_of(action))?;
            Ok(judge(exit == check.expect_exit, format!("check exit code {exit}")))
        }
    }
}

/// Records the state [`apply`] is about to replace.
pub fn capture(os: &Os, action: &Action, target: &Target) -> Result<Snapshot, OsError> {
    match action {
        Action::Registry(action) => {
            let hive = hive_for(action, target);
            let value = os.registry.read(&hive, &action.key, &action.name)?;
            let first_missing_key = first_missing_key(os, &hive, &action.key)?;
            Ok(Snapshot::Registry { value, first_missing_key })
        }
        Action::Service(action) => match os.services.start_type(&action.service)? {
            Some(start) => Ok(Snapshot::Service { start }),
            None => Err(OsError::new(
                OsErrorKind::NotFound,
                format!("query service {}", action.service),
                0,
            )),
        },
        Action::Power(action) => match os.power.read(&action.subgroup, &action.setting)? {
            Some(values) => Ok(Snapshot::Power(values)),
            None => Err(OsError::new(
                OsErrorKind::NotFound,
                format!("read power setting {}/{}", action.subgroup, action.setting),
                0,
            )),
        },
        Action::Command(_) => Ok(Snapshot::Command),
    }
}

/// Brings the system into the desired state.
pub fn apply(os: &Os, action: &Action, target: &Target) -> Result<(), OsError> {
    match action {
        Action::Registry(action) => {
            let hive = hive_for(action, target);
            match RegValue::from_data(&action.data) {
                Some(value) => os.registry.write(&hive, &action.key, &action.name, &value),
                None => os.registry.delete_value(&hive, &action.key, &action.name),
            }
        }
        Action::Service(ServiceAction { service, start }) => {
            os.services.set_start_type(service, service_start(*start))
        }
        Action::Power(PowerAction { subgroup, setting, ac, dc }) => {
            os.power.write(subgroup, setting, *ac, *dc)
        }
        Action::Command(action) => run_expecting_success(os, action, &action.apply),
    }
}

/// Restores the state captured before the first apply.
pub fn revert(
    os: &Os,
    action: &Action,
    target: &Target,
    snapshot: &Snapshot,
) -> Result<(), OsError> {
    match (action, snapshot) {
        (Action::Registry(action), Snapshot::Registry { value, first_missing_key }) => {
            let hive = hive_for(action, target);
            match value {
                Some(value) => os.registry.write(&hive, &action.key, &action.name, value)?,
                None => os.registry.delete_value(&hive, &action.key, &action.name)?,
            }
            if let Some(root) = first_missing_key {
                remove_created_keys(os, &hive, &action.key, root)?;
            }
            Ok(())
        }
        (Action::Service(action), Snapshot::Service { start }) => {
            os.services.set_start_type(&action.service, *start)
        }
        (Action::Power(action), Snapshot::Power(values)) => {
            // Only the sources the action manages are restored, so a change
            // the user made to the other one in the meantime survives.
            let ac = action.ac.map(|_| values.ac);
            let dc = action.dc.map(|_| values.dc);
            os.power.write(&action.subgroup, &action.setting, ac, dc)
        }
        (Action::Command(action), Snapshot::Command) => match &action.undo {
            Some(undo) => run_expecting_success(os, action, undo),
            None => Ok(()),
        },
        _ => Err(OsError::new(
            OsErrorKind::Other,
            format!("revert {}: the stored snapshot is for another action kind", action.describe()),
            0,
        )),
    }
}

/// The registry key to watch for changes to this action's state, if any.
pub fn watch_for(action: &Action, target: &Target) -> Option<WatchSpec> {
    match action {
        Action::Registry(action) => Some(WatchSpec {
            hive: hive_for(action, target),
            key: action.key.clone(),
            subtree: false,
        }),
        Action::Service(action) => Some(WatchSpec {
            hive: Hive::Machine,
            key: format!(r"{SERVICES_KEY}\{}", action.service),
            subtree: false,
        }),
        Action::Power(_) => {
            Some(WatchSpec { hive: Hive::Machine, key: POWER_SCHEMES_KEY.into(), subtree: true })
        }
        Action::Command(_) => None,
    }
}

/// The deepest ancestor of `key` (or `key` itself) that exists, if any.
pub fn nearest_existing_key(os: &Os, hive: &Hive, key: &str) -> Result<Option<String>, OsError> {
    let mut candidate = key;
    loop {
        if os.registry.key_exists(hive, candidate)? {
            return Ok(Some(candidate.to_owned()));
        }
        match candidate.rsplit_once('\\') {
            Some((parent, _)) => candidate = parent,
            None => return Ok(None),
        }
    }
}

fn first_missing_key(os: &Os, hive: &Hive, key: &str) -> Result<Option<String>, OsError> {
    let mut end = 0;
    for part in key.split('\\') {
        end += part.len();
        let prefix = &key[..end];
        if !os.registry.key_exists(hive, prefix)? {
            return Ok(Some(prefix.to_owned()));
        }
        end += 1; // the separator
    }
    Ok(None)
}

/// Deletes the keys from `leaf` up to and including `root`, stopping at the
/// first one that is not empty (someone else put something there).
fn remove_created_keys(os: &Os, hive: &Hive, leaf: &str, root: &str) -> Result<(), OsError> {
    let mut key = leaf;
    while key.len() >= root.len() {
        if !os.registry.delete_key_if_empty(hive, key)? {
            break;
        }
        match key.rsplit_once('\\') {
            Some((parent, _)) => key = parent,
            None => break,
        }
    }
    Ok(())
}

fn timeout_of(action: &CommandAction) -> Duration {
    Duration::from_secs(u64::from(action.timeout_secs))
}

fn run_expecting_success(
    os: &Os,
    action: &CommandAction,
    command: &syspilot_proto::action::CommandLine,
) -> Result<(), OsError> {
    match os.launcher.run(command, action.run_as, timeout_of(action))? {
        0 => Ok(()),
        exit => Err(OsError::new(
            OsErrorKind::Other,
            format!("run {command}: expected exit code 0 | received: {exit}"),
            0,
        )),
    }
}

/// Whether the action needs a logged-on user to be evaluated.
pub fn is_per_user(action: &Action) -> bool {
    match action {
        Action::Registry(action) => action.scope == Scope::User,
        Action::Command(action) => action.run_as == RunAs::User,
        Action::Service(_) | Action::Power(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeOs;
    use crate::os::{Registry, reg_type};
    use syspilot_proto::action::{CommandCheck, CommandLine};

    const KEY: &str = r"SOFTWARE\SysPilot-Test\Ops";
    const SUBGROUP: &str = "54533251-82be-4824-96c1-47b60b740d00";
    const SETTING: &str = "0cc5b647-c1df-4637-891a-dec35c318583";
    const SID: &str = "S-1-5-21-1000";

    fn registry_action(scope: Scope, data: RegData) -> Action {
        Action::Registry(RegistryAction { scope, key: KEY.into(), name: "Value".into(), data })
    }

    fn dword(value: u32) -> RegValue {
        RegValue { kind: reg_type::DWORD, data: value.to_le_bytes().to_vec() }
    }

    fn command_action(check: bool, undo: bool) -> Action {
        let line = |name: &str| CommandLine { program: format!(r"C:\{name}.exe"), args: vec![] };
        Action::Command(CommandAction {
            apply: line("apply"),
            run_as: RunAs::System,
            timeout_secs: 30,
            check: check.then(|| CommandCheck { command: line("check"), expect_exit: 0 }),
            undo: undo.then(|| line("undo")),
        })
    }

    #[test]
    fn registry_value_goes_through_detect_apply_revert() {
        let fake = FakeOs::default();
        let os = fake.os();
        let action = registry_action(Scope::Machine, RegData::Dword(1));
        fake.registry.set_external(&Hive::Machine, KEY, "Value", Some(dword(7)));

        let detection = detect(&os, &action, &Target::Machine).unwrap();
        assert_eq!(detection, Detection::Different { current: "dword:7".into() });

        let snapshot = capture(&os, &action, &Target::Machine).unwrap();
        apply(&os, &action, &Target::Machine).unwrap();
        let detection = detect(&os, &action, &Target::Machine).unwrap();
        assert_eq!(detection, Detection::Compliant { current: "dword:1".into() });

        revert(&os, &action, &Target::Machine, &snapshot).unwrap();
        assert_eq!(fake.registry.value(&Hive::Machine, KEY, "Value"), Some(dword(7)));
    }

    #[test]
    fn revert_removes_the_value_and_keys_that_did_not_exist() {
        let fake = FakeOs::default();
        let os = fake.os();
        fake.registry.create_key(&Hive::Machine, "SOFTWARE");
        let action = registry_action(Scope::Machine, RegData::Sz("on".into()));

        let snapshot = capture(&os, &action, &Target::Machine).unwrap();
        assert_eq!(
            snapshot,
            Snapshot::Registry {
                value: None,
                first_missing_key: Some(r"SOFTWARE\SysPilot-Test".into())
            }
        );
        apply(&os, &action, &Target::Machine).unwrap();
        assert!(fake.registry.has_key(&Hive::Machine, KEY));

        revert(&os, &action, &Target::Machine, &snapshot).unwrap();
        assert!(!fake.registry.has_key(&Hive::Machine, r"SOFTWARE\SysPilot-Test"));
        assert!(fake.registry.has_key(&Hive::Machine, "SOFTWARE"), "pre-existing parent stays");
    }

    #[test]
    fn revert_keeps_created_keys_that_someone_else_filled() {
        let fake = FakeOs::default();
        let os = fake.os();
        let action = registry_action(Scope::Machine, RegData::Dword(1));
        let snapshot = capture(&os, &action, &Target::Machine).unwrap();
        apply(&os, &action, &Target::Machine).unwrap();
        fake.registry.set_external(
            &Hive::Machine,
            r"SOFTWARE\SysPilot-Test",
            "Other",
            Some(dword(5)),
        );

        revert(&os, &action, &Target::Machine, &snapshot).unwrap();
        assert!(!fake.registry.has_key(&Hive::Machine, KEY), "the empty leaf is removed");
        assert!(fake.registry.has_key(&Hive::Machine, r"SOFTWARE\SysPilot-Test"));
    }

    #[test]
    fn absent_is_a_desired_state() {
        let fake = FakeOs::default();
        let os = fake.os();
        let action = registry_action(Scope::Machine, RegData::Absent);
        let detection = detect(&os, &action, &Target::Machine).unwrap();
        assert_eq!(detection, Detection::Compliant { current: "(absent)".into() });

        fake.registry.set_external(&Hive::Machine, KEY, "Value", Some(dword(1)));
        let snapshot = capture(&os, &action, &Target::Machine).unwrap();
        apply(&os, &action, &Target::Machine).unwrap();
        assert_eq!(fake.registry.value(&Hive::Machine, KEY, "Value"), None);
        revert(&os, &action, &Target::Machine, &snapshot).unwrap();
        assert_eq!(fake.registry.value(&Hive::Machine, KEY, "Value"), Some(dword(1)));
    }

    #[test]
    fn per_user_action_targets_that_users_hive() {
        let fake = FakeOs::default();
        let os = fake.os();
        let action = registry_action(Scope::User, RegData::Dword(0));
        apply(&os, &action, &Target::User(SID.into())).unwrap();
        assert_eq!(fake.registry.value(&Hive::User(SID.into()), KEY, "Value"), Some(dword(0)));
        assert_eq!(fake.registry.value(&Hive::Machine, KEY, "Value"), None);
        assert!(is_per_user(&action));
    }

    #[test]
    fn service_start_type_goes_through_detect_apply_revert() {
        let fake = FakeOs::default();
        let os = fake.os();
        fake.services.install("DiagTrack", ServiceStart::Automatic);
        let action = Action::Service(ServiceAction {
            service: "DiagTrack".into(),
            start: StartType::Disabled,
        });

        let detection = detect(&os, &action, &Target::Machine).unwrap();
        assert_eq!(detection, Detection::Different { current: "automatic".into() });
        let snapshot = capture(&os, &action, &Target::Machine).unwrap();
        apply(&os, &action, &Target::Machine).unwrap();
        let detection = detect(&os, &action, &Target::Machine).unwrap();
        assert_eq!(detection, Detection::Compliant { current: "disabled".into() });
        revert(&os, &action, &Target::Machine, &snapshot).unwrap();
        assert_eq!(os.services.start_type("DiagTrack").unwrap(), Some(ServiceStart::Automatic));
    }

    #[test]
    fn missing_service_and_boot_driver_are_not_applicable() {
        let fake = FakeOs::default();
        let os = fake.os();
        let action = |service: &str| {
            Action::Service(ServiceAction { service: service.into(), start: StartType::Disabled })
        };
        let detection = detect(&os, &action("Ghost"), &Target::Machine).unwrap();
        assert_eq!(
            detection,
            Detection::NotApplicable { reason: "service Ghost is not installed".into() }
        );
        assert_eq!(
            capture(&os, &action("Ghost"), &Target::Machine).unwrap_err().kind,
            OsErrorKind::NotFound
        );

        fake.services.install("disk", ServiceStart::Boot);
        let detection = detect(&os, &action("disk"), &Target::Machine).unwrap();
        assert!(matches!(detection, Detection::NotApplicable { .. }), "{detection:?}");
    }

    #[test]
    fn power_setting_restores_only_the_managed_source() {
        let fake = FakeOs::default();
        let os = fake.os();
        fake.power.define(SUBGROUP, SETTING, PowerValues { ac: 5, dc: 5 });
        let action = Action::Power(PowerAction {
            subgroup: SUBGROUP.into(),
            setting: SETTING.into(),
            ac: Some(100),
            dc: None,
        });

        let detection = detect(&os, &action, &Target::Machine).unwrap();
        assert_eq!(detection, Detection::Different { current: "AC 5, DC 5".into() });
        let snapshot = capture(&os, &action, &Target::Machine).unwrap();
        apply(&os, &action, &Target::Machine).unwrap();
        assert!(matches!(
            detect(&os, &action, &Target::Machine).unwrap(),
            Detection::Compliant { .. }
        ));

        // The user changes the unmanaged DC value while the tweak is on.
        fake.power.define(SUBGROUP, SETTING, PowerValues { ac: 100, dc: 42 });
        revert(&os, &action, &Target::Machine, &snapshot).unwrap();
        assert_eq!(os.power.read(SUBGROUP, SETTING).unwrap(), Some(PowerValues { ac: 5, dc: 42 }));
    }

    #[test]
    fn power_setting_missing_from_the_plan_is_not_applicable() {
        let fake = FakeOs::default();
        let action = Action::Power(PowerAction {
            subgroup: SUBGROUP.into(),
            setting: SETTING.into(),
            ac: Some(1),
            dc: Some(1),
        });
        let detection = detect(&fake.os(), &action, &Target::Machine).unwrap();
        assert!(matches!(detection, Detection::NotApplicable { .. }), "{detection:?}");
    }

    #[test]
    fn command_with_check_detects_by_exit_code() {
        let fake = FakeOs::default();
        let os = fake.os();
        let action = command_action(true, false);
        fake.launcher.script(r"C:\check.exe", Ok(1));
        let detection = detect(&os, &action, &Target::Machine).unwrap();
        assert_eq!(detection, Detection::Different { current: "check exit code 1".into() });
        let detection = detect(&os, &action, &Target::Machine).unwrap();
        assert_eq!(detection, Detection::Compliant { current: "check exit code 0".into() });
    }

    #[test]
    fn command_without_check_is_unverifiable() {
        let fake = FakeOs::default();
        let detection =
            detect(&fake.os(), &command_action(false, false), &Target::Machine).unwrap();
        assert_eq!(detection, Detection::Unverifiable);
        assert!(fake.launcher.launched().is_empty(), "nothing may run just to detect");
    }

    #[test]
    fn command_apply_fails_on_non_zero_exit() {
        let fake = FakeOs::default();
        fake.launcher.script(r"C:\apply.exe", Ok(2));
        let error = apply(&fake.os(), &command_action(false, false), &Target::Machine).unwrap_err();
        assert_eq!(error.context, r"run C:\apply.exe: expected exit code 0 | received: 2");
    }

    #[test]
    fn command_revert_runs_the_undo_command_when_there_is_one() {
        let fake = FakeOs::default();
        let os = fake.os();
        revert(&os, &command_action(false, false), &Target::Machine, &Snapshot::Command).unwrap();
        assert!(fake.launcher.launched().is_empty());

        revert(&os, &command_action(false, true), &Target::Machine, &Snapshot::Command).unwrap();
        let launched = fake.launcher.launched();
        assert_eq!(launched.len(), 1);
        assert_eq!(launched[0].command.program, r"C:\undo.exe");
        assert_eq!(launched[0].timeout, Duration::from_secs(30));
    }

    #[test]
    fn revert_with_a_snapshot_of_another_kind_is_refused() {
        let fake = FakeOs::default();
        let action = registry_action(Scope::Machine, RegData::Dword(1));
        let error = revert(&fake.os(), &action, &Target::Machine, &Snapshot::Command).unwrap_err();
        assert!(error.context.contains("another action kind"), "{error}");
    }

    #[test]
    fn watches_point_at_where_windows_stores_the_state() {
        let registry = registry_action(Scope::User, RegData::Dword(1));
        assert_eq!(
            watch_for(&registry, &Target::User(SID.into())),
            Some(WatchSpec { hive: Hive::User(SID.into()), key: KEY.into(), subtree: false })
        );
        let service =
            Action::Service(ServiceAction { service: "Spooler".into(), start: StartType::Manual });
        assert_eq!(
            watch_for(&service, &Target::Machine).unwrap().key,
            r"SYSTEM\CurrentControlSet\Services\Spooler"
        );
        let power = Action::Power(PowerAction {
            subgroup: SUBGROUP.into(),
            setting: SETTING.into(),
            ac: Some(1),
            dc: None,
        });
        assert!(watch_for(&power, &Target::Machine).unwrap().subtree);
        assert_eq!(watch_for(&command_action(true, true), &Target::Machine), None);
    }

    #[test]
    fn nearest_existing_key_walks_up_to_an_ancestor() {
        let fake = FakeOs::default();
        let os = fake.os();
        assert_eq!(nearest_existing_key(&os, &Hive::Machine, KEY).unwrap(), None);
        fake.registry.create_key(&Hive::Machine, "SOFTWARE");
        assert_eq!(
            nearest_existing_key(&os, &Hive::Machine, KEY).unwrap(),
            Some("SOFTWARE".into())
        );
        fake.registry.write(&Hive::Machine, KEY, "v", &dword(1)).unwrap();
        assert_eq!(nearest_existing_key(&os, &Hive::Machine, KEY).unwrap(), Some(KEY.into()));
    }
}
