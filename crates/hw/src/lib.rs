//! SysPilot hardware layer.
//!
//! A [`Provider`] reads one source of sensor values (the OS, a GPU, a Super
//! I/O chip). The [`Hub`] owns a set of providers, assigns sensor IDs and
//! samples them on request. Nothing here samples on its own: the service
//! calls [`Hub::sample`] only while a subscriber, the logger or a fan curve
//! needs values.

pub mod hub;
pub mod os;

pub use hub::{FakeProvider, Hub, Provider, SensorSpec};
