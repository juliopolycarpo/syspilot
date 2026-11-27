//! Where everything on a [`Screen`] goes, in device-independent pixels
//! (1/96 inch), after Fluent's spacing: a card on the backdrop with the
//! badge, headline, steps, bar and footer, and the buttons under it.

use crate::screen::Screen;

/// Width of the window's content. The height follows from what it shows.
pub const WIDTH: f32 = 520.0;
/// From the window's edge to the card.
const MARGIN: f32 = 16.0;
/// From the card's edge to its content.
const PADDING: f32 = 20.0;
/// The badge's side.
pub const BADGE: f32 = 32.0;
/// Between the badge and the text column.
const BADGE_GAP: f32 = 12.0;
/// Between two blocks on the card.
const BLOCK_GAP: f32 = 16.0;
/// Between two steps.
const ROW_GAP: f32 = 4.0;
/// A step's mark.
pub const MARK: f32 = 16.0;
/// The bar's thickness.
pub const BAR: f32 = 3.0;
/// A button's height, and its narrowest width.
const BUTTON_HEIGHT: f32 = 32.0;
const BUTTON_MIN_WIDTH: f32 = 120.0;
/// Room on either side of a button's label.
const BUTTON_PADDING: f32 = 12.0;
/// Between two buttons.
const BUTTON_GAP: f32 = 8.0;

/// The kinds of text, after Fluent's type ramp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// Subtitle: 20 px semibold.
    Headline,
    /// Body: 14 px.
    Body,
    /// Caption: 12 px.
    Caption,
}

/// Measures text as it will be drawn.
pub trait Measure {
    /// The width and height of `text` in `style`, wrapped at `width`.
    fn measure(&self, text: &str, style: Style, width: f32) -> (f32, f32);
}

/// A rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    /// Left.
    pub x: f32,
    /// Top.
    pub y: f32,
    /// Width.
    pub width: f32,
    /// Height.
    pub height: f32,
}

impl Rect {
    /// Right edge.
    pub fn right(&self) -> f32 {
        self.x + self.width
    }

    /// Bottom edge.
    pub fn bottom(&self) -> f32 {
        self.y + self.height
    }

    /// Whether `(x, y)` is inside.
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
}

/// Where each part of a screen goes.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Layout {
    /// The window's content width.
    pub width: f32,
    /// The window's content height.
    pub height: f32,
    /// The card.
    pub card: Rect,
    /// The badge.
    pub badge: Rect,
    /// The headline.
    pub headline: Rect,
    /// Each step: its mark, then its text.
    pub rows: Vec<(Rect, Rect)>,
    /// The reason for a failure.
    pub reason: Option<Rect>,
    /// The bar.
    pub bar: Option<Rect>,
    /// The footer.
    pub note: Option<Rect>,
    /// The buttons, as on the screen.
    pub buttons: Vec<Rect>,
}

impl Layout {
    /// The button at `(x, y)`, if any.
    pub fn button_at(&self, x: f32, y: f32) -> Option<usize> {
        self.buttons.iter().position(|button| button.contains(x, y))
    }
}

/// Lays `screen` out, measuring its text with `measure`.
pub fn lay_out(screen: &Screen, measure: &dyn Measure) -> Layout {
    let card_x = MARGIN;
    let card_width = WIDTH - 2.0 * MARGIN;
    let inner_x = card_x + PADDING;
    let text_x = inner_x + BADGE + BADGE_GAP;
    let text_width = card_x + card_width - PADDING - text_x;
    let top = MARGIN + PADDING;

    let badge = Rect { x: inner_x, y: top, width: BADGE, height: BADGE };
    let (_, headline_height) = measure.measure(&screen.headline, Style::Headline, text_width);
    // A one-line headline sits in the middle of the badge; a longer one
    // starts at its top.
    let headline_y = top + ((BADGE - headline_height) / 2.0).max(0.0);
    let headline = Rect { x: text_x, y: headline_y, width: text_width, height: headline_height };
    let mut bottom = badge.bottom().max(headline.bottom());

    let mut rows = Vec::with_capacity(screen.rows.len());
    for (index, row) in screen.rows.iter().enumerate() {
        let gap = if index == 0 { BLOCK_GAP } else { ROW_GAP };
        let (_, height) = measure.measure(row.text, Style::Body, text_width);
        let text = Rect { x: text_x, y: bottom + gap, width: text_width, height };
        // Marks sit in the badge's column, in the middle of their line.
        let mark = Rect {
            x: inner_x + (BADGE - MARK) / 2.0,
            y: text.y + (height - MARK) / 2.0,
            width: MARK,
            height: MARK,
        };
        rows.push((mark, text));
        bottom = text.bottom();
    }

    let block = |text: Option<&str>, style: Style, bottom: &mut f32| {
        text.map(|text| {
            let (_, height) = measure.measure(text, style, text_width);
            let rect = Rect { x: text_x, y: *bottom + BLOCK_GAP, width: text_width, height };
            *bottom = rect.bottom();
            rect
        })
    };
    let reason = block(screen.reason.as_deref(), Style::Body, &mut bottom);
    let bar = screen.bar.map(|_| {
        let rect = Rect { x: text_x, y: bottom + BLOCK_GAP, width: text_width, height: BAR };
        bottom = rect.bottom();
        rect
    });
    let note = block(screen.note, Style::Caption, &mut bottom);
    let card = Rect { x: card_x, y: MARGIN, width: card_width, height: bottom + PADDING - MARGIN };

    // Buttons under the card, at its right, in screen order.
    let widths: Vec<f32> = screen
        .buttons
        .iter()
        .map(|button| {
            let (width, _) = measure.measure(button.label, Style::Body, WIDTH);
            (width + 2.0 * BUTTON_PADDING).max(BUTTON_MIN_WIDTH).ceil()
        })
        .collect();
    let total = widths.iter().sum::<f32>() + BUTTON_GAP * widths.len().saturating_sub(1) as f32;
    let mut x = card.right() - total;
    let button_y = card.bottom() + BLOCK_GAP;
    let buttons: Vec<Rect> = widths
        .iter()
        .map(|&width| {
            let rect = Rect { x, y: button_y, width, height: BUTTON_HEIGHT };
            x += width + BUTTON_GAP;
            rect
        })
        .collect();
    let height = buttons.first().map_or(card.bottom(), Rect::bottom) + MARGIN;

    Layout { width: WIDTH, height, card, badge, headline, rows, reason, bar, note, buttons }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::Screen;
    use crate::watch::Step;

    /// Text as if every character were as wide as `char_width` and every
    /// line `line_height` tall, wrapped by characters.
    struct GridMeasure {
        char_width: f32,
        line_height: f32,
    }

    impl Measure for GridMeasure {
        fn measure(&self, text: &str, _style: Style, width: f32) -> (f32, f32) {
            let full = text.chars().count() as f32 * self.char_width;
            let lines = (full / width).ceil().max(1.0);
            (full.min(width), lines * self.line_height)
        }
    }

    const GRID: GridMeasure = GridMeasure { char_width: 7.0, line_height: 20.0 };

    #[test]
    fn everything_stays_inside_the_window_and_the_card() {
        let screen = Screen::working(Some("0.3.1"), Step::Installing, 400);
        let layout = lay_out(&screen, &GRID);
        let inside = |rect: &Rect, outer: &Rect, name: &str| {
            assert!(
                rect.x >= outer.x
                    && rect.y >= outer.y
                    && rect.right() <= outer.right()
                    && rect.bottom() <= outer.bottom(),
                "({name}): expected inside {outer:?} | received: {rect:?}"
            );
        };
        let window = Rect { x: 0.0, y: 0.0, width: layout.width, height: layout.height };
        inside(&layout.card, &window, "card");
        inside(&layout.headline, &layout.card, "headline");
        for (mark, text) in &layout.rows {
            inside(mark, &layout.card, "mark");
            inside(text, &layout.card, "step");
        }
        inside(&layout.bar.unwrap(), &layout.card, "bar");
        inside(&layout.note.unwrap(), &layout.card, "note");
        for button in &layout.buttons {
            inside(button, &window, "button");
            assert!(button.y >= layout.card.bottom(), "(button): expected under the card");
        }
    }

    #[test]
    fn blocks_follow_each_other_without_overlapping() {
        let before = Screen::working(None, Step::Installing, 400);
        let screen = Screen::failed(
            "Failed",
            "A reason long enough to wrap onto a second line \
            of the card, and then some more words.",
            true,
            Some(&before),
        );
        let layout = lay_out(&screen, &GRID);
        let mut tops: Vec<(f32, f32, &str)> =
            vec![(layout.headline.y, layout.headline.bottom(), "headline")];
        tops.extend(layout.rows.iter().map(|(_, text)| (text.y, text.bottom(), "step")));
        let reason = layout.reason.unwrap();
        tops.push((reason.y, reason.bottom(), "reason"));
        let bar = layout.bar.unwrap();
        tops.push((bar.y, bar.bottom(), "bar"));
        for pair in tops.windows(2) {
            assert!(
                pair[1].0 >= pair[0].1,
                "({} then {}): expected no overlap | received: {:?}",
                pair[0].2,
                pair[1].2,
                pair
            );
        }
        assert!(reason.height >= 40.0, "(long reason): expected two lines | received: {reason:?}");
    }

    #[test]
    fn the_window_keeps_its_size_from_step_to_done() {
        let working = lay_out(&Screen::working(Some("0.3.1"), Step::Preparing, 0), &GRID);
        let done = lay_out(&Screen::done(Some("0.3.1")), &GRID);
        assert_eq!(
            (working.width, working.height),
            (done.width, done.height),
            "(done): expected the size of the window while it ran"
        );
    }

    #[test]
    fn buttons_sit_at_the_right_in_screen_order_and_fit_their_label() {
        let screen = Screen::failed("h", "r", true, None);
        let layout = lay_out(&screen, &GRID);
        let [open, close] = layout.buttons[..] else {
            panic!("(failure with Open): expected two buttons | received: {:?}", layout.buttons)
        };
        assert!(open.right() <= close.x, "(order): Open SysPilot then Close");
        assert!((close.right() - layout.card.right()).abs() < 0.01, "(close): at the card's right");
        assert!(open.width >= 13.0 * 7.0, "(Open SysPilot): expected room for its label");
        assert_eq!(layout.button_at(close.x + 1.0, close.y + 1.0), Some(1));
        assert_eq!(layout.button_at(0.0, 0.0), None);
    }

    #[test]
    fn marks_sit_beside_their_step_in_the_badge_column() {
        let layout = lay_out(&Screen::working(None, Step::Preparing, 0), &GRID);
        for (mark, text) in &layout.rows {
            let middle = |rect: &Rect| rect.y + rect.height / 2.0;
            assert!((middle(mark) - middle(text)).abs() < 0.01, "{mark:?} {text:?}");
            assert!(
                (mark.x + MARK / 2.0 - (layout.badge.x + BADGE / 2.0)).abs() < 0.01,
                "(mark): expected under the badge's middle | received: {mark:?}"
            );
        }
    }
}
