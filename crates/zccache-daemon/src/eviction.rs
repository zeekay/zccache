//! Memory-bounded eviction for long-running daemon.
//!
//! Estimates in-memory cache size using per-entry constants and evicts
//! entries in priority order when the total exceeds the configured budget.

use dashmap::DashMap;
use std::time::Duration;
use zccache_depgraph::{ContextKey, DepGraph};
use zccache_fscache::CacheSystem;

use crate::server::{trim_fast_hit_cache, CachedArtifact, FastHitEntry};

/// Estimated bytes per metadata cache entry.
const METADATA_ENTRY_BYTES: usize = 400;
/// Estimated bytes per journal `last_change` entry.
const JOURNAL_ENTRY_BYTES: usize = 280;
/// Estimated bytes per depgraph file entry.
const DEPGRAPH_FILE_BYTES: usize = 600;
/// Estimated bytes per depgraph context entry.
const DEPGRAPH_CONTEXT_BYTES: usize = 2048;
/// Estimated bytes per fast-hit cache entry.
const FAST_HIT_ENTRY_BYTES: usize = 200;
/// Estimated fixed overhead per cached artifact entry (excludes payload).
const ARTIFACT_OVERHEAD_BYTES: usize = 200;

/// Snapshot of estimated memory usage across all in-memory caches.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct MemorySnapshot {
    pub(crate) metadata_entries: usize,
    pub(crate) journal_entries: usize,
    pub(crate) depgraph_files: usize,
    pub(crate) depgraph_contexts: usize,
    pub(crate) fast_hit_entries: usize,
    pub(crate) artifact_entries: usize,
    /// Total artifact payload bytes (actual, not estimated).
    pub(crate) artifact_payload_bytes: usize,
    /// Bytes in spawn_blocking persistence tasks, not yet visible to eviction.
    pub(crate) in_flight_bytes: usize,
    /// Total estimated bytes across all subsystems.
    pub(crate) total_bytes: usize,
}

/// Compute a memory usage snapshot.
pub(crate) fn memory_snapshot(
    cache_system: &CacheSystem,
    dep_graph: &DepGraph,
    fast_hit_cache: &DashMap<ContextKey, FastHitEntry>,
    artifacts: &DashMap<String, CachedArtifact>,
    in_flight_bytes: usize,
) -> MemorySnapshot {
    let metadata_entries = cache_system.metadata().len();
    let journal_entries = cache_system.journal().last_change_len();
    let dg_stats = dep_graph.stats();
    let depgraph_files = dg_stats.file_count;
    let depgraph_contexts = dg_stats.context_count;
    let fast_hit_entries = fast_hit_cache.len();
    let artifact_entries = artifacts.len();

    let artifact_payload_bytes: usize = artifacts
        .iter()
        .map(|entry| {
            entry
                .value()
                .artifact
                .outputs
                .iter()
                .map(|o| o.data.len())
                .sum::<usize>()
        })
        .sum();

    let total_bytes = metadata_entries * METADATA_ENTRY_BYTES
        + journal_entries * JOURNAL_ENTRY_BYTES
        + depgraph_files * DEPGRAPH_FILE_BYTES
        + depgraph_contexts * DEPGRAPH_CONTEXT_BYTES
        + fast_hit_entries * FAST_HIT_ENTRY_BYTES
        + artifact_entries * ARTIFACT_OVERHEAD_BYTES
        + artifact_payload_bytes
        + in_flight_bytes;

    MemorySnapshot {
        metadata_entries,
        journal_entries,
        depgraph_files,
        depgraph_contexts,
        fast_hit_entries,
        artifact_entries,
        artifact_payload_bytes,
        in_flight_bytes,
        total_bytes,
    }
}

/// Evict entries until estimated memory is at or below `budget_bytes`.
///
/// Eviction priority (cheapest to regenerate first):
/// 1. Fast-hit cache (ephemeral, regenerated on next compile hit)
/// 2. Metadata cache + journal (re-populated on next stat)
/// 3. Depgraph contexts + orphaned files (rebuilt on next compile)
///
/// Artifacts are **not** evicted (per design — handled separately).
///
/// Evicts to 90% of budget to avoid thrashing.
///
/// Returns `(estimated_freed_bytes, items_removed)`.
pub(crate) fn evict_to_budget(
    budget_bytes: u64,
    cache_system: &CacheSystem,
    dep_graph: &DepGraph,
    fast_hit_cache: &DashMap<ContextKey, FastHitEntry>,
    artifacts: &DashMap<String, CachedArtifact>,
    in_flight_bytes: usize,
) -> (u64, usize) {
    let snap = memory_snapshot(
        cache_system,
        dep_graph,
        fast_hit_cache,
        artifacts,
        in_flight_bytes,
    );

    if (snap.total_bytes as u64) <= budget_bytes {
        return (0, 0);
    }

    // Target 90% of budget to avoid evicting a tiny bit every cycle.
    let target = (budget_bytes as f64 * 0.9) as u64;
    let mut to_free = snap.total_bytes as u64 - target;
    let mut total_freed: u64 = 0;
    let mut total_items: usize = 0;

    // Priority 1: fast-hit cache (cheapest to lose).
    if to_free > 0 && snap.fast_hit_entries > 0 {
        let removed = trim_fast_hit_cache(fast_hit_cache, Duration::ZERO);
        let freed = (removed * FAST_HIT_ENTRY_BYTES) as u64;
        total_freed += freed;
        total_items += removed;
        to_free = to_free.saturating_sub(freed);
    }

    // Priority 2: metadata + journal.
    if to_free > 0 && !cache_system.metadata().is_empty() {
        let entries_to_evict = (to_free as usize / METADATA_ENTRY_BYTES)
            .max(1)
            .min(cache_system.metadata().len());
        let (meta_removed, journal_removed) = cache_system.evict_oldest(entries_to_evict);
        let freed =
            (meta_removed * METADATA_ENTRY_BYTES + journal_removed * JOURNAL_ENTRY_BYTES) as u64;
        total_freed += freed;
        total_items += meta_removed + journal_removed;
        to_free = to_free.saturating_sub(freed);
    }

    // Priority 3: depgraph contexts (trim all, then orphaned files cleaned up).
    if to_free > 0 {
        let dg_stats = dep_graph.stats();
        if dg_stats.context_count > 0 {
            let removed = dep_graph.trim(Duration::ZERO);
            let freed = (removed * DEPGRAPH_CONTEXT_BYTES) as u64;
            // File entries cleaned up by trim() internally.
            let files_after = dep_graph.stats().file_count;
            let files_freed = dg_stats.file_count.saturating_sub(files_after);
            let file_bytes = (files_freed * DEPGRAPH_FILE_BYTES) as u64;
            total_freed += freed + file_bytes;
            total_items += removed + files_freed;
        }
    }

    (total_freed, total_items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{Instant, SystemTime};
    use zccache_depgraph::CompileContext;
    use zccache_fscache::{Confidence, FileMetadata};

    fn empty_caches() -> (
        CacheSystem,
        DepGraph,
        DashMap<ContextKey, FastHitEntry>,
        DashMap<String, CachedArtifact>,
    ) {
        (
            CacheSystem::new(),
            DepGraph::new(),
            DashMap::new(),
            DashMap::new(),
        )
    }

    fn make_ctx(source: &str) -> CompileContext {
        CompileContext {
            source_file: PathBuf::from(source),
            include_search: zccache_depgraph::IncludeSearchPaths::default(),
            defines: Vec::new(),
            flags: Vec::new(),
            force_includes: Vec::new(),
            unknown_flags: Vec::new(),
        }
    }

    fn make_context_key(source: &str) -> ContextKey {
        make_ctx(source).context_key()
    }

    #[test]
    fn snapshot_empty() {
        let (cs, dg, fh, art) = empty_caches();
        let snap = memory_snapshot(&cs, &dg, &fh, &art, 0);
        assert_eq!(snap.total_bytes, 0);
        assert_eq!(snap.metadata_entries, 0);
        assert_eq!(snap.fast_hit_entries, 0);
        assert_eq!(snap.artifact_entries, 0);
    }

    #[test]
    fn snapshot_with_entries() {
        let (cs, dg, fh, _art) = empty_caches();

        // Add a fast-hit entry.
        fh.insert(
            make_context_key("/tmp/snap.c"),
            FastHitEntry {
                clock: zccache_fscache::Clock::ZERO,
                artifact_key_hex: String::new(),
                cached_at: Instant::now(),
            },
        );

        let snap = memory_snapshot(&cs, &dg, &fh, &DashMap::new(), 0);
        assert_eq!(snap.fast_hit_entries, 1);
        assert!(snap.total_bytes >= FAST_HIT_ENTRY_BYTES);
    }

    #[test]
    fn evict_noop_under_budget() {
        let (cs, dg, fh, art) = empty_caches();
        let (freed, items) = evict_to_budget(1_073_741_824, &cs, &dg, &fh, &art, 0);
        assert_eq!(freed, 0);
        assert_eq!(items, 0);
    }

    #[test]
    fn evict_fast_hit_first() {
        let (cs, dg, fh, art) = empty_caches();

        // Add enough fast-hit entries to exceed a tiny budget.
        for i in 0..100 {
            fh.insert(
                make_context_key(&format!("/tmp/fh{i}.c")),
                FastHitEntry {
                    clock: zccache_fscache::Clock::ZERO,
                    artifact_key_hex: String::new(),
                    cached_at: Instant::now(),
                },
            );
        }

        let budget = 1000; // Very small budget.
        let (freed, items) = evict_to_budget(budget, &cs, &dg, &fh, &art, 0);
        assert!(freed > 0);
        assert_eq!(items, 100); // All fast-hit entries evicted.
        assert!(fh.is_empty());
    }

    #[test]
    fn evict_cascades_to_metadata() {
        let (cs, dg, fh, art) = empty_caches();

        // Add metadata entries.
        for i in 0..50 {
            cs.metadata().insert(
                PathBuf::from(format!("/tmp/meta{i}.c")),
                FileMetadata {
                    mtime: SystemTime::now(),
                    size: 100,
                    confidence: Confidence::High,
                    last_verified: Instant::now(),
                    content_hash: None,
                },
            );
        }
        // Track in journal.
        let paths: Vec<PathBuf> = (0..50)
            .map(|i| PathBuf::from(format!("/tmp/meta{i}.c")))
            .collect();
        cs.apply_changes(paths);

        let budget = 1000; // Very small budget.
        let (freed, items) = evict_to_budget(budget, &cs, &dg, &fh, &art, 0);
        assert!(freed > 0);
        assert!(items > 0);
        // Metadata should be reduced.
        assert!(cs.metadata().len() < 50);
    }

    #[test]
    fn evict_cascades_to_depgraph() {
        let (cs, dg, fh, art) = empty_caches();

        // Add depgraph contexts.
        for i in 0..20 {
            dg.register(make_ctx(&format!("/tmp/src{i}.c")));
        }

        // Also add metadata so metadata eviction happens first.
        for i in 0..10 {
            cs.metadata().insert(
                PathBuf::from(format!("/tmp/m{i}.c")),
                FileMetadata {
                    mtime: SystemTime::now(),
                    size: 100,
                    confidence: Confidence::High,
                    last_verified: Instant::now(),
                    content_hash: None,
                },
            );
        }

        let budget = 1000; // Very small budget.
        let (freed, items) = evict_to_budget(budget, &cs, &dg, &fh, &art, 0);
        assert!(freed > 0);
        assert!(items > 0);
        // Depgraph contexts should be cleared (trim(ZERO) removes all).
        assert_eq!(dg.stats().context_count, 0);
    }

    #[test]
    fn snapshot_includes_in_flight_bytes() {
        let (cs, dg, fh, art) = empty_caches();
        let snap = memory_snapshot(&cs, &dg, &fh, &art, 500_000);
        assert_eq!(snap.in_flight_bytes, 500_000);
        assert_eq!(snap.total_bytes, 500_000);
    }

    #[test]
    fn in_flight_bytes_push_over_budget_triggers_eviction() {
        let (cs, dg, fh, art) = empty_caches();

        // Add fast-hit entries worth 100 * 200 = 20_000 bytes estimated.
        for i in 0..100 {
            fh.insert(
                make_context_key(&format!("/tmp/inflight{i}.c")),
                FastHitEntry {
                    clock: zccache_fscache::Clock::ZERO,
                    artifact_key_hex: String::new(),
                    cached_at: Instant::now(),
                },
            );
        }

        // Budget of 100_000 — fast-hit alone (20_000) fits fine.
        let (freed, items) = evict_to_budget(100_000, &cs, &dg, &fh, &art, 0);
        assert_eq!(freed, 0);
        assert_eq!(items, 0);

        // Now add 90_000 in-flight bytes → total = 110_000 > 100_000 budget.
        let (freed, items) = evict_to_budget(100_000, &cs, &dg, &fh, &art, 90_000);
        assert!(freed > 0);
        assert!(items > 0);
        // Fast-hit entries should have been evicted to bring total under budget.
        assert!(fh.is_empty());
    }

    #[test]
    fn in_flight_bytes_alone_over_budget_evicts_nothing_when_caches_empty() {
        let (cs, dg, fh, art) = empty_caches();
        // In-flight bytes exceed budget but there's nothing to evict.
        let (freed, items) = evict_to_budget(1000, &cs, &dg, &fh, &art, 50_000);
        assert_eq!(freed, 0);
        assert_eq!(items, 0);
    }
}
