//! What the views show, derived from what the service reports.
//!
//! Everything here is plain data and pure functions, so the wording and the
//! state mapping are tested without a window.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fmt::Write as _;

use syspilot_proto::catalog::{Catalog, Category, Direction, Level, Reboot, Tweak};
use syspilot_proto::message::{
    ActionState, DriverState, EntryStatus, FanAvailability, FanChannelStatus, FanPhase, FanStatus,
    HistoryEvent, HistoryKind, ServiceStatus, SystemInfo, UpdateState, UpdateStatus,
};
use syspilot_proto::profile::{CurvePoint, Profile, UpdateMode};
use syspilot_proto::sensor::{DeviceClass, Sample, SensorInfo, SensorKind, SensorList};
use syspilot_proto::tuning::{
    TuningControl, TuningDevice, TuningKind, TuningSetting, TuningState, TuningUnit,
};

/// Samples kept per sensor for the small trend lines: one minute.
pub const TREND_LEN: usize = 60;

/// Seconds averaged into one point of a trend line. Every changed point
/// redraws a line segment, so a chart that only changes when a whole
/// bucket is complete costs about a quarter of one that moves each second.
pub const CHART_BUCKET: usize = 2;

/// Points on a trend line.
pub const CHART_POINTS: usize = TREND_LEN / CHART_BUCKET;

/// Everything read from the service in one refresh.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Snapshot {
    /// Whether the service treats this app as an elevated administrator.
    pub elevated: bool,
    /// Service, driver and feature status.
    pub status: Option<ServiceStatus>,
    /// The built-in tweaks.
    pub catalog: Catalog,
    /// State of every tweak and custom action.
    pub states: Vec<EntryStatus>,
    /// The stored profile. Only readable when elevated.
    pub profile: Option<Profile>,
    /// Recent enforcement events. Only readable when elevated.
    pub history: Vec<HistoryEvent>,
    /// Sensor metadata.
    pub sensors: SensorList,
    /// Hardware and software inventory.
    pub system: SystemInfo,
}

/// Whether the app has data to show.
#[derive(Debug, Clone, PartialEq)]
pub enum Link {
    /// First load in progress.
    Connecting,
    /// Connected; the snapshot is current.
    Ready,
    /// The service is not installed or not started.
    NotRunning,
    /// Something else went wrong; the text says what.
    Failed(String),
}

/// How much of the product is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ViewLevel {
    /// Common options in plain language.
    Simple,
    /// Everything, with technical detail.
    Advanced,
}

/// Colour meaning of a badge or figure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// No judgement.
    Neutral,
    /// As intended.
    Good,
    /// Needs a look.
    Caution,
    /// Wrong or refused.
    Bad,
}

/// A short state label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Badge {
    /// Text, from the glossary in `docs/ui-glossary.md`.
    pub text: &'static str,
    /// Colour meaning.
    pub tone: Tone,
}

const fn badge(text: &'static str, tone: Tone) -> Option<Badge> {
    Some(Badge { text, tone })
}

/// The badge for an entry. `None` means nothing worth saying (off, and the
/// system is in its usual state).
pub fn entry_badge(status: &EntryStatus, audit: bool) -> Option<Badge> {
    match (&status.state, status.enabled, audit) {
        (ActionState::NotApplicable { .. }, _, _) => badge("Not available", Tone::Neutral),
        (ActionState::Compliant, _, true) => badge("Passed", Tone::Good),
        (ActionState::Different, _, true) => badge("Needs attention", Tone::Caution),
        (_, _, true) => badge("Can't check", Tone::Neutral),
        (ActionState::Compliant, true, _) => badge("Applied", Tone::Good),
        (ActionState::Compliant, false, _) => badge("Already set", Tone::Neutral),
        (_, false, _) => None,
        (ActionState::Different, true, _) => badge("Not applied yet", Tone::Caution),
        (ActionState::Contested { .. }, true, _) => badge("Contested", Tone::Caution),
        (ActionState::NotEnforceable { .. }, true, _) => badge("Blocked by Windows", Tone::Bad),
        (ActionState::Unknown, true, _) => badge("Can't check", Tone::Neutral),
        (ActionState::Failed { .. }, true, _) => badge("Failed", Tone::Bad),
    }
}

/// A sentence explaining a state that needs explaining.
pub fn entry_note(status: &EntryStatus) -> Option<String> {
    match &status.state {
        ActionState::Contested { retry_in_secs } => Some(format!(
            "Something else keeps changing this setting. SysPilot stopped restoring it and will try again in {}.",
            duration_text(u64::from(*retry_in_secs))
        )),
        ActionState::NotEnforceable { reason } => {
            Some(format!("Windows does not allow this change. {reason}"))
        }
        ActionState::NotApplicable { reason } => Some(sentence(reason)),
        ActionState::Failed { error } => Some(format!("This could not be applied. {error}")),
        ActionState::Unknown if status.enabled => Some(
            "It is turned on, but its current state can't be read right now (for per-user settings, this is normal until that user signs in)."
                .into(),
        ),
        _ => None,
    }
}

/// Capitalises the first letter and ends with a full stop.
fn sentence(text: &str) -> String {
    let mut characters = text.trim().chars();
    let Some(first) = characters.next() else { return String::new() };
    let mut out: String = first.to_uppercase().chain(characters).collect();
    if !out.ends_with(['.', '!', '?']) {
        out.push('.');
    }
    out
}

/// `2 d 4 h`, `2 h 5 min`, `41 s`: the two most significant units.
pub fn duration_text(seconds: u64) -> String {
    let (days, hours, minutes) = (seconds / 86_400, seconds / 3600 % 24, seconds / 60 % 60);
    match (days, hours, minutes) {
        (0, 0, 0) => format!("{seconds} s"),
        (0, 0, _) => format!("{minutes} min"),
        (0, _, 0) => format!("{hours} h"),
        (0, _, _) => format!("{hours} h {minutes} min"),
        (_, 0, _) => format!("{days} d"),
        _ => format!("{days} d {hours} h"),
    }
}

/// Name of an update mode in its selector.
pub fn update_mode_name(mode: UpdateMode) -> &'static str {
    match mode {
        UpdateMode::Off => "Don't look for updates",
        UpdateMode::Notify => "Tell me about updates",
        UpdateMode::Automatic => "Install updates automatically",
    }
}

/// What an update mode does, in a sentence.
pub fn update_mode_detail(mode: UpdateMode) -> &'static str {
    match mode {
        UpdateMode::Off => "SysPilot never looks for a new version and never mentions one.",
        UpdateMode::Notify => {
            "SysPilot looks once a day and tells you when there is a new version. You decide when to install it."
        }
        UpdateMode::Automatic => {
            "SysPilot looks once a day and installs a new version by itself while this window is closed."
        }
    }
}

/// What the app says while a new version is being put in place.
const UPDATE_INSTALLING: &str = "Installing the new version of SysPilot. This window closes when it is ready; open SysPilot again afterwards.";

/// Where updates stand, for the Settings page. `now_unix` is the current
/// time, for "last looked".
pub fn update_text(update: &UpdateStatus, now_unix: u64) -> (String, Tone) {
    match &update.state {
        UpdateState::Unavailable => (
            "This copy of SysPilot does not update itself. An installed release does.".to_owned(),
            Tone::Neutral,
        ),
        UpdateState::Idle => match update.checked_unix {
            Some(checked) => {
                let ago = match now_unix.saturating_sub(checked) {
                    0..60 => "just now".to_owned(),
                    seconds => format!("{} ago", duration_text(seconds)),
                };
                (format!("SysPilot is up to date. Last looked {ago}."), Tone::Good)
            }
            None if update.mode == UpdateMode::Off => {
                ("SysPilot is not looking for updates.".to_owned(), Tone::Neutral)
            }
            None => (
                "SysPilot has not looked for updates yet. It does a few minutes after the PC starts."
                    .to_owned(),
                Tone::Neutral,
            ),
        },
        UpdateState::Checking => ("Looking for a new version.".to_owned(), Tone::Neutral),
        UpdateState::Available { version } => {
            (format!("SysPilot {version} is available."), Tone::Caution)
        }
        UpdateState::Installing => (UPDATE_INSTALLING.to_owned(), Tone::Neutral),
        UpdateState::Failed { reason } => {
            (format!("The last update did not work. {reason}"), Tone::Bad)
        }
    }
}

/// The message shown on every page while a new version waits or is being
/// installed, and whether it comes with the offer to install now. `None`
/// when there is nothing to say, which is always the case with updates off:
/// the service then forgets what it found.
pub fn update_banner(update: &UpdateStatus) -> Option<(String, bool)> {
    match &update.state {
        UpdateState::Available { version } if update.mode == UpdateMode::Automatic => Some((
            format!(
                "SysPilot {version} is ready and will be installed when you close this window."
            ),
            true,
        )),
        UpdateState::Available { version } => {
            Some((format!("SysPilot {version} is available."), true))
        }
        UpdateState::Installing => Some((UPDATE_INSTALLING.to_owned(), false)),
        _ => None,
    }
}

/// Display name of a category.
pub fn category_name(category: Category) -> &'static str {
    match category {
        Category::Performance => "Performance",
        Category::Privacy => "Privacy",
        Category::Security => "Security",
        Category::Reliability => "Reliability",
        Category::Other => "Look and feel",
    }
}

/// One line saying what a category is for.
pub fn category_blurb(category: Category) -> &'static str {
    match category {
        Category::Performance => "Responsiveness, latency and background activity.",
        Category::Privacy => "What Windows sends to Microsoft and shares with apps.",
        Category::Security => "Attack surface, protections and checks of this PC's posture.",
        Category::Reliability => "Updates, recovery and long-term stability.",
        Category::Other => "How Windows looks and what it suggests.",
    }
}

/// Whether `tweak` is shown at `level`.
pub fn visible_at(tweak: &Tweak, level: ViewLevel) -> bool {
    level == ViewLevel::Advanced || tweak.level == Level::Simple
}

/// A tweak with its state, ready to draw.
#[derive(Debug, Clone, PartialEq)]
pub struct TweakRow {
    /// Entry ID.
    pub id: String,
    /// Headline.
    pub title: String,
    /// What the user will notice.
    pub summary: String,
    /// Whether it is turned on.
    pub enabled: bool,
    /// Whether the switch can be used at all.
    pub available: bool,
    /// Checks have no switch.
    pub audit: bool,
    /// Turning it on lowers security and needs a confirmation.
    pub weakens: bool,
    /// State label.
    pub badge: Option<Badge>,
    /// Explanation of an unusual state.
    pub note: Option<String>,
    /// How often it was restored after something changed it.
    pub restored: u32,
    /// Extra facts for the Advanced level, as label and value.
    pub facts: Vec<(&'static str, String)>,
    /// The settings it writes with their current values.
    pub settings: Vec<(String, String)>,
    /// Microsoft's documentation for it.
    pub source: String,
}

fn tweak_row(tweak: &Tweak, status: Option<&EntryStatus>) -> TweakRow {
    let mut facts = Vec::new();
    if !tweak.detail.is_empty() {
        facts.push(("How it works", tweak.detail.clone()));
    }
    if let Some(risk) = &tweak.risk {
        facts.push(("Risk", risk.clone()));
    }
    if !tweak.side_effects.is_empty() {
        facts.push(("Side effects", tweak.side_effects.clone()));
    }
    match tweak.reboot {
        Reboot::None => {}
        Reboot::SignOut => facts.push(("Takes effect", "After you sign out and back in".into())),
        Reboot::Restart => facts.push(("Takes effect", "After a restart".into())),
    }
    let settings = tweak
        .actions
        .iter()
        .enumerate()
        .map(|(index, action)| {
            let current = status.and_then(|status| status.actions.get(index));
            (action.describe(), current.map(|c| c.current.clone()).unwrap_or_default())
        })
        .collect();
    let not_applicable =
        status.is_some_and(|s| matches!(s.state, ActionState::NotApplicable { .. }));
    TweakRow {
        id: tweak.id.clone(),
        title: tweak.title.clone(),
        summary: tweak.summary.clone(),
        enabled: status.is_some_and(|status| status.enabled),
        available: !not_applicable && !tweak.audit,
        audit: tweak.audit,
        weakens: tweak.direction == Some(Direction::Weakens),
        badge: status.and_then(|status| entry_badge(status, tweak.audit)),
        note: status.and_then(entry_note),
        restored: status.map_or(0, |status| status.drift_count),
        facts,
        settings,
        source: tweak.source.clone(),
    }
}

/// The tweaks of one category that are visible at `level`, matching
/// `filter` (case-insensitive, in title, summary or ID).
pub fn tweak_rows(
    snapshot: &Snapshot,
    category: Category,
    level: ViewLevel,
    filter: &str,
) -> Vec<TweakRow> {
    let needle = filter.trim().to_lowercase();
    snapshot
        .catalog
        .tweaks
        .iter()
        .filter(|tweak| tweak.category == category && visible_at(tweak, level))
        .filter(|tweak| {
            needle.is_empty()
                || [&tweak.title, &tweak.summary, &tweak.id]
                    .iter()
                    .any(|text| text.to_lowercase().contains(&needle))
        })
        .map(|tweak| tweak_row(tweak, snapshot.states.iter().find(|s| s.id == tweak.id)))
        .collect()
}

/// Categories that have at least one tweak visible at `level`.
pub fn categories(snapshot: &Snapshot, level: ViewLevel) -> Vec<Category> {
    Category::ALL
        .into_iter()
        .filter(|category| {
            snapshot.catalog.tweaks.iter().any(|t| t.category == *category && visible_at(t, level))
        })
        .collect()
}

/// The headline numbers of the home page.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Overview {
    /// Tweaks and custom actions turned on.
    pub enabled: usize,
    /// Of those, how many are in place.
    pub applied: usize,
    /// Of those, how many are contested, blocked or failed.
    pub problems: usize,
    /// Security checks that passed.
    pub checks_passed: usize,
    /// Security checks in total.
    pub checks_total: usize,
    /// Times a setting was put back after something changed it.
    pub restored: u32,
}

/// Summarises the entry states.
pub fn overview(snapshot: &Snapshot) -> Overview {
    let mut overview = Overview::default();
    for status in &snapshot.states {
        let audit = snapshot.catalog.get(&status.id).is_some_and(|tweak| tweak.audit);
        if audit {
            overview.checks_total += 1;
            overview.checks_passed += usize::from(status.state == ActionState::Compliant);
            continue;
        }
        overview.restored += status.drift_count;
        if !status.enabled {
            continue;
        }
        overview.enabled += 1;
        match status.state {
            ActionState::Compliant => overview.applied += 1,
            ActionState::Contested { .. }
            | ActionState::NotEnforceable { .. }
            | ActionState::Failed { .. } => overview.problems += 1,
            _ => {}
        }
    }
    overview
}

/// Entries that need the user's attention, with the reason.
pub fn attention(snapshot: &Snapshot) -> Vec<(String, Badge, String)> {
    snapshot
        .states
        .iter()
        .filter_map(|status| {
            let tweak = snapshot.catalog.get(&status.id);
            let audit = tweak.is_some_and(|tweak| tweak.audit);
            let badge = entry_badge(status, audit)?;
            if !matches!(badge.tone, Tone::Caution | Tone::Bad) {
                return None;
            }
            let title = tweak.map_or_else(|| status.id.clone(), |tweak| tweak.title.clone());
            let note = entry_note(status)
                .or_else(|| tweak.filter(|_| audit).map(|tweak| tweak.summary.clone()))
                .unwrap_or_default();
            Some((title, badge, note))
        })
        .collect()
}

/// One sentence about the driver, and whether it is something to act on.
pub fn driver_text(driver: &DriverState) -> (String, Tone) {
    match driver {
        DriverState::Missing => (
            "PawnIO is not installed. Sensors that need hardware access are unavailable.".into(),
            Tone::Caution,
        ),
        DriverState::Ready { version } => (format!("PawnIO {version} is ready."), Tone::Good),
        DriverState::TooOld { version, required } => (
            format!("PawnIO {version} is too old. Version {required} or later is needed."),
            Tone::Caution,
        ),
        DriverState::Failed { error } => {
            (format!("PawnIO is installed but can't be used. {}", sentence(error)), Tone::Bad)
        }
    }
}

/// Formats a reading with a sensible precision and its unit.
pub fn reading_text(value: Option<f64>, kind: SensorKind) -> String {
    let Some(value) = value else { return "—".into() };
    match kind {
        SensorKind::Data => bytes_text(value),
        SensorKind::Throughput => format!("{}/s", bytes_text(value)),
        SensorKind::Frequency if value >= 1000.0 => format!("{:.2} GHz", value / 1000.0),
        SensorKind::Voltage => format!("{value:.3} V"),
        SensorKind::Temperature | SensorKind::Load | SensorKind::Duty | SensorKind::Level => {
            format!("{value:.0} {}", kind.unit())
        }
        _ if value.abs() >= 100.0 => format!("{value:.0} {}", kind.unit()),
        _ => format!("{value:.1} {}", kind.unit()),
    }
}

/// `1.5 GB`, `512 MB`: binary multiples with Windows' labels.
pub fn bytes_text(bytes: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes.max(0.0);
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 || value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Recent values of every sensor.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Trends {
    generation: u32,
    series: Vec<VecDeque<Option<f64>>>,
    /// Samples pushed since the sensor set last changed. Every series gets
    /// one value per push, so the oldest value kept is sample number
    /// `pushes - len`.
    pushes: u64,
}

impl Trends {
    /// Adds one sample. A new sensor set starts the history over, because
    /// its IDs mean other sensors.
    pub fn push(&mut self, generation: u32, samples: &[Sample]) {
        if generation != self.generation {
            self.generation = generation;
            self.series.clear();
            self.pushes = 0;
        }
        self.pushes += 1;
        let sensor_count = samples.iter().map(|sample| sample.id as usize + 1).max().unwrap_or(0);
        if self.series.len() < sensor_count {
            self.series.resize_with(sensor_count, VecDeque::new);
        }
        for (id, series) in self.series.iter_mut().enumerate() {
            if series.len() == TREND_LEN {
                series.pop_front();
            }
            let sample = samples.iter().find(|sample| sample.id as usize == id);
            series.push_back(sample.and_then(|sample| sample.value));
        }
    }

    /// The latest value of a sensor.
    pub fn latest(&self, id: u32) -> Option<f64> {
        self.series.get(id as usize)?.back().copied().flatten()
    }

    /// The recent values of a sensor, oldest first.
    pub fn series(&self, id: u32) -> Vec<Option<f64>> {
        self.series.get(id as usize).map(|s| s.iter().copied().collect()).unwrap_or_default()
    }

    /// The points of a sensor's trend line: the average of each complete
    /// [`CHART_BUCKET`] of samples, oldest first. Buckets are aligned to the
    /// sample count, not to the window, so a point never changes once drawn
    /// and the line moves only when a bucket completes. A bucket without a
    /// single reading is a gap.
    ///
    /// The oldest bucket may have lost samples to the window; it is averaged
    /// over what is left rather than dropped, so the number of points (and
    /// with it every segment's position) does not change between two
    /// completed buckets.
    ///
    /// Also returns the number of the first point, counted in buckets since
    /// the sensor set last changed, so a chart can give each point a place
    /// that does not move when newer points arrive.
    pub fn numbered_chart_points(&self, id: u32) -> (u64, Vec<Option<f64>>) {
        let values = self.series(id);
        if values.is_empty() {
            return (0, Vec::new());
        }
        let size = CHART_BUCKET as u64;
        let end = self.pushes;
        let first = end - values.len() as u64;
        let start = first - first % size;
        let complete_end = end - end % size;
        let points = (start..complete_end)
            .step_by(CHART_BUCKET)
            .map(|bucket| {
                let from = (bucket.max(first) - first) as usize;
                let to = (bucket + size - first) as usize;
                let known: Vec<f64> = values[from..to].iter().flatten().copied().collect();
                (!known.is_empty()).then(|| known.iter().sum::<f64>() / known.len() as f64)
            })
            .collect();
        (start / size, points)
    }

    /// Lowest and highest recent value of a sensor.
    pub fn range(&self, id: u32) -> Option<(f64, f64)> {
        let values = self.series.get(id as usize)?.iter().flatten();
        values.fold(None, |range, value| match range {
            None => Some((*value, *value)),
            Some((low, high)) => Some((low.min(*value), high.max(*value))),
        })
    }
}

/// A headline sensor for the home page.
#[derive(Debug, Clone, PartialEq)]
pub struct Vital {
    /// What it measures, e.g. `Processor load`.
    pub label: &'static str,
    /// The sensor behind it.
    pub sensor: SensorInfo,
}

/// Picks the few sensors worth showing at a glance: one temperature and one
/// load per processor and graphics card, memory in use, and the first fan.
pub fn vitals(sensors: &SensorList) -> Vec<Vital> {
    let pick = |class: DeviceClass, kind: SensorKind| {
        sensors.sensors.iter().find(|sensor| {
            sensor.kind == kind
                && sensors.devices.get(sensor.device as usize).is_some_and(|d| d.class == class)
        })
    };
    let wanted = [
        ("Processor load", DeviceClass::Cpu, SensorKind::Load),
        ("Processor temperature", DeviceClass::Cpu, SensorKind::Temperature),
        ("Graphics load", DeviceClass::Gpu, SensorKind::Load),
        ("Graphics temperature", DeviceClass::Gpu, SensorKind::Temperature),
        ("Memory in use", DeviceClass::Memory, SensorKind::Load),
        ("Storage temperature", DeviceClass::Storage, SensorKind::Temperature),
        ("Fan speed", DeviceClass::Board, SensorKind::Fan),
    ];
    wanted
        .into_iter()
        .filter_map(|(label, class, kind)| {
            pick(class, kind).map(|sensor| Vital { label, sensor: sensor.clone() })
        })
        .collect()
}

/// The value range a chart of `kind` always includes, so that a steady
/// reading draws a calm line instead of magnified noise.
pub fn chart_scale(kind: SensorKind) -> (Option<f64>, Option<f64>) {
    match kind {
        SensorKind::Load | SensorKind::Duty | SensorKind::Level => (Some(0.0), Some(100.0)),
        SensorKind::Temperature => (Some(20.0), Some(100.0)),
        _ => (Some(0.0), None),
    }
}

/// `41 s ago`, `2 h 5 min ago`.
pub fn ago_text(now_unix: u64, then_unix: u64) -> String {
    match now_unix.saturating_sub(then_unix) {
        0 => "Just now".into(),
        seconds => format!("{} ago", duration_text(seconds)),
    }
}

/// The badge for a line of history.
pub fn history_badge(kind: HistoryKind) -> Badge {
    let (text, tone) = match kind {
        HistoryKind::Applied => ("Applied", Tone::Good),
        HistoryKind::Restored => ("Restored", Tone::Good),
        HistoryKind::Reverted => ("Turned off", Tone::Neutral),
        HistoryKind::Contested => ("Contested", Tone::Caution),
        HistoryKind::Refused => ("Blocked by Windows", Tone::Bad),
        HistoryKind::Failed => ("Failed", Tone::Bad),
    };
    Badge { text, tone }
}

/// The name of an entry: its catalog title, its custom action title, or
/// (for an entry that no longer exists) its ID.
pub fn entry_title(snapshot: &Snapshot, id: &str) -> String {
    let custom = || {
        snapshot.profile.iter().flat_map(|profile| &profile.custom).find(|custom| custom.id == id)
    };
    snapshot
        .catalog
        .get(id)
        .map(|tweak| tweak.title.clone())
        .or_else(|| custom().map(|custom| custom.title.clone()))
        .unwrap_or_else(|| id.to_owned())
}

/// How many tweaks of a category are on, and how many there are, counting
/// what is visible at `level` and leaving out checks.
pub fn category_count(snapshot: &Snapshot, category: Category, level: ViewLevel) -> (usize, usize) {
    let tweaks = snapshot
        .catalog
        .tweaks
        .iter()
        .filter(|tweak| tweak.category == category && !tweak.audit && visible_at(tweak, level));
    tweaks.fold((0, 0), |(on, all), tweak| {
        let enabled = snapshot.states.iter().any(|status| status.id == tweak.id && status.enabled);
        (on + usize::from(enabled), all + 1)
    })
}

/// Whether a sensor is one of many of its kind on a device (one per
/// processor thread or core), told by a numbered part in its key such as
/// `cpu.thread12.load` or `cpu.core3.temp`. Those are shown folded away.
pub fn is_per_unit_sensor(key: &str) -> bool {
    key.split('.').any(|part| {
        ["thread", "core"].iter().any(|unit| {
            part.strip_prefix(unit).is_some_and(|number| {
                !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())
            })
        })
    })
}

/// Whether a sensor is a raw input of the board's monitoring chip whose
/// meaning depends on how the board is wired: an auxiliary temperature
/// input may be connected to nothing and read over 100 °C, a voltage input
/// has no fixed rail, a duty output may drive no fan. Until a description
/// of the board says what they are, they are detail, and never coloured.
pub fn is_raw_board_input(key: &str) -> bool {
    ["board.temp.auxtin", "board.vin", "board.pwm"].iter().any(|raw| key.starts_with(raw))
}

/// The sensors of one device the user has not hidden, split into the ones
/// always shown and the detail that is folded away: per-unit readings and
/// raw board inputs.
pub fn device_sensors<'a>(
    sensors: &'a SensorList,
    device: usize,
    hidden: &BTreeSet<String>,
) -> (Vec<&'a SensorInfo>, Vec<&'a SensorInfo>) {
    sensors
        .sensors
        .iter()
        .filter(|sensor| sensor.device as usize == device && !hidden.contains(&sensor.key))
        .partition(|sensor| !is_per_unit_sensor(&sensor.key) && !is_raw_board_input(&sensor.key))
}

/// The sensors of this PC the user hid, in the service's order. A hidden
/// key this PC no longer reports is left out: there is nothing to show.
pub fn hidden_sensors<'a>(
    sensors: &'a SensorList,
    hidden: &BTreeSet<String>,
) -> Vec<&'a SensorInfo> {
    sensors.sensors.iter().filter(|sensor| hidden.contains(&sensor.key)).collect()
}

/// Whether a temperature is high enough to colour. A raw board input is
/// not: what it measures is not known, so neither is what is high for it.
pub fn reading_tone(sensor: &SensorInfo, value: Option<f64>) -> Tone {
    match (value, sensor.kind) {
        _ if is_raw_board_input(&sensor.key) => Tone::Neutral,
        (Some(v), SensorKind::Temperature) if v >= 90.0 => Tone::Bad,
        (Some(v), SensorKind::Temperature) if v >= 80.0 => Tone::Caution,
        _ => Tone::Neutral,
    }
}

/// Why fans are not controlled on this PC, in the order the user can act on
/// it: a build without the feature cannot be fixed by anything on the PC, a
/// missing driver can, and a board that is not validated cannot be fixed
/// from the app at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FanGate {
    /// Colour meaning of the explanation.
    pub tone: Tone,
    /// The explanation, as sentences.
    pub text: String,
    /// Whether installing or updating PawnIO is the way out, which makes
    /// the page offer "Get PawnIO".
    pub needs_driver: bool,
}

/// Ends `text` with a full stop, keeping its capitals as written.
fn with_full_stop(text: &str) -> String {
    let text = text.trim();
    if text.is_empty() || text.ends_with(['.', '!', '?']) {
        text.to_owned()
    } else {
        format!("{text}.")
    }
}

/// What stops fan control, or `None` when the service follows curves.
///
/// Precedence: the build, then the driver (the service's own reason would
/// only repeat it, with less detail), then the board.
pub fn fan_gate(status: &ServiceStatus) -> Option<FanGate> {
    if status.fans.availability == FanAvailability::NotInBuild {
        return Some(FanGate {
            tone: Tone::Neutral,
            text: "This build of SysPilot cannot control fans. Fan control is left out of releases until it has been checked on real hardware, so the BIOS stays in charge of every fan."
                .into(),
            needs_driver: false,
        });
    }
    let driver = match &status.driver {
        DriverState::Ready { .. } => None,
        DriverState::Missing => Some((
            Tone::Caution,
            "PawnIO is not installed. SysPilot reads and sets fan speeds through it.".to_owned(),
        )),
        DriverState::TooOld { version, required } => Some((
            Tone::Caution,
            format!("PawnIO {version} is too old. Version {required} or later is needed."),
        )),
        DriverState::Failed { error } => {
            Some((Tone::Bad, format!("PawnIO is installed but can't be used. {}", sentence(error))))
        }
    };
    if let Some((tone, why)) = driver {
        return Some(FanGate {
            tone,
            text: format!("Fan control is off. {why}"),
            needs_driver: true,
        });
    }
    match &status.fans.availability {
        FanAvailability::ReadOnly { reason } => Some(FanGate {
            tone: Tone::Neutral,
            text: if reason.trim().is_empty() {
                "Fan control is off for this board.".into()
            } else {
                format!("Fan control is off for this board: {}", with_full_stop(reason))
            },
            needs_driver: false,
        }),
        FanAvailability::NotInBuild | FanAvailability::Controllable => None,
    }
}

/// The line under the page title: what the page shows, and for which board.
pub fn fan_page_subtitle(board: &str) -> String {
    let about = "Each fan's speed, the curve that drives it and whether SysPilot follows it.";
    match board.trim() {
        "" => about.to_owned(),
        board => format!("{board}. {about}"),
    }
}

/// What the page says when there are no fan channels to list. A build
/// without fan control never reads the board, so it says nothing about it.
pub fn fan_channels_missing_text(fans: &FanStatus) -> String {
    let board = fans.board.trim();
    if fans.availability == FanAvailability::NotInBuild {
        "Fan speeds are on the Sensors page.".into()
    } else if board.is_empty() {
        "This PC's board is not in SysPilot's fan catalog, so there are no fan channels to show here. Raw fan speeds are on the Sensors page."
            .into()
    } else {
        format!(
            "The catalog lists no fan channels for {board}. Raw fan speeds are on the Sensors page."
        )
    }
}

/// How one fan channel stands, as a badge and a sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FanState {
    /// Short label, from the glossary in `docs/ui-glossary.md`.
    pub badge: Badge,
    /// The state in words, with the duty SysPilot asks for when it does.
    pub text: String,
}

/// The state of a channel in words.
///
/// The phase says who holds the channel; `active` and the curve say why the
/// BIOS holds it when it does.
pub fn fan_state(channel: &FanChannelStatus) -> FanState {
    let target = channel
        .target_duty_pct
        .map_or(String::new(), |duty| format!(" SysPilot is asking for {duty} %."));
    let (badge, text) = match channel.phase {
        FanPhase::Manual => (
            Badge { text: "Following curve", tone: Tone::Good },
            format!("Following its curve.{target}"),
        ),
        FanPhase::FailSafe => (
            Badge { text: "Full speed", tone: Tone::Caution },
            format!("Held at full speed because a temperature sensor failed.{target}"),
        ),
        FanPhase::ReleasePending => (
            Badge { text: "Release pending", tone: Tone::Caution },
            "Handing this fan back to the BIOS did not work. SysPilot is retrying.".to_owned(),
        ),
        FanPhase::Blocked => (
            Badge { text: "Left alone", tone: Tone::Caution },
            "Left alone: this fan was found in manual mode, with no saved setting to put back."
                .to_owned(),
        ),
        FanPhase::Bios if channel.curve.is_empty() => (
            Badge { text: "BIOS control", tone: Tone::Neutral },
            "The BIOS controls this fan. The profile has no curve for it.".to_owned(),
        ),
        FanPhase::Bios if !channel.active => (
            Badge { text: "BIOS control", tone: Tone::Neutral },
            "The BIOS controls this fan. Its curve is shown but not followed.".to_owned(),
        ),
        FanPhase::Bios => (
            Badge { text: "BIOS control", tone: Tone::Neutral },
            "Handed back to the BIOS.".to_owned(),
        ),
    };
    FanState { badge, text }
}

/// The problem of a channel as a sentence for the page, if it has one.
pub fn fan_problem_text(channel: &FanChannelStatus) -> Option<String> {
    let problem = channel.problem.as_deref().map(str::trim).filter(|text| !text.is_empty())?;
    Some(format!("Problem: {}", with_full_stop(problem)))
}

/// What a channel's curve is driven by and how low it may go. `None` for a
/// channel without a curve: it has no sensor, and its lowest duty only
/// matters once a curve drives it, so the line would be noise.
pub fn fan_limits_text(channel: &FanChannelStatus) -> Option<String> {
    if channel.sources.is_empty() {
        return None;
    }
    let sources = format!("Driven by the hottest of {}.", channel.sources.join(", "));
    Some(if channel.min_duty_pct == 0 {
        sources
    } else {
        // A no-break space keeps "30" and "%" on one line.
        format!("{sources} Never below {}\u{a0}%.", channel.min_duty_pct)
    })
}

/// The live numbers of one fan channel.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct FanReadings {
    /// Fan speed, RPM, from the tachometer sensor.
    pub speed: Option<f64>,
    /// Duty the chip drives, percent, from the channel's duty sensor.
    pub duty: Option<f64>,
    /// The hottest of the curve's temperature sources, degrees Celsius.
    pub temperature: Option<f64>,
}

/// Joins a channel to its sensors by key. A sensor of the wrong kind under
/// the key counts as missing: showing a voltage as RPM would be worse than
/// showing nothing.
pub fn fan_readings(
    channel: &FanChannelStatus,
    sensors: &SensorList,
    trends: &Trends,
) -> FanReadings {
    let latest = |key: &str, kind: SensorKind| {
        let sensor = sensors.sensors.iter().find(|sensor| sensor.key == key)?;
        if sensor.kind == kind { trends.latest(sensor.id) } else { None }
    };
    FanReadings {
        speed: channel.tach_sensor.as_deref().and_then(|key| latest(key, SensorKind::Fan)),
        duty: latest(&channel.channel, SensorKind::Duty),
        temperature: channel
            .sources
            .iter()
            .filter_map(|key| latest(key, SensorKind::Temperature))
            .reduce(f64::max),
    }
}

/// Temperatures every curve chart spans at least, so that the charts of
/// different fans line up.
const CURVE_TEMPERATURES: (f64, f64) = (20.0, 100.0);

/// The temperature range a chart of `curve` spans: the usual range, widened
/// to whole tens when a point lies outside it.
pub fn curve_temperature_domain(curve: &[CurvePoint]) -> (f64, f64) {
    let (mut low, mut high) = CURVE_TEMPERATURES;
    for point in curve {
        let temperature = f64::from(point.temp_c());
        low = low.min((temperature / 10.0).floor() * 10.0);
        high = high.max((temperature / 10.0).ceil() * 10.0);
    }
    (low, high)
}

/// A curve laid out in pixels, ready to draw. Pure data, so the mapping is
/// tested without a window.
#[derive(Debug, Clone, PartialEq)]
pub struct CurveShape {
    /// The curve's points, left to right.
    pub points: Vec<(f64, f64)>,
    /// Heights of the grid lines: 0, 50 and 100 % duty.
    pub grid: [f64; 3],
    /// Horizontal position of the current temperature, when it is known.
    pub marker: Option<f64>,
    /// Temperature range the chart spans.
    pub domain: (f64, f64),
}

/// Space kept free inside a chart, so that a point on the edge is not cut.
const CURVE_INSET: f64 = 5.0;

/// Lays `channel`'s curve out in a chart of `size` pixels.
/// `temperature` places the marker; it is clamped to the chart's range.
pub fn curve_shape(
    channel: &FanChannelStatus,
    temperature: Option<f64>,
    (width, height): (f64, f64),
) -> CurveShape {
    let domain = curve_temperature_domain(&channel.curve);
    let x = |temperature: f64| {
        let share = (temperature.clamp(domain.0, domain.1) - domain.0) / (domain.1 - domain.0);
        CURVE_INSET + share * (width - 2.0 * CURVE_INSET)
    };
    let y = |duty: f64| {
        height - CURVE_INSET - duty.clamp(0.0, 100.0) / 100.0 * (height - 2.0 * CURVE_INSET)
    };
    CurveShape {
        points: channel
            .curve
            .iter()
            .map(|point| (x(f64::from(point.temp_c())), y(f64::from(point.duty_pct()))))
            .collect(),
        grid: [y(0.0), y(50.0), y(100.0)],
        marker: temperature.map(x),
        domain,
    }
}

/// A curve as text, for screen readers and tooltips: the chart's data.
pub fn curve_description(curve: &[CurvePoint]) -> String {
    if curve.is_empty() {
        return "No curve.".to_owned();
    }
    let points: Vec<String> = curve
        .iter()
        .map(|point| format!("{} °C: {} %", point.temp_c(), point.duty_pct()))
        .collect();
    format!("Fan curve, temperature to duty. {}.", points.join("; "))
}

/// Values the user entered on the Tuning page and has not saved: by device
/// key, then by control key.
pub type TuningDrafts = HashMap<String, BTreeMap<String, i32>>;

/// The badge for a device on the Tuning page. `None` for a device left to
/// its driver, which has nothing worth saying.
pub fn tuning_badge(state: &TuningState) -> Option<Badge> {
    match state {
        TuningState::Unmanaged => None,
        TuningState::Applied => badge("Applied", Tone::Good),
        TuningState::Different => badge("Not applied yet", Tone::Caution),
        TuningState::Contested => badge("Contested", Tone::Caution),
        TuningState::Held => badge("Held back", Tone::Caution),
        TuningState::Failed { .. } => badge("Failed", Tone::Bad),
        TuningState::Missing => badge("Not found", Tone::Neutral),
    }
}

/// A sentence explaining a tuning state that needs explaining, and whether
/// "Apply again" is the way out of it.
pub fn tuning_note(state: &TuningState) -> Option<(String, bool)> {
    match state {
        TuningState::Unmanaged | TuningState::Applied => None,
        TuningState::Different => Some((
            "The device has other values right now. SysPilot is setting yours.".to_owned(),
            false,
        )),
        TuningState::Contested => Some((
            "Something else keeps changing these values, often the graphics driver's own \
             app. SysPilot stopped setting them."
                .to_owned(),
            true,
        )),
        TuningState::Held => Some((
            "Windows did not shut down normally after these values were last set, so SysPilot \
             has not set them again. Change them, or apply them again if they were not the \
             cause."
                .to_owned(),
            true,
        )),
        TuningState::Failed { reason } => {
            Some((format!("The values could not be set. {}", sentence(reason)), true))
        }
        TuningState::Missing => Some((
            "This device is not in this PC right now. Its values are kept until you remove them."
                .to_owned(),
            false,
        )),
    }
}

/// The name of a control. State tables name their columns instead.
pub fn tuning_control_name(kind: TuningKind) -> &'static str {
    match kind {
        TuningKind::PowerLimit => "Power limit",
        TuningKind::TemperatureLimit => "Temperature limit",
        TuningKind::FanTargetTemperature => "Fan target temperature",
        TuningKind::FanMaxSpeed => "Highest fan speed",
        TuningKind::FanMinSpeed => "Lowest fan speed",
        TuningKind::QuietClockFloor => "Lowest clock when quiet",
        TuningKind::CoreClock => "Clock",
        TuningKind::CoreVoltage => "Voltage",
        TuningKind::MemoryClock => "Memory clock",
        TuningKind::MemoryVoltage => "Memory voltage",
    }
}

/// What a control does, for the controls whose name does not say it.
pub fn tuning_control_detail(kind: TuningKind) -> &'static str {
    match kind {
        TuningKind::PowerLimit => {
            "How much more or less power than its default the device may draw."
        }
        TuningKind::TemperatureLimit => "Above this the device slows itself down.",
        TuningKind::FanTargetTemperature => "The temperature its fan works to hold.",
        TuningKind::FanMaxSpeed | TuningKind::FanMinSpeed => "A bound of the driver's fan curve.",
        TuningKind::QuietClockFloor => {
            "How far the device may slow down to keep its fan at the target."
        }
        TuningKind::CoreClock
        | TuningKind::CoreVoltage
        | TuningKind::MemoryClock
        | TuningKind::MemoryVoltage => "",
    }
}

/// Whether a wrong value of this kind can make the device unstable. Limits
/// only make it slower or louder.
pub fn tuning_kind_is_risky(kind: TuningKind) -> bool {
    matches!(
        kind,
        TuningKind::CoreClock
            | TuningKind::CoreVoltage
            | TuningKind::MemoryClock
            | TuningKind::MemoryVoltage
    )
}

/// A value with its unit, with a real minus sign and a no-break space.
pub fn tuning_value_text(value: i32, unit: TuningUnit) -> String {
    let sign = if value < 0 { "\u{2212}" } else { "" };
    format!("{sign}{}\u{a0}{}", value.unsigned_abs(), unit.suffix())
}

/// The value the page shows for a control: what the user entered, else what
/// is saved, else the driver's default.
pub fn tuning_shown_value(control: &TuningControl, draft: Option<&BTreeMap<String, i32>>) -> i32 {
    draft.and_then(|draft| draft.get(&control.key).copied()).unwrap_or_else(|| control.expected())
}

/// The line under a control: its range and default, and what the device has
/// now when that is something else than what is shown.
pub fn tuning_control_hint(control: &TuningControl, shown: i32) -> String {
    let text = |value| tuning_value_text(value, control.unit);
    let mut hint = format!(
        "{} to {}, default {}",
        text(control.min),
        text(control.max),
        text(control.default)
    );
    if control.current != shown {
        let _ = write!(hint, " · now {}", text(control.current));
    }
    hint
}

/// The short line under a value in a state table, where there is no room
/// for the range: the default when the shown value is another, and what the
/// device has now when that is another again. Empty when all three agree.
pub fn tuning_cell_note(control: &TuningControl, shown: i32) -> String {
    let text = |value| tuning_value_text(value, control.unit);
    let mut parts = Vec::new();
    if shown != control.default {
        parts.push(format!("default {}", text(control.default)));
    }
    if control.current != shown {
        parts.push(format!("now {}", text(control.current)));
    }
    parts.join(" · ")
}

/// One performance state in a table: its clock and its voltage, either of
/// which the driver may not offer.
#[derive(Debug, Clone, PartialEq)]
pub struct TuningStateRow<'a> {
    /// Number of the state; higher states are used under more load.
    pub state: u8,
    /// The clock control.
    pub clock: Option<&'a TuningControl>,
    /// The voltage control.
    pub voltage: Option<&'a TuningControl>,
}

/// The controls of a device, arranged for the page.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TuningLayout<'a> {
    /// Controls that stand alone: limits and fan bounds.
    pub limits: Vec<&'a TuningControl>,
    /// The graphics processor's states, lowest first.
    pub core: Vec<TuningStateRow<'a>>,
    /// The graphics memory's states, lowest first.
    pub memory: Vec<TuningStateRow<'a>>,
}

/// Arranges the controls of a device: limits in the order the service lists
/// them, per-state controls as rows of a table.
pub fn tuning_layout(controls: &[TuningControl]) -> TuningLayout<'_> {
    fn row<'a, 'b>(rows: &'b mut Vec<TuningStateRow<'a>>, state: u8) -> &'b mut TuningStateRow<'a> {
        let index = match rows.iter().position(|row| row.state == state) {
            Some(index) => index,
            None => {
                rows.push(TuningStateRow { state, clock: None, voltage: None });
                rows.len() - 1
            }
        };
        &mut rows[index]
    }
    let mut layout = TuningLayout::default();
    for control in controls {
        match (control.kind, control.state) {
            (TuningKind::CoreClock, Some(state)) => {
                row(&mut layout.core, state).clock = Some(control)
            }
            (TuningKind::CoreVoltage, Some(state)) => {
                row(&mut layout.core, state).voltage = Some(control);
            }
            (TuningKind::MemoryClock, Some(state)) => {
                row(&mut layout.memory, state).clock = Some(control);
            }
            (TuningKind::MemoryVoltage, Some(state)) => {
                row(&mut layout.memory, state).voltage = Some(control);
            }
            _ => layout.limits.push(control),
        }
    }
    layout.core.sort_by_key(|row| row.state);
    layout.memory.sort_by_key(|row| row.state);
    layout
}

/// The values to save for a device: every shown value that is not the
/// driver's default. A default needs no entry; it is what an unlisted
/// control gets.
pub fn tuning_values(
    device: &TuningDevice,
    draft: Option<&BTreeMap<String, i32>>,
) -> BTreeMap<String, i32> {
    device
        .controls
        .iter()
        .map(|control| (control, tuning_shown_value(control, draft)))
        .filter(|(control, shown)| *shown != control.default)
        .map(|(control, shown)| (control.key.clone(), shown))
        .collect()
}

/// Whether `values` move a clock or a voltage of `device` off its default.
pub fn tuning_values_are_risky(device: &TuningDevice, values: &BTreeMap<String, i32>) -> bool {
    device
        .controls
        .iter()
        .any(|control| tuning_kind_is_risky(control.kind) && values.contains_key(&control.key))
}

/// The settings to send when `device` gets `values` and is turned on or
/// off: the profile's settings with this device's replaced, or added.
pub fn tuning_settings_with(
    saved: &[TuningSetting],
    device: &str,
    enabled: bool,
    values: BTreeMap<String, i32>,
) -> Vec<TuningSetting> {
    let setting = TuningSetting { device: device.to_owned(), enabled, values };
    let mut settings = saved.to_vec();
    match settings.iter_mut().find(|existing| existing.device == device) {
        Some(existing) => *existing = setting,
        None => settings.push(setting),
    }
    // A setting that is off and lists nothing says nothing.
    settings.retain(|setting| setting.enabled || !setting.values.is_empty());
    settings
}

/// The draft for a control after the user entered `entered`: clamped to the
/// control's range, and dropped when it is what is saved anyway. `None`
/// means the entry changes nothing (the box echoing its own value, or an
/// emptied box).
pub fn tuning_draft_after(
    control: &TuningControl,
    draft: Option<&BTreeMap<String, i32>>,
    entered: Option<f64>,
) -> Option<Option<i32>> {
    let entered = entered.filter(|value| value.is_finite())?;
    // Clamped to the device's own range, the number fits an `i32`, so the
    // cast cannot wrap however large the entry was.
    let value = entered.round().clamp(f64::from(control.min), f64::from(control.max)) as i32;
    if value == tuning_shown_value(control, draft) {
        return None;
    }
    Some((value != control.expected()).then_some(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use syspilot_proto::message::{ActionStatus, FanStatus};
    use syspilot_proto::sensor::DeviceInfo;

    const CATALOG: &str = r#"
[[tweak]]
id = "privacy.ads"
title = "Turn off the advertising ID"
summary = "Apps can no longer use your advertising ID."
level = "simple"
category = "privacy"
side-effects = "Ads are less relevant."
reboot = "sign-out"
source = "https://learn.microsoft.com/windows/"
[[tweak.action]]
registry = { scope = "user", key = 'Software\Example', name = "Enabled", data = { dword = 0 } }

[[tweak]]
id = "security.mitigations"
title = "Turn off mitigations"
summary = "Removes a protection."
level = "advanced"
category = "security"
direction = "weakens"
risk = "Other programs could read memory they should not."
source = "https://learn.microsoft.com/windows/"
[[tweak.action]]
registry = { scope = "machine", key = 'SYSTEM\Example', name = "Off", data = { dword = 1 } }

[[tweak]]
id = "security.check"
title = "Check that Secure Boot is on"
summary = "Secure Boot stops tampered boot software."
level = "advanced"
category = "security"
direction = "hardens"
audit = true
source = "https://learn.microsoft.com/windows/"
[[tweak.action]]
registry = { scope = "machine", key = 'SYSTEM\Example', name = "On", data = { dword = 1 } }
"#;

    fn status(id: &str, enabled: bool, state: ActionState) -> EntryStatus {
        EntryStatus {
            id: id.into(),
            enabled,
            state: state.clone(),
            actions: vec![ActionStatus { state, current: "dword:1".into() }],
            drift_count: 0,
            last_drift_unix: None,
        }
    }

    fn snapshot(states: Vec<EntryStatus>) -> Snapshot {
        Snapshot { catalog: Catalog::parse(CATALOG).unwrap(), states, ..Snapshot::default() }
    }

    #[test]
    fn every_state_maps_to_one_glossary_term() {
        let text = |enabled, state, audit| {
            entry_badge(&status("a.b", enabled, state), audit).map(|badge| badge.text)
        };
        assert_eq!(text(true, ActionState::Compliant, false), Some("Applied"));
        assert_eq!(text(false, ActionState::Compliant, false), Some("Already set"));
        assert_eq!(text(false, ActionState::Different, false), None);
        assert_eq!(text(true, ActionState::Different, false), Some("Not applied yet"));
        assert_eq!(
            text(true, ActionState::Contested { retry_in_secs: 5 }, false),
            Some("Contested")
        );
        let blocked = ActionState::NotEnforceable { reason: "x".into() };
        assert_eq!(text(true, blocked, false), Some("Blocked by Windows"));
        assert_eq!(text(true, ActionState::Failed { error: "x".into() }, false), Some("Failed"));
        assert_eq!(text(true, ActionState::Unknown, false), Some("Can't check"));
        let absent = ActionState::NotApplicable { reason: "x".into() };
        assert_eq!(text(false, absent, false), Some("Not available"));
        assert_eq!(text(false, ActionState::Compliant, true), Some("Passed"));
        assert_eq!(text(false, ActionState::Different, true), Some("Needs attention"));
        assert_eq!(text(false, ActionState::Unknown, true), Some("Can't check"));
    }

    #[test]
    fn unusual_states_are_explained_in_a_sentence() {
        let contested = status("a.b", true, ActionState::Contested { retry_in_secs: 125 });
        assert!(entry_note(&contested).unwrap().ends_with("will try again in 2 min."));
        let absent = ActionState::NotApplicable { reason: "needs Windows build 26100".into() };
        assert_eq!(
            entry_note(&status("a.b", false, absent)).unwrap(),
            "Needs Windows build 26100."
        );
        assert_eq!(entry_note(&status("a.b", true, ActionState::Compliant)), None);
        assert_eq!(entry_note(&status("a.b", false, ActionState::Unknown)), None);
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(duration_text(41), "41 s");
        assert_eq!(duration_text(125), "2 min");
        assert_eq!(duration_text(3600), "1 h");
        assert_eq!(duration_text(2 * 3600 + 300), "2 h 5 min");
        assert_eq!(duration_text(3 * 86_400 + 4 * 3600), "3 d 4 h");
        assert_eq!(duration_text(86_400 + 300), "1 d", "no empty hours after a day count");
    }

    #[test]
    fn simple_level_hides_advanced_tweaks_and_empty_categories() {
        let snapshot = snapshot(vec![]);
        assert_eq!(categories(&snapshot, ViewLevel::Simple), [Category::Privacy]);
        assert_eq!(
            categories(&snapshot, ViewLevel::Advanced),
            [Category::Privacy, Category::Security]
        );
        assert!(tweak_rows(&snapshot, Category::Security, ViewLevel::Simple, "").is_empty());
        assert_eq!(tweak_rows(&snapshot, Category::Security, ViewLevel::Advanced, "").len(), 2);
    }

    #[test]
    fn rows_carry_state_flags_and_facts() {
        let mut ads = status("privacy.ads", true, ActionState::Compliant);
        ads.drift_count = 3;
        let snapshot = snapshot(vec![ads]);
        let row = &tweak_rows(&snapshot, Category::Privacy, ViewLevel::Simple, "")[0];
        assert!(row.enabled && row.available && !row.audit && !row.weakens);
        assert_eq!(row.badge.as_ref().unwrap().text, "Applied");
        assert_eq!(row.restored, 3);
        assert_eq!(
            row.facts,
            [
                ("Side effects", "Ads are less relevant.".to_owned()),
                ("Takes effect", "After you sign out and back in".to_owned()),
            ]
        );
        assert_eq!(row.settings.len(), 1);
        assert_eq!(row.settings[0].1, "dword:1");

        let rows = tweak_rows(&snapshot, Category::Security, ViewLevel::Advanced, "");
        assert!(rows[0].weakens, "a weakening tweak is flagged for confirmation");
        assert!(rows[1].audit && !rows[1].available, "a check has no switch");
    }

    #[test]
    fn filter_matches_title_summary_and_id_ignoring_case() {
        let snapshot = snapshot(vec![]);
        let found =
            |filter| tweak_rows(&snapshot, Category::Security, ViewLevel::Advanced, filter).len();
        assert_eq!(found("SECURE boot"), 1);
        assert_eq!(found("protection"), 1);
        assert_eq!(found("security."), 2);
        assert_eq!(found("no such thing"), 0);
    }

    #[test]
    fn overview_counts_tweaks_and_checks_separately() {
        let mut restored = status("privacy.ads", true, ActionState::Compliant);
        restored.drift_count = 4;
        let snapshot = snapshot(vec![
            restored,
            status("security.mitigations", true, ActionState::Contested { retry_in_secs: 60 }),
            status("security.check", false, ActionState::Compliant),
            status("custom.mine", false, ActionState::Different),
        ]);
        assert_eq!(
            overview(&snapshot),
            Overview {
                enabled: 2,
                applied: 1,
                problems: 1,
                checks_passed: 1,
                checks_total: 1,
                restored: 4
            }
        );
        let attention = attention(&snapshot);
        assert_eq!(attention.len(), 1);
        assert_eq!(attention[0].0, "Turn off mitigations");
        assert_eq!(attention[0].1.text, "Contested");
    }

    #[test]
    fn failed_check_asks_for_attention_with_its_summary() {
        let snapshot = snapshot(vec![status("security.check", false, ActionState::Different)]);
        let attention = attention(&snapshot);
        assert_eq!(attention[0].1.text, "Needs attention");
        assert_eq!(attention[0].2, "Secure Boot stops tampered boot software.");
    }

    #[test]
    fn readings_use_fitting_precision_and_units() {
        assert_eq!(reading_text(Some(45.4), SensorKind::Temperature), "45 °C");
        assert_eq!(reading_text(Some(12.34), SensorKind::Power), "12.3 W");
        assert_eq!(reading_text(Some(1234.4), SensorKind::Fan), "1234 RPM");
        assert_eq!(reading_text(Some(4200.0), SensorKind::Frequency), "4.20 GHz");
        assert_eq!(reading_text(Some(800.0), SensorKind::Frequency), "800 MHz");
        assert_eq!(reading_text(Some(1.2345), SensorKind::Voltage), "1.234 V");
        assert_eq!(reading_text(Some(1536.0 * 1024.0), SensorKind::Data), "1.5 MB");
        assert_eq!(reading_text(Some(2048.0), SensorKind::Throughput), "2.0 KB/s");
        assert_eq!(reading_text(None, SensorKind::Load), "—");
        assert_eq!(bytes_text(512.0), "512 B");
        assert_eq!(bytes_text(34.0 * 1024.0 * 1024.0 * 1024.0), "34.0 GB");
    }

    #[test]
    fn trends_keep_one_minute_and_restart_on_a_new_sensor_set() {
        let mut trends = Trends::default();
        for second in 0..(TREND_LEN + 5) {
            let failed_read = Sample { id: 1, value: None };
            trends.push(1, &[Sample { id: 0, value: Some(second as f64) }, failed_read]);
        }
        assert_eq!(trends.series(0).len(), TREND_LEN);
        assert_eq!(trends.latest(0), Some((TREND_LEN + 4) as f64));
        assert_eq!(trends.range(0), Some((5.0, (TREND_LEN + 4) as f64)));
        assert_eq!(trends.latest(1), None, "a sensor that could not be read has a gap");
        assert_eq!(trends.range(1), None);
        assert_eq!(trends.latest(7), None, "an unknown sensor has no history");

        trends.push(2, &[Sample { id: 0, value: Some(1.0) }]);
        assert_eq!(trends.series(0), [Some(1.0)], "IDs of a new set mean other sensors");
    }

    /// The points of a trend line, without their numbers.
    fn chart_points(trends: &Trends, id: u32) -> Vec<Option<f64>> {
        trends.numbered_chart_points(id).1
    }

    #[test]
    fn chart_points_average_complete_buckets_and_only_move_when_one_completes() {
        let mut trends = Trends::default();
        let push = |trends: &mut Trends, value: Option<f64>| {
            trends.push(1, &[Sample { id: 0, value }]);
        };
        push(&mut trends, Some(1.0));
        assert!(chart_points(&trends, 0).is_empty(), "(1 sample): expected no complete bucket");
        push(&mut trends, Some(3.0));
        assert_eq!(chart_points(&trends, 0), [Some(2.0)]);
        push(&mut trends, None);
        assert_eq!(chart_points(&trends, 0), [Some(2.0)], "a half bucket changes nothing");
        push(&mut trends, Some(5.0));
        assert_eq!(chart_points(&trends, 0), [Some(2.0), Some(5.0)], "a failed read is skipped");
        push(&mut trends, None);
        push(&mut trends, None);
        assert_eq!(
            chart_points(&trends, 0),
            [Some(2.0), Some(5.0), None],
            "an empty bucket is a gap"
        );
    }

    #[test]
    fn chart_points_keep_their_count_while_the_oldest_bucket_leaves_the_window() {
        let mut trends = Trends::default();
        for second in 0..TREND_LEN {
            trends.push(1, &[Sample { id: 0, value: Some(second as f64) }]);
        }
        let full = chart_points(&trends, 0);
        assert_eq!(full.len(), CHART_POINTS);

        // Samples 1..=60 are kept: bucket 0-1 lost sample 0, bucket 60-61 is half full.
        trends.push(1, &[Sample { id: 0, value: Some(TREND_LEN as f64) }]);
        let points = chart_points(&trends, 0);
        assert_eq!(
            points.len(),
            CHART_POINTS,
            "(points with sample 0 gone): expected {CHART_POINTS} | received: {}",
            points.len()
        );
        assert_eq!(points[0], Some(1.0), "(oldest point): expected what is left, sample 1");
        assert_eq!(points[1..], full[1..], "the other points are unchanged, so nothing moves");
        assert_eq!(trends.numbered_chart_points(0).0, 0, "(first point): expected bucket 0");

        trends.push(1, &[Sample { id: 0, value: Some(61.0) }]);
        let (first, points) = trends.numbered_chart_points(0);
        assert_eq!(first, 1, "(first point after bucket 0 left): expected bucket 1");
        assert_eq!(points.last(), Some(&Some(60.5)), "(newest point): expected samples 60 and 61");
        assert!(chart_points(&trends, 9).is_empty(), "an unknown sensor has no points");
    }

    #[test]
    fn vitals_pick_one_sensor_per_headline_measure() {
        let sensor = |id, device, kind| SensorInfo {
            id,
            device,
            key: format!("s{id}"),
            label: format!("Sensor {id}"),
            kind,
            driver: false,
        };
        let sensors = SensorList {
            generation: 1,
            devices: vec![
                DeviceInfo { name: "CPU".into(), class: DeviceClass::Cpu },
                DeviceInfo { name: "GPU".into(), class: DeviceClass::Gpu },
            ],
            sensors: vec![
                sensor(0, 0, SensorKind::Frequency),
                sensor(1, 0, SensorKind::Load),
                sensor(2, 0, SensorKind::Load),
                sensor(3, 1, SensorKind::Temperature),
            ],
        };
        let picked: Vec<_> = vitals(&sensors).iter().map(|v| (v.label, v.sensor.id)).collect();
        assert_eq!(picked, [("Processor load", 1), ("Graphics temperature", 3)]);
    }

    #[test]
    fn per_thread_and_per_core_sensors_are_told_from_the_rest() {
        for key in ["cpu.thread0.load", "cpu.thread27.load", "cpu.core3.temp", "cpu1.core12.temp"] {
            assert!(is_per_unit_sensor(key), "{key}");
        }
        for key in
            ["cpu.load", "cpu.core-max.temp", "cpu.package.temp", "board.fan1", "cpu.threads"]
        {
            assert!(!is_per_unit_sensor(key), "{key}");
        }
    }

    #[test]
    fn a_devices_sensors_split_into_main_and_per_unit() {
        let sensor = |id, device, key: &str| SensorInfo {
            id,
            device,
            key: key.into(),
            label: key.into(),
            kind: SensorKind::Load,
            driver: false,
        };
        let sensors = SensorList {
            generation: 1,
            devices: Vec::new(),
            sensors: vec![
                sensor(0, 0, "cpu.load"),
                sensor(1, 0, "cpu.thread0.load"),
                sensor(2, 1, "gpu0.load"),
                sensor(3, 0, "cpu.thread1.load"),
            ],
        };
        let nothing_hidden = BTreeSet::new();
        let (main, per_unit) = device_sensors(&sensors, 0, &nothing_hidden);
        assert_eq!(main.iter().map(|s| s.id).collect::<Vec<_>>(), [0]);
        assert_eq!(per_unit.iter().map(|s| s.id).collect::<Vec<_>>(), [1, 3]);
        assert_eq!(device_sensors(&sensors, 1, &nothing_hidden).0.len(), 1);
    }

    #[test]
    fn chart_scales_fit_the_quantity() {
        assert_eq!(chart_scale(SensorKind::Load), (Some(0.0), Some(100.0)));
        assert_eq!(chart_scale(SensorKind::Temperature), (Some(20.0), Some(100.0)));
        assert_eq!(chart_scale(SensorKind::Throughput), (Some(0.0), None));
    }

    #[test]
    fn times_are_told_relative_to_now() {
        assert_eq!(ago_text(1000, 1000), "Just now");
        assert_eq!(ago_text(1000, 1003), "Just now", "a clock slightly behind is not the future");
        assert_eq!(ago_text(1000, 959), "41 s ago");
        assert_eq!(ago_text(100_000, 100_000 - 7500), "2 h 5 min ago");
    }

    #[test]
    fn history_kinds_use_the_glossary_terms() {
        use syspilot_proto::message::HistoryKind;
        let text = |kind| history_badge(kind).text;
        assert_eq!(text(HistoryKind::Applied), "Applied");
        assert_eq!(text(HistoryKind::Restored), "Restored");
        assert_eq!(text(HistoryKind::Reverted), "Turned off");
        assert_eq!(text(HistoryKind::Contested), "Contested");
        assert_eq!(text(HistoryKind::Refused), "Blocked by Windows");
        assert_eq!(text(HistoryKind::Failed), "Failed");
        assert_eq!(history_badge(HistoryKind::Refused).tone, Tone::Bad);
    }

    #[test]
    fn entries_are_named_by_catalog_then_profile_then_id() {
        use syspilot_proto::action::{Action, ServiceAction, StartType};
        use syspilot_proto::profile::CustomAction;
        let mut snapshot = snapshot(vec![]);
        assert_eq!(entry_title(&snapshot, "privacy.ads"), "Turn off the advertising ID");
        assert_eq!(entry_title(&snapshot, "custom.mine"), "custom.mine");
        snapshot.profile = Some(Profile {
            custom: vec![CustomAction {
                id: "custom.mine".into(),
                title: "My action".into(),
                enabled: true,
                action: Action::Service(ServiceAction {
                    service: "Spooler".into(),
                    start: StartType::Manual,
                }),
                triggers: Vec::new(),
            }],
            ..Profile::default()
        });
        assert_eq!(entry_title(&snapshot, "custom.mine"), "My action");
    }

    #[test]
    fn category_counts_ignore_checks_and_hidden_tweaks() {
        let snapshot = snapshot(vec![
            status("security.mitigations", true, ActionState::Compliant),
            status("security.check", false, ActionState::Compliant),
        ]);
        assert_eq!(category_count(&snapshot, Category::Security, ViewLevel::Advanced), (1, 1));
        assert_eq!(category_count(&snapshot, Category::Security, ViewLevel::Simple), (0, 0));
        assert_eq!(category_count(&snapshot, Category::Privacy, ViewLevel::Simple), (0, 1));
    }

    #[test]
    fn only_hot_temperatures_are_coloured() {
        let sensor = |key: &str, kind| SensorInfo {
            id: 0,
            device: 0,
            key: key.into(),
            label: key.into(),
            kind,
            driver: false,
        };
        let temperature = sensor("cpu.package.temp", SensorKind::Temperature);
        assert_eq!(reading_tone(&temperature, Some(95.0)), Tone::Bad);
        assert_eq!(reading_tone(&temperature, Some(82.0)), Tone::Caution);
        assert_eq!(reading_tone(&temperature, Some(60.0)), Tone::Neutral);
        assert_eq!(reading_tone(&sensor("cpu.load", SensorKind::Load), Some(100.0)), Tone::Neutral);
        // An input nothing is wired to reads like this; it is not an alarm.
        let floating = sensor("board.temp.auxtin1", SensorKind::Temperature);
        assert_eq!(reading_tone(&floating, Some(109.0)), Tone::Neutral);
    }

    #[test]
    fn raw_board_inputs_are_detail_and_known_board_readings_are_not() {
        for key in
            ["board.temp.auxtin0", "board.temp.auxtin3", "board.vin0", "board.vin14", "board.pwm2"]
        {
            assert!(is_raw_board_input(key), "{key}");
        }
        for key in ["board.temp.systin", "board.temp.peci0", "board.fan2", "cpu.package.temp"] {
            assert!(!is_raw_board_input(key), "{key}");
        }
        let sensor = |id, key: &str| SensorInfo {
            id,
            device: 0,
            key: key.into(),
            label: key.into(),
            kind: SensorKind::Temperature,
            driver: true,
        };
        let sensors = SensorList {
            generation: 1,
            devices: Vec::new(),
            sensors: vec![sensor(0, "board.temp.systin"), sensor(1, "board.temp.auxtin1")],
        };
        let (main, detail) = device_sensors(&sensors, 0, &BTreeSet::new());
        assert_eq!((main[0].id, detail[0].id), (0, 1));
    }

    #[test]
    fn hidden_sensors_leave_their_device_and_are_listed_apart() {
        let sensor = |id, key: &str| SensorInfo {
            id,
            device: 0,
            key: key.into(),
            label: key.into(),
            kind: SensorKind::Temperature,
            driver: true,
        };
        let sensors = SensorList {
            generation: 1,
            devices: Vec::new(),
            sensors: vec![
                sensor(0, "board.temp.systin"),
                sensor(1, "board.temp.auxtin1"),
                sensor(2, "board.temp.auxtin2"),
            ],
        };
        // One key this PC reports, one it does not (another board's).
        let hidden = BTreeSet::from(["board.temp.auxtin1".to_owned(), "other.temp".to_owned()]);
        let (main, detail) = device_sensors(&sensors, 0, &hidden);
        let shown: Vec<u32> = main.iter().chain(&detail).map(|sensor| sensor.id).collect();
        assert_eq!(shown, [0, 2], "(device sensors): expected the hidden one left out");
        let listed: Vec<u32> = hidden_sensors(&sensors, &hidden).iter().map(|s| s.id).collect();
        assert_eq!(listed, [1], "(hidden list): expected only keys this PC reports");
    }

    #[test]
    fn driver_states_read_as_sentences() {
        assert_eq!(driver_text(&DriverState::Missing).1, Tone::Caution);
        let ready = driver_text(&DriverState::Ready { version: "2.2.0".into() });
        assert_eq!(ready, ("PawnIO 2.2.0 is ready.".to_owned(), Tone::Good));
        let failed = driver_text(&DriverState::Failed { error: "access denied".into() });
        assert_eq!(failed.0, "PawnIO is installed but can't be used. Access denied.");
    }

    fn update(mode: UpdateMode, state: UpdateState, checked_unix: Option<u64>) -> UpdateStatus {
        UpdateStatus { mode, state, checked_unix }
    }

    #[test]
    fn the_update_text_says_where_things_stand() {
        let now = 1_800_000_000;
        let text = |status: UpdateStatus| update_text(&status, now);
        assert_eq!(
            text(update(UpdateMode::Notify, UpdateState::Idle, Some(now - 3 * 3600))),
            ("SysPilot is up to date. Last looked 3 h ago.".to_owned(), Tone::Good)
        );
        assert_eq!(
            text(update(UpdateMode::Notify, UpdateState::Idle, Some(now - 5))).0,
            "SysPilot is up to date. Last looked just now."
        );
        assert_eq!(
            text(update(UpdateMode::Off, UpdateState::Idle, None)).0,
            "SysPilot is not looking for updates."
        );
        assert!(text(update(UpdateMode::Notify, UpdateState::Idle, None)).0.contains("not looked"));
        let found = UpdateState::Available { version: "0.2.1".into() };
        assert_eq!(
            text(update(UpdateMode::Notify, found, Some(now))),
            ("SysPilot 0.2.1 is available.".to_owned(), Tone::Caution)
        );
        let failed = UpdateState::Failed { reason: "github.com could not be reached.".into() };
        assert_eq!(
            text(update(UpdateMode::Notify, failed, None)),
            (
                "The last update did not work. github.com could not be reached.".to_owned(),
                Tone::Bad
            )
        );
        let unavailable = text(update(UpdateMode::Notify, UpdateState::Unavailable, None));
        assert!(unavailable.0.starts_with("This copy of SysPilot does not update itself"));
    }

    #[test]
    fn only_a_found_or_installing_version_gets_a_banner() {
        let found = || UpdateState::Available { version: "0.2.1".into() };
        assert_eq!(
            update_banner(&update(UpdateMode::Notify, found(), None)),
            Some(("SysPilot 0.2.1 is available.".to_owned(), true))
        );
        let automatic = update_banner(&update(UpdateMode::Automatic, found(), None)).unwrap();
        assert!(automatic.0.ends_with("installed when you close this window.") && automatic.1);
        let installing =
            update_banner(&update(UpdateMode::Notify, UpdateState::Installing, None)).unwrap();
        assert!(!installing.1, "(banner): expected no second offer while installing");
        for quiet in [
            UpdateState::Idle,
            UpdateState::Checking,
            UpdateState::Failed { reason: "offline".into() },
            UpdateState::Unavailable,
        ] {
            assert_eq!(update_banner(&update(UpdateMode::Notify, quiet, None)), None);
        }
    }

    #[test]
    fn every_update_mode_has_a_name_and_a_sentence() {
        let names: Vec<&str> = UpdateMode::ALL.into_iter().map(update_mode_name).collect();
        assert_eq!(
            names,
            ["Don't look for updates", "Tell me about updates", "Install updates automatically"]
        );
        assert!(UpdateMode::ALL.into_iter().all(|mode| update_mode_detail(mode).ends_with('.')));
    }

    /// A status whose only meaningful parts are the driver and fan control.
    fn status_with_fans(driver: DriverState, availability: FanAvailability) -> ServiceStatus {
        ServiceStatus {
            version: "0.0.0".into(),
            instance: "Fake".into(),
            uptime_secs: 0,
            console: false,
            driver,
            driver_url: "https://pawnio.eu/".into(),
            features: Vec::new(),
            subscribers: 0,
            sampling: true,
            os_build: 26_200,
            log_dir: String::new(),
            fan_control: true,
            fans: FanStatus { availability, board: "Fake board".into(), channels: Vec::new() },
            update: UpdateStatus::default(),
            tuning: Default::default(),
            problems: Vec::new(),
        }
    }

    fn tuning_control(key: &str, kind: TuningKind, state: Option<u8>) -> TuningControl {
        let (unit, min, max, default) = match kind {
            TuningKind::PowerLimit => (TuningUnit::Percent, -75, 75, 0),
            TuningKind::CoreVoltage | TuningKind::MemoryVoltage => {
                (TuningUnit::Millivolt, 700, 1075, 1050)
            }
            _ => (TuningUnit::Megahertz, 214, 1800, 1183),
        };
        TuningControl {
            key: key.into(),
            kind,
            state,
            unit,
            min,
            max,
            step: 1,
            default,
            current: default,
            wanted: None,
        }
    }

    fn tuning_card() -> TuningDevice {
        TuningDevice {
            key: "gpu.amd-699f.bus-3".into(),
            name: "Radeon RX 550 Series".into(),
            interface: "AMD Radeon driver (Overdrive N)".into(),
            controls: vec![
                tuning_control("power-limit", TuningKind::PowerLimit, None),
                tuning_control("core-p7-voltage", TuningKind::CoreVoltage, Some(7)),
                tuning_control("core-p7-clock", TuningKind::CoreClock, Some(7)),
                tuning_control("core-p1-clock", TuningKind::CoreClock, Some(1)),
                tuning_control("memory-p2-clock", TuningKind::MemoryClock, Some(2)),
            ],
            enabled: false,
            state: TuningState::Unmanaged,
        }
    }

    fn draft(values: &[(&str, i32)]) -> BTreeMap<String, i32> {
        values.iter().map(|(key, value)| ((*key).to_owned(), *value)).collect()
    }

    #[test]
    fn every_tuning_state_but_the_quiet_ones_has_a_badge_and_a_sentence() {
        let failed = TuningState::Failed { reason: "the driver refused the voltage".into() };
        let badge_text = |state: &TuningState| tuning_badge(state).map(|badge| badge.text);
        assert_eq!(badge_text(&TuningState::Unmanaged), None);
        assert_eq!(tuning_note(&TuningState::Unmanaged), None);
        assert_eq!(badge_text(&TuningState::Applied), Some("Applied"));
        assert_eq!(tuning_note(&TuningState::Applied), None);
        assert_eq!(badge_text(&TuningState::Different), Some("Not applied yet"));
        assert_eq!(badge_text(&TuningState::Contested), Some("Contested"));
        assert_eq!(badge_text(&TuningState::Held), Some("Held back"));
        assert_eq!(badge_text(&failed), Some("Failed"));
        assert_eq!(badge_text(&TuningState::Missing), Some("Not found"));
        assert_eq!(
            tuning_note(&failed),
            Some(("The values could not be set. The driver refused the voltage.".to_owned(), true))
        );
        // "Apply again" is offered where SysPilot stopped by itself.
        let offers = |state| tuning_note(&state).map(|(_, again)| again);
        assert_eq!(offers(TuningState::Contested), Some(true));
        assert_eq!(offers(TuningState::Held), Some(true));
        assert_eq!(offers(TuningState::Different), Some(false));
        assert_eq!(offers(TuningState::Missing), Some(false));
    }

    #[test]
    fn tuning_values_read_with_their_unit_and_a_real_minus() {
        assert_eq!(tuning_value_text(-20, TuningUnit::Percent), "\u{2212}20\u{a0}%");
        assert_eq!(tuning_value_text(1183, TuningUnit::Megahertz), "1183\u{a0}MHz");
        let mut control = tuning_control("power-limit", TuningKind::PowerLimit, None);
        assert_eq!(
            tuning_control_hint(&control, 0),
            "\u{2212}75\u{a0}% to 75\u{a0}%, default 0\u{a0}%"
        );
        assert_eq!(tuning_cell_note(&control, 0), "", "default, shown and current agree");
        control.current = 5;
        assert_eq!(
            tuning_control_hint(&control, 0),
            "\u{2212}75\u{a0}% to 75\u{a0}%, default 0\u{a0}% · now 5\u{a0}%"
        );
        assert_eq!(
            tuning_control_hint(&control, 5),
            "\u{2212}75\u{a0}% to 75\u{a0}%, default 0\u{a0}%",
            "shown and current agree"
        );
        assert_eq!(tuning_cell_note(&control, 0), "now 5\u{a0}%");
        assert_eq!(tuning_cell_note(&control, 5), "default 0\u{a0}%");
        assert_eq!(tuning_cell_note(&control, -20), "default 0\u{a0}% · now 5\u{a0}%");
    }

    #[test]
    fn a_control_shows_the_draft_then_the_saved_value_then_the_default() {
        let mut control = tuning_control("power-limit", TuningKind::PowerLimit, None);
        assert_eq!(tuning_shown_value(&control, None), 0);
        control.wanted = Some(-20);
        assert_eq!(tuning_shown_value(&control, None), -20);
        assert_eq!(tuning_shown_value(&control, Some(&draft(&[("power-limit", -30)]))), -30);
        assert_eq!(tuning_shown_value(&control, Some(&draft(&[("other", 1)]))), -20);
    }

    #[test]
    fn per_state_controls_become_table_rows_and_the_rest_stay_limits() {
        let card = tuning_card();
        let layout = tuning_layout(&card.controls);
        let keys = |controls: &[&TuningControl]| -> Vec<String> {
            controls.iter().map(|control| control.key.clone()).collect()
        };
        assert_eq!(keys(&layout.limits), ["power-limit"]);
        let states: Vec<u8> = layout.core.iter().map(|row| row.state).collect();
        assert_eq!(states, [1, 7], "lowest state first, whatever order they came in");
        let lowest = &layout.core[0];
        assert_eq!(
            (lowest.clock.map(|control| control.key.as_str()), lowest.voltage),
            (Some("core-p1-clock"), None),
            "(layout): expected state 1 with a clock and no voltage | received: {lowest:?}"
        );
        assert_eq!(
            layout.core[1].voltage.map(|control| control.key.as_str()),
            Some("core-p7-voltage")
        );
        assert_eq!(layout.memory.len(), 1);
        assert_eq!(tuning_layout(&[]), TuningLayout::default());
    }

    #[test]
    fn only_values_off_their_default_are_saved_and_clocks_and_voltages_are_the_risky_ones() {
        let mut card = tuning_card();
        assert!(tuning_values(&card, None).is_empty());
        card.controls[0].wanted = Some(-20);
        let limit_only = tuning_values(&card, None);
        assert_eq!(limit_only, draft(&[("power-limit", -20)]));
        assert!(!tuning_values_are_risky(&card, &limit_only));

        // A draft back at the default takes the value out again.
        let entered = draft(&[("power-limit", 0), ("core-p7-voltage", 1000)]);
        let values = tuning_values(&card, Some(&entered));
        assert_eq!(values, draft(&[("core-p7-voltage", 1000)]));
        assert!(tuning_values_are_risky(&card, &values));
    }

    #[test]
    fn saving_replaces_the_devices_setting_and_keeps_the_others() {
        let other = TuningSetting { device: "gpu.other".into(), enabled: true, values: draft(&[]) };
        let saved = vec![
            other.clone(),
            TuningSetting { device: "gpu.card".into(), enabled: false, values: draft(&[("a", 1)]) },
        ];
        let settings = tuning_settings_with(&saved, "gpu.card", true, draft(&[("a", 2)]));
        assert_eq!(settings.len(), 2);
        assert_eq!(settings[0], other);
        assert_eq!((settings[1].enabled, &settings[1].values), (true, &draft(&[("a", 2)])));

        let added = tuning_settings_with(&[], "gpu.card", true, draft(&[]));
        assert_eq!(added.len(), 1, "turned on with nothing listed still holds the defaults");
        let emptied = tuning_settings_with(&saved, "gpu.card", false, draft(&[]));
        assert_eq!(emptied, [other], "off and nothing listed: the setting goes");
    }

    #[test]
    fn an_entry_becomes_a_draft_only_when_it_changes_something() {
        let mut control = tuning_control("power-limit", TuningKind::PowerLimit, None);
        control.wanted = Some(-20);
        let after = |draft: Option<&BTreeMap<String, i32>>, entered| {
            tuning_draft_after(&control, draft, entered)
        };
        assert_eq!(after(None, Some(-20.0)), None, "the box echoing the saved value");
        assert_eq!(after(None, None), None, "an emptied box");
        assert_eq!(after(None, Some(f64::NAN)), None);
        assert_eq!(after(None, Some(-30.4)), Some(Some(-30)));
        assert_eq!(after(None, Some(500.0)), Some(Some(75)), "clamped to the range");
        assert_eq!(after(None, Some(1e30)), Some(Some(75)), "too large for an i32 still clamps");
        let entered = draft(&[("power-limit", -30)]);
        assert_eq!(after(Some(&entered), Some(-30.0)), None, "the box echoing the draft");
        assert_eq!(after(Some(&entered), Some(-20.0)), Some(None), "back at what is saved");
    }

    #[test]
    fn every_tuning_kind_has_a_name_and_limits_have_a_sentence() {
        let kinds = [
            TuningKind::PowerLimit,
            TuningKind::TemperatureLimit,
            TuningKind::FanTargetTemperature,
            TuningKind::FanMaxSpeed,
            TuningKind::FanMinSpeed,
            TuningKind::QuietClockFloor,
            TuningKind::CoreClock,
            TuningKind::CoreVoltage,
            TuningKind::MemoryClock,
            TuningKind::MemoryVoltage,
        ];
        for kind in kinds {
            assert!(!tuning_control_name(kind).is_empty(), "{kind:?}");
            let detail = tuning_control_detail(kind);
            assert_eq!(detail.is_empty(), tuning_kind_is_risky(kind), "{kind:?}: {detail:?}");
        }
    }

    fn ready() -> DriverState {
        DriverState::Ready { version: "2.2.0".into() }
    }

    fn read_only(reason: &str) -> FanAvailability {
        FanAvailability::ReadOnly { reason: reason.into() }
    }

    #[test]
    fn a_build_without_fan_control_says_so_before_anything_else() {
        // Even with the driver missing: installing PawnIO would not help.
        let status = status_with_fans(DriverState::Missing, FanAvailability::NotInBuild);
        let gate = fan_gate(&status).unwrap();
        assert!(
            gate.text.starts_with("This build of SysPilot cannot control fans."),
            "(gate text): expected the build to be named first | received: {}",
            gate.text
        );
        assert!(
            !gate.needs_driver,
            "(needs_driver): expected false for a build without the feature"
        );
        assert_eq!(gate.tone, Tone::Neutral);
    }

    #[test]
    fn a_missing_or_broken_driver_outranks_the_boards_own_reason() {
        let reason = read_only("PawnIO is not installed");
        let missing = fan_gate(&status_with_fans(DriverState::Missing, reason.clone())).unwrap();
        assert!(missing.needs_driver && missing.tone == Tone::Caution, "{missing:?}");
        assert!(
            missing.text.starts_with("Fan control is off. PawnIO is not installed."),
            "(gate text): expected the driver sentence | received: {}",
            missing.text
        );
        let old = DriverState::TooOld { version: "1.0".into(), required: "2.0".into() };
        let old = fan_gate(&status_with_fans(old, reason.clone())).unwrap();
        assert!(old.text.contains("PawnIO 1.0 is too old. Version 2.0 or later is needed."));
        let failed = DriverState::Failed { error: "access denied".into() };
        let failed = fan_gate(&status_with_fans(failed, reason)).unwrap();
        assert_eq!(failed.tone, Tone::Bad);
        assert!(failed.text.ends_with("can't be used. Access denied."), "{}", failed.text);
    }

    #[test]
    fn a_read_only_board_gives_the_services_reason_as_a_sentence() {
        let status =
            status_with_fans(ready(), read_only("the board's entry has not been validated"));
        let gate = fan_gate(&status).unwrap();
        assert_eq!(
            gate.text,
            "Fan control is off for this board: the board's entry has not been validated."
        );
        assert!(!gate.needs_driver);
        let already = status_with_fans(ready(), read_only("board not in the catalog."));
        assert!(fan_gate(&already).unwrap().text.ends_with("catalog."), "no doubled full stop");
        let silent = fan_gate(&status_with_fans(ready(), read_only("  "))).unwrap();
        assert_eq!(silent.text, "Fan control is off for this board.");
    }

    #[test]
    fn a_controllable_board_with_a_ready_driver_has_nothing_to_explain() {
        assert_eq!(fan_gate(&status_with_fans(ready(), FanAvailability::Controllable)), None);
    }

    #[test]
    fn the_subtitle_names_the_board_when_it_is_known() {
        let plain = fan_page_subtitle("");
        assert!(plain.starts_with("Each fan's speed"), "{plain}");
        assert_eq!(fan_page_subtitle("  "), plain, "blank is unknown");
        assert_eq!(fan_page_subtitle("Fake board"), format!("Fake board. {plain}"));
    }

    #[test]
    fn an_empty_channel_list_points_to_the_sensors_page() {
        let fans = |availability, board: &str| FanStatus {
            availability,
            board: board.into(),
            channels: Vec::new(),
        };
        let unknown = fan_channels_missing_text(&fans(read_only("x"), ""));
        assert!(unknown.contains("not in SysPilot's fan catalog"), "{unknown}");
        assert!(unknown.ends_with("on the Sensors page."), "{unknown}");
        let known = fan_channels_missing_text(&fans(read_only("x"), " Fake board "));
        assert!(known.starts_with("The catalog lists no fan channels for Fake board."), "{known}");
        // Without the feature nothing was looked up, so the catalog is not blamed.
        let unbuilt = fan_channels_missing_text(&fans(FanAvailability::NotInBuild, ""));
        assert_eq!(unbuilt, "Fan speeds are on the Sensors page.");
    }

    fn channel(phase: FanPhase) -> FanChannelStatus {
        FanChannelStatus {
            channel: "board.pwm2".into(),
            label: "Rear fan".into(),
            tach_sensor: Some("board.fan2".into()),
            min_duty_pct: 30,
            curve: vec![CurvePoint(40, 30), CurvePoint(60, 60), CurvePoint(80, 100)],
            sources: vec!["cpu.package.temp".into(), "board.temp1".into()],
            active: true,
            target_duty_pct: Some(42),
            phase,
            problem: None,
        }
    }

    #[test]
    fn every_phase_is_told_in_its_own_words() {
        let state = |phase| fan_state(&channel(phase));
        let manual = state(FanPhase::Manual);
        assert_eq!(manual.badge, Badge { text: "Following curve", tone: Tone::Good });
        assert_eq!(manual.text, "Following its curve. SysPilot is asking for 42 %.");
        let failsafe = state(FanPhase::FailSafe);
        assert_eq!(failsafe.badge.tone, Tone::Caution);
        assert_eq!(
            failsafe.text,
            "Held at full speed because a temperature sensor failed. SysPilot is asking for 42 %."
        );
        let release = state(FanPhase::ReleasePending);
        assert_eq!(release.badge.text, "Release pending");
        assert!(release.text.ends_with("SysPilot is retrying."), "{}", release.text);
        let blocked = state(FanPhase::Blocked);
        assert_eq!(blocked.badge.text, "Left alone");
        assert!(blocked.text.starts_with("Left alone: this fan was found in manual mode"));
        assert_eq!(state(FanPhase::Bios).text, "Handed back to the BIOS.");
    }

    #[test]
    fn a_channel_the_bios_holds_says_why_it_does() {
        let mut no_curve = channel(FanPhase::Bios);
        no_curve.curve.clear();
        assert_eq!(
            fan_state(&no_curve).text,
            "The BIOS controls this fan. The profile has no curve for it."
        );
        let mut not_followed = channel(FanPhase::Bios);
        not_followed.active = false;
        not_followed.target_duty_pct = None;
        assert_eq!(
            fan_state(&not_followed).text,
            "The BIOS controls this fan. Its curve is shown but not followed."
        );
        // A target is only mentioned while SysPilot holds the channel.
        let mut manual = channel(FanPhase::Manual);
        manual.target_duty_pct = None;
        assert_eq!(fan_state(&manual).text, "Following its curve.");
    }

    #[test]
    fn a_problem_is_shown_only_when_there_is_one() {
        let mut with_problem = channel(FanPhase::FailSafe);
        assert_eq!(fan_problem_text(&with_problem), None);
        with_problem.problem = Some("board.temp2 stopped answering".into());
        assert_eq!(
            fan_problem_text(&with_problem).as_deref(),
            Some("Problem: board.temp2 stopped answering.")
        );
        with_problem.problem = Some("   ".into());
        assert_eq!(fan_problem_text(&with_problem), None, "blank text is no problem");
    }

    #[test]
    fn limits_name_the_sources_and_the_lowest_duty() {
        assert_eq!(
            fan_limits_text(&channel(FanPhase::Manual)).as_deref(),
            Some("Driven by the hottest of cpu.package.temp, board.temp1. Never below 30\u{a0}%.")
        );
        let mut no_floor = channel(FanPhase::Manual);
        no_floor.min_duty_pct = 0;
        assert_eq!(
            fan_limits_text(&no_floor).as_deref(),
            Some("Driven by the hottest of cpu.package.temp, board.temp1.")
        );
        // A fan without a curve gets no line at all, even with a floor.
        let mut bare = channel(FanPhase::Bios);
        bare.sources.clear();
        assert_eq!(fan_limits_text(&bare), None, "(limits of a fan without a curve)");
    }

    /// A sensor list and trends standing in for the shared-memory samples.
    fn fake_sensors(readings: &[(&str, SensorKind, Option<f64>)]) -> (SensorList, Trends) {
        let sensors = readings
            .iter()
            .enumerate()
            .map(|(id, (key, kind, _))| SensorInfo {
                id: id as u32,
                device: 0,
                key: (*key).into(),
                label: (*key).into(),
                kind: *kind,
                driver: true,
            })
            .collect();
        let samples: Vec<Sample> = readings
            .iter()
            .enumerate()
            .map(|(id, (_, _, value))| Sample { id: id as u32, value: *value })
            .collect();
        let mut trends = Trends::default();
        trends.push(1, &samples);
        (SensorList { generation: 1, devices: Vec::new(), sensors }, trends)
    }

    #[test]
    fn a_channel_is_joined_to_its_tachometer_duty_and_hottest_source() {
        let (sensors, trends) = fake_sensors(&[
            ("board.fan2", SensorKind::Fan, Some(910.0)),
            ("board.pwm2", SensorKind::Duty, Some(42.0)),
            ("cpu.package.temp", SensorKind::Temperature, Some(55.0)),
            ("board.temp1", SensorKind::Temperature, Some(36.0)),
        ]);
        assert_eq!(
            fan_readings(&channel(FanPhase::Manual), &sensors, &trends),
            FanReadings { speed: Some(910.0), duty: Some(42.0), temperature: Some(55.0) }
        );
    }

    #[test]
    fn a_missing_failed_or_mistyped_sensor_leaves_a_gap_instead_of_a_wrong_number() {
        let (sensors, trends) = fake_sensors(&[
            // Under the tachometer's key, but a voltage: not a speed.
            ("board.fan2", SensorKind::Voltage, Some(12.0)),
            ("board.pwm2", SensorKind::Duty, None),
            ("board.temp1", SensorKind::Temperature, Some(36.0)),
        ]);
        let readings = fan_readings(&channel(FanPhase::Manual), &sensors, &trends);
        assert_eq!(
            readings,
            // The first source is absent and the second still counts.
            FanReadings { speed: None, duty: None, temperature: Some(36.0) },
            "(readings): expected gaps for a mistyped, failed and absent sensor"
        );
        let mut untied = channel(FanPhase::Manual);
        untied.tach_sensor = None;
        untied.sources.clear();
        assert_eq!(fan_readings(&untied, &sensors, &trends), FanReadings::default());
    }

    #[test]
    fn the_chart_spans_the_usual_temperatures_and_widens_for_outliers() {
        assert_eq!(curve_temperature_domain(&[]), (20.0, 100.0));
        assert_eq!(
            curve_temperature_domain(&[CurvePoint(30, 30), CurvePoint(90, 100)]),
            (20.0, 100.0)
        );
        assert_eq!(
            curve_temperature_domain(&[CurvePoint(5, 30), CurvePoint(107, 100)]),
            (0.0, 110.0)
        );
    }

    #[test]
    fn curve_points_land_inside_the_chart_with_duty_growing_upwards() {
        let shape = curve_shape(&channel(FanPhase::Manual), Some(60.0), (200.0, 100.0));
        // 20 to 100 °C over 190 px of plot (5 px inset each side).
        let expected_x =
            [5.0 + 20.0 / 80.0 * 190.0, 5.0 + 40.0 / 80.0 * 190.0, 5.0 + 60.0 / 80.0 * 190.0];
        for (index, (x, _)) in shape.points.iter().enumerate() {
            assert!(
                (x - expected_x[index]).abs() < 1e-9,
                "(point {index} x): expected {} | received: {x}",
                expected_x[index]
            );
        }
        let heights: Vec<f64> = shape.points.iter().map(|(_, y)| *y).collect();
        assert!(heights[0] > heights[1] && heights[1] > heights[2], "{heights:?}");
        assert_eq!(shape.grid, [95.0, 50.0, 5.0], "0, 50 and 100 % duty");
        assert_eq!(shape.marker, Some(5.0 + 40.0 / 80.0 * 190.0));
        assert_eq!(shape.domain, (20.0, 100.0));
    }

    #[test]
    fn the_temperature_marker_stays_on_the_chart_and_is_absent_without_a_reading() {
        let mut flat = channel(FanPhase::Manual);
        flat.curve.clear();
        let hot = curve_shape(&flat, Some(300.0), (200.0, 100.0));
        assert_eq!(hot.marker, Some(195.0), "clamped to the right edge");
        let cold = curve_shape(&flat, Some(-5.0), (200.0, 100.0));
        assert_eq!(cold.marker, Some(5.0), "clamped to the left edge");
        assert_eq!(curve_shape(&flat, None, (200.0, 100.0)).marker, None);
        assert!(hot.points.is_empty(), "no curve, no points");
    }

    #[test]
    fn a_curve_is_described_in_words_for_screen_readers() {
        assert_eq!(
            curve_description(&[CurvePoint(40, 30), CurvePoint(80, 100)]),
            "Fan curve, temperature to duty. 40 °C: 30 %; 80 °C: 100 %."
        );
        assert_eq!(curve_description(&[]), "No curve.");
    }
}
