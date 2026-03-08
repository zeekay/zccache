//! Test utilities for zccache.
//!
//! Provides helpers for integration tests, including temp directories,
//! daemon lifecycle management, and test fixtures.

/// Create a temporary directory for test artifacts.
///
/// The directory and its contents are deleted when the returned
/// `TempDir` is dropped.
///
/// # Errors
///
/// Returns an error if the temp directory cannot be created.
pub fn temp_cache_dir() -> std::io::Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix("zccache-test-")
        .tempdir()
}

/// Initialize tracing for tests (only installs once).
pub fn init_test_tracing() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        tracing_subscriber::fmt()
            .with_env_filter("zccache=trace")
            .with_test_writer()
            .try_init()
            .ok();
    });
}
