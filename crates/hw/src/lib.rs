//! SysPilot hardware layer.
//!
//! A [`Provider`] reads one source of sensor values (the OS, a GPU, a Super
//! I/O chip). The [`Hub`] owns a set of providers, assigns sensor IDs and
//! samples them on request. Nothing here samples on its own: the service
//! calls [`Hub::sample`] only while a subscriber, the logger or a fan curve
//! needs values.
//!
//! The driver-backed providers (CPU package and core sensors, Super I/O,
//! DIMM temperatures) sit on the [`bus`] layer, which reaches the hardware
//! only through PawnIO and is read-only apart from the index writes a read
//! needs.

pub mod backoff;
pub mod bus;
pub mod chips;
pub mod cpu;
pub mod hub;
pub mod os;
pub mod sysinfo;

pub use hub::{FakeProvider, Hub, Provider, SensorSpec};
