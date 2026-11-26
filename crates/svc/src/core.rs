//! The service's decision logic, free of Win32.
//!
//! [`Core`] owns the catalog, the profile and the enforcer, and turns one
//! request into one response. The host around it (pipe, timers, registry
//! watches, service control) lives in other modules; everything here runs
//! against fakes in tests.

use std::collections::HashSet;

use syspilot_engine::enforcer::{Enforcer, EnforcerConfig, Entry, StateStore, TriggerEvent};
use syspilot_engine::os::Os;
use syspilot_proto::catalog::{Catalog, Tweak};
use syspilot_proto::message::{ErrorCode, Request, Response, ServiceError};
use syspilot_proto::profile::{CustomAction, Profile};

/// Name under which the profile is stored.
pub const PROFILE_NAME: &str = "profile.toml";
/// Name under which a profile that failed to load is kept for inspection.
pub const REJECTED_PROFILE_NAME: &str = "profile.rejected.toml";
/// Most history events returned in one response.
const HISTORY_LIMIT: u32 = 500;

/// What a handled request changed, so the host knows whom to tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Effects {
    /// Entry states or the history may have changed.
    pub states: bool,
    /// The stored profile changed.
    pub profile: bool,
    /// The logging settings changed.
    pub logging: bool,
    /// How SysPilot updates itself changed.
    pub updates: bool,
    /// The tuning settings changed.
    pub tuning: bool,
    /// The service should stop.
    pub stop: bool,
}

/// Catalog, profile and enforcer behind one request handler.
pub struct Core<S> {
    catalog: Catalog,
    profile: Profile,
    enforcer: Enforcer,
    store: S,
    os_build: u32,
    console: bool,
    fan_control: bool,
    profile_problem: Option<String>,
}

fn error(code: ErrorCode, message: impl Into<String>) -> Response {
    Response::Error(ServiceError { code, message: message.into() })
}

/// The refusal a request that needs an elevated administrator gets from
/// anyone else. One wording, whichever part of the service answers.
pub fn refusal(request: &Request, elevated: bool) -> Option<Response> {
    let message = "This needs administrator rights. Restart the app as administrator.";
    (request.requires_elevation() && !elevated).then(|| error(ErrorCode::AccessDenied, message))
}

impl<S: StateStore + Clone + 'static> Core<S> {
    /// Loads the profile and the enforcer state from `store`. Nothing is
    /// applied until [`Core::boot`].
    ///
    /// `console` allows [`Request::Shutdown`]; `fan_control` says whether
    /// this build accepts fan curves.
    pub fn new(os: Os, store: S, catalog: Catalog, os_build: u32, console: bool) -> Self {
        let (profile, profile_problem) = match store.load(PROFILE_NAME) {
            None => (Profile::default(), None),
            Some(text) => match Profile::parse(&text) {
                Ok(profile) => (profile, None),
                Err(problem) => {
                    // Keep the unreadable file for the user and start from
                    // nothing enabled: everything applied earlier is then
                    // reverted, which is the one consistent state left.
                    let _ = store.save(REJECTED_PROFILE_NAME, &text);
                    let message = format!(
                        "The saved profile could not be read and was set aside; all tweaks were turned off. {problem}"
                    );
                    (Profile::default(), Some(message))
                }
            },
        };
        let enforcer = Enforcer::new(os, Box::new(store.clone()), EnforcerConfig::default());
        Self {
            catalog,
            profile,
            enforcer,
            store,
            os_build,
            console,
            fan_control: cfg!(feature = "fan-control"),
            profile_problem,
        }
    }

    /// Applies the profile at service start. Returns whether anything
    /// changed on the system.
    pub fn boot(&mut self) -> bool {
        self.configure_enforcer();
        self.enforcer.trigger(&TriggerEvent::Boot)
    }

    /// The enforcer, for triggers, watches and deadlines.
    pub fn enforcer(&mut self) -> &mut Enforcer {
        &mut self.enforcer
    }

    /// The current profile.
    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    /// Whether this build accepts fan curves.
    pub fn fan_control(&self) -> bool {
        self.fan_control
    }

    /// Makes a test build accept fan curves whatever the cargo features say,
    /// so the default test run covers the wiring. Nothing writes to a fan
    /// through the core: the hardware it talks to is a fake.
    #[cfg(test)]
    pub fn accept_fan_curves_in_tests(&mut self) {
        self.fan_control = true;
    }

    /// Problems the user should see in the status view.
    pub fn problems(&self) -> Vec<String> {
        let store_problem = self
            .enforcer
            .store_error()
            .map(|problem| format!("Settings could not be saved to disk: {problem}"));
        self.profile_problem.iter().cloned().chain(store_problem).collect()
    }

    /// Handles one request that only needs the core. `elevated` is whether
    /// the client is an elevated administrator.
    ///
    /// `Hello`, `Subscribe`, `Status`, `Sensors`, `SystemInfo`,
    /// `CheckForUpdate`, `InstallUpdate` and `ApplyTuning` belong to the host
    /// and are answered with an error here. The host also judges the values
    /// of `SetTuning` against the devices before it passes the request on;
    /// here only the shape of the settings is checked.
    pub fn handle(&mut self, request: Request, elevated: bool) -> (Response, Effects) {
        let mut effects = Effects::default();
        if let Some(refused) = refusal(&request, elevated) {
            return (refused, effects);
        }
        let response = match request {
            Request::Catalog => Response::Catalog(self.catalog.clone()),
            Request::States => Response::States(self.enforcer.status()),
            Request::Profile => Response::Profile(self.profile.clone()),
            Request::History { limit } => {
                Response::History(self.enforcer.history(limit.min(HISTORY_LIMIT) as usize))
            }
            Request::SetTweak { id, enabled } => self.set_tweak(&id, enabled, &mut effects),
            Request::PutCustom(custom) => self.put_custom(custom, &mut effects),
            Request::RemoveCustom { id } => self.remove_custom(&id, &mut effects),
            Request::Reapply { id } => {
                if self.enforcer.has_entry(&id) {
                    effects.states = self.enforcer.trigger(&TriggerEvent::Reapply { id });
                    Response::Ok
                } else {
                    error(ErrorCode::NotFound, format!("There is no entry with the ID {id}."))
                }
            }
            Request::SetProfile(profile) => self.set_profile(profile, &mut effects),
            Request::SetLogging(logging) => match logging.validate() {
                Ok(()) => {
                    let mut profile = self.profile.clone();
                    profile.logging = logging;
                    effects.logging = true;
                    self.commit(profile, &mut effects)
                }
                Err(problem) => error(ErrorCode::Invalid, problem.to_string()),
            },
            Request::SetFans(fans) => {
                if !self.fan_control && !fans.is_empty() {
                    let message = "Fan control is not enabled in this build.";
                    return (error(ErrorCode::Unsupported, message), effects);
                }
                let mut profile = self.profile.clone();
                profile.fans = fans;
                match profile.validate() {
                    Ok(()) => self.commit(profile, &mut effects),
                    Err(problem) => error(ErrorCode::Invalid, problem.to_string()),
                }
            }
            Request::SetUpdates(mode) => {
                let mut profile = self.profile.clone();
                profile.updates = mode;
                effects.updates = true;
                self.commit(profile, &mut effects)
            }
            Request::SetTuning(tuning) => {
                let mut profile = self.profile.clone();
                profile.tuning = tuning;
                match profile.validate() {
                    Ok(()) => {
                        effects.tuning = true;
                        self.commit(profile, &mut effects)
                    }
                    Err(problem) => error(ErrorCode::Invalid, problem.to_string()),
                }
            }
            Request::ClearHistory => {
                self.enforcer.clear_history();
                effects.states = true;
                Response::Ok
            }
            Request::Shutdown => {
                if self.console {
                    effects.stop = true;
                    Response::Ok
                } else {
                    let message =
                        "A Windows service is stopped through the service control manager.";
                    error(ErrorCode::Unsupported, message)
                }
            }
            Request::Hello { .. }
            | Request::Subscribe { .. }
            | Request::Status
            | Request::Sensors
            | Request::SystemInfo
            | Request::CheckForUpdate
            | Request::InstallUpdate
            | Request::ApplyTuning => {
                error(ErrorCode::Invalid, "(request): not valid at this point of the connection")
            }
        };
        (response, effects)
    }

    fn set_tweak(&mut self, id: &str, enabled: bool, effects: &mut Effects) -> Response {
        let Some(tweak) = self.catalog.get(id) else {
            return error(ErrorCode::NotFound, format!("There is no tweak with the ID {id}."));
        };
        if enabled {
            if tweak.audit {
                let message = format!("{} is a check and cannot be turned on.", tweak.title);
                return error(ErrorCode::Invalid, message);
            }
            if let Some(reason) = self.inapplicable(tweak) {
                return error(ErrorCode::Unsupported, format!("{}: {reason}.", tweak.title));
            }
        }
        let mut profile = self.profile.clone();
        profile.enabled.retain(|existing| existing != id);
        if enabled {
            profile.enabled.push(id.to_owned());
        }
        self.commit(profile, effects)
    }

    fn put_custom(&mut self, custom: CustomAction, effects: &mut Effects) -> Response {
        if let Err(problem) = custom.validate() {
            return error(ErrorCode::Invalid, problem.to_string());
        }
        let mut profile = self.profile.clone();
        match profile.custom.iter_mut().find(|existing| existing.id == custom.id) {
            Some(existing) => *existing = custom,
            None => profile.custom.push(custom),
        }
        self.commit(profile, effects)
    }

    fn remove_custom(&mut self, id: &str, effects: &mut Effects) -> Response {
        let mut profile = self.profile.clone();
        let before = profile.custom.len();
        profile.custom.retain(|custom| custom.id != id);
        if profile.custom.len() == before {
            return error(
                ErrorCode::NotFound,
                format!("There is no custom action with the ID {id}."),
            );
        }
        self.commit(profile, effects)
    }

    fn set_profile(&mut self, profile: Profile, effects: &mut Effects) -> Response {
        if let Err(problem) = profile.validate() {
            return error(ErrorCode::Invalid, problem.to_string());
        }
        for id in &profile.enabled {
            match self.catalog.get(id) {
                None => {
                    let message =
                        format!("(profile.enabled): expected catalog tweak ids | received: {id:?}");
                    return error(ErrorCode::Invalid, message);
                }
                Some(tweak) if tweak.audit => {
                    let message =
                        format!("(profile.enabled): {id} is a check and cannot be turned on");
                    return error(ErrorCode::Invalid, message);
                }
                Some(_) => {}
            }
        }
        if !self.fan_control && !profile.fans.is_empty() {
            return error(ErrorCode::Unsupported, "Fan control is not enabled in this build.");
        }
        effects.logging = profile.logging != self.profile.logging;
        effects.updates = profile.updates != self.profile.updates;
        effects.tuning = profile.tuning != self.profile.tuning;
        self.commit(profile, effects)
    }

    /// Saves `profile`, makes it current and brings the system in line.
    /// The profile is saved first: if that fails nothing is changed.
    fn commit(&mut self, profile: Profile, effects: &mut Effects) -> Response {
        if let Err(problem) = self.store.save(PROFILE_NAME, &profile.to_toml()) {
            return error(ErrorCode::Failed, format!("The profile could not be saved: {problem}"));
        }
        self.profile = profile;
        self.profile_problem = None;
        self.configure_enforcer();
        self.enforcer.reconcile();
        effects.profile = true;
        effects.states = true;
        Response::Ok
    }

    /// Why `tweak` cannot be used on this Windows build, if so.
    fn inapplicable(&self, tweak: &Tweak) -> Option<String> {
        tweak.min_build.filter(|min| self.os_build < *min).map(|min| {
            format!("needs Windows build {min} or later (this is build {})", self.os_build)
        })
    }

    /// Hands the enforcer the current definitions and what is enabled.
    fn configure_enforcer(&mut self) {
        let tweak_entries = self.catalog.tweaks.iter().map(|tweak| Entry {
            id: tweak.id.clone(),
            triggers: (0..tweak.actions.len()).map(|index| tweak.triggers_for(index)).collect(),
            actions: tweak.actions.clone(),
            audit: tweak.audit,
            not_applicable: self.inapplicable(tweak),
        });
        let custom_entries = self.profile.custom.iter().map(|custom| Entry {
            id: custom.id.clone(),
            triggers: vec![custom.effective_triggers()],
            actions: vec![custom.action.clone()],
            audit: false,
            not_applicable: None,
        });
        let entries: Vec<Entry> = tweak_entries.chain(custom_entries).collect();
        let enabled: HashSet<String> = self
            .profile
            .enabled
            .iter()
            .cloned()
            .chain(self.profile.custom.iter().filter(|c| c.enabled).map(|c| c.id.clone()))
            .collect();
        self.enforcer.configure(entries, enabled);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syspilot_engine::fake::FakeOs;
    use syspilot_engine::os::{Hive, RegValue, reg_type};
    use syspilot_engine::store::MemoryStateStore;
    use syspilot_proto::action::{Action, RegData, RegistryAction, Scope};
    use syspilot_proto::message::{ActionState, EntryStatus};
    use syspilot_proto::profile::{LoggingConfig, UpdateMode};

    const KEY: &str = r"SOFTWARE\SysPilot-Test\Core";
    const CATALOG: &str = r#"
[[tweak]]
id = "other.first"
title = "First scratch value"
summary = "Sets the first scratch value."
level = "simple"
category = "other"
source = "https://learn.microsoft.com/windows/"
[[tweak.action]]
registry = { scope = "machine", key = 'SOFTWARE\SysPilot-Test\Core', name = "First", data = { dword = 1 } }

[[tweak]]
id = "other.future"
title = "Needs a future build"
summary = "Only applies to a later Windows build."
level = "advanced"
category = "other"
min-build = 99999
source = "https://learn.microsoft.com/windows/"
[[tweak.action]]
registry = { scope = "machine", key = 'SOFTWARE\SysPilot-Test\Core', name = "Future", data = { dword = 1 } }

[[tweak]]
id = "security.check"
title = "A posture check"
summary = "Reports whether the check value is set."
level = "advanced"
category = "security"
direction = "hardens"
audit = true
source = "https://learn.microsoft.com/windows/"
[[tweak.action]]
registry = { scope = "machine", key = 'SOFTWARE\SysPilot-Test\Core', name = "Check", data = { dword = 1 } }
"#;

    struct Fixture {
        fake: FakeOs,
        store: MemoryStateStore,
        core: Core<MemoryStateStore>,
    }

    impl Fixture {
        fn new() -> Self {
            Self::with_store(MemoryStateStore::default(), FakeOs::default(), true)
        }

        fn with_store(store: MemoryStateStore, fake: FakeOs, console: bool) -> Self {
            let catalog = Catalog::parse(CATALOG).unwrap();
            let mut core = Core::new(fake.os(), store.clone(), catalog, 26100, console);
            core.boot();
            Self { fake, store, core }
        }

        fn ok(&mut self, request: Request) -> Effects {
            match self.core.handle(request.clone(), true) {
                (Response::Ok, effects) => effects,
                (other, _) => panic!("expected Ok for {request:?} | received: {other:?}"),
            }
        }

        fn refused(&mut self, request: Request, elevated: bool) -> ServiceError {
            match self.core.handle(request.clone(), elevated) {
                (Response::Error(error), effects) => {
                    assert_eq!(effects, Effects::default(), "a refused request has no effect");
                    error
                }
                (other, _) => panic!("expected an error for {request:?} | received: {other:?}"),
            }
        }

        fn status(&mut self, id: &str) -> EntryStatus {
            let (Response::States(states), _) = self.core.handle(Request::States, false) else {
                panic!("expected States");
            };
            states.into_iter().find(|status| status.id == id).expect("the entry has a status")
        }

        fn value(&self, name: &str) -> Option<RegValue> {
            self.fake.registry.value(&Hive::Machine, KEY, name)
        }
    }

    fn dword(value: u32) -> RegValue {
        RegValue { kind: reg_type::DWORD, data: value.to_le_bytes().to_vec() }
    }

    fn custom(id: &str, enabled: bool) -> CustomAction {
        CustomAction {
            id: id.into(),
            title: "Scratch".into(),
            enabled,
            action: Action::Registry(RegistryAction {
                scope: Scope::Machine,
                key: KEY.into(),
                name: "Custom".into(),
                data: RegData::Dword(5),
            }),
            triggers: vec![],
        }
    }

    fn set_tweak(id: &str, enabled: bool) -> Request {
        Request::SetTweak { id: id.into(), enabled }
    }

    #[test]
    fn enabling_a_tweak_applies_it_and_persists_the_profile() {
        let mut fixture = Fixture::new();
        let effects = fixture.ok(set_tweak("other.first", true));
        assert!(effects.states && effects.profile);
        assert_eq!(fixture.value("First"), Some(dword(1)));
        assert!(fixture.status("other.first").enabled);
        let saved = Profile::parse(&fixture.store.load(PROFILE_NAME).unwrap()).unwrap();
        assert_eq!(saved.enabled, ["other.first"]);

        fixture.ok(set_tweak("other.first", false));
        assert_eq!(fixture.value("First"), None, "it did not exist before");
        assert!(!fixture.status("other.first").enabled);
    }

    #[test]
    fn enabling_twice_keeps_one_profile_entry() {
        let mut fixture = Fixture::new();
        fixture.ok(set_tweak("other.first", true));
        fixture.ok(set_tweak("other.first", true));
        assert_eq!(fixture.core.profile().enabled, ["other.first"]);
    }

    #[test]
    fn every_privileged_request_is_refused_without_elevation() {
        let mut fixture = Fixture::new();
        let privileged = [
            Request::Profile,
            Request::History { limit: 10 },
            set_tweak("other.first", true),
            Request::PutCustom(custom("custom.x", true)),
            Request::RemoveCustom { id: "custom.x".into() },
            Request::Reapply { id: "other.first".into() },
            Request::SetProfile(Profile::default()),
            Request::SetLogging(LoggingConfig::default()),
            Request::SetFans(vec![]),
            Request::ClearHistory,
            Request::Shutdown,
            Request::SetUpdates(UpdateMode::Automatic),
            Request::CheckForUpdate,
            Request::InstallUpdate,
            Request::SetTuning(vec![]),
            Request::ApplyTuning,
        ];
        for request in privileged {
            assert!(request.requires_elevation(), "{request:?}");
            let error = fixture.refused(request, false);
            assert_eq!(error.code, ErrorCode::AccessDenied);
        }
        assert_eq!(fixture.fake.registry.engine_writes(), 0);
        assert_eq!(fixture.store.load(PROFILE_NAME), None, "nothing was saved");
    }

    #[test]
    fn open_requests_work_without_elevation() {
        let mut fixture = Fixture::new();
        assert!(matches!(fixture.core.handle(Request::Catalog, false).0, Response::Catalog(_)));
        assert!(matches!(fixture.core.handle(Request::States, false).0, Response::States(_)));
    }

    #[test]
    fn unknown_audit_and_inapplicable_tweaks_cannot_be_enabled() {
        let mut fixture = Fixture::new();
        assert_eq!(fixture.refused(set_tweak("other.nope", true), true).code, ErrorCode::NotFound);
        let audit = fixture.refused(set_tweak("security.check", true), true);
        assert_eq!(audit.code, ErrorCode::Invalid);
        assert_eq!(audit.message, "A posture check is a check and cannot be turned on.");
        let future = fixture.refused(set_tweak("other.future", true), true);
        assert_eq!(future.code, ErrorCode::Unsupported);
        assert_eq!(
            future.message,
            "Needs a future build: needs Windows build 99999 or later (this is build 26100)."
        );
        assert!(fixture.core.profile().enabled.is_empty());
        assert!(matches!(fixture.status("other.future").state, ActionState::NotApplicable { .. }));
    }

    #[test]
    fn custom_action_lifecycle() {
        let mut fixture = Fixture::new();
        fixture.ok(Request::PutCustom(custom("custom.scratch", true)));
        assert_eq!(fixture.value("Custom"), Some(dword(5)));
        assert_eq!(fixture.status("custom.scratch").state, ActionState::Compliant);

        // Replacing with a disabled definition reverts it.
        fixture.ok(Request::PutCustom(custom("custom.scratch", false)));
        assert_eq!(fixture.value("Custom"), None);
        assert_eq!(fixture.core.profile().custom.len(), 1);

        fixture.ok(Request::PutCustom(custom("custom.scratch", true)));
        fixture.ok(Request::RemoveCustom { id: "custom.scratch".into() });
        assert_eq!(fixture.value("Custom"), None);
        assert!(fixture.core.profile().custom.is_empty());
        let error = fixture.refused(Request::RemoveCustom { id: "custom.scratch".into() }, true);
        assert_eq!(error.code, ErrorCode::NotFound);
    }

    #[test]
    fn invalid_custom_action_is_rejected_with_the_field() {
        let mut fixture = Fixture::new();
        let error = fixture.refused(Request::PutCustom(custom("not-custom", true)), true);
        assert_eq!(error.code, ErrorCode::Invalid);
        assert!(error.message.starts_with("(profile.custom[not-custom].id)"), "{}", error.message);
    }

    #[test]
    fn replacing_the_profile_reverts_and_applies_the_difference() {
        let mut fixture = Fixture::new();
        fixture.ok(set_tweak("other.first", true));
        let mut profile = Profile::default();
        profile.custom.push(custom("custom.scratch", true));
        fixture.ok(Request::SetProfile(profile));
        assert_eq!(fixture.value("First"), None, "no longer enabled");
        assert_eq!(fixture.value("Custom"), Some(dword(5)));
    }

    #[test]
    fn profile_with_unknown_or_audit_ids_is_rejected_whole() {
        let mut fixture = Fixture::new();
        for id in ["other.nope", "security.check"] {
            let profile =
                Profile { enabled: vec!["other.first".into(), id.into()], ..Profile::default() };
            let error = fixture.refused(Request::SetProfile(profile), true);
            assert_eq!(error.code, ErrorCode::Invalid, "{id}");
            assert!(error.message.contains(id), "{}", error.message);
        }
        assert_eq!(fixture.value("First"), None, "nothing from a rejected profile is applied");
    }

    #[test]
    fn logging_settings_are_validated_and_flagged() {
        let mut fixture = Fixture::new();
        let logging = LoggingConfig { enabled: true, ..LoggingConfig::default() };
        let effects = fixture.ok(Request::SetLogging(logging.clone()));
        assert!(effects.logging);
        assert_eq!(fixture.core.profile().logging, logging);

        let invalid = LoggingConfig { interval_secs: 0, ..LoggingConfig::default() };
        let error = fixture.refused(Request::SetLogging(invalid), true);
        assert_eq!(error.code, ErrorCode::Invalid);
    }

    #[test]
    fn the_update_mode_is_kept_in_the_profile_and_flagged() {
        let mut fixture = Fixture::new();
        assert_eq!(fixture.core.profile().updates, UpdateMode::Notify, "the default");
        let effects = fixture.ok(Request::SetUpdates(UpdateMode::Automatic));
        assert!(effects.updates && effects.profile);
        let saved = Profile::parse(&fixture.store.load(PROFILE_NAME).unwrap()).unwrap();
        assert_eq!(saved.updates, UpdateMode::Automatic);

        // A whole profile flags it only when it differs.
        let same = Profile { updates: UpdateMode::Automatic, ..Profile::default() };
        assert!(!fixture.ok(Request::SetProfile(same)).updates);
        assert!(fixture.ok(Request::SetProfile(Profile::default())).updates);
    }

    fn tuning_setting(device: &str, limit: i32) -> syspilot_proto::tuning::TuningSetting {
        syspilot_proto::tuning::TuningSetting {
            device: device.into(),
            enabled: true,
            values: [("power-limit".to_owned(), limit)].into(),
        }
    }

    #[test]
    fn tuning_settings_are_kept_in_the_profile_and_flagged() {
        let mut fixture = Fixture::new();
        let setting = tuning_setting("gpu.amd-699f.bus-3", -20);
        let effects = fixture.ok(Request::SetTuning(vec![setting.clone()]));
        assert!(effects.tuning && effects.profile);
        let saved = Profile::parse(&fixture.store.load(PROFILE_NAME).unwrap()).unwrap();
        assert_eq!(saved.tuning, std::slice::from_ref(&setting));

        // A whole profile flags them only when they differ.
        let same = Profile { tuning: vec![setting], ..Profile::default() };
        assert!(!fixture.ok(Request::SetProfile(same)).tuning);
        assert!(fixture.ok(Request::SetProfile(Profile::default())).tuning);
    }

    #[test]
    fn tuning_settings_of_the_wrong_shape_are_rejected_with_the_field() {
        let mut fixture = Fixture::new();
        let twice = vec![tuning_setting("gpu.a", 1), tuning_setting("gpu.a", 2)];
        let error = fixture.refused(Request::SetTuning(twice), true);
        assert_eq!(error.code, ErrorCode::Invalid);
        assert_eq!(
            error.message,
            "(profile.tuning): expected one setting per device | received: \"gpu.a\""
        );
        assert!(fixture.core.profile().tuning.is_empty());
    }

    #[test]
    fn fan_curves_are_refused_while_fan_control_is_off() {
        let mut fixture = Fixture::new();
        fixture.core.fan_control = false;
        let curve = syspilot_proto::profile::FanCurve {
            channel: "fan1".into(),
            enabled: true,
            sources: vec!["cpu.package.temp".into()],
            points: vec![
                syspilot_proto::profile::CurvePoint(40, 30),
                syspilot_proto::profile::CurvePoint(80, 100),
            ],
            hysteresis_c: 3,
            min_duty_pct: 20,
            ramp_pct_per_sec: 10,
        };
        let error = fixture.refused(Request::SetFans(vec![curve]), true);
        assert_eq!(error.code, ErrorCode::Unsupported);
        fixture.ok(Request::SetFans(vec![]));
    }

    #[test]
    fn shutdown_is_honoured_only_in_console_mode() {
        let mut console = Fixture::new();
        assert!(console.ok(Request::Shutdown).stop);

        let mut service =
            Fixture::with_store(MemoryStateStore::default(), FakeOs::default(), false);
        let error = service.refused(Request::Shutdown, true);
        assert_eq!(error.code, ErrorCode::Unsupported);
    }

    #[test]
    fn failed_profile_save_changes_nothing() {
        let mut fixture = Fixture::new();
        fixture.store.fail_saves(true);
        let error = fixture.refused(set_tweak("other.first", true), true);
        assert_eq!(error.code, ErrorCode::Failed);
        assert_eq!(fixture.value("First"), None);
        assert!(fixture.core.profile().enabled.is_empty());
    }

    #[test]
    fn profile_survives_a_restart() {
        let mut fixture = Fixture::new();
        fixture.ok(set_tweak("other.first", true));
        // Something changes the value while the service is down.
        fixture.fake.registry.set_external(&Hive::Machine, KEY, "First", Some(dword(0)));

        let mut restarted = Fixture::with_store(fixture.store.clone(), fixture.fake.clone(), true);
        assert_eq!(restarted.value("First"), Some(dword(1)), "restored at boot");
        assert!(restarted.status("other.first").enabled);
        assert!(restarted.core.problems().is_empty());
    }

    #[test]
    fn unreadable_profile_is_set_aside_and_everything_is_reverted() {
        let mut fixture = Fixture::new();
        fixture.ok(set_tweak("other.first", true));
        fixture.store.save(PROFILE_NAME, "schema = 1\nenabled = 7").unwrap();

        let restarted = Fixture::with_store(fixture.store.clone(), fixture.fake.clone(), true);
        assert_eq!(restarted.value("First"), None, "nothing is enabled, so it is reverted");
        let problems = restarted.core.problems();
        assert_eq!(problems.len(), 1);
        assert!(problems[0].starts_with("The saved profile could not be read"), "{}", problems[0]);
        assert_eq!(
            restarted.store.load(REJECTED_PROFILE_NAME).as_deref(),
            Some("schema = 1\nenabled = 7")
        );
    }

    #[test]
    fn history_limit_is_capped_and_clearable() {
        let mut fixture = Fixture::new();
        fixture.ok(set_tweak("other.first", true));
        let (Response::History(history), _) =
            fixture.core.handle(Request::History { limit: u32::MAX }, true)
        else {
            panic!("expected History");
        };
        assert_eq!(history.len(), 1);
        fixture.ok(Request::ClearHistory);
        let (Response::History(history), _) =
            fixture.core.handle(Request::History { limit: 10 }, true)
        else {
            panic!("expected History");
        };
        assert!(history.is_empty());
    }

    #[test]
    fn reapply_needs_a_known_entry() {
        let mut fixture = Fixture::new();
        let error = fixture.refused(Request::Reapply { id: "other.nope".into() }, true);
        assert_eq!(error.code, ErrorCode::NotFound);
        fixture.ok(Request::Reapply { id: "other.first".into() });
    }

    #[test]
    fn host_requests_are_not_answered_by_the_core() {
        let mut fixture = Fixture::new();
        let host_requests = [
            Request::Status,
            Request::Sensors,
            Request::Hello { protocol: 1 },
            Request::CheckForUpdate,
            Request::InstallUpdate,
            Request::ApplyTuning,
        ];
        for request in host_requests {
            assert_eq!(fixture.refused(request, true).code, ErrorCode::Invalid);
        }
    }
}
