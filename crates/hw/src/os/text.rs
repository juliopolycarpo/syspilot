//! Text cleanup for strings that come from firmware, drivers and the
//! registry.

use std::collections::HashSet;

/// Normalises a device or vendor string for display: control characters and
/// anything after the first NUL are dropped, runs of whitespace collapse to
/// one space, and the ends are trimmed.
///
/// Firmware pads fixed-width fields with spaces (`"CT500BX500SSD1       "`),
/// CPU brand strings are right-aligned with leading spaces, and some drivers
/// leave stray NULs, so every string shown to the user goes through here.
pub(crate) fn clean_text(raw: &str) -> String {
    let until_nul = raw.split('\0').next().unwrap_or_default();
    let mut cleaned = String::with_capacity(until_nul.len());
    for word in until_nul.split(|c: char| c.is_whitespace() || c.is_control()) {
        if word.is_empty() {
            continue;
        }
        if !cleaned.is_empty() {
            cleaned.push(' ');
        }
        cleaned.push_str(word);
    }
    cleaned
}

/// Decodes UTF-16 up to the first NUL (or the end of `units`) and cleans it
/// with [`clean_text`]. Unpaired surrogates become U+FFFD.
pub(crate) fn clean_utf16(units: &[u16]) -> String {
    let end = units.iter().position(|&unit| unit == 0).unwrap_or(units.len());
    clean_text(&String::from_utf16_lossy(&units[..end]))
}

/// Makes device names unique by numbering repeats: two identical cards
/// become `X` and `X (2)`. The hub groups sensors by device name, so without
/// this two identical drives would collapse into one device.
pub(crate) fn number_repeated_names(names: &mut [String]) {
    let mut seen: HashSet<String> = HashSet::new();
    for name in names.iter_mut() {
        let base = name.clone();
        let mut number = 2;
        while !seen.insert(name.clone()) {
            *name = format!("{base} ({number})");
            number += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_names_are_numbered_and_unique_names_untouched() {
        let mut names =
            vec!["RTX".to_owned(), "RTX".to_owned(), "Radeon".to_owned(), "RTX".to_owned()];
        number_repeated_names(&mut names);
        assert_eq!(names, ["RTX", "RTX (2)", "Radeon", "RTX (3)"]);
    }

    #[test]
    fn numbering_does_not_collide_with_a_name_that_already_ends_in_a_number() {
        let mut names = vec!["Disk (2)".to_owned(), "Disk".to_owned(), "Disk".to_owned()];
        number_repeated_names(&mut names);
        assert_eq!(names, ["Disk (2)", "Disk", "Disk (3)"]);
    }

    #[test]
    fn padding_and_runs_of_spaces_are_removed() {
        assert_eq!(
            clean_text("  Intel(R)  Xeon(R) CPU   E5-2680 v4 @ 2.40GHz   "),
            "Intel(R) Xeon(R) CPU E5-2680 v4 @ 2.40GHz"
        );
        assert_eq!(clean_text("CT500BX500SSD1       "), "CT500BX500SSD1");
    }

    #[test]
    fn text_after_a_nul_is_dropped() {
        assert_eq!(clean_text("Radeon\0garbage"), "Radeon");
        assert_eq!(clean_text("\0Radeon"), "");
    }

    #[test]
    fn control_characters_and_tabs_become_separators() {
        assert_eq!(clean_text("a\tb\r\nc\u{7}d"), "a b c d");
    }

    #[test]
    fn empty_and_blank_inputs_give_an_empty_string() {
        assert_eq!(clean_text(""), "");
        assert_eq!(clean_text("   \t "), "");
    }

    #[test]
    fn non_ascii_text_is_kept() {
        assert_eq!(clean_text(" Conexão  Local* 2 "), "Conexão Local* 2");
    }

    #[test]
    fn utf16_stops_at_the_nul_and_ignores_the_tail() {
        let units: Vec<u16> = "Realtek PCIe GbE\0leftover".encode_utf16().collect();
        assert_eq!(clean_utf16(&units), "Realtek PCIe GbE");
    }

    #[test]
    fn utf16_without_a_nul_uses_the_whole_slice() {
        let units: Vec<u16> = " AMD ".encode_utf16().collect();
        assert_eq!(clean_utf16(&units), "AMD");
    }

    #[test]
    fn utf16_with_an_unpaired_surrogate_is_replaced_not_rejected() {
        assert_eq!(clean_utf16(&[0x41, 0xD800, 0x42]), "A\u{FFFD}B");
    }
}
