//! Settings of the app and the service, and what this installation is.

use windows_reactor::*;

use crate::app::{App, Message};
use crate::model;
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
            "How often a row is written. Old files are removed automatically.",
            NumberBox::new()
                .minimum(1.0)
                .maximum(3600.0)
                .value(f64::from(profile.logging.interval_secs))
                .is_enabled(app.can_change())
                .automation_name("Seconds between rows")
                .min_width(120.0)
                .on_value_changed(context.callback(Message::LoggingInterval)),
        ),
        Button::new()
            .on_click(context.message(Message::OpenLogs))
            .horizontal_alignment(HorizontalAlignment::Left)
            .content("Open the log folder"),
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
    widgets::card().content(
        StackPanel::new().spacing(GAP_SMALL).children((
            StackPanel::new().spacing(GAP_SMALL).keyed_children(facts),
            Button::new()
                .style(ButtonStyle::TextLink)
                .on_click(context.message(Message::OpenLink(PROJECT_LINK.to_owned())))
                .content("Source code, documentation and issues")
                .tooltip(PROJECT_LINK),
        )),
    )
}

/// The settings page.
pub fn view(app: &App, context: &ViewContext<App>) -> View {
    let theme = Theme::ALL.iter().position(|theme| *theme == app.preferences.theme);
    let mut content = vec![
        KeyedView::new(
            "level",
            setting(
                "Level",
                "Simple shows the common, safe options. Advanced adds everything else, with the technical detail, custom actions and the history. Switch at the top of the window.",
                widgets::strong(if app.level() == model::ViewLevel::Simple {
                    "Simple"
                } else {
                    "Advanced"
                }),
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
        content.push(KeyedView::new("access-heading", widgets::heading("Hardware access")));
        content.push(KeyedView::new("access", hardware_access(app, status, context)));
    }
    content.push(KeyedView::new("about-heading", widgets::heading("About")));
    content.push(KeyedView::new("about", about(app, context)));
    widgets::page("Settings", "How SysPilot looks and what it records.", content)
}
