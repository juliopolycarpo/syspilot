//! Sensor providers that read the operating system directly and need no
//! kernel driver.
//!
//! Each provider module has a `probe()` that returns its provider, or
//! nothing when the machine has no such hardware. [`probe`] runs them all.
//! Probing may open handles and PDH queries but never starts a thread or a
//! timer: every read happens inside [`Provider::sample`].

pub mod battery;
pub mod cpu;
pub mod gpu;
pub mod memory;
pub mod network;
pub(crate) mod pdh;
pub(crate) mod rate;
pub(crate) mod registry;
pub mod storage;
pub(crate) mod text;
pub mod thermal;
pub mod volumes;

use crate::hub::Provider;

/// A module's probe: its providers, or none if the machine lacks the hardware.
pub type ProbeFn = fn() -> Vec<Box<dyn Provider>>;

/// Every module's probe by module name, in the order [`probe`] runs them.
/// Exposed so tools can time or test one module at a time.
pub const PROBES: &[(&str, ProbeFn)] = &[
    ("cpu", cpu::probe),
    ("memory", memory::probe),
    ("gpu", gpu::probe),
    ("storage", storage::probe),
    ("volumes", volumes::probe),
    ("network", network::probe),
    ("battery", battery::probe),
    ("thermal", thermal::probe),
];

/// Constructs every provider that finds hardware on this machine.
pub fn probe() -> Vec<Box<dyn Provider>> {
    PROBES.iter().flat_map(|(_, probe)| probe()).collect()
}
