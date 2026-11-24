//! Home: is everything in place, how is the machine doing, where to go.

use syspilot_proto::message::DriverState;
use windows_reactor::*;

use crate::app::{App, Message, Page};
use crate::model::{self, CHART_POINTS, Overview, Tone, ViewLevel};
use crate::widgets::{
    self, CARD_PADDING, GAP_MEDIUM, GAP_SMALL, GAP_TIGHT, TEXT_FIGURE, TEXT_SUBTITLE,
};

/// Width and height of a reading tile, including the gap to its neighbour.
const TILE: (f64, f64) = (236.0, 132.0);
/// Width and height of a category card, including the gap.
const CATEGORY: (f64, f64) = (322.0, 112.0);

/// The sentence at the top and the colour of its dot.
fn headline(overview: &Overview, attention: usize) -> (String, String, Tone) {
    let tweaks = match overview.enabled {
        0 => "No tweaks are turned on yet.".to_owned(),
        1 if overview.applied == 1 => "1 tweak is on and in place.".to_owned(),
        count if overview.applied == count => {
            format!("All {count} tweaks you turned on are in place.")
        }
        count => format!("{} of {count} tweaks you turned on are in place.", overview.applied),
    };
    match attention {
        0 if overview.enabled == 0 => (
            "SysPilot is ready".to_owned(),
            "Pick a category to see what you can change.".to_owned(),
            Tone::Neutral,
        ),
        0 => ("Everything is in place".to_owned(), tweaks, Tone::Good),
        1 => ("1 thing needs your attention".to_owned(), tweaks, Tone::Caution),
        count => (format!("{count} things need your attention"), tweaks, Tone::Caution),
    }
}

fn figure(column: i32, value: String, label: &str, detail: String) -> View {
    widgets::card().grid_column(column).content(StackPanel::new().spacing(GAP_TIGHT).children((
        widgets::caption(label),
        TextBlock::new().text(value).font_size(TEXT_FIGURE).font_weight(FontWeight::SEMI_BOLD),
        widgets::caption(detail),
    )))
}

fn figures(overview: &Overview) -> View {
    let checks = if overview.checks_total == 0 {
        ("—".to_owned(), "Shown at the Advanced level".to_owned())
    } else {
        (
            format!("{} of {}", overview.checks_passed, overview.checks_total),
            "Security checks passed".to_owned(),
        )
    };
    Grid::new()
        .columns([GridLength::Star(1.0), GridLength::Star(1.0), GridLength::Star(1.0)])
        .column_spacing(GAP_MEDIUM)
        .children((
            figure(
                0,
                overview.enabled.to_string(),
                "Tweaks on",
                format!("{} in place", overview.applied),
            ),
            figure(
                1,
                overview.restored.to_string(),
                "Times restored",
                "Something changed a setting and SysPilot put it back".to_owned(),
            ),
            figure(2, checks.0, "Checks", checks.1),
        ))
}

fn readings(app: &App) -> View {
    let trends = app.trends();
    let tiles = model::vitals(&app.snapshot.sensors).into_iter().map(|vital| {
        let value = trends.latest(vital.sensor.id);
        let tone = model::reading_tone(&vital.sensor, value);
        let text = TextBlock::new()
            .text(model::reading_text(value, vital.sensor.kind))
            .font_size(TEXT_SUBTITLE)
            .font_weight(FontWeight::SEMI_BOLD);
        let (first, points) = trends.numbered_chart_points(vital.sensor.id);
        let chart = widgets::sparkline(
            (first, &points),
            CHART_POINTS,
            (TILE.0 - GAP_MEDIUM - 2.0 * CARD_PADDING, 36.0),
            model::chart_scale(vital.sensor.kind),
            app.palette.chart(),
        );
        let tile = widgets::card()
            .margin(Thickness::new(0.0, 0.0, GAP_MEDIUM, GAP_MEDIUM))
            .automation_name(vital.label)
            .content(StackPanel::new().spacing(GAP_TIGHT).children((
                widgets::caption(vital.label),
                widgets::toned(text, tone, app.palette),
                chart,
            )));
        KeyedView::new(vital.sensor.key.clone(), tile)
    });
    VariableSizedWrapGrid::new()
        .orientation(Orientation::Horizontal)
        .item_width(TILE.0)
        .item_height(TILE.1)
        .keyed_children(tiles)
}

fn attention(app: &App) -> View {
    let rows = model::attention(&app.snapshot).into_iter().enumerate().map(
        |(index, (title, badge, note))| {
            let row = widgets::card().content(
                StackPanel::new().spacing(GAP_TIGHT).children((
                    StackPanel::new()
                        .orientation(Orientation::Horizontal)
                        .spacing(GAP_SMALL)
                        .children((widgets::strong(title), widgets::badge(&badge, app.palette))),
                    widgets::dim(note),
                )),
            );
            KeyedView::new(index.to_string(), row)
        },
    );
    StackPanel::new().spacing(GAP_SMALL).keyed_children(rows)
}

fn driver(app: &App, context: &ViewContext<App>) -> View {
    let Some(status) = &app.snapshot.status else { return View::empty() };
    if matches!(status.driver, DriverState::Ready { .. }) {
        return View::empty();
    }
    let (text, tone) = model::driver_text(&status.driver);
    let link = status.driver_url.clone();
    let get = Button::new()
        .on_click(context.callback(move |()| Message::OpenLink(link.clone())))
        .content("Get PawnIO");
    widgets::banner(tone, app.palette, text, Some(get))
}

fn categories(app: &App, context: &ViewContext<App>) -> View {
    let level = app.level();
    let cards = model::categories(&app.snapshot, level).into_iter().map(|category| {
        let (on, all) = model::category_count(&app.snapshot, category, level);
        let count = if all == 0 { "Checks only".to_owned() } else { format!("{on} of {all} on") };
        let page = Page::Tweaks(category);
        let card = Button::new()
            .style(ButtonStyle::Subtle)
            .width(CATEGORY.0 - GAP_MEDIUM)
            .height(CATEGORY.1 - GAP_MEDIUM)
            .margin(Thickness::new(0.0, 0.0, GAP_MEDIUM, GAP_MEDIUM))
            .horizontal_content_alignment(HorizontalAlignment::Stretch)
            .vertical_content_alignment(VerticalAlignment::Stretch)
            .automation_name(model::category_name(category))
            .on_click(context.message(Message::Go(page)))
            .content(widgets::card().content(StackPanel::new().spacing(GAP_TIGHT).children((
                widgets::strong(model::category_name(category)),
                widgets::caption(model::category_blurb(category)),
                widgets::caption(count),
            ))));
        KeyedView::new(page.tag(), card)
    });
    VariableSizedWrapGrid::new()
        .orientation(Orientation::Horizontal)
        .item_width(CATEGORY.0)
        .item_height(CATEGORY.1)
        .keyed_children(cards)
}

/// The home page.
pub fn view(app: &App, context: &ViewContext<App>) -> View {
    let overview = model::overview(&app.snapshot);
    let attention_count = model::attention(&app.snapshot).len();
    let (title, detail, tone) = headline(&overview, attention_count);
    let hero = widgets::card().content(
        Grid::new()
            .columns([GridLength::Auto, GridLength::Star(1.0)])
            .column_spacing(GAP_MEDIUM)
            .children((
                widgets::dot(tone, app.palette, 14.0),
                StackPanel::new().grid_column(1).spacing(GAP_TIGHT).children((
                    TextBlock::new()
                        .text(title)
                        .font_size(TEXT_SUBTITLE)
                        .font_weight(FontWeight::SEMI_BOLD),
                    widgets::dim(detail),
                )),
            )),
    );

    let mut content = vec![
        KeyedView::new("hero", hero),
        KeyedView::new("figures", figures(&overview)),
        KeyedView::new("driver", driver(app, context)),
    ];
    if attention_count > 0 {
        content.push(KeyedView::new("attention-heading", widgets::heading("Needs attention")));
        content.push(KeyedView::new("attention", attention(app)));
    }
    if !app.snapshot.sensors.sensors.is_empty() {
        content.push(KeyedView::new("readings-heading", widgets::heading("Right now")));
        content.push(KeyedView::new("readings", readings(app)));
    }
    let heading = if app.level() == ViewLevel::Simple { "Make Windows yours" } else { "Tweaks" };
    content.push(KeyedView::new("categories-heading", widgets::heading(heading)));
    content.push(KeyedView::new("categories", categories(app, context)));

    let computer = &app.snapshot.system.summary.computer;
    let subtitle = if computer.is_empty() {
        "This PC at a glance.".to_owned()
    } else {
        format!("{computer} at a glance.")
    };
    widgets::page("Home", &subtitle, content)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overview(enabled: usize, applied: usize) -> Overview {
        Overview { enabled, applied, ..Overview::default() }
    }

    #[test]
    fn headline_counts_what_needs_attention() {
        assert_eq!(headline(&overview(0, 0), 0).0, "SysPilot is ready");
        assert_eq!(headline(&overview(0, 0), 0).2, Tone::Neutral);

        let calm = headline(&overview(9, 9), 0);
        assert_eq!((calm.0.as_str(), calm.2), ("Everything is in place", Tone::Good));
        assert_eq!(calm.1, "All 9 tweaks you turned on are in place.");

        let one = headline(&overview(1, 1), 1);
        assert_eq!((one.0.as_str(), one.2), ("1 thing needs your attention", Tone::Caution));
        assert_eq!(one.1, "1 tweak is on and in place.");

        let several = headline(&overview(11, 9), 3);
        assert_eq!(several.0, "3 things need your attention");
        assert_eq!(several.1, "9 of 11 tweaks you turned on are in place.");
    }

    #[test]
    fn a_failed_check_alone_still_asks_for_attention() {
        let lone = headline(&overview(0, 0), 1);
        assert_eq!(lone.0, "1 thing needs your attention");
        assert_eq!(lone.1, "No tweaks are turned on yet.");
    }
}
