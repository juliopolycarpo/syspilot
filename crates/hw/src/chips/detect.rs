//! Finding Super I/O monitor chips on the two port pairs.
//!
//! Each slot (`0x2E`/`0x2F`, `0x4E`/`0x4F`) is tried with the Winbond/Nuvoton
//! unlock sequence, then, when nothing answered, the ITE one. Both are
//! chip-ID reads: after the unlock a chip answers at configuration register
//! `0x20`; an empty slot reads `0x00` or `0xFF`. The writes are the unlock
//! and leave sequences, the logical-device select and the monitor's own
//! index and bank registers, and nothing else.

use std::ops::{Deref, DerefMut};

use crate::bus::{BusError, SuperIo, SuperIoSlot, Unlock};

use super::{it87xx, nct67xx};

const CHIP_ID_REGISTER: u8 = 0x20;
const REVISION_REGISTER: u8 = 0x21;
/// Base address of the selected logical device (two bytes, high first).
const BASE_ADDRESS_REGISTER: u8 = 0x60;
/// Nuvoton configuration register with the hardware-monitor I/O space lock.
const IO_SPACE_LOCK_REGISTER: u8 = 0x28;
const IO_SPACE_LOCK_BIT: u8 = 0x10;

/// Configuration mode of a Super I/O chip, left again on every path.
///
/// Entering writes the unlock sequence; dropping the guard writes the leave
/// sequence, so an early return or a panic cannot leave the chip's
/// configuration registers exposed. [`ConfigMode::leave`] leaves explicitly
/// and reports the error a drop would have to swallow.
pub struct ConfigMode<'a> {
    io: &'a mut dyn SuperIo,
    unlock: Unlock,
    active: bool,
}

impl<'a> ConfigMode<'a> {
    /// Writes the unlock sequence. The guard is armed before the writes: a
    /// sequence that fails halfway is still followed by the leave sequence.
    pub fn enter(io: &'a mut dyn SuperIo, unlock: Unlock) -> Result<Self, BusError> {
        let mode = Self { io, unlock, active: true };
        mode.io.enter_config_mode(unlock)?;
        Ok(mode)
    }

    /// Leaves configuration mode and reports a failure to do so.
    pub fn leave(mut self) -> Result<(), BusError> {
        self.active = false;
        self.io.exit_config_mode(self.unlock)
    }

    /// Declares that no chip answered the unlock sequence, so there is
    /// nothing to leave. Only for the ITE sequence, whose leave is a write
    /// to a register that a bare bus should not receive.
    pub fn abandon(&mut self) {
        self.active = false;
    }
}

impl<'a> Deref for ConfigMode<'a> {
    type Target = dyn SuperIo + 'a;

    fn deref(&self) -> &Self::Target {
        &*self.io
    }
}

impl DerefMut for ConfigMode<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut *self.io
    }
}

impl Drop for ConfigMode<'_> {
    fn drop(&mut self) {
        if self.active {
            // Nothing can be done about a failure here; the explicit
            // `leave` is where one is reported.
            let _ = self.io.exit_config_mode(self.unlock);
        }
    }
}

/// A chip found on a slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detected {
    /// A Nuvoton NCT67xx whose monitor answered with the Nuvoton vendor ID.
    Nuvoton {
        /// Where the chip answers.
        slot: SuperIoSlot,
        /// The chip.
        chip: nct67xx::Chip,
        /// Configuration registers `0x20` and `0x21`.
        id_and_revision: u16,
        /// Base of the monitor's index/data ports.
        base: u16,
        /// Configuration register `0x28`, where the I/O space lock lives.
        lock_register: u8,
    },
    /// An ITE IT87xx whose monitor answered with an ITE vendor ID.
    Ite {
        /// Where the chip answers.
        slot: SuperIoSlot,
        /// The chip.
        chip: it87xx::Chip,
        /// Chip version, the low nibble of configuration register `0x22`.
        version: u8,
        /// Base of the monitor's index/data ports.
        base: u16,
    },
    /// A chip that answered but cannot be read.
    Unsupported {
        /// Where the chip answers.
        slot: SuperIoSlot,
        /// The raw ID: `id << 8 | revision` for the Winbond family, the
        /// 16-bit chip ID for ITE.
        raw_id: u16,
        /// The chip's name when the ID is known.
        name: Option<String>,
        /// Why it is not read, naming the value found.
        reason: String,
    },
}

impl Detected {
    /// Where the chip answers.
    pub fn slot(&self) -> SuperIoSlot {
        match self {
            Self::Nuvoton { slot, .. }
            | Self::Ite { slot, .. }
            | Self::Unsupported { slot, .. } => *slot,
        }
    }

    /// The chip name, when known.
    pub fn name(&self) -> Option<String> {
        match self {
            Self::Nuvoton { chip, .. } => Some(chip.name().to_owned()),
            Self::Ite { chip, .. } => Some(chip.name().to_owned()),
            Self::Unsupported { name, .. } => name.clone(),
        }
    }

    /// One line for a feature detail: the chip, where it answers and what
    /// was read.
    pub fn describe(&self) -> String {
        let slot = self.slot();
        let ports = format!("ports {:#04x}/{:#04x}", slot.register_port(), slot.value_port());
        match self {
            Self::Nuvoton { chip, id_and_revision, base, .. } => format!(
                "{} (id/revision {id_and_revision:#06x}) at {ports}, monitor at {base:#06x}",
                chip.name()
            ),
            Self::Ite { chip, version, base, .. } => {
                format!("{} (version {version:#x}) at {ports}, monitor at {base:#06x}", chip.name())
            }
            Self::Unsupported { raw_id, name, reason, .. } => {
                let name = name.as_deref().unwrap_or("unknown chip");
                format!("{name} (id {raw_id:#06x}) at {ports} is not read: {reason}")
            }
        }
    }
}

/// The outcome of scanning both slots.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Detection {
    /// The chips that answered, primary slot first.
    pub chips: Vec<Detected>,
    /// Bus errors that stopped the scan; empty when it completed.
    pub problems: Vec<String>,
}

/// Scans both slots. The caller holds the ISA bus guard.
///
/// A bus error ends the scan: the chips found before it are kept, the error
/// is reported in [`Detection::problems`] and no further port is touched.
pub fn detect(io: &mut dyn SuperIo) -> Detection {
    let mut detection = Detection::default();
    for slot in SuperIoSlot::ALL {
        match scan_slot(io, slot) {
            Ok(Some(found)) => detection.chips.push(found),
            Ok(None) => {}
            Err(error) => {
                detection.problems.push(format!(
                    "ports {:#04x}/{:#04x}: {error}",
                    slot.register_port(),
                    slot.value_port()
                ));
                break;
            }
        }
    }
    detection
}

fn scan_slot(io: &mut dyn SuperIo, slot: SuperIoSlot) -> Result<Option<Detected>, BusError> {
    io.select_slot(slot)?;
    if let Some(found) = scan_winbond_family(io, slot)? {
        return Ok(Some(found));
    }
    scan_ite(io, slot)
}

/// Why `base` cannot be a hardware-monitor base address, if it cannot.
///
/// The base must read the same twice (a floating bus does not) and, being a
/// 16-byte aligned I/O range above the legacy ports, have bits 15:12 and
/// 2:0 clear.
fn base_problem(base: u16, verify: u16) -> Option<String> {
    if base != verify {
        return Some(format!(
            "(superio): expected a stable monitor base address | received: {base:#06x} then \
             {verify:#06x}"
        ));
    }
    if base < 0x100 || base & 0xF007 != 0 {
        return Some(format!(
            "(superio): expected a monitor base address of at least 0x100 with bits 15:12 and \
             2:0 clear | received: {base:#06x}"
        ));
    }
    None
}

fn scan_winbond_family(
    io: &mut dyn SuperIo,
    slot: SuperIoSlot,
) -> Result<Option<Detected>, BusError> {
    let mut mode = ConfigMode::enter(io, Unlock::WinbondNuvoton)?;
    let id = mode.config_read(CHIP_ID_REGISTER)?;
    let revision = mode.config_read(REVISION_REGISTER)?;
    if id == 0x00 || id == 0xFF {
        mode.leave()?;
        return Ok(None);
    }
    let raw_id = u16::from_be_bytes([id, revision]);
    let Some(chip) = nct67xx::identify(id, revision) else {
        mode.leave()?;
        let name = nct67xx::recognised_but_unsupported(id, revision).map(str::to_owned);
        let reason = match &name {
            Some(name) => format!(
                "(nct67xx): {name} keeps its monitor in an embedded-controller register space \
                 this reader does not support"
            ),
            None => format!(
                "(superio): expected a supported Nuvoton chip id/revision | received: \
                 {raw_id:#06x}"
            ),
        };
        return Ok(Some(Detected::Unsupported { slot, raw_id, name, reason }));
    };

    // The driver learns the chip's I/O ranges here; scanning logical devices
    // changes the selected one, so select the monitor afterwards.
    mode.discover_io_ranges()?;
    mode.select_logical_device(nct67xx::HWM_LOGICAL_DEVICE)?;
    let base = mode.config_read_word(BASE_ADDRESS_REGISTER)?;
    let verify = mode.config_read_word(BASE_ADDRESS_REGISTER)?;
    // Read only: SysPilot never clears the I/O space lock.
    let lock_register = mode.config_read(IO_SPACE_LOCK_REGISTER)?;
    mode.leave()?;

    let unsupported = |reason: String| Detected::Unsupported {
        slot,
        raw_id,
        name: Some(chip.name().to_owned()),
        reason,
    };
    if let Some(reason) = base_problem(base, verify) {
        return Ok(Some(unsupported(reason)));
    }
    let vendor = nct67xx::read_vendor_id(io, base)?;
    if vendor != nct67xx::VENDOR_ID {
        let locked = chip.has_io_space_lock() && lock_register & IO_SPACE_LOCK_BIT != 0;
        let cause = if locked {
            "; the hardware-monitor I/O space lock (configuration register 0x28 bit 4) is set \
             and SysPilot does not clear it"
        } else {
            ""
        };
        return Ok(Some(unsupported(format!(
            "(nct67xx): expected vendor id {:#06x} | received: {vendor:#06x}{cause}",
            nct67xx::VENDOR_ID
        ))));
    }
    Ok(Some(Detected::Nuvoton { slot, chip, id_and_revision: raw_id, base, lock_register }))
}

fn scan_ite(io: &mut dyn SuperIo, slot: SuperIoSlot) -> Result<Option<Detected>, BusError> {
    let mut mode = ConfigMode::enter(io, Unlock::Ite)?;
    let raw_id = mode.config_read_word(CHIP_ID_REGISTER)?;
    if raw_id == 0x0000 || raw_id == 0xFFFF {
        mode.abandon();
        return Ok(None);
    }
    let Some(chip) = it87xx::identify(raw_id) else {
        mode.leave()?;
        return Ok(Some(Detected::Unsupported {
            slot,
            raw_id,
            name: None,
            reason: format!("(it87xx): expected a known ITE chip id | received: {raw_id:#06x}"),
        }));
    };
    if chip.needs_bank_select() {
        mode.leave()?;
        return Ok(Some(Detected::Unsupported {
            slot,
            raw_id,
            name: Some(chip.name().to_owned()),
            reason: format!(
                "(it87xx): {} needs the bank-select register (0x06) written before each read, \
                 and SysPilot writes no register that is not an index",
                chip.name()
            ),
        }));
    }

    mode.discover_io_ranges()?;
    mode.select_logical_device(it87xx::HWM_LOGICAL_DEVICE)?;
    let base = mode.config_read_word(BASE_ADDRESS_REGISTER)?;
    let verify = mode.config_read_word(BASE_ADDRESS_REGISTER)?;
    let version = mode.config_read(it87xx::VERSION_REGISTER)? & 0x0F;
    mode.leave()?;

    let unsupported = |reason: String| Detected::Unsupported {
        slot,
        raw_id,
        name: Some(chip.name().to_owned()),
        reason,
    };
    if let Some(reason) = base_problem(base, verify) {
        return Ok(Some(unsupported(reason)));
    }
    match it87xx::verify(io, base, chip)? {
        it87xx::Verification::Verified => Ok(Some(Detected::Ite { slot, chip, version, base })),
        it87xx::Verification::Rejected(reason) => Ok(Some(unsupported(reason))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::replay::{
        ReplaySuperIo, SuperIoWrite, superio_config_key, superio_hwm_key, superio_unlock_key,
        unlock_code,
    };
    use crate::bus::{Fixture, SuperIoSlot};

    const PRIMARY: SuperIoSlot = SuperIoSlot::Primary;
    const SECONDARY: SuperIoSlot = SuperIoSlot::Secondary;

    fn set_config(fixture: &mut Fixture, slot: SuperIoSlot, device: u8, register: u8, value: u8) {
        fixture.set(superio_config_key(slot, device, register), u64::from(value));
    }

    /// A slot with no chip: the unlock reads back as 0xFF.
    fn empty_slot(fixture: &mut Fixture, slot: SuperIoSlot) {
        fixture.set(superio_unlock_key(slot), 0);
        set_config(fixture, slot, 0, 0x20, 0xFF);
        set_config(fixture, slot, 0, 0x21, 0xFF);
    }

    /// A Nuvoton chip with its monitor at `base`.
    fn nuvoton_slot(
        fixture: &mut Fixture,
        slot: SuperIoSlot,
        id: u8,
        revision: u8,
        base: u16,
        vendor: u16,
        lock_register: u8,
    ) {
        fixture.set(superio_unlock_key(slot), unlock_code(Unlock::WinbondNuvoton));
        set_config(fixture, slot, 0, 0x20, id);
        set_config(fixture, slot, 0, 0x21, revision);
        set_config(fixture, slot, 0, 0x28, lock_register);
        set_config(fixture, slot, 0x0B, 0x60, (base >> 8) as u8);
        set_config(fixture, slot, 0x0B, 0x61, base as u8);
        fixture.set(superio_hwm_key(slot, 0x80, 0x4F), u64::from(vendor >> 8));
        fixture.set(superio_hwm_key(slot, 0x00, 0x4F), u64::from(vendor & 0xFF));
    }

    /// An ITE chip with its monitor at `base`.
    fn ite_slot(
        fixture: &mut Fixture,
        slot: SuperIoSlot,
        id: u16,
        base: u16,
        vendor: u8,
        configuration: u8,
    ) {
        fixture.set(superio_unlock_key(slot), unlock_code(Unlock::Ite));
        set_config(fixture, slot, 0, 0x20, (id >> 8) as u8);
        set_config(fixture, slot, 0, 0x21, id as u8);
        set_config(fixture, slot, 0, 0x22, 0x15);
        set_config(fixture, slot, 0x04, 0x60, (base >> 8) as u8);
        set_config(fixture, slot, 0x04, 0x61, base as u8);
        fixture.set(superio_hwm_key(slot, 0, 0x58), u64::from(vendor));
        fixture.set(superio_hwm_key(slot, 0, 0x00), u64::from(configuration));
    }

    fn run(fixture: Fixture) -> (Detection, Vec<SuperIoWrite>) {
        let mut io = ReplaySuperIo::new(fixture);
        let log = io.write_log();
        let detection = detect(&mut io);
        let writes = log.lock().unwrap().clone();
        (detection, writes)
    }

    #[test]
    fn the_reference_machine_is_an_nct6779d_on_the_primary_slot() {
        let mut fixture = Fixture::new();
        nuvoton_slot(&mut fixture, PRIMARY, 0xC5, 0x62, 0x0A20, 0x5CA3, 0x00);
        empty_slot(&mut fixture, SECONDARY);
        let (detection, _) = run(fixture);
        assert_eq!(detection.problems, Vec::<String>::new());
        assert_eq!(
            detection.chips,
            [Detected::Nuvoton {
                slot: PRIMARY,
                chip: nct67xx::Chip::Nct6779D,
                id_and_revision: 0xC562,
                base: 0x0A20,
                lock_register: 0x00,
            }]
        );
        assert_eq!(
            detection.chips[0].describe(),
            "NCT6779D (id/revision 0xc562) at ports 0x2e/0x2f, monitor at 0x0a20"
        );
    }

    #[test]
    fn the_scan_writes_only_the_documented_sequences() {
        let mut fixture = Fixture::new();
        nuvoton_slot(&mut fixture, PRIMARY, 0xC5, 0x62, 0x0A20, 0x5CA3, 0x00);
        empty_slot(&mut fixture, SECONDARY);
        let (_, writes) = run(fixture);
        assert_eq!(
            writes,
            [
                SuperIoWrite::SelectSlot(PRIMARY),
                SuperIoWrite::Enter(Unlock::WinbondNuvoton),
                SuperIoWrite::DiscoverRanges,
                SuperIoWrite::LogicalDevice(0x0B),
                SuperIoWrite::Exit(Unlock::WinbondNuvoton),
                SuperIoWrite::HwmIndex { base: 0x0A20, index: 0x4E },
                SuperIoWrite::HwmBank { base: 0x0A20, bank: 0x80 },
                SuperIoWrite::HwmIndex { base: 0x0A20, index: 0x4F },
                SuperIoWrite::HwmIndex { base: 0x0A20, index: 0x4E },
                SuperIoWrite::HwmBank { base: 0x0A20, bank: 0x00 },
                SuperIoWrite::HwmIndex { base: 0x0A20, index: 0x4F },
                SuperIoWrite::SelectSlot(SECONDARY),
                SuperIoWrite::Enter(Unlock::WinbondNuvoton),
                SuperIoWrite::Exit(Unlock::WinbondNuvoton),
                SuperIoWrite::Enter(Unlock::Ite),
            ],
            "no ITE leave sequence when nothing answered, and nothing is ever written to 0x28"
        );
    }

    #[test]
    fn an_ite_chip_is_found_after_the_nuvoton_unlock_gets_no_answer() {
        let mut fixture = Fixture::new();
        ite_slot(&mut fixture, PRIMARY, 0x8686, 0x0290, 0x90, 0x1B);
        empty_slot(&mut fixture, SECONDARY);
        let (detection, writes) = run(fixture);
        assert_eq!(
            detection.chips,
            [Detected::Ite {
                slot: PRIMARY,
                chip: it87xx::Chip::It8686E,
                version: 5,
                base: 0x0290
            }]
        );
        assert!(writes.contains(&SuperIoWrite::Exit(Unlock::Ite)), "{writes:?}");
        assert_eq!(
            detection.chips[0].describe(),
            "IT8686E (version 0x5) at ports 0x2e/0x2f, monitor at 0x0290"
        );
    }

    #[test]
    fn both_slots_can_hold_a_chip() {
        let mut fixture = Fixture::new();
        nuvoton_slot(&mut fixture, PRIMARY, 0xC5, 0x62, 0x0A20, 0x5CA3, 0x00);
        ite_slot(&mut fixture, SECONDARY, 0x8733, 0x0A30, 0x7F, 0x10);
        let (detection, _) = run(fixture);
        let names: Vec<_> = detection.chips.iter().filter_map(Detected::name).collect();
        assert_eq!(names, ["NCT6779D", "IT8792E"]);
        assert_eq!(detection.chips[1].slot(), SECONDARY);
    }

    #[test]
    fn an_empty_bus_finds_nothing_and_leaves_no_configuration_mode_behind() {
        let mut fixture = Fixture::new();
        empty_slot(&mut fixture, PRIMARY);
        empty_slot(&mut fixture, SECONDARY);
        let (detection, writes) = run(fixture);
        assert_eq!(detection, Detection::default());
        let entered = writes
            .iter()
            .filter(|w| matches!(w, SuperIoWrite::Enter(Unlock::WinbondNuvoton)))
            .count();
        let left = writes
            .iter()
            .filter(|w| matches!(w, SuperIoWrite::Exit(Unlock::WinbondNuvoton)))
            .count();
        assert_eq!((entered, left), (2, 2));
    }

    #[test]
    fn an_unknown_chip_is_reported_with_its_raw_id_and_not_read() {
        let mut fixture = Fixture::new();
        nuvoton_slot(&mut fixture, PRIMARY, 0xE7, 0x11, 0x0A20, 0x5CA3, 0x00);
        empty_slot(&mut fixture, SECONDARY);
        let (detection, writes) = run(fixture);
        let [Detected::Unsupported { raw_id, name, reason, .. }] = detection.chips.as_slice()
        else {
            panic!("expected one unsupported chip, got {:?}", detection.chips);
        };
        assert_eq!(*raw_id, 0xE711);
        assert_eq!(*name, None);
        assert_eq!(
            reason,
            "(superio): expected a supported Nuvoton chip id/revision | received: 0xe711"
        );
        assert!(
            !writes.iter().any(|w| matches!(w, SuperIoWrite::DiscoverRanges)),
            "an unknown chip's ranges are not discovered"
        );
        assert!(writes.contains(&SuperIoWrite::Exit(Unlock::WinbondNuvoton)));
    }

    #[test]
    fn an_embedded_controller_chip_is_named_but_not_read() {
        let mut fixture = Fixture::new();
        nuvoton_slot(&mut fixture, PRIMARY, 0xD5, 0x92, 0x0A20, 0x5CA3, 0x00);
        empty_slot(&mut fixture, SECONDARY);
        let (detection, _) = run(fixture);
        assert_eq!(detection.chips[0].name().as_deref(), Some("NCT6687D"));
        assert!(
            detection.chips[0].describe().contains("is not read"),
            "{}",
            detection.chips[0].describe()
        );
    }

    #[test]
    fn a_monitor_that_does_not_report_the_nuvoton_vendor_id_is_rejected() {
        let mut fixture = Fixture::new();
        nuvoton_slot(&mut fixture, PRIMARY, 0xC5, 0x62, 0x0A20, 0xFFFF, 0x00);
        empty_slot(&mut fixture, SECONDARY);
        let (detection, _) = run(fixture);
        let [Detected::Unsupported { reason, name, .. }] = detection.chips.as_slice() else {
            panic!("expected one unsupported chip, got {:?}", detection.chips);
        };
        assert_eq!(name.as_deref(), Some("NCT6779D"));
        assert_eq!(reason, "(nct67xx): expected vendor id 0x5ca3 | received: 0xffff");
    }

    #[test]
    fn a_set_io_space_lock_is_reported_and_never_cleared() {
        let mut fixture = Fixture::new();
        nuvoton_slot(&mut fixture, PRIMARY, 0xD3, 0x52, 0x0A20, 0xFFFF, 0x10);
        empty_slot(&mut fixture, SECONDARY);
        let (detection, writes) = run(fixture);
        let [Detected::Unsupported { reason, .. }] = detection.chips.as_slice() else {
            panic!("expected one unsupported chip, got {:?}", detection.chips);
        };
        assert!(
            reason.contains("I/O space lock (configuration register 0x28 bit 4) is set"),
            "{reason}"
        );
        assert!(reason.contains("does not clear it"), "{reason}");
        // The only logical-device write is the monitor select; nothing else is written to configuration space.
        let config_writes =
            writes.iter().filter(|w| matches!(w, SuperIoWrite::LogicalDevice(_))).count();
        assert_eq!(config_writes, 1);
    }

    #[test]
    fn the_lock_bit_only_counts_on_chips_that_have_the_lock() {
        // An NCT6779D has no lock: bit 4 of 0x28 means nothing there.
        let mut fixture = Fixture::new();
        nuvoton_slot(&mut fixture, PRIMARY, 0xC5, 0x62, 0x0A20, 0xFFFF, 0x10);
        empty_slot(&mut fixture, SECONDARY);
        let (detection, _) = run(fixture);
        let [Detected::Unsupported { reason, .. }] = detection.chips.as_slice() else {
            panic!("expected one unsupported chip");
        };
        assert!(!reason.contains("lock"), "{reason}");
    }

    #[test]
    fn an_unstable_base_address_is_rejected_naming_both_reads() {
        assert_eq!(
            base_problem(0x0A20, 0x0A24),
            Some(
                "(superio): expected a stable monitor base address | received: 0x0a20 then 0x0a24"
                    .to_owned()
            )
        );
    }

    #[test]
    fn base_addresses_below_0x100_or_with_stray_bits_are_rejected() {
        assert_eq!(base_problem(0x0A20, 0x0A20), None);
        assert_eq!(base_problem(0x0290, 0x0290), None);
        for bad in [0x0000u16, 0x00FF, 0x0A21, 0x0A25, 0x1A20, 0xFFFF] {
            let reason = base_problem(bad, bad).unwrap_or_else(|| panic!("{bad:#x} was accepted"));
            assert!(reason.contains(&format!("received: {bad:#06x}")), "{reason}");
        }
    }

    #[test]
    fn an_ite_chip_needing_bank_select_is_named_and_not_read() {
        let mut fixture = Fixture::new();
        ite_slot(&mut fixture, PRIMARY, 0x8665, 0x0290, 0x90, 0x1B);
        empty_slot(&mut fixture, SECONDARY);
        let (detection, writes) = run(fixture);
        let [Detected::Unsupported { name, reason, .. }] = detection.chips.as_slice() else {
            panic!("expected one unsupported chip");
        };
        assert_eq!(name.as_deref(), Some("IT8665E"));
        assert!(reason.contains("bank-select register (0x06)"), "{reason}");
        assert!(!writes.iter().any(|w| matches!(w, SuperIoWrite::DiscoverRanges)));
        assert!(writes.contains(&SuperIoWrite::Exit(Unlock::Ite)));
    }

    #[test]
    fn an_unknown_ite_id_is_reported_with_its_raw_id() {
        let mut fixture = Fixture::new();
        ite_slot(&mut fixture, PRIMARY, 0x8883, 0x0290, 0x90, 0x1B);
        empty_slot(&mut fixture, SECONDARY);
        let (detection, _) = run(fixture);
        let [Detected::Unsupported { raw_id, reason, .. }] = detection.chips.as_slice() else {
            panic!("expected one unsupported chip");
        };
        assert_eq!(*raw_id, 0x8883);
        assert_eq!(reason, "(it87xx): expected a known ITE chip id | received: 0x8883");
    }

    #[test]
    fn an_ite_monitor_with_a_foreign_vendor_id_is_rejected() {
        let mut fixture = Fixture::new();
        ite_slot(&mut fixture, PRIMARY, 0x8686, 0x0290, 0x23, 0x1B);
        empty_slot(&mut fixture, SECONDARY);
        let (detection, _) = run(fixture);
        let [Detected::Unsupported { reason, .. }] = detection.chips.as_slice() else {
            panic!("expected one unsupported chip");
        };
        assert_eq!(reason, "(it87xx): expected vendor id 0x90 or 0x7f | received: 0x23");
    }

    #[test]
    fn a_bus_error_ends_the_scan_and_is_reported() {
        // The primary slot has nothing recorded for its chip ID.
        let mut fixture = Fixture::new();
        fixture.set(superio_unlock_key(PRIMARY), unlock_code(Unlock::WinbondNuvoton));
        empty_slot(&mut fixture, SECONDARY);
        let (detection, writes) = run(fixture);
        assert!(detection.chips.is_empty());
        assert_eq!(detection.problems.len(), 1, "{:?}", detection.problems);
        assert!(detection.problems[0].starts_with("ports 0x2e/0x2f: "), "{:?}", detection.problems);
        assert!(
            !writes.contains(&SuperIoWrite::SelectSlot(SECONDARY)),
            "the second slot is not touched after an error"
        );
        assert!(
            writes.contains(&SuperIoWrite::Exit(Unlock::WinbondNuvoton)),
            "configuration mode is left even though the read failed: {writes:?}"
        );
    }

    #[test]
    fn the_guard_leaves_configuration_mode_when_dropped_early() {
        let mut fixture = Fixture::new();
        empty_slot(&mut fixture, PRIMARY);
        let mut io = ReplaySuperIo::new(fixture);
        let log = io.write_log();
        io.select_slot(PRIMARY).unwrap();
        {
            let mut mode = ConfigMode::enter(&mut io, Unlock::WinbondNuvoton).unwrap();
            let _ = mode.config_read(0x20);
            // dropped without `leave`
        }
        assert_eq!(
            *log.lock().unwrap(),
            [
                SuperIoWrite::SelectSlot(PRIMARY),
                SuperIoWrite::Enter(Unlock::WinbondNuvoton),
                SuperIoWrite::Exit(Unlock::WinbondNuvoton)
            ]
        );
    }

    #[test]
    fn an_abandoned_guard_writes_no_leave_sequence() {
        let mut fixture = Fixture::new();
        empty_slot(&mut fixture, PRIMARY);
        let mut io = ReplaySuperIo::new(fixture);
        let log = io.write_log();
        io.select_slot(PRIMARY).unwrap();
        let mut mode = ConfigMode::enter(&mut io, Unlock::Ite).unwrap();
        mode.abandon();
        drop(mode);
        assert_eq!(
            *log.lock().unwrap(),
            [SuperIoWrite::SelectSlot(PRIMARY), SuperIoWrite::Enter(Unlock::Ite)]
        );
    }

    #[test]
    fn an_explicit_leave_writes_the_sequence_once_and_the_drop_adds_nothing() {
        let mut fixture = Fixture::new();
        empty_slot(&mut fixture, PRIMARY);
        let mut io = ReplaySuperIo::new(fixture);
        let log = io.write_log();
        io.select_slot(PRIMARY).unwrap();
        let mode = ConfigMode::enter(&mut io, Unlock::WinbondNuvoton).unwrap();
        mode.leave().unwrap();
        let exits = log
            .lock()
            .unwrap()
            .iter()
            .filter(|write| matches!(write, SuperIoWrite::Exit(_)))
            .count();
        assert_eq!(exits, 1, "(config mode): expected one exit sequence | received: {exits}");
    }

    #[test]
    fn a_failed_explicit_leave_is_reported_to_the_caller() {
        let mut fixture = Fixture::new();
        empty_slot(&mut fixture, PRIMARY);
        let mut io = ReplaySuperIo::new(fixture);
        io.select_slot(PRIMARY).unwrap();
        let mut mode = ConfigMode::enter(&mut io, Unlock::WinbondNuvoton).unwrap();
        // Leave behind the guard's back, so the guard's own leave finds the
        // chip already out of configuration mode.
        mode.exit_config_mode(Unlock::WinbondNuvoton).unwrap();
        let error = mode.leave().expect_err("a second exit must be rejected");
        assert_eq!(
            error.to_string(),
            "(replay superio exit_config_mode): expected configuration mode to be active | \
             received: not in configuration mode"
        );
    }
}
