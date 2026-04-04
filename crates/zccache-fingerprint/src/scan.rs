use std::collections::HashSet;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::error::{FingerprintError, Result};

/// A discovered file with its absolute and relative paths.
#[derive(Debug)]
pub struct ScannedFile {
    /// Absolute path on disk.
    pub absolute: PathBuf,
    /// Path relative to scan root, with forward slashes for cross-platform determinism.
    pub relative: String,
}

/// Walk a directory tree, filter by extensions and excluded directory names,
/// and return a sorted list of files.
///
/// - `extensions`: file extensions to include (without dot, e.g. `["rs", "toml"]`).
///   Empty slice means include all files.
/// - `exclude_dirs`: directory names to skip (e.g. `[".git", "target"]`).
///
/// Results are sorted by relative path for deterministic hashing.
pub fn walk_files(
    root: &Path,
    extensions: &[&str],
    exclude_dirs: &[&str],
) -> Result<Vec<ScannedFile>> {
    let root = root.canonicalize().map_err(|e| FingerprintError::Scan {
        path: root.to_path_buf(),
        message: format!("cannot canonicalize root: {e}"),
    })?;

    // Owned set for the Send + Sync closure.
    let exclude_set: HashSet<String> = exclude_dirs.iter().map(|s| s.to_string()).collect();

    let mut files = Vec::new();

    let walker = jwalk::WalkDir::new(&root)
        .follow_links(false)
        .skip_hidden(false)
        .sort(true)
        .process_read_dir(move |_depth, _path, _state, children| {
            // Prune excluded directories so they are never descended into.
            children.retain(|entry| {
                if let Ok(ref e) = entry {
                    if e.file_type.is_dir() {
                        if let Some(name) = e.file_name.to_str() {
                            if exclude_set.contains(name) {
                                return false;
                            }
                        }
                    }
                }
                true
            });
        });

    for entry in walker {
        let entry = entry.map_err(|e| FingerprintError::Scan {
            path: root.clone(),
            message: format!("jwalk error: {e}"),
        })?;

        if !entry.file_type.is_file() {
            continue;
        }

        let abs = entry.path();

        // Filter by extension.
        if !extensions.is_empty() {
            let matches = abs
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| extensions.iter().any(|&e| e.eq_ignore_ascii_case(ext)));
            if !matches {
                continue;
            }
        }

        let rel = abs
            .strip_prefix(&root)
            .map_err(|_| FingerprintError::Scan {
                path: abs.clone(),
                message: "path is not under root".to_string(),
            })?;

        let relative = normalize_slashes(rel);

        files.push(ScannedFile {
            absolute: abs,
            relative,
        });
    }

    files.sort_by(|a, b| a.relative.cmp(&b.relative));
    Ok(files)
}

/// Normalize a relative path to forward slashes for cross-platform determinism.
fn normalize_slashes(rel: &Path) -> String {
    let mut result = String::with_capacity(rel.as_os_str().len());
    let mut first = true;
    for c in rel.components() {
        if !first {
            result.push('/');
        }
        first = false;
        result.push_str(&c.as_os_str().to_string_lossy());
    }
    result
}

fn build_globset(patterns: &[&str]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern).map_err(|e| FingerprintError::Scan {
            path: PathBuf::from(pattern),
            message: format!("invalid glob pattern: {e}"),
        })?;
        builder.add(glob);
    }
    builder.build().map_err(|e| FingerprintError::Scan {
        path: PathBuf::new(),
        message: format!("failed to compile glob set: {e}"),
    })
}

/// Extract directory-level patterns from exclude globs for short-circuiting.
/// E.g., `.git/**` → match directory `.git`; `target/**` → match directory `target`.
fn build_dir_exclude_set(exclude: &[&str]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in exclude {
        // If pattern ends with "/**" we can skip the directory entirely.
        if let Some(prefix) = pattern.strip_suffix("/**") {
            let glob = Glob::new(prefix).map_err(|e| FingerprintError::Scan {
                path: PathBuf::from(pattern),
                message: format!("invalid glob pattern: {e}"),
            })?;
            builder.add(glob);
        }
    }
    builder.build().map_err(|e| FingerprintError::Scan {
        path: PathBuf::new(),
        message: format!("failed to compile dir exclude set: {e}"),
    })
}

/// Walk a directory tree, selecting files by glob patterns.
///
/// - `include`: glob patterns for files to include (e.g. `["src/**/*.rs", "Cargo.toml"]`).
///   Empty slice means include all files (equivalent to `["**"]`).
/// - `exclude`: glob patterns for files/directories to exclude (e.g. `[".git/**", "target/**"]`).
///   Exclude takes priority over include.
///
/// Patterns are matched against the **relative path** from `root`, using forward slashes
/// on all platforms. Results are sorted by relative path for deterministic hashing.
pub fn walk_files_glob(
    root: &Path,
    include: &[&str],
    exclude: &[&str],
) -> Result<Vec<ScannedFile>> {
    let root = root.canonicalize().map_err(|e| FingerprintError::Scan {
        path: root.to_path_buf(),
        message: format!("cannot canonicalize root: {e}"),
    })?;

    let include_set = build_globset(include)?;
    let exclude_set = build_globset(exclude)?;
    let dir_exclude_set = build_dir_exclude_set(exclude)?;
    // Only run the expensive ancestor check when there are exclude patterns
    // that don't end with "/**" (and thus weren't caught by process_read_dir).
    let has_non_dir_excludes = exclude.iter().any(|p| !p.ends_with("/**"));

    let mut files = Vec::new();

    // Clone root for the Send + Sync closure.
    let prune_root = root.clone();
    let prune_dir_exclude_set = dir_exclude_set.clone();

    let walker = jwalk::WalkDir::new(&root)
        .follow_links(false)
        .skip_hidden(false)
        .sort(true)
        .process_read_dir(move |_depth, _path, _state, children| {
            if prune_dir_exclude_set.is_empty() {
                return;
            }
            // Prune directories matching dir_exclude_set.
            children.retain(|entry| {
                if let Ok(ref e) = entry {
                    if e.file_type.is_dir() {
                        let abs = e.path();
                        if let Ok(rel) = abs.strip_prefix(&prune_root) {
                            if rel.components().next().is_some() {
                                let rel_str = normalize_slashes(rel);
                                if prune_dir_exclude_set.is_match(&rel_str) {
                                    return false;
                                }
                            }
                        }
                    }
                }
                true
            });
        });

    for entry in walker {
        let entry = entry.map_err(|e| FingerprintError::Scan {
            path: root.clone(),
            message: format!("jwalk error: {e}"),
        })?;

        if !entry.file_type.is_file() {
            continue;
        }

        let abs = entry.path();
        let rel = abs
            .strip_prefix(&root)
            .map_err(|_| FingerprintError::Scan {
                path: abs.clone(),
                message: "path is not under root".to_string(),
            })?;

        let relative = normalize_slashes(rel);

        // Exclude check first (exclude wins).
        if !exclude_set.is_empty() && exclude_set.is_match(&relative) {
            continue;
        }

        // Include check (empty include = include all).
        if !include_set.is_empty() && !include_set.is_match(&relative) {
            continue;
        }

        // Ancestor exclude check — file inside excluded directory
        // (handles excludes that don't end with /** so process_read_dir didn't catch them).
        // Skip entirely when all excludes are /**-style (already handled above).
        if has_non_dir_excludes && !exclude_set.is_empty() {
            let in_excluded = rel
                .ancestors()
                .skip(1) // skip the file itself
                .any(|ancestor| {
                    if ancestor == Path::new("") {
                        return false;
                    }
                    let ancestor_str = normalize_slashes(ancestor);
                    exclude_set.is_match(&ancestor_str)
                });
            if in_excluded {
                continue;
            }
        }

        files.push(ScannedFile {
            absolute: abs,
            relative,
        });
    }

    files.sort_by(|a, b| a.relative.cmp(&b.relative));
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn create_file(dir: &Path, rel: &str, content: &str) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, content).unwrap();
    }

    #[test]
    fn empty_dir() {
        let dir = TempDir::new().unwrap();
        let files = walk_files(dir.path(), &[], &[]).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn filters_by_extension() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "a.rs", "rust");
        create_file(dir.path(), "b.txt", "text");
        create_file(dir.path(), "c.rs", "more rust");

        let files = walk_files(dir.path(), &["rs"], &[]).unwrap();
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|f| f.relative.ends_with(".rs")));
    }

    #[test]
    fn excludes_directories() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "src/a.rs", "ok");
        create_file(dir.path(), ".git/config", "nope");
        create_file(dir.path(), "target/debug/b.rs", "nope");

        let files = walk_files(dir.path(), &[], &[".git", "target"]).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].relative, "src/a.rs");
    }

    #[test]
    fn sorted_output() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "z.rs", "z");
        create_file(dir.path(), "a.rs", "a");
        create_file(dir.path(), "m.rs", "m");

        let files = walk_files(dir.path(), &[], &[]).unwrap();
        let rels: Vec<_> = files.iter().map(|f| f.relative.as_str()).collect();
        assert_eq!(rels, vec!["a.rs", "m.rs", "z.rs"]);
    }

    #[test]
    fn relative_paths_use_forward_slashes() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "src/nested/deep/file.rs", "content");

        let files = walk_files(dir.path(), &[], &[]).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].relative, "src/nested/deep/file.rs");
        assert!(!files[0].relative.contains('\\'));
    }

    #[test]
    fn nested_directories() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "a/b/c.rs", "abc");
        create_file(dir.path(), "a/d.rs", "ad");
        create_file(dir.path(), "e.rs", "e");

        let files = walk_files(dir.path(), &["rs"], &[]).unwrap();
        assert_eq!(files.len(), 3);
    }

    #[test]
    fn all_extensions_when_empty() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "a.rs", "r");
        create_file(dir.path(), "b.py", "p");
        create_file(dir.path(), "c.txt", "t");

        let files = walk_files(dir.path(), &[], &[]).unwrap();
        assert_eq!(files.len(), 3);
    }

    #[test]
    fn extension_filter_case_insensitive() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "a.RS", "rust");
        create_file(dir.path(), "b.Rs", "rust");

        let files = walk_files(dir.path(), &["rs"], &[]).unwrap();
        assert_eq!(files.len(), 2);
    }

    // ── Adversarial tests ─────────────────────────────────────────

    #[test]
    fn nonexistent_root_errors() {
        let dir = TempDir::new().unwrap();
        let bad = dir.path().join("does_not_exist");
        let result = walk_files(&bad, &[], &[]);
        assert!(result.is_err());
    }

    #[test]
    fn files_without_extension_included_when_no_filter() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "Makefile", "all:");
        create_file(dir.path(), "LICENSE", "MIT");

        let files = walk_files(dir.path(), &[], &[]).unwrap();
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn files_without_extension_excluded_when_filter_set() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "Makefile", "all:");
        create_file(dir.path(), "main.rs", "fn main() {}");

        let files = walk_files(dir.path(), &["rs"], &[]).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].relative, "main.rs");
    }

    #[test]
    fn file_named_like_excluded_dir_not_excluded() {
        // A file named "target.rs" should NOT be excluded when "target" is
        // in exclude_dirs — only directories named "target" are excluded.
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "target.rs", "not excluded");
        create_file(dir.path(), "target/debug/a.rs", "excluded");

        let files = walk_files(dir.path(), &[], &["target"]).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].relative, "target.rs");
    }

    #[test]
    fn deeply_nested_excluded_dir() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "a/b/c/.git/config", "nope");
        create_file(dir.path(), "a/b/c/ok.rs", "ok");

        let files = walk_files(dir.path(), &[], &[".git"]).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].relative, "a/b/c/ok.rs");
    }

    #[test]
    fn empty_file_included() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "empty.rs", "");

        let files = walk_files(dir.path(), &[], &[]).unwrap();
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn multiple_extension_filters() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "a.rs", "r");
        create_file(dir.path(), "b.toml", "t");
        create_file(dir.path(), "c.py", "p");
        create_file(dir.path(), "d.txt", "x");

        let files = walk_files(dir.path(), &["rs", "toml"], &[]).unwrap();
        assert_eq!(files.len(), 2);
        let rels: Vec<_> = files.iter().map(|f| f.relative.as_str()).collect();
        assert!(rels.contains(&"a.rs"));
        assert!(rels.contains(&"b.toml"));
    }

    #[test]
    fn absolute_paths_are_valid() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "a.rs", "content");

        let files = walk_files(dir.path(), &[], &[]).unwrap();
        assert!(files[0].absolute.is_absolute());
        assert!(files[0].absolute.exists());
    }

    #[test]
    fn many_files_sorted_correctly() {
        let dir = TempDir::new().unwrap();
        for i in (0..50).rev() {
            create_file(dir.path(), &format!("file_{i:03}.rs"), &format!("{i}"));
        }

        let files = walk_files(dir.path(), &[], &[]).unwrap();
        assert_eq!(files.len(), 50);
        for i in 0..49 {
            assert!(files[i].relative < files[i + 1].relative);
        }
    }

    #[test]
    fn dotfiles_not_excluded_unless_in_exclude_list() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), ".hidden", "secret");
        create_file(dir.path(), ".config/setting", "val");

        let files = walk_files(dir.path(), &[], &[]).unwrap();
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn mixed_exclude_and_extension_filter() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "src/a.rs", "ok");
        create_file(dir.path(), "src/b.py", "skip ext");
        create_file(dir.path(), "target/c.rs", "skip dir");

        let files = walk_files(dir.path(), &["rs"], &["target"]).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].relative, "src/a.rs");
    }

    // ── walk_files_glob tests ─────────────────────────────────────

    fn rels(files: &[ScannedFile]) -> Vec<&str> {
        files.iter().map(|f| f.relative.as_str()).collect()
    }

    #[test]
    fn glob_recursive_include() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "src/a.rs", "r");
        create_file(dir.path(), "src/nested/b.rs", "r");
        create_file(dir.path(), "src/c.py", "p");

        let files = walk_files_glob(dir.path(), &["**/*.rs"], &[]).unwrap();
        assert_eq!(files.len(), 2);
        assert!(rels(&files).contains(&"src/a.rs"));
        assert!(rels(&files).contains(&"src/nested/b.rs"));
    }

    #[test]
    fn glob_exact_filename() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "Cargo.toml", "[package]");
        create_file(dir.path(), "src/lib.rs", "");
        create_file(dir.path(), "README.md", "");

        let files = walk_files_glob(dir.path(), &["Cargo.toml"], &[]).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].relative, "Cargo.toml");
    }

    #[test]
    fn glob_directory_scoped() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "src/a.rs", "ok");
        create_file(dir.path(), "tests/b.rs", "skip");
        create_file(dir.path(), "lib/c.rs", "skip");

        let files = walk_files_glob(dir.path(), &["src/**/*.rs"], &[]).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].relative, "src/a.rs");
    }

    #[test]
    fn glob_multiple_include() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "src/a.rs", "r");
        create_file(dir.path(), "Cargo.toml", "t");
        create_file(dir.path(), "tests/b.rs", "r");

        let files = walk_files_glob(dir.path(), &["src/**", "Cargo.toml"], &[]).unwrap();
        assert_eq!(files.len(), 2);
        assert!(rels(&files).contains(&"Cargo.toml"));
        assert!(rels(&files).contains(&"src/a.rs"));
    }

    #[test]
    fn glob_brace_alternation() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "a.rs", "r");
        create_file(dir.path(), "b.toml", "t");
        create_file(dir.path(), "c.py", "p");

        let files = walk_files_glob(dir.path(), &["*.{rs,toml}"], &[]).unwrap();
        assert_eq!(files.len(), 2);
        assert!(rels(&files).contains(&"a.rs"));
        assert!(rels(&files).contains(&"b.toml"));
    }

    #[test]
    fn glob_exclude_overrides_include() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "src/a.rs", "ok");
        create_file(dir.path(), "tests/b.rs", "skip");

        let files = walk_files_glob(dir.path(), &["**/*.rs"], &["tests/**"]).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].relative, "src/a.rs");
    }

    #[test]
    fn glob_directory_short_circuit() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "src/a.rs", "ok");
        create_file(dir.path(), ".git/config", "skip");
        create_file(dir.path(), ".git/objects/ab/cd", "skip");

        let files = walk_files_glob(dir.path(), &[], &[".git/**"]).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].relative, "src/a.rs");
    }

    #[test]
    fn glob_empty_include_matches_all() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "a.rs", "r");
        create_file(dir.path(), "b.py", "p");

        let files = walk_files_glob(dir.path(), &[], &[]).unwrap();
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn glob_no_matches_returns_empty() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "a.rs", "r");

        let files = walk_files_glob(dir.path(), &["*.xyz"], &[]).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn glob_invalid_pattern_errors() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "a.rs", "r");

        let result = walk_files_glob(dir.path(), &["[invalid"], &[]);
        assert!(result.is_err());
    }

    #[test]
    fn glob_sorted_output() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "z.rs", "z");
        create_file(dir.path(), "a.rs", "a");

        let files = walk_files_glob(dir.path(), &["**/*.rs"], &[]).unwrap();
        assert_eq!(rels(&files), vec!["a.rs", "z.rs"]);
    }

    #[test]
    fn glob_forward_slash_normalization() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "src/nested/a.rs", "r");

        let files = walk_files_glob(dir.path(), &["**/*.rs"], &[]).unwrap();
        assert_eq!(files[0].relative, "src/nested/a.rs");
        assert!(!files[0].relative.contains('\\'));
    }

    #[test]
    fn glob_overlapping_patterns_no_dupes() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "src/a.rs", "r");

        let files = walk_files_glob(dir.path(), &["**/*.rs", "src/**"], &[]).unwrap();
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn glob_exclude_specific_file() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "Cargo.toml", "t");
        create_file(dir.path(), "Cargo.lock", "l");
        create_file(dir.path(), "src/lib.rs", "r");

        let files = walk_files_glob(dir.path(), &[], &["Cargo.lock"]).unwrap();
        assert_eq!(files.len(), 2);
        assert!(!rels(&files).contains(&"Cargo.lock"));
    }

    #[test]
    fn glob_dotfiles_included() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), ".hidden", "secret");
        create_file(dir.path(), "visible.rs", "ok");

        let files = walk_files_glob(dir.path(), &[], &[]).unwrap();
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn glob_nonexistent_root_errors() {
        let dir = TempDir::new().unwrap();
        let bad = dir.path().join("nope");
        let result = walk_files_glob(&bad, &[], &[]);
        assert!(result.is_err());
    }

    #[test]
    fn glob_parity_with_walk_files() {
        // walk_files(root, &["rs"], &[".git"]) should produce the same result
        // as walk_files_glob(root, &["**/*.rs"], &[".git/**"]).
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "src/a.rs", "r");
        create_file(dir.path(), "src/b.py", "p");
        create_file(dir.path(), ".git/config", "nope");
        create_file(dir.path(), "lib/c.rs", "r");

        let from_walk = walk_files(dir.path(), &["rs"], &[".git"]).unwrap();
        let from_glob = walk_files_glob(dir.path(), &["**/*.rs"], &[".git/**"]).unwrap();

        let walk_rels: Vec<_> = from_walk.iter().map(|f| &f.relative).collect();
        let glob_rels: Vec<_> = from_glob.iter().map(|f| &f.relative).collect();
        assert_eq!(walk_rels, glob_rels);
    }
}
