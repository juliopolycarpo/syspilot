//! What the window shows while there is no data: connecting, service not
//! running, or an unexpected failure.

use windows_reactor::*;

use crate::app::{App, Message};
use crate::model::Tone;
use crate::widgets::{self, GAP_LARGE, GAP_MEDIUM, GAP_SMALL, TEXT_SUBTITLE};

/// A message in the middle of the window.
fn centred(children: impl IntoViews) -> View {
    Grid::new().children((StackPanel::new()
        .spacing(GAP_MEDIUM)
        .max_width(520.0)
        .margin(GAP_LARGE)
        .horizontal_alignment(HorizontalAlignment::Center)
        .vertical_alignment(VerticalAlignment::Center)
        .children(children),))
}

fn title(text: &str) -> TextBlock {
    TextBlock::new()
        .text(text)
        .font_size(TEXT_SUBTITLE)
        .font_weight(FontWeight::SEMI_BOLD)
        .text_wrapping(TextWrapping::Wrap)
        .automation_heading_level(AutomationHeadingLevel::Level1)
}

/// The first load is under way.
pub fn connecting() -> View {
    centred((
        ProgressRing::new()
            .is_active(true)
            .width(32.0)
            .height(32.0)
            .horizontal_alignment(HorizontalAlignment::Left)
            .automation_name("Connecting"),
        widgets::dim("Connecting to the SysPilot service…"),
    ))
}

fn notice(app: &App) -> View {
    match &app.notice {
        Some((tone, text)) => widgets::banner(*tone, app.palette, text.clone(), None),
        None => View::empty(),
    }
}

/// The service is not installed or not started.
pub fn not_running(app: &App, context: &ViewContext<App>) -> View {
    centred((
        title("The SysPilot service is not running"),
        widgets::body(
            "The service is the part of SysPilot that applies your settings and reads the sensors. This window only shows what it reports.",
        ),
        StackPanel::new().orientation(Orientation::Horizontal).spacing(GAP_SMALL).children((
            Button::new()
                .style(ButtonStyle::Accent)
                .on_click(context.message(Message::StartService))
                .content("Start the service"),
            ProgressRing::new()
                .is_active(true)
                .width(16.0)
                .height(16.0)
                .automation_name("Looking for the service"),
        )),
        widgets::caption(
            "Windows asks for administrator rights. If the service is not installed yet, it is installed first. SysPilot keeps looking for the service and continues by itself once it answers. You can also run \"syspilot-svc install\" in an administrator terminal.",
        ),
        notice(app),
    ))
}

/// Something unexpected stands between the app and the service.
pub fn failed(app: &App, message: &str) -> View {
    centred((
        title("SysPilot can't talk to its service"),
        widgets::banner(Tone::Bad, app.palette, message.to_owned(), None),
        widgets::caption("SysPilot tries again every few seconds."),
        notice(app),
    ))
}
