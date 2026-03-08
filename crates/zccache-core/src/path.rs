//! Cross-platform path utilities.
//!
//! Handles path normalization, case sensitivity, and platform differences.

use std::path::{Path, PathBuf};

/// A normalized, platform-aware path representation.
///
/// On case-insensitive filesystems (Windows, default macOS), paths are
/// stored in a canonical form for consistent cache keying.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NormalizedPath {
    /// The original path, normalized but preserving original casing.
    path: PathBuf,
    /// Lowercased version for case-insensitive comparison, if applicable.
    case_key: Option<String>,
}

impl NormalizedPath {
    /// Create a new normalized path.
    ///
    /// On Windows, this also computes a lowercase key for case-insensitive matching.
    pub fn new(path: impl AsRef<Path>) -> Self {
        let path = normalize(path.as_ref());
        let case_key = if cfg!(windows) || cfg!(target_os = "macos") {
            path.to_str().map(|s| s.to_lowercase())
        } else {
            None
        };
        Self { path, case_key }
    }

    /// Returns the underlying path.
    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.path
    }

    /// Returns the case-insensitive comparison key, if applicable.
    #[must_use]
    pub fn case_key(&self) -> Option<&str> {
        self.case_key.as_deref()
    }
}

/// Normalize a path by resolving `.` and `..` components without
/// touching the filesystem (no symlink resolution).
///
/// This is intentionally not `canonicalize()` --- we avoid filesystem
/// access and symlink resolution for performance and determinism.
#[must_use]
pub fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if let Some(Component::Normal(_)) = components.last() {
                    components.pop();
                } else {
                    components.push(component);
                }
            }
            _ => components.push(component),
        }
    }
    components.iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_removes_dot() {
        let p = normalize(Path::new("a/./b/c"));
        assert_eq!(p, PathBuf::from("a/b/c"));
    }

    #[test]
    fn normalize_resolves_dotdot() {
        let p = normalize(Path::new("a/b/../c"));
        assert_eq!(p, PathBuf::from("a/c"));
    }
}
