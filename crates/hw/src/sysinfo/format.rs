//! Text shown in the information sections for sizes, speeds and dates.

/// Which unit family a size is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SizeUnits {
    /// Powers of 1024 (`KiB`, `MiB`, `GiB`): memory and caches.
    Binary,
    /// Powers of 1000 (`KB`, `MB`, `GB`): drive capacities, which are sold
    /// and labelled that way.
    Decimal,
}

const BINARY_NAMES: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
const DECIMAL_NAMES: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];

/// A byte count in the largest unit that keeps the number at 1 or more:
/// `16 GiB`, `1.5 TB`, `500 GB`. One decimal below 100, none from 100 up,
/// and none when the value is whole.
pub(crate) fn format_size(bytes: u64, units: SizeUnits) -> String {
    let (base, names) = match units {
        SizeUnits::Binary => (1024.0_f64, BINARY_NAMES),
        SizeUnits::Decimal => (1000.0_f64, DECIMAL_NAMES),
    };
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= base && unit + 1 < names.len() {
        value /= base;
        unit += 1;
    }
    let tenths = (value * 10.0).round() / 10.0;
    let number = if tenths.fract() == 0.0 || value >= 100.0 {
        format!("{value:.0}")
    } else {
        format!("{tenths:.1}")
    };
    format!("{number} {}", names[unit])
}

/// A clock speed: `800 MHz`, `2.4 GHz`, `4 GHz`, `3.35 GHz`.
pub(crate) fn format_megahertz(megahertz: u32) -> String {
    if megahertz < 1000 {
        return format!("{megahertz} MHz");
    }
    let gigahertz = format!("{:.2}", f64::from(megahertz) / 1000.0);
    let trimmed = gigahertz.trim_end_matches('0').trim_end_matches('.');
    format!("{trimmed} GHz")
}

/// A memory transfer rate: `2400 MT/s`.
pub(crate) fn format_transfers(mega_transfers: u32) -> String {
    format!("{mega_transfers} MT/s")
}

/// Writes a date as `YYYY-MM-DD` when it is one of the numeric forms the
/// firmware and the display driver use (`03/22/2023`, `5-20-2026`, or
/// already `2023-03-22`). Anything else is returned unchanged: showing the
/// vendor's text beats guessing.
///
/// Both forms are month first, as SMBIOS and the display class key write them.
pub(crate) fn normalize_date(text: &str) -> String {
    let text = text.trim();
    let parts: Vec<&str> = text.split(['/', '-']).collect();
    let numbers: Option<Vec<u32>> = parts.iter().map(|part| part.parse().ok()).collect();
    let (Some(numbers), 3) = (numbers, parts.len()) else {
        return text.to_owned();
    };
    let (year, month, day) = if parts[0].len() == 4 {
        (numbers[0], numbers[1], numbers[2])
    } else if parts[2].len() == 4 {
        (numbers[2], numbers[0], numbers[1])
    } else {
        return text.to_owned();
    };
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return text.to_owned();
    }
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    #[test]
    fn binary_sizes_use_the_largest_whole_unit() {
        assert_eq!(format_size(0, SizeUnits::Binary), "0 B");
        assert_eq!(format_size(512, SizeUnits::Binary), "512 B");
        assert_eq!(format_size(32 << 10, SizeUnits::Binary), "32 KiB");
        assert_eq!(format_size(35 << 20, SizeUnits::Binary), "35 MiB");
        assert_eq!(format_size(16 * GIB, SizeUnits::Binary), "16 GiB");
        assert_eq!(format_size(1536 << 20, SizeUnits::Binary), "1.5 GiB");
        assert_eq!(format_size(4 << 40, SizeUnits::Binary), "4 TiB");
    }

    #[test]
    fn decimal_sizes_match_the_label_on_the_drive() {
        assert_eq!(format_size(500_107_862_016, SizeUnits::Decimal), "500 GB");
        assert_eq!(format_size(1_000_204_886_016, SizeUnits::Decimal), "1 TB");
        assert_eq!(format_size(256_060_514_304, SizeUnits::Decimal), "256 GB");
        assert_eq!(format_size(1_200_000_000_000, SizeUnits::Decimal), "1.2 TB");
        assert_eq!(format_size(64_023_257_088, SizeUnits::Decimal), "64 GB");
    }

    #[test]
    fn size_just_under_a_unit_boundary_is_not_shown_as_the_next_unit_early() {
        // 1023 KiB stays in KiB; it does not round up to "1 MiB".
        assert_eq!(format_size(1023 << 10, SizeUnits::Binary), "1023 KiB");
    }

    #[test]
    fn size_of_the_largest_unit_does_not_run_past_the_names() {
        assert_eq!(format_size(u64::MAX, SizeUnits::Binary), "16384 PiB");
    }

    #[test]
    fn clock_speeds_switch_to_gigahertz_and_drop_trailing_zeros() {
        assert_eq!(format_megahertz(800), "800 MHz");
        assert_eq!(format_megahertz(999), "999 MHz");
        assert_eq!(format_megahertz(1000), "1 GHz");
        assert_eq!(format_megahertz(2400), "2.4 GHz");
        assert_eq!(format_megahertz(4000), "4 GHz");
        assert_eq!(format_megahertz(3350), "3.35 GHz");
    }

    #[test]
    fn transfer_rate_names_its_unit() {
        assert_eq!(format_transfers(2400), "2400 MT/s");
    }

    #[test]
    fn month_first_dates_become_iso() {
        assert_eq!(normalize_date("03/22/2023"), "2023-03-22");
        assert_eq!(normalize_date("5-20-2026"), "2026-05-20");
        assert_eq!(normalize_date(" 12/1/1999 "), "1999-12-01");
    }

    #[test]
    fn iso_dates_are_left_alone() {
        assert_eq!(normalize_date("2023-03-22"), "2023-03-22");
    }

    #[test]
    fn text_that_is_not_a_date_is_returned_unchanged() {
        for text in ["", "unknown", "03/22", "13/40/2023", "03/22/23", "a/b/2023", "1/2/3/4"] {
            assert_eq!(normalize_date(text), text.trim(), "input {text:?}");
        }
    }
}
