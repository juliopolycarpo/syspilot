//! What the service did, newest first.

use std::time::{SystemTime, UNIX_EPOCH};

use windows_reactor::*;

use crate::app::{App, Message};
use crate::model;
use crate::widgets::{self, GAP_MEDIUM, GAP_SMALL, GAP_TIGHT};

/// The history page.
pub fn view(app: &App, context: &ViewContext<App>) -> View {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs());
    let mut content: Vec<KeyedView> = Vec::new();

    if !app.snapshot.elevated {
        content.push(KeyedView::new(
            "locked",
            widgets::dim(
                "The history can name programs and settings of other users, so only administrators can read it.",
            ),
        ));
    } else if app.snapshot.history.is_empty() {
        content.push(KeyedView::new("empty", widgets::dim("Nothing has happened yet.")));
    } else {
        content.push(KeyedView::new(
            "clear",
            Button::new()
                .on_click(context.message(Message::AskClearHistory))
                .horizontal_alignment(HorizontalAlignment::Left)
                .content("Clear history"),
        ));
    }

    let events = app.snapshot.history.iter().enumerate().map(|(index, event)| {
        let row = Grid::new()
            .columns([GridLength::Pixel(130.0), GridLength::Star(1.0)])
            .column_spacing(GAP_MEDIUM)
            .children((
                widgets::caption(model::ago_text(now, event.unix_secs)),
                StackPanel::new().grid_column(1).spacing(GAP_TIGHT).children((
                    StackPanel::new()
                        .orientation(Orientation::Horizontal)
                        .spacing(GAP_SMALL)
                        .children((
                            widgets::strong(model::entry_title(&app.snapshot, &event.id)),
                            widgets::badge(&model::history_badge(event.kind), app.palette),
                        )),
                    widgets::dim(&event.detail).is_text_selection_enabled(true),
                )),
            ));
        // Events have no ID of their own; time, entry and position
        // together are stable while the list only grows at the top.
        let key =
            format!("{}-{}-{}", event.unix_secs, event.id, app.snapshot.history.len() - index);
        KeyedView::new(key, widgets::card().content(row))
    });
    content.extend(events);

    widgets::page("History", "What SysPilot applied, restored and turned off on this PC.", content)
}
