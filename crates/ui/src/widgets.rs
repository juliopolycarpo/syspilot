//! Design tokens and the building blocks every page is made of.
//!
//! Pages do not pick sizes or colours themselves. They compose these
//! pieces, which keeps spacing, type and state colours the same everywhere.

use windows_reactor::*;

use crate::model::{Badge, Tone};

/// Space between tightly related things, e.g. a title and its caption.
pub const GAP_TIGHT: f64 = 4.0;
/// Space between the parts of one component.
pub const GAP_SMALL: f64 = 8.0;
/// Space between components of one group.
pub const GAP_MEDIUM: f64 = 12.0;
/// Space between groups.
pub const GAP_LARGE: f64 = 24.0;
/// Padding inside a card.
pub const CARD_PADDING: f64 = 16.0;
/// Corner radius of cards.
pub const CARD_RADIUS: f64 = 8.0;
/// Widest a page's content gets; beyond this, lines become hard to read.
pub const PAGE_MAX_WIDTH: f64 = 1000.0;

/// Size of captions and badges.
pub const TEXT_CAPTION: f64 = 12.0;
/// Size of running text.
pub const TEXT_BODY: f64 = 14.0;
/// Size of section headings.
pub const TEXT_SUBTITLE: f64 = 20.0;
/// Size of page titles.
pub const TEXT_TITLE: f64 = 28.0;
/// Size of headline figures.
pub const TEXT_FIGURE: f64 = 32.0;

/// Opacity of secondary text.
pub const DIM: f64 = 0.7;
/// Opacity of things that are unavailable.
pub const UNAVAILABLE: f64 = 0.45;

/// Segoe Fluent Icons code points used by the app.
pub mod glyph {
    /// Home.
    pub const HOME: &str = "\u{E80F}";
    /// A speedometer.
    pub const PERFORMANCE: &str = "\u{EC4A}";
    /// A padlock.
    pub const PRIVACY: &str = "\u{E72E}";
    /// A shield.
    pub const SECURITY: &str = "\u{EA18}";
    /// A heart with a pulse line.
    pub const RELIABILITY: &str = "\u{E95E}";
    /// A paint palette.
    pub const APPEARANCE: &str = "\u{E790}";
    /// A pulse line.
    pub const SENSORS: &str = "\u{E9D9}";
    /// A computer.
    pub const SYSTEM: &str = "\u{E7F8}";
    /// Angle brackets.
    pub const CUSTOM: &str = "\u{E943}";
    /// A clock with an arrow.
    pub const HISTORY: &str = "\u{E81C}";
    /// A cog.
    pub const SETTINGS: &str = "\u{E713}";
}

/// State colours for the current light or dark scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    dark: bool,
}

impl Palette {
    /// The palette for `scheme`.
    pub fn new(scheme: ColorScheme) -> Self {
        Self { dark: scheme == ColorScheme::Dark }
    }

    /// Text and icon colour for `tone`: Windows' system state colours.
    /// `None` for neutral, which uses the normal text colour.
    pub fn ink(self, tone: Tone) -> Option<Color> {
        let (dark, light) = match tone {
            Tone::Neutral => return None,
            Tone::Good => ((0x6C, 0xCB, 0x5F), (0x0F, 0x7B, 0x0F)),
            Tone::Caution => ((0xFC, 0xE1, 0x00), (0x9D, 0x5D, 0x00)),
            Tone::Bad => ((0xFF, 0x99, 0xA4), (0xC4, 0x2B, 0x1C)),
        };
        let (red, green, blue) = if self.dark { dark } else { light };
        Some(Color::rgb(red, green, blue))
    }

    /// A faint fill behind text of `tone`.
    pub fn wash(self, tone: Tone) -> Color {
        let (red, green, blue) = match (tone, self.dark) {
            (Tone::Neutral, true) => (0xFF, 0xFF, 0xFF),
            (Tone::Neutral, false) => (0x00, 0x00, 0x00),
            (Tone::Good, true) => (0x6C, 0xCB, 0x5F),
            (Tone::Good, false) => (0x0F, 0x7B, 0x0F),
            (Tone::Caution, true) => (0xFC, 0xE1, 0x00),
            (Tone::Caution, false) => (0x9D, 0x5D, 0x00),
            (Tone::Bad, true) => (0xFF, 0x99, 0xA4),
            (Tone::Bad, false) => (0xC4, 0x2B, 0x1C),
        };
        Color::argb(if tone == Tone::Neutral { 0x14 } else { 0x26 }, red, green, blue)
    }

    /// The line colour of charts.
    pub fn chart(self) -> Color {
        if self.dark { Color::rgb(0x60, 0xCD, 0xFF) } else { Color::rgb(0x00, 0x5F, 0xB8) }
    }
}

/// Running text that wraps.
pub fn body(text: impl Into<String>) -> TextBlock {
    TextBlock::new().text(text).font_size(TEXT_BODY).text_wrapping(TextWrapping::Wrap)
}

/// Emphasised running text, e.g. the title of a row.
pub fn strong(text: impl Into<String>) -> TextBlock {
    body(text).font_weight(FontWeight::SEMI_BOLD)
}

/// Secondary text that wraps.
pub fn dim(text: impl Into<String>) -> TextBlock {
    body(text).opacity(DIM)
}

/// Small secondary text: labels and units.
pub fn caption(text: impl Into<String>) -> TextBlock {
    TextBlock::new()
        .text(text)
        .font_size(TEXT_CAPTION)
        .text_wrapping(TextWrapping::Wrap)
        .opacity(DIM)
}

/// A section heading inside a page.
pub fn heading(text: impl Into<String>) -> TextBlock {
    TextBlock::new()
        .text(text)
        .font_size(TEXT_SUBTITLE)
        .font_weight(FontWeight::SEMI_BOLD)
        .automation_heading_level(AutomationHeadingLevel::Level2)
        .margin(Thickness::new(0.0, GAP_MEDIUM, 0.0, 0.0))
}

/// Text in the colour of `tone`.
pub fn toned(text: TextBlock, tone: Tone, palette: Palette) -> TextBlock {
    match palette.ink(tone) {
        Some(colour) => text.foreground(colour),
        None => text,
    }
}

/// A filled circle in the colour of `tone`: the app's state indicator.
/// Icons cannot be coloured, a shape can.
pub fn dot(tone: Tone, palette: Palette, size: f64) -> Ellipse {
    let colour = palette.ink(tone).unwrap_or(Color::rgb(0x8A, 0x8A, 0x8A));
    Ellipse::new()
        .width(size)
        .height(size)
        .fill(colour)
        .vertical_alignment(VerticalAlignment::Center)
}

/// The surface most content sits on. Finish it with `.content(…)`.
pub fn card() -> Border {
    Border::new()
        .padding(CARD_PADDING)
        .corner_radius(CARD_RADIUS)
        .border_thickness(1.0)
        .background(ThemeBrush::CardBackground)
        .border_brush(ThemeBrush::CardStroke)
}

/// An invisible holder that places a finished view in a grid column.
/// Layout properties belong to builders; a finished view has none.
pub fn column(index: i32) -> Border {
    Border::new().grid_column(index).vertical_alignment(VerticalAlignment::Center)
}

/// A short state label.
pub fn badge(badge: &Badge, palette: Palette) -> View {
    let text = TextBlock::new()
        .text(badge.text)
        .font_size(TEXT_CAPTION)
        .font_weight(FontWeight::SEMI_BOLD);
    Border::new()
        .padding(Thickness::new(GAP_SMALL, 2.0, GAP_SMALL, 3.0))
        .corner_radius(4.0)
        .background(palette.wash(badge.tone))
        .horizontal_alignment(HorizontalAlignment::Left)
        .vertical_alignment(VerticalAlignment::Center)
        .content(toned(text, badge.tone, palette))
}

/// A message across the page with an optional action on the right.
pub fn banner(tone: Tone, palette: Palette, text: impl Into<String>, action: Option<View>) -> View {
    Border::new()
        .padding(Thickness::new(CARD_PADDING, GAP_MEDIUM, CARD_PADDING, GAP_MEDIUM))
        .corner_radius(CARD_RADIUS)
        .border_thickness(1.0)
        .border_brush(ThemeBrush::CardStroke)
        .background(palette.wash(tone))
        .content(
            Grid::new()
                .columns([GridLength::Auto, GridLength::Star(1.0), GridLength::Auto])
                .column_spacing(GAP_MEDIUM)
                .children((
                    dot(tone, palette, 10.0),
                    body(text).vertical_alignment(VerticalAlignment::Center).grid_column(1),
                    column(2).content(action.unwrap_or_else(View::empty)),
                )),
        )
}

/// A label with its value on the same line, for fact tables.
pub fn fact(label: impl Into<String>, value: impl Into<String>) -> View {
    Grid::new()
        .columns([GridLength::Pixel(180.0), GridLength::Star(1.0)])
        .column_spacing(GAP_MEDIUM)
        .children((dim(label), body(value).is_text_selection_enabled(true).grid_column(1)))
}

/// A scrolling page: title, one line about it, then its content.
pub fn page(title: &str, subtitle: &str, content: impl IntoIterator<Item = KeyedView>) -> View {
    let title = TextBlock::new()
        .text(title)
        .font_size(TEXT_TITLE)
        .font_weight(FontWeight::SEMI_BOLD)
        .automation_heading_level(AutomationHeadingLevel::Level1);
    let header = StackPanel::new().spacing(GAP_TIGHT).children((title, dim(subtitle)));
    let children = std::iter::once(KeyedView::new("page-header", header)).chain(content);
    ScrollViewer::new().content(
        Border::new().padding(Thickness::new(36.0, 24.0, 36.0, 36.0)).content(
            // Stretched up to the maximum, so every page has the same width
            // whatever its content; on a wide window the column is centred.
            StackPanel::new()
                .spacing(GAP_MEDIUM)
                .max_width(PAGE_MAX_WIDTH)
                .horizontal_alignment(HorizontalAlignment::Stretch)
                .keyed_children(children),
        ),
    )
}

/// A small line chart of recent values, oldest on the left. Gaps (failed
/// readings) are left open. `floor` and `ceiling` widen the scale so that a
/// nearly flat series does not look dramatic.
pub fn sparkline(
    values: &[Option<f64>],
    slots: usize,
    (width, height): (f64, f64),
    (floor, ceiling): (Option<f64>, Option<f64>),
    colour: Color,
) -> View {
    let known = values.iter().flatten();
    let low = known.clone().copied().fold(f64::INFINITY, f64::min);
    let high = known.copied().fold(f64::NEG_INFINITY, f64::max);
    let low = floor.map_or(low, |floor| low.min(floor));
    let high = ceiling.map_or(high, |ceiling| high.max(ceiling));
    // A flat series still needs a scale; draw it through the middle.
    let span = if (high - low).abs() < f64::EPSILON { 1.0 } else { high - low };
    let step = width / (slots.max(2) - 1) as f64;
    // New values enter on the right, so a short history is right-aligned.
    let offset = slots.saturating_sub(values.len());
    let point = |index: usize, value: f64| {
        let x = (offset + index) as f64 * step;
        let y = (height - 1.0) - (value - low) / span * (height - 2.0);
        (x, y)
    };
    let segments = values.windows(2).enumerate().filter_map(|(index, pair)| {
        let (from, to) = (pair[0]?, pair[1]?);
        let ((x1, y1), (x2, y2)) = (point(index, from), point(index + 1, to));
        let line = Line::new().x1(x1).y1(y1).x2(x2).y2(y2).stroke(colour).stroke_thickness(1.5);
        Some(KeyedView::new(index.to_string(), line))
    });
    Canvas::new().width(width).height(height).keyed_children(segments)
}
