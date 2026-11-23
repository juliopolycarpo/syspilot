//! The board catalog: which boards SysPilot may control fans on, and how.
//!
//! A board is found by the SMBIOS manufacturer and product strings of its
//! baseboard plus the Super I/O chip the driver detected; never by a serial
//! number. Control is offered only for a board that
//!
//! 1. has an entry in the catalog,
//! 2. whose entry says `validated = true` (a human tried it on that board and
//!    named themselves and the date in the entry), and
//! 3. whose detected chip is the chip the entry names.
//!
//! Everything else is read-only: [`BoardCatalog::offer`] returns a
//! [`ReadOnlyReason`], and no [`ControlGrant`], the only way to the channel
//! limits the write path needs, exists for it. The
//! catalog is data and never writes anything, so it is always compiled.
//!
//! Format, one or more `[[board]]` tables per file under `catalog/boards/`
//! (every key is required unless marked optional; unknown keys are errors):
//!
//! ```toml
//! [[board]]
//! manufacturer = "ACME"        # SMBIOS baseboard manufacturer
//! product = "B550 EXAMPLE"     # SMBIOS baseboard product
//! chip = "NCT6798D"            # as the driver names it
//! validated = false            # true only after a human tried it
//! validated-by = "Name"        # required with validated = true, else absent
//! validated-on = "2026-12-01"  # required with validated = true, else absent
//! note = "free text"           # optional
//!
//! [[board.channel]]
//! pwm = 1                      # the chip's fan duty output, from 1
//! tach = 1                     # optional: the tachometer input, from 1
//! label = "CPU fan"
//! min-duty = 20                # percent; 0 only with may-stop = true
//! may-stop = false             # true lets the curve stop the fan (min-duty = 0)
//! ```

use std::fmt;
use std::path::Path;

use serde::Deserialize;

use crate::chips::nct67xx::Chip;

/// The chips that have a fan-control write path. A board entry may only be
/// validated for one of them.
pub const FAN_CONTROL_CHIPS: [Chip; 2] = [Chip::Nct6776F, Chip::Nct6779D];

/// A catalog that breaks the format or a rule, with the offending value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogError {
    expected: String,
    received: String,
}

impl CatalogError {
    fn new(expected: impl Into<String>, received: impl Into<String>) -> Self {
        Self { expected: expected.into(), received: received.into() }
    }
}

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "(board catalog): expected {} | received: {}", self.expected, self.received)
    }
}

impl std::error::Error for CatalogError {}

// ---------------------------------------------------------------------------
// File format
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct RawFile {
    #[serde(default)]
    board: Vec<RawBoard>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct RawBoard {
    manufacturer: String,
    product: String,
    chip: String,
    /// Required: there is no default, so an entry cannot become trusted by
    /// leaving the key out.
    validated: bool,
    validated_by: Option<String>,
    validated_on: Option<String>,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    channel: Vec<RawChannel>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct RawChannel {
    pwm: u8,
    #[serde(default)]
    tach: Option<u8>,
    label: String,
    min_duty: u8,
    may_stop: bool,
}

// ---------------------------------------------------------------------------
// Validated model
// ---------------------------------------------------------------------------

/// What a channel's duty may be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelLimits {
    /// The duty never goes below this, percent.
    pub min_duty: u8,
    /// Whether the fan may be stopped. Only then is `min_duty` 0.
    pub may_stop: bool,
}

/// One fan output of a board.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardChannel {
    /// The chip's duty output, counted from 1 (`board.pwm1` is output 1).
    pub pwm: u8,
    /// The tachometer input of the fan on this output, counted from 1.
    pub tach: Option<u8>,
    /// What the header is called on the board.
    pub label: String,
    /// The duty limits.
    pub limits: ChannelLimits,
}

/// Whether a human has validated a board entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Validation {
    /// Nobody has: no control is offered.
    Unvalidated,
    /// A human tried control on this board.
    Validated {
        /// Who.
        by: String,
        /// When, as written in the entry.
        on: String,
    },
}

/// One board of the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Board {
    manufacturer: String,
    product: String,
    chip: Chip,
    validation: Validation,
    note: Option<String>,
    channels: Vec<BoardChannel>,
}

impl Board {
    /// The SMBIOS baseboard manufacturer.
    pub fn manufacturer(&self) -> &str {
        &self.manufacturer
    }

    /// The SMBIOS baseboard product.
    pub fn product(&self) -> &str {
        &self.product
    }

    /// The Super I/O chip.
    pub fn chip(&self) -> Chip {
        self.chip
    }

    /// Whether a human validated the entry.
    pub fn validation(&self) -> &Validation {
        &self.validation
    }

    /// The free-text note, if any.
    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }

    /// The fan outputs, in the order of the entry.
    pub fn channels(&self) -> &[BoardChannel] {
        &self.channels
    }

    fn matches(&self, manufacturer: &str, product: &str) -> bool {
        same_text(&self.manufacturer, manufacturer) && same_text(&self.product, product)
    }
}

/// SMBIOS strings are padded and cased differently between BIOS versions.
fn same_text(left: &str, right: &str) -> bool {
    left.trim().eq_ignore_ascii_case(right.trim())
}

fn describe_board(manufacturer: &str, product: &str) -> String {
    format!("board `{manufacturer} {product}`")
}

fn validate_board(source: &str, raw: RawBoard) -> Result<Board, CatalogError> {
    let name = describe_board(&raw.manufacturer, &raw.product);
    let at = |detail: String| format!("{detail} for {name} in {source}");
    if raw.manufacturer.trim().is_empty() || raw.product.trim().is_empty() {
        return Err(CatalogError::new(
            format!("a non-empty manufacturer and product in {source}"),
            format!("manufacturer `{}`, product `{}`", raw.manufacturer, raw.product),
        ));
    }
    let chip = Chip::from_name(&raw.chip).ok_or_else(|| {
        let names: Vec<_> = Chip::ALL.iter().map(|chip| chip.name()).collect();
        CatalogError::new(
            at(format!("`chip` to be one of {}", names.join(", "))),
            format!("`{}`", raw.chip),
        )
    })?;
    let validation = validation_of(&raw, &at)?;
    if matches!(validation, Validation::Validated { .. }) {
        if !FAN_CONTROL_CHIPS.contains(&chip) {
            let names: Vec<_> = FAN_CONTROL_CHIPS.iter().map(|chip| chip.name()).collect();
            return Err(CatalogError::new(
                at(format!(
                    "`validated = true` only for a chip with a fan-control write path ({})",
                    names.join(", ")
                )),
                format!("chip {}", chip.name()),
            ));
        }
        if raw.channel.is_empty() {
            return Err(CatalogError::new(
                at("`validated = true` with at least one `[[board.channel]]`".to_owned()),
                "no channels".to_owned(),
            ));
        }
    }
    let mut channels: Vec<BoardChannel> = Vec::with_capacity(raw.channel.len());
    for channel in raw.channel {
        let channel = validate_channel(&at, chip, channel)?;
        if let Some(earlier) = channels.iter().find(|earlier| earlier.pwm == channel.pwm) {
            return Err(CatalogError::new(
                at(format!("each `pwm` used once (`{}` has pwm {})", earlier.label, earlier.pwm)),
                format!("pwm {} again, for `{}`", channel.pwm, channel.label),
            ));
        }
        if let Some(earlier) = channels.iter().find(|earlier| earlier.label == channel.label) {
            return Err(CatalogError::new(
                at("each channel label used once".to_owned()),
                format!("`{}` for pwm {} and pwm {}", channel.label, earlier.pwm, channel.pwm),
            ));
        }
        if let (Some(tach), Some(earlier)) = (
            channel.tach,
            channels.iter().find(|earlier| earlier.tach.is_some() && earlier.tach == channel.tach),
        ) {
            return Err(CatalogError::new(
                at("each `tach` used once".to_owned()),
                format!("tach {tach} for pwm {} and pwm {}", earlier.pwm, channel.pwm),
            ));
        }
        channels.push(channel);
    }
    Ok(Board {
        manufacturer: raw.manufacturer.trim().to_owned(),
        product: raw.product.trim().to_owned(),
        chip,
        validation,
        note: raw.note,
        channels,
    })
}

fn validation_of(
    raw: &RawBoard,
    at: &impl Fn(String) -> String,
) -> Result<Validation, CatalogError> {
    let present = |value: &Option<String>| value.as_deref().is_some_and(|v| !v.trim().is_empty());
    match (raw.validated, &raw.validated_by, &raw.validated_on) {
        (true, by, on) if present(by) && present(on) => Ok(Validation::Validated {
            by: by.clone().unwrap_or_default().trim().to_owned(),
            on: on.clone().unwrap_or_default().trim().to_owned(),
        }),
        (true, by, on) => Err(CatalogError::new(
            at("`validated = true` with a non-empty `validated-by` and `validated-on`".to_owned()),
            format!("validated-by {by:?}, validated-on {on:?}"),
        )),
        (false, None, None) => Ok(Validation::Unvalidated),
        (false, by, on) => Err(CatalogError::new(
            at("`validated-by` and `validated-on` only with `validated = true`".to_owned()),
            format!("validated = false with validated-by {by:?}, validated-on {on:?}"),
        )),
    }
}

fn validate_channel(
    at: &impl Fn(String) -> String,
    chip: Chip,
    raw: RawChannel,
) -> Result<BoardChannel, CatalogError> {
    let outputs = chip.pwm_channels();
    if raw.pwm == 0 || usize::from(raw.pwm) > outputs {
        return Err(CatalogError::new(
            at(format!("`pwm` within 1..={outputs} ({} has {outputs} outputs)", chip.name())),
            format!("pwm {}", raw.pwm),
        ));
    }
    if let Some(tach) = raw.tach {
        let inputs = chip.fan_tachometers();
        if tach == 0 || usize::from(tach) > inputs {
            return Err(CatalogError::new(
                at(format!("`tach` within 1..={inputs} ({} has {inputs} inputs)", chip.name())),
                format!("tach {tach} on pwm {}", raw.pwm),
            ));
        }
    }
    if raw.label.trim().is_empty() {
        return Err(CatalogError::new(
            at("a non-empty `label`".to_owned()),
            format!("an empty label on pwm {}", raw.pwm),
        ));
    }
    if raw.min_duty > 100 {
        return Err(CatalogError::new(
            at("`min-duty` within 0..=100".to_owned()),
            format!("min-duty {} on pwm {}", raw.min_duty, raw.pwm),
        ));
    }
    match (raw.may_stop, raw.min_duty) {
        (false, 0) => {
            return Err(CatalogError::new(
                at("`min-duty` of 1 or more when `may-stop = false`".to_owned()),
                format!("min-duty 0, may-stop false on pwm {}", raw.pwm),
            ));
        }
        (true, minimum) if minimum > 0 => {
            return Err(CatalogError::new(
                at("`min-duty = 0` when `may-stop = true`".to_owned()),
                format!("min-duty {minimum}, may-stop true on pwm {}", raw.pwm),
            ));
        }
        _ => {}
    }
    Ok(BoardChannel {
        pwm: raw.pwm,
        tach: raw.tach,
        label: raw.label.trim().to_owned(),
        limits: ChannelLimits { min_duty: raw.min_duty, may_stop: raw.may_stop },
    })
}

// ---------------------------------------------------------------------------
// Catalog
// ---------------------------------------------------------------------------

/// Why no fan control is offered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOnlyReason {
    /// The catalog has no entry for the board.
    UnknownBoard,
    /// The entry exists but no human has validated it.
    NotValidated,
    /// The detected chip is not the one the entry was written for.
    ChipMismatch {
        /// The chip the entry names.
        catalog: Chip,
        /// The chip the driver found.
        detected: Chip,
    },
}

impl fmt::Display for ReadOnlyReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownBoard => f.write_str("the board is not in the catalog"),
            Self::NotValidated => f.write_str("the board's entry has not been validated"),
            Self::ChipMismatch { catalog, detected } => write!(
                f,
                "the entry is for {} but the board has {}",
                catalog.name(),
                detected.name()
            ),
        }
    }
}

/// Permission to control the fans of one validated board. Only
/// [`BoardCatalog::offer`] makes one, so code that writes to a fan can
/// demand a grant and so cannot be reached for an unvalidated board.
#[derive(Debug, Clone, Copy)]
pub struct ControlGrant<'a> {
    board: &'a Board,
}

impl<'a> ControlGrant<'a> {
    /// The validated board.
    pub fn board(&self) -> &'a Board {
        self.board
    }

    /// The chip.
    pub fn chip(&self) -> Chip {
        self.board.chip
    }

    /// The channel on the chip's duty output `pwm` (counted from 1).
    pub fn channel(&self, pwm: u8) -> Option<&'a BoardChannel> {
        self.board.channels.iter().find(|channel| channel.pwm == pwm)
    }
}

/// Boards read from the catalog files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BoardCatalog {
    boards: Vec<Board>,
}

impl BoardCatalog {
    /// An empty catalog: every board is read-only.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Parses one catalog file; `source` names it in errors.
    pub fn parse(source: &str, text: &str) -> Result<Self, CatalogError> {
        let file: RawFile = toml::from_str(text).map_err(|error| {
            CatalogError::new(
                format!("{source} to be a board catalog in the documented format"),
                error.to_string().trim().to_owned(),
            )
        })?;
        let mut catalog = Self::empty();
        for raw in file.board {
            catalog.add(source, validate_board(source, raw)?)?;
        }
        Ok(catalog)
    }

    /// Reads and merges every `*.toml` file of `directory`, in file-name
    /// order.
    pub fn load_directory(directory: &Path) -> Result<Self, CatalogError> {
        let unreadable = |what: &str, error: std::io::Error| {
            CatalogError::new(
                format!("a readable {what} {}", directory.display()),
                error.to_string(),
            )
        };
        let mut files = Vec::new();
        for entry in std::fs::read_dir(directory).map_err(|error| unreadable("directory", error))? {
            let path = entry.map_err(|error| unreadable("directory", error))?.path();
            if path.extension().is_some_and(|extension| extension == "toml") {
                files.push(path);
            }
        }
        files.sort();
        let mut catalog = Self::empty();
        for path in files {
            let text = std::fs::read_to_string(&path).map_err(|error| unreadable("file", error))?;
            let source = path.file_name().map_or_else(
                || path.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            );
            for board in Self::parse(&source, &text)?.boards {
                catalog.add(&source, board)?;
            }
        }
        Ok(catalog)
    }

    fn add(&mut self, source: &str, board: Board) -> Result<(), CatalogError> {
        if self.boards.iter().any(|known| known.matches(&board.manufacturer, &board.product)) {
            return Err(CatalogError::new(
                format!("each board listed once ({source})"),
                format!("{} twice", describe_board(&board.manufacturer, &board.product)),
            ));
        }
        self.boards.push(board);
        Ok(())
    }

    /// The boards, in the order they were read.
    pub fn boards(&self) -> &[Board] {
        &self.boards
    }

    /// The entry for a baseboard, if any.
    pub fn find(&self, manufacturer: &str, product: &str) -> Option<&Board> {
        self.boards.iter().find(|board| board.matches(manufacturer, product))
    }

    /// Whether fans may be controlled on the board with these SMBIOS strings
    /// and this detected chip. Anything the catalog does not positively
    /// allow is read-only.
    pub fn offer(
        &self,
        manufacturer: &str,
        product: &str,
        detected: Chip,
    ) -> Result<ControlGrant<'_>, ReadOnlyReason> {
        let board = self.find(manufacturer, product).ok_or(ReadOnlyReason::UnknownBoard)?;
        if board.chip != detected {
            return Err(ReadOnlyReason::ChipMismatch { catalog: board.chip, detected });
        }
        match board.validation {
            Validation::Unvalidated => Err(ReadOnlyReason::NotValidated),
            Validation::Validated { .. } => Ok(ControlGrant { board }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A validated two-channel board, the base the rule tests alter.
    const VALIDATED: &str = r#"
[[board]]
manufacturer = "ACME"
product = "B550 EXAMPLE"
chip = "NCT6779D"
validated = true
validated-by = "A. Tester"
validated-on = "2026-12-01"
note = "checked by hand"

[[board.channel]]
pwm = 1
tach = 1
label = "CPU fan"
min-duty = 20
may-stop = false

[[board.channel]]
pwm = 2
label = "Case fan"
min-duty = 0
may-stop = true
"#;

    fn parse(text: &str) -> Result<BoardCatalog, CatalogError> {
        BoardCatalog::parse("test.toml", text)
    }

    /// `VALIDATED` with `from` replaced by `to`; panics when `from` is not in
    /// it, so a typo cannot make a rule test pass for the wrong reason.
    fn altered(from: &str, to: &str) -> String {
        assert!(VALIDATED.contains(from), "(test setup): expected `{from}` in the base entry");
        VALIDATED.replacen(from, to, 1)
    }

    fn rejection(text: &str) -> String {
        parse(text).expect_err("the entry should be refused").to_string()
    }

    #[test]
    fn a_validated_board_is_read_into_its_parts() {
        let catalog = parse(VALIDATED).unwrap();
        let board = &catalog.boards()[0];
        assert_eq!((board.manufacturer(), board.product()), ("ACME", "B550 EXAMPLE"));
        assert_eq!(board.chip(), Chip::Nct6779D);
        assert_eq!(
            board.validation(),
            &Validation::Validated { by: "A. Tester".into(), on: "2026-12-01".into() }
        );
        assert_eq!(board.note(), Some("checked by hand"));
        assert_eq!(
            board.channels(),
            [
                BoardChannel {
                    pwm: 1,
                    tach: Some(1),
                    label: "CPU fan".into(),
                    limits: ChannelLimits { min_duty: 20, may_stop: false },
                },
                BoardChannel {
                    pwm: 2,
                    tach: None,
                    label: "Case fan".into(),
                    limits: ChannelLimits { min_duty: 0, may_stop: true },
                },
            ]
        );
    }

    #[test]
    fn control_is_offered_only_for_a_validated_board_with_the_expected_chip() {
        let catalog = parse(VALIDATED).unwrap();
        let grant = catalog.offer("ACME", "B550 EXAMPLE", Chip::Nct6779D).unwrap();
        assert_eq!(grant.chip(), Chip::Nct6779D);
        assert_eq!(grant.channel(2).map(|channel| channel.label.as_str()), Some("Case fan"));
        assert!(grant.channel(3).is_none());
        assert_eq!(grant.board().product(), "B550 EXAMPLE");
    }

    #[test]
    fn smbios_strings_match_without_regard_to_case_and_padding() {
        let catalog = parse(VALIDATED).unwrap();
        assert!(catalog.offer("  acme ", "b550 example\t", Chip::Nct6779D).is_ok());
        assert_eq!(
            catalog.offer("ACME", "B550 EXAMPLE PLUS", Chip::Nct6779D).unwrap_err(),
            ReadOnlyReason::UnknownBoard,
            "a different product is a different board"
        );
    }

    #[test]
    fn an_unknown_board_and_an_empty_catalog_are_read_only() {
        let catalog = parse(VALIDATED).unwrap();
        assert_eq!(
            catalog.offer("OTHER", "B550 EXAMPLE", Chip::Nct6779D).unwrap_err(),
            ReadOnlyReason::UnknownBoard
        );
        assert_eq!(
            BoardCatalog::empty().offer("ACME", "B550 EXAMPLE", Chip::Nct6779D).unwrap_err(),
            ReadOnlyReason::UnknownBoard
        );
        assert!(BoardCatalog::empty().boards().is_empty());
    }

    #[test]
    fn an_unvalidated_board_is_read_only() {
        let text = VALIDATED
            .replace("validated = true", "validated = false")
            .replace("validated-by = \"A. Tester\"\n", "")
            .replace("validated-on = \"2026-12-01\"\n", "");
        let catalog = parse(&text).unwrap();
        assert_eq!(catalog.boards()[0].validation(), &Validation::Unvalidated);
        assert_eq!(
            catalog.offer("ACME", "B550 EXAMPLE", Chip::Nct6779D).unwrap_err(),
            ReadOnlyReason::NotValidated
        );
    }

    #[test]
    fn a_different_detected_chip_is_read_only() {
        let catalog = parse(VALIDATED).unwrap();
        let reason = catalog.offer("ACME", "B550 EXAMPLE", Chip::Nct6798D).unwrap_err();
        assert_eq!(
            reason,
            ReadOnlyReason::ChipMismatch { catalog: Chip::Nct6779D, detected: Chip::Nct6798D }
        );
        assert_eq!(reason.to_string(), "the entry is for NCT6779D but the board has NCT6798D");
    }

    #[test]
    fn read_only_reasons_say_what_is_missing() {
        assert_eq!(ReadOnlyReason::UnknownBoard.to_string(), "the board is not in the catalog");
        assert_eq!(
            ReadOnlyReason::NotValidated.to_string(),
            "the board's entry has not been validated"
        );
    }

    #[test]
    fn the_validated_key_is_required() {
        let text = altered("validated = true\n", "");
        let message = rejection(&text);
        assert!(
            message.starts_with(
                "(board catalog): expected test.toml to be a board catalog in the documented format | received: "
            ),
            "{message}"
        );
        assert!(message.contains("missing field `validated`"), "{message}");
    }

    #[test]
    fn an_unknown_key_is_refused() {
        let message = rejection(&altered("chip = \"NCT6779D\"", "chip = \"NCT6779D\"\nspeed = 3"));
        assert!(message.contains("unknown field `speed`"), "{message}");
        let message = rejection("[[board]]\nfoo = 1\n");
        assert!(message.contains("unknown field `foo`"), "{message}");
    }

    #[test]
    fn text_that_is_not_toml_is_refused_with_the_parser_message() {
        let message = rejection("[[board");
        assert!(
            message.starts_with("(board catalog): expected test.toml to be a board catalog"),
            "{message}"
        );
    }

    #[test]
    fn every_rule_names_the_board_and_the_offending_value() {
        let validated_false = VALIDATED
            .replace("validated = true", "validated = false")
            .replace("validated-by = \"A. Tester\"\n", "")
            .replace("validated-on = \"2026-12-01\"\n", "");
        let cases: Vec<(String, &str)> = vec![
            (
                altered("manufacturer = \"ACME\"", "manufacturer = \"  \""),
                "a non-empty manufacturer and product in test.toml | received: manufacturer `  `, product `B550 EXAMPLE`",
            ),
            (
                altered("chip = \"NCT6779D\"", "chip = \"NCT9999\""),
                "`chip` to be one of NCT6771F, NCT6776F, NCT6779D, NCT6791D, NCT6792D, NCT6792D-A, NCT6793D, NCT6795D, NCT6796D, NCT6796D-R, NCT6797D, NCT6798D, NCT6799D for board `ACME B550 EXAMPLE` in test.toml | received: `NCT9999`",
            ),
            (
                altered("chip = \"NCT6779D\"", "chip = \"NCT6798D\""),
                "`validated = true` only for a chip with a fan-control write path (NCT6776F, NCT6779D) for board `ACME B550 EXAMPLE` in test.toml | received: chip NCT6798D",
            ),
            (
                altered("validated-by = \"A. Tester\"", "validated-by = \" \""),
                "`validated = true` with a non-empty `validated-by` and `validated-on` for board `ACME B550 EXAMPLE` in test.toml | received: validated-by Some(\" \"), validated-on Some(\"2026-12-01\")",
            ),
            (
                altered("validated-by = \"A. Tester\"\n", ""),
                "`validated = true` with a non-empty `validated-by` and `validated-on` for board `ACME B550 EXAMPLE` in test.toml | received: validated-by None, validated-on Some(\"2026-12-01\")",
            ),
            (
                validated_false
                    .replace("validated = false", "validated = false\nvalidated-by = \"X\""),
                "`validated-by` and `validated-on` only with `validated = true` for board `ACME B550 EXAMPLE` in test.toml | received: validated = false with validated-by Some(\"X\"), validated-on None",
            ),
            (
                altered("pwm = 2", "pwm = 6"),
                "`pwm` within 1..=5 (NCT6779D has 5 outputs) for board `ACME B550 EXAMPLE` in test.toml | received: pwm 6",
            ),
            (
                altered("pwm = 2", "pwm = 0"),
                "`pwm` within 1..=5 (NCT6779D has 5 outputs) for board `ACME B550 EXAMPLE` in test.toml | received: pwm 0",
            ),
            (
                altered("pwm = 2", "pwm = 1"),
                "each `pwm` used once (`CPU fan` has pwm 1) for board `ACME B550 EXAMPLE` in test.toml | received: pwm 1 again, for `Case fan`",
            ),
            (
                altered("tach = 1", "tach = 9"),
                "`tach` within 1..=5 (NCT6779D has 5 inputs) for board `ACME B550 EXAMPLE` in test.toml | received: tach 9 on pwm 1",
            ),
            (
                altered("label = \"Case fan\"\n", "label = \"Case fan\"\ntach = 1\n"),
                "each `tach` used once for board `ACME B550 EXAMPLE` in test.toml | received: tach 1 for pwm 1 and pwm 2",
            ),
            (
                altered("label = \"Case fan\"", "label = \"CPU fan\""),
                "each channel label used once for board `ACME B550 EXAMPLE` in test.toml | received: `CPU fan` for pwm 1 and pwm 2",
            ),
            (
                altered("label = \"Case fan\"", "label = \" \""),
                "a non-empty `label` for board `ACME B550 EXAMPLE` in test.toml | received: an empty label on pwm 2",
            ),
            (
                altered("min-duty = 20", "min-duty = 101"),
                "`min-duty` within 0..=100 for board `ACME B550 EXAMPLE` in test.toml | received: min-duty 101 on pwm 1",
            ),
            (
                altered("min-duty = 20", "min-duty = 0"),
                "`min-duty` of 1 or more when `may-stop = false` for board `ACME B550 EXAMPLE` in test.toml | received: min-duty 0, may-stop false on pwm 1",
            ),
            (
                altered("min-duty = 0\nmay-stop = true", "min-duty = 10\nmay-stop = true"),
                "`min-duty = 0` when `may-stop = true` for board `ACME B550 EXAMPLE` in test.toml | received: min-duty 10, may-stop true on pwm 2",
            ),
            (
                VALIDATED.split("[[board.channel]]").next().unwrap().to_owned(),
                "`validated = true` with at least one `[[board.channel]]` for board `ACME B550 EXAMPLE` in test.toml | received: no channels",
            ),
        ];
        for (text, detail) in cases {
            assert_eq!(rejection(&text), format!("(board catalog): expected {detail}"));
        }
    }

    #[test]
    fn an_unvalidated_entry_may_name_a_chip_without_a_write_path() {
        let text = VALIDATED
            .replace("validated = true", "validated = false")
            .replace("validated-by = \"A. Tester\"\n", "")
            .replace("validated-on = \"2026-12-01\"\n", "")
            .replace("NCT6779D", "NCT6798D");
        assert!(parse(&text).is_ok());
    }

    #[test]
    fn a_board_listed_twice_is_refused() {
        let text = format!("{VALIDATED}\n{}", VALIDATED.replace("ACME", "acme"));
        assert_eq!(
            rejection(&text),
            "(board catalog): expected each board listed once (test.toml) | received: board `acme B550 EXAMPLE` twice"
        );
    }

    #[test]
    fn a_file_without_boards_is_an_empty_catalog() {
        assert!(parse("# nothing yet\n").unwrap().boards().is_empty());
    }

    /// A directory under the system temp directory that removes itself.
    struct ScratchDirectory(std::path::PathBuf);

    impl ScratchDirectory {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("syspilot-board-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for ScratchDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_directory_is_loaded_in_file_name_order_and_ignores_other_files() {
        let scratch = ScratchDirectory::new("merge");
        std::fs::write(scratch.0.join("b.toml"), VALIDATED.replace("ACME", "SECOND")).unwrap();
        std::fs::write(scratch.0.join("a.toml"), VALIDATED).unwrap();
        std::fs::write(scratch.0.join("notes.txt"), "not a catalog").unwrap();
        let catalog = BoardCatalog::load_directory(&scratch.0).unwrap();
        let names: Vec<_> = catalog.boards().iter().map(Board::manufacturer).collect();
        assert_eq!(names, ["ACME", "SECOND"]);
    }

    #[test]
    fn a_board_repeated_across_files_is_refused_naming_the_second_file() {
        let scratch = ScratchDirectory::new("duplicate");
        std::fs::write(scratch.0.join("a.toml"), VALIDATED).unwrap();
        std::fs::write(scratch.0.join("b.toml"), VALIDATED).unwrap();
        let message = BoardCatalog::load_directory(&scratch.0).unwrap_err().to_string();
        assert_eq!(
            message,
            "(board catalog): expected each board listed once (b.toml) | received: board `ACME B550 EXAMPLE` twice"
        );
    }

    #[test]
    fn a_file_with_a_rule_broken_is_reported_with_its_name() {
        let scratch = ScratchDirectory::new("broken");
        std::fs::write(scratch.0.join("bad.toml"), altered("pwm = 2", "pwm = 7")).unwrap();
        let message = BoardCatalog::load_directory(&scratch.0).unwrap_err().to_string();
        assert!(message.ends_with("in bad.toml | received: pwm 7"), "{message}");
    }

    #[test]
    fn a_missing_directory_is_an_error_naming_it() {
        let scratch = ScratchDirectory::new("missing");
        let absent = scratch.0.join("absent");
        let message = BoardCatalog::load_directory(&absent).unwrap_err().to_string();
        assert!(
            message.starts_with(&format!(
                "(board catalog): expected a readable directory {} | received: ",
                absent.display()
            )),
            "{message}"
        );
    }

    #[test]
    fn the_shipped_catalog_loads_and_offers_no_control_for_any_board() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../catalog/boards");
        let catalog = BoardCatalog::load_directory(&directory).unwrap();
        assert!(!catalog.boards().is_empty(), "the example entry is shipped");
        for board in catalog.boards() {
            assert_eq!(
                board.validation(),
                &Validation::Unvalidated,
                "(catalog/boards): expected no validated entry until a human has validated one on real hardware | received: {} {} validated",
                board.manufacturer(),
                board.product()
            );
            assert_eq!(
                catalog.offer(board.manufacturer(), board.product(), board.chip()).unwrap_err(),
                ReadOnlyReason::NotValidated
            );
        }
    }

    #[test]
    fn the_fan_control_chips_have_the_outputs_the_catalog_may_name() {
        for chip in FAN_CONTROL_CHIPS {
            assert!(chip.pwm_channels() > 0, "{} has outputs", chip.name());
        }
    }
}
