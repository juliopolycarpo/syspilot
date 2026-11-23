//! Hardware buses behind PawnIO, as traits the providers depend on.
//!
//! The traits are deliberately narrow. A provider cannot write an arbitrary
//! register: the only write operations that exist are the ones a read needs
//! (entering and leaving Super I/O configuration mode, selecting a logical
//! device, setting a hardware-monitor index or bank, selecting a DDR4 SPD
//! page). Everything else is a read. That makes "SysPilot only reads"
//! something the type system enforces and the replay fakes can audit,
//! instead of a convention each provider has to keep.
//!
//! - [`pawn`]: the implementations that call the driver.
//! - [`replay`]: fakes that serve recorded values and reject anything out of
//!   protocol, plus recorders that capture them from real hardware.
//! - [`locks`]: the shared bus mutexes every driver-backed access takes.

mod error;
pub mod fixture;
pub mod locks;
pub mod pawn;
pub mod replay;

pub use error::BusError;
pub use fixture::{Fixture, FixtureError};
pub use locks::{BusGuard, BusKind, BusLocks, FakeBusLocks, SystemBusLocks};

/// One logical processor, addressed the way Windows does: processor group
/// and bit index inside the group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LogicalCpu {
    /// Processor group.
    pub group: u16,
    /// Bit position of the processor inside its group's affinity mask.
    pub index: u8,
}

impl LogicalCpu {
    /// The affinity mask that selects exactly this processor.
    pub fn mask(self) -> usize {
        1usize << self.index
    }
}

/// Model-specific registers. Read only: there is no write operation.
pub trait Msr: Send {
    /// Reads `register` on `cpu`. Implementations that talk to hardware pin
    /// the calling thread to `cpu` for the duration of the read, because the
    /// driver module reads on whichever processor runs the call.
    fn read(&mut self, cpu: LogicalCpu, register: u32) -> Result<u64, BusError>;
}

/// The AMD System Management Network, reached through the PCI window.
/// Callers hold the [`BusKind::Pci`] guard.
pub trait Smn: Send {
    /// Reads the 32-bit register at SMN `address`.
    fn read(&mut self, address: u32) -> Result<u32, BusError>;
}

/// The SMN addresses [`pawn::PawnSmn`] reads: the thermal block that holds
/// the reported temperature (`0x59800`) and the per-CCD temperatures
/// (`0x59954` to `0x59B38` over all supported models).
///
/// `AMDFamily17` does not restrict `ioctl_read_smn`: it writes whatever
/// address it is given to the PCI index register. Reading an SMN register
/// can have side effects (some clear on read), so this layer allows only
/// the registers a temperature needs.
pub const SMN_THERMAL_BLOCK: std::ops::RangeInclusive<u32> = 0x0005_9800..=0x0005_9BFF;

/// Whether `address` is inside [`SMN_THERMAL_BLOCK`].
pub fn is_permitted_smn_address(address: u32) -> bool {
    SMN_THERMAL_BLOCK.contains(&address)
}

/// Which register/value port pair the Super I/O chip answers on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SuperIoSlot {
    /// Ports `0x2E`/`0x2F`.
    Primary,
    /// Ports `0x4E`/`0x4F`.
    Secondary,
}

impl SuperIoSlot {
    /// Both slots, primary first.
    pub const ALL: [SuperIoSlot; 2] = [SuperIoSlot::Primary, SuperIoSlot::Secondary];

    /// The register (index) port.
    pub fn register_port(self) -> u16 {
        match self {
            Self::Primary => 0x2E,
            Self::Secondary => 0x4E,
        }
    }

    /// The value (data) port.
    pub fn value_port(self) -> u16 {
        self.register_port() + 1
    }
}

/// Which magic sequence unlocks the chip's configuration registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unlock {
    /// Winbond, Nuvoton and Fintek: `0x87, 0x87` to enter, `0xAA` to leave.
    WinbondNuvoton,
    /// ITE: `0x87, 0x01, 0x55`, then `0x55` (slot `0x2E`) or `0xAA` (slot
    /// `0x4E`) to enter; a write of `0x02` to register `0x02` leaves.
    Ite,
}

/// The ISA-bus Super I/O chip: its configuration registers and the
/// hardware-monitor index/data port pair.
///
/// All writes here move an index, they do not change what the chip does. A
/// sequence that enters configuration mode must be paired with
/// [`SuperIo::exit_config_mode`] on every path; providers use an RAII guard
/// for that. Callers hold the [`BusKind::Isa`] guard.
pub trait SuperIo: Send {
    /// Chooses the register/value ports. Forgets any discovered I/O ranges.
    fn select_slot(&mut self, slot: SuperIoSlot) -> Result<(), BusError>;

    /// Writes the unlock sequence to the register port.
    fn enter_config_mode(&mut self, unlock: Unlock) -> Result<(), BusError>;

    /// Leaves configuration mode.
    fn exit_config_mode(&mut self, unlock: Unlock) -> Result<(), BusError>;

    /// Writes the logical-device-select register (`0x07`). In configuration
    /// mode only.
    fn select_logical_device(&mut self, device: u8) -> Result<(), BusError>;

    /// Reads one configuration register of the selected logical device (or a
    /// global one). In configuration mode only.
    fn config_read(&mut self, register: u8) -> Result<u8, BusError>;

    /// Reads a big-endian word from `register` and `register + 1`.
    fn config_read_word(&mut self, register: u8) -> Result<u16, BusError> {
        let high = self.config_read(register)?;
        let low = self.config_read(register.wrapping_add(1))?;
        Ok(u16::from_be_bytes([high, low]))
    }

    /// Lets the driver learn the chip's I/O ranges, which it then allows
    /// reads and index writes inside. In configuration mode only, with a
    /// chip ID the driver knows.
    fn discover_io_ranges(&mut self) -> Result<(), BusError>;

    /// Writes `index` to the hardware monitor's address port (`base + 5`).
    fn hwm_set_index(&mut self, base: u16, index: u8) -> Result<(), BusError>;

    /// Writes `bank` to the hardware monitor's data port (`base + 6`). Only
    /// valid directly after [`SuperIo::hwm_set_index`] with the chip's
    /// bank-select register (`0x4E`); anything else would write a data
    /// register.
    fn hwm_write_bank(&mut self, base: u16, bank: u8) -> Result<(), BusError>;

    /// Reads the address port (`base + 5`): the index last written.
    fn hwm_read_index(&mut self, base: u16) -> Result<u8, BusError>;

    /// Reads the data port (`base + 6`): the register the index selects.
    fn hwm_read_data(&mut self, base: u16) -> Result<u8, BusError>;
}

/// Whether `bank` is a value the hardware monitor's bank-select register may
/// be given.
///
/// Bits 3:0 choose one of the register banks `0..=0xF`. Bit 7 (`HBACS` in
/// the Nuvoton datasheets) selects the high byte of the registers that exist
/// as a 16-bit pair, which is how the vendor ID at register `0x4F` reads as
/// `0x5C` (bank `0x80`) and `0xA3` (bank `0`). No other bit is ever set, so
/// no other value is allowed: both [`pawn::PawnSuperIo`] and
/// [`replay::ReplaySuperIo`] refuse everything else.
pub fn is_permitted_bank(bank: u8) -> bool {
    bank <= 0x0F || bank == 0x80
}

/// An SMBus host controller. Callers hold the [`BusKind::Smbus`] guard.
///
/// There is no general write: a write on an SMBus can reconfigure any chip
/// on it (a clock generator, a voltage regulator). The one write that
/// exists selects the DDR4 SPD page.
pub trait Smbus: Send {
    /// Reads the byte at `command` of the device at 7-bit `address`.
    fn read_byte_data(&mut self, address: u8, command: u8) -> Result<u8, BusError>;

    /// Reads a word as the controller returns it: the first byte on the wire
    /// is the low byte.
    fn read_word_data(&mut self, address: u8, command: u8) -> Result<u16, BusError>;

    /// Selects DDR4 SPD page 0 or 1 by addressing the page-select pseudo
    /// device `0x36 + page`. All modules on the bus switch page together.
    fn select_spd_page(&mut self, page: u8) -> Result<(), BusError>;
}

/// The SPD page-select addresses (`SPA0`, `SPA1`).
pub const SPD_PAGE_SELECT_ADDRESSES: [u8; 2] = [0x36, 0x37];
