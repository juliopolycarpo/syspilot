//! The error every bus operation returns.

use std::fmt;

use syspilot_pawnio::Error as DriverError;

use super::locks::BusKind;

/// Why a bus access did not produce a value.
///
/// Providers react to the cause, not the text: a missing device ends a
/// scan, a bus fault stops all further access until the next sample, a
/// contended bus skips one sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BusError {
    /// Another program holds the shared bus mutex; this sample is skipped.
    Contended(BusKind),
    /// The shared bus mutex could not be created or opened.
    Lock {
        /// The bus.
        bus: BusKind,
        /// The Win32 error.
        win32: u32,
    },
    /// The driver refused or failed the access.
    Driver(DriverError),
    /// A raw thread or processor query failed.
    Os {
        /// What was being done.
        operation: &'static str,
        /// The Win32 error.
        win32: u32,
    },
    /// The access broke a rule of the bus layer itself (or a replayed
    /// recording): the hardware was not touched.
    Rejected {
        /// What was attempted.
        operation: String,
        /// What the layer allows.
        expected: String,
        /// What was asked for.
        received: String,
    },
}

impl BusError {
    /// A [`BusError::Rejected`] with the message shape the project uses.
    pub fn rejected(
        operation: impl Into<String>,
        expected: impl Into<String>,
        received: impl Into<String>,
    ) -> Self {
        Self::Rejected {
            operation: operation.into(),
            expected: expected.into(),
            received: received.into(),
        }
    }

    /// Nothing acknowledged the access: an empty SMBus address.
    pub fn is_no_device(&self) -> bool {
        matches!(self, Self::Driver(DriverError::NoSuchDevice { .. }))
    }

    /// A busy, timed-out or failed transaction. The caller stops touching
    /// the bus for this sample and does not retry.
    pub fn is_bus_fault(&self) -> bool {
        matches!(self, Self::Driver(error) if error.is_bus_fault())
    }

    /// Another program holds the bus.
    pub fn is_contended(&self) -> bool {
        matches!(self, Self::Contended(_))
    }
}

impl From<DriverError> for BusError {
    fn from(error: DriverError) -> Self {
        Self::Driver(error)
    }
}

impl fmt::Display for BusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Contended(bus) => write!(
                f,
                "({bus}): expected the shared bus mutex within the wait limit | received: held by \
                 another program"
            ),
            Self::Lock { bus, win32 } => write!(
                f,
                "({bus}): expected to create or open the shared bus mutex | received: Win32 error \
                 {win32}"
            ),
            Self::Driver(error) => error.fmt(f),
            Self::Os { operation, win32 } => {
                write!(f, "({operation}): expected success | received: Win32 error {win32}")
            }
            Self::Rejected { operation, expected, received } => {
                write!(f, "({operation}): expected {expected} | received: {received}")
            }
        }
    }
}

impl std::error::Error for BusError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_such_device() -> DriverError {
        DriverError::NoSuchDevice { function: "ioctl_smbus_xfer".into() }
    }

    #[test]
    fn a_missing_device_is_not_a_bus_fault() {
        let error = BusError::from(no_such_device());
        assert!(error.is_no_device());
        assert!(!error.is_bus_fault());
    }

    #[test]
    fn timeouts_and_retries_are_bus_faults() {
        for driver in [
            DriverError::Timeout { function: "f".into() },
            DriverError::Retry { function: "f".into() },
            DriverError::Busy { function: "f".into() },
        ] {
            let error = BusError::from(driver);
            assert!(error.is_bus_fault(), "{error}");
            assert!(!error.is_no_device(), "{error}");
        }
    }

    #[test]
    fn an_allow_list_refusal_is_neither() {
        let error = BusError::from(DriverError::ArgumentDenied {
            function: "ioctl_read_msr".into(),
            argument: Some(0x10),
        });
        assert!(!error.is_bus_fault() && !error.is_no_device() && !error.is_contended());
    }

    #[test]
    fn contention_names_the_bus() {
        let error = BusError::Contended(BusKind::Smbus);
        assert!(error.is_contended());
        assert_eq!(
            error.to_string(),
            "(smbus): expected the shared bus mutex within the wait limit | received: held by \
             another program"
        );
    }

    #[test]
    fn a_rejection_states_expected_and_received() {
        let error = BusError::rejected("smbus write", "target 0x36 or 0x37", "target 0x50");
        assert_eq!(
            error.to_string(),
            "(smbus write): expected target 0x36 or 0x37 | received: target 0x50"
        );
    }
}
