//! JSONL compile journal for build replay.
//!
//! Records every compile/link command with enough detail to replay the entire
//! build. One JSON object per line, written to `{cache_dir}/logs/compile_journal.jsonl`.
//!
//! Architecture: same lock-free channel + background `std::thread` pattern as
//! `EventLogger`. Serialization happens on the caller's tokio task; the
//! background thread does file I/O only. Zero contention on the hot path.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
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

/// Message sent to the background journal writer thread.
enum JournalMessage {
    /// Write a line to the global journal and optionally to a session journal.
    Entry {
        line: String,
        session_path: Option<PathBuf>,
    },
    /// Close a session journal file handle.
    CloseSession { path: PathBuf },
}

/// JSONL compile journal backed by a lock-free channel and background writer thread.
pub struct CompileJournal {
    sender: Option<mpsc::UnboundedSender<JournalMessage>>,
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
    ///
    /// If `session_path` is provided, the entry is also written to that
    /// per-session JSONL file.
    pub fn log(&self, entry: &JournalEntry, session_path: Option<&Path>) {
        if let Some(tx) = &self.sender {
            // Serialize on caller's thread (tokio task).
            match serde_json::to_string(entry) {
                Ok(line) => {
                    let _ = tx.send(JournalMessage::Entry {
                        line,
                        session_path: session_path.map(Path::to_path_buf),
                    });
                }
                Err(e) => {
                    tracing::debug!("journal serialize error: {e}");
                }
            }
        }
    }

    /// Close a session journal file handle. Call this when a session ends
    /// so the background thread can release the file.
    pub fn close_session(&self, path: &Path) {
        if let Some(tx) = &self.sender {
            let _ = tx.send(JournalMessage::CloseSession {
                path: path.to_path_buf(),
            });
        }
    }
}

/// Background thread: receives journal messages and writes to files.
fn journal_thread(
    mut rx: mpsc::UnboundedReceiver<JournalMessage>,
    global_path: PathBuf,
    mut global_file: std::fs::File,
) {
    let mut session_files: HashMap<PathBuf, std::fs::File> = HashMap::new();

    while let Some(msg) = rx.blocking_recv() {
        match msg {
            JournalMessage::Entry { line, session_path } => {
                // Write to global journal.
                if writeln!(global_file, "{line}").is_err() {
                    if let Ok(f) = open_append(&global_path) {
                        global_file = f;
                        let _ = writeln!(global_file, "{line}");
                    }
                }
                // Write to session journal if requested.
                if let Some(ref path) = session_path {
                    let file = session_files.entry(path.clone()).or_insert_with(|| {
                        match open_append(path) {
                            Ok(f) => f,
                            Err(e) => {
                                tracing::debug!("session journal open error: {e}");
                                // Return a dummy that will fail writes — we'll
                                // skip silently via is_err() below.
                                open_append(path).unwrap_or_else(|_| {
                                    // Last resort: /dev/null equivalent. The HashMap
                                    // entry will be cleaned up on CloseSession.
                                    std::fs::File::open(if cfg!(windows) {
                                        "NUL"
                                    } else {
                                        "/dev/null"
                                    })
                                    .expect("cannot open null device")
                                })
                            }
                        }
                    });
                    let _ = writeln!(file, "{line}");
                }
            }
            JournalMessage::CloseSession { path } => {
                session_files.remove(&path);
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
        journal.log(&entry, None);

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
        journal.log(&entry, None);
    }

    #[test]
    fn test_session_journal_file_write() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("sessions");
        fs::create_dir_all(&session_dir).unwrap();
        let session_path = session_dir.join("test-session.jsonl");

        let journal = CompileJournal::new(dir.path().to_path_buf());

        let ctx = JournalContext {
            compiler: "/usr/bin/clang++".to_string(),
            args: vec!["-c".to_string(), "test.cpp".to_string()],
            cwd: "/project".to_string(),
            env: None,
            session_id: Some("test-session".to_string()),
        };
        let entry = JournalEntry::new(ctx, "miss", 0, 2_000_000);
        journal.log(&entry, Some(&session_path));

        // Give the background thread time to write.
        std::thread::sleep(Duration::from_millis(200));

        // Global journal should have the entry.
        let global = fs::read_to_string(dir.path().join("compile_journal.jsonl")).unwrap();
        assert!(!global.is_empty(), "global journal should have content");

        // Session journal should also have the entry.
        let session = fs::read_to_string(&session_path).unwrap();
        assert!(!session.is_empty(), "session journal should have content");
        let v: serde_json::Value = serde_json::from_str(session.trim()).unwrap();
        assert_eq!(v["outcome"], "miss");
        assert_eq!(v["session_id"], "test-session");
    }

    #[test]
    fn test_close_session_releases_handle() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("sessions");
        fs::create_dir_all(&session_dir).unwrap();
        let session_path = session_dir.join("close-test.jsonl");

        let journal = CompileJournal::new(dir.path().to_path_buf());

        let ctx = JournalContext {
            compiler: "clang".to_string(),
            args: vec![],
            cwd: "/tmp".to_string(),
            env: None,
            session_id: Some("close-test".to_string()),
        };
        let entry = JournalEntry::new(ctx, "hit", 0, 100);
        journal.log(&entry, Some(&session_path));
        journal.close_session(&session_path);

        std::thread::sleep(Duration::from_millis(200));

        // File should exist and have content.
        let content = fs::read_to_string(&session_path).unwrap();
        assert!(!content.is_empty());
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
