//! Tests for the issue #784 phase 2d deferred-load path on the
//! `ArtifactStore` index blob. Mirrors the compiler-hash phase-2a
//! tests, the metadata phase-2b tests, and the system-includes
//! phase-2c tests.

use super::super::*;

/// `bind_with_cache_dir` no longer reads the on-disk `index.bin` blob.
/// The store starts empty regardless of what is on disk; the daemon
/// binary's `artifact_store_loader().load_and_install()` does the merge
/// after the readiness lockfile is written.
#[tokio::test]
async fn bind_does_not_load_artifact_index_from_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = crate::core::NormalizedPath::new(tmp.path());
    let index_path = crate::core::config::index_path_from_cache_dir(&cache_dir);

    // Pre-write an index blob with synthetic entries.
    let pre = crate::artifact::ArtifactStore::open(index_path.as_path()).unwrap();
    pre.insert("key-a", &synthetic_index_entry(7));
    pre.insert("key-b", &synthetic_index_entry(11));
    pre.flush().unwrap();
    assert!(index_path.as_path().exists());

    // Bind — must NOT read the blob.
    let endpoint = crate::ipc::unique_test_endpoint();
    let server = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).unwrap();
    let state = server.test_state();
    assert!(
        state.artifact_store.get("key-a").is_none(),
        "bind must start with an empty artifact store (no key-a)",
    );
    assert!(
        state.artifact_store.get("key-b").is_none(),
        "bind must start with an empty artifact store (no key-b)",
    );
    assert!(
        !state.artifact_store_loaded.load(Ordering::Acquire),
        "the loaded flag must start false until the background loader runs",
    );
}

/// The `artifact_store_loader()` handle reads the blob and merges its
/// entries into the live store. Confirms the deferred-load path
/// reaches functional parity with the old sync-in-bind path.
#[tokio::test]
async fn artifact_store_loader_merges_index_into_live_store() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = crate::core::NormalizedPath::new(tmp.path());
    let index_path = crate::core::config::index_path_from_cache_dir(&cache_dir);

    let pre = crate::artifact::ArtifactStore::open(index_path.as_path()).unwrap();
    pre.insert("hot-key", &synthetic_index_entry(0x42));
    pre.flush().unwrap();

    let endpoint = crate::ipc::unique_test_endpoint();
    let server = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).unwrap();

    server.artifact_store_loader().load_and_install();

    let state = server.test_state();
    let entry = state
        .artifact_store
        .get("hot-key")
        .expect("loader must merge the persisted entry into the live store");
    assert_eq!(entry.total_size, 0x42);
    assert!(
        state.artifact_store_loaded.load(Ordering::Acquire),
        "loader must set artifact_store_loaded=true",
    );
}

/// Missing on-disk blob is not an error: the loader is a no-op and
/// still flips the loaded flag.
#[tokio::test]
async fn artifact_store_loader_tolerates_missing_index_file() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = crate::core::NormalizedPath::new(tmp.path());
    let endpoint = crate::ipc::unique_test_endpoint();
    let server = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).unwrap();

    server.artifact_store_loader().load_and_install();

    let state = server.test_state();
    assert!(state.artifact_store.get("any").is_none());
    assert!(
        state.artifact_store_loaded.load(Ordering::Acquire),
        "loaded flag flips even when the blob was absent",
    );
}

/// `ArtifactStore::open_empty` produces a usable store rooted at the
/// given path without touching disk. Documents the contract the
/// deferred bind relies on.
#[test]
fn artifact_store_open_empty_does_not_touch_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let missing_path = tmp.path().join("does-not-exist").join("index.bin");

    // No parent dir, no file. `open()` would fail or error-log; we
    // expect `open_empty` to succeed without I/O.
    let store = crate::artifact::ArtifactStore::open_empty(&missing_path);

    assert!(store.get("anything").is_none());
    assert!(
        !missing_path.exists(),
        "open_empty must not create the index file",
    );
}

fn synthetic_index_entry(total_size: u64) -> crate::artifact::ArtifactIndex {
    use std::sync::Arc;
    crate::artifact::ArtifactIndex {
        output_names: Arc::from(vec!["foo.o".to_string()]),
        output_sizes: vec![total_size],
        stdout: Arc::new(Vec::new()),
        stderr: Arc::new(Vec::new()),
        exit_code: 0,
        total_size,
        stored_at_secs: 0,
    }
}
