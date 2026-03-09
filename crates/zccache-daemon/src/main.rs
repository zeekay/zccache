//! zccache daemon process.
//!
//! The daemon maintains in-memory caches, manages the artifact store,
//! runs the file watcher, and handles IPC requests from CLI/wrappers.

use clap::Parser;

/// zccache daemon -- local compiler cache service.
#[derive(Debug, Parser)]
#[command(name = "zccache-daemon", version, about)]
struct Args {
    /// Path to configuration file.
    #[arg(long)]
    config: Option<std::path::PathBuf>,

    /// Log level (trace, debug, info, warn, error).
    #[arg(long, default_value = "info")]
    log_level: String,

    /// Run in foreground (don't daemonize).
    #[arg(long)]
    foreground: bool,

    /// IPC endpoint (default: platform-specific).
    #[arg(long)]
    endpoint: Option<String>,

    /// Idle timeout in seconds (0 = no timeout). Default: 3600.
    #[arg(long, default_value = "3600")]
    idle_timeout: u64,
}

fn main() {
    let args = Args::parse();

    if args.foreground {
        init_tracing(&args.log_level);
        run_server(args);
    } else {
        print_status(&args);
    }
}

fn print_status(args: &Args) {
    let endpoint = args
        .endpoint
        .clone()
        .unwrap_or_else(zccache_ipc::default_endpoint);

    println!("zccache-daemon v{}", env!("CARGO_PKG_VERSION"));
    println!();
    println!("  endpoint:   {endpoint}");
    println!("  lock file:  {}", zccache_ipc::lock_file_path().display());
    println!();

    // Try to connect and get status from a running daemon
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to create tokio runtime");

    match rt.block_on(query_daemon_status(&endpoint)) {
        Ok(status) => {
            println!("  status:     running");
            println!("  uptime:     {}s", status.uptime_secs);
            println!("  artifacts:  {}", status.artifact_count);
            println!("  cache size: {} bytes", status.cache_size_bytes);
            println!("  metadata:   {} entries", status.metadata_entries);
            println!(
                "  hits/miss:  {} / {}",
                status.cache_hits, status.cache_misses
            );
        }
        Err(_) => {
            println!("  status:     not running");
            println!();
            println!("Start with: zccache-daemon --foreground");
        }
    }
}

async fn query_daemon_status(
    endpoint: &str,
) -> Result<zccache_protocol::DaemonStatus, Box<dyn std::error::Error>> {
    let mut conn = zccache_ipc::connect(endpoint).await?;
    conn.send(&zccache_protocol::Request::Status).await?;
    let resp: Option<zccache_protocol::Response> = conn.recv().await?;
    match resp {
        Some(zccache_protocol::Response::Status(s)) => Ok(s),
        Some(other) => Err(format!("unexpected response: {other:?}").into()),
        None => Err("connection closed".into()),
    }
}

fn run_server(args: Args) {
    let endpoint = args.endpoint.unwrap_or_else(zccache_ipc::default_endpoint);
    let idle_timeout = args.idle_timeout;

    tracing::info!(%endpoint, idle_timeout, "zccache-daemon starting");

    // Write lock file so CLI can detect us
    let pid = std::process::id();
    if let Err(e) = zccache_ipc::write_lock_file(pid) {
        tracing::warn!("failed to write lock file: {e}");
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to create tokio runtime");

    rt.block_on(async {
        let mut server = match zccache_daemon::DaemonServer::bind(&endpoint) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("failed to bind {endpoint}: {e}");
                zccache_ipc::remove_lock_file();
                std::process::exit(1);
            }
        };

        // Wire up Ctrl+C to trigger graceful shutdown
        let shutdown = server.shutdown_handle();
        tokio::spawn(async move {
            if let Ok(()) = tokio::signal::ctrl_c().await {
                tracing::info!("received Ctrl+C — shutting down");
                shutdown.notify_one();
            }
        });

        tracing::info!(%endpoint, "listening for connections");

        if let Err(e) = server.run(idle_timeout).await {
            tracing::error!("server error: {e}");
            zccache_ipc::remove_lock_file();
            std::process::exit(1);
        }

        tracing::info!("daemon exiting cleanly");
        zccache_ipc::remove_lock_file();
    });
}

fn init_tracing(level: &str) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(true)
        .init();
}
