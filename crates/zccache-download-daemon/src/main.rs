#[cfg(unix)]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(windows)]
#[global_allocator]
static GLOBAL_WIN: mimalloc::MiMalloc = mimalloc::MiMalloc;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "zccache-download-daemon", version, about)]
struct Args {
    #[arg(long, default_value = "info")]
    log_level: String,

    #[arg(long)]
    foreground: bool,

    #[arg(long)]
    endpoint: Option<String>,
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
        .unwrap_or_else(zccache_download_client::default_endpoint);
    println!("zccache-download-daemon v{}", env!("CARGO_PKG_VERSION"));
    println!();
    println!("  endpoint:   {endpoint}");
    println!(
        "  lock file:  {}",
        zccache_download_client::lock_file_path().display()
    );
    let client = zccache_download_client::DownloadClient::new(Some(endpoint));
    match client.daemon_status() {
        Ok(status) => {
            println!("  status:     running");
            println!("  uptime:     {}s", status.uptime_secs);
            println!("  downloads:  {}", status.active_downloads);
            println!("  clients:    {}", status.connected_clients);
        }
        Err(_) => {
            println!("  status:     not running");
        }
    }
}

fn run_server(args: Args) {
    let endpoint = args
        .endpoint
        .unwrap_or_else(zccache_download_client::default_endpoint);
    let pid = std::process::id();
    if let Err(err) = zccache_download_client::write_lock_file(pid) {
        tracing::warn!("failed to write download daemon lock file: {err}");
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to create runtime");
    rt.block_on(async move {
        let mut server = match zccache_download_daemon::DownloadDaemon::bind(&endpoint) {
            Ok(server) => server,
            Err(err) => {
                eprintln!("failed to bind download daemon at {endpoint}: {err}");
                zccache_download_client::remove_lock_file();
                std::process::exit(1);
            }
        };
        let shutdown = server.shutdown_handle();
        tokio::spawn(async move {
            if let Ok(()) = tokio::signal::ctrl_c().await {
                shutdown.notify_one();
            }
        });
        if let Err(err) = server.run().await {
            eprintln!("download daemon error: {err}");
            zccache_download_client::remove_lock_file();
            std::process::exit(1);
        }
        zccache_download_client::remove_lock_file();
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
