//! File watcher abstraction for zccache.
//!
//! Provides a platform-abstracted file watching interface that
//! integrates with the metadata cache's confidence model.

#![allow(clippy::missing_errors_doc)]

use std::path::{Path, PathBuf};

/// Events produced by the file watcher.
#[derive(Debug, Clone)]
pub enum WatchEvent {
    /// A file was modified.
    Modified(PathBuf),
    /// A file was created.
    Created(PathBuf),
    /// A file was removed.
    Removed(PathBuf),
    /// A file was renamed (from, to).
    Renamed { from: PathBuf, to: PathBuf },
    /// The watcher's event buffer overflowed. All watched paths
    /// should be considered stale.
    Overflow,
    /// An error occurred in the watcher backend.
    Error(String),
}

/// Trait for file watcher implementations.
///
/// Abstracts over platform-specific file watching backends.
pub trait FileWatcher: Send + Sync {
    /// Start watching a directory (recursively).
    fn watch(&mut self, path: &Path) -> zccache_core::Result<()>;

    /// Stop watching a directory.
    fn unwatch(&mut self, path: &Path) -> zccache_core::Result<()>;

    /// Receive the next batch of events.
    ///
    /// Returns an empty vec if no events are pending.
    fn poll_events(&mut self) -> zccache_core::Result<Vec<WatchEvent>>;
}

/// Configuration for the file watcher.
#[derive(Debug, Clone)]
pub struct WatcherConfig {
    /// Whether to use polling fallback instead of native events.
    pub use_polling: bool,
    /// Polling interval in milliseconds (only used in polling mode).
    pub poll_interval_ms: u64,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            use_polling: false,
            poll_interval_ms: 1000,
        }
    }
}
