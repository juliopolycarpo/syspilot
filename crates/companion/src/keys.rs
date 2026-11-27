//! What the keyboard does in the window, as in any Windows dialog: Tab and
//! the arrows move between the buttons, Enter presses the focused one (or
//! the default before the keyboard was used), Space the focused one, Esc
//! closes.

/// The keys the window answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// Esc.
    Escape,
    /// Enter.
    Enter,
    /// Space.
    Space,
    /// Tab, or Shift+Tab when `back`.
    Tab { back: bool },
    /// Left arrow.
    Left,
    /// Right arrow.
    Right,
}

/// Where the keyboard is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Focus {
    /// The button it is on.
    pub at: usize,
    /// Whether its ring shows: only once the keyboard was used.
    pub visible: bool,
}

/// What a key does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Close the window.
    Close,
    /// Press this button.
    Press(usize),
    /// Show the focus on this button.
    Focus(usize),
    /// Nothing.
    Nothing,
}

/// What `key` does among `count` buttons, with the keyboard at `focus`
/// and Enter's button `default`.
pub fn action(key: Key, count: usize, focus: Focus, default: Option<usize>) -> Action {
    let step = |forward: bool| {
        if count == 0 {
            Action::Nothing
        } else if !focus.visible {
            // The first key only shows where the focus already is.
            Action::Focus(focus.at.min(count - 1))
        } else if forward {
            Action::Focus((focus.at + 1) % count)
        } else {
            Action::Focus((focus.at + count - 1) % count)
        }
    };
    match key {
        Key::Escape => Action::Close,
        Key::Enter if focus.visible && focus.at < count => Action::Press(focus.at),
        Key::Enter => default.map_or(Action::Nothing, Action::Press),
        Key::Space if focus.visible && focus.at < count => Action::Press(focus.at),
        Key::Space => Action::Nothing,
        Key::Tab { back } => step(!back),
        Key::Left => step(false),
        Key::Right => step(true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HIDDEN: Focus = Focus { at: 0, visible: false };

    #[test]
    fn enter_presses_the_default_until_the_keyboard_moves_the_focus() {
        assert_eq!(action(Key::Enter, 2, HIDDEN, Some(0)), Action::Press(0));
        assert_eq!(action(Key::Enter, 1, HIDDEN, None), Action::Nothing, "(no default)");
        let on_close = Focus { at: 1, visible: true };
        assert_eq!(action(Key::Enter, 2, on_close, Some(0)), Action::Press(1));
        assert_eq!(action(Key::Space, 2, on_close, Some(0)), Action::Press(1));
        assert_eq!(action(Key::Space, 2, HIDDEN, Some(0)), Action::Nothing, "(hidden focus)");
    }

    #[test]
    fn tab_first_shows_the_focus_then_moves_it_round() {
        assert_eq!(action(Key::Tab { back: false }, 2, HIDDEN, Some(0)), Action::Focus(0));
        let shown = Focus { at: 0, visible: true };
        assert_eq!(action(Key::Tab { back: false }, 2, shown, Some(0)), Action::Focus(1));
        assert_eq!(action(Key::Tab { back: true }, 2, shown, Some(0)), Action::Focus(1));
        assert_eq!(action(Key::Right, 2, Focus { at: 1, visible: true }, None), Action::Focus(0));
        assert_eq!(action(Key::Left, 2, Focus { at: 1, visible: true }, None), Action::Focus(0));
    }

    #[test]
    fn without_buttons_only_esc_does_anything() {
        assert_eq!(action(Key::Escape, 0, HIDDEN, None), Action::Close);
        for key in [Key::Enter, Key::Space, Key::Tab { back: false }, Key::Left, Key::Right] {
            assert_eq!(action(key, 0, HIDDEN, None), Action::Nothing, "{key:?}");
        }
    }
}
