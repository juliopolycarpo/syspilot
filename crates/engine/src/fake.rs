//! In-memory implementations of the [`crate::os`] traits.
//!
//! Every fake is a cheap handle onto shared state: tests keep one clone to
//! arrange and inspect, and hand another to the engine.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use syspilot_proto::action::{CommandLine, RunAs};

use crate::os::{
    Clock, Hive, Launcher, Os, OsError, OsErrorKind, Power, PowerValues, RegValue, Registry,
    ServiceStart, Services, Sessions,
};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A poisoned fake means a test already panicked; keep reporting that
    // panic instead of a second one from here.
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

type KeyId = (Hive, String);

fn key_id(hive: &Hive, key: &str) -> KeyId {
    (hive.clone(), key.to_ascii_lowercase())
}

#[derive(Default)]
struct RegistryState {
    /// Existing keys and their values, by lowercased path and value name.
    keys: BTreeMap<KeyId, BTreeMap<String, RegValue>>,
    read_only: BTreeSet<KeyId>,
    /// Values another writer forces back after every write.
    contested: HashMap<(KeyId, String), Option<RegValue>>,
    notifications: VecDeque<(Hive, String)>,
    writes: usize,
}

impl RegistryState {
    fn create_key(&mut self, hive: &Hive, key: &str) {
        let mut path = String::new();
        for part in key.split('\\') {
            if !path.is_empty() {
                path.push('\\');
            }
            path.push_str(part);
            self.keys.entry(key_id(hive, &path)).or_default();
        }
    }

    fn store(&mut self, hive: &Hive, key: &str, name: &str, value: Option<RegValue>) {
        match value {
            Some(value) => {
                self.create_key(hive, key);
                if let Some(values) = self.keys.get_mut(&key_id(hive, key)) {
                    values.insert(name.to_ascii_lowercase(), value);
                }
            }
            None => {
                if let Some(values) = self.keys.get_mut(&key_id(hive, key)) {
                    values.remove(&name.to_ascii_lowercase());
                }
            }
        }
        self.notifications.push_back((hive.clone(), key.to_ascii_lowercase()));
    }

    /// Applies an engine write, then lets a contesting writer undo it.
    fn engine_store(&mut self, hive: &Hive, key: &str, name: &str, value: Option<RegValue>) {
        self.writes += 1;
        self.store(hive, key, name, value);
        let contested = self.contested.get(&(key_id(hive, key), name.to_ascii_lowercase()));
        if let Some(forced) = contested.cloned() {
            self.store(hive, key, name, forced);
        }
    }
}

/// An in-memory registry.
#[derive(Clone, Default)]
pub struct FakeRegistry(Arc<Mutex<RegistryState>>);

impl FakeRegistry {
    /// Sets a value as if another program wrote it, and queues a change
    /// notification. `None` deletes the value.
    pub fn set_external(&self, hive: &Hive, key: &str, name: &str, value: Option<RegValue>) {
        lock(&self.0).store(hive, key, name, value);
    }

    /// Creates an empty key as if it had always existed.
    pub fn create_key(&self, hive: &Hive, key: &str) {
        lock(&self.0).create_key(hive, key);
    }

    /// Makes every write and delete under `key` fail with access denied, as
    /// Windows does for protected settings.
    pub fn protect(&self, hive: &Hive, key: &str) {
        lock(&self.0).read_only.insert(key_id(hive, key));
    }

    /// Simulates another writer that forces `value` back immediately after
    /// every engine write to this value.
    pub fn contest(&self, hive: &Hive, key: &str, name: &str, value: Option<RegValue>) {
        lock(&self.0).contested.insert((key_id(hive, key), name.to_ascii_lowercase()), value);
    }

    /// Stops the writer installed by [`FakeRegistry::contest`].
    pub fn stop_contesting(&self, hive: &Hive, key: &str, name: &str) {
        lock(&self.0).contested.remove(&(key_id(hive, key), name.to_ascii_lowercase()));
    }

    /// Takes the oldest pending change notification: the hive and the
    /// lowercased key that changed.
    pub fn take_notification(&self) -> Option<(Hive, String)> {
        lock(&self.0).notifications.pop_front()
    }

    /// Drops all pending change notifications.
    pub fn clear_notifications(&self) {
        lock(&self.0).notifications.clear();
    }

    /// Number of writes and deletes the engine performed.
    pub fn engine_writes(&self) -> usize {
        lock(&self.0).writes
    }

    /// Current value, for assertions.
    pub fn value(&self, hive: &Hive, key: &str, name: &str) -> Option<RegValue> {
        let state = lock(&self.0);
        state.keys.get(&key_id(hive, key))?.get(&name.to_ascii_lowercase()).cloned()
    }

    /// Whether the key exists, for assertions.
    pub fn has_key(&self, hive: &Hive, key: &str) -> bool {
        lock(&self.0).keys.contains_key(&key_id(hive, key))
    }
}

impl Registry for FakeRegistry {
    fn read(&self, hive: &Hive, key: &str, name: &str) -> Result<Option<RegValue>, OsError> {
        Ok(self.value(hive, key, name))
    }

    fn write(&self, hive: &Hive, key: &str, name: &str, value: &RegValue) -> Result<(), OsError> {
        let mut state = lock(&self.0);
        if state.read_only.contains(&key_id(hive, key)) {
            let context = format!(r"write {hive}\{key}\{name}");
            return Err(OsError::new(OsErrorKind::AccessDenied, context, 5));
        }
        state.engine_store(hive, key, name, Some(value.clone()));
        Ok(())
    }

    fn delete_value(&self, hive: &Hive, key: &str, name: &str) -> Result<(), OsError> {
        let mut state = lock(&self.0);
        if state.read_only.contains(&key_id(hive, key)) {
            let context = format!(r"delete {hive}\{key}\{name}");
            return Err(OsError::new(OsErrorKind::AccessDenied, context, 5));
        }
        state.engine_store(hive, key, name, None);
        Ok(())
    }

    fn key_exists(&self, hive: &Hive, key: &str) -> Result<bool, OsError> {
        Ok(self.has_key(hive, key))
    }

    fn delete_key_if_empty(&self, hive: &Hive, key: &str) -> Result<bool, OsError> {
        let mut state = lock(&self.0);
        let id = key_id(hive, key);
        let prefix = format!("{}\\", id.1);
        let has_subkeys =
            state.keys.keys().any(|(other, path)| *other == id.0 && path.starts_with(&prefix));
        let is_empty = state.keys.get(&id).is_some_and(BTreeMap::is_empty);
        if is_empty && !has_subkeys {
            state.keys.remove(&id);
            state.writes += 1;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

#[derive(Default)]
struct ServicesState {
    start: BTreeMap<String, ServiceStart>,
    protected: BTreeSet<String>,
    changes: usize,
}

/// An in-memory service control manager.
#[derive(Clone, Default)]
pub struct FakeServices(Arc<Mutex<ServicesState>>);

impl FakeServices {
    /// Installs a service, or changes its start type from outside.
    pub fn install(&self, service: &str, start: ServiceStart) {
        lock(&self.0).start.insert(service.to_ascii_lowercase(), start);
    }

    /// Makes configuration changes to `service` fail with access denied.
    pub fn protect(&self, service: &str) {
        lock(&self.0).protected.insert(service.to_ascii_lowercase());
    }

    /// Number of configuration changes the engine made.
    pub fn engine_changes(&self) -> usize {
        lock(&self.0).changes
    }
}

impl Services for FakeServices {
    fn start_type(&self, service: &str) -> Result<Option<ServiceStart>, OsError> {
        Ok(lock(&self.0).start.get(&service.to_ascii_lowercase()).copied())
    }

    fn set_start_type(&self, service: &str, start: ServiceStart) -> Result<(), OsError> {
        let mut state = lock(&self.0);
        let name = service.to_ascii_lowercase();
        let context = format!("configure service {service}");
        if state.protected.contains(&name) {
            return Err(OsError::new(OsErrorKind::AccessDenied, context, 5));
        }
        match state.start.get_mut(&name) {
            Some(current) => {
                *current = start;
                state.changes += 1;
                Ok(())
            }
            None => Err(OsError::new(OsErrorKind::NotFound, context, 1060)),
        }
    }
}

/// An in-memory active power scheme.
#[derive(Clone, Default)]
pub struct FakePower(Arc<Mutex<BTreeMap<(String, String), PowerValues>>>);

impl FakePower {
    /// Defines a setting, or changes it from outside.
    pub fn define(&self, subgroup: &str, setting: &str, values: PowerValues) {
        lock(&self.0).insert((subgroup.to_ascii_lowercase(), setting.to_ascii_lowercase()), values);
    }
}

impl Power for FakePower {
    fn read(&self, subgroup: &str, setting: &str) -> Result<Option<PowerValues>, OsError> {
        let id = (subgroup.to_ascii_lowercase(), setting.to_ascii_lowercase());
        Ok(lock(&self.0).get(&id).copied())
    }

    fn write(
        &self,
        subgroup: &str,
        setting: &str,
        ac: Option<u32>,
        dc: Option<u32>,
    ) -> Result<(), OsError> {
        let id = (subgroup.to_ascii_lowercase(), setting.to_ascii_lowercase());
        match lock(&self.0).get_mut(&id) {
            Some(values) => {
                values.ac = ac.unwrap_or(values.ac);
                values.dc = dc.unwrap_or(values.dc);
                Ok(())
            }
            None => Err(OsError::new(
                OsErrorKind::NotFound,
                format!("write power setting {subgroup}/{setting}"),
                2,
            )),
        }
    }
}

/// One command the engine ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchedCommand {
    /// The command line.
    pub command: CommandLine,
    /// The account it ran under.
    pub run_as: RunAs,
    /// The timeout it was given.
    pub timeout: Duration,
}

#[derive(Default)]
struct LauncherState {
    results: HashMap<String, VecDeque<Result<i32, OsError>>>,
    launched: Vec<LaunchedCommand>,
}

/// A launcher that records commands and returns scripted exit codes.
/// Unscripted programs exit with 0.
#[derive(Clone, Default)]
pub struct FakeLauncher(Arc<Mutex<LauncherState>>);

impl FakeLauncher {
    /// Queues the result of the next run of `program`.
    pub fn script(&self, program: &str, result: Result<i32, OsError>) {
        lock(&self.0).results.entry(program.to_owned()).or_default().push_back(result);
    }

    /// Every command run so far, in order.
    pub fn launched(&self) -> Vec<LaunchedCommand> {
        lock(&self.0).launched.clone()
    }
}

impl Launcher for FakeLauncher {
    fn run(&self, command: &CommandLine, run_as: RunAs, timeout: Duration) -> Result<i32, OsError> {
        let mut state = lock(&self.0);
        state.launched.push(LaunchedCommand { command: command.clone(), run_as, timeout });
        state.results.get_mut(&command.program).and_then(VecDeque::pop_front).unwrap_or(Ok(0))
    }
}

/// A settable list of logged-on users.
#[derive(Clone, Default)]
pub struct FakeSessions(Arc<Mutex<Vec<String>>>);

impl FakeSessions {
    /// Replaces the logged-on users.
    pub fn set(&self, sids: &[&str]) {
        *lock(&self.0) = sids.iter().map(|sid| (*sid).to_owned()).collect();
    }
}

impl Sessions for FakeSessions {
    fn interactive_user_sids(&self) -> Vec<String> {
        lock(&self.0).clone()
    }
}

/// A clock that only moves when told to.
#[derive(Clone)]
pub struct FakeClock(Arc<Mutex<Duration>>);

impl FakeClock {
    /// Wall-clock time at monotonic zero.
    pub const EPOCH_UNIX_SECS: u64 = 1_800_000_000;

    /// Moves time forward.
    pub fn advance(&self, by: Duration) {
        *lock(&self.0) += by;
    }
}

impl Default for FakeClock {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(Duration::ZERO)))
    }
}

impl Clock for FakeClock {
    fn monotonic(&self) -> Duration {
        *lock(&self.0)
    }

    fn unix_secs(&self) -> u64 {
        Self::EPOCH_UNIX_SECS + lock(&self.0).as_secs()
    }
}

/// A complete fake operating system.
#[derive(Clone, Default)]
pub struct FakeOs {
    /// The registry.
    pub registry: FakeRegistry,
    /// The services.
    pub services: FakeServices,
    /// The power scheme.
    pub power: FakePower,
    /// The command launcher.
    pub launcher: FakeLauncher,
    /// The logged-on users.
    pub sessions: FakeSessions,
    /// The clock.
    pub clock: FakeClock,
}

impl FakeOs {
    /// An [`Os`] whose parts share state with `self`.
    pub fn os(&self) -> Os {
        Os {
            registry: Box::new(self.registry.clone()),
            services: Box::new(self.services.clone()),
            power: Box::new(self.power.clone()),
            launcher: Box::new(self.launcher.clone()),
            sessions: Box::new(self.sessions.clone()),
            clock: Box::new(self.clock.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::os::reg_type;

    fn dword(value: u32) -> RegValue {
        RegValue { kind: reg_type::DWORD, data: value.to_le_bytes().to_vec() }
    }

    #[test]
    fn registry_paths_and_names_are_case_insensitive() {
        let registry = FakeRegistry::default();
        registry.write(&Hive::Machine, r"SOFTWARE\Test", "Value", &dword(1)).unwrap();
        assert_eq!(
            registry.read(&Hive::Machine, r"software\TEST", "VALUE").unwrap(),
            Some(dword(1))
        );
    }

    #[test]
    fn writing_creates_parent_keys() {
        let registry = FakeRegistry::default();
        registry.write(&Hive::Machine, r"A\B\C", "v", &dword(1)).unwrap();
        for key in ["A", r"A\B", r"A\B\C"] {
            assert!(registry.key_exists(&Hive::Machine, key).unwrap(), "{key}");
        }
    }

    #[test]
    fn only_empty_leaf_keys_are_deleted() {
        let registry = FakeRegistry::default();
        registry.write(&Hive::Machine, r"A\B", "v", &dword(1)).unwrap();
        assert!(!registry.delete_key_if_empty(&Hive::Machine, "A").unwrap(), "has a subkey");
        assert!(!registry.delete_key_if_empty(&Hive::Machine, r"A\B").unwrap(), "has a value");
        registry.delete_value(&Hive::Machine, r"A\B", "v").unwrap();
        assert!(registry.delete_key_if_empty(&Hive::Machine, r"A\B").unwrap());
        assert!(registry.delete_key_if_empty(&Hive::Machine, "A").unwrap());
        assert!(!registry.delete_key_if_empty(&Hive::Machine, "A").unwrap(), "already gone");
    }

    #[test]
    fn protected_key_refuses_writes() {
        let registry = FakeRegistry::default();
        registry.protect(&Hive::Machine, r"SOFTWARE\Protected");
        let error =
            registry.write(&Hive::Machine, r"SOFTWARE\Protected", "v", &dword(1)).unwrap_err();
        assert_eq!(error.kind, OsErrorKind::AccessDenied);
        assert_eq!(registry.engine_writes(), 0);
    }

    #[test]
    fn contested_value_snaps_back_and_notifies() {
        let registry = FakeRegistry::default();
        registry.contest(&Hive::Machine, "K", "v", Some(dword(9)));
        registry.write(&Hive::Machine, "K", "v", &dword(1)).unwrap();
        assert_eq!(registry.value(&Hive::Machine, "K", "v"), Some(dword(9)));
        assert_eq!(registry.take_notification(), Some((Hive::Machine, "k".into())));
    }

    #[test]
    fn hives_are_separate() {
        let registry = FakeRegistry::default();
        registry.write(&Hive::User("S-1-5-21-1".into()), "K", "v", &dword(1)).unwrap();
        assert_eq!(registry.value(&Hive::User("S-1-5-21-2".into()), "K", "v"), None);
        assert_eq!(registry.value(&Hive::Machine, "K", "v"), None);
    }

    #[test]
    fn services_report_missing_and_protected() {
        let services = FakeServices::default();
        assert_eq!(services.start_type("Nope").unwrap(), None);
        assert_eq!(
            services.set_start_type("Nope", ServiceStart::Disabled).unwrap_err().kind,
            OsErrorKind::NotFound
        );
        services.install("WinDefend", ServiceStart::Automatic);
        services.protect("windefend");
        assert_eq!(
            services.set_start_type("WinDefend", ServiceStart::Disabled).unwrap_err().kind,
            OsErrorKind::AccessDenied
        );
    }

    #[test]
    fn power_write_leaves_unspecified_source_untouched() {
        let power = FakePower::default();
        power.define("sub", "set", PowerValues { ac: 1, dc: 2 });
        power.write("SUB", "SET", Some(5), None).unwrap();
        assert_eq!(power.read("sub", "set").unwrap(), Some(PowerValues { ac: 5, dc: 2 }));
    }

    #[test]
    fn launcher_replays_scripted_results_then_succeeds() {
        let launcher = FakeLauncher::default();
        let command = CommandLine { program: r"C:\x.exe".into(), args: vec!["/a".into()] };
        launcher.script(r"C:\x.exe", Ok(3));
        let timeout = Duration::from_secs(5);
        assert_eq!(launcher.run(&command, RunAs::System, timeout).unwrap(), 3);
        assert_eq!(launcher.run(&command, RunAs::User, timeout).unwrap(), 0);
        assert_eq!(launcher.launched().len(), 2);
        assert_eq!(launcher.launched()[1].run_as, RunAs::User);
    }

    #[test]
    fn clock_moves_only_when_advanced() {
        let clock = FakeClock::default();
        assert_eq!(clock.monotonic(), Duration::ZERO);
        clock.advance(Duration::from_secs(90));
        assert_eq!(clock.monotonic(), Duration::from_secs(90));
        assert_eq!(clock.unix_secs(), FakeClock::EPOCH_UNIX_SECS + 90);
    }
}
