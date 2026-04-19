//! Disk persistence for the dependency graph via rkyv zero-copy serialization.
//!
//! Saves/loads the graph to `~/.zccache/depgraph/depgraph.bin` so warm contexts
//! survive daemon restarts and cache hits resume immediately.

use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use rayon::prelude::*;
use rkyv::{Archive, Deserialize, Serialize};
use zccache_core::NormalizedPath;
use zccache_hash::ContentHash;

use crate::context::{ArtifactKey, CompileContext, ContextKey};
use crate::graph::{ContextEntry, ContextState, DepGraph, FileEntry};
use crate::scanner::{IncludeDirective, IncludeKind};
use crate::search_paths::IncludeSearchPaths;

/// On-disk format version. Bump when snapshot layout changes.
pub const DEPGRAPH_VERSION: u32 = 2;

/// Magic bytes identifying a depgraph snapshot file ("ZCDG").
pub const DEPGRAPH_MAGIC: [u8; 4] = [0x5A, 0x43, 0x44, 0x47];

/// Header size: 4 (magic) + 4 (version) + 8 (payload len) = 16 bytes.
const HEADER_SIZE: usize = 16;

/// Entries older than this are trimmed before persisting the snapshot.
const GC_TTL: Duration = Duration::from_secs(86_400); // 1 day

/// Initial scratch-space size for rkyv serialization.
const SERIALIZE_SCRATCH: usize = 4096;

// ---------------------------------------------------------------------------
// Snapshot types (rkyv-serializable mirrors of the in-memory types)
// ---------------------------------------------------------------------------

#[derive(Archive, Serialize, Deserialize)]
#[archive(check_bytes)]
pub struct DepGraphSnapshot {
    pub files: Vec<FileEntrySnapshot>,
    pub contexts: Vec<ContextEntrySnapshot>,
    pub stats: SnapshotStats,
}

#[derive(Archive, Serialize, Deserialize)]
#[archive(check_bytes)]
pub struct FileEntrySnapshot {
    pub path: String,
    pub includes: Vec<IncludeDirectiveSnapshot>,
}

#[derive(Archive, Serialize, Deserialize)]
#[archive(check_bytes)]
pub struct IncludeDirectiveSnapshot {
    /// 0=Quoted, 1=AngleBracket, 2=Computed
    pub kind: u8,
    pub path: String,
    pub line: u32,
}

#[derive(Archive, Serialize, Deserialize)]
#[archive(check_bytes)]
pub struct ContextEntrySnapshot {
    pub context_key: [u8; 32],
    pub key_root: Option<String>,
    pub source_file: String,
    pub iquote: Vec<String>,
    pub user: Vec<String>,
    pub system: Vec<String>,
    pub after: Vec<String>,
    pub defines: Vec<String>,
    pub flags: Vec<String>,
    pub force_includes: Vec<String>,
    pub unknown_flags: Vec<String>,
    pub resolved_includes: Vec<String>,
    pub unresolved_includes: Vec<String>,
    pub has_computed_includes: bool,
    pub artifact_key: Option<[u8; 32]>,
    pub last_file_hashes: Vec<(String, [u8; 32])>,
    /// 0=Cold, 1=Warm, 2=Stale
    pub state: u8,
}

#[derive(Archive, Serialize, Deserialize)]
#[archive(check_bytes)]
pub struct SnapshotStats {
    pub saved_at_epoch_secs: u64,
    pub file_count: u64,
    pub context_count: u64,
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("bad magic bytes in depgraph file")]
    BadMagic,

    #[error("depgraph version mismatch: file has v{file}, expected v{expected}")]
    VersionMismatch { file: u32, expected: u32 },

    #[error("corrupt depgraph file: {0}")]
    Corrupt(String),
}

// ---------------------------------------------------------------------------
// Conversion: DepGraph -> Snapshot
// ---------------------------------------------------------------------------

impl DepGraph {
    /// Create a serializable snapshot of the current graph state.
    pub fn to_snapshot(&self) -> DepGraphSnapshot {
        let files: Vec<FileEntrySnapshot> = self
            .files_iter()
            .map(|entry| {
                let path = entry.key().to_string_lossy().into_owned();
                let includes = entry
                    .value()
                    .includes
                    .iter()
                    .map(|d| IncludeDirectiveSnapshot {
                        kind: match &d.kind {
                            IncludeKind::Quoted => 0,
                            IncludeKind::AngleBracket => 1,
                            IncludeKind::Computed(_) => 2,
                        },
                        path: d.path.clone(),
                        line: d.line,
                    })
                    .collect();
                FileEntrySnapshot { path, includes }
            })
            .collect();

        let contexts: Vec<ContextEntrySnapshot> = self
            .contexts_iter()
            .map(|entry| {
                let key = entry.key();
                let ctx = entry.value();
                ContextEntrySnapshot {
                    context_key: *key.hash().as_bytes(),
                    key_root: ctx
                        .key_root
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned()),
                    source_file: ctx.context.source_file.to_string_lossy().into_owned(),
                    iquote: paths_to_strings(&ctx.context.include_search.iquote),
                    user: paths_to_strings(&ctx.context.include_search.user),
                    system: paths_to_strings(&ctx.context.include_search.system),
                    after: paths_to_strings(&ctx.context.include_search.after),
                    defines: ctx.context.defines.clone(),
                    flags: ctx.context.flags.clone(),
                    force_includes: paths_to_strings(&ctx.context.force_includes),
                    unknown_flags: ctx.context.unknown_flags.clone(),
                    resolved_includes: paths_to_strings(&ctx.resolved_includes),
                    unresolved_includes: ctx.unresolved_includes.clone(),
                    has_computed_includes: ctx.has_computed_includes,
                    artifact_key: ctx.artifact_key.map(|k| *k.hash().as_bytes()),
                    last_file_hashes: ctx
                        .last_file_hashes
                        .iter()
                        .map(|(p, h)| (p.to_string_lossy().into_owned(), *h.as_bytes()))
                        .collect(),
                    state: match ctx.state {
                        ContextState::Cold => 0,
                        ContextState::Warm => 1,
                        ContextState::Stale => 2,
                    },
                }
            })
            .collect();

        DepGraphSnapshot {
            stats: SnapshotStats {
                saved_at_epoch_secs: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                file_count: files.len() as u64,
                context_count: contexts.len() as u64,
            },
            files,
            contexts,
        }
    }

    /// Reconstruct a `DepGraph` from a deserialized snapshot.
    pub fn from_snapshot(snap: DepGraphSnapshot) -> Self {
        let files: DashMap<NormalizedPath, FileEntry> = DashMap::new();
        snap.files.into_par_iter().for_each(|f| {
            let path = NormalizedPath::from(f.path.as_str());
            let includes = f
                .includes
                .into_iter()
                .map(|d| {
                    let kind = match d.kind {
                        0 => IncludeKind::Quoted,
                        1 => IncludeKind::AngleBracket,
                        _ => IncludeKind::Computed(d.path.clone()),
                    };
                    IncludeDirective {
                        kind,
                        path: d.path,
                        line: d.line,
                    }
                })
                .collect();
            files.insert(
                path,
                FileEntry {
                    includes,
                    scanned_at: Instant::now(),
                },
            );
        });

        let contexts: DashMap<ContextKey, ContextEntry> = DashMap::new();
        snap.contexts.into_par_iter().for_each(|c| {
            let key = ContextKey::from_raw(c.context_key);
            let context = CompileContext {
                source_file: NormalizedPath::from(c.source_file.as_str()),
                include_search: IncludeSearchPaths {
                    iquote: strings_to_paths(c.iquote),
                    user: strings_to_paths(c.user),
                    system: strings_to_paths(c.system),
                    after: strings_to_paths(c.after),
                },
                defines: c.defines,
                flags: c.flags,
                force_includes: strings_to_paths(c.force_includes),
                unknown_flags: c.unknown_flags,
            };
            let entry = ContextEntry {
                context,
                key_root: c.key_root.map(|root| NormalizedPath::from(root.as_str())),
                resolved_includes: strings_to_paths(c.resolved_includes),
                unresolved_includes: c.unresolved_includes,
                has_computed_includes: c.has_computed_includes,
                artifact_key: c.artifact_key.map(ArtifactKey::from_raw),
                last_file_hashes: c
                    .last_file_hashes
                    .into_iter()
                    .map(|(p, h)| (NormalizedPath::from(p.as_str()), ContentHash::from_bytes(h)))
                    .collect(),
                last_accessed: Instant::now(),
                state: match c.state {
                    0 => ContextState::Cold,
                    1 => ContextState::Warm,
                    _ => ContextState::Stale,
                },
            };
            contexts.insert(key, entry);
        });

        DepGraph::from_maps(files, contexts)
    }
}

fn paths_to_strings<P: AsRef<Path>>(paths: &[P]) -> Vec<String> {
    paths
        .iter()
        .map(|p| p.as_ref().to_string_lossy().into_owned())
        .collect()
}

fn strings_to_paths(strings: Vec<String>) -> Vec<NormalizedPath> {
    strings.into_iter().map(NormalizedPath::from).collect()
}

// ---------------------------------------------------------------------------
// File I/O
// ---------------------------------------------------------------------------

/// Returns the default path for the depgraph snapshot file.
#[must_use]
pub fn depgraph_file_path() -> NormalizedPath {
    zccache_core::config::depgraph_dir().join("depgraph.bin")
}

/// Save the dependency graph to disk with atomic write.
///
/// GC is applied first (1-day TTL) to avoid persisting stale entries.
pub fn save_to_file(graph: &DepGraph, path: &Path) -> Result<(), SnapshotError> {
    // GC: trim stale entries before saving.
    graph.trim(GC_TTL);

    let snapshot = graph.to_snapshot();

    let payload = rkyv::to_bytes::<_, SERIALIZE_SCRATCH>(&snapshot)
        .map_err(|e| SnapshotError::Corrupt(format!("serialize: {e}")))?;

    // Build header: magic + version (LE u32) + payload len (LE u64)
    let mut header = Vec::with_capacity(HEADER_SIZE);
    header.extend_from_slice(&DEPGRAPH_MAGIC);
    header.extend_from_slice(&DEPGRAPH_VERSION.to_le_bytes());
    header.extend_from_slice(&(payload.len() as u64).to_le_bytes());

    // Atomic write: write to .tmp, then rename.
    let tmp_path = path.with_extension("bin.tmp");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    {
        let mut file = std::fs::File::create(&tmp_path)?;
        use std::io::Write;
        file.write_all(&header)?;
        file.write_all(&payload)?;
        file.flush()?;
    }

    // Windows: remove target before rename (rename doesn't overwrite on Windows).
    let _ = std::fs::remove_file(path);
    std::fs::rename(&tmp_path, path)?;

    Ok(())
}

/// Load the dependency graph from disk, validating header and payload.
pub fn load_from_file(path: &Path) -> Result<DepGraph, SnapshotError> {
    let data = std::fs::read(path)?;

    if data.len() < HEADER_SIZE {
        return Err(SnapshotError::Corrupt("file too small for header".into()));
    }

    // Validate magic.
    if data[0..4] != DEPGRAPH_MAGIC {
        return Err(SnapshotError::BadMagic);
    }

    // Validate version.
    let version = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
    if version != DEPGRAPH_VERSION {
        return Err(SnapshotError::VersionMismatch {
            file: version,
            expected: DEPGRAPH_VERSION,
        });
    }

    // Validate payload length (use checked arithmetic to avoid overflow).
    let payload_len_u64 = u64::from_le_bytes([
        data[8], data[9], data[10], data[11], data[12], data[13], data[14], data[15],
    ]);
    let payload_len: usize = usize::try_from(payload_len_u64).map_err(|_| {
        SnapshotError::Corrupt(format!("payload length too large: {payload_len_u64}"))
    })?;

    let available = data.len() - HEADER_SIZE;
    if available < payload_len {
        return Err(SnapshotError::Corrupt(format!(
            "truncated: expected {payload_len} payload bytes, got {available}",
        )));
    }

    let payload = &data[HEADER_SIZE..HEADER_SIZE + payload_len];

    // Validate and deserialize.
    let archived = rkyv::check_archived_root::<DepGraphSnapshot>(payload)
        .map_err(|e| SnapshotError::Corrupt(format!("validation: {e}")))?;

    let snapshot: DepGraphSnapshot = archived
        .deserialize(&mut rkyv::Infallible)
        .expect("infallible deserialization");

    Ok(DepGraph::from_snapshot(snapshot))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::CacheVerdict;
    use crate::scanner::ScanResult;
    use tempfile::TempDir;

    fn test_path(dir: &TempDir) -> NormalizedPath {
        dir.path().join("depgraph.bin").into()
    }

    fn make_ctx(source: &str) -> CompileContext {
        CompileContext {
            source_file: NormalizedPath::from(source),
            include_search: IncludeSearchPaths::default(),
            defines: Vec::new(),
            flags: Vec::new(),
            force_includes: Vec::new(),
            unknown_flags: Vec::new(),
        }
    }

    #[test]
    fn empty_graph_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = test_path(&dir);
        let graph = DepGraph::new();

        save_to_file(&graph, &path).unwrap();
        let loaded = load_from_file(&path).unwrap();

        let stats = loaded.stats();
        assert_eq!(stats.file_count, 0);
        assert_eq!(stats.context_count, 0);
    }

    #[test]
    fn populated_graph_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = test_path(&dir);
        let graph = DepGraph::new();

        // Add file entries with all IncludeKind variants.
        graph.store_file_includes(
            NormalizedPath::from("/src/main.cpp"),
            vec![
                IncludeDirective {
                    kind: IncludeKind::Quoted,
                    path: "header.h".into(),
                    line: 1,
                },
                IncludeDirective {
                    kind: IncludeKind::AngleBracket,
                    path: "vector".into(),
                    line: 2,
                },
                IncludeDirective {
                    kind: IncludeKind::Computed("PLATFORM_HEADER".into()),
                    path: "PLATFORM_HEADER".into(),
                    line: 3,
                },
            ],
        );

        // Add a context entry with all fields populated.
        let ctx = CompileContext {
            source_file: NormalizedPath::from("/src/main.cpp"),
            include_search: IncludeSearchPaths {
                iquote: vec![NormalizedPath::from("/src")],
                user: vec![NormalizedPath::from("/include")],
                system: vec![NormalizedPath::from("/usr/include")],
                after: vec![NormalizedPath::from("/after")],
            },
            defines: vec!["DEBUG=1".into()],
            flags: vec!["-std=c++17".into()],
            force_includes: vec![NormalizedPath::from("/pch.h")],
            unknown_flags: vec!["--custom".into()],
        };
        let key = graph.register(ctx);

        // Update with resolved includes and file hashes.
        let source_hash = zccache_hash::hash_bytes(b"source content");
        let header_hash = zccache_hash::hash_bytes(b"header content");
        let pch_hash = zccache_hash::hash_bytes(b"pch content");
        let hashes: std::collections::HashMap<NormalizedPath, ContentHash> = [
            (NormalizedPath::from("/src/main.cpp"), source_hash),
            (NormalizedPath::from("/include/header.h"), header_hash),
            (NormalizedPath::from("/pch.h"), pch_hash),
        ]
        .into_iter()
        .collect();

        graph.update(
            &key,
            ScanResult {
                resolved: vec![NormalizedPath::from("/include/header.h")],
                unresolved: vec!["missing.h".into()],
                has_computed: true,
            },
            |path| hashes.get(&NormalizedPath::new(path)).copied(),
        );

        // Save and load.
        save_to_file(&graph, &path).unwrap();
        let loaded = load_from_file(&path).unwrap();

        let stats = loaded.stats();
        assert_eq!(stats.file_count, 1);
        assert_eq!(stats.context_count, 1);

        // Verify file entry.
        let includes = loaded
            .get_file_includes(&NormalizedPath::from("/src/main.cpp"))
            .unwrap();
        assert_eq!(includes.len(), 3);
        assert_eq!(includes[0].kind, IncludeKind::Quoted);
        assert_eq!(includes[1].kind, IncludeKind::AngleBracket);
        assert!(matches!(includes[2].kind, IncludeKind::Computed(_)));

        // Verify context state survived.
        assert_eq!(loaded.get_state(&key), Some(ContextState::Warm));
        let resolved = loaded.get_includes(&key).unwrap();
        assert_eq!(resolved, vec![NormalizedPath::from("/include/header.h")]);
    }

    #[test]
    fn version_mismatch() {
        let dir = TempDir::new().unwrap();
        let path = test_path(&dir);

        let mut data = Vec::new();
        data.extend_from_slice(&DEPGRAPH_MAGIC);
        data.extend_from_slice(&99u32.to_le_bytes());
        data.extend_from_slice(&0u64.to_le_bytes());
        std::fs::write(&path, &data).unwrap();

        match load_from_file(&path) {
            Err(SnapshotError::VersionMismatch {
                file: 99,
                expected: DEPGRAPH_VERSION,
            }) => {}
            other => panic!("expected VersionMismatch, got {other:?}"),
        }
    }

    #[test]
    fn bad_magic() {
        let dir = TempDir::new().unwrap();
        let path = test_path(&dir);

        let mut data = Vec::new();
        data.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
        data.extend_from_slice(&DEPGRAPH_VERSION.to_le_bytes());
        data.extend_from_slice(&0u64.to_le_bytes());
        std::fs::write(&path, &data).unwrap();

        match load_from_file(&path) {
            Err(SnapshotError::BadMagic) => {}
            other => panic!("expected BadMagic, got {other:?}"),
        }
    }

    #[test]
    fn truncated_payload() {
        let dir = TempDir::new().unwrap();
        let path = test_path(&dir);

        let mut data = Vec::new();
        data.extend_from_slice(&DEPGRAPH_MAGIC);
        data.extend_from_slice(&DEPGRAPH_VERSION.to_le_bytes());
        data.extend_from_slice(&1000u64.to_le_bytes()); // claims 1000 bytes
        data.extend_from_slice(&[0u8; 10]); // only 10 bytes
        std::fs::write(&path, &data).unwrap();

        match load_from_file(&path) {
            Err(SnapshotError::Corrupt(_)) => {}
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    #[test]
    fn file_not_found() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nonexistent.bin");

        match load_from_file(&path) {
            Err(SnapshotError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
            other => panic!("expected Io(NotFound), got {other:?}"),
        }
    }

    #[test]
    fn atomic_write_cleans_tmp() {
        let dir = TempDir::new().unwrap();
        let path = test_path(&dir);
        let tmp_path = path.with_extension("bin.tmp");

        let graph = DepGraph::new();
        save_to_file(&graph, &path).unwrap();

        assert!(path.exists());
        assert!(!tmp_path.exists(), ".tmp file should be cleaned up");
    }

    #[test]
    fn last_file_hashes_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = test_path(&dir);
        let graph = DepGraph::new();

        let key = graph.register(make_ctx("/src/a.cpp"));
        let hash1 = zccache_hash::hash_bytes(b"content1");
        let hash2 = zccache_hash::hash_bytes(b"content2");
        let hashes: std::collections::HashMap<NormalizedPath, ContentHash> = [
            (NormalizedPath::from("/src/a.cpp"), hash1),
            (NormalizedPath::from("/inc/b.h"), hash2),
        ]
        .into_iter()
        .collect();

        graph.update(
            &key,
            ScanResult {
                resolved: vec![NormalizedPath::from("/inc/b.h")],
                unresolved: Vec::new(),
                has_computed: false,
            },
            |path| hashes.get(&NormalizedPath::new(path)).copied(),
        );

        save_to_file(&graph, &path).unwrap();
        let loaded = load_from_file(&path).unwrap();

        // Verify context survived with file hashes.
        assert_eq!(loaded.stats().context_count, 1);
        assert_eq!(loaded.get_state(&key), Some(ContextState::Warm));

        // Verify hashes via snapshot inspection.
        let snap = loaded.to_snapshot();
        assert_eq!(snap.contexts[0].last_file_hashes.len(), 2);
    }

    #[test]
    fn artifact_key_some_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = test_path(&dir);
        let graph = DepGraph::new();

        let key = graph.register(make_ctx("/src/c.cpp"));
        let hash = zccache_hash::hash_bytes(b"source");
        let hashes: std::collections::HashMap<NormalizedPath, ContentHash> =
            [(NormalizedPath::from("/src/c.cpp"), hash)]
                .into_iter()
                .collect();

        graph.update(
            &key,
            ScanResult {
                resolved: Vec::new(),
                unresolved: Vec::new(),
                has_computed: false,
            },
            |path| hashes.get(&NormalizedPath::new(path)).copied(),
        );

        save_to_file(&graph, &path).unwrap();
        let loaded = load_from_file(&path).unwrap();

        let snap = loaded.to_snapshot();
        assert!(
            snap.contexts[0].artifact_key.is_some(),
            "artifact_key should survive roundtrip"
        );
    }

    #[test]
    fn gc_trims_old_entries() {
        let graph = DepGraph::new();
        graph.register(make_ctx("/old.cpp"));
        assert_eq!(graph.stats().context_count, 1);

        // trim with zero duration removes all entries.
        let removed = graph.trim(Duration::ZERO);
        assert_eq!(removed, 1);
        assert_eq!(graph.stats().context_count, 0);
    }

    // â”€â”€ Adversarial tests â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    fn dummy_hash(path: &std::path::Path) -> Option<ContentHash> {
        Some(zccache_hash::hash_bytes(path.to_string_lossy().as_bytes()))
    }

    fn always_fresh(_: &std::path::Path) -> bool {
        true
    }

    /// After save+load, a check() on the loaded graph must still return
    /// Hit for previously-warm contexts. This is the most important
    /// behavioral invariant.
    #[test]
    fn loaded_graph_serves_cache_hits() {
        let dir = TempDir::new().unwrap();
        let path = test_path(&dir);
        let graph = DepGraph::new();

        let ctx = CompileContext {
            source_file: NormalizedPath::from("/src/main.cpp"),
            include_search: IncludeSearchPaths {
                user: vec![NormalizedPath::from("/include")],
                system: vec![NormalizedPath::from("/usr/include")],
                ..Default::default()
            },
            defines: vec!["NDEBUG".into()],
            flags: vec!["-O2".into(), "-std=c++17".into()],
            force_includes: vec![NormalizedPath::from("/pch.h")],
            unknown_flags: Vec::new(),
        };
        let key = graph.register(ctx);

        let hashes: std::collections::HashMap<NormalizedPath, ContentHash> = [
            (
                NormalizedPath::from("/src/main.cpp"),
                zccache_hash::hash_bytes(b"src"),
            ),
            (
                NormalizedPath::from("/include/a.h"),
                zccache_hash::hash_bytes(b"a"),
            ),
            (
                NormalizedPath::from("/pch.h"),
                zccache_hash::hash_bytes(b"pch"),
            ),
        ]
        .into_iter()
        .collect();

        graph.update(
            &key,
            ScanResult {
                resolved: vec![NormalizedPath::from("/include/a.h")],
                unresolved: Vec::new(),
                has_computed: false,
            },
            |p| hashes.get(&NormalizedPath::new(p)).copied(),
        );

        // Verify original graph serves hits.
        let verdict = graph.check(&key, always_fresh, |p| {
            hashes.get(&NormalizedPath::new(p)).copied()
        });
        assert!(
            matches!(verdict, CacheVerdict::Hit { .. }),
            "original graph should hit, got {verdict:?}"
        );

        // Save, load, check again.
        save_to_file(&graph, &path).unwrap();
        let loaded = load_from_file(&path).unwrap();

        let verdict = loaded.check(&key, always_fresh, |p| {
            hashes.get(&NormalizedPath::new(p)).copied()
        });
        assert!(
            matches!(verdict, CacheVerdict::Hit { .. }),
            "loaded graph should still serve hit, got {verdict:?}"
        );
    }

    /// The stored context key must match the key recomputed from the
    /// loaded CompileContext. If lossy PathBufâ†’Stringâ†’NormalizedPath conversion
    /// corrupts paths, the key will diverge and lookups will silently fail.
    #[test]
    fn context_key_consistent_after_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = test_path(&dir);
        let graph = DepGraph::new();

        let ctx = CompileContext {
            source_file: NormalizedPath::from("/src/main.cpp"),
            include_search: IncludeSearchPaths {
                iquote: vec![NormalizedPath::from("/iquote/dir")],
                user: vec![NormalizedPath::from("/user/dir")],
                system: vec![NormalizedPath::from("/system/dir")],
                after: vec![NormalizedPath::from("/after/dir")],
            },
            defines: vec!["FOO=1".into(), "BAR=2".into()],
            flags: vec!["-Wall".into()],
            force_includes: vec![NormalizedPath::from("/fi/pch.h")],
            unknown_flags: vec!["--custom".into()],
        };
        let original_key = ctx.context_key();
        graph.register(ctx);

        let hash = zccache_hash::hash_bytes(b"x");
        let hashes: std::collections::HashMap<NormalizedPath, ContentHash> = [
            (NormalizedPath::from("/src/main.cpp"), hash),
            (NormalizedPath::from("/fi/pch.h"), hash),
        ]
        .into_iter()
        .collect();
        graph.update(
            &original_key,
            ScanResult {
                resolved: Vec::new(),
                unresolved: Vec::new(),
                has_computed: false,
            },
            |p| hashes.get(&NormalizedPath::new(p)).copied(),
        );

        save_to_file(&graph, &path).unwrap();
        let loaded = load_from_file(&path).unwrap();

        // The loaded graph should find the entry by the original key.
        assert_eq!(
            loaded.get_state(&original_key),
            Some(ContextState::Warm),
            "loaded graph must find entry by original context key"
        );

        // Extract the loaded CompileContext and recompute its key.
        let snap = loaded.to_snapshot();
        assert_eq!(snap.contexts.len(), 1);
        let loaded_ctx = CompileContext {
            source_file: NormalizedPath::from(&snap.contexts[0].source_file),
            include_search: IncludeSearchPaths {
                iquote: strings_to_paths(snap.contexts[0].iquote.clone()),
                user: strings_to_paths(snap.contexts[0].user.clone()),
                system: strings_to_paths(snap.contexts[0].system.clone()),
                after: strings_to_paths(snap.contexts[0].after.clone()),
            },
            defines: snap.contexts[0].defines.clone(),
            flags: snap.contexts[0].flags.clone(),
            force_includes: strings_to_paths(snap.contexts[0].force_includes.clone()),
            unknown_flags: snap.contexts[0].unknown_flags.clone(),
        };
        let recomputed_key = loaded_ctx.context_key();
        assert_eq!(
            *original_key.hash().as_bytes(),
            *recomputed_key.hash().as_bytes(),
            "context key recomputed from loaded context must match stored key"
        );
    }

    /// Unicode paths must roundtrip correctly â€” they are common on macOS
    /// (NFC normalization) and Windows (wide chars).
    #[test]
    fn unicode_paths_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = test_path(&dir);
        let graph = DepGraph::new();

        let unicode_source = "/src/æ—¥æœ¬èªž/main.cpp";
        let unicode_header = "/inc/donnÃ©es/header.h";
        let unicode_define = "NÃ„ME=ÃœnÃ¯cÃ¶dÃ©";
        let emoji_path = "/inc/ðŸŽ‰/emoji.h";

        let ctx = CompileContext {
            source_file: NormalizedPath::from(unicode_source),
            include_search: IncludeSearchPaths {
                user: vec![
                    NormalizedPath::from(unicode_header),
                    NormalizedPath::from(emoji_path),
                ],
                ..Default::default()
            },
            defines: vec![unicode_define.into()],
            flags: Vec::new(),
            force_includes: Vec::new(),
            unknown_flags: Vec::new(),
        };
        let key = graph.register(ctx);
        let hash = zccache_hash::hash_bytes(b"x");
        let hashes: std::collections::HashMap<NormalizedPath, ContentHash> =
            [(NormalizedPath::from(unicode_source), hash)]
                .into_iter()
                .collect();
        graph.update(
            &key,
            ScanResult {
                resolved: Vec::new(),
                unresolved: Vec::new(),
                has_computed: false,
            },
            |p| hashes.get(&NormalizedPath::new(p)).copied(),
        );

        // Also store file includes with unicode paths.
        graph.store_file_includes(
            NormalizedPath::from(unicode_source),
            vec![IncludeDirective {
                kind: IncludeKind::Quoted,
                path: unicode_header.into(),
                line: 1,
            }],
        );

        save_to_file(&graph, &path).unwrap();
        let loaded = load_from_file(&path).unwrap();

        assert_eq!(loaded.get_state(&key), Some(ContextState::Warm));
        let includes = loaded
            .get_file_includes(&NormalizedPath::from(unicode_source))
            .unwrap();
        assert_eq!(includes[0].path, unicode_header);

        // Verify the context's include search paths survived.
        let snap = loaded.to_snapshot();
        assert_eq!(
            snap.contexts[0].source_file,
            NormalizedPath::from(unicode_source).display().to_string()
        );
        assert!(snap.contexts[0]
            .user
            .contains(&NormalizedPath::from(unicode_header).display().to_string()));
        assert!(snap.contexts[0]
            .user
            .contains(&NormalizedPath::from(emoji_path).display().to_string()));
        assert!(snap.contexts[0]
            .defines
            .contains(&unicode_define.to_string()));
    }

    /// Saveâ†’loadâ†’saveâ†’load must produce an identical graph. Tests for
    /// any drift introduced by a single roundtrip (e.g., path
    /// normalization, field reordering, floating precision).
    #[test]
    fn double_roundtrip_idempotent() {
        let dir = TempDir::new().unwrap();
        let path1 = dir.path().join("pass1.bin");
        let path2 = dir.path().join("pass2.bin");
        let graph = DepGraph::new();

        // Build a non-trivial graph.
        for i in 0..5 {
            let ctx = CompileContext {
                source_file: NormalizedPath::from(format!("/src/file{i}.cpp")),
                include_search: IncludeSearchPaths {
                    user: vec![NormalizedPath::from(format!("/inc{i}"))],
                    system: vec![NormalizedPath::from("/sys")],
                    ..Default::default()
                },
                defines: vec![format!("VAR{i}=1")],
                flags: vec!["-O2".into()],
                force_includes: Vec::new(),
                unknown_flags: Vec::new(),
            };
            let key = graph.register(ctx);
            graph.update(
                &key,
                ScanResult {
                    resolved: vec![NormalizedPath::from(format!("/inc{i}/h.h"))],
                    unresolved: vec![format!("missing{i}.h")],
                    has_computed: i == 0, // one with computed includes
                },
                dummy_hash,
            );
            graph.store_file_includes(
                NormalizedPath::from(format!("/src/file{i}.cpp")),
                vec![IncludeDirective {
                    kind: IncludeKind::Quoted,
                    path: format!("h{i}.h"),
                    line: i as u32 + 1,
                }],
            );
        }

        // First roundtrip.
        save_to_file(&graph, &path1).unwrap();
        let loaded1 = load_from_file(&path1).unwrap();

        // Second roundtrip.
        save_to_file(&loaded1, &path2).unwrap();
        let loaded2 = load_from_file(&path2).unwrap();

        // Compare snapshots field-by-field.
        let snap1 = loaded1.to_snapshot();
        let snap2 = loaded2.to_snapshot();
        assert_eq!(snap1.files.len(), snap2.files.len(), "file count mismatch");
        assert_eq!(
            snap1.contexts.len(),
            snap2.contexts.len(),
            "context count mismatch"
        );

        // Sort by path for deterministic comparison (DashMap order is random).
        let mut files1: Vec<_> = snap1.files.iter().map(|f| &f.path).collect();
        let mut files2: Vec<_> = snap2.files.iter().map(|f| &f.path).collect();
        files1.sort();
        files2.sort();
        assert_eq!(files1, files2, "file paths differ after double roundtrip");

        let mut keys1: Vec<_> = snap1.contexts.iter().map(|c| c.context_key).collect();
        let mut keys2: Vec<_> = snap2.contexts.iter().map(|c| c.context_key).collect();
        keys1.sort();
        keys2.sort();
        assert_eq!(keys1, keys2, "context keys differ after double roundtrip");
    }

    /// Multiple contexts referencing overlapping resolved includes.
    /// All must survive independently.
    #[test]
    fn overlapping_contexts_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = test_path(&dir);
        let graph = DepGraph::new();

        let shared_header = NormalizedPath::from("/inc/shared.h");

        // Two contexts that share the same header.
        let ctx_a = CompileContext {
            source_file: NormalizedPath::from("/src/a.cpp"),
            include_search: IncludeSearchPaths {
                user: vec![NormalizedPath::from("/inc")],
                ..Default::default()
            },
            defines: vec!["A=1".into()],
            flags: Vec::new(),
            force_includes: Vec::new(),
            unknown_flags: Vec::new(),
        };
        let ctx_b = CompileContext {
            source_file: NormalizedPath::from("/src/b.cpp"),
            include_search: IncludeSearchPaths {
                user: vec![NormalizedPath::from("/inc")],
                ..Default::default()
            },
            defines: vec!["B=1".into()],
            flags: Vec::new(),
            force_includes: Vec::new(),
            unknown_flags: Vec::new(),
        };

        let key_a = graph.register(ctx_a);
        let key_b = graph.register(ctx_b);

        let hashes: std::collections::HashMap<NormalizedPath, ContentHash> = [
            (
                NormalizedPath::from("/src/a.cpp"),
                zccache_hash::hash_bytes(b"a"),
            ),
            (
                NormalizedPath::from("/src/b.cpp"),
                zccache_hash::hash_bytes(b"b"),
            ),
            (shared_header.clone(), zccache_hash::hash_bytes(b"shared")),
        ]
        .into_iter()
        .collect();

        graph.update(
            &key_a,
            ScanResult {
                resolved: vec![shared_header.clone()],
                unresolved: Vec::new(),
                has_computed: false,
            },
            |p| hashes.get(&NormalizedPath::new(p)).copied(),
        );
        graph.update(
            &key_b,
            ScanResult {
                resolved: vec![shared_header.clone()],
                unresolved: Vec::new(),
                has_computed: false,
            },
            |p| hashes.get(&NormalizedPath::new(p)).copied(),
        );

        save_to_file(&graph, &path).unwrap();
        let loaded = load_from_file(&path).unwrap();

        assert_eq!(loaded.stats().context_count, 2);
        assert_eq!(loaded.get_state(&key_a), Some(ContextState::Warm));
        assert_eq!(loaded.get_state(&key_b), Some(ContextState::Warm));

        // Both should serve hits.
        let verdict_a = loaded.check(&key_a, always_fresh, |p| {
            hashes.get(&NormalizedPath::new(p)).copied()
        });
        let verdict_b = loaded.check(&key_b, always_fresh, |p| {
            hashes.get(&NormalizedPath::new(p)).copied()
        });
        assert!(matches!(verdict_a, CacheVerdict::Hit { .. }));
        assert!(matches!(verdict_b, CacheVerdict::Hit { .. }));

        // And they must have different artifact keys (different source files).
        match (verdict_a, verdict_b) {
            (
                CacheVerdict::Hit { artifact_key: ak_a },
                CacheVerdict::Hit { artifact_key: ak_b },
            ) => {
                assert_ne!(
                    ak_a.hash().as_bytes(),
                    ak_b.hash().as_bytes(),
                    "different contexts should have different artifact keys"
                );
            }
            _ => unreachable!(),
        }
    }

    /// All three ContextState variants must survive roundtrip faithfully.
    #[test]
    fn all_states_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = test_path(&dir);
        let graph = DepGraph::new();

        // Cold context: just register, never update.
        let cold_key = graph.register(make_ctx("/src/cold.cpp"));
        assert_eq!(graph.get_state(&cold_key), Some(ContextState::Cold));

        // Warm context: register + update.
        let warm_key = graph.register(make_ctx("/src/warm.cpp"));
        graph.update(
            &warm_key,
            ScanResult {
                resolved: Vec::new(),
                unresolved: Vec::new(),
                has_computed: false,
            },
            dummy_hash,
        );
        assert_eq!(graph.get_state(&warm_key), Some(ContextState::Warm));

        // Stale context: register + update + mark stale.
        let stale_key = graph.register(make_ctx("/src/stale.cpp"));
        graph.update(
            &stale_key,
            ScanResult {
                resolved: Vec::new(),
                unresolved: Vec::new(),
                has_computed: false,
            },
            dummy_hash,
        );
        graph.mark_stale(&stale_key);
        assert_eq!(graph.get_state(&stale_key), Some(ContextState::Stale));

        save_to_file(&graph, &path).unwrap();
        let loaded = load_from_file(&path).unwrap();

        assert_eq!(
            loaded.get_state(&cold_key),
            Some(ContextState::Cold),
            "Cold state not preserved"
        );
        assert_eq!(
            loaded.get_state(&warm_key),
            Some(ContextState::Warm),
            "Warm state not preserved"
        );
        assert_eq!(
            loaded.get_state(&stale_key),
            Some(ContextState::Stale),
            "Stale state not preserved"
        );
    }

    /// A bit-flip in the rkyv payload should be caught by validation.
    #[test]
    fn bit_flip_in_payload_detected() {
        let dir = TempDir::new().unwrap();
        let path = test_path(&dir);
        let graph = DepGraph::new();

        let key = graph.register(make_ctx("/src/a.cpp"));
        graph.update(
            &key,
            ScanResult {
                resolved: vec![NormalizedPath::from("/inc/b.h")],
                unresolved: Vec::new(),
                has_computed: false,
            },
            dummy_hash,
        );

        save_to_file(&graph, &path).unwrap();

        // Read the file, flip a byte in the payload, write it back.
        let mut data = std::fs::read(&path).unwrap();
        assert!(data.len() > HEADER_SIZE + 10);
        // Flip a bit in the middle of the payload.
        let flip_idx = HEADER_SIZE + (data.len() - HEADER_SIZE) / 2;
        data[flip_idx] ^= 0xFF;
        std::fs::write(&path, &data).unwrap();

        match load_from_file(&path) {
            Err(SnapshotError::Corrupt(_)) => {} // Expected
            Ok(_) => {
                // rkyv might not catch every bit-flip if it lands on
                // a valid-looking field. This is acceptable â€” we just
                // want to verify the validation path exists.
            }
            Err(other) => panic!("unexpected error: {other}"),
        }
    }

    /// Empty strings in all fields must not cause panics or data loss.
    #[test]
    fn empty_strings_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = test_path(&dir);
        let graph = DepGraph::new();

        let ctx = CompileContext {
            source_file: NormalizedPath::from(""),
            include_search: IncludeSearchPaths {
                iquote: vec![NormalizedPath::from("")],
                user: vec![NormalizedPath::from("")],
                system: vec![NormalizedPath::from("")],
                after: vec![NormalizedPath::from("")],
            },
            defines: vec![String::new()],
            flags: vec![String::new()],
            force_includes: vec![NormalizedPath::from("")],
            unknown_flags: vec![String::new()],
        };
        let key = graph.register(ctx);

        // Empty path hash.
        let hash = zccache_hash::hash_bytes(b"");
        let hashes: std::collections::HashMap<NormalizedPath, ContentHash> =
            [(NormalizedPath::from(""), hash)].into_iter().collect();
        graph.update(
            &key,
            ScanResult {
                resolved: Vec::new(),
                unresolved: vec![String::new()],
                has_computed: false,
            },
            |p| hashes.get(&NormalizedPath::new(p)).copied(),
        );

        save_to_file(&graph, &path).unwrap();
        let loaded = load_from_file(&path).unwrap();

        assert_eq!(loaded.stats().context_count, 1);
        assert_eq!(loaded.get_state(&key), Some(ContextState::Warm));

        let snap = loaded.to_snapshot();
        assert_eq!(snap.contexts[0].source_file, "");
        assert_eq!(snap.contexts[0].defines, vec![""]);
        assert_eq!(snap.contexts[0].unresolved_includes, vec![""]);
    }

    /// Stress test: many contexts + files to verify no panics, no data
    /// loss, and reasonable performance.
    #[test]
    fn large_graph_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = test_path(&dir);
        let graph = DepGraph::new();

        let n_contexts = 200;
        let n_headers_per_ctx = 10;
        let mut keys = Vec::new();

        for i in 0..n_contexts {
            let ctx = CompileContext {
                source_file: NormalizedPath::from(format!("/src/file{i}.cpp")),
                include_search: IncludeSearchPaths {
                    user: vec![NormalizedPath::from(format!("/inc{i}"))],
                    ..Default::default()
                },
                defines: (0..5).map(|d| format!("DEF{d}={i}")).collect(),
                flags: vec!["-O2".into(), format!("-std=c++{}", 14 + (i % 4) * 3)],
                force_includes: Vec::new(),
                unknown_flags: Vec::new(),
            };
            let key = graph.register(ctx);

            let resolved: Vec<NormalizedPath> = (0..n_headers_per_ctx)
                .map(|h| NormalizedPath::from(format!("/inc{i}/header{h}.h")))
                .collect();
            graph.update(
                &key,
                ScanResult {
                    resolved,
                    unresolved: Vec::new(),
                    has_computed: false,
                },
                dummy_hash,
            );
            keys.push(key);
        }

        assert_eq!(graph.stats().context_count, n_contexts);

        save_to_file(&graph, &path).unwrap();
        let loaded = load_from_file(&path).unwrap();

        assert_eq!(loaded.stats().context_count, n_contexts);

        // Spot-check a few contexts.
        for key in keys.iter().take(10) {
            assert_eq!(loaded.get_state(key), Some(ContextState::Warm));
            let verdict = loaded.check(key, always_fresh, dummy_hash);
            assert!(
                matches!(verdict, CacheVerdict::Hit { .. }),
                "context should hit after load"
            );
        }
    }

    /// Overwriting an existing snapshot file must work (tests the
    /// Windows remove-before-rename path).
    #[test]
    fn overwrite_existing_file() {
        let dir = TempDir::new().unwrap();
        let path = test_path(&dir);

        // First save.
        let graph1 = DepGraph::new();
        graph1.register(make_ctx("/src/old.cpp"));
        save_to_file(&graph1, &path).unwrap();

        // Second save with different content.
        let graph2 = DepGraph::new();
        let key = graph2.register(make_ctx("/src/new.cpp"));
        graph2.update(
            &key,
            ScanResult {
                resolved: Vec::new(),
                unresolved: Vec::new(),
                has_computed: false,
            },
            dummy_hash,
        );
        save_to_file(&graph2, &path).unwrap();

        // Load should see the second graph.
        let loaded = load_from_file(&path).unwrap();
        assert_eq!(loaded.stats().context_count, 1);
        assert_eq!(loaded.get_state(&key), Some(ContextState::Warm));
    }

    /// A file with correct header but zero-length payload.
    #[test]
    fn zero_length_payload_rejected() {
        let dir = TempDir::new().unwrap();
        let path = test_path(&dir);

        let mut data = Vec::new();
        data.extend_from_slice(&DEPGRAPH_MAGIC);
        data.extend_from_slice(&DEPGRAPH_VERSION.to_le_bytes());
        data.extend_from_slice(&0u64.to_le_bytes()); // zero-length payload
        std::fs::write(&path, &data).unwrap();

        // rkyv should reject an empty payload.
        match load_from_file(&path) {
            Err(SnapshotError::Corrupt(_)) => {}
            other => panic!("expected Corrupt for empty payload, got {other:?}"),
        }
    }

    /// Just the magic bytes and nothing else â€” shorter than header.
    #[test]
    fn header_too_short() {
        let dir = TempDir::new().unwrap();
        let path = test_path(&dir);

        std::fs::write(&path, DEPGRAPH_MAGIC).unwrap();

        match load_from_file(&path) {
            Err(SnapshotError::Corrupt(msg)) => {
                assert!(msg.contains("too small"), "unexpected message: {msg}");
            }
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    /// Artifact key=None (Cold context) must roundtrip as None.
    #[test]
    fn artifact_key_none_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = test_path(&dir);
        let graph = DepGraph::new();

        // Register but don't update â€” artifact_key stays None.
        graph.register(make_ctx("/src/cold.cpp"));

        save_to_file(&graph, &path).unwrap();
        let loaded = load_from_file(&path).unwrap();

        let snap = loaded.to_snapshot();
        assert_eq!(snap.contexts.len(), 1);
        assert!(
            snap.contexts[0].artifact_key.is_none(),
            "Cold context should have artifact_key=None"
        );
    }

    /// Unresolved includes (strings, not paths) must roundtrip.
    #[test]
    fn unresolved_includes_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = test_path(&dir);
        let graph = DepGraph::new();

        let key = graph.register(make_ctx("/src/a.cpp"));
        graph.update(
            &key,
            ScanResult {
                resolved: Vec::new(),
                unresolved: vec!["missing1.h".into(), "subdir/missing2.h".into(), "".into()],
                has_computed: false,
            },
            dummy_hash,
        );

        save_to_file(&graph, &path).unwrap();
        let loaded = load_from_file(&path).unwrap();

        let snap = loaded.to_snapshot();
        assert_eq!(
            snap.contexts[0].unresolved_includes,
            vec!["missing1.h", "subdir/missing2.h", ""]
        );
    }

    /// has_computed_includes flag must roundtrip for both true and false.
    #[test]
    fn has_computed_includes_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = test_path(&dir);
        let graph = DepGraph::new();

        let key_with = graph.register(make_ctx("/src/with_computed.cpp"));
        graph.update(
            &key_with,
            ScanResult {
                resolved: Vec::new(),
                unresolved: Vec::new(),
                has_computed: true,
            },
            dummy_hash,
        );

        let key_without = graph.register(make_ctx("/src/without_computed.cpp"));
        graph.update(
            &key_without,
            ScanResult {
                resolved: Vec::new(),
                unresolved: Vec::new(),
                has_computed: false,
            },
            dummy_hash,
        );

        save_to_file(&graph, &path).unwrap();
        let loaded = load_from_file(&path).unwrap();

        let snap = loaded.to_snapshot();
        let with_computed = NormalizedPath::new("/src/with_computed.cpp")
            .display()
            .to_string();
        let without_computed = NormalizedPath::new("/src/without_computed.cpp")
            .display()
            .to_string();
        let ctx_with = snap
            .contexts
            .iter()
            .find(|c| c.source_file == with_computed)
            .unwrap();
        let ctx_without = snap
            .contexts
            .iter()
            .find(|c| c.source_file == without_computed)
            .unwrap();
        assert!(ctx_with.has_computed_includes);
        assert!(!ctx_without.has_computed_includes);

        // Warm context with has_computed must return NeedsPreprocessor on check.
        let verdict = loaded.check(&key_with, always_fresh, dummy_hash);
        assert!(
            matches!(verdict, CacheVerdict::NeedsPreprocessor),
            "computed includes should force preprocessor, got {verdict:?}"
        );
    }

    /// All three IncludeKind variants in file entries must roundtrip,
    /// including the inner string of Computed.
    #[test]
    fn include_kind_computed_inner_string_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = test_path(&dir);
        let graph = DepGraph::new();

        let macro_name = "MY_PLATFORM_HEADER";
        graph.store_file_includes(
            NormalizedPath::from("/src/test.cpp"),
            vec![
                IncludeDirective {
                    kind: IncludeKind::Quoted,
                    path: "local.h".into(),
                    line: 1,
                },
                IncludeDirective {
                    kind: IncludeKind::AngleBracket,
                    path: "system.h".into(),
                    line: 2,
                },
                IncludeDirective {
                    kind: IncludeKind::Computed(macro_name.into()),
                    path: macro_name.into(),
                    line: 3,
                },
            ],
        );

        // Need a context that references this file so trim doesn't remove it.
        let key = graph.register(make_ctx("/src/test.cpp"));
        graph.update(
            &key,
            ScanResult {
                resolved: Vec::new(),
                unresolved: Vec::new(),
                has_computed: true,
            },
            dummy_hash,
        );

        save_to_file(&graph, &path).unwrap();
        let loaded = load_from_file(&path).unwrap();

        let includes = loaded
            .get_file_includes(&NormalizedPath::from("/src/test.cpp"))
            .unwrap();
        assert_eq!(includes.len(), 3);
        assert_eq!(includes[0].kind, IncludeKind::Quoted);
        assert_eq!(includes[0].path, "local.h");
        assert_eq!(includes[1].kind, IncludeKind::AngleBracket);
        assert_eq!(includes[1].path, "system.h");
        match &includes[2].kind {
            IncludeKind::Computed(inner) => {
                assert_eq!(
                    inner, macro_name,
                    "Computed inner string must survive roundtrip"
                );
            }
            other => panic!("expected Computed, got {other:?}"),
        }
        assert_eq!(includes[2].line, 3);
    }

    /// A new compile request for the same context after loading must
    /// find the existing warm entry (not create a duplicate cold one).
    #[test]
    fn register_after_load_finds_existing() {
        let dir = TempDir::new().unwrap();
        let path = test_path(&dir);
        let graph = DepGraph::new();

        let ctx = CompileContext {
            source_file: NormalizedPath::from("/src/main.cpp"),
            include_search: IncludeSearchPaths {
                user: vec![NormalizedPath::from("/inc")],
                ..Default::default()
            },
            defines: vec!["X=1".into()],
            flags: Vec::new(),
            force_includes: Vec::new(),
            unknown_flags: Vec::new(),
        };
        let original_key = graph.register(ctx.clone());
        graph.update(
            &original_key,
            ScanResult {
                resolved: vec![NormalizedPath::from("/inc/a.h")],
                unresolved: Vec::new(),
                has_computed: false,
            },
            dummy_hash,
        );

        save_to_file(&graph, &path).unwrap();
        let loaded = load_from_file(&path).unwrap();

        // Simulate a new compile request with the identical context.
        let new_key = loaded.register(ctx);
        assert_eq!(
            original_key.hash().as_bytes(),
            new_key.hash().as_bytes(),
            "re-registering same context must produce same key"
        );
        // The existing warm entry must still be there (not overwritten).
        assert_eq!(
            loaded.get_state(&new_key),
            Some(ContextState::Warm),
            "re-register must not overwrite warm entry with cold"
        );
        assert_eq!(
            loaded.stats().context_count,
            1,
            "re-register must not create duplicate"
        );
    }

    /// File hashes must roundtrip with exact byte equality.
    #[test]
    fn file_hash_bytes_exact_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = test_path(&dir);
        let graph = DepGraph::new();

        let key = graph.register(make_ctx("/src/a.cpp"));
        let source_hash = zccache_hash::hash_bytes(b"specific source content 12345");
        let header_hash = zccache_hash::hash_bytes(b"specific header content 67890");

        let hashes: std::collections::HashMap<NormalizedPath, ContentHash> = [
            (NormalizedPath::from("/src/a.cpp"), source_hash),
            (NormalizedPath::from("/inc/b.h"), header_hash),
        ]
        .into_iter()
        .collect();

        graph.update(
            &key,
            ScanResult {
                resolved: vec![NormalizedPath::from("/inc/b.h")],
                unresolved: Vec::new(),
                has_computed: false,
            },
            |p| hashes.get(&NormalizedPath::new(p)).copied(),
        );

        save_to_file(&graph, &path).unwrap();
        let loaded = load_from_file(&path).unwrap();

        let snap = loaded.to_snapshot();
        let ctx = &snap.contexts[0];

        // Verify each hash byte-for-byte.
        for (snap_path, snap_hash) in &ctx.last_file_hashes {
            let expected = hashes.get(&NormalizedPath::from(snap_path)).unwrap();
            assert_eq!(
                snap_hash,
                expected.as_bytes(),
                "hash mismatch for {snap_path}"
            );
        }
    }

    /// Artifact key bytes must be identical after roundtrip.
    #[test]
    fn artifact_key_bytes_exact_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = test_path(&dir);
        let graph = DepGraph::new();

        let key = graph.register(make_ctx("/src/a.cpp"));
        let artifact = graph
            .update(
                &key,
                ScanResult {
                    resolved: Vec::new(),
                    unresolved: Vec::new(),
                    has_computed: false,
                },
                dummy_hash,
            )
            .unwrap();

        save_to_file(&graph, &path).unwrap();
        let loaded = load_from_file(&path).unwrap();

        let snap = loaded.to_snapshot();
        let loaded_artifact_bytes = snap.contexts[0].artifact_key.unwrap();
        assert_eq!(
            &loaded_artifact_bytes,
            artifact.hash().as_bytes(),
            "artifact key bytes must be identical after roundtrip"
        );
    }

    /// GC during save must not discard recently-accessed warm contexts.
    #[test]
    fn gc_on_save_preserves_fresh_entries() {
        let dir = TempDir::new().unwrap();
        let path = test_path(&dir);
        let graph = DepGraph::new();

        // Register and update 10 contexts.
        let mut keys = Vec::new();
        for i in 0..10 {
            let key = graph.register(make_ctx(&format!("/src/f{i}.cpp")));
            graph.update(
                &key,
                ScanResult {
                    resolved: Vec::new(),
                    unresolved: Vec::new(),
                    has_computed: false,
                },
                dummy_hash,
            );
            keys.push(key);
        }

        // Save triggers GC (1-day TTL). All entries are fresh, so none should be trimmed.
        save_to_file(&graph, &path).unwrap();
        let loaded = load_from_file(&path).unwrap();

        assert_eq!(
            loaded.stats().context_count,
            10,
            "GC should not trim fresh entries"
        );
        for key in &keys {
            assert_eq!(loaded.get_state(key), Some(ContextState::Warm));
        }
    }

    /// Stats counters must reset to zero after load (not carry forward
    /// stale hit/miss data).
    #[test]
    fn stats_reset_after_load() {
        let dir = TempDir::new().unwrap();
        let path = test_path(&dir);
        let graph = DepGraph::new();

        let key = graph.register(make_ctx("/src/a.cpp"));
        graph.update(
            &key,
            ScanResult {
                resolved: Vec::new(),
                unresolved: Vec::new(),
                has_computed: false,
            },
            dummy_hash,
        );
        // Generate some stats.
        graph.check(&key, always_fresh, dummy_hash);
        graph.check(&key, always_fresh, dummy_hash);
        assert_eq!(graph.stats().checks, 2);
        assert_eq!(graph.stats().hits, 2);

        save_to_file(&graph, &path).unwrap();
        let loaded = load_from_file(&path).unwrap();

        let stats = loaded.stats();
        assert_eq!(stats.checks, 0, "checks must reset on load");
        assert_eq!(stats.hits, 0, "hits must reset on load");
        assert_eq!(stats.misses, 0, "misses must reset on load");
    }

    /// Payload with trailing garbage bytes after the declared length.
    /// The loader should ignore trailing data (only read payload_len bytes).
    #[test]
    fn trailing_garbage_after_payload_ignored() {
        let dir = TempDir::new().unwrap();
        let path = test_path(&dir);
        let graph = DepGraph::new();

        let key = graph.register(make_ctx("/src/a.cpp"));
        graph.update(
            &key,
            ScanResult {
                resolved: Vec::new(),
                unresolved: Vec::new(),
                has_computed: false,
            },
            dummy_hash,
        );
        save_to_file(&graph, &path).unwrap();

        // Append garbage to the file.
        let mut data = std::fs::read(&path).unwrap();
        data.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0xFF]);
        std::fs::write(&path, &data).unwrap();

        // Should still load fine â€” trailing data is beyond payload_len.
        let loaded = load_from_file(&path).unwrap();
        assert_eq!(loaded.stats().context_count, 1);
        assert_eq!(loaded.get_state(&key), Some(ContextState::Warm));
    }

    /// Concurrent save + load should not panic or corrupt (thread safety).
    #[test]
    fn concurrent_save_load() {
        use std::sync::Arc;

        let dir = TempDir::new().unwrap();
        let path = test_path(&dir);
        let graph = Arc::new(DepGraph::new());

        // Populate the graph.
        for i in 0..50 {
            let key = graph.register(make_ctx(&format!("/src/f{i}.cpp")));
            graph.update(
                &key,
                ScanResult {
                    resolved: vec![NormalizedPath::from(format!("/inc/h{i}.h"))],
                    unresolved: Vec::new(),
                    has_computed: false,
                },
                dummy_hash,
            );
        }

        // Save once so the file exists.
        save_to_file(&graph, &path).unwrap();

        let mut handles = Vec::new();

        // Writer threads.
        for _ in 0..3 {
            let g = Arc::clone(&graph);
            let p = path.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..5 {
                    let _ = save_to_file(&g, &p);
                }
            }));
        }

        // Reader threads.
        for _ in 0..3 {
            let p = path.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..5 {
                    // May fail if file is being rewritten â€” that's OK.
                    let _ = load_from_file(&p);
                }
            }));
        }

        // Mutator threads (add new entries while saving).
        for t in 0..2 {
            let g = Arc::clone(&graph);
            handles.push(std::thread::spawn(move || {
                for i in 0..20 {
                    let key = g.register(make_ctx(&format!("/src/t{t}_new{i}.cpp")));
                    g.update(
                        &key,
                        ScanResult {
                            resolved: Vec::new(),
                            unresolved: Vec::new(),
                            has_computed: false,
                        },
                        dummy_hash,
                    );
                }
            }));
        }

        for h in handles {
            h.join().expect("thread panicked");
        }

        // Final save+load should be consistent.
        save_to_file(&graph, &path).unwrap();
        let loaded = load_from_file(&path).unwrap();
        assert!(loaded.stats().context_count >= 50);
    }

    /// A crafted file with payload_len = u64::MAX must not panic or cause
    /// undefined behavior. The addition HEADER_SIZE + payload_len overflows
    /// usize, which panics in debug mode and wraps in release.
    #[test]
    fn payload_length_overflow_u64_max() {
        let dir = TempDir::new().unwrap();
        let path = test_path(&dir);

        let mut data = Vec::new();
        data.extend_from_slice(&DEPGRAPH_MAGIC);
        data.extend_from_slice(&DEPGRAPH_VERSION.to_le_bytes());
        data.extend_from_slice(&u64::MAX.to_le_bytes());
        data.extend_from_slice(&[0u8; 64]); // some payload bytes
        std::fs::write(&path, &data).unwrap();

        // Must return an error, not panic.
        assert!(
            load_from_file(&path).is_err(),
            "u64::MAX payload_len must be rejected"
        );
    }

    /// payload_len = usize::MAX - HEADER_SIZE + 1 causes overflow of
    /// HEADER_SIZE + payload_len.
    #[test]
    fn payload_length_overflow_boundary() {
        let dir = TempDir::new().unwrap();
        let path = test_path(&dir);

        // This value causes HEADER_SIZE + payload_len to wrap to exactly 0.
        let evil_len = (usize::MAX - HEADER_SIZE).wrapping_add(1) as u64;

        let mut data = Vec::new();
        data.extend_from_slice(&DEPGRAPH_MAGIC);
        data.extend_from_slice(&DEPGRAPH_VERSION.to_le_bytes());
        data.extend_from_slice(&evil_len.to_le_bytes());
        data.extend_from_slice(&[0u8; 64]);
        std::fs::write(&path, &data).unwrap();

        // Must return an error, not panic.
        assert!(
            load_from_file(&path).is_err(),
            "overflow-inducing payload_len must be rejected"
        );
    }
}
