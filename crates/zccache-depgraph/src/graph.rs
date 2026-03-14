//! Core dependency graph.
//!
//! Two-map design:
//! - `files`: shared file nodes (one per unique path, across all contexts)
//! - `contexts`: per-compilation-context entries with resolved include lists

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use zccache_hash::ContentHash;

use crate::context::{compute_artifact_key, ArtifactKey, CompileContext, ContextKey};
use crate::scanner::{IncludeDirective, ScanResult};

/// A file node in the graph. Shared across all contexts.
#[derive(Debug, Clone)]
pub struct FileEntry {
    /// Raw `#include` directives found in this file.
    pub includes: Vec<IncludeDirective>,
    /// When this file was last scanned for includes.
    pub scanned_at: Instant,
}

/// State of a compilation context in the graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextState {
    /// No include list yet — needs full recursive scan.
    Cold,
    /// Include list populated and believed current.
    Warm,
    /// Something changed — needs partial or full rescan.
    Stale,
}

/// A compilation context entry in the graph.
#[derive(Debug, Clone)]
pub struct ContextEntry {
    /// The compilation context (source + flags).
    pub context: CompileContext,
    /// Flat list of all transitive resolved headers (absolute paths).
    pub resolved_includes: Vec<PathBuf>,
    /// Include names that could not be resolved to any file.
    pub unresolved_includes: Vec<String>,
    /// True if any `#include MACRO` was found during scanning.
    pub has_computed_includes: bool,
    /// Last computed artifact key.
    pub artifact_key: Option<ArtifactKey>,
    /// When this entry was last accessed (for trimming).
    pub last_accessed: Instant,
    /// Current state.
    pub state: ContextState,
}

/// Result of checking a context against the file cache.
#[derive(Debug, Clone)]
pub enum CacheVerdict {
    /// All files fresh, artifact key valid. Use cached object.
    Hit { artifact_key: ArtifactKey },
    /// Source changed but headers are fresh. New artifact key computed.
    SourceChanged { artifact_key: ArtifactKey },
    /// One or more headers changed. Rescan needed.
    HeadersChanged { changed: Vec<PathBuf> },
    /// No include list yet. Full scan required.
    Cold,
    /// Contains `#include MACRO`. Needs preprocessor fallback.
    NeedsPreprocessor,
}

/// Statistics about the dependency graph.
#[derive(Debug, Clone)]
pub struct DepGraphStats {
    /// Number of unique files tracked.
    pub file_count: usize,
    /// Number of compilation contexts tracked.
    pub context_count: usize,
    /// Number of check() calls.
    pub checks: u64,
    /// Number of cache hits (ultra-fast + fast path).
    pub hits: u64,
    /// Number of cache misses.
    pub misses: u64,
}

/// The core dependency graph.
pub struct DepGraph {
    /// Shared file nodes: path → scanned includes.
    files: DashMap<PathBuf, FileEntry>,
    /// Per-context entries: context key → include list + state.
    contexts: DashMap<ContextKey, ContextEntry>,
    /// Stats counters.
    checks: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl DepGraph {
    /// Create a new empty dependency graph.
    #[must_use]
    pub fn new() -> Self {
        Self {
            files: DashMap::new(),
            contexts: DashMap::new(),
            checks: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// Register a compilation context. Returns the context key.
    /// If the context already exists, returns the existing key.
    pub fn register(&self, ctx: CompileContext) -> ContextKey {
        let key = ctx.context_key();

        self.contexts.entry(key).or_insert_with(|| ContextEntry {
            context: ctx,
            resolved_includes: Vec::new(),
            unresolved_includes: Vec::new(),
            has_computed_includes: false,
            artifact_key: None,
            last_accessed: Instant::now(),
            state: ContextState::Cold,
        });

        key
    }

    /// Check if a compilation can use cached output.
    ///
    /// `is_fresh` is called for each file path. It should query Layer 1
    /// (fscache) and return `true` if the file has not changed since last
    /// known state.
    ///
    /// `get_hash` retrieves the content hash for a file from Layer 1.
    pub fn check<F, G>(&self, key: &ContextKey, is_fresh: F, get_hash: G) -> CacheVerdict
    where
        F: Fn(&Path) -> bool,
        G: Fn(&Path) -> Option<ContentHash>,
    {
        self.checks.fetch_add(1, Ordering::Relaxed);

        let mut entry = match self.contexts.get_mut(key) {
            Some(e) => e,
            None => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                return CacheVerdict::Cold;
            }
        };

        entry.last_accessed = Instant::now();

        if entry.state == ContextState::Cold {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return CacheVerdict::Cold;
        }

        if entry.has_computed_includes {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return CacheVerdict::NeedsPreprocessor;
        }

        // Check source file freshness.
        let source_fresh = is_fresh(&entry.context.source_file);

        // Check all headers.
        let mut changed_headers = Vec::new();
        for header in &entry.resolved_includes {
            if !is_fresh(header) {
                changed_headers.push(header.clone());
            }
        }
        // Also check force-included files (PCH, -include).
        for fi in &entry.context.force_includes {
            if !is_fresh(fi) {
                changed_headers.push(fi.clone());
            }
        }

        if !changed_headers.is_empty() {
            self.misses.fetch_add(1, Ordering::Relaxed);
            entry.state = ContextState::Stale;
            return CacheVerdict::HeadersChanged {
                changed: changed_headers,
            };
        }

        // All headers fresh. Compute artifact key.
        let mut file_hashes = Vec::new();

        if let Some(h) = get_hash(&entry.context.source_file) {
            file_hashes.push((entry.context.source_file.clone(), h));
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return CacheVerdict::Cold;
        }

        for header in &entry.resolved_includes {
            if let Some(h) = get_hash(header) {
                file_hashes.push((header.clone(), h));
            } else {
                self.misses.fetch_add(1, Ordering::Relaxed);
                return CacheVerdict::Cold;
            }
        }
        // Hash force-included files (PCH content must affect artifact key).
        for fi in &entry.context.force_includes {
            if let Some(h) = get_hash(fi) {
                file_hashes.push((fi.clone(), h));
            } else {
                self.misses.fetch_add(1, Ordering::Relaxed);
                return CacheVerdict::Cold;
            }
        }

        let artifact_key = compute_artifact_key(key, &mut file_hashes);

        if source_fresh {
            // Ultra-fast path: nothing changed at all.
            if entry.artifact_key == Some(artifact_key) {
                self.hits.fetch_add(1, Ordering::Relaxed);
                return CacheVerdict::Hit { artifact_key };
            }
            // Source is "fresh" by watcher but artifact key differs
            // (could be first check after update).
            entry.artifact_key = Some(artifact_key);
            self.hits.fetch_add(1, Ordering::Relaxed);
            CacheVerdict::Hit { artifact_key }
        } else {
            // Fast path: only source changed, headers all fresh.
            entry.artifact_key = Some(artifact_key);
            self.hits.fetch_add(1, Ordering::Relaxed);
            CacheVerdict::SourceChanged { artifact_key }
        }
    }

    /// Check if a compilation can use cached output, with diagnostic reason.
    ///
    /// Same logic as [`check()`](Self::check) but returns a reason string
    /// explaining why the verdict was reached (useful for session logs).
    pub fn check_diagnostic<F, G>(
        &self,
        key: &ContextKey,
        is_fresh: F,
        get_hash: G,
    ) -> (CacheVerdict, String)
    where
        F: Fn(&Path) -> bool,
        G: Fn(&Path) -> Option<ContentHash>,
    {
        self.checks.fetch_add(1, Ordering::Relaxed);

        let mut entry = match self.contexts.get_mut(key) {
            Some(e) => e,
            None => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                return (CacheVerdict::Cold, "context_key not registered".to_string());
            }
        };

        entry.last_accessed = Instant::now();

        if entry.state == ContextState::Cold {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return (
                CacheVerdict::Cold,
                "context never updated (state=Cold)".to_string(),
            );
        }

        if entry.has_computed_includes {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return (
                CacheVerdict::NeedsPreprocessor,
                "has computed includes, needs preprocessor".to_string(),
            );
        }

        // Check source file freshness.
        let source_fresh = is_fresh(&entry.context.source_file);

        // Check all headers.
        let mut changed_headers = Vec::new();
        for header in &entry.resolved_includes {
            if !is_fresh(header) {
                changed_headers.push(header.clone());
            }
        }
        // Also check force-included files (PCH, -include).
        for fi in &entry.context.force_includes {
            if !is_fresh(fi) {
                changed_headers.push(fi.clone());
            }
        }

        if !changed_headers.is_empty() {
            self.misses.fetch_add(1, Ordering::Relaxed);
            entry.state = ContextState::Stale;
            let names: Vec<String> = changed_headers
                .iter()
                .map(|p| p.display().to_string())
                .collect();
            return (
                CacheVerdict::HeadersChanged {
                    changed: changed_headers,
                },
                format!("headers changed: [{}]", names.join(", ")),
            );
        }

        // All headers fresh. Compute artifact key.
        let mut file_hashes = Vec::new();

        if let Some(h) = get_hash(&entry.context.source_file) {
            file_hashes.push((entry.context.source_file.clone(), h));
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return (
                CacheVerdict::Cold,
                format!(
                    "source hash missing: {}",
                    entry.context.source_file.display()
                ),
            );
        }

        for header in &entry.resolved_includes {
            if let Some(h) = get_hash(header) {
                file_hashes.push((header.clone(), h));
            } else {
                self.misses.fetch_add(1, Ordering::Relaxed);
                return (
                    CacheVerdict::Cold,
                    format!("header hash missing: {}", header.display()),
                );
            }
        }
        // Hash force-included files (PCH content must affect artifact key).
        for fi in &entry.context.force_includes {
            if let Some(h) = get_hash(fi) {
                file_hashes.push((fi.clone(), h));
            } else {
                self.misses.fetch_add(1, Ordering::Relaxed);
                return (
                    CacheVerdict::Cold,
                    format!("force-include hash missing: {}", fi.display()),
                );
            }
        }

        let artifact_key = compute_artifact_key(key, &mut file_hashes);

        if source_fresh {
            if entry.artifact_key == Some(artifact_key) {
                self.hits.fetch_add(1, Ordering::Relaxed);
                let hex = &artifact_key.hash().to_hex()[..8];
                return (
                    CacheVerdict::Hit { artifact_key },
                    format!("hit: artifact_key={hex}"),
                );
            }
            // Source is "fresh" by watcher but artifact key differs.
            entry.artifact_key = Some(artifact_key);
            self.hits.fetch_add(1, Ordering::Relaxed);
            let hex = &artifact_key.hash().to_hex()[..8];
            (
                CacheVerdict::Hit { artifact_key },
                format!("hit: artifact_key={hex} (first check after update)"),
            )
        } else {
            entry.artifact_key = Some(artifact_key);
            self.hits.fetch_add(1, Ordering::Relaxed);
            (
                CacheVerdict::SourceChanged { artifact_key },
                "source content changed".to_string(),
            )
        }
    }

    /// After a compile (or on cold path), record the full include list.
    ///
    /// `get_hash` retrieves the content hash for a file from Layer 1.
    pub fn update<G>(
        &self,
        key: &ContextKey,
        scan_result: ScanResult,
        get_hash: G,
    ) -> Option<ArtifactKey>
    where
        G: Fn(&Path) -> Option<ContentHash>,
    {
        let mut entry = self.contexts.get_mut(key)?;

        entry.resolved_includes = scan_result.resolved;
        entry.unresolved_includes = scan_result.unresolved;
        entry.has_computed_includes = scan_result.has_computed;
        entry.state = ContextState::Warm;
        entry.last_accessed = Instant::now();

        // Compute artifact key.
        let mut file_hashes = Vec::new();
        if let Some(h) = get_hash(&entry.context.source_file) {
            file_hashes.push((entry.context.source_file.clone(), h));
        }
        for header in &entry.resolved_includes {
            if let Some(h) = get_hash(header) {
                file_hashes.push((header.clone(), h));
            }
        }
        // Hash force-included files (PCH content must affect artifact key).
        for fi in &entry.context.force_includes {
            if let Some(h) = get_hash(fi) {
                file_hashes.push((fi.clone(), h));
            }
        }

        let artifact_key = compute_artifact_key(key, &mut file_hashes);
        entry.artifact_key = Some(artifact_key);

        Some(artifact_key)
    }

    /// Trim entries not accessed within the given duration.
    /// Returns the number of entries removed.
    pub fn trim(&self, max_age: Duration) -> usize {
        let now = Instant::now();
        let mut removed = 0;

        self.contexts.retain(|_, entry| {
            if now.duration_since(entry.last_accessed) > max_age {
                removed += 1;
                false
            } else {
                true
            }
        });

        // Also trim file entries not referenced by any context.
        let referenced: std::collections::HashSet<PathBuf> = self
            .contexts
            .iter()
            .flat_map(
                |entry: dashmap::mapref::multiple::RefMulti<'_, ContextKey, ContextEntry>| {
                    let mut paths = entry.value().resolved_includes.clone();
                    paths.push(entry.value().context.source_file.clone());
                    paths
                },
            )
            .collect();

        self.files.retain(|path, _| referenced.contains(path));

        removed
    }

    /// Clear all graph state: files, contexts, and stats counters.
    pub fn clear(&self) {
        self.files.clear();
        self.contexts.clear();
        self.checks.store(0, Ordering::Relaxed);
        self.hits.store(0, Ordering::Relaxed);
        self.misses.store(0, Ordering::Relaxed);
    }

    /// Get statistics about the graph.
    #[must_use]
    pub fn stats(&self) -> DepGraphStats {
        DepGraphStats {
            file_count: self.files.len(),
            context_count: self.contexts.len(),
            checks: self.checks.load(Ordering::Relaxed),
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
        }
    }

    /// Get the state of a context entry.
    #[must_use]
    pub fn get_state(&self, key: &ContextKey) -> Option<ContextState> {
        self.contexts.get(key).map(|e| e.state)
    }

    /// Get the resolved includes for a context.
    #[must_use]
    pub fn get_includes(&self, key: &ContextKey) -> Option<Vec<PathBuf>> {
        self.contexts.get(key).map(|e| e.resolved_includes.clone())
    }

    /// Store scanned includes for a file (shared file node).
    pub fn store_file_includes(&self, path: PathBuf, includes: Vec<IncludeDirective>) {
        self.files.insert(
            path,
            FileEntry {
                includes,
                scanned_at: Instant::now(),
            },
        );
    }

    /// Get scanned includes for a file.
    #[must_use]
    pub fn get_file_includes(&self, path: &PathBuf) -> Option<Vec<IncludeDirective>> {
        self.files.get(path).map(|e| e.includes.clone())
    }

    /// Iterate over all context entries.
    pub(crate) fn contexts_iter(&self) -> dashmap::iter::Iter<'_, ContextKey, ContextEntry> {
        self.contexts.iter()
    }

    /// Mark a context as stale, requiring rescan on next check.
    /// Returns `true` if the context existed and was marked stale.
    pub fn mark_stale(&self, key: &ContextKey) -> bool {
        if let Some(mut entry) = self.contexts.get_mut(key) {
            entry.state = ContextState::Stale;
            true
        } else {
            false
        }
    }

    /// Bulk-populate contexts from parsed compile commands.
    ///
    /// For each command, parses the arguments, builds a `CompileContext`
    /// (merging in the provided system include paths), and registers it.
    /// Returns the context keys for all successfully registered entries.
    pub fn ingest_compile_commands(
        &self,
        commands: &[crate::compile_commands::CompileCommand],
        system_includes: &[std::path::PathBuf],
    ) -> Vec<ContextKey> {
        commands
            .iter()
            .map(|cmd| {
                let parsed = cmd.parse();
                let mut ctx = CompileContext::from_parsed_args(&parsed);

                // Merge system includes into the context's search paths.
                // These go into the `system` field, appended after any
                // explicit -isystem paths.
                for path in system_includes {
                    if !ctx.include_search.system.contains(path) {
                        ctx.include_search.system.push(path.clone());
                    }
                }

                self.register(ctx)
            })
            .collect()
    }
}

impl Default for DepGraph {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use crate::search_paths::IncludeSearchPaths;

    fn make_ctx(source: &str) -> CompileContext {
        CompileContext {
            source_file: PathBuf::from(source),
            include_search: IncludeSearchPaths::default(),
            defines: Vec::new(),
            flags: Vec::new(),
            force_includes: Vec::new(),
        }
    }

    fn always_fresh(_: &Path) -> bool {
        true
    }

    fn never_fresh(_: &Path) -> bool {
        false
    }

    fn dummy_hash(path: &Path) -> Option<ContentHash> {
        Some(zccache_hash::hash_bytes(path.to_string_lossy().as_bytes()))
    }

    #[test]
    fn register_returns_consistent_key() {
        let graph = DepGraph::new();
        let ctx = make_ctx("/src/a.c");
        let k1 = graph.register(ctx.clone());
        let k2 = graph.register(ctx);
        assert_eq!(k1, k2);
    }

    #[test]
    fn cold_context_returns_cold() {
        let graph = DepGraph::new();
        let key = graph.register(make_ctx("/src/a.c"));
        let verdict = graph.check(&key, always_fresh, dummy_hash);
        assert!(matches!(verdict, CacheVerdict::Cold));
    }

    #[test]
    fn unregistered_key_returns_cold() {
        let graph = DepGraph::new();
        let ctx = make_ctx("/src/a.c");
        let key = ctx.context_key();
        let verdict = graph.check(&key, always_fresh, dummy_hash);
        assert!(matches!(verdict, CacheVerdict::Cold));
    }

    #[test]
    fn warm_context_all_fresh_returns_hit() {
        let graph = DepGraph::new();
        let key = graph.register(make_ctx("/src/a.c"));

        let scan = ScanResult {
            resolved: vec![PathBuf::from("/inc/b.h")],
            unresolved: Vec::new(),
            has_computed: false,
        };
        graph.update(&key, scan, dummy_hash);

        let verdict = graph.check(&key, always_fresh, dummy_hash);
        assert!(matches!(verdict, CacheVerdict::Hit { .. }));
    }

    #[test]
    fn warm_context_source_changed_returns_source_changed() {
        let graph = DepGraph::new();
        let key = graph.register(make_ctx("/src/a.c"));

        let scan = ScanResult {
            resolved: vec![PathBuf::from("/inc/b.h")],
            unresolved: Vec::new(),
            has_computed: false,
        };
        graph.update(&key, scan, dummy_hash);

        // Source is stale, headers are fresh.
        let is_fresh = |p: &Path| p != Path::new("/src/a.c");
        let verdict = graph.check(&key, is_fresh, dummy_hash);
        assert!(matches!(verdict, CacheVerdict::SourceChanged { .. }));
    }

    #[test]
    fn warm_context_header_changed_returns_headers_changed() {
        let graph = DepGraph::new();
        let key = graph.register(make_ctx("/src/a.c"));

        let scan = ScanResult {
            resolved: vec![PathBuf::from("/inc/b.h"), PathBuf::from("/inc/c.h")],
            unresolved: Vec::new(),
            has_computed: false,
        };
        graph.update(&key, scan, dummy_hash);

        // b.h is stale.
        let is_fresh = |p: &Path| p != Path::new("/inc/b.h");
        let verdict = graph.check(&key, is_fresh, dummy_hash);
        match verdict {
            CacheVerdict::HeadersChanged { changed } => {
                assert_eq!(changed, vec![PathBuf::from("/inc/b.h")]);
            }
            other => panic!("expected HeadersChanged, got {other:?}"),
        }
    }

    #[test]
    fn computed_includes_returns_needs_preprocessor() {
        let graph = DepGraph::new();
        let key = graph.register(make_ctx("/src/a.c"));

        let scan = ScanResult {
            resolved: vec![PathBuf::from("/inc/b.h")],
            unresolved: Vec::new(),
            has_computed: true,
        };
        graph.update(&key, scan, dummy_hash);

        let verdict = graph.check(&key, always_fresh, dummy_hash);
        assert!(matches!(verdict, CacheVerdict::NeedsPreprocessor));
    }

    #[test]
    fn update_sets_warm_state() {
        let graph = DepGraph::new();
        let key = graph.register(make_ctx("/src/a.c"));
        assert_eq!(graph.get_state(&key), Some(ContextState::Cold));

        let scan = ScanResult {
            resolved: Vec::new(),
            unresolved: Vec::new(),
            has_computed: false,
        };
        graph.update(&key, scan, dummy_hash);
        assert_eq!(graph.get_state(&key), Some(ContextState::Warm));
    }

    #[test]
    fn header_change_sets_stale_state() {
        let graph = DepGraph::new();
        let key = graph.register(make_ctx("/src/a.c"));

        let scan = ScanResult {
            resolved: vec![PathBuf::from("/h.h")],
            unresolved: Vec::new(),
            has_computed: false,
        };
        graph.update(&key, scan, dummy_hash);
        assert_eq!(graph.get_state(&key), Some(ContextState::Warm));

        graph.check(&key, never_fresh, dummy_hash);
        assert_eq!(graph.get_state(&key), Some(ContextState::Stale));
    }

    #[test]
    fn trim_removes_old_entries() {
        let graph = DepGraph::new();
        let key = graph.register(make_ctx("/src/a.c"));

        let scan = ScanResult {
            resolved: Vec::new(),
            unresolved: Vec::new(),
            has_computed: false,
        };
        graph.update(&key, scan, dummy_hash);

        // Sleep briefly so the entry's last_accessed is older than Duration::ZERO.
        std::thread::sleep(Duration::from_millis(5));

        // Trim with max_age=0: everything not accessed this exact instant is removed.
        let removed = graph.trim(Duration::ZERO);
        assert_eq!(removed, 1);
        assert_eq!(graph.stats().context_count, 0);
    }

    #[test]
    fn trim_keeps_recent_entries() {
        let graph = DepGraph::new();
        graph.register(make_ctx("/src/a.c"));
        let removed = graph.trim(Duration::from_secs(60));
        assert_eq!(removed, 0);
        assert_eq!(graph.stats().context_count, 1);
    }

    #[test]
    fn stats_track_checks_and_hits() {
        let graph = DepGraph::new();
        let key = graph.register(make_ctx("/src/a.c"));

        let scan = ScanResult {
            resolved: Vec::new(),
            unresolved: Vec::new(),
            has_computed: false,
        };
        graph.update(&key, scan, dummy_hash);

        graph.check(&key, always_fresh, dummy_hash);
        graph.check(&key, always_fresh, dummy_hash);

        let stats = graph.stats();
        assert_eq!(stats.checks, 2);
        assert_eq!(stats.hits, 2);
        assert_eq!(stats.misses, 0);
        assert_eq!(stats.context_count, 1);
    }

    #[test]
    fn artifact_key_changes_when_hash_changes() {
        let graph = DepGraph::new();
        let key = graph.register(make_ctx("/src/a.c"));

        let scan = ScanResult {
            resolved: Vec::new(),
            unresolved: Vec::new(),
            has_computed: false,
        };

        let hash_v1 = |_: &Path| Some(zccache_hash::hash_bytes(b"v1"));
        let ak1 = graph.update(&key, scan.clone(), hash_v1).unwrap();

        let hash_v2 = |_: &Path| Some(zccache_hash::hash_bytes(b"v2"));
        let ak2 = graph.update(&key, scan, hash_v2).unwrap();

        assert_ne!(ak1, ak2);
    }

    #[test]
    fn store_and_get_file_includes() {
        let graph = DepGraph::new();
        let path = PathBuf::from("/src/foo.h");
        let includes = vec![crate::IncludeDirective {
            kind: crate::IncludeKind::Quoted,
            path: "bar.h".to_string(),
            line: 1,
        }];

        graph.store_file_includes(path.clone(), includes.clone());
        let retrieved = graph.get_file_includes(&path).unwrap();
        assert_eq!(retrieved.len(), 1);
        assert_eq!(retrieved[0].path, "bar.h");
    }

    #[test]
    fn concurrent_register_and_check() {
        use std::sync::Arc;
        use std::thread;

        let graph = Arc::new(DepGraph::new());
        let mut handles = Vec::new();

        // 4 threads registering and checking.
        for t in 0..4 {
            let graph = Arc::clone(&graph);
            handles.push(thread::spawn(move || {
                for i in 0..50 {
                    let ctx = make_ctx(&format!("/src/t{t}_f{i}.c"));
                    let key = graph.register(ctx);

                    let scan = ScanResult {
                        resolved: vec![PathBuf::from(format!("/inc/t{t}_h{i}.h"))],
                        unresolved: Vec::new(),
                        has_computed: false,
                    };
                    graph.update(&key, scan, dummy_hash);
                    graph.check(&key, always_fresh, dummy_hash);
                }
            }));
        }

        for h in handles {
            h.join().expect("thread panicked");
        }

        let stats = graph.stats();
        assert_eq!(stats.context_count, 200); // 4 * 50
        assert_eq!(stats.checks, 200);
    }

    #[test]
    fn ingest_compile_commands_registers_contexts() {
        let json = r#"[
            {
                "directory": "/build",
                "command": "g++ -I/project/include -DNDEBUG -std=c++17 -c /project/src/main.cpp -o main.o",
                "file": "/project/src/main.cpp"
            },
            {
                "directory": "/build",
                "command": "g++ -I/project/include -DNDEBUG -std=c++17 -c /project/src/util.cpp -o util.o",
                "file": "/project/src/util.cpp"
            }
        ]"#;

        let commands = crate::compile_commands::parse_compile_commands_json(json).unwrap();
        let graph = DepGraph::new();
        let system_includes = vec![PathBuf::from("/usr/include")];
        let keys = graph.ingest_compile_commands(&commands, &system_includes);

        assert_eq!(keys.len(), 2);
        assert_eq!(graph.stats().context_count, 2);

        // All contexts should be Cold (not yet scanned).
        for key in &keys {
            assert_eq!(graph.get_state(key), Some(ContextState::Cold));
        }
    }

    #[test]
    fn ingest_merges_system_includes() {
        let json = r#"[
            {
                "directory": "/build",
                "command": "g++ -isystem /explicit/system -c /src/main.cpp",
                "file": "/src/main.cpp"
            }
        ]"#;

        let commands = crate::compile_commands::parse_compile_commands_json(json).unwrap();
        let graph = DepGraph::new();
        let system_includes = vec![PathBuf::from("/usr/include")];
        let keys = graph.ingest_compile_commands(&commands, &system_includes);

        assert_eq!(keys.len(), 1);

        // The context should have both the explicit and system includes.
        // We can verify by checking the context key differs with/without system includes.
        let keys_no_sys = graph.ingest_compile_commands(&commands, &[]);

        // Same source + different system includes = different context keys.
        // Wait, ingest re-uses existing contexts if key matches.
        // Since system includes affect the context key, these should differ.
        // But we already registered the first one, so let's check differently.
        // The first call added /usr/include to system paths, so the key
        // incorporates it. A second call with empty system_includes would
        // produce a different key.
        assert_ne!(keys[0], keys_no_sys[0]);
    }

    #[test]
    fn ingest_deduplicates_system_includes() {
        let json = r#"[
            {
                "directory": "/build",
                "command": "g++ -isystem /usr/include -c /src/main.cpp",
                "file": "/src/main.cpp"
            }
        ]"#;

        let commands = crate::compile_commands::parse_compile_commands_json(json).unwrap();
        let graph = DepGraph::new();
        // /usr/include is already in -isystem, should not be added twice.
        let system_includes = vec![PathBuf::from("/usr/include")];
        let keys = graph.ingest_compile_commands(&commands, &system_includes);
        assert_eq!(keys.len(), 1);
    }

    #[test]
    fn clear_resets_everything() {
        let graph = DepGraph::new();
        let key = graph.register(make_ctx("/src/a.c"));

        let scan = ScanResult {
            resolved: vec![PathBuf::from("/inc/b.h")],
            unresolved: Vec::new(),
            has_computed: false,
        };
        graph.update(&key, scan, dummy_hash);
        graph.check(&key, always_fresh, dummy_hash);

        let stats_before = graph.stats();
        assert!(stats_before.context_count > 0);
        assert!(stats_before.checks > 0);
        assert!(stats_before.hits > 0);

        graph.clear();

        let stats_after = graph.stats();
        assert_eq!(stats_after.context_count, 0);
        assert_eq!(stats_after.file_count, 0);
        assert_eq!(stats_after.checks, 0);
        assert_eq!(stats_after.hits, 0);
        assert_eq!(stats_after.misses, 0);
    }

    #[test]
    fn mark_stale_changes_state() {
        let graph = DepGraph::new();
        let key = graph.register(make_ctx("/src/a.c"));

        let scan = ScanResult {
            resolved: Vec::new(),
            unresolved: Vec::new(),
            has_computed: false,
        };
        graph.update(&key, scan, dummy_hash);
        assert_eq!(graph.get_state(&key), Some(ContextState::Warm));

        assert!(graph.mark_stale(&key));
        assert_eq!(graph.get_state(&key), Some(ContextState::Stale));
    }
}
