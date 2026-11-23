//! What the fans of this machine allow, decided in one place.
//!
//! [`assess`] turns the board's identity, the Super I/O chip the driver
//! found, the driver's state and whether the build has fan control into the
//! [`FanStatus`] the status view shows: the board's channels and, when
//! nothing may be written, the reason in words. Always compiled, because it
//! only describes: a [`ControlGrant`] still comes from
//! [`BoardCatalog::offer`] and nowhere else.
//!
//! The order of the reasons is the order a user can act on them: PawnIO
//! first, then the board's name, then the chip, then the catalog.

use syspilot_proto::message::{
    DriverState, FanAvailability, FanChannelStatus, FanPhase, FanStatus, SystemInfo,
};

use crate::chips::Detected;

use super::board::{Board, BoardCatalog, CatalogError};

/// Title of the inventory section that holds the baseboard's strings.
const BOARD_SECTION: &str = "Motherboard";

/// The key of the duty sensor of output `pwm`, which names the channel in a
/// profile and in the status.
pub fn channel_key(pwm: u8) -> String {
    format!("{}{pwm}", syspilot_proto::profile::FAN_CHANNEL_PREFIX)
}

/// The key of the tachometer sensor of input `tach`.
pub fn tachometer_key(tach: u8) -> String {
    format!("board.fan{tach}")
}

/// The baseboard's manufacturer and product as the inventory read them from
/// SMBIOS; `None` when either is unknown.
pub fn board_identity(system: &SystemInfo) -> Option<(String, String)> {
    let section = system.sections.iter().find(|section| section.title == BOARD_SECTION)?;
    let row = |label: &str| {
        section.rows.iter().find(|(name, _)| name == label).map(|(_, value)| value.trim())
    };
    let (manufacturer, product) = (row("Manufacturer")?, row("Product")?);
    (!manufacturer.is_empty() && !product.is_empty())
        .then(|| (manufacturer.to_owned(), product.to_owned()))
}

/// Everything the decision rests on.
pub struct FanInputs<'a> {
    /// The boards SysPilot knows.
    pub catalog: &'a BoardCatalog,
    /// The baseboard's manufacturer and product, if known.
    pub board: Option<(&'a str, &'a str)>,
    /// The Super I/O chip the driver found, if any.
    pub chip: Option<&'a Detected>,
    /// The PawnIO driver's state.
    pub driver: &'a DriverState,
    /// Whether this build has the `fan-control` cargo feature.
    pub control_compiled: bool,
}

fn driver_reason(driver: &DriverState) -> Option<String> {
    match driver {
        DriverState::Ready { .. } => None,
        DriverState::Missing => Some("PawnIO is not installed".to_owned()),
        DriverState::TooOld { version, required } => {
            Some(format!("PawnIO {version} is installed and {required} or newer is needed"))
        }
        DriverState::Failed { error } => Some(format!("PawnIO could not be opened: {error}")),
    }
}

fn catalog_channels(board: &Board) -> Vec<FanChannelStatus> {
    board
        .channels()
        .iter()
        .map(|channel| FanChannelStatus {
            channel: channel_key(channel.pwm),
            label: channel.label.clone(),
            tach_sensor: channel.tach.map(tachometer_key),
            min_duty_pct: channel.limits.min_duty,
            phase: FanPhase::Bios,
            ..FanChannelStatus::default()
        })
        .collect()
}

/// Decides what is allowed and lists the board's channels.
pub fn assess(inputs: &FanInputs<'_>) -> FanStatus {
    let entry =
        inputs.board.and_then(|(manufacturer, product)| inputs.catalog.find(manufacturer, product));
    let board = match (entry, inputs.board) {
        (Some(entry), _) => format!("{} {}", entry.manufacturer(), entry.product()),
        (None, Some((manufacturer, product))) => format!("{manufacturer} {product}"),
        (None, None) => String::new(),
    };
    let channels = entry.map(catalog_channels).unwrap_or_default();
    let read_only = |reason: String| FanStatus {
        availability: FanAvailability::ReadOnly { reason },
        board: board.clone(),
        channels: channels.clone(),
    };

    if let Some(reason) = driver_reason(inputs.driver) {
        return read_only(reason);
    }
    let Some((manufacturer, product)) = inputs.board else {
        return read_only("the board's name could not be read".to_owned());
    };
    let nuvoton = match inputs.chip {
        Some(Detected::Nuvoton { chip, .. }) => *chip,
        Some(other) => {
            let found = other.name().unwrap_or_else(|| "an unknown chip".to_owned());
            return read_only(format!(
                "the board's sensor chip is {found}, which has no fan control"
            ));
        }
        None => return read_only("no supported sensor chip was found on the board".to_owned()),
    };
    match inputs.catalog.offer(manufacturer, product, nuvoton) {
        Err(reason) => read_only(reason.to_string()),
        // A grant exists, so the entry is validated. Without the feature the
        // build still cannot write.
        Ok(_) if inputs.control_compiled => {
            FanStatus { availability: FanAvailability::Controllable, board, channels }
        }
        Ok(_) => FanStatus { availability: FanAvailability::NotInBuild, board, channels },
    }
}

/// [`assess`] against the catalog compiled into the binary. A catalog that
/// cannot be read leaves the fans read-only and says so.
pub fn assess_shipped(
    system: &SystemInfo,
    chip: Option<&Detected>,
    driver: &DriverState,
    control_compiled: bool,
) -> FanStatus {
    let identity = board_identity(system);
    let board =
        identity.as_ref().map(|(manufacturer, product)| (manufacturer.as_str(), product.as_str()));
    match BoardCatalog::shipped() {
        Ok(catalog) => {
            assess(&FanInputs { catalog: &catalog, board, chip, driver, control_compiled })
        }
        Err(error) => FanStatus {
            availability: FanAvailability::ReadOnly { reason: catalog_failure(&error) },
            board: String::new(),
            channels: Vec::new(),
        },
    }
}

fn catalog_failure(error: &CatalogError) -> String {
    format!("the board catalog could not be read: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::SuperIoSlot;
    use crate::chips::nct67xx::Chip;
    use syspilot_proto::message::InfoSection;

    const CATALOG: &str = r#"
[[board]]
manufacturer = "ACME"
product = "B550 EXAMPLE"
chip = "NCT6779D"
validated = true
validated-by = "A. Tester"
validated-on = "2026-12-01"

[[board.channel]]
pwm = 1
tach = 1
label = "CPU fan"
min-duty = 20
may-stop = false

[[board.channel]]
pwm = 2
label = "Case fan"
min-duty = 30
may-stop = false

[[board]]
manufacturer = "ACME"
product = "A320 UNTRIED"
chip = "NCT6779D"
validated = false

[[board.channel]]
pwm = 1
tach = 1
label = "Fan 1 (unverified)"
min-duty = 30
may-stop = false
"#;

    const READY: DriverState = DriverState::Ready { version: String::new() };

    fn catalog() -> BoardCatalog {
        BoardCatalog::parse("test.toml", CATALOG).unwrap()
    }

    fn nuvoton(chip: Chip) -> Detected {
        Detected::Nuvoton {
            slot: SuperIoSlot::Primary,
            chip,
            id_and_revision: 0,
            base: 0x0A20,
            lock_register: 0,
        }
    }

    fn assessed(
        board: Option<(&str, &str)>,
        chip: Option<&Detected>,
        driver: &DriverState,
        compiled: bool,
    ) -> FanStatus {
        assess(&FanInputs { catalog: &catalog(), board, chip, driver, control_compiled: compiled })
    }

    fn reason(status: &FanStatus) -> &str {
        match &status.availability {
            FanAvailability::ReadOnly { reason } => reason,
            other => panic!("(fan status): expected read-only | received: {other:?}"),
        }
    }

    #[test]
    fn a_validated_board_with_its_chip_is_controllable_when_the_build_has_the_feature() {
        let chip = nuvoton(Chip::Nct6779D);
        let status = assessed(Some(("ACME", "B550 EXAMPLE")), Some(&chip), &READY, true);
        assert_eq!(status.availability, FanAvailability::Controllable);
        assert_eq!(status.board, "ACME B550 EXAMPLE");
        let keys: Vec<_> = status.channels.iter().map(|c| c.channel.as_str()).collect();
        assert_eq!(keys, ["board.pwm1", "board.pwm2"]);
        assert_eq!(status.channels[0].tach_sensor.as_deref(), Some("board.fan1"));
        assert_eq!(status.channels[1].tach_sensor, None);
        assert_eq!(status.channels[1].min_duty_pct, 30);
        assert!(status.channels.iter().all(|c| !c.active && c.phase == FanPhase::Bios));
    }

    #[test]
    fn a_build_without_the_feature_is_told_so_even_for_a_validated_board() {
        let chip = nuvoton(Chip::Nct6779D);
        let status = assessed(Some(("ACME", "B550 EXAMPLE")), Some(&chip), &READY, false);
        assert_eq!(status.availability, FanAvailability::NotInBuild);
        assert_eq!(status.channels.len(), 2, "the channels are still listed");
    }

    #[test]
    fn a_board_whose_entry_is_not_validated_stays_read_only_with_its_channels_listed() {
        let chip = nuvoton(Chip::Nct6779D);
        for compiled in [true, false] {
            let status = assessed(Some(("ACME", "A320 UNTRIED")), Some(&chip), &READY, compiled);
            assert_eq!(reason(&status), "the board's entry has not been validated");
            assert_eq!(status.channels.len(), 1);
            assert_eq!(status.channels[0].label, "Fan 1 (unverified)");
        }
    }

    #[test]
    fn an_unknown_board_is_read_only_with_no_channels_but_keeps_its_name() {
        let chip = nuvoton(Chip::Nct6779D);
        let status = assessed(Some(("OTHER", "X1")), Some(&chip), &READY, true);
        assert_eq!(reason(&status), "the board is not in the catalog");
        assert_eq!(status.board, "OTHER X1");
        assert!(status.channels.is_empty());
    }

    #[test]
    fn a_chip_other_than_the_entrys_is_read_only() {
        let chip = nuvoton(Chip::Nct6776F);
        let status = assessed(Some(("ACME", "B550 EXAMPLE")), Some(&chip), &READY, true);
        assert_eq!(reason(&status), "the entry is for NCT6779D but the board has NCT6776F");
    }

    #[test]
    fn the_reasons_come_in_the_order_a_user_can_act_on_them() {
        let chip = nuvoton(Chip::Nct6779D);
        let known = Some(("ACME", "B550 EXAMPLE"));
        // PawnIO first, whatever else is wrong.
        let missing = assessed(None, None, &DriverState::Missing, true);
        assert_eq!(reason(&missing), "PawnIO is not installed");
        let too_old = DriverState::TooOld { version: "1.9.0".into(), required: "2.0.0".into() };
        assert_eq!(
            reason(&assessed(known, Some(&chip), &too_old, true)),
            "PawnIO 1.9.0 is installed and 2.0.0 or newer is needed"
        );
        let failed = DriverState::Failed { error: "access denied".into() };
        assert_eq!(
            reason(&assessed(known, Some(&chip), &failed, true)),
            "PawnIO could not be opened: access denied"
        );
        // Then the board's name, then the chip.
        assert_eq!(
            reason(&assessed(None, Some(&chip), &READY, true)),
            "the board's name could not be read"
        );
        assert_eq!(
            reason(&assessed(known, None, &READY, true)),
            "no supported sensor chip was found on the board"
        );
    }

    #[test]
    fn a_chip_without_a_fan_write_path_is_named() {
        let unsupported = Detected::Unsupported {
            slot: SuperIoSlot::Primary,
            raw_id: 0x1234,
            name: Some("IT8688E".into()),
            reason: "no reader".into(),
        };
        let status = assessed(Some(("ACME", "B550 EXAMPLE")), Some(&unsupported), &READY, true);
        assert_eq!(reason(&status), "the board's sensor chip is IT8688E, which has no fan control");
    }

    fn inventory(rows: &[(&str, &str)]) -> SystemInfo {
        SystemInfo {
            summary: Default::default(),
            sections: vec![
                InfoSection {
                    title: "Memory".into(),
                    rows: vec![("Product".into(), "RAM".into())],
                },
                InfoSection {
                    title: "Motherboard".into(),
                    rows: rows.iter().map(|(a, b)| ((*a).into(), (*b).into())).collect(),
                },
            ],
        }
    }

    #[test]
    fn the_board_identity_is_read_from_the_motherboard_section() {
        let system = inventory(&[("Manufacturer", " MACHINIST "), ("Product", "E5-MR9A PRO")]);
        assert_eq!(
            board_identity(&system),
            Some(("MACHINIST".to_owned(), "E5-MR9A PRO".to_owned()))
        );
        assert_eq!(board_identity(&inventory(&[("Product", "E5-MR9A PRO")])), None);
        assert_eq!(board_identity(&inventory(&[("Manufacturer", "A"), ("Product", " ")])), None);
        assert_eq!(board_identity(&SystemInfo::default()), None);
    }

    #[test]
    fn the_shipped_reference_board_is_listed_but_read_only() {
        let system = inventory(&[("Manufacturer", "MACHINIST"), ("Product", "E5-MR9A PRO")]);
        let chip = nuvoton(Chip::Nct6779D);
        let status = assess_shipped(&system, Some(&chip), &READY, true);
        assert_eq!(reason(&status), "the board's entry has not been validated");
        assert_eq!(status.board, "MACHINIST E5-MR9A PRO");
        assert_eq!(status.channels.len(), 5);
        assert!(
            status.channels.iter().all(|channel| !channel.active),
            "(shipped catalog): expected no channel to be controlled on the unvalidated board"
        );
    }

    #[test]
    fn channel_and_tachometer_keys_match_the_sensor_keys() {
        assert_eq!(channel_key(3), "board.pwm3");
        assert_eq!(tachometer_key(3), "board.fan3");
    }
}
