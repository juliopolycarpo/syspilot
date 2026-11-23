//! The enforcement state machine.
//!
//! The enforcer owns the set of *applied units*: one per (entry, action,
//! target) it has changed, each with the action as it was applied and the
//! snapshot of what it replaced. Everything else follows from comparing
//! that set with what the profile wants:
//!
//! - [`Enforcer::reconcile`] reverts units that are no longer wanted and
//!   applies wanted ones that are missing.
//! - [`Enforcer::trigger`] re-evaluates units when something happened
//!   (boot, logon, resume, a registry change, a timer).
//!
//! A unit that keeps drifting is not fought in a loop: after a few restores
//! in a short window it becomes *contested*, and is retried once per
//! exponentially growing back-off period.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use syspilot_proto::action::{Action, Trigger};
use syspilot_proto::message::{ActionState, ActionStatus, EntryStatus, HistoryEvent, HistoryKind};

use crate::ops::{self, Detection, Snapshot, Target, WatchSpec};
use crate::os::{Os, OsError, OsErrorKind};

/// Name under which the applied units are stored.
pub const APPLIED_STATE_NAME: &str = "applied.toml";
/// Name under which the history is stored.
pub const HISTORY_STATE_NAME: &str = "history.toml";

/// Durable storage for the enforcer's state. The service backs this with
/// files only administrators can write.
pub trait StateStore {
    /// The stored document, or `None` when nothing was stored yet.
    fn load(&self, name: &str) -> Option<String>;
    /// Replaces the stored document.
    fn save(&self, name: &str, content: &str) -> Result<(), String>;
}

/// One enforceable entry: a catalog tweak or a custom action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Tweak or custom action ID.
    pub id: String,
    /// The settings that make up the entry.
    pub actions: Vec<Action>,
    /// Triggers per action, same length as `actions`.
    pub triggers: Vec<Vec<Trigger>>,
    /// Detect-only entry that is never applied.
    pub audit: bool,
    /// Why the entry cannot be used on this system, if so.
    pub not_applicable: Option<String>,
}

/// Tunables of the contested-value handling.
#[derive(Debug, Clone, Copy)]
pub struct EnforcerConfig {
    /// Restores allowed within `contest_window` before a unit is contested.
    pub contest_threshold: usize,
    /// Window in which restores are counted.
    pub contest_window: Duration,
    /// First back-off period; doubles on every further contest.
    pub backoff_base: Duration,
    /// Longest back-off period.
    pub backoff_max: Duration,
    /// History events kept.
    pub history_cap: usize,
}

impl Default for EnforcerConfig {
    fn default() -> Self {
        Self {
            contest_threshold: 3,
            contest_window: Duration::from_secs(60),
            backoff_base: Duration::from_secs(60),
            backoff_max: Duration::from_secs(3600),
            history_cap: 500,
        }
    }
}

/// Something that happened and may require re-evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerEvent {
    /// The service started.
    Boot,
    /// A user logged on.
    Logon {
        /// SID of the user.
        sid: String,
    },
    /// The machine resumed from sleep.
    Resume,
    /// A watched registry key changed. Carries a watch previously returned
    /// by [`Enforcer::watches`].
    Change(WatchSpec),
    /// The deadline from [`Enforcer::next_deadline`] passed.
    Timer,
    /// The user asked for an immediate retry of one entry.
    Reapply {
        /// Entry ID.
        id: String,
    },
}

/// One thing the enforcer changed, as persisted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct AppliedUnit {
    entry: String,
    index: u32,
    /// SID for per-user units, `None` for machine units.
    user: Option<String>,
    /// The action exactly as applied. Revert uses this, not the current
    /// definition, so a catalog update cannot break a revert.
    action: Action,
    snapshot: Snapshot,
    /// The unit is no longer wanted but could not be reverted yet (its
    /// user is logged off, or the revert failed).
    #[serde(default)]
    revert_pending: bool,
    #[serde(default)]
    drift_count: u32,
    last_drift_unix: Option<u64>,
}

impl AppliedUnit {
    fn target(&self) -> Target {
        self.user.clone().map_or(Target::Machine, Target::User)
    }

    fn key(&self) -> UnitKey {
        (self.entry.clone(), self.index, self.target())
    }
}

#[derive(Default, Serialize, Deserialize)]
struct AppliedFile {
    #[serde(default, rename = "unit")]
    units: Vec<AppliedUnit>,
}

#[derive(Default, Serialize, Deserialize)]
struct HistoryFile {
    #[serde(default, rename = "event")]
    events: Vec<HistoryEvent>,
}

type UnitKey = (String, u32, Target);

/// Why a unit is currently not in its desired state through no drift.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Problem {
    NotEnforceable(String),
    Failed(String),
}

impl Problem {
    fn from_error(error: &OsError) -> Self {
        match error.kind {
            OsErrorKind::AccessDenied => Self::NotEnforceable(error.to_string()),
            _ => Self::Failed(error.to_string()),
        }
    }

    fn state(&self) -> ActionState {
        match self {
            Self::NotEnforceable(reason) => ActionState::NotEnforceable { reason: reason.clone() },
            Self::Failed(error) => ActionState::Failed { error: error.clone() },
        }
    }

    fn history_kind(&self) -> HistoryKind {
        match self {
            Self::NotEnforceable(_) => HistoryKind::Refused,
            Self::Failed(_) => HistoryKind::Failed,
        }
    }
}

/// Volatile per-unit state. Lost on restart, which only resets back-offs.
#[derive(Debug, Default)]
struct Runtime {
    /// Monotonic times of recent restores, oldest first.
    recent_restores: VecDeque<Duration>,
    /// While set and in the future, the unit is contested and left alone.
    contested_until: Option<Duration>,
    /// After a contested retry: a drift before this time contests again.
    probation_until: Option<Duration>,
    /// Number of consecutive contests; selects the back-off period.
    contest_level: u32,
    problem: Option<Problem>,
    /// Next run of an interval trigger.
    next_interval: Option<Duration>,
}

/// The enforcement engine.
pub struct Enforcer {
    os: Os,
    store: Box<dyn StateStore>,
    config: EnforcerConfig,
    entries: Vec<Entry>,
    enabled: BTreeSet<String>,
    applied: Vec<AppliedUnit>,
    runtime: HashMap<UnitKey, Runtime>,
    history: VecDeque<HistoryEvent>,
    /// Resolved watches and the units each one covers.
    watch_map: HashMap<WatchSpec, Vec<UnitKey>>,
    /// Set when applied units or history changed and must be saved.
    applied_dirty: bool,
    history_dirty: bool,
    store_error: Option<String>,
}

impl Enforcer {
    /// Creates an enforcer and loads its persisted state.
    pub fn new(os: Os, store: Box<dyn StateStore>, config: EnforcerConfig) -> Self {
        let mut enforcer = Self {
            os,
            store,
            config,
            entries: Vec::new(),
            enabled: BTreeSet::new(),
            applied: Vec::new(),
            runtime: HashMap::new(),
            history: VecDeque::new(),
            watch_map: HashMap::new(),
            applied_dirty: false,
            history_dirty: false,
            store_error: None,
        };
        if let Some(text) = enforcer.store.load(HISTORY_STATE_NAME) {
            // A damaged history is only a loss of past log lines.
            let file: HistoryFile = toml::from_str(&text).unwrap_or_default();
            enforcer.history = file.events.into();
        }
        if let Some(text) = enforcer.store.load(APPLIED_STATE_NAME) {
            match toml::from_str::<AppliedFile>(&text) {
                Ok(file) => enforcer.applied = file.units,
                Err(error) => {
                    // The snapshots are gone: say so rather than pretend
                    // nothing was ever applied.
                    enforcer.record(
                        "",
                        HistoryKind::Failed,
                        format!("The saved original settings could not be read, so earlier changes cannot be reverted automatically: {error}"),
                    );
                }
            }
        }
        enforcer
    }

    /// Replaces the entry definitions and the set of enabled entry IDs.
    /// Changes nothing on the system; call [`Enforcer::reconcile`] next.
    pub fn configure(&mut self, entries: Vec<Entry>, enabled: impl IntoIterator<Item = String>) {
        self.entries = entries;
        self.enabled = enabled.into_iter().collect();
    }

    /// Makes the applied units match the enabled entries: reverts what is
    /// no longer wanted, applies what is missing. Returns whether anything
    /// changed.
    pub fn reconcile(&mut self) -> bool {
        let mut changed = self.revert_unwanted();
        changed |= self.apply_missing();
        self.flush();
        changed
    }

    /// Re-evaluates the units `event` concerns. Returns whether any state
    /// visible to clients changed.
    pub fn trigger(&mut self, event: &TriggerEvent) -> bool {
        let mut changed = false;
        let keys: Vec<(UnitKey, bool)> = match event {
            TriggerEvent::Boot => {
                changed |= self.reconcile();
                self.units_with(|triggers, _| {
                    triggers.iter().any(|t| matches!(t, Trigger::Boot | Trigger::Change))
                })
            }
            TriggerEvent::Logon { sid } => {
                changed |= self.reconcile();
                self.units_with(|triggers, target| match target {
                    Target::User(user) => {
                        user == sid
                            && triggers
                                .iter()
                                .any(|t| matches!(t, Trigger::Logon | Trigger::Change))
                    }
                    Target::Machine => triggers.contains(&Trigger::Logon),
                })
            }
            TriggerEvent::Resume => {
                self.units_with(|triggers, _| triggers.contains(&Trigger::Resume))
            }
            TriggerEvent::Change(watch) => self
                .watch_map
                .get(watch)
                .map(|keys| keys.iter().map(|key| (key.clone(), false)).collect())
                .unwrap_or_default(),
            TriggerEvent::Timer => self.due_units(),
            TriggerEvent::Reapply { id } => self
                .applied
                .iter()
                .filter(|unit| unit.entry == *id && !unit.revert_pending)
                .map(|unit| (unit.key(), true))
                .collect(),
        };
        for (key, forced) in keys {
            changed |= self.enforce(&key, forced);
        }
        self.flush();
        changed
    }

    /// Current state of every entry, in definition order. Reads the system.
    pub fn status(&self) -> Vec<EntryStatus> {
        let users = self.os.sessions.interactive_user_sids();
        self.entries.iter().map(|entry| self.entry_status(entry, &users)).collect()
    }

    /// The registry keys to watch so that drift is noticed without polling.
    /// The result is also remembered for routing [`TriggerEvent::Change`].
    pub fn watches(&mut self) -> Vec<WatchSpec> {
        let mut map: HashMap<WatchSpec, Vec<UnitKey>> = HashMap::new();
        for unit in self.applied.iter().filter(|unit| !unit.revert_pending) {
            if !self.triggers_of(unit).contains(&Trigger::Change) {
                continue;
            }
            let Some(spec) = ops::watch_for(&unit.action, &unit.target()) else { continue };
            // A key that does not exist cannot be watched; its nearest
            // existing ancestor reports when the next level is created.
            let Ok(Some(existing)) = ops::nearest_existing_key(&self.os, &spec.hive, &spec.key)
            else {
                continue;
            };
            let subtree = spec.subtree && existing == spec.key;
            let resolved = WatchSpec { hive: spec.hive, key: existing, subtree };
            map.entry(resolved).or_default().push(unit.key());
        }
        let mut watches: Vec<WatchSpec> = map.keys().cloned().collect();
        watches.sort();
        self.watch_map = map;
        watches
    }

    /// Monotonic time of the next contested retry or interval run, if any.
    /// The caller arms a timer for it and sends [`TriggerEvent::Timer`].
    pub fn next_deadline(&self) -> Option<Duration> {
        self.applied
            .iter()
            .filter(|unit| !unit.revert_pending)
            .filter_map(|unit| self.runtime.get(&unit.key()))
            .flat_map(|runtime| [runtime.contested_until, runtime.next_interval])
            .flatten()
            .min()
    }

    /// The newest `limit` history events, newest first.
    pub fn history(&self, limit: usize) -> Vec<HistoryEvent> {
        self.history.iter().rev().take(limit).cloned().collect()
    }

    /// Deletes the history.
    pub fn clear_history(&mut self) {
        self.history.clear();
        self.history_dirty = true;
        self.flush();
    }

    /// The last failure to persist state, if the most recent save failed.
    pub fn store_error(&self) -> Option<&str> {
        self.store_error.as_deref()
    }

    /// Whether `id` names a known entry.
    pub fn has_entry(&self, id: &str) -> bool {
        self.entries.iter().any(|entry| entry.id == id)
    }

    fn entry(&self, id: &str) -> Option<&Entry> {
        self.entries.iter().find(|entry| entry.id == id)
    }

    /// Whether the entry should currently be enforced.
    fn is_wanted(&self, entry: &Entry) -> bool {
        self.enabled.contains(&entry.id) && !entry.audit && entry.not_applicable.is_none()
    }

    fn triggers_of(&self, unit: &AppliedUnit) -> Vec<Trigger> {
        self.entry(&unit.entry)
            .and_then(|entry| entry.triggers.get(unit.index as usize))
            .cloned()
            .unwrap_or_default()
    }

    fn units_with(&self, select: impl Fn(&[Trigger], &Target) -> bool) -> Vec<(UnitKey, bool)> {
        self.applied
            .iter()
            .filter(|unit| !unit.revert_pending)
            .filter(|unit| select(&self.triggers_of(unit), &unit.target()))
            .map(|unit| (unit.key(), false))
            .collect()
    }

    /// Units whose contested back-off or interval has elapsed.
    fn due_units(&self) -> Vec<(UnitKey, bool)> {
        let now = self.os.clock.monotonic();
        self.applied
            .iter()
            .filter(|unit| !unit.revert_pending)
            .filter(|unit| {
                self.runtime.get(&unit.key()).is_some_and(|runtime| {
                    runtime.contested_until.is_some_and(|until| now >= until)
                        || runtime.next_interval.is_some_and(|at| now >= at)
                })
            })
            .map(|unit| (unit.key(), false))
            .collect()
    }

    fn revert_unwanted(&mut self) -> bool {
        let users = self.os.sessions.interactive_user_sids();
        let mut changed = false;
        // Newest first, so later actions of an entry are undone before
        // earlier ones.
        for position in (0..self.applied.len()).rev() {
            let unit = &self.applied[position];
            let wanted = self.entry(&unit.entry).is_some_and(|entry| {
                self.is_wanted(entry)
                    && entry.actions.get(unit.index as usize) == Some(&unit.action)
            });
            if wanted {
                if unit.revert_pending {
                    // Enabled again before the revert happened.
                    self.applied[position].revert_pending = false;
                    self.applied_dirty = true;
                }
                continue;
            }
            let reachable = match &unit.user {
                Some(sid) => users.contains(sid),
                None => true,
            };
            if !reachable {
                if !unit.revert_pending {
                    self.applied[position].revert_pending = true;
                    self.applied_dirty = true;
                }
                continue;
            }
            let unit = self.applied[position].clone();
            match ops::revert(&self.os, &unit.action, &unit.target(), &unit.snapshot) {
                Ok(()) => {
                    self.applied.remove(position);
                    self.runtime.remove(&unit.key());
                    self.record(&unit.entry, HistoryKind::Reverted, unit.action.describe());
                }
                Err(error) => {
                    // Keep the snapshot and try again on the next reconcile.
                    if !unit.revert_pending {
                        self.applied[position].revert_pending = true;
                        self.record(
                            &unit.entry,
                            HistoryKind::Failed,
                            format!("Could not revert {}: {error}", unit.action.describe()),
                        );
                    }
                }
            }
            self.applied_dirty = true;
            changed = true;
        }
        changed
    }

    fn apply_missing(&mut self) -> bool {
        let users = self.os.sessions.interactive_user_sids();
        let mut wanted: Vec<(String, u32, Action, Target)> = Vec::new();
        for entry in self.entries.iter().filter(|entry| self.is_wanted(entry)) {
            for (index, action) in entry.actions.iter().enumerate() {
                let targets: Vec<Target> = if ops::is_per_user(action) {
                    users.iter().cloned().map(Target::User).collect()
                } else {
                    vec![Target::Machine]
                };
                for target in targets {
                    wanted.push((entry.id.clone(), index as u32, action.clone(), target));
                }
            }
        }
        let mut changed = false;
        for (entry, index, action, target) in wanted {
            let key = (entry, index, target);
            if self.applied.iter().any(|unit| unit.key() == key) {
                continue;
            }
            changed |= self.first_apply(key, action);
        }
        changed
    }

    /// Snapshots and applies a unit that has never been applied.
    fn first_apply(&mut self, key: UnitKey, action: Action) -> bool {
        let (entry, index, target) = key.clone();
        let detection = match ops::detect(&self.os, &action, &target) {
            Ok(detection) => detection,
            Err(error) => {
                return self.set_problem(&key, &action, Some(Problem::from_error(&error)));
            }
        };
        if matches!(detection, Detection::NotApplicable { .. }) {
            return false;
        }
        let snapshot = match ops::capture(&self.os, &action, &target) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return self.set_problem(&key, &action, Some(Problem::from_error(&error)));
            }
        };
        let user = match &target {
            Target::Machine => None,
            Target::User(sid) => Some(sid.clone()),
        };
        self.applied.push(AppliedUnit {
            entry: entry.clone(),
            index,
            user,
            action: action.clone(),
            snapshot,
            revert_pending: false,
            drift_count: 0,
            last_drift_unix: None,
        });
        // The snapshot must be durable before the system is changed: a
        // crash in between would otherwise lose the original state.
        self.applied_dirty = true;
        self.flush();

        let was = match &detection {
            Detection::Compliant { .. } => None,
            Detection::Different { current } => Some(current.clone()),
            _ => Some(String::new()),
        };
        if let Some(was) = was {
            if let Err(error) = ops::apply(&self.os, &action, &target) {
                // Nothing was changed, so there is nothing to revert later.
                self.applied.retain(|unit| unit.key() != key);
                self.applied_dirty = true;
                return self.set_problem(&key, &action, Some(Problem::from_error(&error)));
            }
            let detail = if was.is_empty() {
                action.describe()
            } else {
                format!("{} (was {was})", action.describe())
            };
            self.record(&entry, HistoryKind::Applied, detail);
        }
        self.set_problem(&key, &action, None);
        self.schedule_interval(&key);
        true
    }

    /// Re-evaluates one applied unit. `forced` ignores a contested back-off.
    fn enforce(&mut self, key: &UnitKey, forced: bool) -> bool {
        let Some(position) = self.applied.iter().position(|unit| unit.key() == *key) else {
            return false;
        };
        let action = self.applied[position].action.clone();
        let target = key.2.clone();
        let now = self.os.clock.monotonic();
        let window = self.config.contest_window;

        let runtime = self.runtime.entry(key.clone()).or_default();
        let was_contested = runtime.contested_until.is_some();
        if !forced && runtime.contested_until.is_some_and(|until| now < until) {
            return false;
        }
        self.schedule_interval(key);

        let detection = match ops::detect(&self.os, &action, &target) {
            Ok(detection) => detection,
            Err(error) => return self.set_problem(key, &action, Some(Problem::from_error(&error))),
        };
        let current = match detection {
            Detection::NotApplicable { .. } => return self.set_problem(key, &action, None),
            Detection::Compliant { .. } => {
                let runtime = self.runtime.entry(key.clone()).or_default();
                let mut changed = false;
                if runtime.contested_until.take().is_some() {
                    // Someone else put the desired value back. Stay wary
                    // for one window before trusting the unit again.
                    runtime.probation_until = Some(now + window);
                    changed = true;
                } else if runtime.probation_until.is_some_and(|until| now >= until) {
                    // It held for a whole window: the other writer is gone.
                    runtime.probation_until = None;
                    runtime.contest_level = 0;
                }
                changed |= self.set_problem(key, &action, None);
                return changed;
            }
            Detection::Unverifiable => None,
            Detection::Different { current } => Some(current),
        };

        if let Some(current) = &current {
            let runtime = self.runtime.entry(key.clone()).or_default();
            let on_probation = runtime.probation_until.is_some_and(|until| now < until);
            while runtime.recent_restores.front().is_some_and(|at| now.saturating_sub(*at) > window)
            {
                runtime.recent_restores.pop_front();
            }
            let retrying = was_contested || forced;
            let exhausted = runtime.recent_restores.len() >= self.config.contest_threshold;
            if !retrying && (on_probation || exhausted) {
                return self.contest(key, &action, current);
            }
            if retrying {
                // One attempt per back-off period, then watch it closely.
                runtime.contested_until = None;
                runtime.probation_until = Some(now + window);
                runtime.recent_restores.clear();
            } else {
                runtime.recent_restores.push_back(now);
            }
        }

        if let Err(error) = ops::apply(&self.os, &action, &target) {
            return self.set_problem(key, &action, Some(Problem::from_error(&error)));
        }
        self.set_problem(key, &action, None);
        if let Some(current) = current {
            let unix = self.os.clock.unix_secs();
            let unit = &mut self.applied[position];
            unit.drift_count += 1;
            unit.last_drift_unix = Some(unix);
            self.applied_dirty = true;
            let detail = format!("{} (was {current})", action.describe());
            self.record(&key.0, HistoryKind::Restored, detail);
        }
        true
    }

    /// Marks a unit contested and schedules the next attempt.
    fn contest(&mut self, key: &UnitKey, action: &Action, current: &str) -> bool {
        let now = self.os.clock.monotonic();
        let runtime = self.runtime.entry(key.clone()).or_default();
        runtime.contest_level = runtime.contest_level.saturating_add(1);
        let doublings = (runtime.contest_level - 1).min(16);
        let backoff =
            self.config.backoff_base.saturating_mul(1 << doublings).min(self.config.backoff_max);
        runtime.contested_until = Some(now + backoff);
        runtime.probation_until = None;
        runtime.recent_restores.clear();
        let detail = format!(
            "{} keeps being changed to {current} by another program. Next attempt in {} s.",
            action.describe(),
            backoff.as_secs()
        );
        self.record(&key.0, HistoryKind::Contested, detail);
        true
    }

    /// Updates a unit's problem and logs it when it is new. Returns whether
    /// it changed.
    fn set_problem(&mut self, key: &UnitKey, action: &Action, problem: Option<Problem>) -> bool {
        let runtime = self.runtime.entry(key.clone()).or_default();
        if runtime.problem == problem {
            return false;
        }
        runtime.problem = problem.clone();
        if let Some(problem) = problem {
            let reason = match &problem {
                Problem::NotEnforceable(reason) | Problem::Failed(reason) => reason,
            };
            let detail = format!("{}: {reason}", action.describe());
            self.record(&key.0, problem.history_kind(), detail);
        }
        true
    }

    fn schedule_interval(&mut self, key: &UnitKey) {
        let interval = self
            .entry(&key.0)
            .and_then(|entry| entry.triggers.get(key.1 as usize))
            .and_then(|triggers| {
                triggers.iter().find_map(|trigger| match trigger {
                    Trigger::Interval(secs) => Some(Duration::from_secs(u64::from(*secs))),
                    _ => None,
                })
            });
        let now = self.os.clock.monotonic();
        let runtime = self.runtime.entry(key.clone()).or_default();
        runtime.next_interval = interval.map(|interval| now + interval);
    }

    fn record(&mut self, entry: &str, kind: HistoryKind, detail: String) {
        let unix_secs = self.os.clock.unix_secs();
        self.history.push_back(HistoryEvent { unix_secs, id: entry.to_owned(), kind, detail });
        while self.history.len() > self.config.history_cap {
            self.history.pop_front();
        }
        self.history_dirty = true;
    }

    /// Persists whatever changed since the last flush.
    fn flush(&mut self) {
        // A flag is cleared only by a successful save, so a failed one is
        // retried on the next flush.
        let mut result = Ok(());
        if self.applied_dirty {
            let file = AppliedFile { units: self.applied.clone() };
            result = toml::to_string(&file)
                .map_err(|error| error.to_string())
                .and_then(|text| self.store.save(APPLIED_STATE_NAME, &text));
            self.applied_dirty = result.is_err();
        }
        if self.history_dirty && result.is_ok() {
            let file = HistoryFile { events: self.history.iter().cloned().collect() };
            result = toml::to_string(&file)
                .map_err(|error| error.to_string())
                .and_then(|text| self.store.save(HISTORY_STATE_NAME, &text));
            self.history_dirty = result.is_err();
        }
        self.store_error = result.err();
    }

    fn entry_status(&self, entry: &Entry, users: &[String]) -> EntryStatus {
        let enabled = self.enabled.contains(&entry.id);
        let actions: Vec<ActionStatus> = entry
            .actions
            .iter()
            .enumerate()
            .map(|(index, action)| self.action_status(entry, index as u32, action, users))
            .collect();
        let units = || self.applied.iter().filter(|unit| unit.entry == entry.id);
        EntryStatus {
            id: entry.id.clone(),
            enabled,
            state: combine(actions.iter().map(|status| &status.state)),
            actions,
            drift_count: units().map(|unit| unit.drift_count).sum(),
            last_drift_unix: units().filter_map(|unit| unit.last_drift_unix).max(),
        }
    }

    fn action_status(
        &self,
        entry: &Entry,
        index: u32,
        action: &Action,
        users: &[String],
    ) -> ActionStatus {
        if let Some(reason) = &entry.not_applicable {
            let state = ActionState::NotApplicable { reason: reason.clone() };
            return ActionStatus { state, current: String::new() };
        }
        let targets: Vec<Target> = if ops::is_per_user(action) {
            users.iter().cloned().map(Target::User).collect()
        } else {
            vec![Target::Machine]
        };
        let statuses: Vec<ActionStatus> = targets
            .iter()
            .map(|target| self.target_status(&(entry.id.clone(), index, target.clone()), action))
            .collect();
        let state = combine(statuses.iter().map(|status| &status.state));
        // With several users the first non-compliant value is the
        // informative one; otherwise any.
        let current = statuses
            .iter()
            .find(|status| status.state != ActionState::Compliant)
            .or(statuses.first())
            .map(|status| status.current.clone())
            .unwrap_or_default();
        ActionStatus { state, current }
    }

    fn target_status(&self, key: &UnitKey, action: &Action) -> ActionStatus {
        let runtime = self.runtime.get(key);
        let enforced = self.enabled.contains(&key.0);
        let problem = runtime.and_then(|runtime| runtime.problem.as_ref()).filter(|_| enforced);
        let (state, current) = match ops::detect(&self.os, action, &key.2) {
            Err(error) => (Problem::from_error(&error).state(), String::new()),
            Ok(Detection::NotApplicable { reason }) => {
                (ActionState::NotApplicable { reason }, String::new())
            }
            Ok(Detection::Compliant { current }) => (ActionState::Compliant, current),
            Ok(Detection::Unverifiable) => {
                let applied = self.applied.iter().any(|unit| unit.key() == *key);
                let state = match problem {
                    Some(problem) => problem.state(),
                    None if applied => ActionState::Compliant,
                    None => ActionState::Different,
                };
                (state, String::new())
            }
            Ok(Detection::Different { current }) => {
                let now = self.os.clock.monotonic();
                let contested = runtime
                    .and_then(|runtime| runtime.contested_until)
                    .filter(|_| enforced)
                    .map(|until| until.saturating_sub(now).as_secs() as u32);
                let state = match (contested, problem) {
                    (Some(retry_in_secs), _) => ActionState::Contested { retry_in_secs },
                    (None, Some(problem)) => problem.state(),
                    (None, None) => ActionState::Different,
                };
                (state, current)
            }
        };
        ActionStatus { state, current }
    }
}

/// Combines the states of an entry's parts into one: the most severe
/// problem wins; parts with nothing to do do not hide compliant ones.
fn combine<'a>(states: impl Iterator<Item = &'a ActionState>) -> ActionState {
    fn rank(state: &ActionState) -> u8 {
        match state {
            ActionState::Failed { .. } => 6,
            ActionState::NotEnforceable { .. } => 5,
            ActionState::Contested { .. } => 4,
            ActionState::Different => 3,
            ActionState::Compliant => 2,
            ActionState::NotApplicable { .. } => 1,
            ActionState::Unknown => 0,
        }
    }
    states.max_by_key(|state| rank(state)).cloned().unwrap_or(ActionState::Unknown)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeOs;
    use crate::os::{Hive, RegValue, Registry, reg_type};
    use crate::store::MemoryStateStore;
    use syspilot_proto::action::{
        CommandAction, CommandLine, RegData, RegistryAction, RunAs, Scope,
    };

    const KEY: &str = r"SOFTWARE\SysPilot-Test\Enforcer";
    const ALICE: &str = "S-1-5-21-1001";
    const BOB: &str = "S-1-5-21-1002";

    fn dword(value: u32) -> RegValue {
        RegValue { kind: reg_type::DWORD, data: value.to_le_bytes().to_vec() }
    }

    fn registry_entry(id: &str, scope: Scope, name: &str, value: u32) -> Entry {
        let action = Action::Registry(RegistryAction {
            scope,
            key: KEY.into(),
            name: name.into(),
            data: RegData::Dword(value),
        });
        let triggers = vec![action.default_triggers()];
        Entry { id: id.into(), actions: vec![action], triggers, audit: false, not_applicable: None }
    }

    /// A machine-wide fixture: one entry `test.value` wanting `Value = 1`
    /// where the system currently holds 7.
    struct Fixture {
        fake: FakeOs,
        store: MemoryStateStore,
        enforcer: Enforcer,
    }

    impl Fixture {
        fn new() -> Self {
            let fake = FakeOs::default();
            fake.registry.set_external(&Hive::Machine, KEY, "Value", Some(dword(7)));
            fake.registry.clear_notifications();
            let store = MemoryStateStore::default();
            let enforcer =
                Enforcer::new(fake.os(), Box::new(store.clone()), EnforcerConfig::default());
            let mut fixture = Self { fake, store, enforcer };
            fixture.configure(&["test.value"]);
            fixture
        }

        fn entries() -> Vec<Entry> {
            vec![registry_entry("test.value", Scope::Machine, "Value", 1)]
        }

        fn configure(&mut self, enabled: &[&str]) {
            self.enforcer.configure(Self::entries(), enabled.iter().map(|id| (*id).to_owned()));
        }

        fn value(&self) -> Option<RegValue> {
            self.fake.registry.value(&Hive::Machine, KEY, "Value")
        }

        fn state(&self) -> ActionState {
            self.enforcer.status()[0].state.clone()
        }

        fn kinds(&self) -> Vec<HistoryKind> {
            self.enforcer.history(100).iter().rev().map(|event| event.kind).collect()
        }

        /// Delivers pending registry notifications the way the service
        /// does, until the system is quiet. Returns how many were delivered.
        fn pump_changes(&mut self) -> usize {
            let mut delivered = 0;
            while let Some((hive, key)) = self.fake.registry.take_notification() {
                delivered += 1;
                assert!(delivered < 1000, "enforcement never settled: this is a busy loop");
                let watch = self
                    .enforcer
                    .watches()
                    .into_iter()
                    .find(|watch| watch.hive == hive && watch.key.eq_ignore_ascii_case(&key));
                if let Some(watch) = watch {
                    self.enforcer.trigger(&TriggerEvent::Change(watch));
                }
            }
            delivered
        }
    }

    #[test]
    fn enabling_applies_and_records_the_original() {
        let mut fixture = Fixture::new();
        assert_eq!(fixture.state(), ActionState::Different);

        assert!(fixture.enforcer.reconcile());
        assert_eq!(fixture.value(), Some(dword(1)));
        let status = &fixture.enforcer.status()[0];
        assert!(status.enabled);
        assert_eq!(status.state, ActionState::Compliant);
        assert_eq!(status.actions[0].current, "dword:1");

        let history = fixture.enforcer.history(10);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].kind, HistoryKind::Applied);
        assert_eq!(
            history[0].detail,
            r"HKLM\SOFTWARE\SysPilot-Test\Enforcer\Value = dword:1 (was dword:7)"
        );
        assert!(!fixture.enforcer.reconcile(), "a second reconcile has nothing to do");
    }

    #[test]
    fn disabling_restores_the_original_exactly() {
        let mut fixture = Fixture::new();
        fixture.enforcer.reconcile();
        fixture.configure(&[]);
        assert!(fixture.enforcer.reconcile());
        assert_eq!(fixture.value(), Some(dword(7)));
        assert_eq!(fixture.kinds(), [HistoryKind::Applied, HistoryKind::Reverted]);
        let status = &fixture.enforcer.status()[0];
        assert!(!status.enabled);
        assert_eq!(status.state, ActionState::Different);
    }

    #[test]
    fn enabling_an_already_compliant_entry_writes_nothing() {
        let mut fixture = Fixture::new();
        fixture.fake.registry.set_external(&Hive::Machine, KEY, "Value", Some(dword(1)));
        fixture.enforcer.reconcile();
        assert_eq!(fixture.fake.registry.engine_writes(), 0);
        assert!(fixture.enforcer.history(10).is_empty());

        // It is still tracked: a later drift is restored.
        fixture.fake.registry.set_external(&Hive::Machine, KEY, "Value", Some(dword(5)));
        fixture.pump_changes();
        assert_eq!(fixture.value(), Some(dword(1)));
    }

    #[test]
    fn drift_is_restored_and_counted() {
        let mut fixture = Fixture::new();
        fixture.enforcer.reconcile();
        fixture.fake.registry.clear_notifications();
        fixture.fake.clock.advance(Duration::from_secs(10));

        fixture.fake.registry.set_external(&Hive::Machine, KEY, "Value", Some(dword(3)));
        fixture.pump_changes();

        assert_eq!(fixture.value(), Some(dword(1)));
        let status = &fixture.enforcer.status()[0];
        assert_eq!(status.state, ActionState::Compliant);
        assert_eq!(status.drift_count, 1);
        assert_eq!(status.last_drift_unix, Some(FakeClockEpoch::at(10)));
        assert_eq!(fixture.kinds(), [HistoryKind::Applied, HistoryKind::Restored]);
        assert!(fixture.enforcer.history(1)[0].detail.ends_with("(was dword:3)"));
    }

    /// Wall-clock helper for assertions against the fake clock.
    struct FakeClockEpoch;

    impl FakeClockEpoch {
        fn at(secs: u64) -> u64 {
            crate::fake::FakeClock::EPOCH_UNIX_SECS + secs
        }
    }

    #[test]
    fn a_writer_that_keeps_rewriting_ends_contested_without_a_busy_loop() {
        let mut fixture = Fixture::new();
        fixture.enforcer.reconcile();
        fixture.fake.registry.clear_notifications();
        let writes_before = fixture.fake.registry.engine_writes();

        // From now on another program forces 9 back after every write.
        fixture.fake.registry.contest(&Hive::Machine, KEY, "Value", Some(dword(9)));
        fixture.fake.registry.set_external(&Hive::Machine, KEY, "Value", Some(dword(9)));
        fixture.pump_changes();

        let restores = fixture.fake.registry.engine_writes() - writes_before;
        assert_eq!(restores, 3, "exactly the contest threshold, then it stops");
        assert_eq!(fixture.state(), ActionState::Contested { retry_in_secs: 60 });
        assert_eq!(fixture.enforcer.next_deadline(), Some(Duration::from_secs(60)));
        assert_eq!(fixture.kinds().last(), Some(&HistoryKind::Contested));
    }

    #[test]
    fn contested_unit_is_retried_once_per_doubling_back_off() {
        let mut fixture = Fixture::new();
        fixture.enforcer.reconcile();
        fixture.fake.registry.contest(&Hive::Machine, KEY, "Value", Some(dword(9)));
        fixture.fake.registry.set_external(&Hive::Machine, KEY, "Value", Some(dword(9)));
        fixture.pump_changes();
        let writes_when_contested = fixture.fake.registry.engine_writes();

        // Before the deadline nothing happens, whatever arrives.
        fixture.fake.clock.advance(Duration::from_secs(59));
        fixture.enforcer.trigger(&TriggerEvent::Timer);
        fixture.fake.registry.set_external(&Hive::Machine, KEY, "Value", Some(dword(9)));
        fixture.pump_changes();
        assert_eq!(fixture.fake.registry.engine_writes(), writes_when_contested);

        // At the deadline: one attempt; it is undone again; back off 120 s.
        fixture.fake.clock.advance(Duration::from_secs(1));
        fixture.enforcer.trigger(&TriggerEvent::Timer);
        fixture.pump_changes();
        assert_eq!(fixture.fake.registry.engine_writes(), writes_when_contested + 1);
        assert_eq!(fixture.state(), ActionState::Contested { retry_in_secs: 120 });

        fixture.fake.clock.advance(Duration::from_secs(120));
        fixture.enforcer.trigger(&TriggerEvent::Timer);
        fixture.pump_changes();
        assert_eq!(fixture.fake.registry.engine_writes(), writes_when_contested + 2);
        assert_eq!(fixture.state(), ActionState::Contested { retry_in_secs: 240 });
    }

    #[test]
    fn back_off_is_capped() {
        let mut fixture = Fixture::new();
        fixture.enforcer.reconcile();
        fixture.fake.registry.contest(&Hive::Machine, KEY, "Value", Some(dword(9)));
        fixture.fake.registry.set_external(&Hive::Machine, KEY, "Value", Some(dword(9)));
        fixture.pump_changes();
        for _ in 0..12 {
            fixture.fake.clock.advance(Duration::from_secs(3600));
            fixture.enforcer.trigger(&TriggerEvent::Timer);
            fixture.pump_changes();
        }
        assert_eq!(fixture.state(), ActionState::Contested { retry_in_secs: 3600 });
    }

    #[test]
    fn contest_ends_when_the_other_writer_stops() {
        let mut fixture = Fixture::new();
        fixture.enforcer.reconcile();
        fixture.fake.registry.contest(&Hive::Machine, KEY, "Value", Some(dword(9)));
        fixture.fake.registry.set_external(&Hive::Machine, KEY, "Value", Some(dword(9)));
        fixture.pump_changes();

        fixture.fake.registry.stop_contesting(&Hive::Machine, KEY, "Value");
        fixture.fake.clock.advance(Duration::from_secs(60));
        fixture.enforcer.trigger(&TriggerEvent::Timer);
        fixture.pump_changes();
        assert_eq!(fixture.value(), Some(dword(1)));
        assert_eq!(fixture.state(), ActionState::Compliant);
        assert_eq!(fixture.enforcer.next_deadline(), None, "no timer is left armed");

        // After a quiet window the unit is trusted again: a new drift gets
        // the normal treatment, not an immediate contest.
        fixture.fake.clock.advance(Duration::from_secs(61));
        fixture.fake.registry.set_external(&Hive::Machine, KEY, "Value", Some(dword(4)));
        fixture.pump_changes();
        assert_eq!(fixture.state(), ActionState::Compliant);
    }

    #[test]
    fn reapply_retries_a_contested_entry_immediately() {
        let mut fixture = Fixture::new();
        fixture.enforcer.reconcile();
        fixture.fake.registry.contest(&Hive::Machine, KEY, "Value", Some(dword(9)));
        fixture.fake.registry.set_external(&Hive::Machine, KEY, "Value", Some(dword(9)));
        fixture.pump_changes();
        fixture.fake.registry.stop_contesting(&Hive::Machine, KEY, "Value");

        fixture.enforcer.trigger(&TriggerEvent::Reapply { id: "test.value".into() });
        assert_eq!(fixture.value(), Some(dword(1)));
        assert_eq!(fixture.state(), ActionState::Compliant);
    }

    #[test]
    fn slow_drift_is_never_contested() {
        let mut fixture = Fixture::new();
        fixture.enforcer.reconcile();
        for round in 0..10 {
            fixture.fake.clock.advance(Duration::from_secs(30));
            fixture.fake.registry.set_external(&Hive::Machine, KEY, "Value", Some(dword(5)));
            fixture.pump_changes();
            assert_eq!(fixture.state(), ActionState::Compliant, "round {round}");
        }
        assert_eq!(fixture.enforcer.status()[0].drift_count, 10);
    }

    #[test]
    fn protected_setting_is_reported_once_and_left_alone() {
        let mut fixture = Fixture::new();
        fixture.fake.registry.protect(&Hive::Machine, KEY);
        fixture.enforcer.reconcile();
        assert_eq!(fixture.value(), Some(dword(7)), "the protection is not worked around");
        assert!(
            matches!(fixture.state(), ActionState::NotEnforceable { ref reason } if reason.contains("access denied")),
            "{:?}",
            fixture.state()
        );
        fixture.enforcer.trigger(&TriggerEvent::Boot);
        fixture.enforcer.trigger(&TriggerEvent::Boot);
        assert_eq!(fixture.kinds(), [HistoryKind::Refused], "the refusal is logged once");
    }

    #[test]
    fn state_survives_a_restart_and_still_reverts_exactly() {
        let mut fixture = Fixture::new();
        fixture.enforcer.reconcile();
        fixture.fake.registry.set_external(&Hive::Machine, KEY, "Value", Some(dword(3)));
        fixture.pump_changes();

        let mut restarted = Enforcer::new(
            fixture.fake.os(),
            Box::new(fixture.store.clone()),
            EnforcerConfig::default(),
        );
        restarted.configure(Fixture::entries(), ["test.value".to_owned()]);
        assert!(!restarted.trigger(&TriggerEvent::Boot), "nothing drifted while it was down");
        assert_eq!(restarted.status()[0].drift_count, 1, "counters are persisted");
        assert_eq!(restarted.history(10).len(), 2, "history is persisted");

        restarted.configure(Fixture::entries(), []);
        restarted.reconcile();
        assert_eq!(fixture.value(), Some(dword(7)));
    }

    #[test]
    fn drift_while_the_service_was_down_is_restored_at_boot() {
        let mut fixture = Fixture::new();
        fixture.enforcer.reconcile();
        fixture.fake.registry.set_external(&Hive::Machine, KEY, "Value", Some(dword(2)));

        let mut restarted = Enforcer::new(
            fixture.fake.os(),
            Box::new(fixture.store.clone()),
            EnforcerConfig::default(),
        );
        restarted.configure(Fixture::entries(), ["test.value".to_owned()]);
        assert!(restarted.trigger(&TriggerEvent::Boot));
        assert_eq!(fixture.value(), Some(dword(1)));
    }

    #[test]
    fn unreadable_saved_state_is_reported_not_ignored() {
        let fake = FakeOs::default();
        let store = MemoryStateStore::default();
        store.save(APPLIED_STATE_NAME, "this is [not toml").unwrap();
        let enforcer = Enforcer::new(fake.os(), Box::new(store), EnforcerConfig::default());
        let history = enforcer.history(10);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].kind, HistoryKind::Failed);
        assert!(history[0].detail.contains("could not be read"), "{}", history[0].detail);
    }

    #[test]
    fn a_changed_definition_reverts_the_old_action_and_applies_the_new_one() {
        let mut fixture = Fixture::new();
        fixture.enforcer.reconcile();

        // A catalog update now wants another value name.
        let updated = vec![registry_entry("test.value", Scope::Machine, "Renamed", 1)];
        fixture.enforcer.configure(updated, ["test.value".to_owned()]);
        fixture.enforcer.reconcile();

        assert_eq!(fixture.value(), Some(dword(7)), "the old value is back to its original");
        assert_eq!(fixture.fake.registry.value(&Hive::Machine, KEY, "Renamed"), Some(dword(1)));
    }

    #[test]
    fn audit_and_inapplicable_entries_are_never_applied() {
        let mut fixture = Fixture::new();
        let mut audit = registry_entry("test.audit", Scope::Machine, "Value", 1);
        audit.audit = true;
        let mut old_build = registry_entry("test.new", Scope::Machine, "Value", 1);
        old_build.not_applicable = Some("needs Windows build 26100 or later".into());
        fixture
            .enforcer
            .configure(vec![audit, old_build], ["test.audit".to_owned(), "test.new".to_owned()]);

        assert!(!fixture.enforcer.reconcile());
        assert_eq!(fixture.value(), Some(dword(7)));
        let status = fixture.enforcer.status();
        assert_eq!(status[0].state, ActionState::Different, "audit entries still report");
        assert_eq!(
            status[1].state,
            ActionState::NotApplicable { reason: "needs Windows build 26100 or later".into() }
        );
    }

    #[test]
    fn per_user_entry_follows_logons() {
        let fake = FakeOs::default();
        let store = MemoryStateStore::default();
        let mut enforcer = Enforcer::new(fake.os(), Box::new(store), EnforcerConfig::default());
        let entries = || vec![registry_entry("test.user", Scope::User, "Value", 0)];
        enforcer.configure(entries(), ["test.user".to_owned()]);

        enforcer.trigger(&TriggerEvent::Boot);
        assert_eq!(enforcer.status()[0].state, ActionState::Unknown, "nobody is logged on");

        let alice = Hive::User(ALICE.into());
        let bob = Hive::User(BOB.into());
        fake.registry.set_external(&alice, KEY, "Value", Some(dword(1)));
        fake.sessions.set(&[ALICE]);
        enforcer.trigger(&TriggerEvent::Logon { sid: ALICE.into() });
        assert_eq!(fake.registry.value(&alice, KEY, "Value"), Some(dword(0)));
        assert_eq!(enforcer.status()[0].state, ActionState::Compliant);

        fake.sessions.set(&[ALICE, BOB]);
        enforcer.trigger(&TriggerEvent::Logon { sid: BOB.into() });
        assert_eq!(fake.registry.value(&bob, KEY, "Value"), Some(dword(0)));

        // Bob logs off; the entry is disabled. Alice is reverted now, Bob
        // when he next logs on.
        fake.sessions.set(&[ALICE]);
        enforcer.configure(entries(), []);
        enforcer.reconcile();
        assert_eq!(fake.registry.value(&alice, KEY, "Value"), Some(dword(1)));
        assert_eq!(fake.registry.value(&bob, KEY, "Value"), Some(dword(0)), "hive not loaded");

        fake.sessions.set(&[ALICE, BOB]);
        enforcer.trigger(&TriggerEvent::Logon { sid: BOB.into() });
        assert_eq!(fake.registry.value(&bob, KEY, "Value"), None, "it did not exist before");
        assert!(!fake.registry.has_key(&bob, KEY), "keys created for Bob are removed");
    }

    #[test]
    fn watch_moves_from_an_ancestor_to_the_key_once_it_exists() {
        let fake = FakeOs::default();
        fake.registry.create_key(&Hive::Machine, "SOFTWARE");
        let mut enforcer = Enforcer::new(
            fake.os(),
            Box::new(MemoryStateStore::default()),
            EnforcerConfig::default(),
        );
        // Desired state: the value is absent, and its key does not exist.
        let action = Action::Registry(RegistryAction {
            scope: Scope::Machine,
            key: KEY.into(),
            name: "Value".into(),
            data: RegData::Absent,
        });
        let entry = Entry {
            id: "test.absent".into(),
            triggers: vec![action.default_triggers()],
            actions: vec![action],
            audit: false,
            not_applicable: None,
        };
        enforcer.configure(vec![entry], ["test.absent".to_owned()]);
        enforcer.reconcile();

        let ancestor = WatchSpec { hive: Hive::Machine, key: "SOFTWARE".into(), subtree: false };
        assert_eq!(enforcer.watches(), std::slice::from_ref(&ancestor));

        // Another program creates the key and the value.
        fake.registry.set_external(&Hive::Machine, KEY, "Value", Some(dword(1)));
        enforcer.trigger(&TriggerEvent::Change(ancestor));
        assert_eq!(fake.registry.value(&Hive::Machine, KEY, "Value"), None);
        let exact = WatchSpec { hive: Hive::Machine, key: KEY.into(), subtree: false };
        assert_eq!(enforcer.watches(), [exact]);
    }

    #[test]
    fn units_without_a_change_trigger_are_not_watched() {
        let mut fixture = Fixture::new();
        let mut entry = registry_entry("test.value", Scope::Machine, "Value", 1);
        entry.triggers = vec![vec![Trigger::Boot]];
        fixture.enforcer.configure(vec![entry], ["test.value".to_owned()]);
        fixture.enforcer.reconcile();
        assert!(fixture.enforcer.watches().is_empty());
    }

    fn command_entry(triggers: Vec<Trigger>) -> Entry {
        let action = Action::Command(CommandAction {
            apply: CommandLine { program: r"C:\apply.exe".into(), args: vec![] },
            run_as: RunAs::System,
            timeout_secs: 30,
            check: None,
            undo: None,
        });
        Entry {
            id: "custom.command".into(),
            actions: vec![action],
            triggers: vec![triggers],
            audit: false,
            not_applicable: None,
        }
    }

    #[test]
    fn interval_trigger_runs_on_the_timer_and_reschedules() {
        let fake = FakeOs::default();
        let mut enforcer = Enforcer::new(
            fake.os(),
            Box::new(MemoryStateStore::default()),
            EnforcerConfig::default(),
        );
        enforcer.configure(
            vec![command_entry(vec![Trigger::Interval(600)])],
            ["custom.command".to_owned()],
        );
        enforcer.reconcile();
        assert_eq!(fake.launcher.launched().len(), 1, "applied when enabled");
        assert_eq!(enforcer.next_deadline(), Some(Duration::from_secs(600)));

        fake.clock.advance(Duration::from_secs(599));
        enforcer.trigger(&TriggerEvent::Timer);
        assert_eq!(fake.launcher.launched().len(), 1, "not due yet");

        fake.clock.advance(Duration::from_secs(1));
        enforcer.trigger(&TriggerEvent::Timer);
        assert_eq!(fake.launcher.launched().len(), 2);
        assert_eq!(enforcer.next_deadline(), Some(Duration::from_secs(1200)));
        assert_eq!(enforcer.status()[0].state, ActionState::Compliant);
    }

    #[test]
    fn failing_command_is_reported_as_failed() {
        let fake = FakeOs::default();
        fake.launcher.script(r"C:\apply.exe", Ok(1));
        let mut enforcer = Enforcer::new(
            fake.os(),
            Box::new(MemoryStateStore::default()),
            EnforcerConfig::default(),
        );
        enforcer.configure(vec![command_entry(vec![Trigger::Boot])], ["custom.command".to_owned()]);
        enforcer.reconcile();
        let state = enforcer.status()[0].state.clone();
        assert!(
            matches!(state, ActionState::Failed { ref error } if error.contains("received: 1")),
            "{state:?}"
        );
        assert_eq!(enforcer.history(1)[0].kind, HistoryKind::Failed);
    }

    #[test]
    fn resume_trigger_reaches_only_units_that_ask_for_it() {
        let fake = FakeOs::default();
        let mut enforcer = Enforcer::new(
            fake.os(),
            Box::new(MemoryStateStore::default()),
            EnforcerConfig::default(),
        );
        enforcer.configure(
            vec![command_entry(vec![Trigger::Boot, Trigger::Resume])],
            ["custom.command".to_owned()],
        );
        enforcer.reconcile();
        enforcer.trigger(&TriggerEvent::Resume);
        assert_eq!(fake.launcher.launched().len(), 2);
        enforcer.trigger(&TriggerEvent::Logon { sid: ALICE.into() });
        assert_eq!(fake.launcher.launched().len(), 2, "no logon trigger declared");
    }

    #[test]
    fn history_is_capped_and_clearable() {
        let mut fixture = Fixture::new();
        fixture.enforcer.config.history_cap = 3;
        fixture.enforcer.reconcile();
        for _ in 0..5 {
            fixture.fake.clock.advance(Duration::from_secs(120));
            fixture.fake.registry.set_external(&Hive::Machine, KEY, "Value", Some(dword(5)));
            fixture.pump_changes();
        }
        assert_eq!(fixture.enforcer.history(100).len(), 3);
        fixture.enforcer.clear_history();
        assert!(fixture.enforcer.history(100).is_empty());
    }

    #[test]
    fn failed_save_is_surfaced() {
        let mut fixture = Fixture::new();
        fixture.store.fail_saves(true);
        fixture.enforcer.reconcile();
        assert_eq!(fixture.enforcer.store_error(), Some("the fake store refuses to save"));
        fixture.store.fail_saves(false);
        fixture.enforcer.clear_history();
        assert_eq!(fixture.enforcer.store_error(), None);
    }

    #[test]
    fn combined_state_is_the_most_severe_part() {
        let failed = ActionState::Failed { error: "x".into() };
        let not_applicable = ActionState::NotApplicable { reason: "x".into() };
        assert_eq!(combine([].iter()), ActionState::Unknown);
        assert_eq!(
            combine([ActionState::Compliant, not_applicable.clone()].iter()),
            ActionState::Compliant,
            "a part with nothing to do does not hide a compliant one"
        );
        assert_eq!(
            combine([ActionState::Compliant, ActionState::Different].iter()),
            ActionState::Different
        );
        assert_eq!(
            combine([ActionState::Different, failed.clone(), ActionState::Compliant].iter()),
            failed
        );
        assert_eq!(combine([not_applicable.clone()].iter()), not_applicable);
    }

    #[test]
    fn engine_never_writes_while_contested_even_under_notification_storms() {
        let mut fixture = Fixture::new();
        fixture.enforcer.reconcile();
        fixture.fake.registry.contest(&Hive::Machine, KEY, "Value", Some(dword(9)));
        fixture.fake.registry.set_external(&Hive::Machine, KEY, "Value", Some(dword(9)));
        fixture.pump_changes();
        let writes = fixture.fake.registry.engine_writes();
        for _ in 0..200 {
            fixture.fake.registry.set_external(&Hive::Machine, KEY, "Value", Some(dword(9)));
        }
        assert_eq!(fixture.pump_changes(), 200);
        assert_eq!(fixture.fake.registry.engine_writes(), writes);
        // Reading is still allowed: the write count is what must not grow.
        let _ = fixture.fake.registry.read(&Hive::Machine, KEY, "Value");
    }
}
