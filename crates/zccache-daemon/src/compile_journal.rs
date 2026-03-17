//! JSONL compile journal for build replay.
//!
//! Records every compile/link command with enough detail to replay the entire
//! build. One JSON object per line, written to `{cache_dir}/logs/compile_journal.jsonl`.
//!
//! Architecture: same lock-free channel + background `std::thread` pattern as
//! `EventLogger`. Serialization happens on the caller's tokio task; the
//! background thread does file I/O only. Zero contention on the hot path.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::time::SystemTime;

use serde::Serialize;
use tokio::sync::mpsc;
use zccache_protocol::Response;

use crate::event_log::{format_timestamp, open_append};

/// A single journal entry serialized as one JSON line.
#[derive(Debug, Serialize)]
pub struct JournalEntry {
    /// ISO 8601 UTC timestamp.
    pub ts: String,
    /// Outcome: "hit", "miss", "error", "link_hit", "link_miss".
    pub outcome: &'static str,
    /// Full path to compiler/tool.
    pub compiler: String,
    /// Full argument list (for replay).
    pub args: Vec<String>,
    /// Working directory.
    pub cwd: String,
    /// Environment variables as `[key, value]` pairs. Omitted when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<Vec<(String, String)>>,
    /// Process exit code (-1 for errors).
    pub exit_code: i32,
    /// Session UUID or null for ephemeral.
    pub session_id: Option<String>,
    /// Wall-clock nanoseconds.
    pub latency_ns: u128,
}

/// Pre-captured request metadata for journal logging.
pub struct JournalContext {
    pub compiler: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub env: Option<Vec<(String, String)>>,
    pub session_id: Option<String>,
}

impl JournalEntry {
    /// Create a new journal entry with the current UTC timestamp.
    pub fn new(
        ctx: JournalContext,
        outcome: &'static str,
        exit_code: i32,
        latency_ns: u128,
    ) -> Self {
        Self {
            ts: format_timestamp(SystemTime::now()),
            outcome,
            compiler: ctx.compiler,
            args: ctx.args,
            cwd: ctx.cwd,
            env: ctx.env,
            exit_code,
            session_id: ctx.session_id,
            latency_ns,
        }
    }
}

/// JSONL compile journal backed by a lock-free channel and background writer thread.
pub struct CompileJournal {
    sender: Option<mpsc::UnboundedSender<String>>,
}

impl CompileJournal {
    /// Create a new compile journal writing to `log_dir/compile_journal.jsonl`.
    ///
    /// Spawns a background thread for all I/O. Returns `noop()` on failure.
    pub fn new(log_dir: PathBuf) -> Self {
        match Self::try_new(log_dir) {
            Ok(journal) => journal,
            Err(e) => {
                tracing::warn!("compile journal init failed: {e} — running without journal");
                Self::noop()
            }
        }
    }

    fn try_new(log_dir: PathBuf) -> std::io::Result<Self> {
        fs::create_dir_all(&log_dir)?;
        let path = log_dir.join("compile_journal.jsonl");
        let file = open_append(&path)?;

        let (tx, rx) = mpsc::unbounded_channel();

        std::thread::Builder::new()
            .name("zccache-journal".into())
            .spawn(move || journal_thread(rx, path, file))
            .map_err(std::io::Error::other)?;

        Ok(Self { sender: Some(tx) })
    }

    /// Create a no-op journal that discards all entries.
    #[must_use]
    pub fn noop() -> Self {
        Self { sender: None }
    }

    /// Log a journal entry. Serialization happens on the caller; file I/O
    /// happens on the background thread. Never blocks.
    pub fn log(&self, entry: &JournalEntry) {
        if let Some(tx) = &self.sender {
            // Serialize on caller's thread (tokio task).
            match serde_json::to_string(entry) {
                Ok(line) => {
                    let _ = tx.send(line);
                }
                Err(e) => {
                    tracing::debug!("journal serialize error: {e}");
                }
            }
        }
    }
}

/// Background thread: receives pre-serialized JSON lines and writes to file.
fn journal_thread(mut rx: mpsc::UnboundedReceiver<String>, path: PathBuf, mut file: std::fs::File) {
    while let Some(line) = rx.blocking_recv() {
        if writeln!(file, "{line}").is_err() {
            // Try to reopen (file may have been deleted/rotated).
            if let Ok(f) = open_append(&path) {
                file = f;
                let _ = writeln!(file, "{line}");
            }
        }
    }
}

/// Extract outcome string and exit code from a Response.
///
/// Returns `None` for non-compile/link responses (Ping, Status, etc.).
pub fn extract_outcome(response: &Response) -> Option<(&'static str, i32)> {
    match response {
        Response::CompileResult {
            exit_code, cached, ..
        } => {
            if *exit_code != 0 {
                Some(("error", *exit_code))
            } else if *cached {
                Some(("hit", *exit_code))
            } else {
                Some(("miss", *exit_code))
            }
        }
        Response::LinkResult {
            exit_code, cached, ..
        } => {
            if *exit_code != 0 {
                Some(("error", *exit_code))
            } else if *cached {
                Some(("link_hit", *exit_code))
            } else {
                Some(("link_miss", *exit_code))
            }
        }
        Response::Error { .. } => Some(("error", -1)),
        _ => None,
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_journal_entry_serialization() {
        let entry = JournalEntry {
            ts: "2026-03-17T10:30:00.123Z".to_string(),
            outcome: "hit",
            compiler: "/usr/bin/clang++".to_string(),
            args: vec!["-c".to_string(), "foo.cpp".to_string()],
            cwd: "/project/build".to_string(),
            env: Some(vec![("CC".to_string(), "clang".to_string())]),
            exit_code: 0,
            session_id: Some("test-uuid".to_string()),
            latency_ns: 1_234_567,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"outcome\":\"hit\""), "json: {json}");
        assert!(
            json.contains("\"compiler\":\"/usr/bin/clang++\""),
            "json: {json}"
        );
        assert!(json.contains("\"latency_ns\":1234567"), "json: {json}");
        assert!(
            json.contains("\"env\":[[\"CC\",\"clang\"]]"),
            "json: {json}"
        );
        // Verify it's valid JSON
        let _: serde_json::Value = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn test_journal_entry_env_omitted_when_none() {
        let entry = JournalEntry {
            ts: "2026-03-17T10:30:00.123Z".to_string(),
            outcome: "miss",
            compiler: "clang".to_string(),
            args: vec![],
            cwd: "/tmp".to_string(),
            env: None,
            exit_code: 0,
            session_id: None,
            latency_ns: 0,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(!json.contains("\"env\""), "env should be omitted: {json}");
        assert!(json.contains("\"session_id\":null"), "json: {json}");
    }

    #[test]
    fn test_journal_file_write() {
        let dir = tempfile::tempdir().unwrap();
        let journal = CompileJournal::new(dir.path().to_path_buf());

        let ctx = JournalContext {
            compiler: "/usr/bin/clang++".to_string(),
            args: vec!["-c".to_string(), "test.cpp".to_string()],
            cwd: "/project".to_string(),
            env: None,
            session_id: Some("session-1".to_string()),
        };
        let entry = JournalEntry::new(ctx, "hit", 0, 5_000_000);
        journal.log(&entry);

        // Give the background thread time to write.
        std::thread::sleep(Duration::from_millis(200));

        let content = fs::read_to_string(dir.path().join("compile_journal.jsonl")).unwrap();
        assert!(!content.is_empty(), "journal should have content");
        // Each line should be valid JSON.
        for line in content.lines() {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(v["outcome"], "hit");
            assert_eq!(v["compiler"], "/usr/bin/clang++");
        }
    }

    #[test]
    fn test_noop_journal() {
        let journal = CompileJournal::noop();
        let ctx = JournalContext {
            compiler: "clang".to_string(),
            args: vec![],
            cwd: "/tmp".to_string(),
            env: None,
            session_id: None,
        };
        let entry = JournalEntry::new(ctx, "miss", 0, 0);
        // Should not panic.
        journal.log(&entry);
    }

    #[test]
    fn test_extract_outcome_compile_hit() {
        let resp = Response::CompileResult {
            exit_code: 0,
            stdout: vec![],
            stderr: vec![],
            cached: true,
        };
        assert_eq!(extract_outcome(&resp), Some(("hit", 0)));
    }

    #[test]
    fn test_extract_outcome_compile_miss() {
        let resp = Response::CompileResult {
            exit_code: 0,
            stdout: vec![],
            stderr: vec![],
            cached: false,
        };
        assert_eq!(extract_outcome(&resp), Some(("miss", 0)));
    }

    #[test]
    fn test_extract_outcome_compile_error() {
        let resp = Response::CompileResult {
            exit_code: 1,
            stdout: vec![],
            stderr: vec![],
            cached: false,
        };
        assert_eq!(extract_outcome(&resp), Some(("error", 1)));
    }

    #[test]
    fn test_extract_outcome_link_hit() {
        let resp = Response::LinkResult {
            exit_code: 0,
            stdout: vec![],
            stderr: vec![],
            cached: true,
            warning: None,
        };
        assert_eq!(extract_outcome(&resp), Some(("link_hit", 0)));
    }

    #[test]
    fn test_extract_outcome_link_miss() {
        let resp = Response::LinkResult {
            exit_code: 0,
            stdout: vec![],
            stderr: vec![],
            cached: false,
            warning: None,
        };
        assert_eq!(extract_outcome(&resp), Some(("link_miss", 0)));
    }

    #[test]
    fn test_extract_outcome_error_response() {
        let resp = Response::Error {
            message: "something broke".to_string(),
        };
        assert_eq!(extract_outcome(&resp), Some(("error", -1)));
    }

    #[test]
    fn test_extract_outcome_non_compile() {
        assert_eq!(extract_outcome(&Response::Pong), None);
    }
}
