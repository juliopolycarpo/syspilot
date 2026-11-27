//! The tweaks of one category.

use syspilot_proto::catalog::Category;
use windows_reactor::*;

use crate::app::{App, Message};
use crate::model::{self, Tone, TweakRow, ViewLevel};
use crate::widgets::{self, GAP_LARGE, GAP_MEDIUM, GAP_SMALL, GAP_TIGHT};

/// A switch with fixed English labels (the control's own follow the
/// Windows display language, which would mix languages).
pub fn switch(name: &str, on: bool, usable: bool, on_toggled: Callback<bool>) -> View {
    ToggleSwitch::new()
        .is_on(on)
        .is_enabled(usable)
        .on_toggled(on_toggled)
        .automation_name(name)
        .min_width(0.0)
        .vertical_alignment(VerticalAlignment::Center)
        .slots([
            SlotView::new(ToggleSwitchSlot::OnContent, "On"),
            SlotView::new(ToggleSwitchSlot::OffContent, "Off"),
        ])
}

/// Title, state and summary on the left; the switch on the right.
fn summary(app: &App, row: &TweakRow, context: &ViewContext<App>) -> View {
    let mut title_line: Vec<KeyedView> = vec![KeyedView::new("title", widgets::strong(&row.title))];
    if let Some(badge) = &row.badge {
        title_line.push(KeyedView::new("badge", widgets::badge(badge, app.palette)));
    }
    let mut text: Vec<KeyedView> = vec![
        KeyedView::new(
            "title-line",
            StackPanel::new()
                .orientation(Orientation::Horizontal)
                .spacing(GAP_SMALL)
                .keyed_children(title_line),
        ),
        KeyedView::new("summary", widgets::dim(&row.summary)),
    ];
    if let Some(note) = &row.note {
        let tone = row.badge.as_ref().map_or(Tone::Neutral, |badge| badge.tone);
        text.push(KeyedView::new("note", widgets::toned(widgets::body(note), tone, app.palette)));
    }
    if row.restored > 0 && row.enabled {
        let times =
            if row.restored == 1 { "once".to_owned() } else { format!("{} times", row.restored) };
        text.push(KeyedView::new(
            "restored",
            widgets::caption(format!("Something changed it and SysPilot put it back {times}.")),
        ));
    }

    let control: View = if row.audit {
        View::empty()
    } else {
        let id = row.id.clone();
        switch(
            &row.title,
            app.switch_position(&row.id),
            row.available && app.can_change(),
            context.callback(move |on| Message::Toggle(id.clone(), on)),
        )
    };
    Grid::new()
        .columns([GridLength::Star(1.0), GridLength::Auto])
        .column_spacing(GAP_LARGE)
        .opacity(if row.available || row.audit { 1.0 } else { widgets::UNAVAILABLE })
        .children((
            StackPanel::new().spacing(GAP_TIGHT).keyed_children(text),
            widgets::column(1).content(control),
        ))
}

/// What the Advanced level adds: mechanism, risk, the exact settings with
/// their current values, and the source.
fn details(app: &App, row: &TweakRow, context: &ViewContext<App>) -> View {
    let mut lines: Vec<KeyedView> = row
        .facts
        .iter()
        .map(|(label, value)| KeyedView::new(*label, widgets::fact(*label, value)))
        .collect();
    for (index, (setting, current)) in row.settings.iter().enumerate() {
        let label = if index == 0 { "Sets" } else { "" };
        let value =
            if current.is_empty() { setting.clone() } else { format!("{setting}\nNow: {current}") };
        lines.push(KeyedView::new(format!("setting-{index}"), widgets::fact(label, value)));
    }
    lines.push(KeyedView::new("id", widgets::fact("ID", &row.id)));

    let source = row.source.clone();
    let mut actions: Vec<KeyedView> = vec![KeyedView::new(
        "source",
        widgets::link(
            "Microsoft's documentation",
            row.source.clone(),
            context.callback(move |()| Message::OpenLink(source.clone())),
        ),
    )];
    let stuck = row.badge.as_ref().is_some_and(|badge| badge.tone != Tone::Good);
    if row.enabled && stuck && app.can_change() {
        let id = row.id.clone();
        actions.push(KeyedView::new(
            "reapply",
            Button::new()
                .on_click(context.callback(move |()| Message::Reapply(id.clone())))
                .content("Apply again"),
        ));
    }
    lines.push(KeyedView::new(
        "actions",
        StackPanel::new()
            .orientation(Orientation::Horizontal)
            .spacing(GAP_SMALL)
            .keyed_children(actions),
    ));
    StackPanel::new().spacing(GAP_SMALL).keyed_children(lines)
}

fn row(app: &App, row: &TweakRow, context: &ViewContext<App>) -> KeyedView {
    let summary = summary(app, row, context);
    let view = if app.level() == ViewLevel::Advanced {
        // Named, so that a screen reader says which tweak the details are
        // of; the switch inside carries the tweak's own title.
        let id = row.id.clone();
        Expander::new()
            .is_expanded(app.details_open.contains(&row.id))
            .on_is_expanded_changed(
                context.callback(move |open| Message::ShowDetails(id.clone(), open)),
            )
            .horizontal_alignment(HorizontalAlignment::Stretch)
            .automation_name(format!("Details of {}", row.title))
            .slots([
                SlotView::new(
                    ExpanderSlot::Header,
                    Border::new().padding(Thickness::xy(0.0, GAP_MEDIUM)).content(summary),
                ),
                SlotView::new(ExpanderSlot::Content, details(app, row, context)),
            ])
    } else {
        widgets::card().content(summary)
    };
    KeyedView::new(row.id.clone(), view)
}

/// The page of one category.
pub fn view(app: &App, category: Category, context: &ViewContext<App>) -> View {
    let level = app.level();
    let rows = model::tweak_rows(&app.snapshot, category, level, &app.filter);
    let (on, all) = model::category_count(&app.snapshot, category, level);

    let mut content: Vec<KeyedView> = Vec::new();
    let unfiltered = model::tweak_rows(&app.snapshot, category, level, "").len();
    // A filter is only worth its space on a long list.
    if unfiltered > 8 {
        let mut filter_bar = vec![KeyedView::new(
            "box",
            TextBox::new()
                .text(app.filter.clone())
                .placeholder_text("Filter by name, description or ID")
                .on_text_changed(context.callback(Message::Filter))
                .automation_name("Filter")
                .width(320.0),
        )];
        if !app.filter.is_empty() {
            filter_bar.push(KeyedView::new(
                "clear",
                Button::new()
                    .on_click(context.message(Message::Filter(String::new())))
                    .content("Clear"),
            ));
            filter_bar.push(KeyedView::new(
                "count",
                widgets::caption(format!("{} of {unfiltered} shown", rows.len()))
                    .vertical_alignment(VerticalAlignment::Center),
            ));
        }
        content.push(KeyedView::new(
            "filter",
            StackPanel::new()
                .orientation(Orientation::Horizontal)
                .spacing(GAP_SMALL)
                .keyed_children(filter_bar),
        ));
    }
    if rows.is_empty() {
        let nothing = if app.filter.trim().is_empty() {
            "There is nothing here at this level.".to_owned()
        } else {
            format!("Nothing here matches \"{}\".", app.filter.trim())
        };
        content.push(KeyedView::new("empty", widgets::dim(nothing)));
    }
    let (checks, tweaks): (Vec<&TweakRow>, Vec<&TweakRow>) = rows.iter().partition(|row| row.audit);
    content.extend(tweaks.into_iter().map(|tweak| row(app, tweak, context)));
    if !checks.is_empty() {
        content.push(KeyedView::new("checks-heading", widgets::heading("Checks")));
        content.push(KeyedView::new(
            "checks-note",
            widgets::dim(
                "SysPilot only reports these. Windows or the firmware decides them, so there is nothing to switch.",
            ),
        ));
        content.extend(checks.into_iter().map(|check| row(app, check, context)));
    }

    let count = if all == 0 { String::new() } else { format!(" {on} of {all} on.") };
    let subtitle = format!("{}{count}", model::category_blurb(category));
    widgets::page(model::category_name(category), &subtitle, content)
}
