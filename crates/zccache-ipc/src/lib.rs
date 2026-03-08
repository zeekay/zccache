//! IPC transport layer for zccache.
//!
//! Provides platform-abstracted IPC between CLI/compiler wrapper
//! and the daemon, using Unix domain sockets on Unix and named
//! pipes on Windows.

#![allow(clippy::missing_errors_doc)]

use std::path::PathBuf;

/// Returns the platform-specific default IPC endpoint path.
///
/// - Linux: `$XDG_RUNTIME_DIR/zccache/sock` or `/tmp/zccache-$USER/sock`
/// - macOS: `/tmp/zccache-$USER/sock`
/// - Windows: `\\.\pipe\zccache-{username}`
#[must_use]
pub fn default_endpoint() -> String {
    #[cfg(unix)]
    {
        if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
            return format!("{runtime_dir}/zccache/sock");
        }
        let user = std::env::var("USER").unwrap_or_else(|_| String::from("unknown"));
        format!("/tmp/zccache-{user}/sock")
    }
    #[cfg(windows)]
    {
        let username = std::env::var("USERNAME").unwrap_or_else(|_| String::from("unknown"));
        format!(r"\\.\pipe\zccache-{username}")
    }
}

/// Returns the path for the daemon lock file.
#[must_use]
pub fn lock_file_path() -> PathBuf {
    #[cfg(unix)]
    {
        let endpoint = default_endpoint();
        let dir = std::path::Path::new(&endpoint)
            .parent()
            .expect("endpoint should have parent directory");
        dir.join("daemon.lock")
    }
    #[cfg(windows)]
    {
        let local_app_data = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| String::from("."));
        PathBuf::from(local_app_data)
            .join("zccache")
            .join("daemon.lock")
    }
}
