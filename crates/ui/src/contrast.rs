//! Whether a Windows contrast theme is on.

use windows_sys::Win32::UI::Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW};
use windows_sys::Win32::UI::WindowsAndMessaging::{SPI_GETHIGHCONTRAST, SystemParametersInfoW};

/// Whether the `dwFlags` of a `HIGHCONTRAST` structure say a contrast theme
/// is on. Other bits describe the hot key and the settings page.
fn flags_say_on(flags: u32) -> bool {
    flags & HCF_HIGHCONTRASTON != 0
}

/// Asks Windows whether a contrast theme is on. `None` when the question
/// itself fails, which the caller treats as off.
fn query() -> Option<bool> {
    let mut settings = HIGHCONTRASTW {
        cbSize: size_of::<HIGHCONTRASTW>() as u32,
        dwFlags: 0,
        lpszDefaultScheme: std::ptr::null_mut(),
    };
    // SAFETY: `settings` is a correctly sized HIGHCONTRASTW that lives for
    // the call; SPI_GETHIGHCONTRAST only writes into it.
    let ok = unsafe {
        SystemParametersInfoW(SPI_GETHIGHCONTRAST, settings.cbSize, (&raw mut settings).cast(), 0)
    };
    (ok != 0).then(|| flags_say_on(settings.dwFlags))
}

/// Whether a contrast theme is on now.
pub fn high_contrast_on() -> bool {
    query().unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_on_bit_turns_contrast_on() {
        assert!(flags_say_on(HCF_HIGHCONTRASTON));
        assert!(flags_say_on(HCF_HIGHCONTRASTON | 0x2));
        assert!(!flags_say_on(0));
        // HCF_AVAILABLE and HCF_HOTKEYACTIVE alone do not mean on.
        assert!(!flags_say_on(0x2 | 0x4));
    }

    #[test]
    fn windows_answers_the_question() {
        assert!(query().is_some(), "(SPI_GETHIGHCONTRAST): expected an answer | received: failure");
    }
}
