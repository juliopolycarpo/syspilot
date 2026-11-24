//! The app: its state, what can happen to it, and the window around the
//! pages.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use syspilot_proto::catalog::Category;
use syspilot_proto::message::Request;
use syspilot_proto::profile::{LoggingConfig, UpdateMode};
use windows_reactor::*;

use crate::backend::{Backend, BackendHandle, Change, Failure, Listener};
use crate::form::{Choice, CustomForm, Field};
use crate::model::{self, Link, Snapshot, Tone, Trends, ViewLevel};
use crate::preferences::{self, Preferences, Theme};
use crate::widgets::{self, Palette, glyph};
use crate::{contrast, launch, pages};

/// Pause before trying to reach the service again.
const RETRY_DELAY: Duration = Duration::from_secs(2);

/// How the app was started.
#[derive(Clone, PartialEq)]
pub struct Launch {
    /// Where the data comes from.
    pub backend: BackendHandle,
    /// Where preferences are kept. `None` keeps them for this run only.
    pub preferences_file: Option<PathBuf>,
    /// Page to open on.
    pub page: Page,
    /// Level to use for this run instead of the stored one.
    pub level: Option<ViewLevel>,
    /// Colours to use for this run instead of the stored ones.
    pub theme: Option<Theme>,
    /// Arguments to start the app again with, e.g. elevated.
    pub arguments: Vec<String>,
    /// Arguments for the service executable to install and start the
    /// service this app talks to.
    pub install_arguments: Vec<String>,
}

/// A destination in the navigation pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    /// Overview.
    Home,
    /// The tweaks of one category.
    Tweaks(Category),
    /// Live sensor readings.
    Sensors,
    /// Fan speeds and curves. Advanced level only.
    Fans,
    /// Hardware and software inventory.
    System,
    /// User-defined actions. Advanced level only.
    Custom,
    /// What the service did. Advanced level only.
    History,
    /// The app's and the service's settings.
    Settings,
}

impl Page {
    /// Every page, in pane order.
    pub const ALL: [Self; 12] = [
        Self::Home,
        Self::Tweaks(Category::Performance),
        Self::Tweaks(Category::Privacy),
        Self::Tweaks(Category::Security),
        Self::Tweaks(Category::Reliability),
        Self::Tweaks(Category::Other),
        Self::Sensors,
        Self::Fans,
        Self::System,
        Self::Custom,
        Self::History,
        Self::Settings,
    ];

    /// Stable name, used as navigation tag and on the command line.
    pub fn tag(self) -> &'static str {
        match self {
            Self::Home => "home",
            Self::Tweaks(Category::Performance) => "performance",
            Self::Tweaks(Category::Privacy) => "privacy",
            Self::Tweaks(Category::Security) => "security",
            Self::Tweaks(Category::Reliability) => "reliability",
            Self::Tweaks(Category::Other) => "look-and-feel",
            Self::Sensors => "sensors",
            Self::Fans => "fans",
            Self::System => "system",
            Self::Custom => "custom-actions",
            Self::History => "history",
            Self::Settings => "settings",
        }
    }

    /// The page with this tag.
    pub fn from_tag(tag: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|page| page.tag() == tag)
    }

    /// Name in the pane.
    pub fn label(self) -> &'static str {
        match self {
            Self::Home => "Home",
            Self::Tweaks(category) => model::category_name(category),
            Self::Sensors => "Sensors",
            Self::Fans => "Fans",
            Self::System => "System",
            Self::Custom => "Custom actions",
            Self::History => "History",
            Self::Settings => "Settings",
        }
    }

    fn glyph(self) -> &'static str {
        match self {
            Self::Home => glyph::HOME,
            Self::Tweaks(Category::Performance) => glyph::PERFORMANCE,
            Self::Tweaks(Category::Privacy) => glyph::PRIVACY,
            Self::Tweaks(Category::Security) => glyph::SECURITY,
            Self::Tweaks(Category::Reliability) => glyph::RELIABILITY,
            Self::Tweaks(Category::Other) => glyph::APPEARANCE,
            Self::Sensors => glyph::SENSORS,
            Self::Fans => glyph::FANS,
            Self::System => glyph::SYSTEM,
            Self::Custom => glyph::CUSTOM,
            Self::History => glyph::HISTORY,
            Self::Settings => glyph::SETTINGS,
        }
    }

    /// Whether the page exists at `level`.
    pub fn exists_at(self, level: ViewLevel) -> bool {
        level == ViewLevel::Advanced || !matches!(self, Self::Fans | Self::Custom | Self::History)
    }

    /// Whether the page shows live sensor readings.
    fn shows_readings(self) -> bool {
        matches!(self, Self::Home | Self::Sensors | Self::Fans)
    }
}

/// A question the app is asking before it acts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dialog {
    /// Turning on a tweak that lowers security or reliability.
    Weakens {
        /// Tweak ID.
        id: String,
        /// Tweak title.
        title: String,
        /// The risk, from the catalog.
        risk: String,
    },
    /// Switching to the Advanced level for the first time.
    AdvancedLevel,
    /// Removing a custom action.
    RemoveCustom {
        /// Action ID.
        id: String,
        /// Action title.
        title: String,
    },
    /// Deleting the history.
    ClearHistory,
}

/// Everything that can happen to the app.
#[derive(Clone)]
pub enum Message {
    /// A load finished.
    Loaded(Result<Box<Snapshot>, Failure>),
    /// The listener reported something, or broke.
    Signal(Result<Signal, Failure>),
    /// Time to try the service again.
    Retry,
    /// The pane selection changed.
    Navigate(Option<String>),
    /// Go to a page.
    Go(Page),
    /// Windows switched between light and dark.
    Scheme(ColorScheme),
    /// The tweak filter text changed.
    Filter(String),
    /// A tweak switch was flipped.
    Toggle(String, bool),
    /// A custom action switch was flipped.
    ToggleCustom(String, bool),
    /// "Apply again" on an entry.
    Reapply(String),
    /// A request finished. The ID is the entry whose switch was waiting.
    Sent(Option<String>, Result<(), Failure>),
    /// The open dialog was answered.
    Answered(ContentDialogResult),
    /// The message banner was closed.
    DismissNotice,
    /// A level was picked by its name in the selector.
    PickLevel(Option<String>),
    /// A theme was picked by index.
    PickTheme(Option<usize>),
    /// "Restart as administrator".
    Elevate,
    /// "Start the service", which installs it first if needed.
    StartService,
    /// Open a web page.
    OpenLink(String),
    /// Open the folder with the sensor logs.
    OpenLogs,
    /// A text field of the custom action form changed.
    FormText(Field, String),
    /// The per-thread readings of a device were unfolded or folded.
    Unfold(String, bool),
    /// The details of a tweak, by id, were opened or closed.
    ShowDetails(String, bool),
    /// The navigation pane was opened or closed.
    PaneOpen(bool),
    /// A sensor, by key, was hidden from the Sensors page (`true`) or
    /// shown again.
    HideSensor(String, bool),
    /// A choice of the custom action form changed.
    FormChoice(Choice, Option<usize>),
    /// "Add" on the custom action form.
    AddCustom,
    /// "Remove" on a custom action.
    AskRemoveCustom(String),
    /// "Clear history".
    AskClearHistory,
    /// Sensor logging was switched.
    LoggingEnabled(bool),
    /// The logging interval was changed.
    LoggingInterval(Option<f64>),
    /// An update mode was picked by index.
    PickUpdateMode(Option<usize>),
    /// "Look now" for a new version.
    CheckForUpdate,
    /// "Update now".
    InstallUpdate,
}

/// What the listener wants from the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// Data changed: load again.
    Reload,
    /// A new reading arrived: draw again.
    Redraw,
}

/// The listener, shared with the one background task that reads it.
type ListenerSlot = Arc<Mutex<Option<Box<dyn Listener>>>>;

/// The app's state.
pub struct App {
    backend: Arc<dyn Backend>,
    preferences_file: Option<PathBuf>,
    arguments: Vec<String>,
    install_arguments: Vec<String>,
    listener: ListenerSlot,
    /// Whether the listener should ask for a redraw on every reading.
    wants_readings: Arc<AtomicBool>,
    trends: Arc<Mutex<Trends>>,
    loading: bool,
    reload_wanted: bool,
    listening: bool,
    retrying: bool,
    /// Entries whose request went out; their switch is released on the
    /// next load.
    settled: Vec<String>,

    /// Whether there is data to show.
    pub link: Link,
    /// The data.
    pub snapshot: Snapshot,
    /// State colours for the current scheme.
    pub palette: Palette,
    /// The page shown.
    pub page: Page,
    /// The user's choices about the app.
    pub preferences: Preferences,
    /// Text in the tweak filter box.
    pub filter: String,
    /// Switch positions the user chose that the service has not confirmed.
    pub pending: HashMap<String, bool>,
    /// The question being asked, if any.
    pub dialog: Option<Dialog>,
    /// A message about the last thing that went wrong (or right).
    pub notice: Option<(Tone, String)>,
    /// The custom action being entered.
    pub form: CustomForm,
    /// Devices whose per-thread and per-core readings are unfolded.
    pub unfolded: HashSet<String>,
    /// Tweaks whose details are open at the Advanced level.
    pub details_open: HashSet<String>,
    /// Whether the navigation pane is open, once the view has said so.
    pane_open: Option<bool>,
}

/// Records an expander being opened or closed. An expander must be told
/// whether it is open on every render: one left to itself is closed again
/// by the toolkit as soon as it opens.
fn set_open(open_keys: &mut HashSet<String>, key: String, open: bool) {
    if open {
        open_keys.insert(key);
    } else {
        open_keys.remove(&key);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // Everything behind these locks is plain data that stays valid if a
    // holder panicked.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Waits for the next thing worth telling the app. Readings are stored on
/// the way and reported only while a page shows them.
fn listen(
    backend: &dyn Backend,
    slot: &ListenerSlot,
    trends: &Mutex<Trends>,
    wants_readings: &AtomicBool,
) -> Result<Signal, Failure> {
    let mut slot = lock(slot);
    let listener = match &mut *slot {
        Some(listener) => listener,
        empty => empty.insert(backend.listen()?),
    };
    loop {
        match listener.next_change() {
            Ok(Change::Data) => return Ok(Signal::Reload),
            Ok(Change::Sample { generation, samples }) => {
                lock(trends).push(generation, &samples);
                if wants_readings.load(Ordering::Relaxed) {
                    return Ok(Signal::Redraw);
                }
            }
            Err(failure) => {
                *slot = None;
                return Err(failure);
            }
        }
    }
}

impl App {
    /// The level in effect.
    pub fn level(&self) -> ViewLevel {
        self.preferences.level
    }

    /// Whether the user may change things.
    pub fn can_change(&self) -> bool {
        self.link == Link::Ready && self.snapshot.elevated
    }

    /// Where a switch should stand: the user's choice while a request is
    /// under way, otherwise what the service reports.
    pub fn switch_position(&self, id: &str) -> bool {
        self.pending.get(id).copied().unwrap_or_else(|| {
            self.snapshot.states.iter().any(|status| status.id == id && status.enabled)
        })
    }

    /// A copy of the recent readings, for drawing.
    pub fn trends(&self) -> Trends {
        lock(&self.trends).clone()
    }

    fn load(&mut self, context: &ComponentContext<Self>) {
        if self.loading {
            self.reload_wanted = true;
            return;
        }
        self.loading = true;
        let backend = Arc::clone(&self.backend);
        _ = context.spawn_background(move |_| Message::Loaded(backend.load().map(Box::new)));
    }

    fn listen(&mut self, context: &ComponentContext<Self>) {
        self.listening = true;
        let backend = Arc::clone(&self.backend);
        let slot = Arc::clone(&self.listener);
        let trends = Arc::clone(&self.trends);
        let wants_readings = Arc::clone(&self.wants_readings);
        _ = context.spawn_background(move |_| {
            Message::Signal(listen(&*backend, &slot, &trends, &wants_readings))
        });
    }

    fn retry_later(&mut self, context: &ComponentContext<Self>) {
        if !self.retrying {
            self.retrying = true;
            _ = context.spawn_background(|_| {
                std::thread::sleep(RETRY_DELAY);
                Message::Retry
            });
        }
    }

    fn disconnected(&mut self, failure: Failure, context: &ComponentContext<Self>) {
        self.link = match failure {
            Failure::NotRunning => Link::NotRunning,
            Failure::Other(message) => Link::Failed(message),
        };
        self.pending.clear();
        self.dialog = None;
        self.retry_later(context);
    }

    fn send(&mut self, entry: Option<String>, request: Request, context: &ComponentContext<Self>) {
        let backend = Arc::clone(&self.backend);
        _ = context.spawn_background(move |_| Message::Sent(entry, backend.send(&request)));
    }

    fn go(&mut self, page: Page) {
        if page.exists_at(self.level()) {
            self.page = page;
            self.filter.clear();
            self.wants_readings.store(page.shows_readings(), Ordering::Relaxed);
        }
    }

    fn set_level(&mut self, level: ViewLevel) {
        self.preferences.level = level;
        if !self.page.exists_at(level) {
            self.go(Page::Home);
        }
        self.save_preferences();
    }

    fn save_preferences(&mut self) {
        let Some(file) = &self.preferences_file else { return };
        if let Err(error) = preferences::save(file, &self.preferences) {
            self.notice = Some((Tone::Caution, format!("Your choice could not be saved. {error}")));
        }
    }

    fn fail(&mut self, error: impl std::fmt::Display) {
        self.notice = Some((Tone::Bad, error.to_string()));
    }

    fn logging(&self) -> LoggingConfig {
        self.snapshot.profile.as_ref().map(|profile| profile.logging.clone()).unwrap_or_default()
    }

    fn toggle_tweak(&mut self, id: String, on: bool, context: &ComponentContext<Self>) {
        // Setting a switch from code raises the same event as a click;
        // only a position that differs from ours is the user's doing.
        if self.switch_position(&id) == on || !self.can_change() {
            return;
        }
        self.pending.insert(id.clone(), on);
        let weakening = self.snapshot.catalog.get(&id).filter(|tweak| on && tweak.weakens());
        if let Some(tweak) = weakening {
            self.dialog = Some(Dialog::Weakens {
                id,
                title: tweak.title.clone(),
                risk: tweak.risk.clone().unwrap_or_default(),
            });
        } else {
            self.send(Some(id.clone()), Request::SetTweak { id, enabled: on }, context);
        }
    }

    fn toggle_custom(&mut self, id: String, on: bool, context: &ComponentContext<Self>) {
        if self.switch_position(&id) == on || !self.can_change() {
            return;
        }
        let custom = self
            .snapshot
            .profile
            .iter()
            .flat_map(|profile| &profile.custom)
            .find(|custom| custom.id == id)
            .cloned();
        if let Some(mut custom) = custom {
            custom.enabled = on;
            self.pending.insert(id.clone(), on);
            self.send(Some(id), Request::PutCustom(custom), context);
        }
    }

    fn answer(&mut self, result: ContentDialogResult, context: &ComponentContext<Self>) {
        let Some(dialog) = self.dialog.take() else { return };
        let agreed = result == ContentDialogResult::Primary;
        match dialog {
            Dialog::Weakens { id, .. } if agreed => {
                self.send(Some(id.clone()), Request::SetTweak { id, enabled: true }, context);
            }
            // Declined: the switch goes back to what the service reports.
            Dialog::Weakens { id, .. } => _ = self.pending.remove(&id),
            Dialog::AdvancedLevel if agreed => {
                self.preferences.advanced_acknowledged = true;
                self.set_level(ViewLevel::Advanced);
            }
            Dialog::RemoveCustom { id, .. } if agreed => {
                self.send(None, Request::RemoveCustom { id }, context);
            }
            Dialog::ClearHistory if agreed => self.send(None, Request::ClearHistory, context),
            Dialog::AdvancedLevel | Dialog::RemoveCustom { .. } | Dialog::ClearHistory => {}
        }
    }

    fn add_custom(&mut self, context: &ComponentContext<Self>) {
        let states = &self.snapshot.states;
        match self.form.build(|id| states.iter().any(|status| status.id == id)) {
            Ok(custom) => {
                self.form = CustomForm { kind: self.form.kind, ..CustomForm::default() };
                self.notice = None;
                self.send(None, Request::PutCustom(custom), context);
            }
            Err(problem) => self.fail(problem),
        }
    }

    fn loaded(&mut self, result: Result<Box<Snapshot>, Failure>, context: &ComponentContext<Self>) {
        self.loading = false;
        match result {
            Ok(snapshot) => {
                self.snapshot = *snapshot;
                self.link = Link::Ready;
                for id in self.settled.drain(..) {
                    self.pending.remove(&id);
                }
                if !self.listening {
                    self.listen(context);
                }
                if std::mem::take(&mut self.reload_wanted) {
                    self.load(context);
                }
            }
            Err(failure) => {
                self.reload_wanted = false;
                self.disconnected(failure, context);
            }
        }
    }
}

impl Component for App {
    type Input = Launch;
    type Message = Message;

    fn create(launch: &Launch, context: &ComponentContext<Self>) -> Self {
        let mut preferences =
            launch.preferences_file.as_deref().map(preferences::load).unwrap_or_default();
        if let Some(level) = launch.level {
            preferences.level = level;
        }
        if let Some(theme) = launch.theme {
            preferences.theme = theme;
        }
        let mut app = Self {
            backend: Arc::clone(&launch.backend.0),
            preferences_file: launch.preferences_file.clone(),
            arguments: launch.arguments.clone(),
            install_arguments: launch.install_arguments.clone(),
            listener: ListenerSlot::default(),
            wants_readings: Arc::new(AtomicBool::new(false)),
            trends: Arc::default(),
            loading: false,
            reload_wanted: false,
            listening: false,
            retrying: false,
            settled: Vec::new(),
            link: Link::Connecting,
            snapshot: Snapshot::default(),
            palette: Palette::new(ColorScheme::Dark, contrast::high_contrast_on()),
            page: Page::Home,
            preferences,
            filter: String::new(),
            pending: HashMap::new(),
            dialog: None,
            notice: None,
            form: CustomForm::default(),
            unfolded: HashSet::new(),
            details_open: HashSet::new(),
            pane_open: None,
        };
        app.go(Page::Home);
        app.go(launch.page);
        app.load(context);
        app
    }

    fn update(&mut self, message: Message, context: &ComponentContext<Self>) {
        match message {
            Message::Loaded(result) => self.loaded(result, context),
            Message::Signal(Ok(signal)) => {
                if signal == Signal::Reload {
                    self.load(context);
                }
                self.listen(context);
            }
            Message::Signal(Err(failure)) => {
                self.listening = false;
                self.disconnected(failure, context);
            }
            Message::Retry => {
                self.retrying = false;
                self.load(context);
            }
            Message::Navigate(tag) => {
                if let Some(page) = tag.as_deref().and_then(Page::from_tag) {
                    self.go(page);
                }
            }
            Message::Go(page) => self.go(page),
            // A contrast theme comes and goes with a theme change, so it is
            // asked about again here.
            Message::Scheme(scheme) => {
                self.palette = Palette::new(scheme, contrast::high_contrast_on());
            }
            Message::Filter(text) => self.filter = text,
            Message::Toggle(id, on) => self.toggle_tweak(id, on, context),
            Message::ToggleCustom(id, on) => self.toggle_custom(id, on, context),
            Message::Reapply(id) => self.send(None, Request::Reapply { id }, context),
            Message::Sent(entry, result) => {
                match (&result, entry) {
                    // The service has it; the next load shows the outcome.
                    (Ok(()), Some(id)) => self.settled.push(id),
                    (Err(_), Some(id)) => _ = self.pending.remove(&id),
                    (_, None) => {}
                }
                if let Err(failure) = result {
                    self.fail(failure);
                }
                self.load(context);
            }
            Message::Answered(result) => self.answer(result, context),
            Message::DismissNotice => self.notice = None,
            Message::PickLevel(name) => {
                let level = match name.as_deref() {
                    Some("Advanced") => ViewLevel::Advanced,
                    Some("Simple") => ViewLevel::Simple,
                    _ => self.level(),
                };
                if level == self.level() {
                    // The selector echoing the current level.
                } else if level == ViewLevel::Advanced && !self.preferences.advanced_acknowledged {
                    self.dialog = Some(Dialog::AdvancedLevel);
                } else {
                    self.set_level(level);
                }
            }
            Message::PickTheme(index) => {
                if let Some(theme) = index.and_then(|index| Theme::ALL.get(index)) {
                    self.preferences.theme = *theme;
                    self.save_preferences();
                }
            }
            Message::Elevate => match launch::relaunch_elevated(&self.arguments) {
                Ok(()) => _ = context.window().request_close(),
                Err(error) => self.fail(format!("SysPilot was not restarted. {error}")),
            },
            Message::StartService => {
                if let Err(error) = launch::run_service_command_elevated(&self.install_arguments) {
                    self.fail(format!("The service was not started. {error}"));
                }
            }
            Message::OpenLink(link) => {
                if let Err(error) = launch::open_web_link(&link) {
                    self.fail(error);
                }
            }
            Message::OpenLogs => {
                let folder = self.snapshot.status.as_ref().map(|status| status.log_dir.clone());
                if let Err(error) = launch::open_folder(&folder.unwrap_or_default()) {
                    self.fail(error);
                }
            }
            Message::FormText(field, text) => self.form.set_text(field, text),
            Message::Unfold(device, open) => set_open(&mut self.unfolded, device, open),
            Message::ShowDetails(id, open) => set_open(&mut self.details_open, id, open),
            Message::PaneOpen(open) => self.pane_open = Some(open),
            Message::HideSensor(key, hide) => {
                let hidden = &mut self.preferences.hidden_sensors;
                let changed = if hide { hidden.insert(key) } else { hidden.remove(&key) };
                if changed {
                    self.save_preferences();
                }
            }
            Message::FormChoice(choice, index) => {
                if let Some(index) = index {
                    self.form.set_choice(choice, index);
                }
            }
            Message::AddCustom => self.add_custom(context),
            Message::AskRemoveCustom(id) => {
                let title = self
                    .snapshot
                    .profile
                    .iter()
                    .flat_map(|profile| &profile.custom)
                    .find(|custom| custom.id == id)
                    .map_or_else(|| id.clone(), |custom| custom.title.clone());
                self.dialog = Some(Dialog::RemoveCustom { id, title });
            }
            Message::AskClearHistory => self.dialog = Some(Dialog::ClearHistory),
            Message::LoggingEnabled(enabled) => {
                let mut logging = self.logging();
                if logging.enabled != enabled && self.can_change() {
                    logging.enabled = enabled;
                    self.pending.insert(pages::settings::LOGGING_SWITCH.to_owned(), enabled);
                    let entry = Some(pages::settings::LOGGING_SWITCH.to_owned());
                    self.send(entry, Request::SetLogging(logging), context);
                }
            }
            Message::LoggingInterval(seconds) => {
                let mut logging = self.logging();
                // An emptied box has no value; the setting stays as it is.
                let seconds = seconds
                    .map_or(logging.interval_secs, |value| value.round().clamp(1.0, 3600.0) as u32);
                if logging.interval_secs != seconds && self.can_change() {
                    logging.interval_secs = seconds;
                    self.send(None, Request::SetLogging(logging), context);
                }
            }
            Message::PickUpdateMode(index) => {
                let picked = index.and_then(|index| UpdateMode::ALL.get(index).copied());
                let current = self.snapshot.status.as_ref().map(|status| status.update.mode);
                // The selector echoes the current mode when it is drawn.
                if let Some(mode) = picked.filter(|mode| Some(*mode) != current)
                    && self.can_change()
                {
                    self.send(None, Request::SetUpdates(mode), context);
                }
            }
            Message::CheckForUpdate => self.send(None, Request::CheckForUpdate, context),
            Message::InstallUpdate => self.send(None, Request::InstallUpdate, context),
        }
    }

    fn view(&self, _launch: &Launch, context: &mut ViewContext<Self>) -> View {
        context.window_title(format!("SysPilot - {}", self.page.label()));
        let mut visuals = WindowVisuals::new()
            .theme(match self.preferences.theme {
                Theme::System => WindowTheme::System,
                Theme::Light => WindowTheme::Light,
                Theme::Dark => WindowTheme::Dark,
            })
            .backdrop(WindowBackdrop::Mica)
            .client_size(1180.0, 800.0);
        if let Some(icon) = launch::window_icon() {
            visuals = visuals.icon(icon);
        }
        context.window_visuals(visuals);
        context.on_color_scheme(context.callback(Message::Scheme));

        let content = match &self.link {
            Link::Connecting => pages::offline::connecting(),
            Link::NotRunning => pages::offline::not_running(self, context),
            Link::Failed(message) => pages::offline::failed(self, message),
            Link::Ready => self.page_with_banners(context),
        };

        let level = self.level();
        let menu = Page::ALL.into_iter().filter(|page| page.exists_at(level)).map(|page| {
            let item = NavigationViewItem::new()
                .tag(page.tag())
                .is_selected(page == self.page)
                .selects_on_invoked(true)
                .slots([
                    SlotView::new(NavigationViewItemSlot::Content, page.label()),
                    SlotView::new(
                        NavigationViewItemSlot::Icon,
                        FontIcon::new().glyph(page.glyph()),
                    ),
                ]);
            KeyedView::new(page.tag(), item)
        });
        let navigation = NavigationView::new()
            .pane_display_mode(NavigationViewPaneDisplayMode::Auto)
            .is_back_button_visible(NavigationViewBackButtonVisible::Collapsed)
            .is_settings_visible(false)
            .always_show_header(false)
            .on_selected_tag_changed(context.callback(Message::Navigate))
            // Left unset until the view reports it, so that it starts the way
            // the window's width suggests, then echoed (see `set_open`).
            .is_pane_open(self.pane_open)
            .on_is_pane_open_changed(context.callback(Message::PaneOpen))
            .grid_row(1)
            .slots([
                SlotView::collection(NavigationViewSlot::MenuItems, menu),
                SlotView::new(NavigationViewSlot::Content, content),
            ]);

        let level_item = |name: &str, own: ViewLevel| {
            KeyedView::new(name, SelectorBarItem::new().text(name).is_selected(level == own))
        };
        let title_bar = TitleBar::new().title("SysPilot").grid_row(0).slots([SlotView::new(
            TitleBarSlot::RightHeader,
            SelectorBar::new()
                .on_selected_text_changed(context.callback(Message::PickLevel))
                .automation_name("Level")
                .collection_slot(
                    SelectorBarSlot::Items,
                    [
                        level_item("Simple", ViewLevel::Simple),
                        level_item("Advanced", ViewLevel::Advanced),
                    ],
                ),
        )]);

        Grid::new().rows([GridLength::Auto, GridLength::Star(1.0)]).children((
            title_bar,
            navigation,
            self.dialog_view(context),
        ))
    }
}

impl App {
    /// The current page under the banners that apply everywhere.
    fn page_with_banners(&self, context: &ViewContext<Self>) -> View {
        let mut banners: Vec<KeyedView> = Vec::new();
        if !self.snapshot.elevated {
            let restart = Button::new()
                .on_click(context.message(Message::Elevate))
                .content("Restart as administrator");
            banners.push(KeyedView::new(
                "read-only",
                widgets::banner(
                    Tone::Neutral,
                    self.palette,
                    "You can look around, but changing settings needs administrator rights.",
                    Some(restart),
                ),
            ));
        }
        let update =
            self.snapshot.status.as_ref().and_then(|status| model::update_banner(&status.update));
        if let Some((text, offer)) = update {
            // Installing needs an administrator, like every other change.
            let install = (offer && self.can_change()).then(|| {
                Button::new()
                    .style(ButtonStyle::Accent)
                    .on_click(context.message(Message::InstallUpdate))
                    .content("Update now")
            });
            banners.push(KeyedView::new(
                "update",
                widgets::banner(Tone::Neutral, self.palette, text, install),
            ));
        }
        let problems = self.snapshot.status.iter().flat_map(|status| &status.problems);
        for (index, problem) in problems.enumerate() {
            banners.push(KeyedView::new(
                format!("problem-{index}"),
                widgets::banner(Tone::Caution, self.palette, problem.clone(), None),
            ));
        }
        if let Some((tone, text)) = &self.notice {
            let dismiss = Button::new()
                .style(ButtonStyle::Subtle)
                .on_click(context.message(Message::DismissNotice))
                .content("Dismiss");
            banners.push(KeyedView::new(
                "notice",
                widgets::banner(*tone, self.palette, text.clone(), Some(dismiss)),
            ));
        }

        let page = match self.page {
            Page::Home => pages::home::view(self, context),
            Page::Tweaks(category) => pages::tweaks::view(self, category, context),
            Page::Sensors => pages::sensors::view(self, context),
            Page::Fans => pages::fans::view(self, context),
            Page::System => pages::system::view(self),
            Page::Custom => pages::custom::view(self, context),
            Page::History => pages::history::view(self, context),
            Page::Settings => pages::settings::view(self, context),
        };
        let has_banners = !banners.is_empty();
        let banners = StackPanel::new()
            .spacing(widgets::GAP_SMALL)
            .margin(if has_banners {
                Thickness::new(36.0, widgets::GAP_MEDIUM, 36.0, 0.0)
            } else {
                Thickness::uniform(0.0)
            })
            .max_width(widgets::PAGE_MAX_WIDTH + 72.0)
            .horizontal_alignment(HorizontalAlignment::Stretch)
            .keyed_children(banners);
        Grid::new().rows([GridLength::Auto, GridLength::Star(1.0)]).children((
            banners,
            // Keyed by page, so each page starts scrolled to its top.
            Grid::new().grid_row(1).keyed_children([KeyedView::new(self.page.tag(), page)]),
        ))
    }

    /// The one dialog of the app; what it asks depends on [`Dialog`].
    fn dialog_view(&self, context: &ViewContext<Self>) -> View {
        let (title, text, agree) = match &self.dialog {
            Some(Dialog::Weakens { title, risk, .. }) => (
                format!("{title}?"),
                format!(
                    "This makes your PC less protected.\n\n{risk}\n\nYou can turn it off again at any time, which restores the previous setting."
                ),
                "Turn on anyway",
            ),
            Some(Dialog::AdvancedLevel) => (
                "Show advanced options?".to_owned(),
                "The Advanced level adds settings that need more care: some trade security or stability for something else, and some can't be undone exactly. Each one says what it changes, what the risk is and where Microsoft documents it.\n\nIt also adds custom actions and the history of what SysPilot did.".to_owned(),
                "Show advanced options",
            ),
            Some(Dialog::RemoveCustom { title, .. }) => (
                format!("Remove \"{title}\"?"),
                "SysPilot stops enforcing it and puts back what was there before, where that is possible.".to_owned(),
                "Remove",
            ),
            Some(Dialog::ClearHistory) => (
                "Clear the history?".to_owned(),
                "The record of what SysPilot applied, restored and turned off is cleared. Your settings stay as they are.".to_owned(),
                "Clear",
            ),
            None => (String::new(), String::new(), ""),
        };
        ContentDialog::new()
            .title(title)
            .primary_button_text(agree)
            .close_button_text("Cancel")
            .is_open(self.dialog.is_some())
            .on_closed(context.callback(Message::Answered))
            .content(widgets::body(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_page_is_found_by_its_tag() {
        for page in Page::ALL {
            assert_eq!(Page::from_tag(page.tag()), Some(page), "{page:?}");
        }
        assert_eq!(Page::from_tag("nowhere"), None);
    }

    #[test]
    fn an_expander_stays_open_until_it_is_closed() {
        let mut open = HashSet::new();
        set_open(&mut open, "privacy.advertising-id".to_owned(), true);
        set_open(&mut open, "privacy.advertising-id".to_owned(), true);
        assert!(
            open.contains("privacy.advertising-id"),
            "(details): expected open after opening | received: {open:?}"
        );
        set_open(&mut open, "privacy.advertising-id".to_owned(), false);
        assert!(
            open.is_empty(),
            "(details): expected none open after closing | received: {open:?}"
        );
    }

    #[test]
    fn simple_level_has_no_fans_custom_actions_or_history() {
        let hidden: Vec<Page> =
            Page::ALL.into_iter().filter(|page| !page.exists_at(ViewLevel::Simple)).collect();
        assert_eq!(hidden, [Page::Fans, Page::Custom, Page::History]);
        assert!(Page::ALL.into_iter().all(|page| page.exists_at(ViewLevel::Advanced)));
    }

    #[test]
    fn only_pages_with_readings_ask_for_redraws() {
        let live: Vec<Page> = Page::ALL.into_iter().filter(|page| page.shows_readings()).collect();
        assert_eq!(live, [Page::Home, Page::Sensors, Page::Fans]);
    }

    /// A listener that plays back a fixed list of changes, then fails.
    struct ScriptedListener(std::vec::IntoIter<Change>);

    impl Listener for ScriptedListener {
        fn next_change(&mut self) -> Result<Change, Failure> {
            self.0.next().ok_or(Failure::NotRunning)
        }
    }

    /// A backend whose only use is to hand out one scripted listener.
    struct ScriptedBackend(Mutex<Option<Vec<Change>>>);

    impl Backend for ScriptedBackend {
        fn load(&self) -> Result<Snapshot, Failure> {
            Ok(Snapshot::default())
        }

        fn send(&self, _request: &Request) -> Result<(), Failure> {
            Ok(())
        }

        fn listen(&self) -> Result<Box<dyn Listener>, Failure> {
            let script = lock(&self.0).take().ok_or(Failure::NotRunning)?;
            Ok(Box::new(ScriptedListener(script.into_iter())))
        }
    }

    fn reading(value: f64) -> Change {
        use syspilot_proto::sensor::Sample;
        Change::Sample { generation: 1, samples: vec![Sample { id: 0, value: Some(value) }] }
    }

    #[test]
    fn readings_are_stored_but_only_reported_to_pages_that_show_them() {
        let script = vec![reading(1.0), reading(2.0), Change::Data, reading(3.0)];
        let backend = ScriptedBackend(Mutex::new(Some(script)));
        let (slot, trends) = (ListenerSlot::default(), Mutex::new(Trends::default()));
        let wants_readings = AtomicBool::new(false);
        let next = || listen(&backend, &slot, &trends, &wants_readings);

        assert_eq!(next(), Ok(Signal::Reload), "two readings pass silently");
        assert_eq!(lock(&trends).series(0), [Some(1.0), Some(2.0)]);

        wants_readings.store(true, Ordering::Relaxed);
        assert_eq!(next(), Ok(Signal::Redraw));
        assert_eq!(lock(&trends).latest(0), Some(3.0));
    }

    #[test]
    fn broken_stream_is_dropped_so_the_next_attempt_reconnects() {
        let backend = ScriptedBackend(Mutex::new(Some(vec![])));
        let (slot, trends) = (ListenerSlot::default(), Mutex::new(Trends::default()));
        let wants_readings = AtomicBool::new(true);

        assert_eq!(listen(&backend, &slot, &trends, &wants_readings), Err(Failure::NotRunning));
        assert!(lock(&slot).is_none(), "the dead listener is not kept");
        // The scripted backend has no second listener: reconnecting fails
        // at `listen`, which proves a new one was asked for.
        assert_eq!(listen(&backend, &slot, &trends, &wants_readings), Err(Failure::NotRunning));
    }
}
