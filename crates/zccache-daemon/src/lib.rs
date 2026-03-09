//! zccache daemon library.
//!
//! The daemon maintains in-memory caches, manages the artifact store,
//! runs the file watcher, and handles IPC requests from CLI/wrappers.

#![allow(clippy::missing_errors_doc)]

pub mod server;

pub use server::DaemonServer;
