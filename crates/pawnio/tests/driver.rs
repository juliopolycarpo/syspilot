//! Driver tests: they talk to the installed PawnIO driver and read (never
//! write) hardware registers.
//!
//! They run only with `SYSPILOT_TEST_PAWNIO=1`, from an elevated shell, on a
//! machine with PawnIO installed and the modules fetched by
//! `cargo xtask fetch-modules`. Otherwise each test prints why it is
//! skipped and passes.
//!
//! The statuses asserted here are the facts `docs/research/pawnio.md`
//! records as observed on real hardware.

use std::path::PathBuf;

use syspilot_pawnio::{
    Error, Executor, MIN_DRIVER_VERSION, PawnIo, Version, installed_version, status,
};

/// `IA32_TEMPERATURE_TARGET`; on the module's read allow-list.
const MSR_TEMPERATURE_TARGET: u64 = 0x1A2;
/// `IA32_TIME_STAMP_COUNTER`; deliberately not on the module's read allow-list.
const MSR_TIME_STAMP_COUNTER: u64 = 0x10;

/// Whether the driver tests are enabled; prints the reason when not.
fn enabled(test: &str) -> bool {
    if std::env::var("SYSPILOT_TEST_PAWNIO").as_deref() == Ok("1") {
        return true;
    }
    eprintln!("skipping {test}: set SYSPILOT_TEST_PAWNIO=1 (elevated, PawnIO installed) to run it");
    false
}

fn modules_dir() -> PathBuf {
    match std::env::var_os("SYSPILOT_PAWNIO_MODULES") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../pawnio-modules"),
    }
}

fn read_module(name: &str) -> Vec<u8> {
    let path = modules_dir().join(name);
    std::fs::read(&path).unwrap_or_else(|error| {
        panic!(
            "(driver test): expected module {} | received: {error}; run `cargo xtask fetch-modules`",
            path.display()
        )
    })
}

fn open_driver() -> PawnIo {
    PawnIo::open().unwrap_or_else(|error| {
        panic!("(driver test): expected an open driver | received: {error}")
    })
}

/// A handle with `IntelMSR.bin` loaded. `None` (after printing why) on a
/// machine whose CPU is not Intel, where the module refuses to load.
fn intel_msr_module(test: &str) -> Option<PawnIo> {
    let device = open_driver();
    match device.load(&read_module("IntelMSR.bin")) {
        Ok(()) => Some(device),
        Err(Error::NotSupported { function: None }) => {
            eprintln!("skipping {test}: IntelMSR.bin refused this CPU (not Intel)");
            None
        }
        Err(other) => panic!("(driver test): expected IntelMSR.bin to load | received: {other}"),
    }
}

#[test]
fn driver_opens_and_reports_a_supported_version() {
    if !enabled("driver_opens_and_reports_a_supported_version") {
        return;
    }
    let device = open_driver();
    let version = device.driver_version().unwrap();
    assert!(
        version >= MIN_DRIVER_VERSION,
        "(version): expected >= {MIN_DRIVER_VERSION} | received: {version}"
    );
    // The uninstall entry and the running driver must agree.
    let installed = installed_version().expect("(installed_version): expected the uninstall entry");
    assert_eq!(
        Version::parse(&installed),
        Some(version),
        "(installed_version): expected the running driver's version | received: {installed:?}"
    );
}

#[test]
fn intel_msr_module_reads_the_temperature_target() {
    if !enabled("intel_msr_module_reads_the_temperature_target") {
        return;
    }
    let Some(device) = intel_msr_module("intel_msr_module_reads_the_temperature_target") else {
        return;
    };
    let value = device.execute_scalar("ioctl_read_msr", &[MSR_TEMPERATURE_TARGET]).unwrap();
    let tjmax = (value >> 16) & 0xFF;
    assert!(
        (60..=125).contains(&tjmax),
        "(msr 0x1A2): expected TjMax in 60..=125 | received: {tjmax} (raw {value:#x})"
    );
}

#[test]
fn a_msr_off_the_allow_list_is_denied_with_the_argument() {
    if !enabled("a_msr_off_the_allow_list_is_denied_with_the_argument") {
        return;
    }
    let Some(device) = intel_msr_module("a_msr_off_the_allow_list_is_denied_with_the_argument")
    else {
        return;
    };
    let error = device.execute_scalar("ioctl_read_msr", &[MSR_TIME_STAMP_COUNTER]).unwrap_err();
    assert_eq!(
        error,
        Error::ArgumentDenied {
            function: "ioctl_read_msr".into(),
            argument: Some(MSR_TIME_STAMP_COUNTER)
        },
        "(ioctl_read_msr): expected STATUS_ACCESS_DENIED mapped to ArgumentDenied"
    );
}

#[test]
fn an_unknown_function_is_reported_as_not_found() {
    if !enabled("an_unknown_function_is_reported_as_not_found") {
        return;
    }
    let Some(device) = intel_msr_module("an_unknown_function_is_reported_as_not_found") else {
        return;
    };
    let error = device.execute_scalar("ioctl_no_such_function", &[0]).unwrap_err();
    assert_eq!(error, Error::FunctionNotFound { function: "ioctl_no_such_function".into() });
}

#[test]
fn wrong_cell_counts_are_rejected_as_invalid_parameters() {
    if !enabled("wrong_cell_counts_are_rejected_as_invalid_parameters") {
        return;
    }
    let Some(device) = intel_msr_module("wrong_cell_counts_are_rejected_as_invalid_parameters")
    else {
        return;
    };
    let too_many_inputs = device.execute_scalar("ioctl_read_msr", &[MSR_TEMPERATURE_TARGET, 0]);
    assert_eq!(too_many_inputs, Err(Error::InvalidParameter { function: "ioctl_read_msr".into() }));
    let mut two_outputs = [0u64; 2];
    let too_many_outputs =
        device.execute("ioctl_read_msr", &[MSR_TEMPERATURE_TARGET], &mut two_outputs);
    assert_eq!(
        too_many_outputs,
        Err(Error::InvalidParameter { function: "ioctl_read_msr".into() })
    );
}

#[test]
fn calling_a_function_before_loading_a_module_is_an_error() {
    if !enabled("calling_a_function_before_loading_a_module_is_an_error") {
        return;
    }
    let device = open_driver();
    let error = device.execute_scalar("ioctl_read_msr", &[MSR_TEMPERATURE_TARGET]).unwrap_err();
    eprintln!("execute on an empty handle: {error}");
    // 2.2.0 answers STATUS_INVALID_PARAMETER (see `dispatch_irp`); an older
    // release may say STATUS_DEVICE_NOT_READY. Either is a typed refusal.
    assert!(
        matches!(error, Error::NotReady { .. } | Error::InvalidParameter { .. }),
        "(ioctl_read_msr): expected NotReady or InvalidParameter on an empty handle | received: {error}"
    );
}

#[test]
fn a_second_load_on_one_handle_is_refused() {
    if !enabled("a_second_load_on_one_handle_is_refused") {
        return;
    }
    let Some(device) = intel_msr_module("a_second_load_on_one_handle_is_refused") else {
        return;
    };
    assert_eq!(device.load(&read_module("IntelMSR.bin")), Err(Error::AlreadyLoaded));
}

#[test]
fn a_module_with_a_changed_byte_fails_the_signature_check() {
    if !enabled("a_module_with_a_changed_byte_fails_the_signature_check") {
        return;
    }
    let mut blob = read_module("IntelMSR.bin");
    let signature_len = u32::from_le_bytes([blob[0], blob[1], blob[2], blob[3]]) as usize;
    let last = blob.len() - 1;
    assert!(
        4 + signature_len < last,
        "(IntelMSR.bin): expected an AMX payload after the signature"
    );
    blob[last] ^= 0x01;

    let device = open_driver();
    match device.load(&blob) {
        Err(Error::SignatureRefused { status: observed }) => {
            eprintln!("changed byte: refused with {observed:#010X}");
            assert_eq!(observed, status::INVALID_SIGNATURE);
        }
        other => panic!("(load): expected SignatureRefused | received: {other:?}"),
    }
}

#[test]
fn malformed_blobs_are_refused_with_a_typed_error() {
    if !enabled("malformed_blobs_are_refused_with_a_typed_error") {
        return;
    }
    // Too short for the length prefix.
    let device = open_driver();
    assert!(matches!(device.load(&[1, 2]), Err(Error::ModuleMalformed { .. })));

    // A signature length that runs past the end of the blob.
    let device = open_driver();
    assert!(matches!(device.load(&[0xFF; 64]), Err(Error::ModuleMalformed { .. })));

    // A well-formed header around garbage: the signature check refuses it.
    let device = open_driver();
    let mut unsigned = vec![0u8; 4];
    unsigned.extend_from_slice(b"not an amx module");
    let error = device.load(&unsigned).unwrap_err();
    eprintln!("empty signature over garbage: {error}");
    assert!(
        matches!(
            error,
            Error::SignatureRefused { .. } | Error::ModuleMalformed { .. } | Error::Status { .. }
        ),
        "(load): expected a typed refusal | received: {error}"
    );
}
