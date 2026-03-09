//! zccache daemon library.
//!
//! The daemon maintains in-memory caches, manages the artifact store,
//! runs the file watcher, and handles IPC requests from CLI/wrappers.

#![allow(clippy::missing_errors_doc)]

pub mod server;
pub mod stats;

pub use server::DaemonServer;
pub use stats::{PhaseProfiler, ProfileSnapshot, StatsCollector};
