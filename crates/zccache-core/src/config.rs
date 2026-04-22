//! Configuration types for zccache.

use crate::NormalizedPath;
use std::ffi::OsString;
use std::path::PathBuf;

/// Environment variable used to override the zccache cache root.
pub const CACHE_DIR_ENV: &str = "ZCCACHE_CACHE_DIR";

/// Top-level configuration for zccache.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Config {
    /// Path to the artifact cache directory.
    pub cache_dir: NormalizedPath,
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

/// Returns the configured cache directory path.
///
/// If `ZCCACHE_CACHE_DIR` is set and non-empty, it is used as the cache root.
/// Relative override paths are made absolute against the current working
/// directory so the daemon and CLI derive the same subpaths when spawned
/// together. If unset, this falls back to `~/.zccache` on all platforms.
#[must_use]
pub fn default_cache_dir() -> NormalizedPath {
    if let Some(cache_dir) = cache_dir_override() {
        return cache_dir;
    }
    dirs_fallback().join(".zccache")
}

/// Returns the cache directory override from `ZCCACHE_CACHE_DIR`, if set.
#[must_use]
pub fn cache_dir_override() -> Option<NormalizedPath> {
    cache_dir_from_env_value(std::env::var_os(CACHE_DIR_ENV))
}

/// Returns the directory for content-addressed compiled outputs.
#[must_use]
pub fn artifacts_dir() -> NormalizedPath {
    default_cache_dir().join("artifacts")
}

/// Returns the directory for in-progress artifact writes (cleaned on startup).
#[must_use]
pub fn tmp_dir() -> NormalizedPath {
    default_cache_dir().join("tmp")
}

/// Returns the base directory for compiler-injected depfiles.
///
/// Each daemon instance creates a `{pid}-{instance}` subdirectory here.
/// Stale subdirectories from dead daemon processes are cleaned on startup.
#[must_use]
pub fn depfile_dir() -> NormalizedPath {
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
pub fn depgraph_dir() -> NormalizedPath {
    default_cache_dir().join("depgraph")
}

/// Returns the path to the artifact index database.
#[must_use]
pub fn index_path() -> NormalizedPath {
    default_cache_dir().join("index.redb")
}

/// Returns the directory for crash dump files.
#[must_use]
pub fn crash_dump_dir() -> NormalizedPath {
    default_cache_dir().join("crashes")
}

/// Returns the directory for daemon log files.
#[must_use]
pub fn log_dir() -> NormalizedPath {
    default_cache_dir().join("logs")
}

fn dirs_fallback() -> NormalizedPath {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(NormalizedPath::from)
        .unwrap_or_else(|_| ".".into())
}

fn cache_dir_from_env_value(value: Option<OsString>) -> Option<NormalizedPath> {
    let value = value?;
    if value.is_empty() {
        return None;
    }
    Some(normalize_cache_dir_override(PathBuf::from(value)))
}

fn normalize_cache_dir_override(path: PathBuf) -> NormalizedPath {
    if path.is_absolute() {
        path.into()
    } else {
        std::env::current_dir()
            .unwrap_or_default()
            .join(path)
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        _lock: MutexGuard<'static, ()>,
        previous: Option<OsString>,
    }

    impl EnvGuard {
        fn set_cache_dir(value: &std::path::Path) -> Self {
            let lock = ENV_LOCK.lock().unwrap();
            let previous = std::env::var_os(CACHE_DIR_ENV);
            std::env::set_var(CACHE_DIR_ENV, value);
            Self {
                _lock: lock,
                previous,
            }
        }

        fn remove_cache_dir() -> Self {
            let lock = ENV_LOCK.lock().unwrap();
            let previous = std::env::var_os(CACHE_DIR_ENV);
            std::env::remove_var(CACHE_DIR_ENV);
            Self {
                _lock: lock,
                previous,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var(CACHE_DIR_ENV, value),
                None => std::env::remove_var(CACHE_DIR_ENV),
            }
        }
    }

    #[test]
    fn default_cache_dir_ends_with_zccache() {
        let _env = EnvGuard::remove_cache_dir();
        let dir = default_cache_dir();
        assert!(dir.ends_with(".zccache"));
    }

    #[test]
    fn cache_dir_override_uses_non_empty_env_value() {
        let root = tempfile::tempdir().unwrap();
        let override_dir = root.path().join("zc");
        let _env = EnvGuard::set_cache_dir(&override_dir);

        assert_eq!(default_cache_dir(), override_dir);
        assert_eq!(artifacts_dir(), override_dir.join("artifacts"));
        assert_eq!(tmp_dir(), override_dir.join("tmp"));
        assert_eq!(depgraph_dir(), override_dir.join("depgraph"));
        assert_eq!(index_path(), override_dir.join("index.redb"));
        assert_eq!(crash_dump_dir(), override_dir.join("crashes"));
        assert_eq!(log_dir(), override_dir.join("logs"));
    }

    #[test]
    fn cache_dir_override_ignores_empty_env_value() {
        assert!(cache_dir_from_env_value(Some(OsString::new())).is_none());
    }

    #[test]
    fn relative_cache_dir_override_is_made_absolute() {
        let override_dir = cache_dir_from_env_value(Some(OsString::from("target/../zc"))).unwrap();
        assert!(override_dir.is_absolute());
        assert!(override_dir.ends_with("zc"));
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
