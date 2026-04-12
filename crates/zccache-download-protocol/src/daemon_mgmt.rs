//! Shared daemon-management utilities used by both the download client and
//! the download daemon binary (lock-file helpers, default endpoint, etc.).

use zccache_core::NormalizedPath;

/// Return the default IPC endpoint for the download daemon.
pub fn default_endpoint() -> String {
    #[cfg(unix)]
    {
        if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
            return format!("{runtime_dir}/zccache-download/sock");
        }
        let user = std::env::var("USER").unwrap_or_else(|_| String::from("unknown"));
        format!("/tmp/zccache-download-{user}/sock")
    }
    #[cfg(windows)]
    {
        let username = std::env::var("USERNAME").unwrap_or_else(|_| String::from("unknown"));
        format!(r"\\.\pipe\zccache-download-{username}")
    }
}

/// Path to the daemon PID lock file.
pub fn lock_file_path() -> NormalizedPath {
    zccache_core::config::default_cache_dir().join("download-daemon.lock")
}

/// Write the daemon PID to the lock file.
pub fn write_lock_file(pid: u32) -> Result<(), std::io::Error> {
    let path = lock_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, pid.to_string())
}

/// Remove the daemon lock file (best-effort).
pub fn remove_lock_file() {
    let _ = std::fs::remove_file(lock_file_path());
}

/// Read the PID stored in the daemon lock file, if it exists and is valid.
pub fn read_lock_file_pid() -> Option<u32> {
    std::fs::read_to_string(lock_file_path())
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
}
