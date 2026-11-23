//! Fans: what each fan is doing, the curve that drives it and whether
//! SysPilot follows that curve.
//!
//! Read-only: the page sends no request. Curves come from the profile file.

use syspilot_proto::message::{FanChannelStatus, ServiceStatus};
use syspilot_proto::sensor::SensorKind;
use windows_reactor::*;

use crate::app::{App, Message, Page};
use crate::model::{self, Tone, Trends};
use crate::widgets::{self, GAP_MEDIUM, GAP_SMALL, GAP_TIGHT, TEXT_SUBTITLE};

/// Where the fan documentation lives: the research note that says how a
/// curve is written and what must be validated before control is offered.
const DOCS_LINK: &str =
    "https://github.com/juliopolycarpo/syspilot/blob/main/docs/research/fan-control.md";

/// Width and height of a curve chart.
const CHART: (f64, f64) = (280.0, 112.0);
/// Width of the column with the duty labels left of a chart.
const DUTY_LABELS: f64 = 44.0;
/// Height of a caption, to centre an axis label on its line.
const AXIS_LABEL_HEIGHT: f64 = 16.0;

/// A caption over a figure, for one live reading.
fn figure(label: &str, value: String) -> View {
    StackPanel::new().spacing(GAP_TIGHT).min_width(96.0).children((
        widgets::caption(label),
        TextBlock::new().text(value).font_size(TEXT_SUBTITLE).font_weight(FontWeight::SEMI_BOLD),
    ))
}

/// The chart of a curve with its axes, or a line of text when the profile
/// has no curve for the channel.
fn curve(app: &App, channel: &FanChannelStatus, temperature: Option<f64>) -> View {
    if channel.curve.is_empty() {
        return widgets::dim("No curve is set for this fan.")
            .width(CHART.0 + DUTY_LABELS)
            .vertical_alignment(VerticalAlignment::Center)
            .into();
    }
    let shape = model::curve_shape(channel, temperature, CHART);
    let (low, high) = shape.domain;
    let plot = Border::new()
        .border_thickness(1.0)
        .border_brush(ThemeBrush::CardStroke)
        .automation_name(model::curve_description(&channel.curve))
        .grid_column(1)
        .content(widgets::curve_chart(&shape, CHART, app.palette.chart(), channel.active));
    // Each duty label is centred on its grid line, which the model placed.
    let duty_label = |text: &str, line: f64| {
        KeyedView::new(
            text.to_owned(),
            Border::new()
                .width(DUTY_LABELS - GAP_SMALL)
                .canvas_top(line - AXIS_LABEL_HEIGHT / 2.0)
                .content(widgets::caption(text).horizontal_alignment(HorizontalAlignment::Right)),
        )
    };
    let duty_labels =
        Canvas::new().width(DUTY_LABELS - GAP_SMALL).height(CHART.1).keyed_children([
            duty_label("100 %", shape.grid[2]),
            duty_label("50 %", shape.grid[1]),
            duty_label("0 %", shape.grid[0]),
        ]);
    let temperature_labels = Grid::new()
        .columns([GridLength::Star(1.0), GridLength::Star(1.0), GridLength::Star(1.0)])
        .grid_row(1)
        .grid_column(1)
        .children((
            widgets::caption(format!("{low:.0} °C")),
            widgets::caption(format!("{:.0} °C", (low + high) / 2.0))
                .horizontal_alignment(HorizontalAlignment::Center)
                .grid_column(1),
            widgets::caption(format!("{high:.0} °C"))
                .horizontal_alignment(HorizontalAlignment::Right)
                .grid_column(2),
        ));
    let now = temperature
        .map(|value| {
            format!(
                "Vertical line: now {}",
                model::reading_text(Some(value), SensorKind::Temperature)
            )
        })
        .unwrap_or_default();
    StackPanel::new().spacing(GAP_TIGHT).children((
        Grid::new()
            .columns([GridLength::Pixel(DUTY_LABELS - GAP_SMALL), GridLength::Auto])
            .column_spacing(GAP_SMALL)
            .rows([GridLength::Auto, GridLength::Auto])
            .children((duty_labels, plot, temperature_labels)),
        widgets::caption(now).margin(Thickness::new(DUTY_LABELS, 0.0, 0.0, 0.0)),
    ))
}

/// One fan channel as a card: name and state, live numbers, then its curve.
fn channel_card(app: &App, channel: &FanChannelStatus, trends: &Trends) -> View {
    let readings = model::fan_readings(channel, &app.snapshot.sensors, trends);
    let state = model::fan_state(channel);
    let dash = || "—".to_owned();
    let target = channel.target_duty_pct.map_or_else(dash, |duty| format!("{duty} %"));
    let inputs = match &channel.tach_sensor {
        Some(tach) => format!("{} · fan input {tach}", channel.channel),
        None => format!("{} · fan input not named", channel.channel),
    };

    let mut text: Vec<KeyedView> = vec![
        KeyedView::new(
            "title",
            StackPanel::new().orientation(Orientation::Horizontal).spacing(GAP_SMALL).children((
                widgets::strong(&channel.label),
                widgets::badge(&state.badge, app.palette),
            )),
        ),
        KeyedView::new("keys", widgets::caption(inputs)),
        KeyedView::new(
            "figures",
            StackPanel::new().orientation(Orientation::Horizontal).spacing(GAP_MEDIUM).children((
                figure("Speed", model::reading_text(readings.speed, SensorKind::Fan)),
                figure("Duty", model::reading_text(readings.duty, SensorKind::Duty)),
                figure("Target", target),
            )),
        ),
        KeyedView::new("state", widgets::body(state.text)),
    ];
    if let Some(problem) = model::fan_problem_text(channel) {
        text.push(KeyedView::new(
            "problem",
            widgets::toned(widgets::body(problem), Tone::Bad, app.palette),
        ));
    }
    text.push(KeyedView::new("limits", widgets::caption(model::fan_limits_text(channel))));

    widgets::card().content(
        Grid::new()
            .columns([GridLength::Star(1.0), GridLength::Auto])
            .column_spacing(widgets::GAP_LARGE)
            .automation_name(channel.label.clone())
            .children((
                StackPanel::new().spacing(GAP_SMALL).keyed_children(text),
                widgets::column(1).content(curve(app, channel, readings.temperature)),
            )),
    )
}

/// The explanation of why nothing is controlled, with the way out where
/// there is one.
fn gate(app: &App, status: &ServiceStatus, context: &ViewContext<App>) -> View {
    let Some(gate) = model::fan_gate(status) else { return View::empty() };
    let action: Option<View> = gate.needs_driver.then(|| {
        let link = status.driver_url.clone();
        Button::new()
            .on_click(context.callback(move |()| Message::OpenLink(link.clone())))
            .content("Get PawnIO")
            .tooltip(status.driver_url.clone())
    });
    widgets::banner(gate.tone, app.palette, gate.text, action)
}

/// The switch for following curves, always shown so that the state is
/// legible, and never usable: the app sends no fan request.
fn follow_curves(status: &ServiceStatus, context: &ViewContext<App>) -> View {
    let following = status.fans.channels.iter().any(|channel| channel.active);
    widgets::card().content(
        Grid::new()
            .columns([GridLength::Star(1.0), GridLength::Auto])
            .column_spacing(GAP_MEDIUM)
            .children((
            StackPanel::new().spacing(GAP_TIGHT).children((
                widgets::strong("Follow curves"),
                widgets::caption("Curves are set in the profile file; see the documentation."),
                widgets::caption(
                    "This switch shows what SysPilot does. It can't be changed from the app yet.",
                ),
                Button::new()
                    .style(ButtonStyle::TextLink)
                    .on_click(context.message(Message::OpenLink(DOCS_LINK.to_owned())))
                    .content("Read the documentation")
                    .tooltip(DOCS_LINK),
            )),
            // Disabled on purpose; the position shows what the service does.
            widgets::column(1).content(
                ToggleSwitch::new()
                    .is_on(following)
                    .is_enabled(false)
                    .automation_name("Follow curves")
                    .min_width(0.0)
                    .slots([
                        SlotView::new(ToggleSwitchSlot::OnContent, "On"),
                        SlotView::new(ToggleSwitchSlot::OffContent, "Off"),
                    ]),
            ),
        )),
    )
}

/// The fans page.
pub fn view(app: &App, context: &ViewContext<App>) -> View {
    let Some(status) = &app.snapshot.status else {
        return widgets::page(
            "Fans",
            &model::fan_page_subtitle(""),
            [KeyedView::new("empty", widgets::dim("The service reports nothing about fans."))],
        );
    };
    let subtitle = model::fan_page_subtitle(&status.fans.board);
    let trends = app.trends();
    let mut content = vec![
        KeyedView::new("gate", gate(app, status, context)),
        KeyedView::new("follow", follow_curves(status, context)),
    ];
    if status.fans.channels.is_empty() {
        content.push(KeyedView::new(
            "no-channels",
            StackPanel::new().spacing(GAP_SMALL).children((
                widgets::dim(model::fan_channels_missing_text(&status.fans)),
                Button::new()
                    .on_click(context.message(Message::Go(Page::Sensors)))
                    .content("Open Sensors"),
            )),
        ));
    }
    let cards = status.fans.channels.iter().map(|channel| {
        KeyedView::new(channel.channel.clone(), channel_card(app, channel, &trends))
    });
    content.extend(cards);
    widgets::page("Fans", &subtitle, content)
}
