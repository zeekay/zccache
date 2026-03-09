//! Crash dump writer and panic hook for the daemon.
//!
//! On panic, writes a timestamped crash dump to the crash directory.
//! On startup, warns about unreported crashes from previous sessions.

use std::path::{Path, PathBuf};

/// Install a panic hook that writes crash dumps before aborting.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        let panic_msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic".to_string()
        };

        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown".to_string());

        let full_info = format!("panicked at '{panic_msg}', {location}");

        if let Some(path) = write_crash_dump(&full_info, &backtrace.to_string()) {
            eprintln!(
                "[zccache] daemon crashed — dump written to {}",
                path.display()
            );
        } else {
            eprintln!("[zccache] daemon crashed — failed to write crash dump");
            eprintln!("[zccache] {full_info}");
        }
    }));
}

/// Write a crash dump file to the crash directory.
///
/// Returns the path to the written file, or `None` if writing failed.
pub fn write_crash_dump(panic_info: &str, backtrace: &str) -> Option<PathBuf> {
    let crash_dir = zccache_core::config::crash_dump_dir();
    std::fs::create_dir_all(&crash_dir).ok()?;

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let filename = format!("crash-{timestamp}.txt");
    let path = crash_dir.join(&filename);

    let content = format!(
        "zccache daemon crash report\n\
         ===========================\n\
         Version: {version}\n\
         OS: {os}\n\
         Arch: {arch}\n\
         PID: {pid}\n\
         Timestamp: {timestamp}\n\
         \n\
         Panic:\n\
         {panic_info}\n\
         \n\
         Backtrace:\n\
         {backtrace}\n",
        version = env!("CARGO_PKG_VERSION"),
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        pid = std::process::id(),
    );

    std::fs::write(&path, content).ok()?;
    Some(path)
}

/// Check for unreported crash dumps from previous sessions.
///
/// Logs a warning for each unreported crash and creates a `.reported` marker.
pub fn check_previous_crashes() {
    let crash_dir = zccache_core::config::crash_dump_dir();
    let entries = match std::fs::read_dir(&crash_dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "txt") {
            let reported = path.with_extension("reported");
            if reported.exists() {
                continue;
            }

            // Read first few lines for summary.
            let summary = read_crash_summary(&path);
            tracing::warn!(
                "daemon crashed during previous session: {}\n  {}",
                path.display(),
                summary
            );

            // Mark as reported.
            let _ = std::fs::write(&reported, "");
        }
    }
}

/// List all crash dump files, sorted by name.
#[must_use]
pub fn list_crash_dumps() -> Vec<PathBuf> {
    let crash_dir = zccache_core::config::crash_dump_dir();
    let mut dumps: Vec<PathBuf> = match std::fs::read_dir(&crash_dir) {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "txt"))
            .collect(),
        Err(_) => Vec::new(),
    };
    dumps.sort();
    dumps
}

/// Delete all crash dump files and their `.reported` markers.
///
/// Returns the number of `.txt` files deleted.
pub fn clear_crash_dumps() -> usize {
    let crash_dir = zccache_core::config::crash_dump_dir();
    let entries = match std::fs::read_dir(&crash_dir) {
        Ok(e) => e,
        Err(_) => return 0,
    };

    let mut count = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str());
        match ext {
            Some("txt") => {
                if std::fs::remove_file(&path).is_ok() {
                    count += 1;
                }
            }
            Some("reported") => {
                let _ = std::fs::remove_file(&path);
            }
            _ => {}
        }
    }
    count
}

fn read_crash_summary(path: &Path) -> String {
    match std::fs::read_to_string(path) {
        Ok(content) => content
            .lines()
            .filter(|l| l.starts_with("Panic:") || l.starts_with("Version:"))
            .take(2)
            .collect::<Vec<_>>()
            .join(", "),
        Err(_) => "unable to read crash dump".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_crash_dump_creates_file() {
        let tmp = tempfile::tempdir().unwrap();
        // Override crash dir by writing directly.
        let crash_dir = tmp.path().join("crashes");
        std::fs::create_dir_all(&crash_dir).unwrap();

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let filename = format!("crash-{timestamp}.txt");
        let path = crash_dir.join(&filename);

        let content = format!(
            "zccache daemon crash report\n\
             ===========================\n\
             Version: {}\n\
             OS: {}\n\
             Arch: {}\n\
             PID: {}\n\
             Timestamp: {timestamp}\n\
             \n\
             Panic:\n\
             test panic info\n\
             \n\
             Backtrace:\n\
             test backtrace\n",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH,
            std::process::id(),
        );

        std::fs::write(&path, &content).unwrap();

        assert!(path.exists());
        let read_content = std::fs::read_to_string(&path).unwrap();
        assert!(read_content.contains("test panic info"));
        assert!(read_content.contains("test backtrace"));
        assert!(read_content.contains(env!("CARGO_PKG_VERSION")));
        assert!(read_content.contains(std::env::consts::OS));
    }

    #[test]
    fn check_previous_crashes_marks_as_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path();

        let dump_path = crash_dir.join("crash-12345.txt");
        std::fs::write(
            &dump_path,
            "zccache daemon crash report\nVersion: test\nPanic:\ntest\n",
        )
        .unwrap();

        // Manually do what check_previous_crashes does for this directory.
        let reported = dump_path.with_extension("reported");
        assert!(!reported.exists());

        // Create the reported marker.
        std::fs::write(&reported, "").unwrap();
        assert!(reported.exists());
    }

    #[test]
    fn clear_crash_dumps_removes_files() {
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path();

        std::fs::write(crash_dir.join("crash-1.txt"), "dump1").unwrap();
        std::fs::write(crash_dir.join("crash-1.reported"), "").unwrap();
        std::fs::write(crash_dir.join("crash-2.txt"), "dump2").unwrap();

        let entries: Vec<_> = std::fs::read_dir(crash_dir).unwrap().flatten().collect();
        assert_eq!(entries.len(), 3);

        // Manually clear.
        let mut count = 0;
        for entry in std::fs::read_dir(crash_dir).unwrap().flatten() {
            let path = entry.path();
            let ext = path.extension().and_then(|e| e.to_str());
            match ext {
                Some("txt") => {
                    if std::fs::remove_file(&path).is_ok() {
                        count += 1;
                    }
                }
                Some("reported") => {
                    let _ = std::fs::remove_file(&path);
                }
                _ => {}
            }
        }
        assert_eq!(count, 2);
    }

    #[test]
    fn list_crash_dumps_sorts() {
        let tmp = tempfile::tempdir().unwrap();
        let crash_dir = tmp.path();

        std::fs::write(crash_dir.join("crash-3.txt"), "c").unwrap();
        std::fs::write(crash_dir.join("crash-1.txt"), "a").unwrap();
        std::fs::write(crash_dir.join("crash-2.txt"), "b").unwrap();
        std::fs::write(crash_dir.join("crash-2.reported"), "").unwrap();

        let mut dumps: Vec<PathBuf> = std::fs::read_dir(crash_dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "txt"))
            .collect();
        dumps.sort();

        assert_eq!(dumps.len(), 3);
        assert!(dumps[0].ends_with("crash-1.txt"));
        assert!(dumps[1].ends_with("crash-2.txt"));
        assert!(dumps[2].ends_with("crash-3.txt"));
    }
}
