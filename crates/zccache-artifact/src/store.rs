//! redb-backed artifact index.
//!
//! Stores lightweight metadata (`ArtifactIndex`) for each cached artifact.
//! Output payloads live on disk as `{key}_0`, `{key}_1`, ... files and are
//! loaded lazily on cache hit.

use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};
use serde::{Deserialize, Serialize};

const ARTIFACTS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("artifacts");

/// Lightweight metadata stored in the redb index for each cached artifact.
///
/// Contains everything needed to serve a cache hit response EXCEPT the output
/// file bytes (which are loaded lazily from `{key}_0` files on disk).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactIndex {
    /// Output filenames, e.g. `["foo.o"]`.
    /// Arc-wrapped so cache-hit clones are O(1) refcount bumps.
    pub output_names: Arc<[String]>,
    /// Output file sizes in bytes (parallel to `output_names`).
    pub output_sizes: Vec<u64>,
    /// Captured compiler stdout.
    /// Arc-wrapped so clones between CachedArtifact and persist tasks are O(1).
    pub stdout: Arc<Vec<u8>>,
    /// Captured compiler stderr.
    /// Arc-wrapped so clones between CachedArtifact and persist tasks are O(1).
    pub stderr: Arc<Vec<u8>>,
    /// Compiler exit code.
    pub exit_code: i32,
    /// Sum of all output file sizes (for eviction budget accounting).
    pub total_size: u64,
    /// Unix epoch seconds when this artifact was stored.
    pub stored_at_secs: u64,
}

impl ArtifactIndex {
    /// Create an `ArtifactIndex` from output names, sizes, and compiler results.
    pub fn new(
        output_names: Vec<String>,
        output_sizes: Vec<u64>,
        stdout: impl Into<Arc<Vec<u8>>>,
        stderr: impl Into<Arc<Vec<u8>>>,
        exit_code: i32,
    ) -> Self {
        let total_size = output_sizes.iter().sum();
        let stored_at_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            output_names: Arc::from(output_names),
            output_sizes,
            stdout: stdout.into(),
            stderr: stderr.into(),
            exit_code,
            total_size,
            stored_at_secs,
        }
    }
}

/// redb-backed artifact index.
///
/// Maps `artifact_key_hex` → `ArtifactIndex` (bincode-serialized).
/// Single-file database at `~/.zccache/index.redb`.
pub struct ArtifactStore {
    db: Database,
}

#[allow(clippy::result_large_err)]
impl ArtifactStore {
    /// Open or create the artifact index at the given path.
    pub fn open(path: &Path) -> Result<Self, redb::Error> {
        let db = Database::create(path)?;
        // Ensure table exists.
        let txn = db.begin_write()?;
        {
            let _table = txn.open_table(ARTIFACTS_TABLE)?;
        }
        txn.commit()?;
        Ok(Self { db })
    }

    /// Insert or update an artifact entry.
    pub fn insert(&self, key: &str, meta: &ArtifactIndex) -> Result<(), redb::Error> {
        let data =
            bincode::serialize(meta).map_err(|e| redb::Error::Io(std::io::Error::other(e)))?;
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(ARTIFACTS_TABLE)?;
            table.insert(key, data.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Look up a single artifact entry.
    pub fn get(&self, key: &str) -> Result<Option<ArtifactIndex>, redb::Error> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(ARTIFACTS_TABLE)?;
        match table.get(key)? {
            Some(value) => {
                let meta: ArtifactIndex = bincode::deserialize(value.value())
                    .map_err(|e| redb::Error::Io(std::io::Error::other(e)))?;
                Ok(Some(meta))
            }
            None => Ok(None),
        }
    }

    /// Remove a single artifact entry. Returns `true` if it existed.
    pub fn remove(&self, key: &str) -> Result<bool, redb::Error> {
        let txn = self.db.begin_write()?;
        let existed;
        {
            let mut table = txn.open_table(ARTIFACTS_TABLE)?;
            existed = table.remove(key)?.is_some();
        }
        txn.commit()?;
        Ok(existed)
    }

    /// Remove a batch of entries in a single write transaction.
    /// Returns the number of entries actually removed.
    pub fn remove_batch(&self, keys: &[&str]) -> Result<usize, redb::Error> {
        if keys.is_empty() {
            return Ok(0);
        }
        let txn = self.db.begin_write()?;
        let mut removed = 0usize;
        {
            let mut table = txn.open_table(ARTIFACTS_TABLE)?;
            for key in keys {
                if table.remove(*key)?.is_some() {
                    removed += 1;
                }
            }
        }
        txn.commit()?;
        Ok(removed)
    }

    /// Load all entries from the index.
    pub fn load_all(&self) -> Result<Vec<(String, ArtifactIndex)>, redb::Error> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(ARTIFACTS_TABLE)?;
        let mut result = Vec::new();
        for entry in table.iter()? {
            let (key, value) = entry?;
            if let Ok(meta) = bincode::deserialize::<ArtifactIndex>(value.value()) {
                result.push((key.value().to_string(), meta));
            }
        }
        Ok(result)
    }

    /// Return the number of entries in the index.
    pub fn len(&self) -> Result<usize, redb::Error> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(ARTIFACTS_TABLE)?;
        Ok(table.len()? as usize)
    }

    /// Return whether the index is empty.
    pub fn is_empty(&self) -> Result<bool, redb::Error> {
        Ok(self.len()? == 0)
    }

    /// Remove all entries from the index.
    pub fn clear(&self) -> Result<usize, redb::Error> {
        let txn = self.db.begin_write()?;
        let removed;
        {
            let mut table = txn.open_table(ARTIFACTS_TABLE)?;
            removed = table.len()? as usize;
            // Drain all entries.
            table.retain(|_, _| false)?;
        }
        txn.commit()?;
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (tempfile::TempDir, ArtifactStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = ArtifactStore::open(&dir.path().join("index.redb")).unwrap();
        (dir, store)
    }

    fn sample_meta() -> ArtifactIndex {
        ArtifactIndex::new(
            vec!["foo.o".to_string()],
            vec![1234],
            b"compiler stdout".to_vec(),
            b"compiler stderr".to_vec(),
            0,
        )
    }

    #[test]
    fn open_creates_database() {
        let (_dir, store) = temp_store();
        assert_eq!(store.len().unwrap(), 0);
        assert!(store.is_empty().unwrap());
    }

    #[test]
    fn insert_and_get() {
        let (_dir, store) = temp_store();
        let meta = sample_meta();
        store.insert("abc123", &meta).unwrap();
        let loaded = store.get("abc123").unwrap().unwrap();
        assert_eq!(&*loaded.output_names, &["foo.o".to_string()]);
        assert_eq!(loaded.output_sizes, vec![1234]);
        assert_eq!(&*loaded.stdout, b"compiler stdout");
        assert_eq!(&*loaded.stderr, b"compiler stderr");
        assert_eq!(loaded.exit_code, 0);
        assert_eq!(loaded.total_size, 1234);
    }

    #[test]
    fn get_missing_returns_none() {
        let (_dir, store) = temp_store();
        assert!(store.get("nonexistent").unwrap().is_none());
    }

    #[test]
    fn insert_overwrites() {
        let (_dir, store) = temp_store();
        let meta1 = ArtifactIndex::new(vec!["a.o".into()], vec![100], vec![], vec![], 0);
        let meta2 = ArtifactIndex::new(vec!["b.o".into()], vec![200], vec![], vec![], 1);
        store.insert("key", &meta1).unwrap();
        store.insert("key", &meta2).unwrap();
        assert_eq!(store.len().unwrap(), 1);
        let loaded = store.get("key").unwrap().unwrap();
        assert_eq!(&*loaded.output_names, &["b.o".to_string()]);
        assert_eq!(loaded.exit_code, 1);
    }

    #[test]
    fn remove_existing() {
        let (_dir, store) = temp_store();
        store.insert("key1", &sample_meta()).unwrap();
        assert!(store.remove("key1").unwrap());
        assert!(store.get("key1").unwrap().is_none());
        assert_eq!(store.len().unwrap(), 0);
    }

    #[test]
    fn remove_missing() {
        let (_dir, store) = temp_store();
        assert!(!store.remove("nope").unwrap());
    }

    #[test]
    fn remove_batch_multiple() {
        let (_dir, store) = temp_store();
        for i in 0..5 {
            store.insert(&format!("k{i}"), &sample_meta()).unwrap();
        }
        assert_eq!(store.len().unwrap(), 5);
        let removed = store.remove_batch(&["k0", "k2", "k4", "missing"]).unwrap();
        assert_eq!(removed, 3);
        assert_eq!(store.len().unwrap(), 2);
        assert!(store.get("k1").unwrap().is_some());
        assert!(store.get("k3").unwrap().is_some());
    }

    #[test]
    fn remove_batch_empty() {
        let (_dir, store) = temp_store();
        assert_eq!(store.remove_batch(&[]).unwrap(), 0);
    }

    #[test]
    fn load_all_round_trip() {
        let (_dir, store) = temp_store();
        let keys = ["aaa", "bbb", "ccc"];
        for key in &keys {
            store.insert(key, &sample_meta()).unwrap();
        }
        let all = store.load_all().unwrap();
        assert_eq!(all.len(), 3);
        let loaded_keys: Vec<&str> = all.iter().map(|(k, _): &(String, _)| k.as_str()).collect();
        for key in &keys {
            assert!(loaded_keys.contains(key));
        }
    }

    #[test]
    fn load_all_empty() {
        let (_dir, store) = temp_store();
        assert!(store.load_all().unwrap().is_empty());
    }

    #[test]
    fn clear_removes_all() {
        let (_dir, store) = temp_store();
        for i in 0..10 {
            store.insert(&format!("k{i}"), &sample_meta()).unwrap();
        }
        assert_eq!(store.len().unwrap(), 10);
        let removed = store.clear().unwrap();
        assert_eq!(removed, 10);
        assert!(store.is_empty().unwrap());
    }

    #[test]
    fn multiple_outputs() {
        let (_dir, store) = temp_store();
        let meta = ArtifactIndex::new(
            vec!["main.o".into(), "main.d".into()],
            vec![50000, 2000],
            vec![],
            b"warning: unused variable".to_vec(),
            0,
        );
        store.insert("multi", &meta).unwrap();
        let loaded = store.get("multi").unwrap().unwrap();
        assert_eq!(loaded.output_names.len(), 2);
        assert_eq!(loaded.output_sizes, vec![50000, 2000]);
        assert_eq!(loaded.total_size, 52000);
        assert_eq!(&*loaded.stderr, b"warning: unused variable");
    }

    #[test]
    fn reopen_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.redb");
        {
            let store = ArtifactStore::open(&path).unwrap();
            store.insert("persist_test", &sample_meta()).unwrap();
        }
        // Reopen and verify.
        let store = ArtifactStore::open(&path).unwrap();
        assert_eq!(store.len().unwrap(), 1);
        assert!(store.get("persist_test").unwrap().is_some());
    }
}
