use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Cache statistics persisted to disk.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Stats {
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub cache_errors: u64,
}

fn stats_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join("stats.json")
}

pub fn load(cache_dir: &Path) -> Result<Stats> {
    let path = stats_path(cache_dir);
    if !path.exists() {
        return Ok(Stats::default());
    }
    let data = fs::read_to_string(&path).context("Failed to read stats file")?;
    serde_json::from_str(&data).context("Failed to parse stats file")
}

/// Atomically update stats by applying `f` to the current stats.
pub fn update<F>(cache_dir: &Path, f: F) -> Result<()>
where
    F: FnOnce(&mut Stats),
{
    // Ensure cache dir exists
    fs::create_dir_all(cache_dir).context("Failed to create cache directory")?;

    let mut stats = load(cache_dir).unwrap_or_default();
    f(&mut stats);

    let json = serde_json::to_string_pretty(&stats).context("Failed to serialize stats")?;

    // Write atomically via a temp file in the same directory.
    let tmp_path = cache_dir.join("stats.json.tmp");
    fs::write(&tmp_path, &json).context("Failed to write stats temp file")?;
    fs::rename(&tmp_path, stats_path(cache_dir)).context("Failed to rename stats file")?;

    Ok(())
}

pub fn record_hit(cache_dir: &Path) -> Result<()> {
    update(cache_dir, |s| s.cache_hits += 1)
}

pub fn record_miss(cache_dir: &Path) -> Result<()> {
    update(cache_dir, |s| s.cache_misses += 1)
}

pub fn record_error(cache_dir: &Path) -> Result<()> {
    update(cache_dir, |s| s.cache_errors += 1)
}

pub fn show(cache_dir: &Path) -> Result<()> {
    let stats = load(cache_dir).unwrap_or_default();
    let total = stats.cache_hits + stats.cache_misses;
    let hit_rate = if total > 0 {
        stats.cache_hits as f64 / total as f64 * 100.0
    } else {
        0.0
    };

    let cache_size = dir_size(cache_dir).unwrap_or(0);

    println!("zccache statistics");
    println!("------------------");
    println!("Cache hits:   {}", stats.cache_hits);
    println!("Cache misses: {}", stats.cache_misses);
    println!("Cache errors: {}", stats.cache_errors);
    println!("Hit rate:     {:.1}%", hit_rate);
    println!("Cache size:   {}", format_size(cache_size));
    println!("Cache dir:    {}", cache_dir.display());

    Ok(())
}

pub fn zero(cache_dir: &Path) -> Result<()> {
    fs::create_dir_all(cache_dir).context("Failed to create cache directory")?;
    update(cache_dir, |s| {
        s.cache_hits = 0;
        s.cache_misses = 0;
        s.cache_errors = 0;
    })
}

fn dir_size(path: &Path) -> Result<u64> {
    let mut total = 0u64;
    if !path.exists() {
        return Ok(0);
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if meta.is_file() {
            total += meta.len();
        } else if meta.is_dir() {
            // Errors from sub-directories are ignored so that a permissions
            // issue on one sub-dir doesn't prevent showing the rest of the stats.
            total += dir_size(&entry.path()).unwrap_or(0);
        }
    }
    Ok(total)
}

fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} bytes", bytes)
    }
}
