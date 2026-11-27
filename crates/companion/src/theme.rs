//! The window's colours and backdrop, chosen as the app chooses its own:
//! the user's theme, Fluent colours with the Windows accent, Mica where
//! Windows has it, and only the system's colours under a contrast theme.

use syspilot_proto::update::CompanionTheme;

/// The byte at `shift` in `hex`, from 0 to 1.
const fn part(hex: u32, shift: u32) -> f32 {
    ((hex >> shift) & 0xFF) as f32 / 255.0
}

/// A colour with straight (not premultiplied) alpha, each part from 0 to 1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color {
    /// Red.
    pub r: f32,
    /// Green.
    pub g: f32,
    /// Blue.
    pub b: f32,
    /// Opacity.
    pub a: f32,
}

impl Color {
    /// An opaque colour from `0xRRGGBB`.
    pub const fn rgb(hex: u32) -> Self {
        Self::argb(0xFF00_0000 | hex)
    }

    /// A colour from `0xAARRGGBB`, as Fluent's theme resources write them.
    pub const fn argb(hex: u32) -> Self {
        Self { a: part(hex, 24), r: part(hex, 16), g: part(hex, 8), b: part(hex, 0) }
    }

    /// The same colour at `alpha` times its opacity.
    pub const fn faded(self, alpha: f32) -> Self {
        Self { a: self.a * alpha, ..self }
    }

    /// Nothing.
    pub const TRANSPARENT: Self = Self::argb(0);
}

/// Light or dark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    /// Dark text on light.
    Light,
    /// Light text on dark.
    Dark,
}

impl Scheme {
    /// The scheme for `choice`, when Windows says apps are light or not.
    pub fn of(choice: CompanionTheme, system_is_light: bool) -> Self {
        match choice {
            CompanionTheme::Light => Self::Light,
            CompanionTheme::Dark => Self::Dark,
            CompanionTheme::System if system_is_light => Self::Light,
            CompanionTheme::System => Self::Dark,
        }
    }
}

/// The two shades of the Windows accent that Fluent fills with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Accent {
    /// For dark: `AccentLight2`.
    pub light2: Color,
    /// For light: `AccentDark1`.
    pub dark1: Color,
}

impl Accent {
    /// The default blue, for when Windows does not say.
    pub const DEFAULT: Self = Self { light2: Color::rgb(0x4CC2FF), dark1: Color::rgb(0x005FB8) };
}

/// The colours Windows uses under a contrast theme.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SystemColors {
    /// `COLOR_WINDOW`.
    pub window: Color,
    /// `COLOR_WINDOWTEXT`.
    pub window_text: Color,
    /// `COLOR_HIGHLIGHT`.
    pub highlight: Color,
    /// `COLOR_HIGHLIGHTTEXT`.
    pub highlight_text: Color,
    /// `COLOR_BTNFACE`.
    pub button_face: Color,
    /// `COLOR_BTNTEXT`.
    pub button_text: Color,
}

/// Where the colours come from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Look {
    /// Fluent, in a scheme, with an accent.
    Fluent(Scheme, Accent),
    /// A contrast theme: the system's colours and nothing else.
    Contrast(SystemColors),
}

/// Every colour the window draws with, named after the Fluent resource it
/// stands for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    /// Behind everything when there is no backdrop.
    pub background: Color,
    /// `CardBackgroundFillColorDefault`.
    pub card: Color,
    /// `CardStrokeColorDefault`.
    pub card_stroke: Color,
    /// `TextFillColorPrimary`.
    pub text: Color,
    /// `TextFillColorSecondary`.
    pub text_secondary: Color,
    /// `AccentFillColorDefault`: the bar, the ticks, the default button.
    pub accent: Color,
    /// `AccentFillColorSecondary`: the default button under the pointer.
    pub accent_hover: Color,
    /// `AccentFillColorTertiary`: the default button pressed.
    pub accent_pressed: Color,
    /// `TextOnAccentFillColorPrimary`.
    pub on_accent: Color,
    /// `ControlStrongStrokeColorDefault`: the bar's track, open steps.
    pub strong_stroke: Color,
    /// The faint ring the running step's spinner turns on.
    pub spinner_track: Color,
    /// `SystemFillColorCritical`: a failure.
    pub critical: Color,
    /// What is drawn on `critical`.
    pub on_critical: Color,
    /// `ControlFillColorDefault`: a button.
    pub button: Color,
    /// `ControlFillColorSecondary`: a button under the pointer.
    pub button_hover: Color,
    /// `ControlFillColorTertiary`: a button pressed.
    pub button_pressed: Color,
    /// `ControlStrokeColorDefault`: a button's edge.
    pub button_stroke: Color,
    /// A button's text.
    pub button_text: Color,
    /// `FocusStrokeColorOuter`: the keyboard focus ring.
    pub focus: Color,
}

impl Palette {
    /// The colours of `look`.
    pub fn new(look: Look) -> Self {
        match look {
            Look::Fluent(Scheme::Dark, accent) => Self {
                background: Color::rgb(0x202020),
                card: Color::argb(0x0DFF_FFFF),
                card_stroke: Color::argb(0x1900_0000),
                text: Color::rgb(0xFFFFFF),
                text_secondary: Color::argb(0xC5FF_FFFF),
                accent: accent.light2,
                accent_hover: accent.light2.faded(0.9),
                accent_pressed: accent.light2.faded(0.8),
                on_accent: Color::rgb(0x000000),
                strong_stroke: Color::argb(0x8BFF_FFFF),
                spinner_track: Color::argb(0x45FF_FFFF),
                critical: Color::rgb(0xFF99A4),
                on_critical: Color::rgb(0x000000),
                button: Color::argb(0x0FFF_FFFF),
                button_hover: Color::argb(0x15FF_FFFF),
                button_pressed: Color::argb(0x08FF_FFFF),
                button_stroke: Color::argb(0x12FF_FFFF),
                button_text: Color::rgb(0xFFFFFF),
                focus: Color::rgb(0xFFFFFF),
            },
            Look::Fluent(Scheme::Light, accent) => Self {
                background: Color::rgb(0xF3F3F3),
                card: Color::argb(0xB3FF_FFFF),
                card_stroke: Color::argb(0x0F00_0000),
                text: Color::argb(0xE400_0000),
                text_secondary: Color::argb(0x9E00_0000),
                accent: accent.dark1,
                accent_hover: accent.dark1.faded(0.9),
                accent_pressed: accent.dark1.faded(0.8),
                on_accent: Color::rgb(0xFFFFFF),
                strong_stroke: Color::argb(0x7200_0000),
                spinner_track: Color::argb(0x3900_0000),
                critical: Color::rgb(0xC42B1C),
                on_critical: Color::rgb(0xFFFFFF),
                button: Color::argb(0xB3FF_FFFF),
                button_hover: Color::argb(0x80F9_F9F9),
                button_pressed: Color::argb(0x4DF9_F9F9),
                button_stroke: Color::argb(0x2900_0000),
                button_text: Color::argb(0xE400_0000),
                focus: Color::argb(0xE400_0000),
            },
            // Every colour the user picked, each in its own role, as the
            // app's own palette does under a contrast theme.
            Look::Contrast(system) => Self {
                background: system.window,
                card: system.window,
                card_stroke: system.window_text,
                text: system.window_text,
                text_secondary: system.window_text,
                accent: system.highlight,
                accent_hover: system.highlight,
                accent_pressed: system.highlight,
                on_accent: system.highlight_text,
                strong_stroke: system.window_text,
                spinner_track: system.window_text,
                critical: system.window_text,
                on_critical: system.window,
                button: system.button_face,
                button_hover: system.highlight,
                button_pressed: system.highlight,
                button_stroke: system.button_text,
                button_text: system.button_text,
                focus: system.window_text,
            },
        }
    }

    /// The text of a button filled with `fill`: a contrast theme pairs the
    /// highlight with its own text colour.
    pub fn text_on(&self, fill: Color) -> Color {
        if fill == self.accent || fill == self.accent_hover || fill == self.accent_pressed {
            self.on_accent
        } else {
            self.button_text
        }
    }
}

/// What is behind the window's content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backdrop {
    /// Mica: the window is cleared to transparent and Windows draws it.
    Mica,
    /// A plain colour, drawn by the window.
    Solid,
}

/// The first Windows build with `DWMWA_SYSTEMBACKDROP_TYPE`: Windows 11 22H2.
pub const MICA_BUILD: u32 = 22621;

/// The backdrop to ask Windows for on `build`: none under a contrast
/// theme, whose colours must be exactly the user's.
pub fn wanted_backdrop(build: u32, contrast: bool) -> Backdrop {
    if contrast || build < MICA_BUILD { Backdrop::Solid } else { Backdrop::Mica }
}

/// The backdrop that is there once Windows was asked: a window cleared to
/// transparent without Mica behind it would show black.
pub fn backdrop(wanted: Backdrop, accepted: bool) -> Backdrop {
    if accepted { wanted } else { Backdrop::Solid }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYSTEM: SystemColors = SystemColors {
        window: Color::rgb(0x000000),
        window_text: Color::rgb(0xFFFFFF),
        highlight: Color::rgb(0x1AEBFF),
        highlight_text: Color::rgb(0x000000),
        button_face: Color::rgb(0x000000),
        button_text: Color::rgb(0xFFFFFF),
    };

    #[test]
    fn the_app_choice_wins_and_system_follows_windows() {
        assert_eq!(Scheme::of(CompanionTheme::Dark, true), Scheme::Dark);
        assert_eq!(Scheme::of(CompanionTheme::Light, false), Scheme::Light);
        assert_eq!(Scheme::of(CompanionTheme::System, true), Scheme::Light);
        assert_eq!(Scheme::of(CompanionTheme::System, false), Scheme::Dark);
    }

    #[test]
    fn colours_read_as_fluent_writes_them() {
        let color = Color::argb(0x80FF_0000);
        assert_eq!((color.r, color.g, color.b), (1.0, 0.0, 0.0));
        assert!((color.a - 128.0 / 255.0).abs() < 1e-6, "(alpha 0x80): received {}", color.a);
        assert_eq!(Color::rgb(0x00FF00).a, 1.0, "(rgb): expected opaque");
    }

    #[test]
    fn the_accent_is_light_on_dark_and_dark_on_light() {
        let accent = Accent { light2: Color::rgb(0x111111), dark1: Color::rgb(0x222222) };
        assert_eq!(Palette::new(Look::Fluent(Scheme::Dark, accent)).accent, accent.light2);
        assert_eq!(Palette::new(Look::Fluent(Scheme::Light, accent)).accent, accent.dark1);
    }

    #[test]
    fn text_reads_on_the_card_in_both_schemes() {
        let dark = Palette::new(Look::Fluent(Scheme::Dark, Accent::DEFAULT));
        let light = Palette::new(Look::Fluent(Scheme::Light, Accent::DEFAULT));
        assert!(dark.text.r > 0.9 && light.text.r < 0.1, "(text): dark {dark:?} light {light:?}");
    }

    #[test]
    fn a_contrast_theme_gets_the_system_colours_only() {
        let palette = Palette::new(Look::Contrast(SYSTEM));
        let allowed = [
            SYSTEM.window,
            SYSTEM.window_text,
            SYSTEM.highlight,
            SYSTEM.highlight_text,
            SYSTEM.button_face,
            SYSTEM.button_text,
        ];
        let used = [
            palette.background,
            palette.card,
            palette.card_stroke,
            palette.text,
            palette.text_secondary,
            palette.accent,
            palette.on_accent,
            palette.strong_stroke,
            palette.spinner_track,
            palette.critical,
            palette.on_critical,
            palette.button,
            palette.button_hover,
            palette.button_stroke,
            palette.button_text,
            palette.focus,
        ];
        for color in used {
            assert!(
                allowed.contains(&color),
                "(contrast theme): expected a system colour | received: {color:?}"
            );
        }
        assert_eq!(palette.text_on(palette.button_hover), SYSTEM.highlight_text);
    }

    #[test]
    fn mica_only_where_windows_has_it_and_no_contrast_theme_is_on() {
        assert_eq!(wanted_backdrop(MICA_BUILD, false), Backdrop::Mica);
        assert_eq!(wanted_backdrop(MICA_BUILD - 1, false), Backdrop::Solid, "(Windows 10)");
        assert_eq!(wanted_backdrop(26300, true), Backdrop::Solid, "(contrast theme)");
    }

    #[test]
    fn a_backdrop_windows_refused_falls_back_to_a_solid_colour() {
        assert_eq!(backdrop(Backdrop::Mica, true), Backdrop::Mica);
        assert_eq!(
            backdrop(Backdrop::Mica, false),
            Backdrop::Solid,
            "(refused Mica): expected a solid colour, not a black window"
        );
        assert_eq!(backdrop(Backdrop::Solid, true), Backdrop::Solid);
    }
}
