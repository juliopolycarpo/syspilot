//! Live readings, grouped by the hardware they come from.

use syspilot_proto::message::{DriverState, ServiceStatus};
use syspilot_proto::sensor::{DeviceInfo, SensorInfo};
use windows_reactor::*;

use crate::app::{App, Message};
use crate::model::{self, TREND_LEN, Trends, ViewLevel};
use crate::widgets::{self, GAP_MEDIUM, GAP_SMALL, GAP_TIGHT};

/// Width and height of a row's chart.
const CHART: (f64, f64) = (180.0, 28.0);

fn reading(app: &App, sensor: &SensorInfo, trends: &Trends) -> KeyedView {
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
        ])
        .column_spacing(GAP_MEDIUM)
        .automation_name(sensor.label.clone())
        .children((
            name,
            widgets::column(1).content(widgets::sparkline(
                &trends.series(sensor.id),
                TREND_LEN,
                CHART,
                model::chart_scale(sensor.kind),
                app.palette.chart(),
            )),
            widgets::toned(text, model::reading_tone(value, sensor.kind), app.palette),
            widgets::caption(range)
                .horizontal_alignment(HorizontalAlignment::Right)
                .vertical_alignment(VerticalAlignment::Center)
                .grid_column(3),
        ));
    KeyedView::new(sensor.key.clone(), row)
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
            "{} more readings, one per thread or core, are listed at the Advanced level.",
            sensors.len()
        ))
        .into();
    }
    let unfolded = app.unfolded.contains(&device.name);
    let rows: Vec<KeyedView> = if unfolded {
        sensors.iter().map(|sensor| reading(app, sensor, trends)).collect()
    } else {
        Vec::new()
    };
    let name = device.name.clone();
    Expander::new()
        .is_expanded(unfolded)
        .on_is_expanded_changed(context.callback(move |open| Message::Unfold(name.clone(), open)))
        .horizontal_alignment(HorizontalAlignment::Stretch)
        .automation_name(format!("Each thread and core of {}", device.name))
        .slots([
            SlotView::new(
                ExpanderSlot::Header,
                widgets::body(format!("Each thread and core ({})", sensors.len())),
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
    for (index, device) in sensors.devices.iter().enumerate() {
        let (main, per_unit) = model::device_sensors(sensors, index);
        if main.is_empty() && per_unit.is_empty() {
            continue;
        }
        let rows = main.iter().map(|sensor| reading(app, sensor, &trends));
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
