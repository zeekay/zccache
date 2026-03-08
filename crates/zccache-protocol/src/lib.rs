//! IPC protocol types and serialization for zccache.
//!
//! Defines the message types exchanged between CLI/wrapper and daemon,
//! and provides serialization/deserialization using bincode.

pub mod messages;

pub use messages::*;

use bytes::{Buf, BufMut, BytesMut};

/// Serialize a message to a length-prefixed byte buffer.
///
/// Format: `[4-byte little-endian length][bincode payload]`
///
/// # Errors
///
/// Returns an error if serialization fails.
pub fn encode_message<T: serde::Serialize>(msg: &T) -> Result<BytesMut, ProtocolError> {
    let payload =
        bincode::serialize(msg).map_err(|e| ProtocolError::Serialization(e.to_string()))?;
    let len: u32 = payload
        .len()
        .try_into()
        .map_err(|_| ProtocolError::MessageTooLarge(payload.len()))?;

    let mut buf = BytesMut::with_capacity(4 + payload.len());
    buf.put_u32_le(len);
    buf.extend_from_slice(&payload);
    Ok(buf)
}

/// Try to decode a message from a byte buffer.
///
/// Returns `None` if the buffer does not contain a complete message.
/// Advances the buffer past the consumed message on success.
///
/// # Errors
///
/// Returns an error if deserialization fails.
pub fn decode_message<T: serde::de::DeserializeOwned>(
    buf: &mut BytesMut,
) -> Result<Option<T>, ProtocolError> {
    if buf.len() < 4 {
        return Ok(None);
    }

    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;

    if len > MAX_MESSAGE_SIZE {
        return Err(ProtocolError::MessageTooLarge(len));
    }

    if buf.len() < 4 + len {
        return Ok(None);
    }

    buf.advance(4);
    let payload = buf.split_to(len);
    let msg =
        bincode::deserialize(&payload).map_err(|e| ProtocolError::Deserialization(e.to_string()))?;
    Ok(Some(msg))
}

/// Maximum message size (16 MB).
const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

/// Protocol-level errors.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("deserialization error: {0}")]
    Deserialization(String),

    #[error("message too large: {0} bytes")]
    MessageTooLarge(usize),
}
