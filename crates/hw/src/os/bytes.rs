//! Bounds-checked little-endian reads from OS reply buffers.
//!
//! Every reader returns `None` when the field does not fit in the buffer, so
//! a short or damaged reply never panics and never reads past its end.

/// The byte at `offset`.
pub(crate) fn read_u8(bytes: &[u8], offset: usize) -> Option<u8> {
    bytes.get(offset).copied()
}

/// The little-endian `u16` at `offset`.
pub(crate) fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?))
}

/// The little-endian `u32` at `offset`.
pub(crate) fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?))
}

/// The little-endian `u64` at `offset`.
pub(crate) fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(offset..offset.checked_add(8)?)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BYTES: [u8; 8] = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];

    #[test]
    fn fields_are_read_little_endian_at_their_offset() {
        assert_eq!(read_u8(&BYTES, 7), Some(0x08));
        assert_eq!(read_u16(&BYTES, 1), Some(0x0302));
        assert_eq!(read_u32(&BYTES, 2), Some(0x0605_0403));
        assert_eq!(read_u64(&BYTES, 0), Some(0x0807_0605_0403_0201));
    }

    #[test]
    fn field_that_does_not_fit_is_none() {
        assert_eq!(read_u8(&BYTES, 8), None);
        assert_eq!(read_u16(&BYTES, 7), None);
        assert_eq!(read_u32(&BYTES, 5), None);
        assert_eq!(read_u64(&BYTES, 1), None);
    }

    #[test]
    fn offset_at_the_edge_of_usize_does_not_overflow() {
        assert_eq!(read_u16(&BYTES, usize::MAX), None);
        assert_eq!(read_u32(&BYTES, usize::MAX - 1), None);
        assert_eq!(read_u64(&BYTES, usize::MAX), None);
    }
}
