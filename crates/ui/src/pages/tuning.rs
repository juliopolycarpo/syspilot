//! Tuning: the power limits, clocks and voltages of devices whose driver
//! lets them be set, and whether SysPilot keeps the user's values in place.
//!
//! The page lists whatever devices and controls the service reports; it
//! knows no vendor. Values the user enters are drafts in the app's state
//! until "Save" sends them.

use syspilot_proto::tuning::{TuningControl, TuningDevice, TuningState, TuningStatus};
use windows_reactor::*;

use crate::app::{App, Message};
use crate::model::{self, Tone, TuningStateRow};
use crate::pages::tweaks::switch;
use crate::widgets::{self, GAP_LARGE, GAP_MEDIUM, GAP_SMALL, GAP_TIGHT};

/// Where the tuning documentation lives.
const DOCS_LINK: &str = "https://github.com/juliopolycarpo/syspilot/blob/main/docs/tuning.md";

/// Width of a box a value is entered in.
const VALUE_BOX: f64 = 150.0;
/// Width of the column that names a state in a table.
const STATE_COLUMN: f64 = 72.0;

/// The box one value is entered in. `name` is what a screen reader calls it.
fn value_box(
    app: &App,
    device: &TuningDevice,
    control: &TuningControl,
    name: String,
    context: &ViewContext<App>,
) -> NumberBox {
    let shown = model::tuning_shown_value(control, app.tuning_drafts.get(&device.key));
    let (device_key, control_key) = (device.key.clone(), control.key.clone());
    NumberBox::new()
        .minimum(f64::from(control.min))
        .maximum(f64::from(control.max))
        .value(f64::from(shown))
        .is_enabled(app.can_change())
        .automation_name(name)
        .width(VALUE_BOX)
        .on_value_changed(context.callback(move |entered| {
            Message::TuningValue(device_key.clone(), control_key.clone(), entered)
        }))
}

/// A control that stands alone: what it is on the left, its box on the right.
fn limit(
    app: &App,
    device: &TuningDevice,
    control: &TuningControl,
    context: &ViewContext<App>,
) -> View {
    let shown = model::tuning_shown_value(control, app.tuning_drafts.get(&device.key));
    let name = model::tuning_control_name(control.kind);
    let unit = control.unit.suffix();
    Grid::new()
        .columns([GridLength::Star(1.0), GridLength::Auto, GridLength::Auto])
        .column_spacing(GAP_SMALL)
        .children((
            StackPanel::new().spacing(GAP_TIGHT).children((
                widgets::body(name),
                widgets::caption(model::tuning_control_detail(control.kind)),
                widgets::caption(model::tuning_control_hint(control, shown)),
            )),
            widgets::column(1).content(value_box(
                app,
                device,
                control,
                format!("{name}, {unit}"),
                context,
            )),
            widgets::column(2).content(widgets::dim(unit).min_width(36.0)),
        ))
}

/// One cell of a state table: the box and, under it, what differs.
fn cell(
    app: &App,
    device: &TuningDevice,
    control: Option<&TuningControl>,
    name: String,
    column: i32,
    context: &ViewContext<App>,
) -> View {
    let holder = StackPanel::new().spacing(GAP_TIGHT).grid_column(column);
    let Some(control) = control else {
        // The driver offers the other value of this state only.
        return holder.keyed_children([KeyedView::new("none", widgets::dim("—"))]);
    };
    let shown = model::tuning_shown_value(control, app.tuning_drafts.get(&device.key));
    let entry = value_box(app, device, control, name, context)
        .horizontal_alignment(HorizontalAlignment::Left);
    let mut parts = vec![KeyedView::new("box", entry)];
    // No line at all when there is nothing to say, so that a table at its
    // defaults stays compact.
    let note = model::tuning_cell_note(control, shown);
    if !note.is_empty() {
        parts.push(KeyedView::new("note", widgets::caption(note)));
    }
    holder.keyed_children(parts)
}

/// A table of performance states: a clock and a voltage per state.
fn state_table(
    app: &App,
    device: &TuningDevice,
    title: &str,
    rows: &[TuningStateRow<'_>],
    context: &ViewContext<App>,
) -> View {
    let columns = || {
        Grid::new()
            .columns([
                GridLength::Pixel(STATE_COLUMN),
                GridLength::Pixel(VALUE_BOX + GAP_LARGE),
                GridLength::Pixel(VALUE_BOX + GAP_LARGE),
            ])
            .column_spacing(GAP_MEDIUM)
    };
    // A column no state has a value for is left out; a single state without
    // one gets a dash in its place.
    let has_clocks = rows.iter().any(|row| row.clock.is_some());
    let has_voltages = rows.iter().any(|row| row.voltage.is_some());
    let voltage_column = if has_clocks { 2 } else { 1 };
    let mut header = vec![KeyedView::new("state", widgets::caption("State"))];
    if has_clocks {
        header.push(KeyedView::new("clock", widgets::caption("Clock, MHz").grid_column(1)));
    }
    if has_voltages {
        let voltage = widgets::caption("Voltage, mV").grid_column(voltage_column);
        header.push(KeyedView::new("voltage", voltage));
    }
    let header = columns().keyed_children(header);
    let lines = rows.iter().map(|row| {
        let label = format!("{title} state {}", row.state);
        let state = widgets::body(format!("{}", row.state))
            .vertical_alignment(VerticalAlignment::Top)
            // Level with the text inside the boxes.
            .margin(Thickness::new(0.0, 6.0, 0.0, 0.0));
        let mut cells = vec![KeyedView::new("state", state)];
        if has_clocks {
            let name = format!("{label} clock, MHz");
            cells.push(KeyedView::new("clock", cell(app, device, row.clock, name, 1, context)));
        }
        if has_voltages {
            let name = format!("{label} voltage, mV");
            let voltage = cell(app, device, row.voltage, name, voltage_column, context);
            cells.push(KeyedView::new("voltage", voltage));
        }
        KeyedView::new(format!("state-{}", row.state), columns().keyed_children(cells))
    });
    StackPanel::new().spacing(GAP_SMALL).children((
        widgets::strong(title),
        header,
        StackPanel::new().spacing(GAP_SMALL).keyed_children(lines),
    ))
}

/// "Save", "Discard" and "Use defaults", with a line on where the entered
/// values stand.
fn actions(app: &App, device: &TuningDevice, context: &ViewContext<App>) -> View {
    let draft = app.tuning_drafts.get(&device.key);
    let entered = draft.is_some();
    let saved_values = device.controls.iter().any(|control| control.wanted.is_some());
    let off_default = !model::tuning_values(device, draft).is_empty();
    let standing = if entered {
        "The values you entered are not saved yet."
    } else if saved_values && !device.enabled {
        "Turn tuning on to use the saved values."
    } else {
        ""
    };
    let key = || device.key.clone();
    StackPanel::new().orientation(Orientation::Horizontal).spacing(GAP_SMALL).children((
        Button::new()
            .style(ButtonStyle::Accent)
            .is_enabled(entered && app.can_change())
            .on_click(context.message(Message::SaveTuning(key())))
            .content("Save"),
        Button::new()
            .is_enabled(entered)
            .on_click(context.message(Message::DiscardTuning(key())))
            .content("Discard"),
        Button::new()
            .is_enabled(off_default && app.can_change())
            .on_click(context.message(Message::DefaultTuning(key())))
            .content("Use defaults")
            .tooltip(
                "Enters the driver's default for every value. Nothing changes until you save.",
            ),
        widgets::caption(standing).vertical_alignment(VerticalAlignment::Center),
    ))
}

/// The name of a device with its state, and what its values are set through.
fn title(app: &App, device: &TuningDevice) -> View {
    let mut line: Vec<KeyedView> = vec![KeyedView::new("name", widgets::strong(&device.name))];
    if let Some(badge) = model::tuning_badge(&device.state) {
        line.push(KeyedView::new("badge", widgets::badge(&badge, app.palette)));
    }
    let name_line = StackPanel::new()
        .orientation(Orientation::Horizontal)
        .spacing(GAP_SMALL)
        .keyed_children(line);
    let mut lines = vec![KeyedView::new("name", name_line)];
    // A device that is not there is named by its key already.
    if !device.interface.is_empty() {
        let through = format!("Set through the {} · {}", device.interface, device.key);
        lines.push(KeyedView::new("through", widgets::caption(through)));
    }
    StackPanel::new().spacing(GAP_TIGHT).keyed_children(lines)
}

/// What the state of a device means, with "Apply again" where that helps.
fn note(app: &App, state: &TuningState, context: &ViewContext<App>) -> Option<View> {
    let (text, again) = model::tuning_note(state)?;
    let tone = match state {
        TuningState::Failed { .. } => Tone::Bad,
        TuningState::Missing | TuningState::Different => Tone::Neutral,
        _ => Tone::Caution,
    };
    let action: Option<View> = (again && app.can_change()).then(|| {
        Button::new().on_click(context.message(Message::ApplyTuning)).content("Apply again")
    });
    Some(widgets::banner(tone, app.palette, text, action))
}

/// One device as a card: its switch, then every value the driver offers.
fn device_card(app: &App, device: &TuningDevice, context: &ViewContext<App>) -> View {
    let mut parts: Vec<KeyedView> = Vec::new();
    let key = device.key.clone();
    let control: View = if device.state == TuningState::Missing {
        Button::new()
            .is_enabled(app.can_change())
            .on_click(context.message(Message::RemoveTuning(key)))
            .content("Remove")
    } else {
        switch(
            &format!("Tuning of {}", device.name),
            app.tuning_switch_position(device),
            app.can_change(),
            context.callback(move |on| Message::TuningEnabled(key.clone(), on)),
        )
    };
    parts.push(KeyedView::new(
        "title",
        Grid::new()
            .columns([GridLength::Star(1.0), GridLength::Auto])
            .column_spacing(GAP_LARGE)
            .children((title(app, device), widgets::column(1).content(control))),
    ));
    if let Some(note) = note(app, &device.state, context) {
        parts.push(KeyedView::new("note", note));
    }
    let layout = model::tuning_layout(&device.controls);
    if !layout.limits.is_empty() {
        let limits = layout.limits.iter().map(|control| {
            KeyedView::new(control.key.clone(), limit(app, device, control, context))
        });
        parts.push(KeyedView::new(
            "limits",
            StackPanel::new().spacing(GAP_MEDIUM).keyed_children(limits),
        ));
    }
    if !layout.core.is_empty() || !layout.memory.is_empty() {
        parts.push(KeyedView::new(
            "states-help",
            widgets::caption(
                "The device moves to higher states as it gets busier. A clock may not be lower than in the state before it.",
            ),
        ));
    }
    if !layout.core.is_empty() {
        let table = state_table(app, device, "Graphics processor", &layout.core, context);
        parts.push(KeyedView::new("core", table));
    }
    if !layout.memory.is_empty() {
        let table = state_table(app, device, "Graphics memory", &layout.memory, context);
        parts.push(KeyedView::new("memory", table));
    }
    if !device.controls.is_empty() {
        parts.push(KeyedView::new("actions", actions(app, device, context)));
    }
    widgets::card().content(
        StackPanel::new()
            .spacing(GAP_MEDIUM)
            .automation_name(device.name.clone())
            .keyed_children(parts),
    )
}

/// What the page says before the devices: what tuning is, and where to read
/// more.
fn introduction(context: &ViewContext<App>) -> View {
    StackPanel::new().spacing(GAP_TIGHT).children((
        widgets::dim(
            "SysPilot sets these values through the device's own driver and sets them again after a restart, after sleep and when something else changes them. Turning tuning off puts the driver's defaults back.",
        ),
        Button::new()
            .style(ButtonStyle::TextLink)
            .on_click(context.message(Message::OpenLink(DOCS_LINK.to_owned())))
            .content("Read the documentation")
            .tooltip(DOCS_LINK),
    ))
}

/// What the page says when the service lists no device.
fn nothing_to_tune(tuning: &TuningStatus) -> &'static str {
    if tuning.problem.is_some() {
        "The devices could not be looked at."
    } else {
        "No device in this PC has a driver interface SysPilot can tune. Today that is AMD Radeon cards of the RX 400 and RX 500 series, through the Radeon driver."
    }
}

/// The tuning page.
pub fn view(app: &App, context: &ViewContext<App>) -> View {
    let subtitle = "Power limits, clocks and voltages of devices whose driver lets them be set.";
    let Some(status) = &app.snapshot.status else {
        return widgets::page(
            "Tuning",
            subtitle,
            [KeyedView::new("empty", widgets::dim("The service reports nothing about tuning."))],
        );
    };
    let tuning = &status.tuning;
    let mut content = vec![KeyedView::new("introduction", introduction(context))];
    if let Some(problem) = &tuning.problem {
        content.push(KeyedView::new(
            "problem",
            widgets::banner(Tone::Caution, app.palette, problem.clone(), None),
        ));
    }
    if tuning.devices.is_empty() {
        content.push(KeyedView::new("no-devices", widgets::dim(nothing_to_tune(tuning))));
    }
    let cards = tuning
        .devices
        .iter()
        .map(|device| KeyedView::new(device.key.clone(), device_card(app, device, context)));
    content.extend(cards);
    widgets::page("Tuning", subtitle, content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_page_says_whether_there_is_nothing_or_nothing_could_be_read() {
        let nothing = nothing_to_tune(&TuningStatus::default());
        assert!(nothing.starts_with("No device in this PC"), "{nothing}");
        let unread = TuningStatus { devices: Vec::new(), problem: Some("no tuner".into()) };
        assert_eq!(nothing_to_tune(&unread), "The devices could not be looked at.");
    }
}
