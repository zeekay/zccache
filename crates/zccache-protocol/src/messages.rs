//! Protocol message definitions.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A request from client to daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// Start a new session with the daemon.
    SessionStart {
        /// Client process ID.
        client_pid: u32,
        /// Client working directory.
        working_dir: PathBuf,
        /// Optional path to a log file for this session.
        log_file: Option<PathBuf>,
        /// Whether to track per-session statistics.
        track_stats: bool,
    },
    /// Compile a source file within an existing session.
    Compile {
        /// Session ID from a prior SessionStart (UUID string).
        session_id: String,
        /// Compiler arguments (e.g., ["-c", "hello.cpp", "-o", "hello.o"]).
        args: Vec<String>,
        /// Working directory for the compilation.
        cwd: PathBuf,
        /// Path to the compiler executable (required).
        compiler: PathBuf,
        /// Client environment variables to pass to the compiler process.
        /// If `None`, the daemon's own environment is inherited (backward compat).
        /// If `Some`, the compiler process uses exactly these env vars.
        env: Option<Vec<(String, String)>>,
    },
    /// End a session.
    SessionEnd {
        /// Session ID to end (UUID string).
        session_id: String,
    },
    /// Query per-session statistics without ending the session.
    SessionStats {
        /// Session ID to query (UUID string).
        session_id: String,
    },
    /// Clear all caches (artifacts, metadata, dep graph).
    Clear,
    /// Single-roundtrip ephemeral compile: session start + compile + session end
    /// in one message. Used by the CLI in drop-in wrapper mode to avoid 3 IPC
    /// roundtrips per invocation.
    CompileEphemeral {
        /// Client process ID.
        client_pid: u32,
        /// Client working directory.
        working_dir: PathBuf,
        /// Path to the compiler executable.
        compiler: PathBuf,
        /// Compiler arguments (e.g., ["-c", "hello.cpp", "-o", "hello.o"]).
        args: Vec<String>,
        /// Working directory for the compilation.
        cwd: PathBuf,
        /// Client environment variables to pass to the compiler process.
        env: Option<Vec<(String, String)>>,
    },
    /// Single-roundtrip ephemeral link/archive: used for `zccache ar ...` or
    /// `zccache ld ...` in drop-in wrapper mode.
    LinkEphemeral {
        /// Client process ID.
        client_pid: u32,
        /// Client working directory.
        working_dir: PathBuf,
        /// Path to the linker/archiver tool (ar, ld, lib.exe, link.exe, etc.).
        tool: PathBuf,
        /// Tool arguments (e.g., ["rcs", "libfoo.a", "a.o", "b.o"]).
        args: Vec<String>,
        /// Working directory for the link operation.
        cwd: PathBuf,
        /// Client environment variables.
        env: Option<Vec<(String, String)>>,
    },
}

/// A response from daemon to client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// Session successfully started.
    SessionStarted {
        /// Assigned session ID (UUID string).
        session_id: String,
    },
    /// Result of a compilation request.
    CompileResult {
        /// Compiler exit code.
        exit_code: i32,
        /// Captured stdout.
        stdout: Vec<u8>,
        /// Captured stderr.
        stderr: Vec<u8>,
        /// Whether this was served from cache.
        cached: bool,
    },
    /// Session ended successfully.
    SessionEnded {
        /// Per-session stats, if the session opted in to tracking.
        stats: Option<SessionStats>,
    },
    /// Mid-session statistics snapshot.
    SessionStatsResult {
        /// Per-session stats, if the session exists and opted in to tracking.
        stats: Option<SessionStats>,
    },
    /// Result of a link/archive request.
    LinkResult {
        /// Tool exit code.
        exit_code: i32,
        /// Captured stdout.
        stdout: Vec<u8>,
        /// Captured stderr.
        stderr: Vec<u8>,
        /// Whether this was served from cache.
        cached: bool,
        /// Non-determinism warning (if tool invocation uses non-deterministic flags).
        warning: Option<String>,
    },
    /// An error occurred processing the request.
    Error {
        /// Human-readable error message.
        message: String,
    },
    /// Cache cleared successfully.
    Cleared {
        /// Number of in-memory artifacts removed.
        artifacts_removed: u64,
        /// Number of metadata cache entries cleared.
        metadata_cleared: u64,
        /// Number of dep graph contexts cleared.
        dep_graph_contexts_cleared: u64,
        /// Bytes freed from on-disk artifact cache.
        on_disk_bytes_freed: u64,
    },
}

/// Daemon status information.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonStatus {
    /// Daemon version (e.g. "1.0.8"). Used by CLI to detect stale daemons.
    #[serde(default)]
    pub version: String,
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
    /// Total compile requests received.
    pub total_compilations: u64,
    /// Non-cacheable invocations (linking, preprocessing, etc.).
    pub non_cacheable: u64,
    /// Compilations that exited with non-zero status.
    pub compile_errors: u64,
    /// Estimated wall-clock time saved in milliseconds.
    pub time_saved_ms: u64,
    /// Total link/archive requests received.
    pub total_links: u64,
    /// Link cache hits.
    pub link_hits: u64,
    /// Link cache misses.
    pub link_misses: u64,
    /// Non-cacheable link invocations.
    pub link_non_cacheable: u64,
    /// Number of compilation contexts in the dependency graph.
    pub dep_graph_contexts: u64,
    /// Number of tracked files in the dependency graph.
    pub dep_graph_files: u64,
    /// Total sessions created since daemon start.
    pub sessions_total: u64,
    /// Currently active sessions.
    pub sessions_active: u64,
    /// Path to the cache directory.
    pub cache_dir: PathBuf,
}

/// Result of a cache lookup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StoreResult {
    /// Successfully stored.
    Stored,
    /// Already existed in cache.
    AlreadyExists,
}

/// Artifact data exchanged over the protocol.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

/// Per-session statistics, returned when the session opted in to tracking.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStats {
    /// Wall-clock duration of the session in milliseconds.
    pub duration_ms: u64,
    /// Total compile requests in this session.
    pub compilations: u64,
    /// Cache hits in this session.
    pub hits: u64,
    /// Cache misses (cold compiles) in this session.
    pub misses: u64,
    /// Non-cacheable invocations (linking, preprocessing, etc.).
    pub non_cacheable: u64,
    /// Compilations that exited with non-zero status.
    pub errors: u64,
    /// Estimated wall-clock time saved in milliseconds.
    pub time_saved_ms: u64,
    /// Distinct source files compiled.
    pub unique_sources: u64,
    /// Total artifact bytes served from cache.
    pub bytes_read: u64,
    /// Total artifact bytes stored into cache.
    pub bytes_written: u64,
}

/// A single output file from compilation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactOutput {
    /// Relative filename (e.g., "foo.o").
    pub name: String,
    /// File contents.
    pub data: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: roundtrip a value through bincode.
    fn roundtrip<T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug>(
        val: &T,
    ) {
        let bytes = bincode::serialize(val).unwrap();
        let decoded: T = bincode::deserialize(&bytes).unwrap();
        assert_eq!(*val, decoded);
    }

    #[test]
    fn session_stats_roundtrip() {
        let stats = SessionStats {
            duration_ms: 12345,
            compilations: 100,
            hits: 80,
            misses: 15,
            non_cacheable: 5,
            errors: 2,
            time_saved_ms: 8000,
            unique_sources: 42,
            bytes_read: 1024 * 1024,
            bytes_written: 512 * 1024,
        };
        roundtrip(&stats);
    }

    #[test]
    fn session_stats_default_zeros() {
        let stats = SessionStats {
            duration_ms: 0,
            compilations: 0,
            hits: 0,
            misses: 0,
            non_cacheable: 0,
            errors: 0,
            time_saved_ms: 0,
            unique_sources: 0,
            bytes_read: 0,
            bytes_written: 0,
        };
        roundtrip(&stats);
    }

    #[test]
    fn daemon_status_expanded_roundtrip() {
        let status = DaemonStatus {
            version: env!("CARGO_PKG_VERSION").to_string(),
            artifact_count: 892,
            cache_size_bytes: 147_000_000,
            metadata_entries: 5430,
            uptime_secs: 8040,
            cache_hits: 1089,
            cache_misses: 143,
            total_compilations: 1247,
            non_cacheable: 15,
            compile_errors: 3,
            time_saved_ms: 750_000,
            total_links: 50,
            link_hits: 38,
            link_misses: 10,
            link_non_cacheable: 2,
            dep_graph_contexts: 892,
            dep_graph_files: 4201,
            sessions_total: 41,
            sessions_active: 3,
            cache_dir: PathBuf::from("/home/user/.cache/zccache"),
        };
        roundtrip(&status);
    }

    #[test]
    fn session_start_with_track_stats_roundtrip() {
        let req = Request::SessionStart {
            client_pid: 1234,
            working_dir: PathBuf::from("/home/user/project"),
            log_file: None,
            track_stats: true,
        };
        roundtrip(&req);

        let req_no_stats = Request::SessionStart {
            client_pid: 1234,
            working_dir: PathBuf::from("/home/user/project"),
            log_file: None,
            track_stats: false,
        };
        roundtrip(&req_no_stats);
    }

    #[test]
    fn session_ended_with_stats_roundtrip() {
        let stats = SessionStats {
            duration_ms: 34000,
            compilations: 32,
            hits: 28,
            misses: 3,
            non_cacheable: 1,
            errors: 0,
            time_saved_ms: 8200,
            unique_sources: 30,
            bytes_read: 2_000_000,
            bytes_written: 500_000,
        };
        let resp = Response::SessionEnded { stats: Some(stats) };
        roundtrip(&resp);

        let resp_no_stats = Response::SessionEnded { stats: None };
        roundtrip(&resp_no_stats);
    }

    #[test]
    fn clear_request_roundtrip() {
        roundtrip(&Request::Clear);
    }

    #[test]
    fn cleared_response_roundtrip() {
        roundtrip(&Response::Cleared {
            artifacts_removed: 42,
            metadata_cleared: 100,
            dep_graph_contexts_cleared: 25,
            on_disk_bytes_freed: 1024 * 1024,
        });
    }

    #[test]
    fn compile_ephemeral_roundtrip() {
        roundtrip(&Request::CompileEphemeral {
            client_pid: 9876,
            working_dir: PathBuf::from("/home/user/project"),
            compiler: PathBuf::from("/usr/bin/clang++"),
            args: vec!["-c".into(), "main.cpp".into(), "-o".into(), "main.o".into()],
            cwd: PathBuf::from("/home/user/project/build"),
            env: Some(vec![("PATH".into(), "/usr/bin".into())]),
        });
        // Also test with env = None
        roundtrip(&Request::CompileEphemeral {
            client_pid: 1,
            working_dir: PathBuf::from("."),
            compiler: PathBuf::from("gcc"),
            args: vec![],
            cwd: PathBuf::from("."),
            env: None,
        });
    }

    #[test]
    fn link_ephemeral_roundtrip() {
        roundtrip(&Request::LinkEphemeral {
            client_pid: 5555,
            working_dir: PathBuf::from("/home/user/project"),
            tool: PathBuf::from("/usr/bin/ar"),
            args: vec!["rcs".into(), "libfoo.a".into(), "a.o".into(), "b.o".into()],
            cwd: PathBuf::from("/home/user/project/build"),
            env: Some(vec![("PATH".into(), "/usr/bin".into())]),
        });
        roundtrip(&Request::LinkEphemeral {
            client_pid: 1,
            working_dir: PathBuf::from("."),
            tool: PathBuf::from("lib.exe"),
            args: vec!["/OUT:foo.lib".into(), "a.obj".into()],
            cwd: PathBuf::from("."),
            env: None,
        });
    }

    #[test]
    fn link_result_roundtrip() {
        roundtrip(&Response::LinkResult {
            exit_code: 0,
            stdout: vec![],
            stderr: vec![],
            cached: true,
            warning: None,
        });
        roundtrip(&Response::LinkResult {
            exit_code: 0,
            stdout: vec![],
            stderr: b"some warning".to_vec(),
            cached: false,
            warning: Some("non-deterministic: missing D flag".into()),
        });
    }

    #[test]
    fn session_stats_request_roundtrip() {
        roundtrip(&Request::SessionStats {
            session_id: "550e8400-e29b-41d4-a716-446655440000".into(),
        });
    }

    #[test]
    fn session_stats_result_roundtrip() {
        let stats = SessionStats {
            duration_ms: 5000,
            compilations: 10,
            hits: 7,
            misses: 2,
            non_cacheable: 1,
            errors: 0,
            time_saved_ms: 3000,
            unique_sources: 9,
            bytes_read: 50_000,
            bytes_written: 20_000,
        };
        roundtrip(&Response::SessionStatsResult { stats: Some(stats) });
        roundtrip(&Response::SessionStatsResult { stats: None });
    }

    #[test]
    fn existing_request_variants_still_work() {
        roundtrip(&Request::Ping);
        roundtrip(&Request::Shutdown);
        roundtrip(&Request::Status);
        roundtrip(&Request::SessionEnd {
            session_id: "550e8400-e29b-41d4-a716-446655440000".into(),
        });
        roundtrip(&Request::Compile {
            session_id: "550e8400-e29b-41d4-a716-446655440000".into(),
            args: vec!["-c".into(), "foo.c".into()],
            cwd: PathBuf::from("/tmp"),
            compiler: PathBuf::from("/usr/bin/gcc"),
            env: None,
        });
    }

    #[test]
    fn existing_response_variants_still_work() {
        roundtrip(&Response::Pong);
        roundtrip(&Response::ShuttingDown);
        roundtrip(&Response::CompileResult {
            exit_code: 0,
            stdout: vec![],
            stderr: vec![],
            cached: true,
        });
        roundtrip(&Response::Error {
            message: "test".into(),
        });
    }

    #[test]
    fn daemon_status_version_field_roundtrips() {
        let with_version = DaemonStatus {
            version: "1.2.3".to_string(),
            artifact_count: 0,
            cache_size_bytes: 0,
            metadata_entries: 0,
            uptime_secs: 0,
            cache_hits: 0,
            cache_misses: 0,
            total_compilations: 0,
            non_cacheable: 0,
            compile_errors: 0,
            time_saved_ms: 0,
            total_links: 0,
            link_hits: 0,
            link_misses: 0,
            link_non_cacheable: 0,
            dep_graph_contexts: 0,
            dep_graph_files: 0,
            sessions_total: 0,
            sessions_active: 0,
            cache_dir: PathBuf::new(),
        };
        roundtrip(&with_version);
    }

    /// Verify that `#[serde(default)]` on `version` produces an empty string
    /// when the field is default-constructed (as an older daemon would omit it).
    #[test]
    fn daemon_status_version_default_is_empty() {
        let default_version: String = Default::default();
        assert_eq!(default_version, "");
    }
}
