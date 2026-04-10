//! zccache CLI -- command-line interface for the compiler cache.
//!
//! Usage modes:
//!
//! 1. Subcommand mode:
//!    zccache session-start --compiler /path/to/clang++
//!    zccache session-end `<id>`
//!    zccache status
//!
//! 2. Compiler wrapper mode (auto-detected):
//!    ZCCACHE_SESSION_ID=42 zccache clang++ -c foo.cpp -o foo.o
//!
//!    If the first arg isn't a known subcommand, zccache treats
//!    the entire command line as a compiler invocation and forwards
//!    it to the daemon via the session from ZCCACHE_SESSION_ID.

#[cfg(unix)]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(windows)]
#[global_allocator]
static GLOBAL_WIN: mimalloc::MiMalloc = mimalloc::MiMalloc;

use clap::{Parser, Subcommand};
use std::path::Path;
use std::process::ExitCode;
use zccache_cli::{
    client_download, run_ino_convert_cached, ArchiveFormat, DownloadParams, InoConvertOptions,
    WaitMode,
};
use zccache_core::NormalizedPath;

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
    #[command(visible_alias = "kill")]
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
        /// Write a per-session JSONL compile journal to this path (must end in .jsonl).
        #[arg(long)]
        journal: Option<String>,
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
    /// Fingerprint-based file change detection.
    ///
    /// Answers "have files changed since the last successful operation?" by
    /// querying the daemon's in-memory watch state (<1ms on cache hit).
    #[command(name = "fp")]
    Fp {
        /// Path to the cache file (e.g., .cache/lint.json).
        #[arg(long)]
        cache_file: String,

        /// Cache algorithm: hash or two-layer.
        #[arg(long, default_value = "two-layer")]
        cache_type: String,

        /// IPC endpoint (default: platform-specific).
        #[arg(long)]
        endpoint: Option<String>,

        #[command(subcommand)]
        fp_command: FpCommands,
    },
    /// Convert an Arduino `.ino` sketch into a generated `.ino.cpp`.
    #[command(name = "ino")]
    Ino {
        /// Input `.ino` file.
        #[arg(long)]
        input: String,
        /// Output `.ino.cpp` file.
        #[arg(long)]
        output: String,
        /// Extra clang arguments used when parsing the `.ino`.
        #[arg(long = "clang-arg")]
        clang_args: Vec<String>,
        /// Do not inject `#include <Arduino.h>`.
        #[arg(long)]
        no_arduino_include: bool,
    },
    /// Download and optionally unarchive an artifact using the dedicated download daemon.
    Download {
        /// Source URL.
        url: String,
        /// Optional archive/cache path. If omitted, zccache chooses a deterministic cache path.
        archive_path: Option<String>,
        /// Optional destination to expand or unarchive into.
        #[arg(long = "unarchive")]
        unarchive_path: Option<String>,
        /// Optional expected SHA-256 of the downloaded artifact.
        #[arg(long = "sha256")]
        expected_sha256: Option<String>,
        /// Number of parallel range parts to use when the server supports it.
        #[arg(long = "multipart-parts")]
        multipart_parts: Option<usize>,
        /// Return immediately with `locked` if another client owns the artifact lock.
        #[arg(long)]
        no_wait: bool,
        /// Report what would happen without mutating the filesystem.
        #[arg(long)]
        dry_run: bool,
        /// Force re-download and re-expand even if cached state is already valid.
        #[arg(long)]
        force: bool,
    },
}

/// Fingerprint subcommands.
#[derive(Debug, Subcommand)]
enum FpCommands {
    /// Check if files have changed since last success.
    ///
    /// Exit 0 = operation should run (files changed).
    /// Exit 1 = skip (no changes detected).
    Check {
        /// Root directory to scan (default: current directory).
        #[arg(long, default_value = ".")]
        root: String,

        /// File extensions to include (without dot, e.g., "rs", "cpp").
        /// Cannot be used with --include.
        #[arg(long, conflicts_with = "include")]
        ext: Vec<String>,

        /// Glob patterns for files to include (e.g., "**/*.rs").
        /// Cannot be used with --ext.
        #[arg(long, conflicts_with = "ext")]
        include: Vec<String>,

        /// Patterns or directory names to exclude.
        #[arg(long)]
        exclude: Vec<String>,
    },
    /// Mark the previous check as successful.
    #[command(name = "mark-success")]
    MarkSuccess,
    /// Mark the previous check as failed.
    #[command(name = "mark-failure")]
    MarkFailure,
    /// Invalidate the cache (delete all state).
    Invalidate,
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
    "fp",
    "ino",
    "download",
    "help",
    "--help",
    "-h",
    "--version",
    "-V",
];

fn absolute_path(path: &str) -> NormalizedPath {
    let path = Path::new(path);
    if path.is_absolute() {
        path.into()
    } else {
        std::env::current_dir()
            .unwrap_or_default()
            .join(path)
            .into()
    }
}

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
        Commands::Ino {
            input,
            output,
            clang_args,
            no_arduino_include,
        } => match run_ino_convert_cached(
            Path::new(&input),
            Path::new(&output),
            &InoConvertOptions {
                clang_args,
                inject_arduino_include: !no_arduino_include,
            },
        ) {
            Ok(_) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("zccache: {err}");
                ExitCode::FAILURE
            }
        },
        Commands::Download {
            url,
            archive_path,
            unarchive_path,
            expected_sha256,
            multipart_parts,
            no_wait,
            dry_run,
            force,
        } => cmd_download(DownloadParams {
            url,
            archive_path: archive_path.map(Into::into),
            unarchive_path: unarchive_path.map(Into::into),
            expected_sha256,
            archive_format: ArchiveFormat::Auto,
            multipart_parts,
            wait_mode: if no_wait {
                WaitMode::NoWait
            } else {
                WaitMode::Block
            },
            dry_run,
            force,
        }),
        Commands::SessionStart {
            cwd,
            log,
            endpoint,
            stats,
            journal,
        } => {
            let endpoint = resolve_endpoint(endpoint.as_deref());
            let cwd = cwd
                .map(NormalizedPath::from)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default().into());
            let log = log.map(|p| absolute_path(&p));
            let journal = journal.map(|p| {
                if !p.ends_with(".jsonl") {
                    eprintln!("error: --journal path must end in .jsonl");
                    std::process::exit(1);
                }
                absolute_path(&p)
            });
            run_async(cmd_session_start(
                &endpoint,
                cwd.as_path(),
                log.as_deref(),
                stats,
                journal,
            ))
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
        Commands::Fp {
            cache_file,
            cache_type,
            endpoint,
            fp_command,
        } => {
            let endpoint = resolve_endpoint(endpoint.as_deref());
            let cache_file = absolute_path(&cache_file);
            match fp_command {
                FpCommands::Check {
                    root,
                    ext,
                    include,
                    exclude,
                } => {
                    let root = absolute_path(&root);
                    run_async(cmd_fp_check(
                        &endpoint,
                        cache_file.as_path(),
                        &cache_type,
                        root.as_path(),
                        &ext,
                        &include,
                        &exclude,
                    ))
                }
                FpCommands::MarkSuccess => {
                    run_async(cmd_fp_mark(&endpoint, cache_file.as_path(), true))
                }
                FpCommands::MarkFailure => {
                    run_async(cmd_fp_mark(&endpoint, cache_file.as_path(), false))
                }
                FpCommands::Invalidate => {
                    run_async(cmd_fp_invalidate(&endpoint, cache_file.as_path()))
                }
            }
        }
    }
}

// ─── Subcommand implementations ────────────────────────────────────────────

fn cmd_download(params: DownloadParams) -> ExitCode {
    match client_download(None, params) {
        Ok(result) => {
            println!("status={:?}", result.status);
            println!("archive_path={}", result.cache_path.display());
            println!("sha256={}", result.sha256);
            if let Some(unarchive_path) = &result.expanded_path {
                println!("unarchive_path={}", unarchive_path.display());
            }
            if let Some(bytes) = result.bytes {
                println!("bytes={bytes}");
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("zccache download: {err}");
            ExitCode::FAILURE
        }
    }
}

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
            let Some(pid) = zccache_ipc::check_running_daemon() else {
                eprintln!("daemon not running at {endpoint}");
                return ExitCode::SUCCESS;
            };

            match zccache_ipc::force_kill_process(pid) {
                Ok(()) => {
                    for _ in 0..50 {
                        if !zccache_ipc::is_process_alive(pid) {
                            zccache_ipc::remove_lock_file();
                            eprintln!(
                                "daemon process {pid} terminated after IPC connection failed"
                            );
                            return ExitCode::SUCCESS;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    eprintln!(
                        "zccache: sent termination to daemon process {pid}, but it did not exit"
                    );
                    return ExitCode::FAILURE;
                }
                Err(e) => {
                    eprintln!(
                        "zccache: cannot connect to daemon at {endpoint}, and failed to kill \
                         locked process {pid}: {e}"
                    );
                    return ExitCode::FAILURE;
                }
            }
        }
    };

    if let Err(e) = conn.send(&zccache_protocol::Request::Shutdown).await {
        eprintln!("zccache: failed to send to daemon: {e}");
        return ExitCode::FAILURE;
    }
    let recv_result = match conn.recv().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("zccache: broken connection to daemon: {e}");
            return ExitCode::FAILURE;
        }
    };
    match recv_result {
        Some(zccache_protocol::Response::ShuttingDown) => {
            eprintln!("daemon stopped");
            ExitCode::SUCCESS
        }
        None => {
            eprintln!("zccache: lost connection to daemon (no response received)");
            ExitCode::FAILURE
        }
        Some(other) => {
            eprintln!("zccache: unexpected response from daemon: {other:?}");
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

    if let Err(e) = conn.send(&zccache_protocol::Request::Status).await {
        eprintln!("zccache: failed to send to daemon: {e}");
        return ExitCode::FAILURE;
    }
    let recv_result = match conn.recv().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("zccache: broken connection to daemon: {e}");
            return ExitCode::FAILURE;
        }
    };
    match recv_result {
        Some(zccache_protocol::Response::Status(s)) => {
            let total = s.cache_hits + s.cache_misses;
            let hit_rate = if total > 0 {
                format!("{:.1}%", s.cache_hits as f64 / total as f64 * 100.0)
            } else {
                "n/a".to_string()
            };

            println!(
                "zccache daemon v{} (protocol v{}) ({}) — uptime {}",
                if s.version.is_empty() {
                    "unknown"
                } else {
                    &s.version
                },
                zccache_protocol::PROTOCOL_VERSION,
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
            {
                let disk_info = if s.dep_graph_disk_size > 0 {
                    format!(
                        "v{}, {} on disk",
                        s.dep_graph_version,
                        format_bytes(s.dep_graph_disk_size)
                    )
                } else {
                    format!("v{}, not persisted", s.dep_graph_version)
                };
                println!(
                    "  Dep graph:     {} contexts, {} files ({})",
                    s.dep_graph_contexts, s.dep_graph_files, disk_info
                );
            }
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
        None => {
            eprintln!("zccache: lost connection to daemon (no response received)");
            ExitCode::FAILURE
        }
        Some(other) => {
            eprintln!("zccache: unexpected response from daemon: {other:?}");
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

    if let Err(e) = conn.send(&zccache_protocol::Request::Clear).await {
        eprintln!("zccache: failed to send to daemon: {e}");
        return ExitCode::FAILURE;
    }
    let recv_result = match conn.recv().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("zccache: broken connection to daemon: {e}");
            return ExitCode::FAILURE;
        }
    };
    match recv_result {
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
        None => {
            eprintln!("zccache: lost connection to daemon (no response received)");
            ExitCode::FAILURE
        }
        Some(other) => {
            eprintln!("zccache: unexpected response from daemon: {other:?}");
            ExitCode::FAILURE
        }
    }
}

async fn cmd_session_start(
    endpoint: &str,
    cwd: &Path,
    log: Option<&Path>,
    track_stats: bool,
    journal: Option<NormalizedPath>,
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

    if let Err(e) = conn
        .send(&zccache_protocol::Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: cwd.into(),
            log_file: log.map(NormalizedPath::from),
            track_stats,
            journal_path: journal,
        })
        .await
    {
        eprintln!("zccache: failed to send to daemon: {e}");
        return ExitCode::FAILURE;
    }

    let recv_result = match conn.recv().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("zccache: broken connection to daemon: {e}");
            return ExitCode::FAILURE;
        }
    };
    match recv_result {
        Some(zccache_protocol::Response::SessionStarted {
            session_id,
            journal_path,
        }) => {
            // One-line JSON so scripts can parse both the session ID and start time:
            //   result=$(zccache session-start)
            let started_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            if let Some(ref jp) = journal_path {
                // Escape backslashes for valid JSON (Windows paths contain `\`)
                let jp_escaped = jp.display().to_string().replace('\\', "\\\\");
                println!(
                    r#"{{"session_id":"{}","started_at":{},"journal_path":"{}"}}"#,
                    session_id, started_at, jp_escaped
                );
            } else {
                println!(
                    r#"{{"session_id":"{}","started_at":{}}}"#,
                    session_id, started_at
                );
            }
            ExitCode::SUCCESS
        }
        Some(zccache_protocol::Response::Error { message }) => {
            eprintln!("session-start failed: {message}");
            ExitCode::FAILURE
        }
        None => {
            eprintln!("zccache: lost connection to daemon (no response received)");
            ExitCode::FAILURE
        }
        Some(other) => {
            eprintln!("zccache: unexpected response from daemon: {other:?}");
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

    if let Err(e) = conn
        .send(&zccache_protocol::Request::SessionEnd {
            session_id: session_id.clone(),
        })
        .await
    {
        eprintln!("zccache: failed to send to daemon: {e}");
        return ExitCode::FAILURE;
    }

    let recv_result = match conn.recv().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("zccache: broken connection to daemon: {e}");
            return ExitCode::FAILURE;
        }
    };
    match recv_result {
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
        None => {
            eprintln!("zccache: lost connection to daemon (no response received)");
            ExitCode::FAILURE
        }
        Some(other) => {
            eprintln!("zccache: unexpected response from daemon: {other:?}");
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

    if let Err(e) = conn
        .send(&zccache_protocol::Request::SessionStats {
            session_id: session_id.clone(),
        })
        .await
    {
        eprintln!("zccache: failed to send to daemon: {e}");
        return ExitCode::FAILURE;
    }

    let recv_result = match conn.recv().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("zccache: broken connection to daemon: {e}");
            return ExitCode::FAILURE;
        }
    };
    match recv_result {
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
        None => {
            eprintln!("zccache: lost connection to daemon (no response received)");
            ExitCode::FAILURE
        }
        Some(other) => {
            eprintln!("zccache: unexpected response from daemon: {other:?}");
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

// ─── Fingerprint subcommands ──────────────────────────────────────────────

async fn cmd_fp_check(
    endpoint: &str,
    cache_file: &Path,
    cache_type: &str,
    root: &Path,
    ext: &[String],
    include: &[String],
    exclude: &[String],
) -> ExitCode {
    if let Err(e) = ensure_daemon(endpoint).await {
        eprintln!("zccache fp: failed to start daemon: {e}");
        return ExitCode::from(2);
    }

    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("zccache fp: cannot connect to daemon: {e}");
            return ExitCode::from(2);
        }
    };

    let request = zccache_protocol::Request::FingerprintCheck {
        cache_file: cache_file.into(),
        cache_type: cache_type.to_string(),
        root: root.into(),
        extensions: ext.to_vec(),
        include_globs: include.to_vec(),
        exclude: exclude.to_vec(),
    };

    if let Err(e) = conn.send(&request).await {
        eprintln!("zccache fp: send error: {e}");
        return ExitCode::from(2);
    }

    match conn.recv::<zccache_protocol::Response>().await {
        Ok(Some(zccache_protocol::Response::FingerprintCheckResult {
            decision,
            reason,
            changed_files,
        })) => {
            if decision == "skip" {
                eprintln!("zccache fp: skip (no changes)");
                ExitCode::from(1)
            } else {
                let reason_str = reason.as_deref().unwrap_or("unknown");
                if changed_files.is_empty() {
                    eprintln!("zccache fp: run ({reason_str})");
                } else {
                    eprintln!(
                        "zccache fp: run ({reason_str}, {} file(s) changed)",
                        changed_files.len()
                    );
                }
                ExitCode::SUCCESS
            }
        }
        Ok(Some(zccache_protocol::Response::Error { message })) => {
            eprintln!("zccache fp: daemon error: {message}");
            ExitCode::from(2)
        }
        Ok(other) => {
            eprintln!("zccache fp: unexpected response: {other:?}");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("zccache fp: recv error: {e}");
            ExitCode::from(2)
        }
    }
}

async fn cmd_fp_mark(endpoint: &str, cache_file: &Path, success: bool) -> ExitCode {
    if let Err(e) = ensure_daemon(endpoint).await {
        eprintln!("zccache fp: failed to start daemon: {e}");
        return ExitCode::from(2);
    }

    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("zccache fp: cannot connect to daemon: {e}");
            return ExitCode::from(2);
        }
    };

    let request = if success {
        zccache_protocol::Request::FingerprintMarkSuccess {
            cache_file: cache_file.into(),
        }
    } else {
        zccache_protocol::Request::FingerprintMarkFailure {
            cache_file: cache_file.into(),
        }
    };

    if let Err(e) = conn.send(&request).await {
        eprintln!("zccache fp: send error: {e}");
        return ExitCode::from(2);
    }

    match conn.recv::<zccache_protocol::Response>().await {
        Ok(Some(zccache_protocol::Response::FingerprintAck)) => {
            let label = if success {
                "mark-success"
            } else {
                "mark-failure"
            };
            eprintln!("zccache fp: {label}");
            ExitCode::SUCCESS
        }
        Ok(Some(zccache_protocol::Response::Error { message })) => {
            eprintln!("zccache fp: daemon error: {message}");
            ExitCode::from(2)
        }
        Ok(other) => {
            eprintln!("zccache fp: unexpected response: {other:?}");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("zccache fp: recv error: {e}");
            ExitCode::from(2)
        }
    }
}

async fn cmd_fp_invalidate(endpoint: &str, cache_file: &Path) -> ExitCode {
    if let Err(e) = ensure_daemon(endpoint).await {
        eprintln!("zccache fp: failed to start daemon: {e}");
        return ExitCode::from(2);
    }

    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("zccache fp: cannot connect to daemon: {e}");
            return ExitCode::from(2);
        }
    };

    let request = zccache_protocol::Request::FingerprintInvalidate {
        cache_file: cache_file.into(),
    };

    if let Err(e) = conn.send(&request).await {
        eprintln!("zccache fp: send error: {e}");
        return ExitCode::from(2);
    }

    match conn.recv::<zccache_protocol::Response>().await {
        Ok(Some(zccache_protocol::Response::FingerprintAck)) => {
            eprintln!("zccache fp: invalidated");
            ExitCode::SUCCESS
        }
        Ok(Some(zccache_protocol::Response::Error { message })) => {
            eprintln!("zccache fp: daemon error: {message}");
            ExitCode::from(2)
        }
        Ok(other) => {
            eprintln!("zccache fp: unexpected response: {other:?}");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("zccache fp: recv error: {e}");
            ExitCode::from(2)
        }
    }
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

// ─── Rustfmt caching ───────────────────────────────────────────────────────

/// Run rustfmt with format caching.
///
/// Files whose content hash is already in the format cache are skipped entirely,
/// preserving their mtime and avoiding unnecessary downstream rebuilds.
/// After formatting, the new content hash of each file is stored in the cache.
fn run_rustfmt_cached(rustfmt_path: &Path, args: &[String], cwd: &Path) -> ExitCode {
    use zccache_compiler::parse_rustfmt::{find_rustfmt_config, parse_rustfmt_invocation};

    let parsed = match parse_rustfmt_invocation(args) {
        Some(p) => p,
        None => {
            // --help, --version, or stdin mode: pass through
            return run_tool_direct(rustfmt_path, args);
        }
    };

    // Build format context: rustfmt binary identity + config + flags.
    // Changes to any of these invalidate the entire format cache scope.
    let context_hash = {
        let mut hasher = zccache_hash::StreamHasher::new();
        hasher.update(b"zccache-fmt-v1");

        // Hash rustfmt binary content for version identity
        if let Ok(bin_hash) = zccache_hash::hash_file(rustfmt_path) {
            hasher.update(bin_hash.as_bytes());
        } else {
            hasher.update(b"unknown-binary");
        }

        // Hash config file content (if found)
        let config_path = parsed
            .config_path
            .clone()
            .or_else(|| find_rustfmt_config(cwd));
        if let Some(ref cfg) = config_path {
            if let Ok(cfg_hash) = zccache_hash::hash_file(cfg) {
                hasher.update(cfg_hash.as_bytes());
            }
        }

        // Hash flags (edition, --check, etc.)
        for flag in &parsed.flags {
            hasher.update(flag.as_bytes());
            hasher.update(b"\0");
        }

        hasher.finalize().to_hex()
    };

    // Format cache directory: {cache_dir}/fmt/{context_hash}/
    let cache_dir = zccache_core::config::default_cache_dir()
        .join("fmt")
        .join(&context_hash);

    // Ensure cache dir exists
    let _ = std::fs::create_dir_all(&cache_dir);

    // Resolve source files to absolute paths and check cache (parallel)
    use rayon::prelude::*;

    let results: Vec<(NormalizedPath, bool, Option<zccache_hash::ContentHash>)> = parsed
        .source_files
        .par_iter()
        .map(|src| {
            let abs = if src.is_absolute() {
                src.clone()
            } else {
                cwd.join(src).into()
            };
            let (is_hit, hash) = match zccache_hash::hash_file(&abs) {
                Ok(content_hash) => {
                    let marker = cache_dir.join(content_hash.to_hex());
                    (marker.exists(), Some(content_hash))
                }
                Err(_) => (false, None),
            };
            (abs, is_hit, hash)
        })
        .collect();

    let mut miss_files: Vec<NormalizedPath> = Vec::new();
    let mut all_files: Vec<(NormalizedPath, bool, Option<zccache_hash::ContentHash>)> = Vec::new();
    for (abs, is_hit, hash) in results {
        if !is_hit {
            miss_files.push(abs.clone());
        }
        all_files.push((abs, is_hit, hash));
    }

    // All files are cache hits — skip rustfmt entirely (mtime preserved!)
    if miss_files.is_empty() {
        if parsed.check_mode {
            // --check: all files are known-formatted → exit 0
            return ExitCode::SUCCESS;
        }
        // Normal mode: all files already formatted → nothing to do
        return ExitCode::SUCCESS;
    }

    // Run rustfmt on miss files only (normal mode) or all files (--check mode)
    let exit_code = if parsed.check_mode {
        // --check mode: run on miss files only; if all would pass, we
        // already returned above. For misses, we must run to determine
        // if they're formatted.
        run_rustfmt_on_files(rustfmt_path, args, &miss_files, &parsed)
    } else {
        // Normal mode: run on miss files only
        run_rustfmt_on_files(rustfmt_path, args, &miss_files, &parsed)
    };

    let exit_i32 = match exit_code {
        Ok(code) => code,
        Err(e) => {
            eprintln!("zccache: failed to run rustfmt: {e}");
            return ExitCode::FAILURE;
        }
    };

    // On success (exit 0), store new content hashes in format cache
    if exit_i32 == 0 {
        // For --check mode with exit 0: the miss files were already formatted
        // (we just didn't know it). Reuse the hash from the lookup phase.
        // For normal mode with exit 0: files were reformatted. Must re-hash.
        for (abs, was_hit, cached_hash) in &all_files {
            if *was_hit {
                continue; // Already in cache
            }
            let new_hash = if parsed.check_mode {
                *cached_hash
            } else {
                zccache_hash::hash_file(abs).ok()
            };
            if let Some(h) = new_hash {
                let marker = cache_dir.join(h.to_hex());
                let _ = std::fs::write(&marker, b"");
            }
        }
    }

    exit_code_from_i32(exit_i32)
}

/// Run rustfmt on a specific set of files, reconstructing the argument list.
fn run_rustfmt_on_files(
    rustfmt_path: &Path,
    original_args: &[String],
    files: &[NormalizedPath],
    parsed: &zccache_compiler::parse_rustfmt::ParsedRustfmt,
) -> Result<i32, std::io::Error> {
    // Reconstruct args: flags + the miss files (not the original file list)
    let mut cmd = std::process::Command::new(rustfmt_path);
    cmd.args(&parsed.flags);
    for f in files {
        cmd.arg(f);
    }

    // Suppress original args' source files — we pass our filtered list above.
    // But we need to preserve any non-file, non-flag args. In practice,
    // flags + files covers everything.
    let _ = original_args; // intentionally unused — we reconstruct from parsed

    let status = cmd.status()?;
    Ok(status.code().unwrap_or(1))
}

/// Run a tool directly and return its exit code.
fn run_tool_direct(tool: &Path, args: &[String]) -> ExitCode {
    match std::process::Command::new(tool).args(args).status() {
        Ok(status) => exit_code_from_i32(status.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("zccache: failed to run {}: {e}", tool.display());
            ExitCode::FAILURE
        }
    }
}

// ─── Wrap (compiler wrapper) ───────────────────────────────────────────────

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

    // Release the CWD handle on the build directory. On Windows, a process's
    // CWD holds an implicit kernel handle that prevents the directory from
    // being deleted. We've captured everything we need into local variables.
    let _ = std::env::set_current_dir(std::env::temp_dir());

    // Check if this is a rustfmt invocation — handle via format cache path
    if zccache_compiler::detect_family(&args[0]).is_formatter() {
        return run_rustfmt_cached(&wrapped_tool, &tool_args, &cwd);
    }

    // Check if this is an archiver or linker tool (including gcc -shared)
    if zccache_compiler::parse_archiver::is_archiver(&args[0])
        || zccache_compiler::parse_linker::is_link_invocation(&args[0], &tool_args)
    {
        return run_async(cmd_link_ephemeral(
            &endpoint,
            &wrapped_tool,
            tool_args,
            cwd.into(),
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
                cwd.into(),
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
                cwd.into(),
                client_env,
            ))
        }
    }
}

/// Resolve a compiler name/path to an absolute path.
/// Normalizes MSYS paths on Windows, then searches PATH if not already absolute.
fn resolve_compiler_path(compiler: &str) -> NormalizedPath {
    let normalized = zccache_core::path::normalize_msys_path(compiler);
    let path = Path::new(&normalized);

    // Already absolute — return as-is.
    if path.is_absolute() {
        return normalized.into();
    }

    // Search PATH for the compiler.
    match which_on_path(&normalized) {
        Some(abs) => abs,
        None => normalized.into(), // Let the daemon report the error.
    }
}

async fn cmd_compile(
    endpoint: &str,
    session_id: &str,
    args: Vec<String>,
    cwd: NormalizedPath,
    compiler: NormalizedPath,
    client_env: Vec<(String, String)>,
) -> ExitCode {
    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot connect to daemon at {endpoint}: {e}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(e) = conn
        .send(&zccache_protocol::Request::Compile {
            session_id: session_id.to_string(),
            args,
            cwd,
            compiler,
            env: Some(client_env),
        })
        .await
    {
        eprintln!("zccache: failed to send to daemon: {e}");
        return ExitCode::FAILURE;
    }

    let recv_result = match conn.recv().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("zccache: broken connection to daemon: {e}");
            return ExitCode::FAILURE;
        }
    };
    match recv_result {
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
        None => {
            eprintln!("zccache: lost connection to daemon (no response received)");
            ExitCode::FAILURE
        }
        Some(other) => {
            eprintln!("zccache: unexpected response from daemon: {other:?}");
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
    cwd: NormalizedPath,
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

    if let Err(e) = conn
        .send(&zccache_protocol::Request::CompileEphemeral {
            client_pid: std::process::id(),
            working_dir: cwd.clone(),
            compiler: compiler.into(),
            args,
            cwd,
            env: Some(client_env),
        })
        .await
    {
        eprintln!("zccache: failed to send to daemon: {e}");
        return ExitCode::FAILURE;
    }

    let recv_result = match conn.recv().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("zccache: broken connection to daemon: {e}");
            return ExitCode::FAILURE;
        }
    };
    match recv_result {
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
        None => {
            eprintln!("zccache: lost connection to daemon (no response received)");
            ExitCode::FAILURE
        }
        Some(other) => {
            eprintln!("zccache: unexpected response from daemon: {other:?}");
            ExitCode::FAILURE
        }
    }
}

/// Ephemeral link/archive: single-roundtrip for `zccache ar ...` etc.
async fn cmd_link_ephemeral(
    endpoint: &str,
    tool: &Path,
    args: Vec<String>,
    cwd: NormalizedPath,
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

    if let Err(e) = conn
        .send(&zccache_protocol::Request::LinkEphemeral {
            client_pid: std::process::id(),
            tool: tool.into(),
            args,
            cwd,
            env: Some(client_env),
        })
        .await
    {
        eprintln!("zccache: failed to send to daemon: {e}");
        return ExitCode::FAILURE;
    }

    let recv_result = match conn.recv().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("zccache: broken connection to daemon: {e}");
            return ExitCode::FAILURE;
        }
    };
    match recv_result {
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
        None => {
            eprintln!("zccache: lost connection to daemon (no response received)");
            ExitCode::FAILURE
        }
        Some(other) => {
            eprintln!("zccache: unexpected response from daemon: {other:?}");
            ExitCode::FAILURE
        }
    }
}

// ─── Daemon auto-start ─────────────────────────────────────────────────────

enum VersionCheck {
    Ok,
    /// Daemon is newer than client — safe to proceed.
    DaemonNewer {
        daemon_ver: String,
    },
    /// Daemon is older than client — must restart.
    DaemonOlder {
        daemon_ver: String,
    },
    /// Could not connect to the daemon at all.
    Unreachable,
    /// Connected but could not complete the version exchange (protocol mismatch, etc.).
    CommError,
}

/// Connect to the daemon and compare its version to ours.
async fn check_daemon_version(endpoint: &str) -> VersionCheck {
    let mut conn = match connect(endpoint).await {
        Ok(c) => c,
        Err(_) => return VersionCheck::Unreachable,
    };
    if conn.send(&zccache_protocol::Request::Status).await.is_err() {
        return VersionCheck::CommError;
    }
    match conn.recv::<zccache_protocol::Response>().await {
        Ok(Some(zccache_protocol::Response::Status(s))) => {
            if s.version == zccache_core::VERSION {
                return VersionCheck::Ok;
            }
            let client_ver = zccache_core::version::current();
            match zccache_core::version::Version::parse(&s.version) {
                Some(daemon_ver) => match daemon_ver.cmp(&client_ver) {
                    std::cmp::Ordering::Equal => VersionCheck::Ok,
                    std::cmp::Ordering::Greater => VersionCheck::DaemonNewer {
                        daemon_ver: s.version,
                    },
                    std::cmp::Ordering::Less => VersionCheck::DaemonOlder {
                        daemon_ver: s.version,
                    },
                },
                // Unparseable daemon version → treat as older (safe default)
                None => VersionCheck::DaemonOlder {
                    daemon_ver: s.version,
                },
            }
        }
        _ => VersionCheck::CommError,
    }
}

/// Spawn a new daemon and wait for it to become ready.
async fn spawn_and_wait(endpoint: &str) -> Result<(), String> {
    let daemon_bin = find_daemon_binary().ok_or("cannot find zccache-daemon binary")?;
    tracing::debug!(?daemon_bin, %endpoint, "spawning daemon");
    spawn_daemon(&daemon_bin, endpoint)?;

    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if connect(endpoint).await.is_ok() {
            return Ok(());
        }
    }
    Err("daemon started but not accepting connections after 10s".to_string())
}

/// Ensure the daemon is running **and version-compatible**.
///
/// Version checking is asymmetric: a newer daemon is accepted (it's
/// backward-compatible), but an older daemon triggers a hard error
/// telling the user to run `zccache stop` first.
///
/// Handles concurrent calls gracefully: when multiple processes race to start
/// the daemon, only one wins the bind. The losers detect this and connect to
/// the winning daemon instead of failing.
async fn ensure_daemon(endpoint: &str) -> Result<(), String> {
    // Fast path: connect + version check
    match check_daemon_version(endpoint).await {
        VersionCheck::Ok => return Ok(()),
        VersionCheck::DaemonNewer { daemon_ver } => {
            tracing::debug!(
                daemon_ver,
                client_ver = zccache_core::VERSION,
                "daemon is newer than client, proceeding"
            );
            return Ok(());
        }
        VersionCheck::DaemonOlder { daemon_ver } => {
            return Err(format!(
                "daemon v{daemon_ver} is older than client v{}. \
                 Run `zccache stop` first.",
                zccache_core::VERSION,
            ));
        }
        VersionCheck::CommError => {
            return Err(
                "cannot communicate with daemon (possible protocol mismatch). \
                 Run `zccache stop` first."
                    .to_string(),
            );
        }
        VersionCheck::Unreachable => {
            // Fall through to lock-file check / spawn
        }
    }

    // Check lock file for a running daemon we just can't reach yet
    if let Some(pid) = zccache_ipc::check_running_daemon() {
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            match check_daemon_version(endpoint).await {
                VersionCheck::Ok => return Ok(()),
                VersionCheck::DaemonNewer { daemon_ver } => {
                    tracing::debug!(
                        daemon_ver,
                        client_ver = zccache_core::VERSION,
                        "daemon is newer than client, proceeding"
                    );
                    return Ok(());
                }
                VersionCheck::DaemonOlder { daemon_ver } => {
                    return Err(format!(
                        "daemon v{daemon_ver} is older than client v{}. \
                         Run `zccache stop` first.",
                        zccache_core::VERSION,
                    ));
                }
                VersionCheck::CommError => {
                    return Err(
                        "cannot communicate with daemon (possible protocol mismatch). \
                         Run `zccache stop` first."
                            .to_string(),
                    );
                }
                VersionCheck::Unreachable => continue,
            }
        }
        return Err(format!(
            "daemon process {pid} exists but not accepting connections"
        ));
    }

    // No daemon running — spawn one
    spawn_and_wait(endpoint).await
}

/// Find the daemon binary. Looks next to the CLI binary first, then on PATH.
fn find_daemon_binary() -> Option<NormalizedPath> {
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
                return Some(candidate.into());
            }
        }
    }

    // Fall back to PATH
    which_on_path(name)
}

/// Simple PATH lookup (no external crate needed).
/// On Windows, also tries appending `.exe` if the name has no extension.
fn which_on_path(name: &str) -> Option<NormalizedPath> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate.into());
        }
        // On Windows, try with .exe suffix
        #[cfg(windows)]
        if std::path::Path::new(name).extension().is_none() {
            let with_exe = dir.join(format!("{name}.exe"));
            if with_exe.is_file() {
                return Some(with_exe.into());
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
