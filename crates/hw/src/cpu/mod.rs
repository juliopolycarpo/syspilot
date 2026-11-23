//! Processor temperatures and power through model-specific registers.
//!
//! - [`identity`]: vendor, family and model from `CPUID`.
//! - [`topology`]: physical cores and their first logical processor.
//! - [`rapl`]: energy counters to watts, with a clock tests control.
//! - [`intel`]: Core and Xeon, validated on hardware.
//! - [`amd`]: Zen (families 17h to 1Ah), validated by replay only.

pub mod amd;
pub mod identity;
pub mod intel;
pub mod rapl;
pub mod topology;

pub use amd::{AmdCpuProvider, AmdProbeError};
pub use identity::{CpuIdentity, Vendor};
pub use intel::{IntelCpuProvider, IntelProbeError};
pub use rapl::{Clock, EnergyMeter, ManualClock, SystemClock};
pub use topology::{CpuTopology, Package};
