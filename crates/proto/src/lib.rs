//! SysPilot IPC protocol.
//!
//! Everything the service and its clients must agree on lives here:
//!
//! - [`product`]: the product name and the OS object names derived from it.
//! - [`action`], [`catalog`], [`profile`]: the configuration model.
//! - [`message`]: requests, responses and events.
//! - [`frame`]: length-prefixed `postcard` framing.
//! - [`sensor`]: sensor metadata and the shared-memory layout.
//! - [`client`]: the pipe client.
//! - [`win`]: the Win32 plumbing underneath.
//!
//! The wire format is described in `docs/protocol.md`.

#![warn(missing_docs)]

pub mod action;
pub mod catalog;
pub mod client;
pub mod frame;
pub mod message;
pub mod product;
pub mod profile;
pub mod sensor;
pub mod win;

pub use client::{Client, ClientError, EventStream};
pub use product::{Instance, PRODUCT_NAME, PROTOCOL_VERSION};
