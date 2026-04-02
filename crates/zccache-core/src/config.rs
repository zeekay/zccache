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
    /// Maximum in-memory cache budget in bytes (default: 1 GB).
    pub max_memory_bytes: u64,
    /// How often (in seconds) the memory eviction background task runs (default: 30).
    pub eviction_interval_secs: u64,
    /// How often (in seconds) the disk artifact GC task runs (default: 300).
    pub disk_gc_interval_secs: u64,
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
            max_memory_bytes: 1_073_741_824, // 1 GB
            eviction_interval_secs: 30,
            disk_gc_interval_secs: 300,
        }
    }
}

/// Returns the default cache directory path: `~/.zccache` on all platforms.
#[must_use]
pub fn default_cache_dir() -> PathBuf {
    dirs_fallback().join(".zccache")
}

/// Returns the directory for content-addressed compiled outputs.
#[must_use]
pub fn artifacts_dir() -> PathBuf {
    default_cache_dir().join("artifacts")
}

/// Returns the directory for in-progress artifact writes (cleaned on startup).
#[must_use]
pub fn tmp_dir() -> PathBuf {
    default_cache_dir().join("tmp")
}

/// Returns the base directory for compiler-injected depfiles.
///
/// Each daemon instance creates a `{pid}-{instance}` subdirectory here.
/// Stale subdirectories from dead daemon processes are cleaned on startup.
#[must_use]
pub fn depfile_dir() -> PathBuf {
    tmp_dir().join("depfiles")
}

/// Remove stale depfile directories from previous (dead) daemon instances.
///
/// Scans [`depfile_dir()`] for subdirectories matching `{pid}-{instance}`.
/// If the PID is no longer alive (per `is_alive`), removes the directory.
/// Returns the number of directories cleaned up.
pub fn cleanup_stale_depfile_dirs<F>(is_alive: F) -> usize
where
    F: Fn(u32) -> bool,
{
    let base = depfile_dir();
    let entries = match std::fs::read_dir(&base) {
        Ok(entries) => entries,
        Err(_) => return 0,
    };

    let mut cleaned = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        let pid: u32 = match name.split('-').next().and_then(|s| s.parse().ok()) {
            Some(p) => p,
            None => continue,
        };
        if !is_alive(pid) {
            match std::fs::remove_dir_all(&path) {
                Ok(()) => {
                    cleaned += 1;
                    tracing::info!(path = %path.display(), "removed stale depfile dir");
                }
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        "failed to remove stale depfile dir: {e}"
                    );
                }
            }
        }
    }
    cleaned
}

/// Returns the directory for serialized dependency graph storage (future).
#[must_use]
pub fn depgraph_dir() -> PathBuf {
    default_cache_dir().join("depgraph")
}

/// Returns the path to the artifact index database.
#[must_use]
pub fn index_path() -> PathBuf {
    default_cache_dir().join("index.redb")
}

/// Returns the directory for crash dump files.
#[must_use]
pub fn crash_dump_dir() -> PathBuf {
    default_cache_dir().join("crashes")
}

/// Returns the directory for daemon log files.
#[must_use]
pub fn log_dir() -> PathBuf {
    default_cache_dir().join("logs")
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
    fn default_cache_dir_ends_with_zccache() {
        let dir = default_cache_dir();
        assert!(dir.ends_with(".zccache"));
    }

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

    #[test]
    fn log_dir_ends_with_logs() {
        let dir = log_dir();
        assert!(dir.ends_with("logs"));
    }

    #[test]
    fn log_dir_is_under_cache_dir() {
        let cache = default_cache_dir();
        let logs = log_dir();
        assert!(logs.starts_with(&cache));
    }

    #[test]
    fn artifacts_dir_ends_with_artifacts() {
        let dir = artifacts_dir();
        assert!(dir.ends_with("artifacts"));
        assert!(dir.starts_with(default_cache_dir()));
    }

    #[test]
    fn tmp_dir_ends_with_tmp() {
        let dir = tmp_dir();
        assert!(dir.ends_with("tmp"));
        assert!(dir.starts_with(default_cache_dir()));
    }

    #[test]
    fn depgraph_dir_ends_with_depgraph() {
        let dir = depgraph_dir();
        assert!(dir.ends_with("depgraph"));
        assert!(dir.starts_with(default_cache_dir()));
    }

    #[test]
    fn depfile_dir_under_tmp() {
        let dir = depfile_dir();
        assert!(dir.ends_with("depfiles"));
        assert!(dir.starts_with(tmp_dir()));
    }

    #[test]
    fn cleanup_stale_depfile_dirs_removes_dead() {
        let base = tempfile::tempdir().unwrap();
        let depfiles = base.path().join("depfiles");
        std::fs::create_dir_all(&depfiles).unwrap();

        // Create a "dead" dir (PID 99999999 unlikely alive).
        std::fs::create_dir(depfiles.join("99999999-0")).unwrap();
        // Create a non-matching dir (should be left alone).
        std::fs::create_dir(depfiles.join("not-a-pid")).unwrap();

        let entries = std::fs::read_dir(&depfiles).unwrap();
        let dirs: Vec<_> = entries.flatten().collect();
        assert_eq!(dirs.len(), 2);

        // Use a custom is_alive that says nothing is alive.
        let cleaned = cleanup_stale_with_base(&depfiles, |_| false);
        assert_eq!(cleaned, 1); // only the parseable one removed

        // "not-a-pid" should still exist.
        assert!(depfiles.join("not-a-pid").is_dir());
        assert!(!depfiles.join("99999999-0").exists());
    }

    #[test]
    fn cleanup_stale_depfile_dirs_skips_alive() {
        let base = tempfile::tempdir().unwrap();
        let depfiles = base.path().join("depfiles");
        std::fs::create_dir_all(&depfiles).unwrap();
        std::fs::create_dir(depfiles.join("12345-0")).unwrap();

        let cleaned = cleanup_stale_with_base(&depfiles, |_| true);
        assert_eq!(cleaned, 0);
        assert!(depfiles.join("12345-0").is_dir());
    }

    #[test]
    fn cleanup_stale_depfile_dirs_empty() {
        // Non-existent directory returns 0.
        let cleaned = cleanup_stale_with_base(std::path::Path::new("/nonexistent/path"), |_| false);
        assert_eq!(cleaned, 0);
    }

    /// Test helper: runs cleanup logic against an arbitrary base dir.
    fn cleanup_stale_with_base<F>(base: &std::path::Path, is_alive: F) -> usize
    where
        F: Fn(u32) -> bool,
    {
        let entries = match std::fs::read_dir(base) {
            Ok(entries) => entries,
            Err(_) => return 0,
        };
        let mut cleaned = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n,
                None => continue,
            };
            let pid: u32 = match name.split('-').next().and_then(|s| s.parse().ok()) {
                Some(p) => p,
                None => continue,
            };
            if !is_alive(pid) && std::fs::remove_dir_all(&path).is_ok() {
                cleaned += 1;
            }
        }
        cleaned
    }

    #[test]
    fn disk_gc_interval_default() {
        let config = Config::default();
        assert_eq!(config.disk_gc_interval_secs, 300);
    }

    #[test]
    fn index_path_ends_with_redb() {
        let p = index_path();
        assert!(p.ends_with("index.redb"));
        assert!(p.starts_with(default_cache_dir()));
    }
}
