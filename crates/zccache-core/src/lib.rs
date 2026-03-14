//! Core types and traits for zccache.
//!
//! This crate contains shared types, error definitions, path utilities,
//! and configuration structures used across all zccache crates.

pub mod config;
pub mod error;
pub mod path;

pub use error::{Error, Result};

/// The version string from Cargo.toml (workspace version).
///
/// This is the single source of truth for the version that `zccache --version`
/// prints (via clap's `#[command(version)]`). The test below ensures it stays
/// in sync with `pyproject.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    /// Ensures the workspace Cargo.toml version and pyproject.toml version
    /// are always in sync. A mismatch means `zccache --version` (from Cargo)
    /// would disagree with the PyPI package version — which is a release bug.
    #[test]
    fn cargo_and_pyproject_versions_match() {
        // Navigate from this crate's manifest dir to the workspace root.
        let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crates/ dir")
            .parent()
            .expect("workspace root");

        let pyproject = std::fs::read_to_string(workspace_root.join("pyproject.toml"))
            .expect("failed to read pyproject.toml");

        let pyproject_version = pyproject
            .lines()
            .find_map(|line| {
                let line = line.trim();
                if line.starts_with("version") {
                    // Parse: version = "1.0.4"
                    let (_, val) = line.split_once('=')?;
                    Some(val.trim().trim_matches('"').to_string())
                } else {
                    None
                }
            })
            .expect("pyproject.toml missing `version` field");

        assert_eq!(
            VERSION, pyproject_version,
            "\n\nVersion mismatch!\n\
             \n  Cargo.toml (workspace): {VERSION}\
             \n  pyproject.toml:         {pyproject_version}\
             \n\nThese must match. The Cargo workspace version is what `zccache --version`\n\
             prints, and the pyproject.toml version is what gets published to PyPI.\n\
             Update both files to the same version.\n"
        );
    }
}
