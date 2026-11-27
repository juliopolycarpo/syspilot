//! Design tokens and the building blocks every page is made of.
//!
//! Pages do not pick sizes or colours themselves. They compose these
//! pieces, which keeps spacing, type and state colours the same everywhere.

use windows_reactor::*;

use crate::model::{Badge, CurveShape, Tone};

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
    /// A thermometer. Segoe Fluent Icons has no fan, and a fan curve is
    /// the fan's answer to temperature.
    pub const FANS: &str = "\u{E9CA}";
    /// Sliders, as on a mixing desk.
    pub const TUNING: &str = "\u{E9E9}";
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
///
/// Under a Windows contrast theme the user has picked every colour, so
/// the palette steps aside: text keeps the system text colour, fills go,
/// and shapes are drawn in the text colour. State is never told by colour
/// alone (badges and banners say it in words), so nothing is lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    dark: bool,
    high_contrast: bool,
}

impl Palette {
    /// The palette for `scheme`, or the system's colours when a contrast
    /// theme is on.
    pub fn new(scheme: ColorScheme, high_contrast: bool) -> Self {
        Self { dark: scheme == ColorScheme::Dark, high_contrast }
    }

    /// Text and icon colour for `tone`: Windows' system state colours.
    /// `None` for neutral, and for everything under a contrast theme,
    /// which then uses the normal text colour.
    pub fn ink(self, tone: Tone) -> Option<Brush> {
        if self.high_contrast {
            return None;
        }
        let (dark, light) = match tone {
            Tone::Neutral => return None,
            Tone::Good => ((0x6C, 0xCB, 0x5F), (0x0F, 0x7B, 0x0F)),
            Tone::Caution => ((0xFC, 0xE1, 0x00), (0x9D, 0x5D, 0x00)),
            Tone::Bad => ((0xFF, 0x99, 0xA4), (0xC4, 0x2B, 0x1C)),
        };
        let (red, green, blue) = if self.dark { dark } else { light };
        Some(Color::rgb(red, green, blue).into())
    }

    /// The fill of a state shape such as a dot.
    pub fn shape(self, tone: Tone) -> Brush {
        if self.high_contrast {
            return ThemeBrush::PrimaryText.into();
        }
        self.ink(tone).unwrap_or(Color::rgb(0x8A, 0x8A, 0x8A).into())
    }

    /// The outline of a badge: none normally, where the wash marks it out;
    /// the text colour under a contrast theme, where there is no wash.
    pub fn outline(self) -> (f64, Brush) {
        if self.high_contrast {
            (1.0, ThemeBrush::PrimaryText.into())
        } else {
            (0.0, Color::transparent().into())
        }
    }

    /// A faint fill behind text of `tone`; nothing under a contrast theme.
    pub fn wash(self, tone: Tone) -> Brush {
        if self.high_contrast {
            return Color::transparent().into();
        }
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
        Color::argb(if tone == Tone::Neutral { 0x14 } else { 0x26 }, red, green, blue).into()
    }

    /// The line colour of charts.
    pub fn chart(self) -> Brush {
        if self.high_contrast {
            ThemeBrush::PrimaryText.into()
        } else if self.dark {
            Color::rgb(0x60, 0xCD, 0xFF).into()
        } else {
            Color::rgb(0x00, 0x5F, 0xB8).into()
        }
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
    Ellipse::new()
        .width(size)
        .height(size)
        .fill(palette.shape(tone))
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
    let (outline, outline_brush) = palette.outline();
    Border::new()
        .padding(Thickness::new(GAP_SMALL, 2.0, GAP_SMALL, 3.0))
        .corner_radius(4.0)
        .border_thickness(outline)
        .border_brush(outline_brush)
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

/// A left-aligned text link whose label lines up with surrounding text.
pub fn link(label: impl Into<String>, tooltip: impl Into<String>, on_click: Callback<()>) -> View {
    Button::new()
        .style(ButtonStyle::TextLink)
        // WinUI's text-link style has 12 px horizontal padding; pull left so
        // the link text lines up with the paragraph above it.
        .margin(Thickness::new(-12.0, 0.0, 0.0, 0.0))
        .horizontal_alignment(HorizontalAlignment::Left)
        .on_click(on_click)
        .content(label.into())
        .tooltip(tooltip)
}

/// Range and current value for [`slider_with_number_box`].
#[derive(Debug, Clone, Copy)]
pub struct NumericRange {
    /// Minimum allowed value.
    pub min: f64,
    /// Maximum allowed value.
    pub max: f64,
    /// Value currently shown.
    pub value: f64,
}

/// A slider paired with a numeric input box for quick adjustment and exact entry.
///
/// When `range.max <= range.min`, only the numeric box is shown so the slider
/// never has a zero-width range.
pub fn slider_with_number_box(
    range: NumericRange,
    enabled: bool,
    slider_width: Option<f64>,
    box_width: f64,
    (slider_name, box_name): (String, String),
    (on_slider, on_box): (Callback<f64>, Callback<Option<f64>>),
) -> View {
    let number_box = NumberBox::new()
        .minimum(range.min)
        .maximum(range.max)
        .value(range.value)
        .is_enabled(enabled)
        .automation_name(box_name)
        .width(box_width)
        .vertical_alignment(VerticalAlignment::Center)
        .on_value_changed(on_box);

    if range.max <= range.min {
        return number_box.into();
    }

    let slider = Slider::new()
        .minimum(range.min)
        .maximum(range.max)
        .value(range.value)
        .step_frequency(1.0)
        .is_enabled(enabled)
        .automation_name(slider_name)
        .vertical_alignment(VerticalAlignment::Center)
        .on_value_changed(on_slider);

    let slider_col = match slider_width {
        Some(width) => GridLength::Pixel(width),
        None => GridLength::Star(1.0),
    };
    Grid::new()
        .columns([slider_col, GridLength::Pixel(box_width)])
        .column_spacing(GAP_SMALL)
        .children((slider, number_box.grid_column(1)))
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

/// Points a chart's coordinates are counted from before they all restart,
/// so the numbers stay small.
const CHART_REBASE: u64 = 4096;

/// End points of one segment, in the strip's coordinates.
type SegmentEnds = ((f64, f64), (f64, f64));

/// Where a chart's segments go. `slots[k]` is the segment drawn by element
/// `k`, or `None` for an element with nothing to draw (a gap, or a history
/// shorter than the chart). The strip holding them is moved `shift` to the
/// right and is `strip_width` wide.
#[derive(Debug, PartialEq)]
struct ChartLayout {
    slots: Vec<Option<SegmentEnds>>,
    shift: f64,
    strip_width: f64,
}

/// Lays out `values`, the first of which is point number `first`.
///
/// The chart keeps a fixed set of line elements, one per slot, and never
/// adds, removes or reorders them: the segment starting at point `n` is
/// always drawn by slot `n % slots`, at an x that depends only on `n`. When
/// a new point arrives the strip moves one step left and only the slot it
/// reuses changes, instead of every segment moving (one property update
/// per line per refresh was the app's main CPU cost). A change of scale
/// still moves every segment, as it must.
fn chart_layout(
    first: u64,
    values: &[Option<f64>],
    slots: usize,
    (width, height): (f64, f64),
    (floor, ceiling): (Option<f64>, Option<f64>),
) -> ChartLayout {
    let slots = slots.max(2);
    let known = values.iter().flatten();
    let low = known.clone().copied().fold(f64::INFINITY, f64::min);
    let high = known.copied().fold(f64::NEG_INFINITY, f64::max);
    let low = floor.map_or(low, |floor| low.min(floor));
    let high = ceiling.map_or(high, |ceiling| high.max(ceiling));
    // A flat series still needs a scale; draw it through the middle.
    let span = if (high - low).abs() < f64::EPSILON { 1.0 } else { high - low };
    let step = width / (slots - 1) as f64;
    let base = first - first % CHART_REBASE;
    let point = |number: u64, value: f64| {
        let x = (number - base) as f64 * step;
        let y = (height - 1.0) - (value - low) / span * (height - 2.0);
        (x, y)
    };
    let mut layout_slots = vec![None; slots];
    // At most `slots - 1` segments, on consecutive numbers: no two share a slot.
    for (pair, number) in values.windows(2).zip(first..).take(slots - 1) {
        if let (Some(from), Some(to)) = (pair[0], pair[1]) {
            layout_slots[(number % slots as u64) as usize] =
                Some((point(number, from), point(number + 1, to)));
        }
    }
    // New values enter on the right, so a short history is right-aligned.
    let offset = slots.saturating_sub(values.len()) as f64;
    let shift = (offset - (first - base) as f64) * step;
    // The strip reaches the chart's right edge. With no values the shift is
    // past that edge; WinUI refuses a negative width, and nothing is drawn.
    ChartLayout { slots: layout_slots, shift, strip_width: (width - shift).max(0.0) }
}

/// A small line chart of recent values, oldest on the left; `first` is the
/// number of the first value (see [`crate::model::Trends::numbered_chart_points`]).
/// Gaps (failed readings) are left open. `floor` and `ceiling` widen the
/// scale so that a nearly flat series does not look dramatic.
pub fn sparkline(
    (first, values): (u64, &[Option<f64>]),
    slots: usize,
    (width, height): (f64, f64),
    scale: (Option<f64>, Option<f64>),
    colour: Brush,
) -> View {
    let layout = chart_layout(first, values, slots, (width, height), scale);
    let lines = layout.slots.into_iter().enumerate().map(|(slot, segment)| {
        let line = Line::new().stroke(colour).stroke_thickness(1.5);
        // An unused slot keeps its element, invisible, so the set of
        // elements never changes.
        let line = match segment {
            Some(((x1, y1), (x2, y2))) => line.x1(x1).y1(y1).x2(x2).y2(y2),
            None => line.opacity(0.0),
        };
        KeyedView::new(slot.to_string(), line)
    });
    // The strip's right edge is always the chart's right edge, so it is
    // right-aligned and only its width changes; moving it by margin left
    // lines that did not change drawn in the wrong place.
    let strip = Canvas::new()
        .width(layout.strip_width)
        .height(height)
        .horizontal_alignment(HorizontalAlignment::Right)
        .keyed_children(lines);
    Grid::new().width(width).height(height).children((strip,))
}

/// Size of the dots on a curve's points.
const CURVE_DOT: f64 = 8.0;
/// Opacity of the grid lines and of the marker line: quieter than the data.
const CHART_GRID: f64 = 0.18;

/// A fan curve as a line chart: the points joined, a faint grid at 0, 50
/// and 100 %, and a vertical line at the current temperature. `followed`
/// says whether the service acts on the curve; one that it does not is drawn
/// at half strength, so it reads as a preview and not as a state.
///
/// Everything is the one series colour (the curve) on the text colour at low
/// opacity (the grid), so it holds in light, dark and contrast themes alike.
pub fn curve_chart(
    shape: &CurveShape,
    (width, height): (f64, f64),
    colour: Brush,
    followed: bool,
) -> View {
    let strength = if followed { 1.0 } else { 0.5 };
    let mut parts: Vec<KeyedView> = Vec::new();
    for (index, y) in shape.grid.iter().enumerate() {
        let line = Line::new()
            .x1(0.0)
            .y1(*y)
            .x2(width)
            .y2(*y)
            .stroke(ThemeBrush::PrimaryText)
            .stroke_thickness(1.0)
            .opacity(CHART_GRID);
        parts.push(KeyedView::new(format!("grid-{index}"), line));
    }
    if let Some(x) = shape.marker {
        let line = Line::new()
            .x1(x)
            .y1(0.0)
            .x2(x)
            .y2(height)
            .stroke(ThemeBrush::PrimaryText)
            .stroke_thickness(1.0)
            .opacity(0.6);
        parts.push(KeyedView::new("now", line));
    }
    for (index, pair) in shape.points.windows(2).enumerate() {
        let ((x1, y1), (x2, y2)) = (pair[0], pair[1]);
        let line = Line::new()
            .x1(x1)
            .y1(y1)
            .x2(x2)
            .y2(y2)
            .stroke(colour)
            .stroke_thickness(2.0)
            .opacity(strength);
        parts.push(KeyedView::new(format!("segment-{index}"), line));
    }
    for (index, (x, y)) in shape.points.iter().enumerate() {
        let dot = Ellipse::new()
            .width(CURVE_DOT)
            .height(CURVE_DOT)
            .fill(colour)
            .canvas_left(x - CURVE_DOT / 2.0)
            .canvas_top(y - CURVE_DOT / 2.0)
            .opacity(strength);
        parts.push(KeyedView::new(format!("point-{index}"), dot));
    }
    Canvas::new().width(width).height(height).keyed_children(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TONES: [Tone; 4] = [Tone::Neutral, Tone::Good, Tone::Caution, Tone::Bad];

    /// The x range on screen of the segment drawn by `slot`.
    fn on_screen(layout: &ChartLayout, slot: usize) -> (f64, f64) {
        let ((x1, _), (x2, _)) = layout.slots[slot].expect("slot draws a segment");
        (x1 + layout.shift, x2 + layout.shift)
    }

    #[test]
    fn a_new_point_changes_one_slot_and_moves_the_strip_one_step() {
        let scale = (Some(0.0), Some(100.0));
        let (size, slots) = ((20.0, 10.0), 3);
        let before = chart_layout(0, &[Some(10.0), Some(20.0), Some(30.0)], slots, size, scale);
        let after = chart_layout(1, &[Some(20.0), Some(30.0), Some(40.0)], slots, size, scale);
        let changed: Vec<usize> =
            (0..slots).filter(|&slot| before.slots[slot] != after.slots[slot]).collect();
        // One slot is always free: the leaving segment's slot empties and
        // the new segment takes the free one. Two lines change, however long
        // the chart.
        assert_eq!(changed, [0, 2], "(changed slots): expected the leaving and the new one");
        assert_eq!(
            on_screen(&before, 1),
            (10.0, 20.0),
            "(segment 1 before): expected the right edge"
        );
        assert_eq!(on_screen(&after, 1), (0.0, 10.0), "(segment 1 after): expected one step left");
        assert_eq!(
            on_screen(&after, 2 % slots),
            (10.0, 20.0),
            "(segment 2): expected the right edge"
        );
    }

    #[test]
    fn a_short_history_is_right_aligned_and_gaps_leave_their_slot_empty() {
        let values = [Some(1.0), None, Some(3.0), Some(4.0)];
        let layout = chart_layout(0, &values, 6, (50.0, 10.0), (None, None));
        let drawn: Vec<usize> = (0..6).filter(|&slot| layout.slots[slot].is_some()).collect();
        assert_eq!(drawn, [2], "(drawn slots): expected only 3 → 4, around the failed reading");
        assert_eq!(on_screen(&layout, 2), (40.0, 50.0), "(last segment): expected the right edge");
    }

    #[test]
    fn the_strip_always_reaches_the_right_edge_and_is_never_negative() {
        let values = vec![Some(1.0); 30];
        for first in [0, 1, 29, CHART_REBASE - 1, CHART_REBASE + 3] {
            let layout = chart_layout(first, &values, 30, (180.0, 28.0), (None, None));
            let right_edge = layout.shift + layout.strip_width;
            assert!(
                (right_edge - 180.0).abs() < 1e-9,
                "(strip right edge at point {first}): expected 180 | received: {right_edge}"
            );
        }
        let empty = chart_layout(0, &[], 30, (180.0, 28.0), (None, None));
        assert!(empty.slots.iter().all(Option::is_none));
        assert_eq!(empty.strip_width, 0.0, "(strip width with no values): expected 0");
    }

    #[test]
    fn coordinates_restart_every_few_thousand_points() {
        let first = CHART_REBASE * 3 + 1;
        let layout = chart_layout(first, &[Some(1.0), Some(2.0)], 2, (10.0, 10.0), (None, None));
        let slot = (first % 2) as usize;
        let ((x1, _), _) = layout.slots[slot].unwrap();
        assert_eq!(x1, 10.0, "(x of point {first}): expected 1 step past the rebase");
        assert_eq!(on_screen(&layout, slot), (0.0, 10.0));
    }

    #[test]
    fn states_have_their_own_colours_in_both_schemes() {
        for scheme in [ColorScheme::Light, ColorScheme::Dark] {
            let palette = Palette::new(scheme, false);
            assert_eq!(palette.ink(Tone::Neutral), None, "{scheme:?}");
            let inks: Vec<_> = TONES[1..].iter().map(|&tone| palette.ink(tone)).collect();
            assert!(inks.iter().all(|ink| matches!(ink, Some(Brush::Solid(_)))), "{inks:?}");
            assert!(inks[0] != inks[1] && inks[1] != inks[2] && inks[0] != inks[2], "{inks:?}");
            assert_eq!(palette.outline().0, 0.0);
        }
        assert_ne!(
            Palette::new(ColorScheme::Light, false).chart(),
            Palette::new(ColorScheme::Dark, false).chart()
        );
    }

    #[test]
    fn a_curve_chart_builds_for_a_curve_a_lone_point_and_no_curve() {
        let shape = |points: Vec<(f64, f64)>, marker| CurveShape {
            points,
            grid: [95.0, 50.0, 5.0],
            marker,
            domain: (20.0, 100.0),
        };
        let colour = Palette::new(ColorScheme::Dark, false).chart();
        let size = (280.0, 100.0);
        for (points, marker, followed) in [
            (vec![(5.0, 90.0), (100.0, 50.0), (270.0, 5.0)], Some(120.0), true),
            (vec![(5.0, 90.0)], None, false),
            (Vec::new(), Some(10.0), true),
        ] {
            // Building must not panic on any shape the model can produce.
            let _ = curve_chart(&shape(points, marker), size, colour, followed);
        }
    }

    #[test]
    fn a_contrast_theme_gets_the_system_colours_only() {
        let text = Brush::from(ThemeBrush::PrimaryText);
        for scheme in [ColorScheme::Light, ColorScheme::Dark] {
            let palette = Palette::new(scheme, true);
            for tone in TONES {
                assert_eq!(palette.ink(tone), None, "(ink of {tone:?}): expected the system text");
                assert_eq!(palette.shape(tone), text, "(shape of {tone:?})");
                assert_eq!(palette.wash(tone), Brush::Solid(Color::transparent()), "{tone:?}");
            }
            assert_eq!(palette.chart(), text);
            // Without a wash a badge needs a line around it to read as one.
            assert_eq!(palette.outline(), (1.0, text));
        }
    }

    #[test]
    fn slider_with_number_box_builds_for_valid_and_degenerate_ranges() {
        let _ = slider_with_number_box(
            NumericRange { min: -50.0, max: 50.0, value: 0.0 },
            true,
            Some(200.0),
            104.0,
            ("Power limit slider".into(), "Power limit".into()),
            (Callback::new(|_| {}), Callback::new(|_| {})),
        );
        let _ = slider_with_number_box(
            NumericRange { min: 100.0, max: 100.0, value: 100.0 },
            false,
            None,
            96.0,
            ("Fixed slider".into(), "Fixed".into()),
            (Callback::new(|_| {}), Callback::new(|_| {})),
        );
    }
}
