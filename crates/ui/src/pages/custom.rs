//! Custom actions: the user's own enforced settings.

use syspilot_proto::profile::CustomAction;
use windows_reactor::*;

use crate::app::{App, Message};
use crate::form::{self, Choice, Field};
use crate::model::{self, Tone};
use crate::pages::tweaks::switch;
use crate::widgets::{self, GAP_LARGE, GAP_MEDIUM, GAP_SMALL, GAP_TIGHT};

fn action_row(app: &App, custom: &CustomAction, context: &ViewContext<App>) -> KeyedView {
    let status = app.snapshot.states.iter().find(|status| status.id == custom.id);
    let badge = status.and_then(|status| model::entry_badge(status, false));
    let mut text: Vec<KeyedView> = vec![KeyedView::new(
        "title-line",
        StackPanel::new().orientation(Orientation::Horizontal).spacing(GAP_SMALL).children((
            widgets::strong(&custom.title),
            badge.as_ref().map_or_else(View::empty, |badge| widgets::badge(badge, app.palette)),
        )),
    )];
    text.push(KeyedView::new(
        "what",
        widgets::dim(custom.action.describe()).is_text_selection_enabled(true),
    ));
    if let Some(note) = status.and_then(model::entry_note) {
        let tone = badge.as_ref().map_or(Tone::Neutral, |badge| badge.tone);
        text.push(KeyedView::new("note", widgets::toned(widgets::body(note), tone, app.palette)));
    }
    text.push(KeyedView::new("id", widgets::caption(&custom.id)));

    let (toggle_id, remove_id) = (custom.id.clone(), custom.id.clone());
    let controls =
        StackPanel::new().orientation(Orientation::Horizontal).spacing(GAP_MEDIUM).children((
            Button::new()
                .is_enabled(app.can_change())
                .on_click(context.callback(move |()| Message::AskRemoveCustom(remove_id.clone())))
                .content("Remove"),
            switch(
                &custom.title,
                app.switch_position(&custom.id),
                app.can_change(),
                context.callback(move |on| Message::ToggleCustom(toggle_id.clone(), on)),
            ),
        ));
    let row = Grid::new()
        .columns([GridLength::Star(1.0), GridLength::Auto])
        .column_spacing(GAP_LARGE)
        .children((
            StackPanel::new().spacing(GAP_TIGHT).keyed_children(text),
            widgets::column(1).content(controls),
        ));
    KeyedView::new(custom.id.clone(), widgets::card().content(row))
}

/// A labelled text box bound to a form field.
fn text_field(
    context: &ViewContext<App>,
    field: Field,
    label: &str,
    value: &str,
    hint: &str,
) -> KeyedView {
    let input = TextBox::new()
        .text(value)
        .placeholder_text(hint)
        .automation_name(label)
        .on_text_changed(context.callback(move |text| Message::FormText(field, text)));
    let view = StackPanel::new().spacing(GAP_TIGHT).children((widgets::caption(label), input));
    KeyedView::new(label, view)
}

/// A labelled choice box bound to a form choice.
fn choice_field(
    context: &ViewContext<App>,
    choice: Choice,
    label: &str,
    options: &[&str],
    selected: usize,
) -> KeyedView {
    let input = ComboBox::new()
        .items_source(options.iter().copied())
        .selected_index(selected)
        .automation_name(label)
        .min_width(280.0)
        .on_selection_changed(context.callback(move |index| Message::FormChoice(choice, index)));
    let view = StackPanel::new().spacing(GAP_TIGHT).children((widgets::caption(label), input));
    KeyedView::new(label, view)
}

fn add_form(app: &App, context: &ViewContext<App>) -> View {
    let form = &app.form;
    let mut fields = vec![
        // The hints form one example that is valid with the choices the
        // form starts on: a machine-wide number.
        text_field(context, Field::Title, "Name", &form.title, "Hide recommendations in Start"),
        choice_field(context, Choice::Kind, "What to enforce", &form::KINDS, form.kind),
    ];
    match form.kind {
        0 => {
            fields.push(choice_field(context, Choice::Scope, "Where", &form::SCOPES, form.scope));
            fields.push(text_field(
                context,
                Field::Key,
                "Key",
                &form.key,
                r"SOFTWARE\Policies\Microsoft\Windows\Explorer",
            ));
            fields.push(text_field(
                context,
                Field::Name,
                "Value name",
                &form.name,
                "HideRecommendedSection",
            ));
            fields.push(choice_field(
                context,
                Choice::DataType,
                "Type",
                &form::DATA_TYPES,
                form.data_type,
            ));
            if form.data_type != 3 {
                fields.push(text_field(context, Field::Data, "Data", &form.data, "1"));
            }
        }
        1 => {
            fields.push(text_field(
                context,
                Field::Service,
                "Service name (not its display name)",
                &form.service,
                "Spooler",
            ));
            fields.push(choice_field(
                context,
                Choice::Start,
                "Start type",
                &form::START_TYPES,
                form.start,
            ));
        }
        _ => {
            fields.push(text_field(
                context,
                Field::Program,
                "Program (full path)",
                &form.program,
                r"C:\Windows\System32\ipconfig.exe",
            ));
            fields.push(text_field(
                context,
                Field::Arguments,
                "Arguments",
                &form.arguments,
                "/flushdns",
            ));
            fields.push(choice_field(
                context,
                Choice::Account,
                "Run as",
                &form::ACCOUNTS,
                form.account,
            ));
            fields.push(KeyedView::new(
                "command-note",
                widgets::caption(
                    "A command runs at every start or sign-in. SysPilot can't tell whether it is still in effect and can't undo it; a profile file can add a check and an undo command.",
                ),
            ));
        }
    }
    fields.push(KeyedView::new(
        "add",
        Button::new()
            .style(ButtonStyle::Accent)
            .is_enabled(app.can_change())
            .on_click(context.message(Message::AddCustom))
            .horizontal_alignment(HorizontalAlignment::Left)
            .content("Add and turn on"),
    ));
    widgets::card().content(
        StackPanel::new()
            .spacing(GAP_MEDIUM)
            .max_width(520.0)
            .horizontal_alignment(HorizontalAlignment::Left)
            .keyed_children(fields),
    )
}

/// The custom actions page.
pub fn view(app: &App, context: &ViewContext<App>) -> View {
    let mut content: Vec<KeyedView> = Vec::new();
    match &app.snapshot.profile {
        None => content.push(KeyedView::new(
            "locked",
            widgets::dim(
                "Custom actions can contain commands that run as the system, so only administrators can read or change them.",
            ),
        )),
        Some(profile) => {
            if profile.custom.is_empty() {
                content.push(KeyedView::new("empty", widgets::dim("You have no custom actions yet.")));
            }
            content.extend(profile.custom.iter().map(|custom| action_row(app, custom, context)));
            content.push(KeyedView::new("add-heading", widgets::heading("Add a custom action")));
            content.push(KeyedView::new("add-form", add_form(app, context)));
        }
    }
    widgets::page(
        "Custom actions",
        "Settings of your own that SysPilot keeps in place the same way as its tweaks, and puts back when you remove them.",
        content,
    )
}
