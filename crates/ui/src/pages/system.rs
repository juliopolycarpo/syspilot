//! What this PC is made of.

use windows_reactor::*;

use crate::app::App;
use crate::model::{self, ViewLevel};
use crate::widgets::{self, GAP_SMALL, GAP_TIGHT};

fn facts(rows: impl IntoIterator<Item = (String, String)>) -> View {
    let rows = rows.into_iter().filter(|(_, value)| !value.is_empty()).enumerate().map(
        |(index, (label, value))| KeyedView::new(index.to_string(), widgets::fact(label, value)),
    );
    widgets::card().content(StackPanel::new().spacing(GAP_SMALL).keyed_children(rows))
}

/// The system page.
pub fn view(app: &App) -> View {
    let summary = &app.snapshot.system.summary;
    let memory = if summary.memory_bytes == 0 {
        String::new()
    } else {
        model::bytes_text(summary.memory_bytes as f64)
    };
    let overview = [
        ("Computer", summary.computer.clone()),
        ("Model", summary.model.clone()),
        ("Processor", summary.cpu.clone()),
        ("Graphics", summary.gpus.join(", ")),
        ("Memory", memory),
        ("Windows", summary.os.clone()),
        ("BIOS", summary.bios.clone()),
    ]
    .map(|(label, value)| (label.to_owned(), value));
    let known = overview.iter().any(|(_, value)| !value.is_empty());

    let mut content = vec![if known {
        KeyedView::new("summary", facts(overview))
    } else {
        KeyedView::new("summary", widgets::dim("The service reports no details about this PC."))
    }];
    if app.level() == ViewLevel::Advanced {
        for (index, section) in app.snapshot.system.sections.iter().enumerate() {
            let group = StackPanel::new()
                .spacing(GAP_TIGHT)
                .children((widgets::strong(&section.title), facts(section.rows.iter().cloned())));
            content.push(KeyedView::new(format!("section-{index}"), group));
        }
    } else if !app.snapshot.system.sections.is_empty() {
        content.push(KeyedView::new(
            "more",
            widgets::caption("The Advanced level lists every component in detail."),
        ));
    }
    widgets::page("System", "What this PC is made of.", content)
}
