//! Typed errors and the NTSTATUS values the driver and its modules return.

use std::fmt;

use crate::version::Version;

/// NTSTATUS codes seen from the driver and the published modules.
///
/// Values come from `ntstatus.inc` of PawnIO.Modules and `ntstatus.h`.
#[allow(missing_docs)]
pub mod status {
    pub const SUCCESS: u32 = 0x0000_0000;
    pub const DEVICE_BUSY: u32 = 0x8000_0011;
    pub const UNSUCCESSFUL: u32 = 0xC000_0001;
    pub const NOT_IMPLEMENTED: u32 = 0xC000_0002;
    pub const INVALID_PARAMETER: u32 = 0xC000_000D;
    pub const NO_SUCH_DEVICE: u32 = 0xC000_000E;
    pub const NO_MEMORY: u32 = 0xC000_0017;
    pub const ACCESS_DENIED: u32 = 0xC000_0022;
    pub const BUFFER_TOO_SMALL: u32 = 0xC000_0023;
    pub const OBJECT_NAME_NOT_FOUND: u32 = 0xC000_0034;
    /// An `RDMSR` of a register the CPU does not implement raises a general
    /// protection fault, which the module reports as this status. Seen for
    /// allow-listed MSRs that this model lacks (`0x64D`, `0x641` on a
    /// Broadwell-EP).
    pub const PRIVILEGED_INSTRUCTION: u32 = 0xC000_0096;
    pub const DEVICE_NOT_READY: u32 = 0xC000_00A3;
    pub const IO_TIMEOUT: u32 = 0xC000_00B5;
    pub const NOT_SUPPORTED: u32 = 0xC000_00BB;
    pub const IO_DEVICE_ERROR: u32 = 0xC000_0185;
    pub const DEVICE_PROTOCOL_ERROR: u32 = 0xC000_0186;
    pub const NOT_FOUND: u32 = 0xC000_0225;
    pub const RETRY: u32 = 0xC000_022D;
    pub const ALREADY_INITIALIZED: u32 = 0xC000_0510;
    pub const INVALID_SIGNATURE: u32 = 0xC000_A000;
}

/// The symbolic name of `status`, if it is one this crate knows.
pub fn status_name(status: u32) -> Option<&'static str> {
    Some(match status {
        status::SUCCESS => "STATUS_SUCCESS",
        status::DEVICE_BUSY => "STATUS_DEVICE_BUSY",
        status::UNSUCCESSFUL => "STATUS_UNSUCCESSFUL",
        status::NOT_IMPLEMENTED => "STATUS_NOT_IMPLEMENTED",
        status::INVALID_PARAMETER => "STATUS_INVALID_PARAMETER",
        status::NO_SUCH_DEVICE => "STATUS_NO_SUCH_DEVICE",
        status::NO_MEMORY => "STATUS_NO_MEMORY",
        status::ACCESS_DENIED => "STATUS_ACCESS_DENIED",
        status::BUFFER_TOO_SMALL => "STATUS_BUFFER_TOO_SMALL",
        status::OBJECT_NAME_NOT_FOUND => "STATUS_OBJECT_NAME_NOT_FOUND",
        status::PRIVILEGED_INSTRUCTION => "STATUS_PRIVILEGED_INSTRUCTION",
        status::DEVICE_NOT_READY => "STATUS_DEVICE_NOT_READY",
        status::IO_TIMEOUT => "STATUS_IO_TIMEOUT",
        status::NOT_SUPPORTED => "STATUS_NOT_SUPPORTED",
        status::IO_DEVICE_ERROR => "STATUS_IO_DEVICE_ERROR",
        status::DEVICE_PROTOCOL_ERROR => "STATUS_DEVICE_PROTOCOL_ERROR",
        status::NOT_FOUND => "STATUS_NOT_FOUND",
        status::RETRY => "STATUS_RETRY",
        status::ALREADY_INITIALIZED => "STATUS_ALREADY_INITIALIZED",
        status::INVALID_SIGNATURE => "STATUS_INVALID_SIGNATURE",
        _ => return None,
    })
}

/// Everything that can go wrong talking to PawnIO.
///
/// The variants separate causes a caller reacts to differently: install the
/// driver, elevate, update it, replace a module, skip a platform, or back
/// off a busy bus. Statuses that none of them covers arrive as
/// [`Error::Status`] with the raw NTSTATUS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The PawnIO device does not exist: the driver is not installed.
    DriverAbsent {
        /// The Win32 error `CreateFileW` returned.
        win32: u32,
    },
    /// The device exists but this process may not open it. Only SYSTEM and
    /// Administrators can, so the process is not elevated.
    NotElevated,
    /// Opening the device failed for another reason.
    OpenFailed {
        /// The Win32 error `CreateFileW` returned.
        win32: u32,
    },
    /// The installed driver is older than [`crate::MIN_DRIVER_VERSION`].
    DriverTooOld {
        /// Version the driver reports.
        found: Version,
        /// Oldest accepted version.
        required: Version,
    },
    /// The module is not signed by the key the driver trusts (or is
    /// corrupt: any byte change breaks the signature).
    SignatureRefused {
        /// The status the load returned.
        status: u32,
    },
    /// The loader could not parse the blob (`STATUS_INVALID_PARAMETER` or
    /// `STATUS_UNSUCCESSFUL` while loading).
    ModuleMalformed {
        /// The status the load returned.
        status: u32,
    },
    /// The handle already holds a module. One handle holds one module.
    AlreadyLoaded,
    /// The module's `main()` refused this machine (wrong CPU vendor, no
    /// supported controller), or a function declined its environment.
    NotSupported {
        /// The function, or `None` while loading.
        function: Option<String>,
    },
    /// The module has no function of this name.
    FunctionNotFound {
        /// The requested function.
        function: String,
    },
    /// The module's allow-list refused the argument (an MSR that is not
    /// listed, a port outside the discovered ranges).
    ArgumentDenied {
        /// The function that refused.
        function: String,
        /// First input cell, usually the register or port.
        argument: Option<u64>,
    },
    /// The function rejected the call shape: wrong cell counts, or no
    /// module is loaded on this handle.
    InvalidParameter {
        /// The function.
        function: String,
    },
    /// The function ran, but the device behind it is not ready (for
    /// example no Super I/O slot selected).
    NotReady {
        /// The function.
        function: String,
    },
    /// Nothing acknowledged the access: an SMBus address without a device.
    NoSuchDevice {
        /// The function.
        function: String,
    },
    /// The host controller is busy with another transaction.
    Busy {
        /// The function.
        function: String,
    },
    /// The bus did not finish in time, for example because firmware holds
    /// it. Back off instead of retrying.
    Timeout {
        /// The function.
        function: String,
    },
    /// The controller reported a bus error (lost arbitration).
    Retry {
        /// The function.
        function: String,
    },
    /// The device or protocol failed (`STATUS_IO_DEVICE_ERROR`,
    /// `STATUS_DEVICE_PROTOCOL_ERROR`).
    DeviceError {
        /// The function.
        function: String,
        /// The NTSTATUS.
        status: u32,
    },
    /// Any other NTSTATUS.
    Status {
        /// What was being done: `load`, `version` or the function name.
        operation: String,
        /// The NTSTATUS.
        status: u32,
    },
    /// The function name is not acceptable to the driver.
    InvalidFunctionName {
        /// The name as given.
        name: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// The request does not fit the driver's 32-bit buffer lengths.
    BufferTooLarge {
        /// The offending length in bytes.
        bytes: usize,
    },
}

impl Error {
    /// Maps a status returned by `IOCTL_PIO_LOAD_BINARY`.
    pub(crate) fn from_load_status(status: u32) -> Self {
        match status {
            status::ALREADY_INITIALIZED => Self::AlreadyLoaded,
            status::INVALID_SIGNATURE => Self::SignatureRefused { status },
            status::NOT_SUPPORTED => Self::NotSupported { function: None },
            // A blob too short for its own header, or one the AMX loader rejects.
            status::INVALID_PARAMETER | status::UNSUCCESSFUL => Self::ModuleMalformed { status },
            other => Self::Status { operation: "load".into(), status: other },
        }
    }

    /// Maps a status returned by `IOCTL_PIO_EXECUTE_FN` for `function`.
    ///
    /// `STATUS_ACCESS_DENIED` is the module's allow-list here; the same
    /// status at open time (see [`Error::NotElevated`]) means something
    /// else, which is why classification depends on the operation.
    pub(crate) fn from_execute_status(function: &str, input: &[u64], status: u32) -> Self {
        let function = function.to_owned();
        match status {
            status::ACCESS_DENIED => {
                Self::ArgumentDenied { function, argument: input.first().copied() }
            }
            status::OBJECT_NAME_NOT_FOUND => Self::FunctionNotFound { function },
            status::INVALID_PARAMETER | status::BUFFER_TOO_SMALL => {
                Self::InvalidParameter { function }
            }
            status::DEVICE_NOT_READY => Self::NotReady { function },
            status::NOT_SUPPORTED => Self::NotSupported { function: Some(function) },
            status::NO_SUCH_DEVICE => Self::NoSuchDevice { function },
            status::DEVICE_BUSY => Self::Busy { function },
            status::IO_TIMEOUT => Self::Timeout { function },
            status::RETRY => Self::Retry { function },
            status::IO_DEVICE_ERROR | status::DEVICE_PROTOCOL_ERROR => {
                Self::DeviceError { function, status }
            }
            other => Self::Status { operation: function, status: other },
        }
    }

    /// Whether this is a bus-level failure of a read that may succeed
    /// later: busy, timeout, retry or a device error. A missing device
    /// ([`Error::NoSuchDevice`]) is an answer, not a fault, and is excluded.
    pub fn is_bus_fault(&self) -> bool {
        matches!(
            self,
            Self::Busy { .. }
                | Self::Timeout { .. }
                | Self::Retry { .. }
                | Self::DeviceError { .. }
        )
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DriverAbsent { win32 } => write!(
                f,
                "(pawnio.open): expected the PawnIO device to exist | received: Win32 error \
                 {win32}; PawnIO is not installed"
            ),
            Self::NotElevated => write!(
                f,
                "(pawnio.open): expected permission to open the PawnIO device | received: access \
                 denied; only SYSTEM and Administrators may open it, run elevated"
            ),
            Self::OpenFailed { win32 } => write!(
                f,
                "(pawnio.open): expected the PawnIO device to open | received: Win32 error {win32}"
            ),
            Self::DriverTooOld { found, required } => write!(
                f,
                "(pawnio.version): expected driver version >= {required} | received: {found}"
            ),
            Self::SignatureRefused { status } => write!(
                f,
                "(pawnio.load): expected a module signed by the trusted key | received: {}; the \
                 module is unsigned, corrupt or signed by another key",
                describe(*status)
            ),
            Self::ModuleMalformed { status } => write!(
                f,
                "(pawnio.load): expected a module file in `[u32 signature length][signature][AMX]` \
                 form | received: {}",
                describe(*status)
            ),
            Self::AlreadyLoaded => write!(
                f,
                "(pawnio.load): expected a handle without a module | received: {}; use one \
                 handle per module",
                describe(status::ALREADY_INITIALIZED)
            ),
            Self::NotSupported { function: None } => write!(
                f,
                "(pawnio.load): expected the module to accept this machine | received: {}; wrong \
                 CPU vendor or the supported device is missing",
                describe(status::NOT_SUPPORTED)
            ),
            Self::NotSupported { function: Some(function) } => write!(
                f,
                "({function}): expected the call to be supported here | received: {}",
                describe(status::NOT_SUPPORTED)
            ),
            Self::FunctionNotFound { function } => write!(
                f,
                "({function}): expected a function the module exports | received: {}",
                describe(status::OBJECT_NAME_NOT_FOUND)
            ),
            Self::ArgumentDenied { function, argument } => {
                let argument = match argument {
                    Some(value) => format!("{value:#x}"),
                    None => "(none)".to_owned(),
                };
                write!(
                    f,
                    "({function}): expected an argument on the module's allow-list | received: \
                     {argument}, {}",
                    describe(status::ACCESS_DENIED)
                )
            }
            Self::InvalidParameter { function } => write!(
                f,
                "({function}): expected the cell counts the function declares and a loaded module \
                 | received: {}",
                describe(status::INVALID_PARAMETER)
            ),
            Self::NotReady { function } => write!(
                f,
                "({function}): expected a ready device | received: {}",
                describe(status::DEVICE_NOT_READY)
            ),
            Self::NoSuchDevice { function } => write!(
                f,
                "({function}): expected an acknowledge | received: {}; nothing answers at that \
                 address",
                describe(status::NO_SUCH_DEVICE)
            ),
            Self::Busy { function } => write!(
                f,
                "({function}): expected an idle host controller | received: {}",
                describe(status::DEVICE_BUSY)
            ),
            Self::Timeout { function } => write!(
                f,
                "({function}): expected the transaction to finish in time | received: {}; \
                 firmware may hold the bus",
                describe(status::IO_TIMEOUT)
            ),
            Self::Retry { function } => write!(
                f,
                "({function}): expected a clean transaction | received: {}; bus error",
                describe(status::RETRY)
            ),
            Self::DeviceError { function, status } => write!(
                f,
                "({function}): expected the device to complete the access | received: {}",
                describe(*status)
            ),
            Self::Status { operation, status } => write!(
                f,
                "(pawnio.{operation}): expected {} | received: {}",
                describe(status::SUCCESS),
                describe(*status)
            ),
            Self::InvalidFunctionName { name, reason } => write!(
                f,
                "(pawnio.execute): expected `ioctl_` followed by at most 25 ASCII characters | \
                 received: {name:?}, {reason}"
            ),
            Self::BufferTooLarge { bytes } => write!(
                f,
                "(pawnio.execute): expected a request that fits in 4 GiB | received: {bytes} bytes"
            ),
        }
    }
}

impl std::error::Error for Error {}

/// `0xC0000022 (STATUS_ACCESS_DENIED)`, or just the hex value when unnamed.
fn describe(value: u32) -> String {
    match status_name(value) {
        Some(name) => format!("{value:#010X} ({name})"),
        None => format!("{value:#010X}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_statuses_map_to_their_causes() {
        assert_eq!(Error::from_load_status(status::ALREADY_INITIALIZED), Error::AlreadyLoaded);
        assert_eq!(
            Error::from_load_status(status::INVALID_SIGNATURE),
            Error::SignatureRefused { status: status::INVALID_SIGNATURE }
        );
        assert_eq!(
            Error::from_load_status(status::NOT_SUPPORTED),
            Error::NotSupported { function: None }
        );
        assert_eq!(
            Error::from_load_status(status::INVALID_PARAMETER),
            Error::ModuleMalformed { status: status::INVALID_PARAMETER }
        );
        assert_eq!(
            Error::from_load_status(status::NO_MEMORY),
            Error::Status { operation: "load".into(), status: status::NO_MEMORY }
        );
    }

    #[test]
    fn access_denied_while_executing_is_an_allow_list_refusal_with_the_argument() {
        let error = Error::from_execute_status("ioctl_read_msr", &[0x10], status::ACCESS_DENIED);
        assert_eq!(
            error,
            Error::ArgumentDenied { function: "ioctl_read_msr".into(), argument: Some(0x10) }
        );
    }

    #[test]
    fn execute_statuses_map_to_their_causes() {
        let map = |status| Error::from_execute_status("ioctl_x", &[], status);
        let function = || "ioctl_x".to_owned();
        assert_eq!(
            map(status::OBJECT_NAME_NOT_FOUND),
            Error::FunctionNotFound { function: function() }
        );
        assert_eq!(
            map(status::INVALID_PARAMETER),
            Error::InvalidParameter { function: function() }
        );
        assert_eq!(map(status::BUFFER_TOO_SMALL), Error::InvalidParameter { function: function() });
        assert_eq!(map(status::DEVICE_NOT_READY), Error::NotReady { function: function() });
        assert_eq!(map(status::NOT_SUPPORTED), Error::NotSupported { function: Some(function()) });
        assert_eq!(map(status::NO_SUCH_DEVICE), Error::NoSuchDevice { function: function() });
        assert_eq!(map(status::DEVICE_BUSY), Error::Busy { function: function() });
        assert_eq!(map(status::IO_TIMEOUT), Error::Timeout { function: function() });
        assert_eq!(map(status::RETRY), Error::Retry { function: function() });
        assert_eq!(
            map(status::IO_DEVICE_ERROR),
            Error::DeviceError { function: function(), status: status::IO_DEVICE_ERROR }
        );
        assert_eq!(map(0xC000_0999), Error::Status { operation: function(), status: 0xC000_0999 });
    }

    #[test]
    fn bus_faults_exclude_a_missing_device() {
        let fault = |status| Error::from_execute_status("ioctl_x", &[], status).is_bus_fault();
        assert!(fault(status::DEVICE_BUSY));
        assert!(fault(status::IO_TIMEOUT));
        assert!(fault(status::RETRY));
        assert!(fault(status::IO_DEVICE_ERROR));
        assert!(!fault(status::NO_SUCH_DEVICE), "an empty SMBus slot is not a fault");
        assert!(!fault(status::ACCESS_DENIED));
    }

    #[test]
    fn display_names_the_received_status_and_the_expectation() {
        let text = Error::from_execute_status("ioctl_read_msr", &[0x10], status::ACCESS_DENIED)
            .to_string();
        assert_eq!(
            text,
            "(ioctl_read_msr): expected an argument on the module's allow-list | received: \
             0x10, 0xC0000022 (STATUS_ACCESS_DENIED)"
        );
    }

    #[test]
    fn a_faulting_msr_read_is_named_in_the_error_text() {
        let text =
            Error::from_execute_status("ioctl_read_msr", &[0x64D], status::PRIVILEGED_INSTRUCTION)
                .to_string();
        assert_eq!(
            text,
            "(pawnio.ioctl_read_msr): expected 0x00000000 (STATUS_SUCCESS) | received: \
             0xC0000096 (STATUS_PRIVILEGED_INSTRUCTION)"
        );
    }

    #[test]
    fn display_of_an_unnamed_status_shows_the_hex_value() {
        let text = Error::Status { operation: "load".into(), status: 0xC000_0999 }.to_string();
        assert!(text.contains("received: 0xC0000999"), "{text}");
        assert!(text.contains("expected 0x00000000 (STATUS_SUCCESS)"), "{text}");
    }

    #[test]
    fn every_named_status_has_a_distinct_name() {
        let all = [
            status::SUCCESS,
            status::DEVICE_BUSY,
            status::UNSUCCESSFUL,
            status::NOT_IMPLEMENTED,
            status::INVALID_PARAMETER,
            status::NO_SUCH_DEVICE,
            status::NO_MEMORY,
            status::ACCESS_DENIED,
            status::BUFFER_TOO_SMALL,
            status::OBJECT_NAME_NOT_FOUND,
            status::PRIVILEGED_INSTRUCTION,
            status::DEVICE_NOT_READY,
            status::IO_TIMEOUT,
            status::NOT_SUPPORTED,
            status::IO_DEVICE_ERROR,
            status::DEVICE_PROTOCOL_ERROR,
            status::NOT_FOUND,
            status::RETRY,
            status::ALREADY_INITIALIZED,
            status::INVALID_SIGNATURE,
        ];
        let mut names: Vec<_> = all.iter().filter_map(|s| status_name(*s)).collect();
        assert_eq!(names.len(), all.len(), "every listed status has a name");
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), all.len(), "names are unique");
    }
}
