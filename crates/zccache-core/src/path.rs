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

/// Convert an MSYS2/Git Bash style path to a native Windows path.
///
/// `/c/Users/foo` → `C:\Users\foo`
///
/// On non-Windows platforms, returns the input unchanged.
/// On Windows, only converts paths matching the MSYS pattern `/<letter>/...`.
/// Already-native paths (e.g., `C:\...`) pass through unchanged.
#[must_use]
pub fn normalize_msys_path(path: &str) -> String {
    #[cfg(windows)]
    {
        let bytes = path.as_bytes();
        // Match pattern: /X/ or /X (end of string) where X is a-zA-Z
        if bytes.len() >= 2
            && bytes[0] == b'/'
            && bytes[1].is_ascii_alphabetic()
            && (bytes.len() == 2 || bytes[2] == b'/')
        {
            let drive = (bytes[1] as char).to_ascii_uppercase();
            let rest = if bytes.len() > 2 { &path[2..] } else { "" };
            return format!("{drive}:{rest}").replace('/', "\\");
        }
        path.to_string()
    }
    #[cfg(not(windows))]
    {
        path.to_string()
    }
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

    #[test]
    fn msys_path_drive_letter() {
        let result = normalize_msys_path("/c/Users/foo/bar");
        #[cfg(windows)]
        assert_eq!(result, r"C:\Users\foo\bar");
        #[cfg(not(windows))]
        assert_eq!(result, "/c/Users/foo/bar");
    }

    #[test]
    fn msys_path_uppercase_drive() {
        let result = normalize_msys_path("/D/project/build");
        #[cfg(windows)]
        assert_eq!(result, r"D:\project\build");
        #[cfg(not(windows))]
        assert_eq!(result, "/D/project/build");
    }

    #[test]
    fn msys_path_bare_drive() {
        let result = normalize_msys_path("/c");
        #[cfg(windows)]
        assert_eq!(result, "C:");
        #[cfg(not(windows))]
        assert_eq!(result, "/c");
    }

    #[test]
    fn native_windows_path_unchanged() {
        let result = normalize_msys_path(r"C:\Users\foo\bar");
        assert_eq!(result, r"C:\Users\foo\bar");
    }

    #[test]
    fn relative_path_unchanged() {
        let result = normalize_msys_path("relative/path");
        assert_eq!(result, "relative/path");
    }

    #[test]
    fn empty_path_unchanged() {
        let result = normalize_msys_path("");
        assert_eq!(result, "");
    }

    #[test]
    fn unix_absolute_path_not_drive() {
        // /usr/bin/gcc — bytes[2] is 's', not '/', so NOT a drive letter path
        let result = normalize_msys_path("/usr/bin/gcc");
        assert_eq!(result, "/usr/bin/gcc");
    }
}
