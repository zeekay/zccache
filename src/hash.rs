use anyhow::{Context, Result};
use blake3::Hasher;
use std::fs;
use std::process::Command;
use std::io::Read;

/// Hash the compiler binary itself (first 256 KB is enough to detect version changes)
/// and append the version string for robustness.
pub fn compiler_identity(compiler: &str) -> Result<Vec<u8>> {
    let mut hasher = Hasher::new();

    // Hash the compiler binary content (capped at 256 KB for speed).
    if let Ok(path) = which_compiler(compiler)
        && let Ok(file) = fs::File::open(&path)
    {
        let mut reader = file.take(256 * 1024);
        let mut buf = Vec::new();
        let _ = reader.read_to_end(&mut buf);
        hasher.update(&buf);
    } else if std::env::var("ZCCACHE_DEBUG").is_ok() {
        eprintln!(
            "zccache: warning: could not hash compiler binary for '{}'; \
             falling back to version string only",
            compiler
        );
    }

    // Also hash the version string so cross-version rebuilds are detected even
    // when the binary path is the same (e.g. system package update).
    let version = compiler_version_string(compiler).unwrap_or_default();
    hasher.update(version.as_bytes());

    Ok(hasher.finalize().as_bytes().to_vec())
}

fn compiler_version_string(compiler: &str) -> Result<String> {
    let output = Command::new(compiler)
        .arg("--version")
        .output()
        .context("Failed to run compiler --version")?;
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn which_compiler(compiler: &str) -> Result<String> {
    // If it's an absolute or relative path, use it directly.
    if compiler.contains('/') {
        return Ok(compiler.to_string());
    }
    // Otherwise find it on PATH.
    let output = Command::new("which")
        .arg(compiler)
        .output()
        .context("Failed to run which")?;
    let path = String::from_utf8_lossy(&output.stdout)
        .trim()
        .to_string();
    if path.is_empty() {
        anyhow::bail!("Compiler '{}' not found on PATH", compiler);
    }
    Ok(path)
}

/// Compute the final cache key from:
/// - compiler identity (binary hash + version)
/// - preprocessed source bytes
/// - sorted, deduplicated hash-relevant flags
pub fn compute_key(compiler_id: &[u8], preprocessed: &[u8], hash_args: &[String]) -> String {
    let mut hasher = Hasher::new();

    // Compiler identity
    hasher.update(&(compiler_id.len() as u64).to_le_bytes());
    hasher.update(compiler_id);

    // Preprocessed source
    hasher.update(&(preprocessed.len() as u64).to_le_bytes());
    hasher.update(preprocessed);

    // Flags (sorted for determinism regardless of argument order)
    let mut sorted_args = hash_args.to_vec();
    sorted_args.sort();
    for arg in &sorted_args {
        hasher.update(&(arg.len() as u64).to_le_bytes());
        hasher.update(arg.as_bytes());
    }

    hasher.finalize().to_hex().to_string()
}
