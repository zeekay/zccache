//! File metadata types and cache implementation.

use dashmap::DashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// Confidence level for a cached metadata entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Confidence {
    /// Entry has not been verified recently. Must stat before trusting.
    Low,
    /// Watcher indicates no changes since last verification.
    Medium,
    /// Recently stat-verified or content-hash verified.
    High,
}

/// Cached metadata for a single file.
///
/// Change detection uses `(mtime, size)`. File replacement detection
/// (same path, new inode) is handled by the file watcher (`notify`),
/// not by platform-specific file identity checks.
#[derive(Debug, Clone)]
pub struct FileMetadata {
    /// File modification time.
    pub mtime: SystemTime,
    /// File size in bytes.
    pub size: u64,
    /// Confidence level of this entry.
    pub confidence: Confidence,
    /// When this entry was last verified via stat.
    pub last_verified: Instant,
    /// Cached content hash (blake3, 32 bytes), if computed.
    pub content_hash: Option<[u8; 32]>,
}

/// Concurrent file metadata cache.
///
/// Uses `DashMap` for sharded, concurrent access. Entries are keyed
/// by normalized path.
#[derive(Debug)]
pub struct MetadataCache {
    entries: DashMap<PathBuf, FileMetadata>,
    /// Duration after which a High-confidence entry decays to Medium.
    high_decay: Duration,
    /// Duration after which a Medium-confidence entry decays to Low.
    medium_decay: Duration,
}

impl MetadataCache {
    /// Create a new metadata cache with default decay durations.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: DashMap::new(),
            high_decay: Duration::from_secs(5),
            medium_decay: Duration::from_secs(30),
        }
    }

    /// Look up metadata for a path, applying confidence decay.
    ///
    /// Returns `None` if the path is not in the cache.
    #[must_use]
    pub fn get(&self, path: &Path) -> Option<FileMetadata> {
        self.entries.get(path).map(|entry| {
            let mut meta = entry.clone();
            meta.confidence = self.decayed_confidence(&meta);
            meta
        })
    }

    /// Insert or update metadata for a path.
    pub fn insert(&self, path: PathBuf, metadata: FileMetadata) {
        self.entries.insert(path, metadata);
    }

    /// Mark a path's entry as Low confidence (e.g., after watcher overflow).
    pub fn downgrade(&self, path: &Path) {
        if let Some(mut entry) = self.entries.get_mut(path) {
            entry.confidence = Confidence::Low;
        }
    }

    /// Downgrade all entries to Low confidence.
    pub fn downgrade_all(&self) {
        for mut entry in self.entries.iter_mut() {
            entry.confidence = Confidence::Low;
        }
    }

    /// Remove a path from the cache.
    pub fn remove(&self, path: &Path) {
        self.entries.remove(path);
    }

    /// Returns the number of entries in the cache.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns whether the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Return a cached content hash if the entry has High or Medium confidence.
    ///
    /// This is the clock-aware fast path: the caller has journal-based
    /// assurance that the file hasn't changed, so time-based decay is
    /// not applied. Returns `None` if the entry is missing, Low confidence,
    /// or has no cached hash.
    #[must_use]
    pub fn get_cached_hash(&self, path: &Path) -> Option<zccache_hash::ContentHash> {
        self.entries
            .get(path)
            .and_then(|entry| match entry.confidence {
                Confidence::High | Confidence::Medium => entry
                    .content_hash
                    .map(zccache_hash::ContentHash::from_bytes),
                Confidence::Low => None,
            })
    }

    fn decayed_confidence(&self, meta: &FileMetadata) -> Confidence {
        let elapsed = meta.last_verified.elapsed();
        match meta.confidence {
            Confidence::High if elapsed > self.high_decay => Confidence::Medium,
            Confidence::Medium if elapsed > self.medium_decay => Confidence::Low,
            other => other,
        }
    }
}

impl Default for MetadataCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_get() {
        let cache = MetadataCache::new();
        let path = PathBuf::from("/tmp/test.c");
        let meta = FileMetadata {
            mtime: SystemTime::now(),
            size: 100,
            confidence: Confidence::High,
            last_verified: Instant::now(),
            content_hash: None,
        };
        cache.insert(path.clone(), meta);
        assert!(cache.get(&path).is_some());
    }

    #[test]
    fn get_returns_none_for_missing_path() {
        let cache = MetadataCache::new();
        assert!(cache.get(Path::new("/no/such/path")).is_none());
    }

    #[test]
    fn insert_overwrites_existing() {
        let cache = MetadataCache::new();
        let path = PathBuf::from("/tmp/overwrite.c");

        let meta1 = FileMetadata {
            mtime: SystemTime::now(),
            size: 100,
            confidence: Confidence::High,
            last_verified: Instant::now(),
            content_hash: None,
        };
        cache.insert(path.clone(), meta1);
        assert_eq!(cache.get(&path).unwrap().size, 100);

        let meta2 = FileMetadata {
            mtime: SystemTime::now(),
            size: 999,
            confidence: Confidence::Medium,
            last_verified: Instant::now(),
            content_hash: None,
        };
        cache.insert(path.clone(), meta2);
        assert_eq!(cache.get(&path).unwrap().size, 999);
    }

    #[test]
    fn downgrade_single_path() {
        let cache = MetadataCache::new();
        let path_a = PathBuf::from("/tmp/a.c");
        let path_b = PathBuf::from("/tmp/b.c");

        for path in [&path_a, &path_b] {
            cache.insert(
                path.clone(),
                FileMetadata {
                    mtime: SystemTime::now(),
                    size: 10,
                    confidence: Confidence::High,
                    last_verified: Instant::now(),
                    content_hash: None,
                },
            );
        }

        cache.downgrade(&path_a);
        assert_eq!(cache.get(&path_a).unwrap().confidence, Confidence::Low);
        assert_eq!(cache.get(&path_b).unwrap().confidence, Confidence::High);
    }

    #[test]
    fn downgrade_nonexistent_is_noop() {
        let cache = MetadataCache::new();
        cache.downgrade(Path::new("/no/such/path")); // should not panic
    }

    #[test]
    fn remove_entry() {
        let cache = MetadataCache::new();
        let path = PathBuf::from("/tmp/removable.c");
        cache.insert(
            path.clone(),
            FileMetadata {
                mtime: SystemTime::now(),
                size: 10,
                confidence: Confidence::High,
                last_verified: Instant::now(),
                content_hash: None,
            },
        );
        assert_eq!(cache.len(), 1);

        cache.remove(&path);
        assert!(cache.get(&path).is_none());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn remove_nonexistent_is_noop() {
        let cache = MetadataCache::new();
        cache.remove(Path::new("/no/such/path")); // should not panic
    }

    #[test]
    fn is_empty_and_len() {
        let cache = MetadataCache::new();
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);

        cache.insert(
            PathBuf::from("/tmp/x.c"),
            FileMetadata {
                mtime: SystemTime::now(),
                size: 1,
                confidence: Confidence::High,
                last_verified: Instant::now(),
                content_hash: None,
            },
        );
        assert!(!cache.is_empty());
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn get_cached_hash_high_confidence() {
        let cache = MetadataCache::new();
        let path = PathBuf::from("/tmp/hashed.c");
        let hash_bytes = [42u8; 32];
        cache.insert(
            path.clone(),
            FileMetadata {
                mtime: SystemTime::now(),
                size: 10,
                confidence: Confidence::High,
                last_verified: Instant::now(),
                content_hash: Some(hash_bytes),
            },
        );

        let result = cache.get_cached_hash(&path);
        assert!(result.is_some());
        assert_eq!(*result.unwrap().as_bytes(), hash_bytes);
    }

    #[test]
    fn get_cached_hash_medium_confidence() {
        let cache = MetadataCache::new();
        let path = PathBuf::from("/tmp/med.c");
        let hash_bytes = [7u8; 32];
        cache.insert(
            path.clone(),
            FileMetadata {
                mtime: SystemTime::now(),
                size: 10,
                confidence: Confidence::Medium,
                last_verified: Instant::now(),
                content_hash: Some(hash_bytes),
            },
        );

        let result = cache.get_cached_hash(&path);
        assert!(result.is_some());
    }

    #[test]
    fn get_cached_hash_low_confidence_returns_none() {
        let cache = MetadataCache::new();
        let path = PathBuf::from("/tmp/low.c");
        cache.insert(
            path.clone(),
            FileMetadata {
                mtime: SystemTime::now(),
                size: 10,
                confidence: Confidence::Low,
                last_verified: Instant::now(),
                content_hash: Some([1u8; 32]),
            },
        );

        assert!(cache.get_cached_hash(&path).is_none());
    }

    #[test]
    fn get_cached_hash_no_hash_returns_none() {
        let cache = MetadataCache::new();
        let path = PathBuf::from("/tmp/nohash.c");
        cache.insert(
            path.clone(),
            FileMetadata {
                mtime: SystemTime::now(),
                size: 10,
                confidence: Confidence::High,
                last_verified: Instant::now(),
                content_hash: None,
            },
        );

        assert!(cache.get_cached_hash(&path).is_none());
    }

    #[test]
    fn get_cached_hash_missing_path_returns_none() {
        let cache = MetadataCache::new();
        assert!(cache.get_cached_hash(Path::new("/no/such")).is_none());
    }

    #[test]
    fn confidence_ordering() {
        assert!(Confidence::Low < Confidence::Medium);
        assert!(Confidence::Medium < Confidence::High);
        assert!(Confidence::Low < Confidence::High);
    }

    #[test]
    fn default_creates_new_cache() {
        let cache = MetadataCache::default();
        assert!(cache.is_empty());
    }

    #[test]
    fn downgrade_all_works() {
        let cache = MetadataCache::new();
        for i in 0..10 {
            let path = PathBuf::from(format!("/tmp/test{i}.c"));
            let meta = FileMetadata {
                mtime: SystemTime::now(),
                size: 100,
                confidence: Confidence::High,
                last_verified: Instant::now(),
                content_hash: None,
            };
            cache.insert(path, meta);
        }
        cache.downgrade_all();
        // After downgrade, all entries should be Low confidence
        for entry in cache.entries.iter() {
            assert_eq!(entry.confidence, Confidence::Low);
        }
    }
}
