//! Fan control: the curve engine, the board catalog and, only with the
//! `fan-control` cargo feature, the code that writes to a fan controller.
//!
//! - [`curve`]: temperature to duty. Pure, always compiled.
//! - [`board`]: which boards SysPilot may control and with which limits.
//!   Always compiled; it only describes, and a board stays read-only until a
//!   human has validated its entry.
//!
//! Decision 12 keeps version 1 from writing to hardware. Everything that can
//! write a fan register is behind the `fan-control` feature, which is off by
//! default and in release builds; without it none of that code exists.

pub mod board;
pub mod curve;
