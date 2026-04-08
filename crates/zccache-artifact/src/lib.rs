//! Disk artifact cache for zccache.
//!
//! Provides content-addressed storage for compilation artifacts,
//! backed by the filesystem with a redb index for metadata and eviction.

#![allow(clippy::missing_errors_doc)] // TODO: add error docs

mod store;

pub use store::{ArtifactIndex, ArtifactStore};

use std::path::Path;
use zccache_core::NormalizedPath;

/// Configuration for the artifact store.
#[derive(Debug, Clone)]
pub struct ArtifactStoreConfig {
    /// Root directory for artifact storage.
    pub cache_dir: NormalizedPath,
    /// Maximum total cache size in bytes.
    pub max_size: u64,
}

/// The artifact store manages cached compilation outputs on disk.
///
/// Artifacts are stored in a content-addressed directory layout:
/// `{cache_dir}/artifacts/{hash[0..2]}/{hash[2..4]}/{hash}`
///
/// A redb database at `{cache_dir}/index.redb` tracks metadata
/// and access times for eviction.
pub struct ArtifactStoreLegacy {
    config: ArtifactStoreConfig,
}

impl ArtifactStoreLegacy {
    /// Open or create an artifact store at the given configuration.
    ///
    /// Creates the cache directory if it does not exist.
    pub fn open(config: ArtifactStoreConfig) -> zccache_core::Result<Self> {
        std::fs::create_dir_all(&config.cache_dir)?;
        Ok(Self { config })
    }

    /// Returns the path where an artifact with the given key would be stored.
    #[must_use]
    pub fn artifact_path(&self, key: &zccache_hash::ContentHash) -> NormalizedPath {
        let shards = key.shard_prefix(2, 1);
        self.config
            .cache_dir
            .join("artifacts")
            .join(&shards[0])
            .join(&shards[1])
            .join(key.to_hex())
    }

    /// Check if an artifact exists for the given cache key.
    #[must_use]
    pub fn contains(&self, key: &zccache_hash::ContentHash) -> bool {
        self.artifact_path(key).exists()
    }

    /// Returns the configured cache directory.
    #[must_use]
    pub fn cache_dir(&self) -> &Path {
        &self.config.cache_dir
    }
}
