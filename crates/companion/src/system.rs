//! What Windows says about how windows should look: its build, light or
//! dark, a contrast theme, the accent colour and whether to animate.

use std::ffi::c_void;

use syspilot_proto::update::CompanionTheme;
use windows::UI::ViewManagement::{UIColorType, UISettings};
use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::Foundation::{ERROR_SUCCESS, STATUS_SUCCESS};
use windows::Win32::Graphics::Gdi::{
    COLOR_BTNFACE, COLOR_BTNTEXT, COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT, COLOR_WINDOW,
    COLOR_WINDOWTEXT, GetSysColor, SYS_COLOR_INDEX,
};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
use windows::Win32::UI::Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW};
use windows::Win32::UI::WindowsAndMessaging::{
    SPI_GETCLIENTAREAANIMATION, SPI_GETHIGHCONTRAST, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    SystemParametersInfoW,
};
use windows::core::{BOOL, w};

use crate::theme::{Accent, Color, Look, Scheme, SystemColors};

/// The Windows build number, e.g. 22631.
pub fn build() -> u32 {
    let mut version = OSVERSIONINFOW {
        dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    // SAFETY: `version` is a valid, sized OSVERSIONINFOW. Unlike
    // GetVersionEx, this reports the real version whatever the manifest
    // declares.
    let status = unsafe { RtlGetVersion(&mut version) };
    if status == STATUS_SUCCESS { version.dwBuildNumber } else { 0 }
}

/// Whether Windows wants apps light. Light when it does not say, as
/// Windows itself is by default.
pub fn apps_are_light() -> bool {
    let mut light: u32 = 1;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: the key and value names are NUL-terminated literals; `light`
    // and `size` are valid out-pointers and `size` is the buffer's size.
    let read = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut light).cast::<c_void>()),
            Some(&mut size),
        )
    };
    read != ERROR_SUCCESS || light != 0
}

fn system_color(index: SYS_COLOR_INDEX) -> Color {
    // SAFETY: a valid system colour index; the call only reads.
    let bgr = unsafe { GetSysColor(index) };
    // COLORREF is 0x00BBGGRR.
    Color::rgb(((bgr & 0xFF) << 16) | (bgr & 0xFF00) | ((bgr >> 16) & 0xFF))
}

/// The contrast theme's colours, when one is on.
pub fn contrast_colors() -> Option<SystemColors> {
    let mut contrast =
        HIGHCONTRASTW { cbSize: size_of::<HIGHCONTRASTW>() as u32, ..Default::default() };
    // SAFETY: `contrast` is a valid HIGHCONTRASTW with its size set, as
    // SPI_GETHIGHCONTRAST takes.
    let read = unsafe {
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            contrast.cbSize,
            Some((&raw mut contrast).cast::<c_void>()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    (read.is_ok() && contrast.dwFlags.contains(HCF_HIGHCONTRASTON)).then(|| SystemColors {
        window: system_color(COLOR_WINDOW),
        window_text: system_color(COLOR_WINDOWTEXT),
        highlight: system_color(COLOR_HIGHLIGHT),
        highlight_text: system_color(COLOR_HIGHLIGHTTEXT),
        button_face: system_color(COLOR_BTNFACE),
        button_text: system_color(COLOR_BTNTEXT),
    })
}

/// The accent colour's shades, or the default blue when Windows does not
/// say.
pub fn accent() -> Accent {
    let shade = |settings: &UISettings, kind| {
        settings.GetColorValue(kind).map(|color| {
            Color::rgb((u32::from(color.R) << 16) | (u32::from(color.G) << 8) | u32::from(color.B))
        })
    };
    UISettings::new()
        .and_then(|settings| {
            Ok(Accent {
                light2: shade(&settings, UIColorType::AccentLight2)?,
                dark1: shade(&settings, UIColorType::AccentDark1)?,
            })
        })
        .unwrap_or(Accent::DEFAULT)
}

/// Whether Windows animates inside windows; off for users who asked for
/// less motion.
pub fn animations_on() -> bool {
    let mut on = BOOL(1);
    // SAFETY: SPI_GETCLIENTAREAANIMATION writes one BOOL to `on`.
    let read = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some((&raw mut on).cast::<c_void>()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    read.is_err() || on.as_bool()
}

/// The look for the app's `choice`, as Windows is set now.
pub fn look(choice: CompanionTheme) -> Look {
    match contrast_colors() {
        Some(colors) => Look::Contrast(colors),
        None => Look::Fluent(Scheme::of(choice, apps_are_light()), accent()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_build_is_a_windows_10_or_later_build() {
        // Windows 10 1809, the oldest this runs on, is build 17763.
        let build = build();
        assert!(build >= 17763, "(Windows build): expected 17763 or later | received: {build}");
    }

    #[test]
    fn the_accent_is_opaque() {
        let accent = accent();
        assert_eq!((accent.light2.a, accent.dark1.a), (1.0, 1.0), "{accent:?}");
    }

    #[test]
    fn the_look_follows_a_forced_scheme_unless_a_contrast_theme_is_on() {
        match (contrast_colors(), look(CompanionTheme::Dark)) {
            (Some(_), Look::Contrast(_)) | (None, Look::Fluent(Scheme::Dark, _)) => {}
            (contrast, look) => panic!(
                "(--theme dark, contrast {contrast:?}): expected the matching look | received: {look:?}"
            ),
        }
    }
}
