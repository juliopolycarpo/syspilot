//! Super I/O hardware-monitor chips: temperatures, fans, voltages and fan
//! duty from the Nuvoton NCT67xx and ITE IT87xx families.
//!
//! - [`detect`] finds the chips on the two possible port pairs, leaving
//!   configuration mode on every path.
//! - [`nct67xx`] and [`it87xx`] hold the register layouts and read a chip.
//! - [`provider`] turns a detected chip into a [`crate::Provider`].
//!
//! Register semantics follow the chip datasheets as documented by the Linux
//! `nct6775` and `it87` drivers and LibreHardwareMonitor's register tables
//! (MPL-2.0); the code here is written from those semantics, not copied.
//! The chip data of every family but the one on the reference machine
//! (NCT6779D) is checked only against replayed registers.

pub mod detect;
pub mod it87xx;
pub mod nct67xx;
pub mod provider;

use syspilot_proto::sensor::SensorKind;

use crate::bus::{BusError, SuperIo};

pub use detect::{Detected, Detection, detect};
pub use provider::{ProviderError, SuperIoProvider};

/// One sensor a chip reports, as declared at probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Channel {
    /// Provider-unique key, e.g. `board.temp.cputin`.
    pub key: String,
    /// Display label, e.g. `CPUTIN`.
    pub label: String,
    /// Quantity and unit.
    pub kind: SensorKind,
}

impl Channel {
    pub(crate) fn new(key: impl Into<String>, label: impl Into<String>, kind: SensorKind) -> Self {
        Self { key: key.into(), label: label.into(), kind }
    }
}

/// A detected chip whose hardware-monitor registers can be read. The
/// caller holds the ISA bus guard and has made the chip's I/O range known
/// to the driver.
pub trait MonitorChip: Send {
    /// The chip's name, e.g. `NCT6779D`.
    fn name(&self) -> String;

    /// Reads the chip once and returns the channels that carry a real value
    /// now. [`MonitorChip::read`] answers in the same order.
    fn discover(&mut self, io: &mut dyn SuperIo) -> Result<Vec<Channel>, BusError>;

    /// Reads one value per discovered channel; `None` where the register
    /// does not hold a plausible reading.
    fn read(&mut self, io: &mut dyn SuperIo) -> Result<Vec<Option<f64>>, BusError>;
}
