//! Sensor providers that read the operating system directly and need no
//! kernel driver.
//!
//! Each provider module has a `probe()` that returns its provider, or
//! nothing when the machine has no such hardware. [`probe`] runs them all.
//! Probing may open handles and PDH queries but never starts a thread or a
//! timer: every read happens inside [`Provider::sample`].

pub mod cpu;
pub mod gpu;
pub mod memory;
pub(crate) mod pdh;
pub(crate) mod rate;
pub(crate) mod registry;
pub(crate) mod text;

use crate::hub::Provider;

/// Constructs every provider that finds hardware on this machine.
pub fn probe() -> Vec<Box<dyn Provider>> {
    let mut providers = Vec::new();
    providers.extend(cpu::probe());
    providers.extend(memory::probe());
    providers.extend(gpu::probe());
    providers
}
