//! Prost wire helpers and v16 dispatcher scaffolding.
//!
//! The live daemon wire remains v15 bincode today. This module intentionally
//! models the v16 prost path without changing `encode_message` /
//! `decode_message`, so old clients keep working while the generated protobuf
//! types and compatibility tests land first.

use bytes::{Buf, BufMut, BytesMut};
use prost::Message;

use super::{ProtocolError, BINCODE_PROTOCOL_VERSION, MAX_MESSAGE_SIZE, PROST_PROTOCOL_VERSION};

/// Generated protobuf schema for the planned zccache v1 wire.
pub mod zccache_v1 {
    include!(concat!(env!("OUT_DIR"), "/zccache.v1.rs"));
}

/// Environment variable reserved for the daemon wire migration fallback.
pub const WIRE_FORMAT_ENV: &str = "ZCCACHE_DAEMON_WIRE";

/// Supported daemon wire families.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireFormat {
    /// Current v15 bincode body.
    BincodeV15,
    /// Planned v16 prost body.
    ProstV16,
}

impl WireFormat {
    /// Protocol version carried in the existing frame header.
    #[must_use]
    pub const fn protocol_version(self) -> u32 {
        match self {
            Self::BincodeV15 => BINCODE_PROTOCOL_VERSION,
            Self::ProstV16 => PROST_PROTOCOL_VERSION,
        }
    }
}

/// Planned default for new clients once the live transport uses the dispatcher.
pub const DEFAULT_CLIENT_WIRE_FORMAT: WireFormat = WireFormat::ProstV16;

/// Return the wire family for a protocol-version header.
#[must_use]
pub const fn wire_format_for_protocol_version(version: u32) -> Option<WireFormat> {
    match version {
        BINCODE_PROTOCOL_VERSION => Some(WireFormat::BincodeV15),
        PROST_PROTOCOL_VERSION => Some(WireFormat::ProstV16),
        _ => None,
    }
}

/// Parse a `ZCCACHE_DAEMON_WIRE` value.
///
/// `None` and `auto` model the migration target: new clients prefer v16 prost,
/// while `bincode` remains the explicit v15 fallback spelling. The live
/// transport still calls the bincode helpers directly until the daemon
/// dispatcher is wired into IPC.
///
/// # Errors
///
/// Returns a message suitable for diagnostics when the value is not recognized.
pub fn wire_format_from_env_value(value: Option<&str>) -> Result<WireFormat, String> {
    let Some(value) = value else {
        return Ok(DEFAULT_CLIENT_WIRE_FORMAT);
    };

    match value.trim().to_ascii_lowercase().as_str() {
        "" | "auto" => Ok(DEFAULT_CLIENT_WIRE_FORMAT),
        "bincode" | "bincode-v15" | "v15" => Ok(WireFormat::BincodeV15),
        "prost" | "prost-v16" | "v16" => Ok(WireFormat::ProstV16),
        other => Err(format!(
            "invalid {WIRE_FORMAT_ENV}={other:?}; expected auto, bincode, or prost"
        )),
    }
}

/// Read `ZCCACHE_DAEMON_WIRE` from the process environment.
///
/// # Errors
///
/// Returns a message suitable for diagnostics when the value is not recognized.
pub fn wire_format_from_env() -> Result<WireFormat, String> {
    wire_format_from_env_value(std::env::var(WIRE_FORMAT_ENV).ok().as_deref())
}

/// Convert the narrow set of v16 prost daemon-control requests that the live
/// dispatcher can handle before the full enum conversion lands.
///
/// # Errors
///
/// Returns a clear diagnostic for missing or unsupported request bodies. The
/// caller should surface this as a daemon response instead of dropping the
/// connection.
pub fn supported_control_request_from_prost(
    request: zccache_v1::Request,
) -> Result<super::Request, String> {
    use zccache_v1::request::Body;

    match request.body {
        Some(Body::Ping(_)) => Ok(super::Request::Ping),
        Some(Body::Status(_)) => Ok(super::Request::Status),
        Some(Body::Shutdown(_)) => Ok(super::Request::Shutdown),
        Some(other) => Err(format!(
            "unsupported v16 prost request body {other:?}; only Ping, Status, and Shutdown are \
             supported before the full zccache prost conversion lands"
        )),
        None => Err(
            "unsupported v16 prost request: missing request body; only Ping, Status, and Shutdown \
             are supported before the full zccache prost conversion lands"
                .to_string(),
        ),
    }
}

/// Serialize a prost message to the planned v16 length-prefixed frame.
///
/// Format: `[4-byte LE length][4-byte LE protocol version][prost payload]`.
/// The length field covers the protocol version plus payload bytes, matching
/// the existing bincode frame envelope.
///
/// # Errors
///
/// Returns an error if prost encoding fails or the payload exceeds the frame
/// size budget.
pub fn encode_prost_message<M: Message>(msg: &M) -> Result<BytesMut, ProtocolError> {
    let mut payload = Vec::with_capacity(msg.encoded_len());
    msg.encode(&mut payload)
        .map_err(|e| ProtocolError::Serialization(e.to_string()))?;

    let frame_len: u32 = (4 + payload.len())
        .try_into()
        .map_err(|_| ProtocolError::MessageTooLarge(payload.len()))?;

    let mut buf = BytesMut::with_capacity(4 + 4 + payload.len());
    buf.put_u32_le(frame_len);
    buf.put_u32_le(PROST_PROTOCOL_VERSION);
    buf.extend_from_slice(&payload);
    Ok(buf)
}

/// Try to decode a v16 prost message from a byte buffer.
///
/// Returns `None` when the buffer does not contain a complete frame.
///
/// # Errors
///
/// Returns a version mismatch for non-v16 frames and a deserialization error
/// for malformed prost payloads.
pub fn decode_prost_message<M: Message + Default>(
    buf: &mut BytesMut,
) -> Result<Option<M>, ProtocolError> {
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

    if len < 4 {
        return Err(ProtocolError::Deserialization(
            "frame too small for protocol version".into(),
        ));
    }

    buf.advance(4);
    let frame = buf.split_to(len);

    let remote_ver = u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]);
    if remote_ver != PROST_PROTOCOL_VERSION {
        return Err(ProtocolError::VersionMismatch {
            expected: PROST_PROTOCOL_VERSION,
            received: remote_ver,
        });
    }

    M::decode(&frame[4..])
        .map(Some)
        .map_err(|e| ProtocolError::Deserialization(e.to_string()))
}
