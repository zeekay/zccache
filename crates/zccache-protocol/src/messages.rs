//! Protocol message definitions.

use serde::{Deserialize, Serialize};

/// A request from client to daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    /// Health check.
    Ping,
    /// Request daemon shutdown.
    Shutdown,
    /// Request daemon status/statistics.
    Status,
    /// Look up a cached artifact by cache key.
    Lookup {
        /// Hex-encoded cache key.
        cache_key: String,
    },
    /// Store a compilation artifact.
    Store {
        /// Hex-encoded cache key.
        cache_key: String,
        /// The artifact data to store.
        artifact: ArtifactData,
    },
}

/// A response from daemon to client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    /// Response to Ping.
    Pong,
    /// Shutdown acknowledged.
    ShuttingDown,
    /// Daemon status information.
    Status(DaemonStatus),
    /// Cache lookup result.
    LookupResult(LookupResult),
    /// Store result.
    StoreResult(StoreResult),
    /// An error occurred processing the request.
    Error {
        /// Human-readable error message.
        message: String,
    },
}

/// Daemon status information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    /// Number of artifacts in cache.
    pub artifact_count: u64,
    /// Total size of cached artifacts in bytes.
    pub cache_size_bytes: u64,
    /// Number of entries in the metadata cache.
    pub metadata_entries: u64,
    /// Daemon uptime in seconds.
    pub uptime_secs: u64,
    /// Total cache hits since startup.
    pub cache_hits: u64,
    /// Total cache misses since startup.
    pub cache_misses: u64,
}

/// Result of a cache lookup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LookupResult {
    /// Cache hit.
    Hit {
        /// The cached artifact data.
        artifact: ArtifactData,
    },
    /// Cache miss.
    Miss,
}

/// Result of storing an artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StoreResult {
    /// Successfully stored.
    Stored,
    /// Already existed in cache.
    AlreadyExists,
}

/// Artifact data exchanged over the protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactData {
    /// The output files (filename to contents).
    pub outputs: Vec<ArtifactOutput>,
    /// Captured stdout from the compiler.
    pub stdout: Vec<u8>,
    /// Captured stderr from the compiler.
    pub stderr: Vec<u8>,
    /// Compiler exit code.
    pub exit_code: i32,
}

/// A single output file from compilation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactOutput {
    /// Relative filename (e.g., "foo.o").
    pub name: String,
    /// File contents.
    pub data: Vec<u8>,
}
