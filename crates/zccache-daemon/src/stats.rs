//! Global daemon statistics collector.
//!
//! All counters are atomic — safe to update from concurrent connection handlers.

use std::sync::atomic::{AtomicU64, Ordering};

/// Global statistics collected since daemon startup.
pub struct StatsCollector {
    /// Total compile requests received.
    compilations: AtomicU64,
    /// Cache hits.
    hits: AtomicU64,
    /// Cache misses (cold compiles).
    misses: AtomicU64,
    /// Non-cacheable invocations.
    non_cacheable: AtomicU64,
    /// Compilations that exited non-zero.
    compile_errors: AtomicU64,
    /// Total sessions created.
    sessions_total: AtomicU64,
    /// Cumulative microseconds spent on cache hits.
    hit_time_us: AtomicU64,
    /// Cumulative microseconds spent on cache misses.
    miss_time_us: AtomicU64,
    /// Total artifact bytes served from cache.
    bytes_read: AtomicU64,
    /// Total artifact bytes stored into cache.
    bytes_written: AtomicU64,
}

/// Snapshot of current stats values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatsSnapshot {
    pub compilations: u64,
    pub hits: u64,
    pub misses: u64,
    pub non_cacheable: u64,
    pub compile_errors: u64,
    pub sessions_total: u64,
    pub hit_time_us: u64,
    pub miss_time_us: u64,
    pub bytes_read: u64,
    pub bytes_written: u64,
}

impl StatsSnapshot {
    /// Estimated time saved: difference between average miss time and average hit time,
    /// multiplied by number of hits. Returns 0 if no data.
    #[must_use]
    pub fn time_saved_ms(&self) -> u64 {
        if self.hits == 0 || self.misses == 0 {
            return 0;
        }
        let avg_hit_us = self.hit_time_us / self.hits;
        let avg_miss_us = self.miss_time_us / self.misses;
        let saved_per_hit_us = avg_miss_us.saturating_sub(avg_hit_us);
        (self.hits * saved_per_hit_us) / 1000
    }
}

impl StatsCollector {
    /// Create a new collector with all counters at zero.
    #[must_use]
    pub fn new() -> Self {
        Self {
            compilations: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            non_cacheable: AtomicU64::new(0),
            compile_errors: AtomicU64::new(0),
            sessions_total: AtomicU64::new(0),
            hit_time_us: AtomicU64::new(0),
            miss_time_us: AtomicU64::new(0),
            bytes_read: AtomicU64::new(0),
            bytes_written: AtomicU64::new(0),
        }
    }

    /// Record a compilation request (always called, regardless of outcome).
    pub fn record_compilation(&self) {
        self.compilations.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a cache hit with its latency.
    pub fn record_hit(&self, latency_us: u64, artifact_bytes: u64) {
        self.hits.fetch_add(1, Ordering::Relaxed);
        self.hit_time_us.fetch_add(latency_us, Ordering::Relaxed);
        self.bytes_read.fetch_add(artifact_bytes, Ordering::Relaxed);
    }

    /// Record a cache miss with its latency.
    pub fn record_miss(&self, latency_us: u64, artifact_bytes: u64) {
        self.misses.fetch_add(1, Ordering::Relaxed);
        self.miss_time_us.fetch_add(latency_us, Ordering::Relaxed);
        self.bytes_written
            .fetch_add(artifact_bytes, Ordering::Relaxed);
    }

    /// Record a non-cacheable invocation.
    pub fn record_non_cacheable(&self) {
        self.non_cacheable.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a compile error (non-zero exit).
    pub fn record_error(&self) {
        self.compile_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a new session creation.
    pub fn record_session(&self) {
        self.sessions_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Take a consistent snapshot of all counters.
    #[must_use]
    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            compilations: self.compilations.load(Ordering::Relaxed),
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            non_cacheable: self.non_cacheable.load(Ordering::Relaxed),
            compile_errors: self.compile_errors.load(Ordering::Relaxed),
            sessions_total: self.sessions_total.load(Ordering::Relaxed),
            hit_time_us: self.hit_time_us.load(Ordering::Relaxed),
            miss_time_us: self.miss_time_us.load(Ordering::Relaxed),
            bytes_read: self.bytes_read.load(Ordering::Relaxed),
            bytes_written: self.bytes_written.load(Ordering::Relaxed),
        }
    }
}

impl Default for StatsCollector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_collector_is_zero() {
        let c = StatsCollector::new();
        let s = c.snapshot();
        assert_eq!(s.compilations, 0);
        assert_eq!(s.hits, 0);
        assert_eq!(s.misses, 0);
        assert_eq!(s.non_cacheable, 0);
        assert_eq!(s.compile_errors, 0);
        assert_eq!(s.sessions_total, 0);
        assert_eq!(s.bytes_read, 0);
        assert_eq!(s.bytes_written, 0);
    }

    #[test]
    fn record_hit_increments() {
        let c = StatsCollector::new();
        c.record_compilation();
        c.record_hit(500, 1024);
        let s = c.snapshot();
        assert_eq!(s.compilations, 1);
        assert_eq!(s.hits, 1);
        assert_eq!(s.hit_time_us, 500);
        assert_eq!(s.bytes_read, 1024);
    }

    #[test]
    fn record_miss_increments() {
        let c = StatsCollector::new();
        c.record_compilation();
        c.record_miss(50_000, 2048);
        let s = c.snapshot();
        assert_eq!(s.compilations, 1);
        assert_eq!(s.misses, 1);
        assert_eq!(s.miss_time_us, 50_000);
        assert_eq!(s.bytes_written, 2048);
    }

    #[test]
    fn record_non_cacheable_and_error() {
        let c = StatsCollector::new();
        c.record_non_cacheable();
        c.record_non_cacheable();
        c.record_error();
        let s = c.snapshot();
        assert_eq!(s.non_cacheable, 2);
        assert_eq!(s.compile_errors, 1);
    }

    #[test]
    fn record_session_increments() {
        let c = StatsCollector::new();
        c.record_session();
        c.record_session();
        c.record_session();
        assert_eq!(c.snapshot().sessions_total, 3);
    }

    #[test]
    fn time_saved_ms_calculation() {
        let c = StatsCollector::new();
        // 10 hits at 1ms each, 5 misses at 100ms each
        for _ in 0..10 {
            c.record_hit(1_000, 0);
        }
        for _ in 0..5 {
            c.record_miss(100_000, 0);
        }
        let s = c.snapshot();
        // avg_hit = 1000us, avg_miss = 100000us, saved_per_hit = 99000us
        // total_saved = 10 * 99000 / 1000 = 990ms
        assert_eq!(s.time_saved_ms(), 990);
    }

    #[test]
    fn time_saved_ms_no_data() {
        let c = StatsCollector::new();
        assert_eq!(c.snapshot().time_saved_ms(), 0);

        // Only hits, no misses — can't estimate
        c.record_hit(500, 0);
        assert_eq!(c.snapshot().time_saved_ms(), 0);
    }

    #[test]
    fn concurrent_increments() {
        use std::sync::Arc;
        use std::thread;

        let c = Arc::new(StatsCollector::new());
        let mut handles = Vec::new();

        for _ in 0..4 {
            let c = Arc::clone(&c);
            handles.push(thread::spawn(move || {
                for _ in 0..250 {
                    c.record_compilation();
                    c.record_hit(100, 10);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        let s = c.snapshot();
        assert_eq!(s.compilations, 1000);
        assert_eq!(s.hits, 1000);
        assert_eq!(s.hit_time_us, 100_000);
        assert_eq!(s.bytes_read, 10_000);
    }
}
