//! IPC transport layer.
//!
//! Provides platform-abstracted IPC using named pipes on Windows
//! and Unix domain sockets on Unix. Messages are length-prefixed
//! bincode via `zccache-protocol`.

use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::error::IpcError;

// ── Platform-specific connection inner ──────────────────────────────

#[cfg(unix)]
type StreamType = tokio::net::UnixStream;

#[cfg(windows)]
type StreamType = tokio::net::windows::named_pipe::NamedPipeServer;

/// A bidirectional IPC connection that sends/receives protocol messages.
///
/// On Unix this wraps a `UnixStream`. On Windows this wraps a
/// `NamedPipeServer` (server-side) or `NamedPipeClient` (client-side).
/// Both sides use the same send/recv interface.
pub struct IpcConnection {
    reader: tokio::io::ReadHalf<StreamType>,
    writer: tokio::io::WriteHalf<StreamType>,
    read_buf: BytesMut,
}

/// Client-side IPC connection (Windows uses a different type).
#[cfg(windows)]
pub struct IpcClientConnection {
    reader: tokio::io::ReadHalf<tokio::net::windows::named_pipe::NamedPipeClient>,
    writer: tokio::io::WriteHalf<tokio::net::windows::named_pipe::NamedPipeClient>,
    read_buf: BytesMut,
}

// ── IpcConnection impl (server-side on Windows, both on Unix) ───────

impl IpcConnection {
    /// Send a serializable message over the connection.
    pub async fn send<T: serde::Serialize>(&mut self, msg: &T) -> Result<(), IpcError> {
        let buf = zccache_protocol::encode_message(msg)?;
        self.writer.write_all(&buf).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// Receive a deserializable message from the connection.
    ///
    /// Returns `None` if the connection was closed cleanly.
    pub async fn recv<T: serde::de::DeserializeOwned>(&mut self) -> Result<Option<T>, IpcError> {
        loop {
            if let Some(msg) = zccache_protocol::decode_message::<T>(&mut self.read_buf)? {
                return Ok(Some(msg));
            }
            let mut tmp = [0u8; 4096];
            let n = self.reader.read(&mut tmp).await?;
            if n == 0 {
                if self.read_buf.is_empty() {
                    return Ok(None);
                }
                return Err(IpcError::ConnectionClosed);
            }
            self.read_buf.extend_from_slice(&tmp[..n]);
        }
    }
}

// ── IpcClientConnection (Windows client-side) ───────────────────────

#[cfg(windows)]
impl IpcClientConnection {
    /// Send a serializable message over the connection.
    pub async fn send<T: serde::Serialize>(&mut self, msg: &T) -> Result<(), IpcError> {
        let buf = zccache_protocol::encode_message(msg)?;
        self.writer.write_all(&buf).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// Receive a deserializable message from the connection.
    ///
    /// Returns `None` if the connection was closed cleanly.
    pub async fn recv<T: serde::de::DeserializeOwned>(&mut self) -> Result<Option<T>, IpcError> {
        loop {
            if let Some(msg) = zccache_protocol::decode_message::<T>(&mut self.read_buf)? {
                return Ok(Some(msg));
            }
            let mut tmp = [0u8; 4096];
            let n = self.reader.read(&mut tmp).await?;
            if n == 0 {
                if self.read_buf.is_empty() {
                    return Ok(None);
                }
                return Err(IpcError::ConnectionClosed);
            }
            self.read_buf.extend_from_slice(&tmp[..n]);
        }
    }
}

// ── IpcListener ─────────────────────────────────────────────────────

/// Listens for incoming IPC connections.
pub struct IpcListener {
    inner: ListenerInner,
}

#[cfg(unix)]
struct ListenerInner {
    listener: tokio::net::UnixListener,
}

#[cfg(windows)]
struct ListenerInner {
    endpoint: String,
    /// Pre-created pipe instance waiting for a client.
    pending: tokio::net::windows::named_pipe::NamedPipeServer,
}

impl IpcListener {
    /// Bind to the given endpoint and start listening.
    pub fn bind(endpoint: &str) -> Result<Self, IpcError> {
        #[cfg(unix)]
        {
            // Remove stale socket if it exists
            let _ = std::fs::remove_file(endpoint);
            if let Some(parent) = std::path::Path::new(endpoint).parent() {
                std::fs::create_dir_all(parent)?;
            }
            let listener = tokio::net::UnixListener::bind(endpoint)?;
            Ok(Self {
                inner: ListenerInner { listener },
            })
        }
        #[cfg(windows)]
        {
            use tokio::net::windows::named_pipe::ServerOptions;

            // Create first pipe instance so clients can find it
            let pending = ServerOptions::new()
                .first_pipe_instance(true)
                .create(endpoint)?;

            Ok(Self {
                inner: ListenerInner {
                    endpoint: endpoint.to_string(),
                    pending,
                },
            })
        }
    }

    /// Accept a new connection.
    ///
    /// On Unix, returns an `IpcConnection` wrapping a `UnixStream`.
    /// On Windows, returns an `IpcConnection` wrapping a `NamedPipeServer`.
    pub async fn accept(&mut self) -> Result<IpcConnection, IpcError> {
        #[cfg(unix)]
        {
            let (stream, _addr) = self.inner.listener.accept().await?;
            let (reader, writer) = tokio::io::split(stream);
            Ok(IpcConnection {
                reader,
                writer,
                read_buf: BytesMut::with_capacity(4096),
            })
        }
        #[cfg(windows)]
        {
            use tokio::net::windows::named_pipe::ServerOptions;

            // Wait for a client to connect to the pending pipe
            self.inner.pending.connect().await?;

            // Take the connected pipe and create a new pending one
            let connected = ServerOptions::new()
                .first_pipe_instance(false)
                .create(&self.inner.endpoint)?;
            let server = std::mem::replace(&mut self.inner.pending, connected);

            let (reader, writer) = tokio::io::split(server);
            Ok(IpcConnection {
                reader,
                writer,
                read_buf: BytesMut::with_capacity(4096),
            })
        }
    }
}

// ── Client connect ──────────────────────────────────────────────────

/// Connect to an IPC endpoint as a client.
///
/// On Unix, returns an `IpcConnection`. On Windows, returns an
/// `IpcClientConnection` (which has the same send/recv interface).
#[cfg(unix)]
pub async fn connect(endpoint: &str) -> Result<IpcConnection, IpcError> {
    let stream = tokio::net::UnixStream::connect(endpoint).await?;
    let (reader, writer) = tokio::io::split(stream);
    Ok(IpcConnection {
        reader,
        writer,
        read_buf: BytesMut::with_capacity(4096),
    })
}

/// Connect to an IPC endpoint as a client.
#[cfg(windows)]
pub async fn connect(endpoint: &str) -> Result<IpcClientConnection, IpcError> {
    use tokio::net::windows::named_pipe::ClientOptions;

    // Retry loop for ERROR_PIPE_BUSY with bounded retries.
    const MAX_PIPE_BUSY_RETRIES: u32 = 100; // 100 × 50ms = 5s max
    let client = {
        let mut attempts = 0u32;
        loop {
            match ClientOptions::new().open(endpoint) {
                Ok(client) => break client,
                Err(e) if e.raw_os_error() == Some(231) => {
                    // ERROR_PIPE_BUSY = 231, wait and retry
                    attempts += 1;
                    if attempts >= MAX_PIPE_BUSY_RETRIES {
                        return Err(IpcError::Io(e));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(e) => return Err(IpcError::Io(e)),
            }
        }
    };

    let (reader, writer) = tokio::io::split(client);
    Ok(IpcClientConnection {
        reader,
        writer,
        read_buf: BytesMut::with_capacity(4096),
    })
}

/// Generate a unique test endpoint name.
pub fn unique_test_endpoint() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();

    #[cfg(unix)]
    {
        format!("/tmp/zccache-test-{pid}-{id}.sock")
    }
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\zccache-test-{pid}-{id}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zccache_protocol::{Request, Response};

    #[tokio::test]
    async fn test_ping_pong() {
        let endpoint = unique_test_endpoint();
        let mut listener = IpcListener::bind(&endpoint).unwrap();

        let server = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            let msg: Option<Request> = conn.recv().await.unwrap();
            assert_eq!(msg, Some(Request::Ping));
            conn.send(&Response::Pong).await.unwrap();
        });

        let mut client = connect(&endpoint).await.unwrap();
        client.send(&Request::Ping).await.unwrap();
        let resp: Option<Response> = client.recv().await.unwrap();
        assert_eq!(resp, Some(Response::Pong));

        server.await.unwrap();
    }

    #[tokio::test]
    async fn test_multiple_messages() {
        let endpoint = unique_test_endpoint();
        let mut listener = IpcListener::bind(&endpoint).unwrap();

        let server = tokio::spawn(async move {
            let mut conn = listener.accept().await.unwrap();
            for _ in 0..5 {
                let msg: Option<Request> = conn.recv().await.unwrap();
                assert_eq!(msg, Some(Request::Ping));
                conn.send(&Response::Pong).await.unwrap();
            }
        });

        let mut client = connect(&endpoint).await.unwrap();
        for _ in 0..5 {
            client.send(&Request::Ping).await.unwrap();
            let resp: Option<Response> = client.recv().await.unwrap();
            assert_eq!(resp, Some(Response::Pong));
        }

        server.await.unwrap();
    }

    #[tokio::test]
    async fn test_connection_closed() {
        let endpoint = unique_test_endpoint();
        let mut listener = IpcListener::bind(&endpoint).unwrap();

        let server = tokio::spawn(async move {
            let _conn = listener.accept().await.unwrap();
            // Drop connection immediately
        });

        // Small delay to let server create pipe and start accepting
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let mut client = connect(&endpoint).await.unwrap();
        // Give server time to accept and drop
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let resp: Result<Option<Response>, _> = client.recv().await;
        // Should get None (clean close) or ConnectionClosed or broken pipe
        match resp {
            Ok(None) => {}
            Err(IpcError::ConnectionClosed) => {}
            Err(IpcError::Io(_)) => {}
            other => panic!("unexpected result: {other:?}"),
        }

        server.await.unwrap();
    }
}
