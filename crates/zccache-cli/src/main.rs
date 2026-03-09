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
use std::process::ExitCode;

/// zccache -- fast local compiler cache.
#[derive(Debug, Parser)]
#[command(name = "zccache", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
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
        /// Path to the compiler executable.
        #[arg(long)]
        compiler: String,
        /// Working directory (defaults to current dir).
        #[arg(long)]
        cwd: Option<String>,
        /// Path to a log file for this session.
        #[arg(long)]
        log: Option<String>,
        /// IPC endpoint (default: platform-specific).
        #[arg(long)]
        endpoint: Option<String>,
    },
    /// End a build session.
    #[command(name = "session-end")]
    SessionEnd {
        /// Session ID to end.
        session_id: u64,
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
    "help",
    "--help",
    "-h",
    "--version",
    "-V",
];

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();

    // Auto-detect: if first arg isn't a known subcommand, enter wrap mode.
    // e.g., `zccache clang++ -c foo.cpp -o foo.o`
    if args.len() > 1 && !KNOWN_SUBCOMMANDS.contains(&args[1].as_str()) {
        return run_wrap(&args[1..]);
    }

    let cli = Cli::parse();

    init_tracing();

    match cli.command {
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
            eprintln!("zccache clear: not yet implemented");
            ExitCode::FAILURE
        }
        Commands::SessionStart {
            compiler,
            cwd,
            log,
            endpoint,
        } => {
            let endpoint = resolve_endpoint(endpoint.as_deref());
            let cwd = cwd.unwrap_or_else(|| {
                std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            });
            run_async(cmd_session_start(
                &endpoint,
                &compiler,
                &cwd,
                log.as_deref(),
            ))
        }
        Commands::SessionEnd {
            session_id,
            endpoint,
        } => {
            let endpoint = resolve_endpoint(endpoint.as_deref());
            run_async(cmd_session_end(&endpoint, session_id))
        }
        Commands::Wrap { args } => run_wrap(&args),
        Commands::Inspect { key } => {
            eprintln!("zccache inspect {key}: not yet implemented");
            ExitCode::FAILURE
        }
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
            println!("zccache daemon ({})", endpoint);
            println!("  artifacts:  {}", s.artifact_count);
            println!("  cache size: {} bytes", s.cache_size_bytes);
            println!("  metadata:   {} entries", s.metadata_entries);
            println!("  uptime:     {}s", s.uptime_secs);
            println!("  hits/miss:  {} / {}", s.cache_hits, s.cache_misses);
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
    compiler: &str,
    cwd: &str,
    log: Option<&str>,
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
        working_dir: cwd.to_string(),
        compiler: compiler.to_string(),
        log_file: log.map(String::from),
    })
    .await
    .unwrap();

    match conn.recv().await.unwrap() {
        Some(zccache_protocol::Response::SessionStarted { session_id, .. }) => {
            // Print just the session ID to stdout so scripts can capture it:
            //   SESSION_ID=$(zccache session-start --compiler /path/to/clang++)
            println!("{session_id}");
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

async fn cmd_session_end(endpoint: &str, session_id: u64) -> ExitCode {
    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot connect to daemon at {endpoint}: {e}");
            return ExitCode::FAILURE;
        }
    };

    conn.send(&zccache_protocol::Request::SessionEnd { session_id })
        .await
        .unwrap();

    match conn.recv().await.unwrap() {
        Some(zccache_protocol::Response::SessionEnded) => ExitCode::SUCCESS,
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

// ─── Wrap (compiler wrapper) ───────────────────────────────────────────────

/// Wrap a compiler invocation. Reads ZCCACHE_SESSION_ID from env,
/// connects to the daemon, sends Compile, relays stdout/stderr/exit_code.
///
/// `args` is the full compiler command: ["clang++", "-c", "foo.cpp", "-o", "foo.o"]
/// The first arg (compiler path) is stripped since the daemon already knows
/// the compiler from SessionStart.
fn run_wrap(args: &[String]) -> ExitCode {
    let session_id: u64 = match std::env::var("ZCCACHE_SESSION_ID") {
        Ok(val) => match val.parse() {
            Ok(id) => id,
            Err(_) => {
                eprintln!("ZCCACHE_SESSION_ID={val:?} is not a valid u64");
                return ExitCode::FAILURE;
            }
        },
        Err(_) => {
            eprintln!("ZCCACHE_SESSION_ID not set. Start a session first:");
            eprintln!(
                "  export ZCCACHE_SESSION_ID=$(zccache session-start --compiler /path/to/clang++)"
            );
            return ExitCode::FAILURE;
        }
    };

    if args.is_empty() {
        eprintln!("usage: zccache <compiler> <args...>");
        return ExitCode::FAILURE;
    }

    // args[0] is the compiler (redundant — daemon knows from session).
    // args[1..] are the actual compiler flags.
    let compiler_args: Vec<String> = if args.len() > 1 {
        args[1..].to_vec()
    } else {
        Vec::new()
    };

    let cwd = std::env::current_dir()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();

    let endpoint = resolve_endpoint(None);

    run_async(cmd_compile(&endpoint, session_id, compiler_args, cwd))
}

async fn cmd_compile(endpoint: &str, session_id: u64, args: Vec<String>, cwd: String) -> ExitCode {
    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot connect to daemon at {endpoint}: {e}");
            return ExitCode::FAILURE;
        }
    };

    conn.send(&zccache_protocol::Request::Compile {
        session_id,
        args,
        cwd,
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
            ExitCode::from(exit_code as u8)
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

/// Ensure the daemon is running. If not, spawn it and wait for it to accept.
async fn ensure_daemon(endpoint: &str) -> Result<(), String> {
    // Fast path: try to connect
    if connect(endpoint).await.is_ok() {
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

    // Wait for daemon to become ready (up to 5s)
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if connect(endpoint).await.is_ok() {
            return Ok(());
        }
    }

    Err("daemon started but not accepting connections after 5s".to_string())
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
fn which_on_path(name: &str) -> Option<std::path::PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Spawn the daemon as a detached background process.
fn spawn_daemon(bin: &std::path::Path, endpoint: &str) -> Result<(), String> {
    let mut cmd = std::process::Command::new(bin);
    cmd.args(["--foreground", "--endpoint", endpoint]);

    // Detach stdio so the daemon doesn't hold our console
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());

    // Platform-specific: prevent console window on Windows
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    cmd.spawn()
        .map_err(|e| format!("failed to spawn daemon: {e}"))?;

    Ok(())
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

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();
}
