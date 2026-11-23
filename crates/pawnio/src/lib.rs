//! A thin wrapper over the PawnIO device interface.
//!
//! [PawnIO](https://pawnio.eu) is a signed kernel driver that runs small
//! signed modules (for MSRs, Super I/O chips, SMBus controllers) and lets
//! user mode call their functions. This crate speaks to it and nothing else.
//!
//! # Licence boundary
//!
//! The driver is GPL-2.0 with an exception for programs that communicate
//! with it solely through the device IO control interface. This crate stays
//! inside that exception: it talks to `\Device\PawnIO` with
//! `NtDeviceIoControlFile` and never loads or links `PawnIOLib.dll` (which
//! is LGPL) nor the Pawn language interface. The modules are LGPL files that
//! the caller reads from disk and passes to [`PawnIo::load`]; nothing here
//! embeds them.
//!
//! # One handle, one module
//!
//! A handle holds a single loaded module. [`PawnIo::open`] opens a fresh
//! handle, [`PawnIo::load`] puts a module on it, and [`Executor::execute`]
//! calls that module's functions. Use one [`PawnIo`] per module.
//!
//! # Buffers
//!
//! `execute` input is the function name zero-padded to 32 bytes followed by
//! the input cells; the driver maps input and output over one system
//! buffer. Two consequences in driver 2.2.0:
//!
//! - The returned length is always the whole output buffer, not the cells
//!   the function wrote, so the output slice must be exactly the size the
//!   function declares and only the cells it documents may be trusted.
//! - Output cells the function does not write hold stale bytes of the
//!   request (the name for cells 0-3, input cells after that).
//!
//! # Testing without the driver
//!
//! Code that calls modules takes `&dyn` [`Executor`]. [`ScriptedExecutor`]
//! records the calls and replays outputs, so register logic is tested
//! without hardware.

#![warn(missing_docs)]

pub mod abi;
mod device;
mod error;
mod executor;
mod install;
mod version;

pub use device::PawnIo;
pub use error::{Error, status, status_name};
pub use executor::{Executor, RecordedCall, ScriptedExecutor};
pub use install::{INSTALL_URL, installed_version};
pub use version::{MIN_DRIVER_VERSION, Version};
