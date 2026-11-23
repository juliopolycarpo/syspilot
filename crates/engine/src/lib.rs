//! SysPilot enforcement engine.
//!
//! - [`os`]: the operating-system surface, as traits.
//! - [`ops`]: detect, capture, apply and revert for each action kind.
//! - [`enforcer`]: the state machine that keeps enabled entries applied.
//! - [`fake`] and [`store`]: in-memory implementations for tests.
//! - [`win`]: the Win32 implementations.
//!
//! The model is described in `docs/architecture/enforcement.md`.

pub mod enforcer;
pub mod fake;
pub mod ops;
pub mod os;
pub mod store;
pub mod win;

pub use enforcer::{Enforcer, EnforcerConfig, Entry, StateStore, TriggerEvent};
pub use ops::{Target, WatchSpec};
pub use os::{Hive, Os};
