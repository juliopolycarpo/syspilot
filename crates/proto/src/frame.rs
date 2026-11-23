//! Length-prefixed `postcard` framing.
//!
//! A frame is a little-endian `u32` payload length followed by the payload.
//! The length is checked against [`MAX_FRAME_LEN`] before anything is
//! allocated, so a hostile peer cannot make the service reserve memory.

use std::fmt;
use std::io::{self, Read, Write};

use serde::Serialize;
use serde::de::DeserializeOwned;

/// Largest accepted payload, in bytes. The biggest legitimate message is the
/// tweak catalog with all its texts, which is well under this.
pub const MAX_FRAME_LEN: u32 = 1024 * 1024;

/// Why a frame could not be read or written.
#[derive(Debug)]
pub enum FrameError {
    /// The peer closed the connection cleanly between frames.
    Closed,
    /// The connection ended in the middle of a frame.
    Truncated {
        /// Bytes the frame header announced.
        expected: usize,
        /// Bytes that actually arrived.
        received: usize,
    },
    /// The announced payload length exceeds [`MAX_FRAME_LEN`].
    Oversized {
        /// Announced length.
        len: u32,
    },
    /// The payload is not a valid encoding of the expected type.
    Malformed(postcard::Error),
    /// The transport failed.
    Io(io::Error),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => f.write_str("(frame): connection closed"),
            Self::Truncated { expected, received } => {
                write!(f, "(frame): expected {expected} payload bytes | received: {received}")
            }
            Self::Oversized { len } => {
                write!(f, "(frame): expected length <= {MAX_FRAME_LEN} | received: {len}")
            }
            Self::Malformed(error) => write!(f, "(frame): payload does not decode: {error}"),
            Self::Io(error) => write!(f, "(frame): transport failed: {error}"),
        }
    }
}

impl std::error::Error for FrameError {}

impl From<io::Error> for FrameError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Encodes `message` and writes it as one frame.
pub fn write_frame<W: Write, T: Serialize>(writer: &mut W, message: &T) -> Result<(), FrameError> {
    let payload = postcard::to_stdvec(message).map_err(FrameError::Malformed)?;
    let len = u32::try_from(payload.len())
        .ok()
        .filter(|len| *len <= MAX_FRAME_LEN)
        .ok_or(FrameError::Oversized { len: u32::try_from(payload.len()).unwrap_or(u32::MAX) })?;
    // One write per frame keeps a frame atomic on a pipe.
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(&payload);
    writer.write_all(&frame)?;
    writer.flush()?;
    Ok(())
}

/// Reads one frame and decodes it.
pub fn read_frame<R: Read, T: DeserializeOwned>(reader: &mut R) -> Result<T, FrameError> {
    let mut header = [0u8; 4];
    match read_up_to(reader, &mut header)? {
        0 => return Err(FrameError::Closed),
        4 => {}
        received => return Err(FrameError::Truncated { expected: 4, received }),
    }
    let len = u32::from_le_bytes(header);
    if len > MAX_FRAME_LEN {
        return Err(FrameError::Oversized { len });
    }
    let mut payload = vec![0u8; len as usize];
    let received = read_up_to(reader, &mut payload)?;
    if received != payload.len() {
        return Err(FrameError::Truncated { expected: payload.len(), received });
    }
    postcard::from_bytes(&payload).map_err(FrameError::Malformed)
}

/// Fills `buffer` as far as the stream allows and returns the byte count.
/// A broken pipe counts as end of stream: that is how a named pipe reports
/// that the peer has gone.
fn read_up_to<R: Read>(reader: &mut R, buffer: &mut [u8]) -> Result<usize, FrameError> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => break,
            Err(error) => return Err(FrameError::Io(error)),
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn frame_round_trips() {
        let mut wire = Vec::new();
        write_frame(&mut wire, &("hello".to_owned(), 42u32)).unwrap();
        let decoded: (String, u32) = read_frame(&mut Cursor::new(wire)).unwrap();
        assert_eq!(decoded, ("hello".to_owned(), 42));
    }

    #[test]
    fn end_of_stream_between_frames_is_a_clean_close() {
        let result: Result<u32, _> = read_frame(&mut Cursor::new(Vec::new()));
        assert!(matches!(result, Err(FrameError::Closed)), "{result:?}");
    }

    #[test]
    fn truncated_header_is_rejected() {
        let result: Result<u32, _> = read_frame(&mut Cursor::new(vec![1, 0]));
        assert!(
            matches!(result, Err(FrameError::Truncated { expected: 4, received: 2 })),
            "{result:?}"
        );
    }

    #[test]
    fn truncated_payload_is_rejected() {
        let mut wire = Vec::new();
        write_frame(&mut wire, &"a longer payload".to_owned()).unwrap();
        wire.truncate(wire.len() - 3);
        let result: Result<String, _> = read_frame(&mut Cursor::new(wire));
        assert!(
            matches!(result, Err(FrameError::Truncated { expected: 17, received: 14 })),
            "{result:?}"
        );
    }

    #[test]
    fn oversized_frame_is_rejected_before_allocating() {
        let wire = (MAX_FRAME_LEN + 1).to_le_bytes().to_vec();
        let result: Result<String, _> = read_frame(&mut Cursor::new(wire));
        assert!(matches!(result, Err(FrameError::Oversized { len }) if len == MAX_FRAME_LEN + 1));
    }

    #[test]
    fn oversized_message_is_not_written() {
        let mut wire = Vec::new();
        let result = write_frame(&mut wire, &vec![0u8; MAX_FRAME_LEN as usize + 1]);
        assert!(matches!(result, Err(FrameError::Oversized { .. })), "{result:?}");
        assert!(wire.is_empty());
    }

    #[test]
    fn garbage_payload_is_malformed() {
        // Length 1, payload 0xFF: not a valid `bool`.
        let result: Result<bool, _> = read_frame(&mut Cursor::new(vec![1, 0, 0, 0, 0xFF]));
        assert!(matches!(result, Err(FrameError::Malformed(_))), "{result:?}");
    }
}
