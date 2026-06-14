//! `zccache release-handles --path <PATH>` — standalone handle-release
//! subcommand. Sends `Request::ReleaseWorktreeHandles` to the daemon so
//! the caller can break daemon-owned file handles before `rm -rf` /
//! `git worktree remove` on Windows.
//!
//! Companions the existing daemon-side handler (`handle_release_worktree_handles`,
//! issue #690) and the soldr Tier 3 worktree-teardown call site. This
//! subcommand exposes the same wire request to anyone who is using
//! zccache without soldr — FastLED CI cleanup, sysadmin scripts, manual
//! debugging — so the Windows file-lock-on-teardown class (soldr#710) is
//! solvable from the zccache binary alone. See zccache#694 Phase 2.

use std::path::Path;
use std::process::ExitCode;

use super::daemon::ensure_daemon;
use super::util::{absolute_path, connect, print_json_value};

/// Exit codes:
/// - 0: request succeeded (whether or not any handles were released).
///   `released` and `unreleased` in the output describe the actual work.
/// - 2: daemon unreachable, send/recv error, or daemon refused (e.g. path
///   overlaps the cache root).
pub(crate) async fn cmd_release_handles(endpoint: &str, path: &str, json: bool) -> ExitCode {
    // Resolve to an absolute path before sending — the daemon-side handler
    // (handle_release_worktree_handles.rs:28) canonicalizes against the
    // active sessions' `working_dir`, and a relative path here would
    // resolve against the daemon's CWD instead of the caller's. The
    // protocol docstring on `Request::ReleaseWorktreeHandles` also
    // requires absolute paths.
    let abs = absolute_path(path);
    let abs_path: &Path = abs.as_path();

    if let Err(e) = ensure_daemon(endpoint).await {
        let message = format!("failed to start daemon: {e}");
        report_error(&message, json, endpoint, path);
        return ExitCode::from(2);
    }

    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(e) => {
            let message = format!("cannot connect to daemon: {e}");
            report_error(&message, json, endpoint, path);
            return ExitCode::from(2);
        }
    };

    let request = crate::protocol::Request::ReleaseWorktreeHandles {
        path: abs_path.into(),
    };

    let wire = crate::protocol::wire_prost::full_family_wire_format_from_env();
    if let Err(e) = conn.send_request(&request, wire).await {
        let message = format!("send error: {e}");
        report_error(&message, json, endpoint, path);
        return ExitCode::from(2);
    }

    match conn.recv_response().await {
        Ok(Some(crate::protocol::Response::ReleaseWorktreeHandlesResult {
            inspected,
            released,
            sessions_dropped,
            unreleased,
        })) => {
            if json {
                let value = serde_json::json!({
                    "status": "ok",
                    "path": abs_path.display().to_string(),
                    "inspected": inspected,
                    "released": released,
                    "sessions_dropped": sessions_dropped,
                    "unreleased": unreleased
                        .iter()
                        .map(|p| p.as_path().display().to_string())
                        .collect::<Vec<_>>(),
                });
                print_json_value(&value);
            } else {
                println!(
                    "zccache release-handles: inspected {inspected} session(s), released {released}"
                );
                if !sessions_dropped.is_empty() {
                    println!("  sessions dropped: {}", sessions_dropped.join(", "));
                }
                if !unreleased.is_empty() {
                    println!("  unreleased paths ({}):", unreleased.len());
                    for p in &unreleased {
                        println!("    {}", p.as_path().display());
                    }
                }
            }
            ExitCode::SUCCESS
        }
        Ok(Some(crate::protocol::Response::Error { message })) => {
            report_error(&format!("daemon error: {message}"), json, endpoint, path);
            ExitCode::from(2)
        }
        Ok(other) => {
            report_error(
                &format!("unexpected response: {other:?}"),
                json,
                endpoint,
                path,
            );
            ExitCode::from(2)
        }
        Err(e) => {
            report_error(&format!("recv error: {e}"), json, endpoint, path);
            ExitCode::from(2)
        }
    }
}

fn report_error(message: &str, json: bool, endpoint: &str, path: &str) {
    if json {
        let value = serde_json::json!({
            "status": "error",
            "endpoint": endpoint,
            "path": path,
            "error": message,
        });
        print_json_value(&value);
    } else {
        eprintln!("zccache release-handles: {message}");
    }
}
