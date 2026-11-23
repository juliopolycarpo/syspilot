//! Integration tests (kind: os): the Win32 backends against the real
//! system.
//!
//! Registry tests write only under `Software\SysPilot-Test` in the current
//! user's hive and under `HKLM\SOFTWARE\SysPilot-Test`, and remove what they
//! create. Service and power tests never change a setting: services are only
//! queried, and the power test writes back the value it has just read.

use std::time::{Duration, Instant};

use syspilot_engine::enforcer::{Enforcer, EnforcerConfig, Entry, TriggerEvent};
use syspilot_engine::os::{
    Hive, Launcher, OsErrorKind, Power, RegValue, Registry, ServiceStart, Services, Sessions,
    reg_type,
};
use syspilot_engine::store::MemoryStateStore;
use syspilot_engine::win::{
    WinLauncher, WinPower, WinRegistry, WinServices, WinSessions, os, process_user_sid,
};
use syspilot_proto::action::{Action, CommandLine, RegData, RegistryAction, RunAs, Scope};
use syspilot_proto::message::{ActionState, HistoryKind};

const SCRATCH_ROOT: &str = r"Software\SysPilot-Test";
/// Processor performance subgroup and "minimum processor state": present
/// in every power plan.
const PROCESSOR_SUBGROUP: &str = "54533251-82be-4824-96c1-47b60b740d00";
const MINIMUM_PROCESSOR_STATE: &str = "893dee8e-2bef-41e0-89c6-b55d0929964c";

fn current_user() -> Hive {
    Hive::User(process_user_sid().expect("this process has a user SID"))
}

/// A scratch key unique to one test, removed (with everything below it)
/// when dropped, together with the scratch root if that is then empty.
struct ScratchKey {
    hive: Hive,
    key: String,
}

impl ScratchKey {
    fn new(hive: Hive, tag: &str) -> Self {
        let key = format!(r"{SCRATCH_ROOT}\engine-{tag}-{}", std::process::id());
        let scratch = Self { hive, key };
        scratch.remove();
        scratch
    }

    fn child(&self, name: &str) -> String {
        format!(r"{}\{name}", self.key)
    }

    /// Deletes the known values and keys the tests create. Kept explicit
    /// (no recursive delete) so a bug here cannot remove anything else.
    fn remove(&self) {
        let registry = WinRegistry;
        for key in [self.child(r"a\b"), self.child("a"), self.key.clone()] {
            for name in ["Value", "Other", "dword", "qword", "sz", "expand", "multi", "binary", ""]
            {
                let _ = registry.delete_value(&self.hive, &key, name);
            }
            let _ = registry.delete_key_if_empty(&self.hive, &key);
        }
        let _ = registry.delete_key_if_empty(&self.hive, SCRATCH_ROOT);
    }
}

impl Drop for ScratchKey {
    fn drop(&mut self) {
        self.remove();
    }
}

fn dword(value: u32) -> RegValue {
    RegValue { kind: reg_type::DWORD, data: value.to_le_bytes().to_vec() }
}

#[test]
fn registry_values_of_every_type_round_trip() {
    let scratch = ScratchKey::new(current_user(), "types");
    let registry = WinRegistry;
    let values = [
        ("dword", RegData::Dword(0xDEAD_BEEF)),
        ("qword", RegData::Qword(u64::MAX - 1)),
        ("sz", RegData::Sz("plain text".into())),
        ("expand", RegData::ExpandSz(r"%SystemRoot%\x".into())),
        ("multi", RegData::MultiSz(vec!["first".into(), "second".into()])),
        ("binary", RegData::Binary(vec![0, 1, 2, 254, 255])),
        ("", RegData::Sz("the default value".into())),
    ];
    for (name, data) in &values {
        let value = RegValue::from_data(data).unwrap();
        registry.write(&scratch.hive, &scratch.key, name, &value).unwrap();
        let read_back = registry.read(&scratch.hive, &scratch.key, name).unwrap().unwrap();
        assert_eq!(read_back, value, "value {name:?}");
        assert!(read_back.matches(data), "value {name:?} reads back as {}", read_back.describe());
    }
}

#[test]
fn missing_values_and_keys_read_as_absent() {
    let scratch = ScratchKey::new(current_user(), "missing");
    let registry = WinRegistry;
    assert_eq!(registry.read(&scratch.hive, &scratch.key, "Value").unwrap(), None);
    assert!(!registry.key_exists(&scratch.hive, &scratch.key).unwrap());
    registry.delete_value(&scratch.hive, &scratch.key, "Value").unwrap();
    assert!(!registry.delete_key_if_empty(&scratch.hive, &scratch.key).unwrap());

    registry.write(&scratch.hive, &scratch.key, "Other", &dword(1)).unwrap();
    assert_eq!(registry.read(&scratch.hive, &scratch.key, "Value").unwrap(), None);
    registry.delete_value(&scratch.hive, &scratch.key, "Value").unwrap();
}

#[test]
fn writing_creates_parents_and_only_empty_keys_are_deleted() {
    let scratch = ScratchKey::new(current_user(), "tree");
    let registry = WinRegistry;
    let leaf = scratch.child(r"a\b");
    registry.write(&scratch.hive, &leaf, "Value", &dword(1)).unwrap();
    assert!(registry.key_exists(&scratch.hive, &scratch.child("a")).unwrap());

    assert!(!registry.delete_key_if_empty(&scratch.hive, &leaf).unwrap(), "it has a value");
    assert!(
        !registry.delete_key_if_empty(&scratch.hive, &scratch.child("a")).unwrap(),
        "it has a subkey"
    );
    registry.delete_value(&scratch.hive, &leaf, "Value").unwrap();
    assert!(registry.delete_key_if_empty(&scratch.hive, &leaf).unwrap());
    assert!(registry.delete_key_if_empty(&scratch.hive, &scratch.child("a")).unwrap());
    assert!(!registry.key_exists(&scratch.hive, &scratch.child("a")).unwrap());
}

#[test]
fn keys_directly_under_a_hive_root_are_never_deleted() {
    // The guard returns before touching the registry, so this is safe to
    // run against the real `SOFTWARE` key.
    assert!(!WinRegistry.delete_key_if_empty(&Hive::Machine, "SOFTWARE").unwrap());
    assert!(WinRegistry.key_exists(&Hive::Machine, "SOFTWARE").unwrap());
}

#[test]
fn machine_hive_is_writable_when_elevated() {
    let scratch = ScratchKey::new(Hive::Machine, "machine");
    let registry = WinRegistry;
    match registry.write(&scratch.hive, &scratch.key, "Value", &dword(7)) {
        Ok(()) => {
            assert_eq!(
                registry.read(&scratch.hive, &scratch.key, "Value").unwrap(),
                Some(dword(7))
            );
        }
        Err(error) if error.kind == OsErrorKind::AccessDenied => {
            eprintln!("skipped: writing HKLM needs an elevated process ({error})");
        }
        Err(error) => panic!("expected success or access denied | received: {error}"),
    }
}

#[test]
fn enforcer_applies_restores_and_reverts_on_the_real_registry() {
    let Hive::User(sid) = current_user() else { unreachable!("current_user returns a user hive") };
    let scratch = ScratchKey::new(Hive::User(sid.clone()), "enforcer");
    let registry = WinRegistry;
    let action = Action::Registry(RegistryAction {
        scope: Scope::User,
        key: scratch.child("a"),
        name: "Value".into(),
        data: RegData::Dword(1),
    });
    let entries = || {
        vec![Entry {
            id: "test.scratch".into(),
            triggers: vec![action.default_triggers()],
            actions: vec![action.clone()],
            audit: false,
            not_applicable: None,
        }]
    };
    let mut enforcer =
        Enforcer::new(os(), Box::new(MemoryStateStore::default()), EnforcerConfig::default());

    enforcer.configure(entries(), ["test.scratch".to_owned()]);
    enforcer.trigger(&TriggerEvent::Logon { sid });
    assert_eq!(registry.read(&scratch.hive, &scratch.child("a"), "Value").unwrap(), Some(dword(1)));
    assert_eq!(enforcer.status()[0].state, ActionState::Compliant);

    // Another writer changes the value; the next evaluation restores it.
    registry.write(&scratch.hive, &scratch.child("a"), "Value", &dword(9)).unwrap();
    assert_eq!(enforcer.status()[0].state, ActionState::Different);
    let watch = enforcer.watches().into_iter().next().expect("the scratch key is watched");
    assert_eq!(watch.key, scratch.child("a"));
    enforcer.trigger(&TriggerEvent::Change(watch));
    assert_eq!(registry.read(&scratch.hive, &scratch.child("a"), "Value").unwrap(), Some(dword(1)));
    assert_eq!(enforcer.history(1)[0].kind, HistoryKind::Restored);

    // Disabling removes the value and every key that did not exist before.
    enforcer.configure(entries(), []);
    enforcer.reconcile();
    assert!(!registry.key_exists(&scratch.hive, &scratch.key).unwrap());
}

#[test]
fn service_start_types_are_read_without_changing_anything() {
    let services = WinServices;
    assert_eq!(services.start_type("EventLog").unwrap(), Some(ServiceStart::Automatic));
    assert_eq!(services.start_type("SysPilot-Test-NoSuchService").unwrap(), None);
    let error =
        services.set_start_type("SysPilot-Test-NoSuchService", ServiceStart::Disabled).unwrap_err();
    assert_eq!(error.kind, OsErrorKind::NotFound, "{error}");
}

#[test]
fn power_setting_is_read_and_rewritten_with_its_own_value() {
    let power = WinPower;
    let unknown = "11111111-2222-3333-4444-555555555555";
    assert_eq!(power.read(PROCESSOR_SUBGROUP, unknown).unwrap(), None);

    let values = power
        .read(PROCESSOR_SUBGROUP, MINIMUM_PROCESSOR_STATE)
        .unwrap()
        .expect("every power plan has a minimum processor state");
    assert!(values.ac <= 100 && values.dc <= 100, "expected percentages | received: {values:?}");

    // Writing the current value exercises the write path without changing
    // the setting.
    match power.write(PROCESSOR_SUBGROUP, MINIMUM_PROCESSOR_STATE, Some(values.ac), Some(values.dc))
    {
        Ok(()) => {
            let after = power.read(PROCESSOR_SUBGROUP, MINIMUM_PROCESSOR_STATE).unwrap();
            assert_eq!(after, Some(values));
        }
        Err(error) if error.kind == OsErrorKind::AccessDenied => {
            eprintln!("skipped: writing power settings was refused ({error})");
        }
        Err(error) => panic!("expected success or access denied | received: {error}"),
    }
}

fn system32(program: &str) -> String {
    format!(r"%SystemRoot%\System32\{program}")
}

#[test]
fn launcher_returns_the_exit_code() {
    let command =
        CommandLine { program: system32("cmd.exe"), args: vec!["/c".into(), "exit 3".into()] };
    for run_as in [RunAs::System, RunAs::User] {
        let exit = WinLauncher.run(&command, run_as, Duration::from_secs(20)).unwrap();
        assert_eq!(exit, 3, "run as {run_as:?}");
    }
}

#[test]
fn launcher_passes_arguments_with_spaces_and_quotes_intact() {
    // `reg.exe` parses its command line with the C runtime rules the
    // launcher quotes for, and stores the argument where it can be read
    // back byte for byte.
    let scratch = ScratchKey::new(current_user(), "args");
    let tricky = r#"a "quoted" b\"#;
    let command = CommandLine {
        program: system32("reg.exe"),
        args: [
            "add",
            &format!(r"HKCU\{}", scratch.key),
            "/v",
            "Value",
            "/t",
            "REG_SZ",
            "/d",
            tricky,
            "/f",
        ]
        .map(String::from)
        .to_vec(),
    };
    let exit = WinLauncher.run(&command, RunAs::System, Duration::from_secs(20)).unwrap();
    assert_eq!(exit, 0, "reg.exe refused the command line");
    let stored = WinRegistry.read(&scratch.hive, &scratch.key, "Value").unwrap().unwrap();
    assert!(
        stored.matches(&RegData::Sz(tricky.into())),
        "expected the argument {tricky:?} to arrive intact | received: {}",
        stored.describe()
    );
}

#[test]
fn launcher_terminates_a_command_that_exceeds_its_timeout() {
    let command = CommandLine {
        program: system32("ping.exe"),
        args: vec!["-n".into(), "60".into(), "127.0.0.1".into()],
    };
    let started = Instant::now();
    let error = WinLauncher.run(&command, RunAs::System, Duration::from_millis(300)).unwrap_err();
    assert_eq!(error.kind, OsErrorKind::Timeout, "{error}");
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the launcher waited for the command instead of terminating it"
    );
}

#[test]
fn launcher_reports_a_missing_program() {
    let command =
        CommandLine { program: r"C:\SysPilot-Test\no-such-program.exe".into(), args: vec![] };
    let error = WinLauncher.run(&command, RunAs::System, Duration::from_secs(5)).unwrap_err();
    assert_eq!(error.kind, OsErrorKind::NotFound, "{error}");
}

#[test]
fn at_least_the_current_user_is_logged_on() {
    let sids = WinSessions.interactive_user_sids();
    assert!(!sids.is_empty(), "expected at least one user SID | received: none");
    assert!(sids.iter().all(|sid| sid.starts_with("S-1-")), "{sids:?}");
}
