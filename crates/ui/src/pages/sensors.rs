//! Live readings, grouped by the hardware they come from.

use syspilot_proto::message::{DriverState, ServiceStatus};
use syspilot_proto::sensor::{DeviceInfo, SensorInfo};
use windows_reactor::*;

use crate::app::{App, Message};
use crate::model::{self, CHART_POINTS, Trends, ViewLevel};
use crate::widgets::{self, GAP_MEDIUM, GAP_SMALL, GAP_TIGHT};

/// Width and height of a row's chart.
const CHART: (f64, f64) = (180.0, 28.0);

/// Glyph of the button that hides a reading: an eye struck through.
const HIDE_GLYPH: &str = "\u{ED1A}";

/// The small button at the end of an Advanced row that hides the reading.
fn hide_button(sensor: &SensorInfo, context: &ViewContext<App>) -> View {
    let name = format!("Hide {}", sensor.label);
    Button::new()
        .style(ButtonStyle::Subtle)
        // Secondary to the reading, like a caption; hover still shows it.
        .opacity(widgets::DIM)
        .automation_name(name.clone())
        .on_click(context.message(Message::HideSensor(sensor.key.clone(), true)))
        .content(FontIcon::new().glyph(HIDE_GLYPH))
        .tooltip(name)
}

fn reading(
    app: &App,
    sensor: &SensorInfo,
    trends: &Trends,
    context: &ViewContext<App>,
) -> KeyedView {
    let advanced = app.level() == ViewLevel::Advanced;
    let value = trends.latest(sensor.id);
    let name: View = if advanced {
        StackPanel::new().children((widgets::body(&sensor.label), widgets::caption(&sensor.key)))
    } else {
        widgets::body(&sensor.label).vertical_alignment(VerticalAlignment::Center).into()
    };
    let range = match trends.range(sensor.id) {
        Some((low, high)) if advanced => format!(
            "{} to {}",
            model::reading_text(Some(low), sensor.kind),
            model::reading_text(Some(high), sensor.kind)
        ),
        _ => String::new(),
    };
    let text = widgets::strong(model::reading_text(value, sensor.kind))
        .horizontal_alignment(HorizontalAlignment::Right)
        .vertical_alignment(VerticalAlignment::Center)
        .grid_column(2);
    let row = Grid::new()
        .columns([
            GridLength::Star(1.0),
            GridLength::Pixel(CHART.0),
            GridLength::Pixel(96.0),
            GridLength::Pixel(if advanced { 150.0 } else { 0.0 }),
            GridLength::Auto,
        ])
        .column_spacing(GAP_MEDIUM)
        .automation_name(sensor.label.clone())
        .children((
            name,
            widgets::column(1).content(widgets::sparkline(
                &trends.chart_points(sensor.id),
                CHART_POINTS,
                CHART,
                model::chart_scale(sensor.kind),
                app.palette.chart(),
            )),
            widgets::toned(text, model::reading_tone(sensor, value), app.palette),
            widgets::caption(range)
                .horizontal_alignment(HorizontalAlignment::Right)
                .vertical_alignment(VerticalAlignment::Center)
                .grid_column(3),
            widgets::column(4).content(if advanced {
                hide_button(sensor, context)
            } else {
                View::empty()
            }),
        ));
    KeyedView::new(sensor.key.clone(), row)
}

/// The readings the user hid, each with a button to show it again. At the
/// Simple level only their number, since showing one is an Advanced action.
fn hidden_readings(app: &App, hidden: &[&SensorInfo], context: &ViewContext<App>) -> View {
    if app.level() == ViewLevel::Simple {
        return widgets::caption(format!(
            "{} hidden readings can be shown again at the Advanced level.",
            hidden.len()
        ))
        .into();
    }
    let rows = hidden.iter().map(|sensor| {
        let device = app
            .snapshot
            .sensors
            .devices
            .get(sensor.device as usize)
            .map_or(String::new(), |device| device.name.clone());
        let name = format!("Show {}", sensor.label);
        let row = Grid::new()
            .columns([GridLength::Star(1.0), GridLength::Auto])
            .column_spacing(GAP_MEDIUM)
            .children((
                StackPanel::new().children((
                    widgets::body(&sensor.label),
                    widgets::caption(format!("{device} · {}", sensor.key)),
                )),
                widgets::column(1).content(
                    Button::new()
                        .automation_name(name)
                        .on_click(context.message(Message::HideSensor(sensor.key.clone(), false)))
                        .content("Show"),
                ),
            ));
        KeyedView::new(sensor.key.clone(), row)
    });
    widgets::card().content(StackPanel::new().spacing(GAP_MEDIUM).keyed_children(rows))
}

/// The one-per-thread and one-per-core readings of a device, folded away
/// until asked for. While folded they are not built at all, so a processor
/// with many threads costs nothing each second.
fn per_unit_readings(
    app: &App,
    device: &DeviceInfo,
    sensors: &[&SensorInfo],
    trends: &Trends,
    context: &ViewContext<App>,
) -> View {
    if app.level() == ViewLevel::Simple {
        return widgets::caption(format!(
            "{} more readings are listed at the Advanced level.",
            sensors.len()
        ))
        .into();
    }
    let unfolded = app.unfolded.contains(&device.name);
    let rows: Vec<KeyedView> = if unfolded {
        sensors.iter().map(|sensor| reading(app, sensor, trends, context)).collect()
    } else {
        Vec::new()
    };
    let name = device.name.clone();
    Expander::new()
        .is_expanded(unfolded)
        .on_is_expanded_changed(context.callback(move |open| Message::Unfold(name.clone(), open)))
        .horizontal_alignment(HorizontalAlignment::Stretch)
        .automation_name(format!("More readings of {}", device.name))
        .slots([
            SlotView::new(
                ExpanderSlot::Header,
                widgets::body(format!("More readings ({})", sensors.len())),
            ),
            SlotView::new(
                ExpanderSlot::Content,
                StackPanel::new().spacing(GAP_MEDIUM).keyed_children(rows),
            ),
        ])
}

/// The driver's state and what depends on it. Unavailable features stay
/// listed, greyed out, so it is clear what installing the driver adds.
pub fn hardware_access(app: &App, status: &ServiceStatus, context: &ViewContext<App>) -> View {
    let (text, tone) = model::driver_text(&status.driver);
    let action: Option<View> = (!matches!(status.driver, DriverState::Ready { .. })).then(|| {
        let link = status.driver_url.clone();
        Button::new()
            .on_click(context.callback(move |()| Message::OpenLink(link.clone())))
            .content("Get PawnIO")
            .tooltip(status.driver_url.clone())
    });
    let features = status.features.iter().map(|feature| {
        let row = Grid::new()
            .columns([GridLength::Auto, GridLength::Star(1.0)])
            .column_spacing(GAP_MEDIUM)
            .opacity(if feature.available { 1.0 } else { widgets::UNAVAILABLE })
            .children((
                widgets::dot(
                    if feature.available { model::Tone::Good } else { model::Tone::Neutral },
                    app.palette,
                    8.0,
                ),
                StackPanel::new().grid_column(1).children((
                    widgets::body(&feature.title),
                    widgets::caption(if feature.available {
                        feature.detail.clone()
                    } else {
                        format!("Not available. {}", feature.detail)
                    }),
                )),
            ));
        KeyedView::new(feature.key.clone(), row)
    });
    StackPanel::new().spacing(GAP_SMALL).children((
        widgets::banner(tone, app.palette, text, action),
        widgets::card().content(StackPanel::new().spacing(GAP_MEDIUM).keyed_children(features)),
    ))
}

/// The sensors page.
pub fn view(app: &App, context: &ViewContext<App>) -> View {
    let sensors = &app.snapshot.sensors;
    let trends = app.trends();
    let mut content: Vec<KeyedView> = Vec::new();

    if sensors.sensors.is_empty() {
        content.push(KeyedView::new(
            "empty",
            widgets::dim("The service reports no sensors on this PC."),
        ));
    }
    let hidden_keys = &app.preferences.hidden_sensors;
    for (index, device) in sensors.devices.iter().enumerate() {
        let (main, per_unit) = model::device_sensors(sensors, index, hidden_keys);
        if main.is_empty() && per_unit.is_empty() {
            continue;
        }
        let rows = main.iter().map(|sensor| reading(app, sensor, &trends, context));
        let mut group = vec![
            KeyedView::new("name", widgets::strong(&device.name)),
            KeyedView::new(
                "main",
                widgets::card().content(StackPanel::new().spacing(GAP_MEDIUM).keyed_children(rows)),
            ),
        ];
        if !per_unit.is_empty() {
            group.push(KeyedView::new(
                "per-unit",
                per_unit_readings(app, device, &per_unit, &trends, context),
            ));
        }
        let group = StackPanel::new().spacing(GAP_TIGHT).keyed_children(group);
        content.push(KeyedView::new(format!("device-{index}"), group));
    }
    let hidden = model::hidden_sensors(sensors, hidden_keys);
    if !hidden.is_empty() {
        content.push(KeyedView::new(
            "hidden-heading",
            widgets::heading(format!("Hidden readings ({})", hidden.len())),
        ));
        content.push(KeyedView::new("hidden", hidden_readings(app, &hidden, context)));
    }
    if let Some(status) = &app.snapshot.status {
        content.push(KeyedView::new("access-heading", widgets::heading("Hardware access")));
        content.push(KeyedView::new("access", hardware_access(app, status, context)));
    }
    widgets::page(
        "Sensors",
        "What this PC's hardware reports, once a second, with the last minute as a line.",
        content,
    )
}
