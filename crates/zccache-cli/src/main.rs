//! zccache CLI -- command-line interface for the compiler cache.
//!
//! Usage modes:
//!
//! 1. Subcommand mode:
//!    zccache session-start --compiler /path/to/clang++
//!    zccache session-end <id>
//!    zccache status
//!
//! 2. Compiler wrapper mode (auto-detected):
//!    ZCCACHE_SESSION_ID=42 zccache clang++ -c foo.cpp -o foo.o
//!
//!    If the first arg isn't a known subcommand, zccache treats
//!    the entire command line as a compiler invocation and forwards
//!    it to the daemon via the session from ZCCACHE_SESSION_ID.

use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// zccache -- fast local compiler cache.
#[derive(Debug, Parser)]
#[command(name = "zccache", version, about)]
struct Cli {
    /// Clear the entire artifact cache (same as `zccache clear`).
    #[arg(long)]
    clear: bool,

    /// Show daemon and cache statistics (same as `zccache status`).
    #[arg(long)]
    show_stats: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Start the daemon (if not already running).
    Start,
    /// Stop the daemon.
    Stop,
    /// Show daemon and cache status.
    Status,
    /// Clear the artifact cache.
    Clear,
    /// Start a build session. Prints session ID to stdout.
    #[command(name = "session-start")]
    SessionStart {
        /// Working directory (defaults to current dir).
        #[arg(long)]
        cwd: Option<String>,
        /// Path to a log file for this session.
        #[arg(long)]
        log: Option<String>,
        /// IPC endpoint (default: platform-specific).
        #[arg(long)]
        endpoint: Option<String>,
        /// Enable per-session hit/miss statistics tracking.
        #[arg(long)]
        stats: bool,
    },
    /// End a build session.
    #[command(name = "session-end")]
    SessionEnd {
        /// Session ID to end.
        session_id: String,
        /// IPC endpoint (default: platform-specific).
        #[arg(long)]
        endpoint: Option<String>,
    },
    /// Query stats for an active session (without ending it).
    #[command(name = "session-stats")]
    SessionStatsCmd {
        /// Session ID to query.
        session_id: String,
        /// IPC endpoint (default: platform-specific).
        #[arg(long)]
        endpoint: Option<String>,
    },
    /// Wrap a compiler invocation (explicit mode).
    Wrap {
        /// The compiler and its arguments.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Show detailed information about a cache entry.
    Inspect {
        /// Cache key (hex).
        key: String,
    },
    /// Show or clear crash dumps from previous daemon crashes.
    Crashes {
        /// Delete all crash dumps.
        #[arg(long)]
        clear: bool,
    },
}

/// Known subcommand names for auto-detect.
const KNOWN_SUBCOMMANDS: &[&str] = &[
    "start",
    "stop",
    "status",
    "clear",
    "wrap",
    "inspect",
    "session-start",
    "session-end",
    "session-stats",
    "crashes",
    "help",
    "--help",
    "-h",
    "--version",
    "-V",
];

/// Convert an i32 exit code to ExitCode without silent truncation.
/// A bare `exit_code as u8` wraps: 256 → 0 (success), masking failures.
/// This preserves success/failure semantics: non-zero stays non-zero.
fn exit_code_from_i32(code: i32) -> ExitCode {
    let truncated = (code & 0xFF) as u8;
    if code != 0 && truncated == 0 {
        ExitCode::from(1)
    } else {
        ExitCode::from(truncated)
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();

    // Auto-detect: if first arg isn't a known subcommand or a --flag, enter wrap mode.
    // e.g., `zccache clang++ -c foo.cpp -o foo.o`
    if args.len() > 1
        && !KNOWN_SUBCOMMANDS.contains(&args[1].as_str())
        && !args[1].starts_with("--")
    {
        return run_wrap(&args[1..]);
    }

    let cli = Cli::parse();

    init_tracing();

    // Handle top-level flags (sccache-compatible)
    if cli.clear {
        let endpoint = resolve_endpoint(None);
        return run_async(cmd_clear(&endpoint));
    }
    if cli.show_stats {
        let endpoint = resolve_endpoint(None);
        return run_async(cmd_status(&endpoint));
    }

    let command = match cli.command {
        Some(cmd) => cmd,
        None => {
            // No subcommand and no flag — show help.
            use clap::CommandFactory;
            Cli::command().print_help().ok();
            return ExitCode::FAILURE;
        }
    };

    match command {
        Commands::Start => {
            let endpoint = resolve_endpoint(None);
            run_async(cmd_start(&endpoint))
        }
        Commands::Stop => {
            let endpoint = resolve_endpoint(None);
            run_async(cmd_stop(&endpoint))
        }
        Commands::Status => {
            let endpoint = resolve_endpoint(None);
            run_async(cmd_status(&endpoint))
        }
        Commands::Clear => {
            let endpoint = resolve_endpoint(None);
            run_async(cmd_clear(&endpoint))
        }
        Commands::SessionStart {
            cwd,
            log,
            endpoint,
            stats,
        } => {
            let endpoint = resolve_endpoint(endpoint.as_deref());
            let cwd = cwd
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            let log = log.map(|p| {
                let path = Path::new(&p);
                if path.is_absolute() {
                    PathBuf::from(p)
                } else {
                    std::env::current_dir().unwrap_or_default().join(p)
                }
            });
            run_async(cmd_session_start(&endpoint, &cwd, log.as_deref(), stats))
        }
        Commands::SessionEnd {
            session_id,
            endpoint,
        } => {
            let endpoint = resolve_endpoint(endpoint.as_deref());
            run_async(cmd_session_end(&endpoint, session_id))
        }
        Commands::SessionStatsCmd {
            session_id,
            endpoint,
        } => {
            let endpoint = resolve_endpoint(endpoint.as_deref());
            run_async(cmd_session_stats(&endpoint, session_id))
        }
        Commands::Wrap { args } => run_wrap(&args),
        Commands::Inspect { key } => {
            eprintln!("zccache inspect {key}: not yet implemented");
            ExitCode::FAILURE
        }
        Commands::Crashes { clear } => cmd_crashes(clear),
    }
}

// ─── Subcommand implementations ────────────────────────────────────────────

async fn cmd_start(endpoint: &str) -> ExitCode {
    match ensure_daemon(endpoint).await {
        Ok(()) => {
            eprintln!("daemon running at {endpoint}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("failed to start daemon: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn cmd_stop(endpoint: &str) -> ExitCode {
    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(_) => {
            eprintln!("daemon not running at {endpoint}");
            return ExitCode::SUCCESS;
        }
    };

    conn.send(&zccache_protocol::Request::Shutdown)
        .await
        .unwrap();
    match conn.recv().await.unwrap() {
        Some(zccache_protocol::Response::ShuttingDown) => {
            eprintln!("daemon stopped");
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("unexpected response: {other:?}");
            ExitCode::FAILURE
        }
    }
}

async fn cmd_status(endpoint: &str) -> ExitCode {
    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("daemon not running at {endpoint}: {e}");
            return ExitCode::FAILURE;
        }
    };

    conn.send(&zccache_protocol::Request::Status).await.unwrap();
    match conn.recv().await.unwrap() {
        Some(zccache_protocol::Response::Status(s)) => {
            let total = s.cache_hits + s.cache_misses;
            let hit_rate = if total > 0 {
                format!("{:.1}%", s.cache_hits as f64 / total as f64 * 100.0)
            } else {
                "n/a".to_string()
            };

            println!(
                "zccache daemon v{} ({}) — uptime {}",
                if s.version.is_empty() {
                    "unknown"
                } else {
                    &s.version
                },
                endpoint,
                format_uptime(s.uptime_secs)
            );
            if !s.cache_dir.as_os_str().is_empty() {
                println!("cache dir: {}", s.cache_dir.display());
            }
            println!();
            println!(
                "  Compilations:  {} total ({} cached, {} cold, {} non-cacheable)",
                s.total_compilations, s.cache_hits, s.cache_misses, s.non_cacheable
            );
            println!("  Hit rate:      {hit_rate}");
            if s.time_saved_ms > 0 {
                println!("  Time saved:    ~{}", format_duration_ms(s.time_saved_ms));
            }
            if s.compile_errors > 0 {
                println!("  Errors:        {}", s.compile_errors);
            }
            println!();
            println!(
                "  Artifacts:     {} ({})",
                s.artifact_count,
                format_bytes(s.cache_size_bytes)
            );
            println!(
                "  Dep graph:     {} contexts, {} files",
                s.dep_graph_contexts, s.dep_graph_files
            );
            println!("  Metadata:      {} entries", s.metadata_entries);
            println!();
            if s.total_links > 0 {
                println!();
                let link_total = s.link_hits + s.link_misses;
                let link_hit_rate = if link_total > 0 {
                    format!("{:.1}%", s.link_hits as f64 / link_total as f64 * 100.0)
                } else {
                    "n/a".to_string()
                };
                println!(
                    "  Links:         {} total ({} cached, {} cold, {} non-cacheable)",
                    s.total_links, s.link_hits, s.link_misses, s.link_non_cacheable
                );
                println!("  Link hit rate: {link_hit_rate}");
            }
            println!();
            println!(
                "  Sessions:      {} active / {} total",
                s.sessions_active, s.sessions_total
            );
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("unexpected response: {other:?}");
            ExitCode::FAILURE
        }
    }
}

async fn cmd_clear(endpoint: &str) -> ExitCode {
    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(_) => {
            eprintln!("daemon not running at {endpoint} — nothing to clear");
            return ExitCode::SUCCESS;
        }
    };

    conn.send(&zccache_protocol::Request::Clear).await.unwrap();
    match conn.recv().await.unwrap() {
        Some(zccache_protocol::Response::Cleared {
            artifacts_removed,
            metadata_cleared,
            dep_graph_contexts_cleared,
            on_disk_bytes_freed,
        }) => {
            println!("Cache cleared:");
            println!("  Artifacts removed:  {artifacts_removed}");
            println!("  Metadata cleared:   {metadata_cleared}");
            println!("  Dep graph contexts: {dep_graph_contexts_cleared}");
            if on_disk_bytes_freed > 0 {
                println!(
                    "  Disk freed:         {}",
                    format_bytes(on_disk_bytes_freed)
                );
            }
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("unexpected response: {other:?}");
            ExitCode::FAILURE
        }
    }
}

async fn cmd_session_start(
    endpoint: &str,
    cwd: &Path,
    log: Option<&Path>,
    track_stats: bool,
) -> ExitCode {
    if let Err(e) = ensure_daemon(endpoint).await {
        eprintln!("cannot start daemon at {endpoint}: {e}");
        return ExitCode::FAILURE;
    }

    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot connect to daemon at {endpoint}: {e}");
            return ExitCode::FAILURE;
        }
    };

    conn.send(&zccache_protocol::Request::SessionStart {
        client_pid: std::process::id(),
        working_dir: cwd.to_path_buf(),
        log_file: log.map(Path::to_path_buf),
        track_stats,
    })
    .await
    .unwrap();

    match conn.recv().await.unwrap() {
        Some(zccache_protocol::Response::SessionStarted { session_id }) => {
            // One-line JSON so scripts can parse both the session ID and start time:
            //   result=$(zccache session-start)
            let started_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            println!(
                r#"{{"session_id":"{}","started_at":{}}}"#,
                session_id, started_at
            );
            ExitCode::SUCCESS
        }
        Some(zccache_protocol::Response::Error { message }) => {
            eprintln!("session-start failed: {message}");
            ExitCode::FAILURE
        }
        other => {
            eprintln!("unexpected response: {other:?}");
            ExitCode::FAILURE
        }
    }
}

async fn cmd_session_end(endpoint: &str, session_id: String) -> ExitCode {
    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot connect to daemon at {endpoint}: {e}");
            return ExitCode::FAILURE;
        }
    };

    conn.send(&zccache_protocol::Request::SessionEnd {
        session_id: session_id.clone(),
    })
    .await
    .unwrap();

    match conn.recv().await.unwrap() {
        Some(zccache_protocol::Response::SessionEnded { stats }) => {
            if let Some(s) = stats {
                let total = s.hits + s.misses;
                let hit_rate = if total > 0 {
                    format!("{:.1}%", s.hits as f64 / total as f64 * 100.0)
                } else {
                    "n/a".to_string()
                };
                eprintln!(
                    "Session {session_id} complete ({})",
                    format_duration_ms(s.duration_ms)
                );
                eprintln!(
                    "  {} compilations: {} hits, {} misses, {} non-cacheable",
                    s.compilations, s.hits, s.misses, s.non_cacheable
                );
                eprintln!("  Hit rate: {hit_rate}");
                if s.time_saved_ms > 0 {
                    eprintln!("  Time saved: ~{}", format_duration_ms(s.time_saved_ms));
                }
            }
            ExitCode::SUCCESS
        }
        Some(zccache_protocol::Response::Error { message }) => {
            eprintln!("session-end failed: {message}");
            ExitCode::FAILURE
        }
        other => {
            eprintln!("unexpected response: {other:?}");
            ExitCode::FAILURE
        }
    }
}

async fn cmd_session_stats(endpoint: &str, session_id: String) -> ExitCode {
    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot connect to daemon at {endpoint}: {e}");
            return ExitCode::FAILURE;
        }
    };

    conn.send(&zccache_protocol::Request::SessionStats {
        session_id: session_id.clone(),
    })
    .await
    .unwrap();

    match conn.recv().await.unwrap() {
        Some(zccache_protocol::Response::SessionStatsResult { stats }) => {
            if let Some(s) = stats {
                let total = s.hits + s.misses;
                let hit_rate = if total > 0 {
                    format!("{:.1}%", s.hits as f64 / total as f64 * 100.0)
                } else {
                    "n/a".to_string()
                };
                eprintln!(
                    "Session {session_id} (active, {})",
                    format_duration_ms(s.duration_ms)
                );
                eprintln!(
                    "  {} compilations: {} hits, {} misses, {} non-cacheable",
                    s.compilations, s.hits, s.misses, s.non_cacheable
                );
                eprintln!("  Hit rate: {hit_rate}");
                if s.time_saved_ms > 0 {
                    eprintln!("  Time saved: ~{}", format_duration_ms(s.time_saved_ms));
                }
            } else {
                eprintln!("Session {session_id}: stats tracking not enabled");
            }
            ExitCode::SUCCESS
        }
        Some(zccache_protocol::Response::Error { message }) => {
            eprintln!("session-stats failed: {message}");
            ExitCode::FAILURE
        }
        other => {
            eprintln!("unexpected response: {other:?}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_crashes(clear: bool) -> ExitCode {
    let crash_dir = zccache_core::config::crash_dump_dir();

    if clear {
        let count = match std::fs::read_dir(&crash_dir) {
            Ok(entries) => {
                let mut n = 0u64;
                for entry in entries.flatten() {
                    if std::fs::remove_file(entry.path()).is_ok() {
                        n += 1;
                    }
                }
                n
            }
            Err(_) => 0,
        };
        println!("Deleted {count} crash dump(s).");
        return ExitCode::SUCCESS;
    }

    let mut dumps: Vec<_> = match std::fs::read_dir(&crash_dir) {
        Ok(entries) => entries
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "txt"))
            .collect(),
        Err(_) => {
            println!("No crash dumps found.");
            return ExitCode::SUCCESS;
        }
    };

    if dumps.is_empty() {
        println!("No crash dumps found.");
        return ExitCode::SUCCESS;
    }

    dumps.sort_by_key(|e| e.file_name());

    println!("Crash dumps ({}):", dumps.len());
    println!();
    for entry in &dumps {
        let path = entry.path();
        println!("  {}", path.display());
        if let Ok(content) = std::fs::read_to_string(&path) {
            for (i, line) in content.lines().enumerate() {
                if i >= 5 {
                    println!("    ...");
                    break;
                }
                println!("    {line}");
            }
            println!();
        }
    }

    ExitCode::SUCCESS
}

// ─── Wrap (compiler wrapper) ───────────────────────────────────────────────

/// Run the compiler/tool directly without caching (ZCCACHE_DISABLE mode).
fn run_passthrough(args: &[String]) -> ExitCode {
    let tool = &args[0];
    let tool_args = if args.len() > 1 { &args[1..] } else { &[] };

    // Resolve the tool path (normalize MSYS paths, search PATH)
    let resolved = resolve_compiler_path(tool);

    match std::process::Command::new(&resolved)
        .args(tool_args)
        .status()
    {
        Ok(status) => exit_code_from_i32(status.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("zccache: failed to run {}: {e}", resolved.display());
            ExitCode::FAILURE
        }
    }
}

/// Wrap a compiler or tool invocation.
///
/// `args` is the full command: ["clang++", "-c", "foo.cpp", "-o", "foo.o"]
/// or ["ar", "rcs", "libfoo.a", "a.o", "b.o"]
///
/// If the first arg is a known archiver (ar, llvm-ar, lib.exe), routes to
/// the link/archive path. Otherwise, routes to the compile path.
///
/// If ZCCACHE_SESSION_ID is set, uses that session and sends the tool
/// as a per-request override. If unset, auto-creates an ephemeral session.
fn run_wrap(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: zccache <compiler|tool> <args...>");
        return ExitCode::FAILURE;
    }

    // ZCCACHE_DISABLE=1 — passthrough to compiler/tool without caching.
    if std::env::var("ZCCACHE_DISABLE").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true")) {
        return run_passthrough(args);
    }

    // Normalize MSYS paths (e.g. /c/Users/... → C:\Users\...) on Windows,
    // then resolve to an absolute path so the daemon can find it.
    let wrapped_tool = resolve_compiler_path(&args[0]);
    let tool_args: Vec<String> = if args.len() > 1 {
        args[1..].to_vec()
    } else {
        Vec::new()
    };

    let cwd = std::env::current_dir().unwrap_or_default();

    let client_env: Vec<(String, String)> = std::env::vars().collect();
    let endpoint = resolve_endpoint(None);

    // Check if this is an archiver or linker tool (including gcc -shared)
    if zccache_compiler::parse_archiver::is_archiver(&args[0])
        || zccache_compiler::parse_linker::is_link_invocation(&args[0], &tool_args)
    {
        return run_async(cmd_link_ephemeral(
            &endpoint,
            &wrapped_tool,
            tool_args,
            cwd,
            client_env,
        ));
    }

    // Otherwise, treat as a compiler invocation
    match std::env::var("ZCCACHE_SESSION_ID") {
        Ok(session_id) => {
            if session_id.is_empty() {
                eprintln!("ZCCACHE_SESSION_ID is empty");
                return ExitCode::FAILURE;
            }
            run_async(cmd_compile(
                &endpoint,
                &session_id,
                tool_args,
                cwd,
                wrapped_tool,
                client_env,
            ))
        }
        Err(_) => {
            // No session — auto-create an ephemeral one for this compilation.
            run_async(cmd_compile_ephemeral(
                &endpoint,
                &wrapped_tool,
                tool_args,
                cwd,
                client_env,
            ))
        }
    }
}

/// Resolve a compiler name/path to an absolute path.
/// Normalizes MSYS paths on Windows, then searches PATH if not already absolute.
fn resolve_compiler_path(compiler: &str) -> PathBuf {
    let normalized = zccache_core::path::normalize_msys_path(compiler);
    let path = Path::new(&normalized);

    // Already absolute — return as-is.
    if path.is_absolute() {
        return PathBuf::from(normalized);
    }

    // Search PATH for the compiler.
    match which_on_path(&normalized) {
        Some(abs) => abs,
        None => PathBuf::from(normalized), // Let the daemon report the error.
    }
}

async fn cmd_compile(
    endpoint: &str,
    session_id: &str,
    args: Vec<String>,
    cwd: PathBuf,
    compiler: PathBuf,
    client_env: Vec<(String, String)>,
) -> ExitCode {
    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot connect to daemon at {endpoint}: {e}");
            return ExitCode::FAILURE;
        }
    };

    conn.send(&zccache_protocol::Request::Compile {
        session_id: session_id.to_string(),
        args,
        cwd,
        compiler,
        env: Some(client_env),
    })
    .await
    .unwrap();

    match conn.recv().await.unwrap() {
        Some(zccache_protocol::Response::CompileResult {
            exit_code,
            stdout,
            stderr,
            ..
        }) => {
            // Relay compiler output
            use std::io::Write;
            let _ = std::io::stdout().write_all(&stdout);
            let _ = std::io::stderr().write_all(&stderr);
            exit_code_from_i32(exit_code)
        }
        Some(zccache_protocol::Response::Error { message }) => {
            eprintln!("zccache error: {message}");
            ExitCode::FAILURE
        }
        other => {
            eprintln!("unexpected response: {other:?}");
            ExitCode::FAILURE
        }
    }
}

/// Ephemeral session: single-roundtrip compile (session start + compile + session end
/// in one IPC message). Used when ZCCACHE_SESSION_ID is not set (drop-in mode).
async fn cmd_compile_ephemeral(
    endpoint: &str,
    compiler: &Path,
    args: Vec<String>,
    cwd: PathBuf,
    client_env: Vec<(String, String)>,
) -> ExitCode {
    // Ensure daemon is running and version-compatible.
    if let Err(e) = ensure_daemon(endpoint).await {
        eprintln!("cannot start daemon at {endpoint}: {e}");
        return ExitCode::FAILURE;
    }
    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot connect to daemon at {endpoint}: {e}");
            return ExitCode::FAILURE;
        }
    };

    conn.send(&zccache_protocol::Request::CompileEphemeral {
        client_pid: std::process::id(),
        working_dir: cwd.clone(),
        compiler: compiler.to_path_buf(),
        args,
        cwd,
        env: Some(client_env),
    })
    .await
    .unwrap();

    match conn.recv().await.unwrap() {
        Some(zccache_protocol::Response::CompileResult {
            exit_code,
            stdout,
            stderr,
            ..
        }) => {
            use std::io::Write;
            let _ = std::io::stdout().write_all(&stdout);
            let _ = std::io::stderr().write_all(&stderr);
            exit_code_from_i32(exit_code)
        }
        Some(zccache_protocol::Response::Error { message }) => {
            eprintln!("zccache error: {message}");
            ExitCode::FAILURE
        }
        other => {
            eprintln!("unexpected response: {other:?}");
            ExitCode::FAILURE
        }
    }
}

/// Ephemeral link/archive: single-roundtrip for `zccache ar ...` etc.
async fn cmd_link_ephemeral(
    endpoint: &str,
    tool: &Path,
    args: Vec<String>,
    cwd: PathBuf,
    client_env: Vec<(String, String)>,
) -> ExitCode {
    if let Err(e) = ensure_daemon(endpoint).await {
        eprintln!("cannot start daemon at {endpoint}: {e}");
        return ExitCode::FAILURE;
    }
    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot connect to daemon at {endpoint}: {e}");
            return ExitCode::FAILURE;
        }
    };

    conn.send(&zccache_protocol::Request::LinkEphemeral {
        client_pid: std::process::id(),
        working_dir: cwd.clone(),
        tool: tool.to_path_buf(),
        args,
        cwd,
        env: Some(client_env),
    })
    .await
    .unwrap();

    match conn.recv().await.unwrap() {
        Some(zccache_protocol::Response::LinkResult {
            exit_code,
            stdout,
            stderr,
            warning,
            ..
        }) => {
            use std::io::Write;
            let _ = std::io::stdout().write_all(&stdout);
            let _ = std::io::stderr().write_all(&stderr);
            if let Some(w) = warning {
                eprintln!("zccache warning: {w}");
            }
            exit_code_from_i32(exit_code)
        }
        Some(zccache_protocol::Response::Error { message }) => {
            eprintln!("zccache error: {message}");
            ExitCode::FAILURE
        }
        other => {
            eprintln!("unexpected response: {other:?}");
            ExitCode::FAILURE
        }
    }
}

// ─── Daemon auto-start ─────────────────────────────────────────────────────

/// Check that the running daemon's version matches the CLI version.
///
/// Sends a Status request to get the daemon's version. Returns `Ok(())` if
/// versions match, or `Err(message)` if there is a mismatch.
async fn check_daemon_version(endpoint: &str) -> Result<(), String> {
    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(_) => return Ok(()), // Can't connect — caller will handle separately.
    };

    if conn.send(&zccache_protocol::Request::Status).await.is_err() {
        return Ok(());
    }

    match conn.recv().await {
        Ok(Some(zccache_protocol::Response::Status(status))) => {
            let daemon_version = if status.version.is_empty() {
                "unknown"
            } else {
                &status.version
            };
            if daemon_version != zccache_core::VERSION {
                return Err(format!(
                    "version mismatch: daemon is v{daemon_version}, client is v{}. \
                     Run `zccache stop` first.",
                    zccache_core::VERSION
                ));
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Ensure the daemon is running. If not, spawn it and wait for it to accept.
///
/// Handles concurrent calls gracefully: when multiple processes race to start
/// the daemon, only one wins the bind. The losers detect this and connect to
/// the winning daemon instead of failing.
///
/// If a running daemon has a different version than this CLI, returns an error
/// telling the user to run `zccache stop` first.
async fn ensure_daemon(endpoint: &str) -> Result<(), String> {
    // Fast path: try to connect
    if connect(endpoint).await.is_ok() {
        // Check version — fail if mismatched.
        check_daemon_version(endpoint).await?;
        return Ok(());
    }

    // Check lock file for a running daemon we just can't reach yet
    if let Some(pid) = zccache_ipc::check_running_daemon() {
        // Daemon process exists — wait a bit for it to become ready
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if connect(endpoint).await.is_ok() {
                return Ok(());
            }
        }
        return Err(format!(
            "daemon process {pid} exists but not accepting connections"
        ));
    }

    // No daemon running — spawn one
    let daemon_bin = find_daemon_binary().ok_or("cannot find zccache-daemon binary")?;

    tracing::debug!(?daemon_bin, %endpoint, "spawning daemon");

    spawn_daemon(&daemon_bin, endpoint)?;

    // Wait for daemon to become ready (up to 10s).
    // Our daemon might win the bind, or another concurrent spawn might win.
    // Either way, we just need a daemon accepting connections on the endpoint.
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if connect(endpoint).await.is_ok() {
            return Ok(());
        }
    }

    // Final attempt: our daemon may have lost the bind race to another
    // process. The winning daemon might have started after our polling began.
    // Check if any daemon is now running and give it one more chance.
    if zccache_ipc::check_running_daemon().is_some() {
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if connect(endpoint).await.is_ok() {
                return Ok(());
            }
        }
    }

    Err("daemon started but not accepting connections after 12s".to_string())
}

/// Find the daemon binary. Looks next to the CLI binary first, then on PATH.
fn find_daemon_binary() -> Option<std::path::PathBuf> {
    let name = if cfg!(windows) {
        "zccache-daemon.exe"
    } else {
        "zccache-daemon"
    };

    // Look next to the CLI binary
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(name);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }

    // Fall back to PATH
    which_on_path(name)
}

/// Simple PATH lookup (no external crate needed).
/// On Windows, also tries appending `.exe` if the name has no extension.
fn which_on_path(name: &str) -> Option<std::path::PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        // On Windows, try with .exe suffix
        #[cfg(windows)]
        if std::path::Path::new(name).extension().is_none() {
            let with_exe = dir.join(format!("{name}.exe"));
            if with_exe.is_file() {
                return Some(with_exe);
            }
        }
    }
    None
}

/// Spawn the daemon as a detached background process.
///
/// On Windows, we must prevent the daemon from inheriting pipe handles.
/// When the CLI is invoked via `subprocess.run(capture_output=True)` (e.g. from
/// Python/meson), the parent creates pipes for stdout/stderr. If the daemon
/// inherits these handles, the parent hangs forever waiting for pipe closure
/// because the daemon never exits.
fn spawn_daemon(bin: &std::path::Path, endpoint: &str) -> Result<(), String> {
    let mut cmd = std::process::Command::new(bin);
    cmd.args(["--foreground", "--endpoint", endpoint]);

    // Detach stdio so the daemon doesn't hold our console
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());

    // Platform-specific: prevent console window on Windows and avoid
    // inheriting parent pipe handles (which cause subprocess hangs).
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);

        // Mark our stdout/stderr as non-inheritable before spawning the daemon.
        // This prevents the daemon from inheriting pipe handles that a grandparent
        // process (e.g. Python subprocess.run) may have created for capture.
        // Without this, the daemon keeps the pipe open indefinitely, causing the
        // grandparent to hang waiting for EOF on the pipe.
        disable_handle_inheritance();
    }

    cmd.spawn()
        .map_err(|e| format!("failed to spawn daemon: {e}"))?;

    // Re-enable inheritance for our own handles (in case we do further spawns)
    #[cfg(windows)]
    restore_handle_inheritance();

    Ok(())
}

/// On Windows, mark stdout/stderr handles as non-inheritable so that child
/// processes (the daemon) do not inherit pipe handles from grandparent processes.
#[cfg(windows)]
fn disable_handle_inheritance() {
    use std::os::windows::io::AsRawHandle;

    extern "system" {
        fn SetHandleInformation(handle: *mut std::ffi::c_void, mask: u32, flags: u32) -> i32;
    }
    const HANDLE_FLAG_INHERIT: u32 = 1;

    // Safety: we're calling a standard Win32 API with valid handle values.
    // The handles come from Rust's stdout/stderr which are always valid.
    unsafe {
        let stdout = std::io::stdout();
        let stderr = std::io::stderr();
        SetHandleInformation(stdout.as_raw_handle() as *mut _, HANDLE_FLAG_INHERIT, 0);
        SetHandleInformation(stderr.as_raw_handle() as *mut _, HANDLE_FLAG_INHERIT, 0);
    }
}

/// Restore stdout/stderr handles as inheritable (undo `disable_handle_inheritance`).
#[cfg(windows)]
fn restore_handle_inheritance() {
    use std::os::windows::io::AsRawHandle;

    extern "system" {
        fn SetHandleInformation(handle: *mut std::ffi::c_void, mask: u32, flags: u32) -> i32;
    }
    const HANDLE_FLAG_INHERIT: u32 = 1;

    unsafe {
        let stdout = std::io::stdout();
        let stderr = std::io::stderr();
        SetHandleInformation(
            stdout.as_raw_handle() as *mut _,
            HANDLE_FLAG_INHERIT,
            HANDLE_FLAG_INHERIT,
        );
        SetHandleInformation(
            stderr.as_raw_handle() as *mut _,
            HANDLE_FLAG_INHERIT,
            HANDLE_FLAG_INHERIT,
        );
    }
}

// ─── Helpers ───────────────────────────────────────────────────────────────

/// Platform-correct connect (returns different types on Unix vs Windows).
#[cfg(unix)]
async fn connect(endpoint: &str) -> Result<zccache_ipc::IpcConnection, zccache_ipc::IpcError> {
    zccache_ipc::connect(endpoint).await
}

#[cfg(windows)]
async fn connect(
    endpoint: &str,
) -> Result<zccache_ipc::IpcClientConnection, zccache_ipc::IpcError> {
    zccache_ipc::connect(endpoint).await
}

fn resolve_endpoint(explicit: Option<&str>) -> String {
    if let Some(ep) = explicit {
        return ep.to_string();
    }
    if let Ok(ep) = std::env::var("ZCCACHE_ENDPOINT") {
        return ep;
    }
    zccache_ipc::default_endpoint()
}

fn run_async(future: impl std::future::Future<Output = ExitCode>) -> ExitCode {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to create tokio runtime")
        .block_on(future)
}

fn format_uptime(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        format!("{h}h {m}m")
    }
}

fn format_duration_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        let secs = ms / 1000;
        format!("{}m {}s", secs / 60, secs % 60)
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes == 0 {
        "0 B".to_string()
    } else if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_zero_stays_zero() {
        assert_eq!(exit_code_from_i32(0), ExitCode::from(0));
    }

    #[test]
    fn exit_code_one_stays_one() {
        assert_eq!(exit_code_from_i32(1), ExitCode::from(1));
    }

    #[test]
    fn exit_code_255_stays_255() {
        assert_eq!(exit_code_from_i32(255), ExitCode::from(255));
    }

    #[test]
    fn exit_code_256_becomes_one_not_zero() {
        // Without the fix, 256 as u8 == 0, masking the failure.
        assert_ne!(exit_code_from_i32(256), ExitCode::from(0));
        assert_eq!(exit_code_from_i32(256), ExitCode::from(1));
    }

    #[test]
    fn exit_code_512_becomes_one_not_zero() {
        assert_eq!(exit_code_from_i32(512), ExitCode::from(1));
    }

    #[test]
    fn exit_code_negative_preserves_failure() {
        // -1 & 0xFF == 255
        assert_ne!(exit_code_from_i32(-1), ExitCode::from(0));
        assert_eq!(exit_code_from_i32(-1), ExitCode::from(255));
    }

    #[test]
    fn exit_code_257_keeps_low_byte() {
        // 257 & 0xFF == 1, non-zero, so kept as-is.
        assert_eq!(exit_code_from_i32(257), ExitCode::from(1));
    }
}
