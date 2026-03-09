//! IPC error types.

/// Errors that can occur during IPC operations.
#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("protocol error: {0}")]
    Protocol(#[from] zccache_protocol::ProtocolError),

    #[error("connection closed")]
    ConnectionClosed,

    #[error("endpoint error: {0}")]
    Endpoint(String),
}
