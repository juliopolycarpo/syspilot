//! The driver's device interface: path, IOCTL codes and buffer layouts.
//!
//! Everything here is plain data and pure functions so the encoding can be
//! tested without a driver.

use crate::error::Error;

/// Win32 path of the device (`\Device\PawnIO` through the object manager
/// root).
pub const DEVICE_PATH: &str = r"\\?\GLOBALROOT\Device\PawnIO";

/// Device type PawnIO registers (`k_device_type` in `pawnio_um.h`).
const DEVICE_TYPE: u32 = 0xA1B2;

/// `CTL_CODE(DEVICE_TYPE, function, METHOD_BUFFERED, FILE_ANY_ACCESS)`.
const fn control_code(function: u32) -> u32 {
    const METHOD_BUFFERED: u32 = 0;
    const FILE_ANY_ACCESS: u32 = 0;
    (DEVICE_TYPE << 16) | (FILE_ANY_ACCESS << 14) | (function << 2) | METHOD_BUFFERED
}

/// Loads a module into the handle. Input: the release `.bin` file.
pub const IOCTL_LOAD_BINARY: u32 = control_code(0x821);
/// Calls a module function. Input: name and cells, output: cells.
pub const IOCTL_EXECUTE_FN: u32 = control_code(0x841);
/// Returns the driver version. Output: exactly four bytes.
pub const IOCTL_VERSION: u32 = control_code(0x861);

/// Bytes the function name occupies at the start of an execute request.
pub const FUNCTION_NAME_BYTES: usize = 32;
/// Longest function name: the driver needs a terminating NUL in the 32 bytes.
pub const MAX_FUNCTION_NAME_LEN: usize = FUNCTION_NAME_BYTES - 1;
/// Every callable function starts with this prefix.
pub const FUNCTION_PREFIX: &str = "ioctl_";

/// Checks `name` the way the driver would, so a bad name fails with a
/// precise error before any IOCTL.
pub fn validate_function_name(name: &str) -> Result<(), Error> {
    let reject =
        |reason: &'static str| Error::InvalidFunctionName { name: name.to_owned(), reason };
    if !name.is_ascii() {
        return Err(reject("not ASCII"));
    }
    if name.len() > MAX_FUNCTION_NAME_LEN {
        return Err(reject("longer than 31 characters"));
    }
    if !name.starts_with(FUNCTION_PREFIX) {
        return Err(reject("does not start with `ioctl_`"));
    }
    if name.len() == FUNCTION_PREFIX.len() {
        return Err(reject("nothing follows the `ioctl_` prefix"));
    }
    if name.bytes().any(|byte| byte == 0) {
        return Err(reject("contains a NUL"));
    }
    Ok(())
}

/// Builds the input of `IOCTL_EXECUTE_FN`: the name, zero-padded to
/// [`FUNCTION_NAME_BYTES`], then each cell as a little-endian `u64`.
pub fn encode_execute_input(name: &str, cells: &[u64]) -> Result<Vec<u8>, Error> {
    validate_function_name(name)?;
    let bytes = FUNCTION_NAME_BYTES + std::mem::size_of_val(cells);
    if u32::try_from(bytes).is_err() {
        return Err(Error::BufferTooLarge { bytes });
    }
    let mut buffer = Vec::with_capacity(bytes);
    buffer.extend_from_slice(name.as_bytes());
    buffer.resize(FUNCTION_NAME_BYTES, 0);
    for cell in cells {
        buffer.extend_from_slice(&cell.to_le_bytes());
    }
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_codes_match_the_published_values() {
        assert_eq!(IOCTL_LOAD_BINARY, 0xA1B2_2084);
        assert_eq!(IOCTL_EXECUTE_FN, 0xA1B2_2104);
        assert_eq!(IOCTL_VERSION, 0xA1B2_2184);
    }

    #[test]
    fn input_is_name_padded_to_32_bytes_then_little_endian_cells() {
        let input = encode_execute_input("ioctl_read_msr", &[0x1A2, u64::MAX]).unwrap();
        assert_eq!(input.len(), 32 + 16);
        assert_eq!(&input[..14], b"ioctl_read_msr");
        assert!(input[14..32].iter().all(|b| *b == 0), "name padding must be zero");
        assert_eq!(&input[32..40], &[0xA2, 0x01, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&input[40..48], &[0xFF; 8]);
    }

    #[test]
    fn input_without_cells_is_just_the_name_block() {
        let input = encode_execute_input("ioctl_find_bars", &[]).unwrap();
        assert_eq!(input.len(), FUNCTION_NAME_BYTES);
    }

    #[test]
    fn a_31_character_name_is_accepted_and_32_is_not() {
        let longest = format!("{FUNCTION_PREFIX}{}", "a".repeat(25));
        assert_eq!(longest.len(), 31);
        assert!(validate_function_name(&longest).is_ok());
        let too_long = format!("{longest}a");
        assert!(matches!(
            validate_function_name(&too_long),
            Err(Error::InvalidFunctionName { reason: "longer than 31 characters", .. })
        ));
    }

    #[test]
    fn names_without_the_prefix_are_rejected() {
        for name in ["read_msr", "IOCTL_read_msr", "", "xioctl_read"] {
            assert!(
                matches!(validate_function_name(name), Err(Error::InvalidFunctionName { .. })),
                "{name:?} must be rejected"
            );
        }
    }

    #[test]
    fn a_bare_prefix_non_ascii_and_nul_are_rejected() {
        assert!(validate_function_name("ioctl_").is_err());
        assert!(validate_function_name("ioctl_lê").is_err());
        assert!(validate_function_name("ioctl_a\0b").is_err());
    }

    #[test]
    fn encoding_rejects_a_bad_name_before_building_a_buffer() {
        let error = encode_execute_input("not_ioctl", &[1]).unwrap_err();
        assert!(error.to_string().contains("\"not_ioctl\""), "{error}");
    }
}
