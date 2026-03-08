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

/// Platform-specific file identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum FileId {
    /// Unix inode (device, inode).
    Unix { dev: u64, ino: u64 },
    /// Windows file index.
    Windows { volume_serial: u32, file_index: u64 },
    /// Fallback when file identity is unavailable.
    Unknown,
}

/// Cached metadata for a single file.
#[derive(Debug, Clone)]
pub struct FileMetadata {
    /// File modification time.
    pub mtime: SystemTime,
    /// File size in bytes.
    pub size: u64,
    /// File identity (inode/file index).
    pub file_id: FileId,
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
            file_id: FileId::Unknown,
            confidence: Confidence::High,
            last_verified: Instant::now(),
            content_hash: None,
        };
        cache.insert(path.clone(), meta);
        assert!(cache.get(&path).is_some());
    }

    #[test]
    fn downgrade_all_works() {
        let cache = MetadataCache::new();
        for i in 0..10 {
            let path = PathBuf::from(format!("/tmp/test{i}.c"));
            let meta = FileMetadata {
                mtime: SystemTime::now(),
                size: 100,
                file_id: FileId::Unknown,
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
