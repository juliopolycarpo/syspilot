//! Replay fakes and recorders for the bus traits.
//!
//! A *recorder* wraps a real bus and stores every value it returns in a
//! [`Fixture`]; a *replay* fake serves those values back. Provider tests run
//! against replays, so the decoders are checked against what real hardware
//! returned, on any machine and without the driver.
//!
//! The replays are strict. They model the protocol the real chips follow
//! and refuse everything outside it, naming the rule that was broken:
//!
//! - [`ReplaySuperIo`]: configuration registers only inside configuration
//!   mode, logical-device and bank writes only in the sequences the real
//!   implementation allows, I/O ranges discovered before the monitor ports
//!   are used, configuration mode left before another slot is selected.
//! - [`ReplaySmbus`]: reads from the recording, one write that must be an
//!   SPD page select, addresses absent from the recording do not
//!   acknowledge.
//! - [`ReplayMsr`], [`ReplaySmn`]: reads only; an unrecorded register is an
//!   error, never a zero.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

use syspilot_pawnio::Error as DriverError;

use super::{
    BusError, Fixture, LogicalCpu, Msr, SPD_PAGE_SELECT_ADDRESSES, Smbus, Smn, SuperIo,
    SuperIoSlot, Unlock, is_permitted_bank,
};

/// A fixture shared between a recorder and the code that saves it.
pub type SharedFixture = Arc<Mutex<Fixture>>;

fn record(fixture: &SharedFixture, key: String, value: u64) {
    if let Ok(mut fixture) = fixture.lock() {
        fixture.set(key, value);
    }
}

fn slot_name(slot: SuperIoSlot) -> &'static str {
    match slot {
        SuperIoSlot::Primary => "primary",
        SuperIoSlot::Secondary => "secondary",
    }
}

// ---------------------------------------------------------------------------
// MSR
// ---------------------------------------------------------------------------

/// Fixture key of one MSR on one logical processor.
pub fn msr_key(cpu: LogicalCpu, register: u32) -> String {
    format!("msr.{}.{}.{register:#x}", cpu.group, cpu.index)
}

/// Serves MSR reads from recorded values or scripted sequences.
///
/// A register with several queued values returns them one per read and then
/// keeps returning the last one, which is how tests step an energy counter
/// through several samples.
#[derive(Debug, Default)]
pub struct ReplayMsr {
    values: BTreeMap<(LogicalCpu, u32), VecDeque<u64>>,
    reads: Arc<Mutex<Vec<(LogicalCpu, u32)>>>,
}

impl ReplayMsr {
    /// An empty replay: every read fails until values are added.
    pub fn new() -> Self {
        Self::default()
    }

    /// A replay of every `msr.*` entry of `fixture`.
    pub fn from_fixture(fixture: &Fixture) -> Self {
        let mut replay = Self::new();
        for (key, value) in fixture.entries() {
            let Some(rest) = key.strip_prefix("msr.") else { continue };
            let mut parts = rest.split('.');
            let (Some(group), Some(index), Some(register)) =
                (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let (Ok(group), Ok(index)) = (group.parse(), index.parse()) else { continue };
            let Some(register) =
                register.strip_prefix("0x").and_then(|hex| u32::from_str_radix(hex, 16).ok())
            else {
                continue;
            };
            replay.set(LogicalCpu { group, index }, register, value);
        }
        replay
    }

    /// Serves `value` for every read of `register` on `cpu`.
    pub fn set(&mut self, cpu: LogicalCpu, register: u32, value: u64) -> &mut Self {
        self.values.insert((cpu, register), VecDeque::from([value]));
        self
    }

    /// Serves `values` in order, then repeats the last one.
    pub fn set_sequence(&mut self, cpu: LogicalCpu, register: u32, values: &[u64]) -> &mut Self {
        self.values.insert((cpu, register), values.iter().copied().collect());
        self
    }

    /// Makes `register` on `cpu` unreadable again, as if it had not been
    /// recorded.
    pub fn remove(&mut self, cpu: LogicalCpu, register: u32) -> &mut Self {
        self.values.remove(&(cpu, register));
        self
    }

    /// Handle to the log of every `(cpu, register)` read, in order. Stays
    /// valid after the replay moves into a provider.
    pub fn read_log(&self) -> Arc<Mutex<Vec<(LogicalCpu, u32)>>> {
        self.reads.clone()
    }
}

impl Msr for ReplayMsr {
    fn read(&mut self, cpu: LogicalCpu, register: u32) -> Result<u64, BusError> {
        if let Ok(mut reads) = self.reads.lock() {
            reads.push((cpu, register));
        }
        let queue = self.values.get_mut(&(cpu, register)).ok_or_else(|| {
            BusError::rejected(
                "replay msr",
                format!("a recorded value for `{}`", msr_key(cpu, register)),
                "no recording",
            )
        })?;
        let value = if queue.len() > 1 { queue.pop_front() } else { queue.front().copied() };
        value.ok_or_else(|| BusError::rejected("replay msr", "a queued value", "an empty queue"))
    }
}

/// Wraps a real [`Msr`] and records what it returns.
pub struct RecordingMsr<M> {
    inner: M,
    fixture: SharedFixture,
}

impl<M: Msr> RecordingMsr<M> {
    /// Records into `fixture`.
    pub fn new(inner: M, fixture: SharedFixture) -> Self {
        Self { inner, fixture }
    }
}

impl<M: Msr> Msr for RecordingMsr<M> {
    fn read(&mut self, cpu: LogicalCpu, register: u32) -> Result<u64, BusError> {
        let value = self.inner.read(cpu, register)?;
        record(&self.fixture, msr_key(cpu, register), value);
        Ok(value)
    }
}

// ---------------------------------------------------------------------------
// SMN
// ---------------------------------------------------------------------------

/// Fixture key of one SMN register.
pub fn smn_key(address: u32) -> String {
    format!("smn.{address:#x}")
}

/// Serves SMN reads from recorded values.
#[derive(Debug, Default)]
pub struct ReplaySmn {
    values: BTreeMap<u32, VecDeque<u32>>,
}

impl ReplaySmn {
    /// An empty replay.
    pub fn new() -> Self {
        Self::default()
    }

    /// Serves `value` for every read of `address`.
    pub fn set(&mut self, address: u32, value: u32) -> &mut Self {
        self.values.insert(address, VecDeque::from([value]));
        self
    }

    /// Serves `values` in order, then repeats the last one.
    pub fn set_sequence(&mut self, address: u32, values: &[u32]) -> &mut Self {
        self.values.insert(address, values.iter().copied().collect());
        self
    }

    /// A replay of every `smn.*` entry of `fixture`.
    pub fn from_fixture(fixture: &Fixture) -> Self {
        let mut replay = Self::new();
        for (key, value) in fixture.entries() {
            let address = key
                .strip_prefix("smn.0x")
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .zip(u32::try_from(value).ok());
            if let Some((address, value)) = address {
                replay.set(address, value);
            }
        }
        replay
    }
}

impl Smn for ReplaySmn {
    fn read(&mut self, address: u32) -> Result<u32, BusError> {
        let queue = self.values.get_mut(&address).ok_or_else(|| {
            BusError::rejected(
                "replay smn",
                format!("a recorded value for `{}`", smn_key(address)),
                "no recording",
            )
        })?;
        let value = if queue.len() > 1 { queue.pop_front() } else { queue.front().copied() };
        value.ok_or_else(|| BusError::rejected("replay smn", "a queued value", "an empty queue"))
    }
}

/// Wraps a real [`Smn`] and records what it returns.
pub struct RecordingSmn<S> {
    inner: S,
    fixture: SharedFixture,
}

impl<S: Smn> RecordingSmn<S> {
    /// Records into `fixture`.
    pub fn new(inner: S, fixture: SharedFixture) -> Self {
        Self { inner, fixture }
    }
}

impl<S: Smn> Smn for RecordingSmn<S> {
    fn read(&mut self, address: u32) -> Result<u32, BusError> {
        let value = self.inner.read(address)?;
        record(&self.fixture, smn_key(address), u64::from(value));
        Ok(value)
    }
}

// ---------------------------------------------------------------------------
// Super I/O
// ---------------------------------------------------------------------------

/// Configuration registers below this are global; the rest belong to the
/// selected logical device.
const FIRST_DEVICE_REGISTER: u8 = 0x30;

/// Fixture key of a configuration register. Global registers (`< 0x30`) do
/// not depend on the logical device.
pub fn superio_config_key(slot: SuperIoSlot, logical_device: u8, register: u8) -> String {
    let scope = if register < FIRST_DEVICE_REGISTER {
        "g".to_owned()
    } else {
        format!("{logical_device:#x}")
    };
    format!("superio.{}.cfg.{scope}.{register:#x}", slot_name(slot))
}

/// Fixture key of a hardware-monitor register in `bank`.
pub fn superio_hwm_key(slot: SuperIoSlot, bank: u8, index: u8) -> String {
    format!("superio.{}.hwm.{bank:#x}.{index:#x}", slot_name(slot))
}

/// Fixture key of the unlock sequence the chip on `slot` answers to. The
/// value is [`unlock_code`] of that sequence, or `0` when nothing answers.
/// A slot without this entry answers to every sequence.
pub fn superio_unlock_key(slot: SuperIoSlot) -> String {
    format!("superio.{}.unlock", slot_name(slot))
}

/// The fixture value of an unlock sequence.
pub fn unlock_code(unlock: Unlock) -> u64 {
    match unlock {
        Unlock::WinbondNuvoton => 1,
        Unlock::Ite => 2,
    }
}

/// A write a [`ReplaySuperIo`] accepted, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuperIoWrite {
    /// The slot was selected.
    SelectSlot(SuperIoSlot),
    /// Configuration mode was entered.
    Enter(Unlock),
    /// Configuration mode was left.
    Exit(Unlock),
    /// The logical device register was written.
    LogicalDevice(u8),
    /// The driver was asked to discover the I/O ranges.
    DiscoverRanges,
    /// The hardware-monitor address port was written.
    HwmIndex {
        /// Base of the port block.
        base: u16,
        /// The index.
        index: u8,
    },
    /// The hardware-monitor bank was selected.
    HwmBank {
        /// Base of the port block.
        base: u16,
        /// The bank.
        bank: u8,
    },
}

/// A Super I/O chip replayed from a recording, enforcing the protocol.
#[derive(Debug)]
pub struct ReplaySuperIo {
    fixture: Fixture,
    slot: Option<SuperIoSlot>,
    config_mode: Option<Unlock>,
    logical_device: u8,
    ranges_discovered: bool,
    hwm_index: u8,
    hwm_bank: u8,
    bank_select_armed: bool,
    writes: Arc<Mutex<Vec<SuperIoWrite>>>,
}

impl ReplaySuperIo {
    /// Replays `fixture`.
    pub fn new(fixture: Fixture) -> Self {
        Self {
            fixture,
            slot: None,
            config_mode: None,
            logical_device: 0,
            ranges_discovered: false,
            hwm_index: 0,
            hwm_bank: 0,
            bank_select_armed: false,
            writes: Arc::default(),
        }
    }

    /// Handle to the accepted writes. Stays valid after the replay moves
    /// into a provider.
    pub fn write_log(&self) -> Arc<Mutex<Vec<SuperIoWrite>>> {
        self.writes.clone()
    }

    fn log(&self, write: SuperIoWrite) {
        if let Ok(mut writes) = self.writes.lock() {
            writes.push(write);
        }
    }

    fn slot(&self, operation: &'static str) -> Result<SuperIoSlot, BusError> {
        self.slot.ok_or_else(|| {
            BusError::rejected(
                format!("replay superio {operation}"),
                "select_slot first",
                "no slot selected",
            )
        })
    }

    /// Whether a chip is in configuration mode: the unlock sequence was
    /// written and it is the one the chip on this slot answers to. A chip
    /// ignores a sequence that is not its own, so writing one changes nothing.
    fn chip_in_config_mode(&self) -> bool {
        let (Some(entered), Some(slot)) = (self.config_mode, self.slot) else { return false };
        match self.fixture.get(&superio_unlock_key(slot)) {
            None => true,
            Some(code) => code == unlock_code(entered),
        }
    }

    fn require_config_mode(&self, operation: &'static str) -> Result<SuperIoSlot, BusError> {
        let slot = self.slot(operation)?;
        if self.config_mode.is_none() {
            return Err(BusError::rejected(
                format!("replay superio {operation}"),
                "configuration mode (the chip ignores configuration registers outside it)",
                "not in configuration mode",
            ));
        }
        Ok(slot)
    }

    /// The driver only allows port access inside ranges it discovered.
    fn require_discovered_ranges(&self, port: u16) -> Result<(), BusError> {
        if self.ranges_discovered {
            return Ok(());
        }
        Err(BusError::Driver(DriverError::ArgumentDenied {
            function: "ioctl_pio_outb".into(),
            argument: Some(u64::from(port)),
        }))
    }

    fn recorded(&self, key: String) -> Result<u8, BusError> {
        self.fixture.get(&key).map(|value| value as u8).ok_or_else(|| {
            BusError::rejected(
                "replay superio read",
                format!("a recorded value for `{key}`"),
                "no recording",
            )
        })
    }
}

impl SuperIo for ReplaySuperIo {
    fn select_slot(&mut self, slot: SuperIoSlot) -> Result<(), BusError> {
        if self.chip_in_config_mode() {
            return Err(BusError::rejected(
                "replay superio select_slot",
                "configuration mode left before another slot is selected",
                "still in configuration mode",
            ));
        }
        // An unlock sequence no chip answered to leaves nothing behind.
        self.config_mode = None;
        self.slot = Some(slot);
        self.ranges_discovered = false;
        self.bank_select_armed = false;
        self.hwm_bank = 0;
        self.log(SuperIoWrite::SelectSlot(slot));
        Ok(())
    }

    fn enter_config_mode(&mut self, unlock: Unlock) -> Result<(), BusError> {
        self.bank_select_armed = false;
        self.slot("enter_config_mode")?;
        if self.chip_in_config_mode() {
            return Err(BusError::rejected(
                "replay superio enter_config_mode",
                "configuration mode not yet entered",
                "already in configuration mode",
            ));
        }
        self.config_mode = Some(unlock);
        self.log(SuperIoWrite::Enter(unlock));
        Ok(())
    }

    fn exit_config_mode(&mut self, unlock: Unlock) -> Result<(), BusError> {
        self.bank_select_armed = false;
        match self.config_mode {
            Some(entered) if entered == unlock => {
                self.config_mode = None;
                self.log(SuperIoWrite::Exit(unlock));
                Ok(())
            }
            Some(entered) => Err(BusError::rejected(
                "replay superio exit_config_mode",
                format!("the exit sequence of {entered:?}"),
                format!("the exit sequence of {unlock:?}"),
            )),
            None => Err(BusError::rejected(
                "replay superio exit_config_mode",
                "configuration mode to be active",
                "not in configuration mode",
            )),
        }
    }

    fn select_logical_device(&mut self, device: u8) -> Result<(), BusError> {
        self.bank_select_armed = false;
        self.require_config_mode("select_logical_device")?;
        self.logical_device = device;
        self.log(SuperIoWrite::LogicalDevice(device));
        Ok(())
    }

    fn config_read(&mut self, register: u8) -> Result<u8, BusError> {
        self.bank_select_armed = false;
        let slot = self.require_config_mode("config_read")?;
        if !self.chip_in_config_mode() {
            // No chip is listening: the data port floats high.
            return Ok(0xFF);
        }
        self.recorded(superio_config_key(slot, self.logical_device, register))
    }

    fn discover_io_ranges(&mut self) -> Result<(), BusError> {
        self.bank_select_armed = false;
        self.require_config_mode("discover_io_ranges")?;
        self.ranges_discovered = true;
        // The real driver selects every logical device while scanning.
        self.logical_device = 0xFF;
        self.log(SuperIoWrite::DiscoverRanges);
        Ok(())
    }

    fn hwm_set_index(&mut self, base: u16, index: u8) -> Result<(), BusError> {
        self.bank_select_armed = false;
        self.slot("hwm_set_index")?;
        self.require_discovered_ranges(base.wrapping_add(5))?;
        self.hwm_index = index;
        self.bank_select_armed = index == 0x4E;
        self.log(SuperIoWrite::HwmIndex { base, index });
        Ok(())
    }

    fn hwm_write_bank(&mut self, base: u16, bank: u8) -> Result<(), BusError> {
        let armed = std::mem::take(&mut self.bank_select_armed);
        self.slot("hwm_write_bank")?;
        self.require_discovered_ranges(base.wrapping_add(6))?;
        if !armed {
            return Err(BusError::rejected(
                "replay superio hwm_write_bank",
                "a write directly after selecting index 0x4e",
                "the data port written without the bank-select index",
            ));
        }
        if !is_permitted_bank(bank) {
            return Err(BusError::rejected(
                "replay superio hwm_write_bank",
                "a bank number 0..=0x0f, or 0x80 for the high byte of 16-bit registers",
                format!("bank {bank:#x}"),
            ));
        }
        self.hwm_bank = bank;
        self.log(SuperIoWrite::HwmBank { base, bank });
        Ok(())
    }

    fn hwm_read_index(&mut self, base: u16) -> Result<u8, BusError> {
        self.bank_select_armed = false;
        self.slot("hwm_read_index")?;
        self.require_discovered_ranges(base.wrapping_add(5))?;
        Ok(self.hwm_index)
    }

    fn hwm_read_data(&mut self, base: u16) -> Result<u8, BusError> {
        self.bank_select_armed = false;
        let slot = self.slot("hwm_read_data")?;
        self.require_discovered_ranges(base.wrapping_add(6))?;
        self.recorded(superio_hwm_key(slot, self.hwm_bank, self.hwm_index))
    }
}

/// Wraps a real [`SuperIo`] and records the registers it returns.
pub struct RecordingSuperIo<S> {
    inner: S,
    fixture: SharedFixture,
    slot: SuperIoSlot,
    /// The unlock sequence last written, to attribute a chip ID to it.
    unlock: Option<Unlock>,
    logical_device: u8,
    hwm_index: u8,
    hwm_bank: u8,
    bank_select_armed: bool,
}

impl<S: SuperIo> RecordingSuperIo<S> {
    /// Records into `fixture`.
    pub fn new(inner: S, fixture: SharedFixture) -> Self {
        Self {
            inner,
            fixture,
            slot: SuperIoSlot::Primary,
            unlock: None,
            logical_device: 0,
            hwm_index: 0,
            hwm_bank: 0,
            bank_select_armed: false,
        }
    }
}

impl<S: SuperIo> SuperIo for RecordingSuperIo<S> {
    fn select_slot(&mut self, slot: SuperIoSlot) -> Result<(), BusError> {
        self.inner.select_slot(slot)?;
        self.slot = slot;
        self.hwm_bank = 0;
        self.bank_select_armed = false;
        // Until a chip ID is read on this slot, nothing is known to answer.
        if let Ok(mut fixture) = self.fixture.lock() {
            let key = superio_unlock_key(slot);
            if fixture.get(&key).is_none() {
                fixture.set(key, 0);
            }
        }
        Ok(())
    }

    fn enter_config_mode(&mut self, unlock: Unlock) -> Result<(), BusError> {
        self.bank_select_armed = false;
        self.unlock = Some(unlock);
        self.inner.enter_config_mode(unlock)
    }

    fn exit_config_mode(&mut self, unlock: Unlock) -> Result<(), BusError> {
        self.bank_select_armed = false;
        self.unlock = None;
        self.inner.exit_config_mode(unlock)
    }

    fn select_logical_device(&mut self, device: u8) -> Result<(), BusError> {
        self.bank_select_armed = false;
        self.inner.select_logical_device(device)?;
        self.logical_device = device;
        Ok(())
    }

    fn config_read(&mut self, register: u8) -> Result<u8, BusError> {
        self.bank_select_armed = false;
        let value = self.inner.config_read(register)?;
        record(
            &self.fixture,
            superio_config_key(self.slot, self.logical_device, register),
            u64::from(value),
        );
        // A chip ID (register 0x20) other than "nothing there" tells which
        // unlock sequence the chip on this slot answers to.
        if let (0x20, Some(unlock), 0x01..=0xFE) = (register, self.unlock, value) {
            record(&self.fixture, superio_unlock_key(self.slot), unlock_code(unlock));
        }
        Ok(value)
    }

    fn discover_io_ranges(&mut self) -> Result<(), BusError> {
        self.bank_select_armed = false;
        self.inner.discover_io_ranges()?;
        // The driver selects every logical device while scanning.
        self.logical_device = 0xFF;
        Ok(())
    }

    fn hwm_set_index(&mut self, base: u16, index: u8) -> Result<(), BusError> {
        self.inner.hwm_set_index(base, index)?;
        self.hwm_index = index;
        self.bank_select_armed = index == 0x4E;
        Ok(())
    }

    fn hwm_write_bank(&mut self, base: u16, bank: u8) -> Result<(), BusError> {
        let armed = std::mem::take(&mut self.bank_select_armed);
        self.inner.hwm_write_bank(base, bank)?;
        if armed {
            self.hwm_bank = bank;
        }
        Ok(())
    }

    fn hwm_read_index(&mut self, base: u16) -> Result<u8, BusError> {
        self.bank_select_armed = false;
        self.inner.hwm_read_index(base)
    }

    fn hwm_read_data(&mut self, base: u16) -> Result<u8, BusError> {
        self.bank_select_armed = false;
        let value = self.inner.hwm_read_data(base)?;
        record(
            &self.fixture,
            superio_hwm_key(self.slot, self.hwm_bank, self.hwm_index),
            u64::from(value),
        );
        Ok(value)
    }
}

// ---------------------------------------------------------------------------
// SMBus
// ---------------------------------------------------------------------------

/// Whether `address` is an SPD EEPROM address, whose contents depend on the
/// selected page.
fn is_spd_address(address: u8) -> bool {
    (0x50..=0x57).contains(&address)
}

/// Fixture key of a byte read. `page` matters only for SPD addresses.
pub fn smbus_byte_key(address: u8, page: u8, command: u8) -> String {
    let page = if is_spd_address(address) { page } else { 0 };
    format!("smbus.byte.{address:#x}.{page}.{command:#x}")
}

/// Fixture key of a word read.
pub fn smbus_word_key(address: u8, command: u8) -> String {
    format!("smbus.word.{address:#x}.{command:#x}")
}

/// An SMBus replayed from a recording.
///
/// Addresses with no entry do not acknowledge, exactly like empty DIMM
/// slots. The only write is the SPD page select.
#[derive(Debug)]
pub struct ReplaySmbus {
    fixture: Fixture,
    present: BTreeSet<u8>,
    page: u8,
    page_select_acknowledged: bool,
    page_selects: Arc<Mutex<Vec<u8>>>,
}

impl ReplaySmbus {
    /// Replays `fixture`.
    pub fn new(fixture: Fixture) -> Self {
        let present = fixture
            .entries()
            .filter_map(|(key, _)| {
                let rest =
                    key.strip_prefix("smbus.byte.").or_else(|| key.strip_prefix("smbus.word."))?;
                let address = rest.split('.').next()?.strip_prefix("0x")?;
                u8::from_str_radix(address, 16).ok()
            })
            .collect();
        Self {
            fixture,
            present,
            page: 0,
            page_select_acknowledged: true,
            page_selects: Arc::default(),
        }
    }

    /// Makes the page-select addresses not acknowledge, as some controllers
    /// and modules behave. The page still changes.
    pub fn without_page_select_acknowledge(mut self) -> Self {
        self.page_select_acknowledged = false;
        self
    }

    /// Handle to the page numbers selected so far, in order.
    pub fn page_select_log(&self) -> Arc<Mutex<Vec<u8>>> {
        self.page_selects.clone()
    }

    fn no_device(address: u8) -> BusError {
        BusError::Driver(DriverError::NoSuchDevice {
            function: format!("replay smbus {address:#04x}"),
        })
    }

    fn recorded(&self, address: u8, key: String) -> Result<u64, BusError> {
        if !self.present.contains(&address) {
            return Err(Self::no_device(address));
        }
        self.fixture.get(&key).ok_or_else(|| {
            BusError::rejected(
                "replay smbus read",
                format!("a recorded value for `{key}`"),
                "no recording",
            )
        })
    }
}

impl Smbus for ReplaySmbus {
    fn read_byte_data(&mut self, address: u8, command: u8) -> Result<u8, BusError> {
        let key = smbus_byte_key(address, self.page, command);
        Ok(self.recorded(address, key)? as u8)
    }

    fn read_word_data(&mut self, address: u8, command: u8) -> Result<u16, BusError> {
        Ok(self.recorded(address, smbus_word_key(address, command))? as u16)
    }

    fn select_spd_page(&mut self, page: u8) -> Result<(), BusError> {
        let target =
            SPD_PAGE_SELECT_ADDRESSES.get(usize::from(page)).copied().ok_or_else(|| {
                BusError::rejected(
                    "replay smbus write",
                    "an SPD page select (page 0 or 1, target 0x36 or 0x37)",
                    format!("page {page}"),
                )
            })?;
        if let Ok(mut selects) = self.page_selects.lock() {
            selects.push(page);
        }
        self.page = page;
        if self.page_select_acknowledged { Ok(()) } else { Err(Self::no_device(target)) }
    }
}

/// Wraps a real [`Smbus`] and records the values it returns. Addresses that
/// do not acknowledge are not recorded, so a replay reproduces them as empty.
pub struct RecordingSmbus<B> {
    inner: B,
    fixture: SharedFixture,
    page: u8,
}

impl<B: Smbus> RecordingSmbus<B> {
    /// Records into `fixture`.
    pub fn new(inner: B, fixture: SharedFixture) -> Self {
        Self { inner, fixture, page: 0 }
    }
}

impl<B: Smbus> Smbus for RecordingSmbus<B> {
    fn read_byte_data(&mut self, address: u8, command: u8) -> Result<u8, BusError> {
        let value = self.inner.read_byte_data(address, command)?;
        record(&self.fixture, smbus_byte_key(address, self.page, command), u64::from(value));
        Ok(value)
    }

    fn read_word_data(&mut self, address: u8, command: u8) -> Result<u16, BusError> {
        let value = self.inner.read_word_data(address, command)?;
        record(&self.fixture, smbus_word_key(address, command), u64::from(value));
        Ok(value)
    }

    fn select_spd_page(&mut self, page: u8) -> Result<(), BusError> {
        let outcome = self.inner.select_spd_page(page);
        // A page select that was not acknowledged may still have switched the
        // page; the recording follows what was asked for, as the reads after
        // it do.
        if outcome.is_ok() || outcome.as_ref().is_err_and(BusError::is_no_device) {
            self.page = page;
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CPU0: LogicalCpu = LogicalCpu { group: 0, index: 0 };
    const CPU1: LogicalCpu = LogicalCpu { group: 0, index: 2 };

    // -- MSR ---------------------------------------------------------------

    #[test]
    fn msr_replay_serves_the_recorded_value_per_processor() {
        let mut replay = ReplayMsr::new();
        replay.set(CPU0, 0x19C, 0x8830_0000).set(CPU1, 0x19C, 0x8828_0000);
        assert_eq!(replay.read(CPU0, 0x19C), Ok(0x8830_0000));
        assert_eq!(replay.read(CPU1, 0x19C), Ok(0x8828_0000));
        assert_eq!(replay.read(CPU0, 0x19C), Ok(0x8830_0000), "a single value is sticky");
    }

    #[test]
    fn an_unrecorded_msr_is_an_error_naming_the_key() {
        let mut replay = ReplayMsr::new();
        let error = replay.read(CPU1, 0x611).unwrap_err();
        assert_eq!(
            error.to_string(),
            "(replay msr): expected a recorded value for `msr.0.2.0x611` | received: no recording"
        );
    }

    #[test]
    fn a_msr_sequence_steps_then_repeats_the_last_value() {
        let mut replay = ReplayMsr::new();
        replay.set_sequence(CPU0, 0x611, &[10, 20, 30]);
        let seen: Vec<_> = (0..5).map(|_| replay.read(CPU0, 0x611).unwrap()).collect();
        assert_eq!(seen, [10, 20, 30, 30, 30]);
    }

    #[test]
    fn a_removed_msr_is_unreadable_again() {
        let mut replay = ReplayMsr::new();
        replay.set(CPU0, 0x19C, 1).set(CPU1, 0x19C, 2);
        replay.remove(CPU0, 0x19C);
        assert!(replay.read(CPU0, 0x19C).is_err());
        assert_eq!(replay.read(CPU1, 0x19C), Ok(2));
    }

    #[test]
    fn the_msr_read_log_survives_moving_the_replay() {
        let mut replay = ReplayMsr::new();
        replay.set(CPU0, 0x606, 0xE0E);
        let log = replay.read_log();
        let mut boxed: Box<dyn Msr> = Box::new(replay);
        boxed.read(CPU0, 0x606).unwrap();
        assert_eq!(*log.lock().unwrap(), [(CPU0, 0x606)]);
    }

    #[test]
    fn msr_recording_replays_identically() {
        let mut source = ReplayMsr::new();
        source.set(CPU0, 0x1A2, 0x0064_0000).set(CPU1, 0x1A2, 0x0064_0000);
        let fixture: SharedFixture = Arc::default();
        let mut recorder = RecordingMsr::new(source, fixture.clone());
        recorder.read(CPU0, 0x1A2).unwrap();
        recorder.read(CPU1, 0x1A2).unwrap();
        // A failed read records nothing.
        assert!(recorder.read(CPU0, 0x10).is_err());

        let recorded = fixture.lock().unwrap().clone();
        assert_eq!(recorded.len(), 2);
        let mut replay = ReplayMsr::from_fixture(&recorded);
        assert_eq!(replay.read(CPU1, 0x1A2), Ok(0x0064_0000));
        assert!(replay.read(CPU0, 0x10).is_err());
    }

    // -- SMN ---------------------------------------------------------------

    #[test]
    fn smn_replay_round_trips_through_a_fixture() {
        let mut source = ReplaySmn::new();
        source.set(0x0005_9800, 0x1234_5678);
        let fixture: SharedFixture = Arc::default();
        let mut recorder = RecordingSmn::new(source, fixture.clone());
        assert_eq!(recorder.read(0x0005_9800), Ok(0x1234_5678));
        let mut replay = ReplaySmn::from_fixture(&fixture.lock().unwrap());
        assert_eq!(replay.read(0x0005_9800), Ok(0x1234_5678));
        let error = replay.read(0x0005_9804).unwrap_err();
        assert!(error.to_string().contains("smn.0x59804"), "{error}");
    }

    #[test]
    fn smn_sequence_steps_then_repeats() {
        let mut replay = ReplaySmn::new();
        replay.set_sequence(0x10, &[1, 2]);
        let seen: Vec<_> = (0..3).map(|_| replay.read(0x10).unwrap()).collect();
        assert_eq!(seen, [1, 2, 2]);
    }

    // -- Super I/O ---------------------------------------------------------

    /// A tiny chip: ID in the global registers, a hardware monitor in
    /// logical device 0x0B with base 0x0290, two banks of monitor registers.
    fn chip_fixture() -> Fixture {
        let mut fixture = Fixture::new();
        let primary = SuperIoSlot::Primary;
        fixture.set(superio_unlock_key(primary), unlock_code(Unlock::WinbondNuvoton));
        fixture.set(superio_config_key(primary, 0, 0x20), 0xC5);
        fixture.set(superio_config_key(primary, 0, 0x21), 0x60);
        fixture.set(superio_config_key(primary, 0x0B, 0x60), 0x02);
        fixture.set(superio_config_key(primary, 0x0B, 0x61), 0x90);
        fixture.set(superio_hwm_key(primary, 0, 0x73), 0x2B);
        fixture.set(superio_hwm_key(primary, 1, 0x73), 0x37);
        fixture
    }

    fn unlocked_replay() -> ReplaySuperIo {
        let mut chip = ReplaySuperIo::new(chip_fixture());
        chip.select_slot(SuperIoSlot::Primary).unwrap();
        chip.enter_config_mode(Unlock::WinbondNuvoton).unwrap();
        chip
    }

    #[test]
    fn config_registers_are_readable_only_in_config_mode() {
        let mut chip = ReplaySuperIo::new(chip_fixture());
        chip.select_slot(SuperIoSlot::Primary).unwrap();
        let error = chip.config_read(0x20).unwrap_err();
        assert!(error.to_string().contains("not in configuration mode"), "{error}");
        chip.enter_config_mode(Unlock::WinbondNuvoton).unwrap();
        assert_eq!(chip.config_read(0x20), Ok(0xC5));
        assert_eq!(chip.config_read_word(0x20), Ok(0xC560));
    }

    #[test]
    fn logical_device_registers_follow_the_selected_device() {
        let mut chip = unlocked_replay();
        chip.select_logical_device(0x0B).unwrap();
        assert_eq!(chip.config_read_word(0x60), Ok(0x0290));
        chip.select_logical_device(0x04).unwrap();
        assert!(chip.config_read(0x60).is_err(), "nothing was recorded for device 4");
    }

    #[test]
    fn discovery_and_monitor_reads_follow_the_protocol() {
        let mut chip = unlocked_replay();
        // The driver refuses the monitor ports before it discovered them.
        let denied = chip.hwm_set_index(0x0290, 0x73).unwrap_err();
        assert!(matches!(denied, BusError::Driver(DriverError::ArgumentDenied { .. })), "{denied}");

        chip.discover_io_ranges().unwrap();
        chip.exit_config_mode(Unlock::WinbondNuvoton).unwrap();
        chip.hwm_set_index(0x0290, 0x73).unwrap();
        assert_eq!(chip.hwm_read_index(0x0290), Ok(0x73));
        assert_eq!(chip.hwm_read_data(0x0290), Ok(0x2B));
    }

    #[test]
    fn the_bank_write_selects_the_bank_for_following_reads() {
        let mut chip = unlocked_replay();
        chip.discover_io_ranges().unwrap();
        chip.exit_config_mode(Unlock::WinbondNuvoton).unwrap();
        chip.hwm_set_index(0x0290, 0x4E).unwrap();
        chip.hwm_write_bank(0x0290, 1).unwrap();
        chip.hwm_set_index(0x0290, 0x73).unwrap();
        assert_eq!(chip.hwm_read_data(0x0290), Ok(0x37));
        chip.hwm_set_index(0x0290, 0x4E).unwrap();
        chip.hwm_write_bank(0x0290, 0).unwrap();
        chip.hwm_set_index(0x0290, 0x73).unwrap();
        assert_eq!(chip.hwm_read_data(0x0290), Ok(0x2B));
    }

    #[test]
    fn the_replay_accepts_the_high_byte_bank_and_refuses_other_values() {
        let mut chip = unlocked_replay();
        chip.discover_io_ranges().unwrap();
        for refused in [0x10, 0x81, 0xFF] {
            chip.hwm_set_index(0x0290, 0x4E).unwrap();
            let error = chip.hwm_write_bank(0x0290, refused).unwrap_err();
            assert!(error.to_string().contains(&format!("bank {refused:#x}")), "{error}");
        }
        chip.hwm_set_index(0x0290, 0x4E).unwrap();
        chip.hwm_write_bank(0x0290, 0x80).unwrap();
        assert_eq!(
            chip.write_log().lock().unwrap().last(),
            Some(&SuperIoWrite::HwmBank { base: 0x0290, bank: 0x80 })
        );
    }

    #[test]
    fn a_data_port_write_without_the_bank_index_is_rejected() {
        let mut chip = unlocked_replay();
        chip.discover_io_ranges().unwrap();
        chip.hwm_set_index(0x0290, 0x73).unwrap();
        let error = chip.hwm_write_bank(0x0290, 1).unwrap_err();
        assert!(matches!(error, BusError::Rejected { .. }), "{error}");
        // The rejected write changed nothing.
        assert_eq!(chip.hwm_read_data(0x0290), Ok(0x2B));
    }

    #[test]
    fn config_mode_must_be_left_before_another_slot_and_with_the_matching_sequence() {
        let mut chip = unlocked_replay();
        assert!(chip.select_slot(SuperIoSlot::Secondary).is_err());
        assert!(chip.enter_config_mode(Unlock::WinbondNuvoton).is_err(), "double enter");
        assert!(chip.exit_config_mode(Unlock::Ite).is_err(), "wrong exit sequence");
        chip.exit_config_mode(Unlock::WinbondNuvoton).unwrap();
        assert!(chip.exit_config_mode(Unlock::WinbondNuvoton).is_err(), "double exit");
        chip.select_slot(SuperIoSlot::Secondary).unwrap();
    }

    #[test]
    fn selecting_a_slot_forgets_the_discovered_ranges() {
        let mut chip = unlocked_replay();
        chip.discover_io_ranges().unwrap();
        chip.exit_config_mode(Unlock::WinbondNuvoton).unwrap();
        chip.select_slot(SuperIoSlot::Secondary).unwrap();
        assert!(chip.hwm_read_data(0x0290).is_err());
    }

    #[test]
    fn every_accepted_write_is_logged_in_order() {
        let mut chip = ReplaySuperIo::new(chip_fixture());
        let log = chip.write_log();
        chip.select_slot(SuperIoSlot::Primary).unwrap();
        chip.enter_config_mode(Unlock::WinbondNuvoton).unwrap();
        chip.select_logical_device(0x0B).unwrap();
        chip.discover_io_ranges().unwrap();
        chip.exit_config_mode(Unlock::WinbondNuvoton).unwrap();
        chip.hwm_set_index(0x0290, 0x4E).unwrap();
        chip.hwm_write_bank(0x0290, 0).unwrap();
        assert_eq!(
            *log.lock().unwrap(),
            [
                SuperIoWrite::SelectSlot(SuperIoSlot::Primary),
                SuperIoWrite::Enter(Unlock::WinbondNuvoton),
                SuperIoWrite::LogicalDevice(0x0B),
                SuperIoWrite::DiscoverRanges,
                SuperIoWrite::Exit(Unlock::WinbondNuvoton),
                SuperIoWrite::HwmIndex { base: 0x0290, index: 0x4E },
                SuperIoWrite::HwmBank { base: 0x0290, bank: 0 },
            ]
        );
    }

    #[test]
    fn global_registers_ignore_the_logical_device_in_the_key() {
        let primary = SuperIoSlot::Primary;
        assert_eq!(superio_config_key(primary, 0x0B, 0x20), superio_config_key(primary, 0, 0x20));
        assert_ne!(superio_config_key(primary, 0x0B, 0x60), superio_config_key(primary, 4, 0x60));
    }

    #[test]
    fn super_io_recording_replays_identically() {
        let fixture: SharedFixture = Arc::default();
        let mut recorder =
            RecordingSuperIo::new(ReplaySuperIo::new(chip_fixture()), fixture.clone());
        recorder.select_slot(SuperIoSlot::Primary).unwrap();
        recorder.enter_config_mode(Unlock::WinbondNuvoton).unwrap();
        recorder.config_read_word(0x20).unwrap();
        recorder.select_logical_device(0x0B).unwrap();
        recorder.config_read_word(0x60).unwrap();
        recorder.discover_io_ranges().unwrap();
        recorder.exit_config_mode(Unlock::WinbondNuvoton).unwrap();
        recorder.hwm_set_index(0x0290, 0x73).unwrap();
        recorder.hwm_read_data(0x0290).unwrap();
        recorder.hwm_set_index(0x0290, 0x4E).unwrap();
        recorder.hwm_write_bank(0x0290, 1).unwrap();
        recorder.hwm_set_index(0x0290, 0x73).unwrap();
        recorder.hwm_read_data(0x0290).unwrap();

        assert_eq!(*fixture.lock().unwrap(), chip_fixture());
    }

    #[test]
    fn a_chip_ignores_an_unlock_sequence_that_is_not_its_own() {
        let mut chip = ReplaySuperIo::new(chip_fixture());
        chip.select_slot(SuperIoSlot::Primary).unwrap();
        // The chip on this slot answers to the Winbond sequence only.
        chip.enter_config_mode(Unlock::Ite).unwrap();
        assert_eq!(chip.config_read(0x20), Ok(0xFF), "nothing listens: the data port floats high");
        // It never entered configuration mode, so another slot can be selected
        // without a leave sequence.
        chip.select_slot(SuperIoSlot::Secondary).unwrap();
    }

    #[test]
    fn recording_attributes_the_chip_id_to_the_sequence_that_got_an_answer() {
        let mut upstream = chip_fixture();
        upstream.set(superio_unlock_key(SuperIoSlot::Secondary), 0);
        upstream.set(superio_config_key(SuperIoSlot::Secondary, 0, 0x20), 0xFF);
        let recorded: SharedFixture = Arc::default();
        let mut recorder = RecordingSuperIo::new(ReplaySuperIo::new(upstream), recorded.clone());

        recorder.select_slot(SuperIoSlot::Secondary).unwrap();
        recorder.enter_config_mode(Unlock::WinbondNuvoton).unwrap();
        assert_eq!(recorder.config_read(0x20), Ok(0xFF));
        recorder.exit_config_mode(Unlock::WinbondNuvoton).unwrap();

        recorder.select_slot(SuperIoSlot::Primary).unwrap();
        recorder.enter_config_mode(Unlock::WinbondNuvoton).unwrap();
        assert_eq!(recorder.config_read(0x20), Ok(0xC5));
        recorder.exit_config_mode(Unlock::WinbondNuvoton).unwrap();

        let recorded = recorded.lock().unwrap();
        assert_eq!(
            recorded.get(&superio_unlock_key(SuperIoSlot::Secondary)),
            Some(0),
            "an empty slot is recorded as unanswered"
        );
        assert_eq!(
            recorded.get(&superio_unlock_key(SuperIoSlot::Primary)),
            Some(unlock_code(Unlock::WinbondNuvoton))
        );
    }

    #[test]
    fn recording_an_ite_chip_attributes_it_to_the_ite_sequence() {
        let mut upstream = Fixture::new();
        upstream.set(superio_unlock_key(SuperIoSlot::Primary), unlock_code(Unlock::Ite));
        upstream.set(superio_config_key(SuperIoSlot::Primary, 0, 0x20), 0x86);
        let recorded: SharedFixture = Arc::default();
        let mut recorder = RecordingSuperIo::new(ReplaySuperIo::new(upstream), recorded.clone());
        recorder.select_slot(SuperIoSlot::Primary).unwrap();
        recorder.enter_config_mode(Unlock::Ite).unwrap();
        assert_eq!(recorder.config_read(0x20), Ok(0x86));
        recorder.exit_config_mode(Unlock::Ite).unwrap();
        assert_eq!(
            recorded.lock().unwrap().get(&superio_unlock_key(SuperIoSlot::Primary)),
            Some(unlock_code(Unlock::Ite))
        );
    }

    // -- SMBus -------------------------------------------------------------

    fn dimm_fixture() -> Fixture {
        let mut fixture = Fixture::new();
        fixture.set(smbus_byte_key(0x50, 0, 2), 0x0C);
        fixture.set(smbus_byte_key(0x50, 0, 0), 0x23);
        fixture.set(smbus_byte_key(0x50, 1, 0), 0x99);
        fixture.set(smbus_word_key(0x18, 5), 0x0161);
        fixture
    }

    #[test]
    fn absent_smbus_addresses_do_not_acknowledge() {
        let mut bus = ReplaySmbus::new(dimm_fixture());
        assert!(bus.read_byte_data(0x51, 0).unwrap_err().is_no_device());
        assert_eq!(bus.read_byte_data(0x50, 2), Ok(0x0C));
        assert_eq!(bus.read_word_data(0x18, 5), Ok(0x0161));
    }

    #[test]
    fn a_missing_command_on_a_present_device_is_a_rejection_not_a_nak() {
        let mut bus = ReplaySmbus::new(dimm_fixture());
        let error = bus.read_byte_data(0x50, 0x77).unwrap_err();
        assert!(matches!(error, BusError::Rejected { .. }), "{error}");
        assert!(error.to_string().contains("smbus.byte.0x50.0.0x77"), "{error}");
    }

    #[test]
    fn the_spd_page_changes_what_an_eeprom_returns() {
        let mut bus = ReplaySmbus::new(dimm_fixture());
        let log = bus.page_select_log();
        assert_eq!(bus.read_byte_data(0x50, 0), Ok(0x23));
        bus.select_spd_page(1).unwrap();
        assert_eq!(bus.read_byte_data(0x50, 0), Ok(0x99));
        bus.select_spd_page(0).unwrap();
        assert_eq!(bus.read_byte_data(0x50, 0), Ok(0x23));
        assert_eq!(*log.lock().unwrap(), [1, 0]);
    }

    #[test]
    fn a_write_to_any_page_but_zero_or_one_is_rejected() {
        let mut bus = ReplaySmbus::new(dimm_fixture());
        for page in [2, 0x36, 0x50, 0xFF] {
            assert!(matches!(bus.select_spd_page(page), Err(BusError::Rejected { .. })), "{page}");
        }
        assert!(bus.page_select_log().lock().unwrap().is_empty());
    }

    #[test]
    fn an_unacknowledged_page_select_reports_no_device_but_still_switches() {
        let mut bus = ReplaySmbus::new(dimm_fixture()).without_page_select_acknowledge();
        assert!(bus.select_spd_page(1).unwrap_err().is_no_device());
        assert_eq!(bus.read_byte_data(0x50, 0), Ok(0x99));
    }

    #[test]
    fn smbus_recording_replays_identically_and_leaves_absent_addresses_empty() {
        let fixture: SharedFixture = Arc::default();
        let mut recorder = RecordingSmbus::new(ReplaySmbus::new(dimm_fixture()), fixture.clone());
        recorder.read_byte_data(0x50, 2).unwrap();
        recorder.read_byte_data(0x50, 0).unwrap();
        recorder.select_spd_page(1).unwrap();
        recorder.read_byte_data(0x50, 0).unwrap();
        recorder.select_spd_page(0).unwrap();
        recorder.read_word_data(0x18, 5).unwrap();
        assert!(recorder.read_byte_data(0x52, 0).is_err());

        assert_eq!(*fixture.lock().unwrap(), dimm_fixture());
    }

    #[test]
    fn the_key_helpers_are_stable_text() {
        assert_eq!(msr_key(CPU1, 0x1B1), "msr.0.2.0x1b1");
        assert_eq!(smn_key(0x59800), "smn.0x59800");
        assert_eq!(
            superio_config_key(SuperIoSlot::Secondary, 4, 0x61),
            "superio.secondary.cfg.0x4.0x61"
        );
        assert_eq!(superio_hwm_key(SuperIoSlot::Primary, 1, 0x20), "superio.primary.hwm.0x1.0x20");
        assert_eq!(superio_unlock_key(SuperIoSlot::Secondary), "superio.secondary.unlock");
        assert_eq!(unlock_code(Unlock::WinbondNuvoton), 1);
        assert_eq!(unlock_code(Unlock::Ite), 2);
        assert_eq!(
            smbus_byte_key(0x18, 1, 5),
            "smbus.byte.0x18.0.0x5",
            "page only matters for SPD"
        );
        assert_eq!(smbus_byte_key(0x50, 1, 5), "smbus.byte.0x50.1.0x5");
        assert_eq!(smbus_word_key(0x18, 5), "smbus.word.0x18.0x5");
    }
}
