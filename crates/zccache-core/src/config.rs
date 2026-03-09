//! Configuration types for zccache.

use std::path::PathBuf;

/// Top-level configuration for zccache.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Config {
    /// Path to the artifact cache directory.
    pub cache_dir: PathBuf,
    /// Maximum artifact cache size in bytes.
    pub max_cache_size: u64,
    /// Daemon idle timeout in seconds before auto-shutdown.
    pub idle_timeout_secs: u64,
    /// Whether to enable the file watcher.
    pub enable_watcher: bool,
    /// Whether to use polling fallback for file watching.
    pub watcher_poll_fallback: bool,
    /// Log level filter (e.g., "info", "debug", "trace").
    pub log_level: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            cache_dir: default_cache_dir(),
            max_cache_size: 10 * 1024 * 1024 * 1024, // 10 GB
            idle_timeout_secs: 3600,
            enable_watcher: true,
            watcher_poll_fallback: false,
            log_level: String::from("info"),
        }
    }
}

/// Returns the default cache directory path.
///
/// - Linux: `$XDG_CACHE_HOME/zccache` or `~/.cache/zccache`
/// - macOS: `~/Library/Caches/zccache`
/// - Windows: `%LOCALAPPDATA%\zccache`
#[must_use]
pub fn default_cache_dir() -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
            return PathBuf::from(xdg).join("zccache");
        }
        dirs_fallback().join(".cache/zccache")
    }
    #[cfg(target_os = "macos")]
    {
        dirs_fallback().join("Library/Caches/zccache")
    }
    #[cfg(target_os = "windows")]
    {
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            return PathBuf::from(local).join("zccache");
        }
        dirs_fallback().join("AppData/Local/zccache")
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        dirs_fallback().join(".cache/zccache")
    }
}

/// Returns the directory for crash dump files.
#[must_use]
pub fn crash_dump_dir() -> PathBuf {
    default_cache_dir().join("crashes")
}

fn dirs_fallback() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crash_dump_dir_ends_with_crashes() {
        let dir = crash_dump_dir();
        assert!(dir.ends_with("crashes"));
    }

    #[test]
    fn crash_dump_dir_is_under_cache_dir() {
        let cache = default_cache_dir();
        let crashes = crash_dump_dir();
        assert!(crashes.starts_with(&cache));
    }
}
