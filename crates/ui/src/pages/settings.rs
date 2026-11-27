//! Settings of the app and the service, and what this installation is.

use std::time::{SystemTime, UNIX_EPOCH};

use syspilot_proto::message::{ServiceStatus, UpdateState};
use syspilot_proto::profile::UpdateMode;
use windows_reactor::*;

use crate::app::{App, Message};
use crate::model::{self, ViewLevel};
use crate::pages::sensors::hardware_access;
use crate::pages::tweaks::switch;
use crate::preferences::Theme;
use crate::widgets::{self, GAP_LARGE, GAP_SMALL, GAP_TIGHT};

/// Key under which the logging switch waits for the service's answer.
pub const LOGGING_SWITCH: &str = "settings.logging";
/// Where the project lives.
const PROJECT_LINK: &str = "https://github.com/juliopolycarpo/syspilot";

/// A setting: what it is on the left, its control on the right.
fn setting(title: &str, detail: impl Into<String>, control: impl Into<View>) -> View {
    widgets::card().content(
        Grid::new()
            .columns([GridLength::Star(1.0), GridLength::Auto])
            .column_spacing(GAP_LARGE)
            .children((
                StackPanel::new()
                    .spacing(GAP_TIGHT)
                    .children((widgets::strong(title), widgets::dim(detail))),
                widgets::column(1).content(control),
            )),
    )
}

fn logging(app: &App, context: &ViewContext<App>) -> View {
    let Some(profile) = &app.snapshot.profile else {
        return widgets::card()
            .content(widgets::dim("Sensor logging can be set up by administrators."));
    };
    let folder = app.snapshot.status.as_ref().map_or("", |status| status.log_dir.as_str());
    let on = app.pending.get(LOGGING_SWITCH).copied().unwrap_or(profile.logging.enabled);
    StackPanel::new().spacing(GAP_SMALL).children((
        setting(
            "Record sensor readings",
            format!(
                "Writes the readings to CSV files in {folder}, also while this window is closed."
            ),
            switch(
                "Record sensor readings",
                on,
                app.can_change(),
                context.callback(Message::LoggingEnabled),
            ),
        ),
        setting(
            "Seconds between rows",
            "How often a row is written (1 to 3600 s). Old files are removed automatically.",
            Grid::new()
                .columns([GridLength::Auto, GridLength::Auto])
                .column_spacing(GAP_SMALL)
                .children((
                    widgets::slider_with_number_box(
                        widgets::NumericRange {
                            min: 1.0,
                            max: 3600.0,
                            value: f64::from(
                                app.logging_interval_shown(profile.logging.interval_secs),
                            ),
                        },
                        app.can_change(),
                        Some(180.0),
                        96.0,
                        (
                            "Seconds between rows slider".to_owned(),
                            "Seconds between rows".to_owned(),
                        ),
                        (
                            context.callback(|value| Message::LoggingInterval(Some(value))),
                            context.callback(Message::LoggingInterval),
                        ),
                    ),
                    widgets::dim("s")
                        .min_width(16.0)
                        .vertical_alignment(VerticalAlignment::Center)
                        .grid_column(1),
                )),
        ),
        Button::new()
            .on_click(context.message(Message::OpenLogs))
            .horizontal_alignment(HorizontalAlignment::Left)
            .content("Open the log folder"),
    ))
}

/// How SysPilot updates itself and where that stands.
fn updates(app: &App, status: &ServiceStatus, context: &ViewContext<App>) -> View {
    let update = &status.update;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs());
    let (text, tone) = model::update_text(update, now);
    let standing = widgets::toned(widgets::body(text), tone, app.palette);
    // A copy that does not update itself has nothing to choose or to start.
    if update.state == UpdateState::Unavailable {
        return widgets::card().content(standing);
    }
    let found = matches!(update.state, UpdateState::Available { .. });
    let busy = matches!(update.state, UpdateState::Checking | UpdateState::Installing);
    let button = Button::new().is_enabled(app.can_change() && !busy);
    let act = if found {
        button
            .style(ButtonStyle::Accent)
            .on_click(context.message(Message::InstallUpdate))
            .content("Update now")
    } else {
        button.on_click(context.message(Message::CheckForUpdate)).content("Look now")
    };
    let mode = UpdateMode::ALL.iter().position(|mode| *mode == update.mode);
    StackPanel::new().spacing(GAP_SMALL).children((
        setting(
            "New versions",
            model::update_mode_detail(update.mode),
            ComboBox::new()
                .items_source(UpdateMode::ALL.map(model::update_mode_name))
                .selected_index(mode)
                .is_enabled(app.can_change())
                .automation_name("New versions")
                .min_width(260.0)
                .on_selection_changed(context.callback(Message::PickUpdateMode)),
        ),
        widgets::card().content(
            Grid::new()
                .columns([GridLength::Star(1.0), GridLength::Auto])
                .column_spacing(GAP_LARGE)
                .children((
                    standing.vertical_alignment(VerticalAlignment::Center),
                    widgets::column(1).content(act),
                )),
        ),
    ))
}

fn about(app: &App, context: &ViewContext<App>) -> View {
    let mut rows = vec![("App", env!("CARGO_PKG_VERSION").to_owned())];
    if let Some(status) = &app.snapshot.status {
        rows.push(("Service", status.version.clone()));
        rows.push(("Running for", model::duration_text(status.uptime_secs)));
        rows.push(("Windows build", status.os_build.to_string()));
        if status.console || status.instance != syspilot_proto::PRODUCT_NAME {
            let mode = if status.console { "console run" } else { "service" };
            rows.push(("Instance", format!("{} ({mode})", status.instance)));
        }
    }
    rows.push(("Licence", "GPL-3.0-or-later. SysPilot comes with no warranty.".to_owned()));
    let facts =
        rows.into_iter().map(|(label, value)| KeyedView::new(label, widgets::fact(label, value)));
    widgets::card().content(StackPanel::new().spacing(GAP_SMALL).children((
        StackPanel::new().spacing(GAP_SMALL).keyed_children(facts),
        widgets::link(
            "Source code, documentation and issues",
            PROJECT_LINK,
            context.message(Message::OpenLink(PROJECT_LINK.to_owned())),
        ),
    )))
}

/// The settings page.
pub fn view(app: &App, context: &ViewContext<App>) -> View {
    let theme = Theme::ALL.iter().position(|theme| *theme == app.preferences.theme);
    let mut content = vec![
        KeyedView::new(
            "level",
            setting(
                "Advanced settings",
                "Off by default; remembers your choice. Adds technical detail, fans, tuning, custom actions and history. You can also toggle this in the title bar.",
                CheckBox::new()
                    .is_checked(app.level() == ViewLevel::Advanced)
                    .on_is_checked_changed(context.callback(Message::ShowAdvanced))
                    .automation_name("Show Advanced Settings")
                    .min_width(0.0)
                    .content("Show Advanced Settings"),
            ),
        ),
        KeyedView::new(
            "theme",
            setting(
                "Colours",
                "Light, dark, or the same as Windows.",
                ComboBox::new()
                    .items_source(Theme::ALL.map(Theme::name))
                    .selected_index(theme)
                    .automation_name("Colours")
                    .min_width(200.0)
                    .on_selection_changed(context.callback(Message::PickTheme)),
            ),
        ),
        KeyedView::new("logging-heading", widgets::heading("Sensor log")),
        KeyedView::new("logging", logging(app, context)),
    ];
    if let Some(status) = &app.snapshot.status {
        content.push(KeyedView::new("updates-heading", widgets::heading("Updates")));
        content.push(KeyedView::new("updates", updates(app, status, context)));
        content.push(KeyedView::new("access-heading", widgets::heading("Hardware access")));
        content.push(KeyedView::new("access", hardware_access(app, status, context)));
    }
    content.push(KeyedView::new("about-heading", widgets::heading("About")));
    content.push(KeyedView::new("about", about(app, context)));
    widgets::page(
        "Settings",
        "How SysPilot looks, what it records and how it stays current.",
        content,
    )
}
