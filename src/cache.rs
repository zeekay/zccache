use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// Return the configured cache directory, creating it if necessary.
pub fn cache_dir() -> Result<PathBuf> {
    let dir = if let Ok(d) = std::env::var("ZCCACHE_DIR") {
        PathBuf::from(d)
    } else {
        dirs::cache_dir()
            .context("Cannot determine user cache directory")?
            .join("zccache")
    };
    fs::create_dir_all(&dir).context("Failed to create cache directory")?;
    Ok(dir)
}

/// Returns the path to the cached object file for `key`, or `None` if not cached.
pub fn lookup(cache_dir: &Path, key: &str) -> Result<Option<PathBuf>> {
    let obj_path = object_path(cache_dir, key);
    if obj_path.exists() {
        Ok(Some(obj_path))
    } else {
        Ok(None)
    }
}

/// Returns the path where the dependency file for `key` would be cached.
pub fn dep_path(cache_dir: &Path, key: &str) -> PathBuf {
    let mut p = object_path(cache_dir, key);
    p.set_extension("d");
    p
}

fn object_path(cache_dir: &Path, key: &str) -> PathBuf {
    // Use first two characters of hash as directory prefix to avoid huge flat dirs.
    let prefix = &key[..2];
    cache_dir.join("objects").join(prefix).join(&key[2..])
}

/// Store the compiled object (and optional dep file) in the cache.
pub fn store(
    cache_dir: &Path,
    key: &str,
    obj_file: &Path,
    dep_file: Option<&Path>,
) -> Result<()> {
    let dest = object_path(cache_dir, key);
    let dest_dir = dest.parent().expect("object path always has a parent");
    fs::create_dir_all(dest_dir).context("Failed to create cache object directory")?;

    copy_or_link(obj_file, &dest).context("Failed to store object in cache")?;

    if let Some(dep_src) = dep_file
        && dep_src.exists()
    {
        let dep_dest = dep_path(cache_dir, key);
        copy_or_link(dep_src, &dep_dest).context("Failed to store dep file in cache")?;
    }

    Ok(())
}

/// Copy a cached object file to the target output path.
/// Tries a hard link first (zero-copy, same filesystem); falls back to a file copy.
pub fn restore(cached: &Path, dest: &Path) -> Result<()> {
    // Remove destination first so hard_link / copy doesn't fail on existing file.
    if dest.exists() {
        fs::remove_file(dest).context("Failed to remove existing output file")?;
    }
    copy_or_link(cached, dest).context("Failed to restore cached file")?;
    Ok(())
}

/// Clear all cached objects (but keep stats).
pub fn clear(cache_dir: &Path) -> Result<()> {
    let objects_dir = cache_dir.join("objects");
    if objects_dir.exists() {
        fs::remove_dir_all(&objects_dir).context("Failed to remove objects directory")?;
    }
    Ok(())
}

// ── helpers ─────────────────────────────────────────────────────────────────

/// Try a hard link first; fall back to regular copy on cross-device or unsupported FS.
fn copy_or_link(src: &Path, dst: &Path) -> Result<()> {
    // Attempt hard link first – O(1) and zero extra disk space.
    if fs::hard_link(src, dst).is_ok() {
        return Ok(());
    }
    // Fall back to regular copy.
    fs::copy(src, dst).context("Failed to copy file")?;
    Ok(())
}
