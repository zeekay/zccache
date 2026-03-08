//! zccache CLI -- command-line interface for the compiler cache.
//!
//! Provides commands for interacting with the zccache daemon,
//! querying cache status, and wrapping compiler invocations.

use clap::{Parser, Subcommand};

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
    /// Wrap a compiler invocation.
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

fn main() {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    match cli.command {
        Commands::Start => {
            tracing::info!("starting daemon...");
            // TODO: Check if daemon is running, start if not.
            println!("zccache daemon start: not yet implemented");
        }
        Commands::Stop => {
            tracing::info!("stopping daemon...");
            // TODO: Send shutdown request to daemon.
            println!("zccache daemon stop: not yet implemented");
        }
        Commands::Status => {
            // TODO: Connect to daemon, request status.
            println!("zccache status: not yet implemented");
        }
        Commands::Clear => {
            // TODO: Connect to daemon or clear cache directly.
            println!("zccache clear: not yet implemented");
        }
        Commands::Wrap { args } => {
            tracing::debug!(?args, "wrapping compiler invocation");
            // TODO: Parse args, check cacheability, contact daemon.
            println!("zccache wrap: not yet implemented");
        }
        Commands::Inspect { key } => {
            // TODO: Look up cache entry.
            println!("zccache inspect {key}: not yet implemented");
        }
    }
}
