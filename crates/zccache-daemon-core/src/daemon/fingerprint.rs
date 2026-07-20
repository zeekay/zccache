//! Daemon-side fingerprint manager.
//!
//! Tracks per-watch dirty state in memory. FS watcher events flow through
//! `on_batch()` to set watches dirty; CLI queries via IPC get sub-millisecond
//! answers from the in-memory state.

use crate::core::NormalizedPath;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use dashmap::DashMap;

/// Key identifying a unique fingerprint watch.
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub(crate) struct WatchKey {
    /// Canonicalized root directory being watched.
    pub root: NormalizedPath,
    /// Canonical path to the cache file.
    pub cache_file: NormalizedPath,
}

/// Per-file tracked entry within a watch.
#[derive(Debug, Clone)]
struct TrackedFile {
    mtime_ns: u64,
    size: u64,
    hash_hex: String,
}

/// Pre-computed metadata for a single changed path in an `on_batch` call.
///
/// Built once per watcher batch *outside* the watch-map lock so the per-watch
/// update loop never holds a DashMap shard lock across filesystem I/O (issue #724).
struct ChangedMeta {
    /// Canonicalized absolute path of the changed file.
    canon: NormalizedPath,
    mtime_ns: u64,
    size: u64,
    hash_hex: String,
}

/// Per-file verdict from an off-lock filesystem verify.
///
/// Built by [`FingerprintManager::verify_offlock`] without holding any DashMap
/// shard lock, then applied under a brief `get_mut` (issue #724).
enum FileVerdict {
    /// Content changed: refresh (mtime, size, hash) and mark the watch dirty.
    Changed {
        mtime_ns: u64,
        size: u64,
        hash_hex: String,
    },
    /// Smart touch: same content, only (mtime, size) moved — refresh those.
    Touched { mtime_ns: u64, size: u64 },
    /// File could not be read (removed / permission): treat as changed, but
    /// leave the tracked metadata untouched.
    Unreadable,
}

/// A tracked file whose on-disk `(mtime, size)` moved since the last scan,
/// paired with the hash it was tracked under (`snap_hash`) so the apply phase
/// can compare-and-swap and never clobber a concurrent `on_batch` write.
struct VerifiedFile {
    rel: String,
    snap_hash: String,
    verdict: FileVerdict,
}

/// State of a single fingerprint watch.
#[allow(dead_code)]
struct WatchState {
    /// Per-file state keyed by relative path (forward slashes).
    files: HashMap<String, TrackedFile>,
    /// Whether any file has changed since last mark-success.
    dirty: bool,
    /// Relative paths of files changed since last mark-success (Bug A fix).
    dirty_files: HashSet<String>,
    /// Monotonic counter bumped on each content-changing `on_batch` (Bug B fix).
    generation: u64,
    /// Generation at the time of the last `check` that returned "run".
    checked_generation: u64,
    /// "success", "pending", or "failure".
    status: String,
    /// Cache algorithm: "hash" or "two-layer".
    cache_type: String,
    /// Root directory (canonical).
    root: NormalizedPath,
}

/// Result of a fingerprint check.
pub(crate) struct FpCheckResult {
    /// "skip" or "run".
    pub decision: String,
    /// Reason string when decision is "run".
    pub reason: Option<String>,
    /// Changed file paths (relative).
    pub changed_files: Vec<String>,
}

/// Daemon-side fingerprint manager.
///
/// Holds in-memory state for all active fingerprint watches. The FS watcher
/// feeds events through `on_batch()`, and IPC queries get answers from
/// memory without touching the filesystem.
pub(crate) struct FingerprintManager {
    watches: DashMap<WatchKey, WatchState>,
}

/// Strip the `\\?\` extended-length prefix on Windows.
/// No-op on other platforms.
fn strip_win_prefix(path: NormalizedPath) -> NormalizedPath {
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        if let Some(stripped) = s.strip_prefix(r"\\?\") {
            return NormalizedPath::from(stripped);
        }
    }
    path
}

/// Canonicalize a path, stripping the `\\?\` prefix on Windows.
fn canon(path: &Path) -> NormalizedPath {
    match path.canonicalize() {
        Ok(c) => strip_win_prefix(c.into()),
        Err(_) => path.into(),
    }
}

/// Canonicalize a path that may not exist yet.
/// Tries full canonicalization first, then falls back to canonicalizing
/// the parent directory and joining the filename. Used for cache file
/// paths and for watcher event paths (removed files no longer exist).
fn canon_maybe_missing(path: &Path) -> NormalizedPath {
    if let Ok(c) = path.canonicalize() {
        return strip_win_prefix(c.into());
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
        if let Ok(cp) = parent.canonicalize() {
            return strip_win_prefix(cp.into()).join(name);
        }
    }
    path.into()
}

impl FingerprintManager {
    pub fn new() -> Self {
        Self {
            watches: DashMap::new(),
        }
    }

    /// Check whether files have changed for the given watch.
    ///
    /// If the watch exists and is clean (not dirty, status == success),
    /// returns Skip immediately (<1ms). Otherwise does the initial scan
    /// via the fingerprint library.
    pub fn check(
        &self,
        cache_file: &Path,
        cache_type: &str,
        root: &Path,
        extensions: &[String],
        include_globs: &[String],
        exclude: &[String],
    ) -> FpCheckResult {
        let canon_root = canon(root);
        let canon_cf = canon_maybe_missing(cache_file);
        let key = WatchKey {
            root: canon_root.clone(),
            cache_file: canon_cf,
        };

        // Fast path: existing watch — branch on dirty/status.
        if let Some(watch) = self.watches.get(&key) {
            let dirty = watch.dirty;
            let status = watch.status.clone();
            let changed_snapshot: Vec<String> = watch.dirty_files.iter().cloned().collect();
            let gen = watch.generation;
            drop(watch);

            if !dirty && status == "success" {
                // Verify against the filesystem to catch missed watcher events.
                //
                // The re-stat + re-hash I/O runs OUTSIDE any DashMap shard lock
                // (issue #724 class). The prior implementation held a `get_mut`
                // write-shard lock across `verify_filesystem`, which re-stats
                // every tracked file and re-hashes each whose (mtime, size)
                // moved — full-file reads under the lock. A large build
                // (hundreds of tracked headers per watch) held the shard for the
                // whole sweep, and `on_batch` / `mark_success` / `mark_failure`
                // all take `iter_mut()` over every shard, so they stalled behind
                // it; under heavy parallel cargo that starved the RPC handlers
                // past the client timeout and wedged the daemon.
                //
                // Three phases: snapshot the tracked list under a brief read
                // lock, do the stat/hash I/O lock-free, then re-acquire briefly
                // to apply. A compare-and-swap on the pre-I/O hash keeps a
                // concurrent `on_batch` write from being clobbered by our
                // now-stale metadata, and a generation/dirty recheck ensures a
                // change recorded while we hashed is never reported as "skip".
                let snapshot = self.watches.get(&key).map(|w| {
                    let files: Vec<(String, u64, u64, String)> = w
                        .files
                        .iter()
                        .map(|(rel, t)| (rel.clone(), t.mtime_ns, t.size, t.hash_hex.clone()))
                        .collect();
                    (w.root.clone(), w.generation, files)
                });
                if let Some((root, gen_at_snapshot, snap_files)) = snapshot {
                    let verified = Self::verify_offlock(&root, &snap_files);
                    let changed: Vec<String> = verified
                        .iter()
                        .filter(|v| !matches!(v.verdict, FileVerdict::Touched { .. }))
                        .map(|v| v.rel.clone())
                        .collect();

                    if let Some(mut w) = self.watches.get_mut(&key) {
                        // Apply refreshed metadata, but only where the tracked
                        // hash still matches what we hashed against — otherwise a
                        // concurrent `on_batch` already recorded a newer state
                        // and must win.
                        for v in &verified {
                            let Some(tr) = w.files.get_mut(&v.rel) else {
                                continue;
                            };
                            if tr.hash_hex != v.snap_hash {
                                continue;
                            }
                            match &v.verdict {
                                FileVerdict::Changed {
                                    mtime_ns,
                                    size,
                                    hash_hex,
                                } => {
                                    tr.mtime_ns = *mtime_ns;
                                    tr.size = *size;
                                    tr.hash_hex = hash_hex.clone();
                                }
                                FileVerdict::Touched { mtime_ns, size } => {
                                    tr.mtime_ns = *mtime_ns;
                                    tr.size = *size;
                                }
                                FileVerdict::Unreadable => {}
                            }
                        }

                        // A change recorded by `on_batch` (or another verifying
                        // check) while we hashed off-lock must not be masked.
                        let advanced = w.dirty || w.generation != gen_at_snapshot;
                        if changed.is_empty() && !advanced {
                            tracing::debug!("fingerprint check: skip (verified, not dirty)");
                            return FpCheckResult {
                                decision: "skip".into(),
                                reason: None,
                                changed_files: vec![],
                            };
                        }

                        if !changed.is_empty() {
                            w.generation += 1;
                            w.dirty = true;
                            for f in &changed {
                                w.dirty_files.insert(f.clone());
                            }
                        }
                        w.status = "pending".into();
                        w.checked_generation = w.generation;
                        let changed_files: Vec<String> = if changed.is_empty() {
                            w.dirty_files.iter().cloned().collect()
                        } else {
                            changed
                        };
                        tracing::debug!("fingerprint check: run (verified, content changed)");
                        return FpCheckResult {
                            decision: "run".into(),
                            reason: Some("content changed".into()),
                            changed_files,
                        };
                    }
                    // Watch vanished between snapshot and re-acquire — fall through.
                }
                // Watch disappeared — fall through to rescan.
            } else if dirty {
                // Bug A fix: collect the actual dirty file paths.
                // Mark as pending and snapshot the generation (Bug B fix).
                if let Some(mut w) = self.watches.get_mut(&key) {
                    w.status = "pending".into();
                    w.checked_generation = gen;
                }
                tracing::debug!("fingerprint check: run (dirty)");
                return FpCheckResult {
                    decision: "run".into(),
                    reason: Some("content changed".into()),
                    changed_files: changed_snapshot,
                };
            } else if status == "failure" {
                if let Some(mut w) = self.watches.get_mut(&key) {
                    w.status = "pending".into();
                    w.checked_generation = gen;
                }
                tracing::debug!("fingerprint check: run (previous failure)");
                return FpCheckResult {
                    decision: "run".into(),
                    reason: Some("previous failure".into()),
                    changed_files: vec![],
                };
            } else {
                // Bug C fix: status is "pending" (initial scan done, not yet marked).
                // Return "run" without doing a wasteful full rescan.
                tracing::debug!("fingerprint check: run (pending)");
                return FpCheckResult {
                    decision: "run".into(),
                    reason: Some("pending".into()),
                    changed_files: vec![],
                };
            }
        }

        // No existing watch — do initial scan.
        tracing::debug!(
            "fingerprint check: initial scan for {}",
            canon_root.display()
        );
        let files = Self::scan_files(&canon_root, extensions, include_globs, exclude);
        let files = match files {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("fingerprint scan failed: {e}");
                return FpCheckResult {
                    decision: "run".into(),
                    reason: Some(format!("scan error: {e}")),
                    changed_files: vec![],
                };
            }
        };

        // Hash all files and build tracked state.
        let mut tracked = HashMap::new();
        for file in &files {
            let mtime = crate::fingerprint::persist::mtime_ns(&file.absolute).unwrap_or(0);
            let size = crate::fingerprint::persist::file_size(&file.absolute).unwrap_or(0);
            let hash_hex = match crate::hash::hash_file(&file.absolute) {
                Ok(h) => h.to_hex(),
                Err(_) => String::new(),
            };
            tracked.insert(
                file.relative.clone(),
                TrackedFile {
                    mtime_ns: mtime,
                    size,
                    hash_hex,
                },
            );
        }

        let watch = WatchState {
            files: tracked,
            dirty: false,
            dirty_files: HashSet::new(),
            generation: 0,
            checked_generation: 0,
            status: "pending".into(),
            cache_type: cache_type.to_string(),
            root: canon_root,
        };
        self.watches.insert(key, watch);

        FpCheckResult {
            decision: "run".into(),
            reason: Some("no cache file".into()),
            changed_files: vec![],
        }
    }

    /// Mark the watch as successful.
    ///
    /// Bug B fix: only clears dirty if no new events arrived since the last
    /// check (generation == checked_generation). If on_batch bumped the
    /// generation between check and mark_success, dirty stays set.
    pub fn mark_success(&self, cache_file: &Path) {
        let canon_cf = canon_maybe_missing(cache_file);
        for mut entry in self.watches.iter_mut() {
            if entry.key().cache_file == canon_cf {
                let w = entry.value_mut();
                if w.generation == w.checked_generation {
                    w.dirty = false;
                    w.dirty_files.clear();
                }
                w.status = "success".into();
                tracing::debug!("fingerprint mark-success: {}", cache_file.display());
                return;
            }
        }
        tracing::debug!(
            "fingerprint mark-success: no watch for {}",
            cache_file.display()
        );
    }

    /// Mark the watch as failed.
    pub fn mark_failure(&self, cache_file: &Path) {
        let canon_cf = canon_maybe_missing(cache_file);
        for mut entry in self.watches.iter_mut() {
            if entry.key().cache_file == canon_cf {
                entry.value_mut().status = "failure".into();
                tracing::debug!("fingerprint mark-failure: {}", cache_file.display());
                return;
            }
        }
    }

    /// Invalidate (remove) a watch entirely.
    pub fn invalidate(&self, cache_file: &Path) {
        let canon_cf = canon_maybe_missing(cache_file);
        self.watches.retain(|key, _| key.cache_file != canon_cf);
        tracing::debug!("fingerprint invalidate: {}", cache_file.display());
    }

    /// Called by the watcher consumer when files change on disk.
    ///
    /// For each changed/removed path, checks all watches whose root contains
    /// the path, marks them dirty, and re-hashes only the affected file.
    pub fn on_batch(&self, changed: &[NormalizedPath], removed: &[NormalizedPath]) {
        if changed.is_empty() && removed.is_empty() {
            return;
        }

        // Pre-compute canonical path + stat + content hash for each changed path
        // ONCE, before touching the watch map. The previous implementation did this
        // canonicalize()/hash_file() I/O *inside* `watches.iter_mut()`, holding a
        // DashMap write-shard lock across blocking filesystem reads and repeating
        // the same hash once per watch. Large ESP32/LPC builds register hundreds of
        // watch roots, so a single watcher batch starved every RPC handler waiting
        // on those shards and wedged the daemon (issue #724). Hoisting the I/O out
        // of the lock keeps shard hold-times to in-memory map updates only, and
        // hashes each changed file exactly once instead of once per watch.
        let changed_meta: Vec<ChangedMeta> = changed
            .iter()
            .map(|path| {
                let canon_path = canon(path);
                let mtime_ns = crate::fingerprint::persist::mtime_ns(&canon_path).unwrap_or(0);
                let size = crate::fingerprint::persist::file_size(&canon_path).unwrap_or(0);
                let hash_hex = match crate::hash::hash_file(&canon_path) {
                    Ok(h) => h.to_hex(),
                    Err(_) => String::new(),
                };
                ChangedMeta {
                    canon: canon_path,
                    mtime_ns,
                    size,
                    hash_hex,
                }
            })
            .collect();
        let removed_canon: Vec<NormalizedPath> = removed
            .iter()
            .map(|path| canon_maybe_missing(path))
            .collect();

        for mut entry in self.watches.iter_mut() {
            let watch = entry.value_mut();
            let root = &watch.root;

            for cm in &changed_meta {
                if let Ok(rel) = cm.canon.strip_prefix(root) {
                    let rel_str = rel.to_string_lossy().replace('\\', "/");

                    // Check if content actually changed (using the pre-hashed value).
                    let content_changed = match watch.files.get(&rel_str) {
                        Some(existing) => existing.hash_hex != cm.hash_hex,
                        None => true, // new file
                    };

                    if content_changed {
                        watch.dirty = true;
                        watch.dirty_files.insert(rel_str.clone());
                        watch.generation += 1;
                        watch.files.insert(
                            rel_str,
                            TrackedFile {
                                mtime_ns: cm.mtime_ns,
                                size: cm.size,
                                hash_hex: cm.hash_hex.clone(),
                            },
                        );
                    } else if let Some(tracked) = watch.files.get_mut(&rel_str) {
                        // Just update mtime/size, content unchanged (smart touch).
                        tracked.mtime_ns = cm.mtime_ns;
                        tracked.size = cm.size;
                    }
                }
            }

            for canon_path in &removed_canon {
                if let Ok(rel) = canon_path.strip_prefix(root) {
                    let rel_str = rel.to_string_lossy().replace('\\', "/");
                    if watch.files.remove(&rel_str).is_some() {
                        watch.dirty = true;
                        watch.dirty_files.insert(rel_str);
                        watch.generation += 1;
                    }
                }
            }
        }
    }

    /// Scan files using the fingerprint library's walk functions.
    fn scan_files(
        root: &Path,
        extensions: &[String],
        include_globs: &[String],
        exclude: &[String],
    ) -> std::result::Result<
        Vec<crate::fingerprint::ScannedFile>,
        crate::fingerprint::FingerprintError,
    > {
        if !include_globs.is_empty() {
            let include_refs: Vec<&str> = include_globs.iter().map(|s| s.as_str()).collect();
            let exclude_refs: Vec<&str> = exclude.iter().map(|s| s.as_str()).collect();
            crate::fingerprint::walk_files_glob(root, &include_refs, &exclude_refs)
        } else {
            let ext_refs: Vec<&str> = extensions.iter().map(|s| s.as_str()).collect();
            let exclude_refs: Vec<&str> = exclude.iter().map(|s| s.as_str()).collect();
            crate::fingerprint::walk_files(root, &ext_refs, &exclude_refs)
        }
    }

    /// Re-stat a snapshot of tracked files and classify each whose on-disk
    /// `(mtime, size)` moved. Runs with NO DashMap shard lock held: the caller
    /// snapshots the tracked list under a brief read lock, calls this, then
    /// applies the result under a brief write lock (issue #724).
    ///
    /// Files whose `(mtime, size)` still match are omitted (Layer 1 fast skip —
    /// one stat, no hashing). The rest are re-hashed (Layer 2) and returned as
    /// `Changed` (content differs), `Touched` (same content, mtime moved), or
    /// `Unreadable`. Each carries the hash it was tracked under so the caller
    /// can compare-and-swap before overwriting.
    fn verify_offlock(
        root: &NormalizedPath,
        snapshot: &[(String, u64, u64, String)],
    ) -> Vec<VerifiedFile> {
        let mut out = Vec::new();
        for (rel, mtime_ns, size, hash_hex) in snapshot {
            let abs = root.join(rel);
            let mtime = crate::fingerprint::persist::mtime_ns(&abs).unwrap_or(0);
            let sz = crate::fingerprint::persist::file_size(&abs).unwrap_or(0);
            if mtime == *mtime_ns && sz == *size {
                continue; // Layer 1: unchanged, no hashing.
            }
            // Layer 2: mtime/size moved — re-hash to confirm a real change.
            let verdict = match crate::hash::hash_file(&abs) {
                Ok(h) => {
                    let new_hash = h.to_hex();
                    if new_hash == *hash_hex {
                        FileVerdict::Touched {
                            mtime_ns: mtime,
                            size: sz,
                        }
                    } else {
                        FileVerdict::Changed {
                            mtime_ns: mtime,
                            size: sz,
                            hash_hex: new_hash,
                        }
                    }
                }
                Err(_) => FileVerdict::Unreadable,
            };
            out.push(VerifiedFile {
                rel: rel.clone(),
                snap_hash: hash_hex.clone(),
                verdict,
            });
        }
        out
    }

    /// Number of active watches (for status/diagnostics).
    #[allow(dead_code)]
    pub fn watch_count(&self) -> usize {
        self.watches.len()
    }
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
    fn first_check_returns_run() {
        let src = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        create_file(src.path(), "a.rs", "fn main() {}");

        let mgr = FingerprintManager::new();
        let result = mgr.check(
            &cache_dir.path().join("fp.json"),
            "two-layer",
            src.path(),
            &[],
            &[],
            &[],
        );
        assert_eq!(result.decision, "run");
        assert_eq!(result.reason.as_deref(), Some("no cache file"));
    }

    #[test]
    fn check_then_mark_success_then_check_returns_skip() {
        let src = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        create_file(src.path(), "a.rs", "fn main() {}");

        let cache_file = cache_dir.path().join("fp.json");
        let mgr = FingerprintManager::new();

        // First check: run (no cache).
        let result = mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
        assert_eq!(result.decision, "run");

        // Mark success.
        mgr.mark_success(&cache_file);

        // Second check: skip (clean).
        let result = mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
        assert_eq!(result.decision, "skip");
    }

    #[test]
    fn on_batch_changed_sets_dirty() {
        let src = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        create_file(src.path(), "a.rs", "original");

        let cache_file = cache_dir.path().join("fp.json");
        let mgr = FingerprintManager::new();

        mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
        mgr.mark_success(&cache_file);

        // Modify the file on disk.
        std::thread::sleep(std::time::Duration::from_millis(50));
        create_file(src.path(), "a.rs", "modified");

        // Simulate watcher event.
        mgr.on_batch(&[src.path().join("a.rs").into()], &[]);

        // Check should return run (dirty).
        let result = mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
        assert_eq!(result.decision, "run");
        assert_eq!(result.reason.as_deref(), Some("content changed"));
    }

    #[test]
    fn on_batch_removed_sets_dirty() {
        let src = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        create_file(src.path(), "a.rs", "content");
        create_file(src.path(), "b.rs", "content2");

        let cache_file = cache_dir.path().join("fp.json");
        let mgr = FingerprintManager::new();

        mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
        mgr.mark_success(&cache_file);

        // Simulate watcher event for removed file.
        mgr.on_batch(&[], &[src.path().join("b.rs").into()]);

        let result = mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
        assert_eq!(result.decision, "run");
    }

    #[test]
    fn smart_touch_does_not_set_dirty() {
        let src = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        create_file(src.path(), "a.rs", "stable");

        let cache_file = cache_dir.path().join("fp.json");
        let mgr = FingerprintManager::new();

        mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
        mgr.mark_success(&cache_file);

        // "Touch" the file (rewrite same content).
        std::thread::sleep(std::time::Duration::from_millis(50));
        create_file(src.path(), "a.rs", "stable");

        // Simulate watcher event — content is the same, so dirty should NOT be set.
        mgr.on_batch(&[src.path().join("a.rs").into()], &[]);

        let result = mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
        assert_eq!(result.decision, "skip");
    }

    #[test]
    fn mark_failure_forces_rerun() {
        let src = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        create_file(src.path(), "a.rs", "content");

        let cache_file = cache_dir.path().join("fp.json");
        let mgr = FingerprintManager::new();

        mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
        mgr.mark_failure(&cache_file);

        let result = mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
        assert_eq!(result.decision, "run");
        assert_eq!(result.reason.as_deref(), Some("previous failure"));
    }

    #[test]
    fn invalidate_removes_watch() {
        let src = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        create_file(src.path(), "a.rs", "content");

        let cache_file = cache_dir.path().join("fp.json");
        let mgr = FingerprintManager::new();

        mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
        mgr.mark_success(&cache_file);
        assert_eq!(mgr.watch_count(), 1);

        mgr.invalidate(&cache_file);
        assert_eq!(mgr.watch_count(), 0);

        // Next check should do a fresh scan.
        let result = mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
        assert_eq!(result.decision, "run");
        assert_eq!(result.reason.as_deref(), Some("no cache file"));
    }

    #[test]
    fn unrelated_watcher_event_ignored() {
        let src = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        create_file(src.path(), "a.rs", "content");

        let cache_file = cache_dir.path().join("fp.json");
        let mgr = FingerprintManager::new();

        mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
        mgr.mark_success(&cache_file);

        // Event for a file outside the watched root.
        mgr.on_batch(&[NormalizedPath::from("/some/other/path.rs")], &[]);

        let result = mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
        assert_eq!(result.decision, "skip");
    }

    // ── Bug regression tests ──────────────────────────────────

    #[test]
    fn bug_a_changed_files_reported_when_dirty() {
        // Bug A: changed_files was always empty even when on_batch detected changes.
        let src = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        create_file(src.path(), "a.rs", "original");
        create_file(src.path(), "b.rs", "stable");

        let cache_file = cache_dir.path().join("fp.json");
        let mgr = FingerprintManager::new();

        mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
        mgr.mark_success(&cache_file);

        // Modify only a.rs.
        std::thread::sleep(std::time::Duration::from_millis(50));
        create_file(src.path(), "a.rs", "modified");
        mgr.on_batch(&[src.path().join("a.rs").into()], &[]);

        let result = mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
        assert_eq!(result.decision, "run");
        // Bug: changed_files was always empty.
        assert!(
            !result.changed_files.is_empty(),
            "changed_files must report which files changed, got empty"
        );
        assert!(
            result.changed_files.iter().any(|f| f.contains("a.rs")),
            "changed_files should contain a.rs, got {:?}",
            result.changed_files
        );
    }

    #[test]
    fn bug_b_mark_success_does_not_swallow_concurrent_events() {
        // Bug B: on_batch between check and mark_success was silently lost.
        let src = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        create_file(src.path(), "a.rs", "v1");

        let cache_file = cache_dir.path().join("fp.json");
        let mgr = FingerprintManager::new();

        mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
        mgr.mark_success(&cache_file);

        // File changes → dirty.
        std::thread::sleep(std::time::Duration::from_millis(50));
        create_file(src.path(), "a.rs", "v2");
        mgr.on_batch(&[src.path().join("a.rs").into()], &[]);

        // check returns "run" (dirty).
        let result = mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
        assert_eq!(result.decision, "run");

        // ANOTHER file changes between check and mark_success.
        std::thread::sleep(std::time::Duration::from_millis(50));
        create_file(src.path(), "a.rs", "v3");
        mgr.on_batch(&[src.path().join("a.rs").into()], &[]);

        // User marks the operation as successful (based on v2 state).
        mgr.mark_success(&cache_file);

        // Bug: mark_success cleared dirty unconditionally, so the v3 change is lost.
        // The next check MUST return "run" because v3 arrived after the check.
        let result = mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
        assert_eq!(
            result.decision, "run",
            "events arriving between check and mark_success must not be lost"
        );
    }

    #[test]
    fn bug_c_pending_status_does_not_rescan() {
        // Bug C: after initial check (status=pending), a second check without
        // mark_success fell through to a full rescan returning "no cache file".
        let src = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        create_file(src.path(), "a.rs", "content");

        let cache_file = cache_dir.path().join("fp.json");
        let mgr = FingerprintManager::new();

        // Initial check → "run" (no cache file).
        let result = mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
        assert_eq!(result.decision, "run");
        assert_eq!(result.reason.as_deref(), Some("no cache file"));

        // Second check without marking → should still say "run" but NOT "no cache file".
        let result = mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
        assert_eq!(result.decision, "run");
        assert_ne!(
            result.reason.as_deref(),
            Some("no cache file"),
            "pending watch should not trigger a full rescan"
        );
    }

    #[test]
    fn bug_d_non_canonical_root_breaks_on_batch() {
        // Bug D: on_batch receives absolute paths from watcher, but root can be
        // non-canonical (e.g. "." or "path/sub/.."). strip_prefix fails silently,
        // so watcher events are never matched and the watch never becomes dirty.
        let src = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        fs::create_dir(src.path().join("sub")).unwrap();
        create_file(src.path(), "a.rs", "original");

        let cache_file = cache_dir.path().join("fp.json");
        let mgr = FingerprintManager::new();

        // Use non-canonical root: /tmp/xxx/sub/.. ≡ /tmp/xxx/
        let non_canonical_root = src.path().join("sub").join("..");
        mgr.check(&cache_file, "two-layer", &non_canonical_root, &[], &[], &[]);
        mgr.mark_success(&cache_file);

        // Modify file on disk.
        std::thread::sleep(std::time::Duration::from_millis(50));
        create_file(src.path(), "a.rs", "modified");

        // Watcher events use canonical paths (\\?\ stripped on Windows).
        let canonical_root = canon(src.path());
        mgr.on_batch(&[canonical_root.join("a.rs")], &[]);

        // Without fix: returns "skip" because on_batch couldn't strip the prefix.
        let result = mgr.check(&cache_file, "two-layer", &non_canonical_root, &[], &[], &[]);
        assert_eq!(
            result.decision, "run",
            "on_batch with canonical paths must work even when root was non-canonical"
        );
    }

    #[test]
    fn bug_e_non_canonical_cache_file_breaks_mark_success() {
        // Bug E: mark_success/mark_failure/invalidate compare cache_file by path
        // equality. If check() and mark_success receive different representations
        // of the same path, they won't match.
        let src = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        fs::create_dir(cache_dir.path().join("sub")).unwrap();
        create_file(src.path(), "a.rs", "content");

        let mgr = FingerprintManager::new();

        // check() with non-canonical cache_file.
        let non_canonical_cache = cache_dir.path().join("sub").join("..").join("fp.json");
        mgr.check(&non_canonical_cache, "two-layer", src.path(), &[], &[], &[]);

        // mark_success() with canonical cache_file.
        let canonical_cache = canon(cache_dir.path()).join("fp.json");
        mgr.mark_success(&canonical_cache);

        // Without fix: mark_success couldn't find the watch, so status is still "pending".
        let result = mgr.check(&non_canonical_cache, "two-layer", src.path(), &[], &[], &[]);
        assert_eq!(
            result.decision, "skip",
            "mark_success with canonical path must match watch created with non-canonical path"
        );
    }

    #[test]
    fn verify_catches_missed_watcher_events() {
        // Regression test for BUGS.md: daemon fp check misses in-place edits
        // when watcher events are not delivered (no on_batch call).
        let src = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        create_file(src.path(), "a.rs", "original");

        let cache_file = cache_dir.path().join("fp.json");
        let mgr = FingerprintManager::new();

        mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
        mgr.mark_success(&cache_file);

        // Modify file WITHOUT calling on_batch (simulates missed watcher event).
        std::thread::sleep(std::time::Duration::from_millis(50));
        create_file(src.path(), "a.rs", "modified");

        // Must detect the change via filesystem verification.
        let result = mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
        assert_eq!(
            result.decision, "run",
            "must detect change without on_batch"
        );
        assert!(
            result.changed_files.iter().any(|f| f.contains("a.rs")),
            "changed_files should contain a.rs, got {:?}",
            result.changed_files
        );
    }

    #[test]
    fn verify_smart_touch_still_skips() {
        // Smart touch (same content, new mtime) should still return "skip".
        let src = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        create_file(src.path(), "a.rs", "stable");

        let cache_file = cache_dir.path().join("fp.json");
        let mgr = FingerprintManager::new();

        mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
        mgr.mark_success(&cache_file);

        // Rewrite same content (mtime changes, content doesn't).
        std::thread::sleep(std::time::Duration::from_millis(50));
        create_file(src.path(), "a.rs", "stable");

        // Filesystem verify should detect mtime change, re-hash, find same content → skip.
        let result = mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
        assert_eq!(result.decision, "skip", "smart touch must not trigger run");
    }

    #[test]
    fn two_watches_independent() {
        let src = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        create_file(src.path(), "a.rs", "content");

        let cache1 = cache_dir.path().join("c1.json");
        let cache2 = cache_dir.path().join("c2.json");
        let mgr = FingerprintManager::new();

        // Initialize both watches.
        mgr.check(&cache1, "two-layer", src.path(), &[], &[], &[]);
        mgr.mark_success(&cache1);
        mgr.check(&cache2, "hash", src.path(), &[], &[], &[]);
        mgr.mark_success(&cache2);

        // Invalidate only cache1.
        mgr.invalidate(&cache1);

        // cache2 should still be clean.
        let r2 = mgr.check(&cache2, "hash", src.path(), &[], &[], &[]);
        assert_eq!(r2.decision, "skip");

        // cache1 should need a fresh scan.
        let r1 = mgr.check(&cache1, "two-layer", src.path(), &[], &[], &[]);
        assert_eq!(r1.decision, "run");
    }

    #[test]
    fn on_batch_many_watches_completes_quickly() {
        // Regression test for issue #724: on_batch must not hold the watch-map lock
        // across per-file canonicalize/hash I/O. With hundreds of watch roots the
        // old implementation canonicalized every changed path once *per watch*
        // (O(n^2) syscalls) and hashed under a DashMap write-shard lock, starving
        // RPC handlers and wedging the daemon. The fix pre-computes path metadata
        // once per batch, so this large batch must finish well within budget.
        const ROOTS: usize = 200;

        let cache_dir = TempDir::new().unwrap();
        let mgr = FingerprintManager::new();
        let mut roots = Vec::with_capacity(ROOTS);
        let mut cache_files = Vec::with_capacity(ROOTS);

        for i in 0..ROOTS {
            let root = TempDir::new().unwrap();
            create_file(root.path(), "src.cpp", "original");
            let cache_file = cache_dir.path().join(format!("fp{i}.json"));
            mgr.check(&cache_file, "two-layer", root.path(), &[], &[], &[]);
            mgr.mark_success(&cache_file);
            cache_files.push(cache_file);
            roots.push(root);
        }

        // Modify every tracked file, then deliver one big watcher batch.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let mut changed = Vec::with_capacity(ROOTS);
        for root in &roots {
            create_file(root.path(), "src.cpp", "modified");
            changed.push(canon(&root.path().join("src.cpp")));
        }

        let start = std::time::Instant::now();
        mgr.on_batch(&changed, &[]);
        let elapsed = start.elapsed();

        // Generous budget: the wedge made this effectively unbounded under lock
        // contention. The lock-free variant is orders of magnitude faster.
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "on_batch over {ROOTS} watches took {elapsed:?}, expected < 10s (issue #724 regression)"
        );

        // Correctness: every watch must now be dirty.
        for (i, cache_file) in cache_files.iter().enumerate() {
            let result = mgr.check(cache_file, "two-layer", roots[i].path(), &[], &[], &[]);
            assert_eq!(
                result.decision, "run",
                "watch {i} should be dirty after on_batch"
            );
        }
    }

    #[test]
    fn check_verify_reruns_hash_off_lock() {
        // Regression for issue #724 — the check() sibling of the on_batch fix.
        // check()'s filesystem verify must re-stat/re-hash OUTSIDE the DashMap
        // shard lock. The old code held a `get_mut` write-shard lock across the
        // whole per-file re-hash sweep. Any concurrent whole-map operation
        // (`mark_success` / `mark_failure` / `on_batch` all use `iter_mut`) then
        // blocked for the entire sweep; under heavy parallel cargo that starved
        // the RPC handlers past the client timeout and wedged the daemon.
        //
        // Build one watch over many files, mark it clean, modify every file so
        // the next check() must re-hash the lot, and — while that check runs on
        // another thread — hammer a full-shard sweep (`mark_success` on a
        // nonexistent cache file always visits every shard, matching nothing).
        // No single sweep call may block for anywhere near the verify duration:
        // if it does, the verify is still holding the shard lock across its I/O.
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        const FILES: usize = 600;
        let src = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        let blob_a = vec![b'a'; 16 * 1024];
        let blob_b = vec![b'b'; 16 * 1024];
        for i in 0..FILES {
            std::fs::write(src.path().join(format!("f{i}.c")), &blob_a).unwrap();
        }

        let cache_file = cache_dir.path().join("fp.json");
        let mgr = Arc::new(FingerprintManager::new());

        // Prime, then mark clean so the next check takes the verify branch.
        mgr.check(&cache_file, "two-layer", src.path(), &[], &[], &[]);
        mgr.mark_success(&cache_file);

        // Change every file (new mtime + new content) → verify must re-hash all.
        std::thread::sleep(std::time::Duration::from_millis(50));
        for i in 0..FILES {
            std::fs::write(src.path().join(format!("f{i}.c")), &blob_b).unwrap();
        }

        let done = Arc::new(AtomicBool::new(false));
        let verify_mgr = Arc::clone(&mgr);
        let verify_done = Arc::clone(&done);
        let src_path = src.path().to_path_buf();
        let cache_path = cache_file.clone();
        let verifier = std::thread::spawn(move || {
            let start = std::time::Instant::now();
            let res = verify_mgr.check(&cache_path, "two-layer", &src_path, &[], &[], &[]);
            verify_done.store(true, Ordering::Release);
            (start.elapsed(), res.decision)
        });

        // Hammer a full-shard sweep until the verify finishes, tracking the
        // longest single call — the time a sweep was blocked by the shard lock.
        let nonexistent = cache_dir.path().join("no-such-watch.json");
        let mut max_sweep = std::time::Duration::ZERO;
        let mut sweeps = 0u64;
        while !done.load(Ordering::Acquire) {
            let t = std::time::Instant::now();
            mgr.mark_success(&nonexistent);
            max_sweep = max_sweep.max(t.elapsed());
            sweeps += 1;
        }

        let (verify_dur, decision) = verifier.join().unwrap();
        assert_eq!(decision, "run", "changed content must be detected");
        assert!(sweeps > 0, "sweep loop must have run at least once");
        // The verify must have taken real time for the test to be meaningful
        // (600 re-hashes). If this trips, the fixture is too small.
        assert!(
            verify_dur >= std::time::Duration::from_millis(3),
            "verify was too fast ({verify_dur:?}) to exercise lock contention"
        );
        // The point: a full-shard sweep is never blocked for the re-hash.
        // Buggy (lock held across I/O): max_sweep ≈ verify_dur. Fixed: ~0.
        assert!(
            max_sweep * 4 < verify_dur,
            "a full-shard sweep blocked for {max_sweep:?} during a {verify_dur:?} verify — \
             check() is holding the DashMap shard lock across re-hash I/O (issue #724)"
        );
    }
}
