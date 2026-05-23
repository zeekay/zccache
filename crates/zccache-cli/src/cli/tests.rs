//! Tests originally living in `main.rs`. Moved here as part of the
//! `cli/` split so they can reach the now-`pub(crate)` helpers via
//! `super::*` rather than `crate::main::*`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use super::analyze::{
    analyze_error_json, analyze_journal, analyze_journal_with, extract_flag_value, AnalyzeError,
    AnalyzeOptions, AnalyzeReport, AnalyzeSort, ANALYZE_EXPECTED_INPUT,
};
use super::args::{Cli, Commands, RustPlanBackendArg, RustPlanCommands};
use super::cache_ops::{
    artifact_matches_lockfile, parse_lockfile_crates, snapshot_bytes_walk, warm_target,
};
use super::daemon::ensure_daemon;
use super::daemon::wait_for_daemon_teardown;
use super::rust_plan::rust_plan_gha_version;
use super::session::{
    session_stats_error_json, session_stats_json, session_stats_unavailable_json,
};
use super::util::{exit_code_from_i32, flag_truthy};

#[test]
fn exit_code_zero_stays_zero() {
    assert_eq!(exit_code_from_i32(0), ExitCode::from(0));
}

#[test]
fn exit_code_one_stays_one() {
    assert_eq!(exit_code_from_i32(1), ExitCode::from(1));
}

#[test]
fn exit_code_255_stays_255() {
    assert_eq!(exit_code_from_i32(255), ExitCode::from(255));
}

#[test]
fn exit_code_256_becomes_one_not_zero() {
    // Without the fix, 256 as u8 == 0, masking the failure.
    assert_ne!(exit_code_from_i32(256), ExitCode::from(0));
    assert_eq!(exit_code_from_i32(256), ExitCode::from(1));
}

#[test]
fn exit_code_512_becomes_one_not_zero() {
    assert_eq!(exit_code_from_i32(512), ExitCode::from(1));
}

#[test]
fn exit_code_negative_preserves_failure() {
    // -1 & 0xFF == 255
    assert_ne!(exit_code_from_i32(-1), ExitCode::from(0));
    assert_eq!(exit_code_from_i32(-1), ExitCode::from(255));
}

#[test]
fn exit_code_257_keeps_low_byte() {
    // 257 & 0xFF == 1, non-zero, so kept as-is.
    assert_eq!(exit_code_from_i32(257), ExitCode::from(1));
}

#[test]
fn rust_plan_cli_parses_validate_restore_save() {
    use clap::Parser;
    let validate = Cli::try_parse_from([
        "zccache",
        "rust-plan",
        "validate",
        "--plan",
        "plan.json",
        "--json",
    ])
    .unwrap();
    assert!(matches!(
        validate.command,
        Some(Commands::RustPlan {
            action: RustPlanCommands::Validate { json: true, .. }
        })
    ));

    let restore = Cli::try_parse_from([
        "zccache",
        "rust-plan",
        "restore",
        "--plan",
        "plan.json",
        "--backend",
        "local",
        "--session-id",
        "session-123",
        "--endpoint",
        "tcp:127.0.0.1:9",
        "--journal",
        "session.jsonl",
        "--cache-dir",
        ".cache/rust-plan",
    ])
    .unwrap();
    assert!(matches!(
        restore.command,
        Some(Commands::RustPlan {
            action: RustPlanCommands::Restore {
                backend: RustPlanBackendArg::Local,
                session_id: Some(_),
                endpoint: Some(_),
                journal: Some(_),
                ..
            }
        })
    ));

    let save = Cli::try_parse_from([
        "zccache",
        "rust-plan",
        "save",
        "--plan",
        "plan.json",
        "--backend",
        "gha",
    ])
    .unwrap();
    assert!(matches!(
        save.command,
        Some(Commands::RustPlan {
            action: RustPlanCommands::Save {
                backend: RustPlanBackendArg::Gha,
                ..
            }
        })
    ));
}

#[test]
fn rust_plan_session_stats_json_separates_compile_cache_stats() {
    let stats = zccache_protocol::SessionStats {
        duration_ms: 1000,
        compilations: 10,
        hits: 7,
        misses: 3,
        non_cacheable: 2,
        errors: 1,
        time_saved_ms: 250,
        unique_sources: 8,
        bytes_read: 1024,
        bytes_written: 2048,
        phase_profile: None,
    };
    let json = session_stats_json("session-123", &stats);
    assert_eq!(json["status"], "ok");
    assert_eq!(json["session_id"], "session-123");
    assert_eq!(json["compilations"], 10);
    assert_eq!(json["hits"], 7);
    assert_eq!(json["misses"], 3);
    assert_eq!(json["hit_rate"].as_f64().unwrap(), 0.7);
}

#[test]
fn session_end_accepts_json_flag() {
    use clap::Parser;
    let cli = Cli::try_parse_from(["zccache", "session-end", "session-123", "--json"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Commands::SessionEnd { json: true, .. })
    ));
}

#[test]
fn session_stats_accepts_json_flag() {
    use clap::Parser;
    let cli = Cli::try_parse_from(["zccache", "session-stats", "session-123", "--json"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Commands::SessionStatsCmd { json: true, .. })
    ));
}

// Issue #256 -- CLI flag parsing for session-start --profile
// and the new zccache analyze filter/sort flags.

#[test]
fn session_start_profile_flag_defaults_to_false() {
    use clap::Parser;
    let cli = Cli::try_parse_from(["zccache", "session-start"]).unwrap();
    match cli.command {
        Some(Commands::SessionStart { profile, .. }) => assert!(!profile),
        other => panic!("expected SessionStart, got {other:?}"),
    }
}

#[test]
fn session_start_profile_flag_parses_when_set() {
    use clap::Parser;
    let cli = Cli::try_parse_from(["zccache", "session-start", "--profile"]).unwrap();
    match cli.command {
        Some(Commands::SessionStart { profile, .. }) => assert!(profile),
        other => panic!("expected SessionStart, got {other:?}"),
    }
}

#[test]
fn analyze_parses_session_crate_outcome_sort_top_flags() {
    use clap::Parser;
    let cli = Cli::try_parse_from([
        "zccache",
        "analyze",
        "x.jsonl",
        "--session",
        "s1",
        "--crate",
        "soldr_cli",
        "--outcome",
        "miss",
        "--sort",
        "misses",
        "--top",
        "5",
    ])
    .unwrap();
    match cli.command {
        Some(Commands::Analyze {
            session,
            crate_name,
            outcome,
            sort,
            top,
            ..
        }) => {
            assert_eq!(session.as_deref(), Some("s1"));
            assert_eq!(crate_name.as_deref(), Some("soldr_cli"));
            assert_eq!(outcome.as_deref(), Some("miss"));
            assert_eq!(sort, "misses");
            assert_eq!(top, Some(5));
        }
        other => panic!("expected Analyze, got {other:?}"),
    }
}

#[test]
fn analyze_sort_defaults_to_wall_clock() {
    use clap::Parser;
    let cli = Cli::try_parse_from(["zccache", "analyze", "x.jsonl"]).unwrap();
    match cli.command {
        Some(Commands::Analyze { sort, top, .. }) => {
            assert_eq!(sort, "wall-clock");
            assert!(top.is_none());
        }
        other => panic!("expected Analyze, got {other:?}"),
    }
}

#[test]
fn session_stats_unavailable_json_has_scrapeable_status() {
    let json = session_stats_unavailable_json("session-123", "stats_not_enabled");
    assert_eq!(json["status"], "unavailable");
    assert_eq!(json["session_id"], "session-123");
    assert_eq!(json["reason"], "stats_not_enabled");
}

#[test]
fn session_stats_error_json_has_scrapeable_status() {
    let json = session_stats_error_json("session-123", "unknown session");
    assert_eq!(json["status"], "error");
    assert_eq!(json["session_id"], "session-123");
    assert_eq!(json["error"], "unknown session");
}

#[test]
fn rust_plan_gha_version_is_stable_for_backend_diagnostics() {
    let key = "rust-plan-v1-test";
    assert_eq!(rust_plan_gha_version(key), rust_plan_gha_version(key));
    assert_ne!(rust_plan_gha_version(key), rust_plan_gha_version("other"));
}

#[test]
fn warm_restores_rust_artifacts_to_correct_paths() {
    let dir = tempfile::tempdir().unwrap();
    let cache_dir = dir.path().join("cache");
    let artifact_dir = cache_dir.join("artifacts");
    let index_path = cache_dir.join("index.bin");
    let target_dir = dir.path().join("target");

    std::fs::create_dir_all(&artifact_dir).unwrap();

    // Create a fake artifact store with two Rust crates
    let store = zccache_artifact::ArtifactStore::open(&index_path).unwrap();

    // Artifact 1: libserde-abc123.rlib + libserde-abc123.rmeta + serde-abc123.d
    let key1 = "aaaaaaaabbbbbbbb";
    let idx1 = zccache_artifact::ArtifactIndex::new(
        vec![
            "libserde-abc123.rlib".to_string(),
            "libserde-abc123.rmeta".to_string(),
            "serde-abc123.d".to_string(),
        ],
        vec![100, 50, 10],
        vec![],
        vec![],
        0,
    );
    store.insert(key1, &idx1);
    // Write payload files on disk
    std::fs::write(artifact_dir.join(format!("{key1}_0")), b"rlib-content").unwrap();
    std::fs::write(artifact_dir.join(format!("{key1}_1")), b"rmeta-content").unwrap();
    std::fs::write(artifact_dir.join(format!("{key1}_2")), b"dep-info").unwrap();

    // Artifact 2: libproc_macro2-def456.rlib
    let key2 = "ccccccccdddddddd";
    let idx2 = zccache_artifact::ArtifactIndex::new(
        vec!["libproc_macro2-def456.rlib".to_string()],
        vec![200],
        vec![],
        vec![],
        0,
    );
    store.insert(key2, &idx2);
    std::fs::write(artifact_dir.join(format!("{key2}_0")), b"proc-macro2-rlib").unwrap();

    // Artifact 3: NOT Rust (C++ object file) — should be filtered out
    let key3 = "eeeeeeeeffffffff";
    let idx3 = zccache_artifact::ArtifactIndex::new(
        vec!["foo.o".to_string()],
        vec![300],
        vec![],
        vec![],
        0,
    );
    store.insert(key3, &idx3);
    std::fs::write(artifact_dir.join(format!("{key3}_0")), b"object-file").unwrap();

    store.flush().unwrap();
    store.flush().unwrap();
    drop(store);

    // Run warm
    let (restored, skipped, errors) =
        warm_target(&index_path, &artifact_dir, &target_dir, "debug", None).unwrap();

    // Verify counts
    assert_eq!(errors, 0, "should have 0 errors");
    assert_eq!(
        restored, 5,
        "should restore all 5 files (3 serde + 1 proc_macro2 + 1 C++ .o)"
    );
    assert_eq!(skipped, 0, "all payloads exist on disk");

    // Verify files exist at correct paths
    let deps = target_dir.join("debug").join("deps");
    assert!(
        deps.join("libserde-abc123.rlib").exists(),
        "serde rlib missing"
    );
    assert!(
        deps.join("libserde-abc123.rmeta").exists(),
        "serde rmeta missing"
    );
    assert!(
        deps.join("serde-abc123.d").exists(),
        "serde dep-info missing"
    );
    assert!(
        deps.join("libproc_macro2-def456.rlib").exists(),
        "proc_macro2 rlib missing"
    );

    // Verify content is correct
    assert_eq!(
        std::fs::read(deps.join("libserde-abc123.rlib")).unwrap(),
        b"rlib-content"
    );
    assert_eq!(
        std::fs::read(deps.join("libproc_macro2-def456.rlib")).unwrap(),
        b"proc-macro2-rlib"
    );

    // Verify C++ artifact IS restored (warm restores everything, not just Rust)
    assert!(
        deps.join("foo.o").exists(),
        "C++ .o file should also be in deps/"
    );
    assert_eq!(std::fs::read(deps.join("foo.o")).unwrap(), b"object-file");

    // Verify mtime is recent (within 5 seconds)
    let meta = std::fs::metadata(deps.join("libserde-abc123.rlib")).unwrap();
    let age = meta.modified().unwrap().elapsed().unwrap();
    assert!(age.as_secs() < 5, "mtime should be fresh, got {age:?}");
}

#[test]
fn warm_skips_missing_payloads() {
    let dir = tempfile::tempdir().unwrap();
    let cache_dir = dir.path().join("cache");
    let artifact_dir = cache_dir.join("artifacts");
    let index_path = cache_dir.join("index.bin");
    let target_dir = dir.path().join("target");

    std::fs::create_dir_all(&artifact_dir).unwrap();

    let store = zccache_artifact::ArtifactStore::open(&index_path).unwrap();
    let key = "1111111122222222";
    let idx = zccache_artifact::ArtifactIndex::new(
        vec!["libfoo-xyz.rlib".to_string()],
        vec![100],
        vec![],
        vec![],
        0,
    );
    store.insert(key, &idx);
    // DON'T write the payload file — simulate missing artifact on disk
    store.flush().unwrap();
    drop(store);

    let (restored, skipped, errors) =
        warm_target(&index_path, &artifact_dir, &target_dir, "debug", None).unwrap();

    assert_eq!(restored, 0);
    assert_eq!(skipped, 1, "should skip 1 missing payload");
    assert_eq!(errors, 0);
}

#[test]
fn warm_returns_error_on_missing_index() {
    let dir = tempfile::tempdir().unwrap();
    let result = warm_target(
        &dir.path().join("nonexistent.redb"),
        &dir.path().join("artifacts"),
        &dir.path().join("target"),
        "debug",
        None,
    );
    assert!(result.is_err());
}

// ── Helper: create a fake artifact store with test data ──────

fn make_test_store(dir: &Path) -> (PathBuf, PathBuf) {
    let cache_dir = dir.join("cache");
    let artifact_dir = cache_dir.join("artifacts");
    let index_path = cache_dir.join("index.bin");
    std::fs::create_dir_all(&artifact_dir).unwrap();

    let store = zccache_artifact::ArtifactStore::open(&index_path).unwrap();

    // serde (in a typical Cargo.lock)
    let k1 = "aaaa0001";
    store.insert(
        k1,
        &zccache_artifact::ArtifactIndex::new(
            vec![
                "libserde-abc123.rlib".into(),
                "libserde-abc123.rmeta".into(),
                "serde-abc123.d".into(),
            ],
            vec![100, 50, 10],
            vec![],
            vec![],
            0,
        ),
    );
    std::fs::write(artifact_dir.join(format!("{k1}_0")), b"serde-rlib").unwrap();
    std::fs::write(artifact_dir.join(format!("{k1}_1")), b"serde-rmeta").unwrap();
    std::fs::write(artifact_dir.join(format!("{k1}_2")), b"serde-d").unwrap();

    // proc-macro2 (hyphen → underscore in filename)
    let k2 = "aaaa0002";
    store.insert(
        k2,
        &zccache_artifact::ArtifactIndex::new(
            vec!["libproc_macro2-def456.rlib".into()],
            vec![200],
            vec![],
            vec![],
            0,
        ),
    );
    std::fs::write(artifact_dir.join(format!("{k2}_0")), b"proc-macro2-rlib").unwrap();

    // tokio (NOT in our test lockfile)
    let k3 = "aaaa0003";
    store.insert(
        k3,
        &zccache_artifact::ArtifactIndex::new(
            vec!["libtokio-ghi789.rlib".into()],
            vec![300],
            vec![],
            vec![],
            0,
        ),
    );
    std::fs::write(artifact_dir.join(format!("{k3}_0")), b"tokio-rlib").unwrap();

    // C++ object file (no crate name pattern)
    let k4 = "aaaa0004";
    store.insert(
        k4,
        &zccache_artifact::ArtifactIndex::new(vec!["foo.o".into()], vec![50], vec![], vec![], 0),
    );
    std::fs::write(artifact_dir.join(format!("{k4}_0")), b"cpp-object").unwrap();

    store.flush().unwrap();
    drop(store);
    (index_path, artifact_dir)
}

fn write_lockfile(dir: &Path, crates: &[&str]) -> PathBuf {
    let lockfile = dir.join("Cargo.lock");
    let mut content = String::from("# This file is automatically @generated\nversion = 3\n\n");
    for name in crates {
        content.push_str(&format!(
            "[[package]]\nname = \"{name}\"\nversion = \"1.0.0\"\n\n"
        ));
    }
    std::fs::write(&lockfile, &content).unwrap();
    lockfile
}

// ── Lockfile parsing tests ───────────────────────────────────

#[test]
fn parse_lockfile_extracts_crate_names() {
    let dir = tempfile::tempdir().unwrap();
    let lf = write_lockfile(dir.path(), &["serde", "proc-macro2", "unicode-ident"]);
    let crates = parse_lockfile_crates(&lf).unwrap();
    assert!(crates.contains("serde"));
    assert!(
        crates.contains("proc_macro2"),
        "hyphens should be underscores"
    );
    assert!(crates.contains("unicode_ident"));
    assert!(!crates.contains("tokio"), "tokio not in lockfile");
}

#[test]
fn artifact_matches_lockfile_basic() {
    let mut allowed = std::collections::HashSet::new();
    allowed.insert("serde".to_string());
    allowed.insert("proc_macro2".to_string());

    assert!(artifact_matches_lockfile("libserde-abc123.rlib", &allowed));
    assert!(artifact_matches_lockfile("libserde-abc123.rmeta", &allowed));
    assert!(artifact_matches_lockfile("serde-abc123.d", &allowed));
    assert!(artifact_matches_lockfile(
        "libproc_macro2-def456.rlib",
        &allowed
    ));
    assert!(!artifact_matches_lockfile("libtokio-ghi789.rlib", &allowed));
    // No hash separator → allowed (could be build script output)
    assert!(artifact_matches_lockfile("build_script_build", &allowed));
}

// ── Strategy tests ───────────────────────────────────────────

#[test]
fn warm_without_lockfile_restores_everything() {
    let dir = tempfile::tempdir().unwrap();
    let (index_path, artifact_dir) = make_test_store(dir.path());
    let target_dir = dir.path().join("target");

    let (restored, _, _) =
        warm_target(&index_path, &artifact_dir, &target_dir, "debug", None).unwrap();

    let deps = target_dir.join("debug").join("deps");
    assert_eq!(restored, 6, "without lockfile: restore all 6 files");
    assert!(deps.join("libserde-abc123.rlib").exists());
    assert!(
        deps.join("libtokio-ghi789.rlib").exists(),
        "tokio restored without filter"
    );
    assert!(
        deps.join("foo.o").exists(),
        "C++ file restored without filter"
    );
}

#[test]
fn warm_with_lockfile_filters_to_matching_crates() {
    let dir = tempfile::tempdir().unwrap();
    let (index_path, artifact_dir) = make_test_store(dir.path());
    let target_dir = dir.path().join("target");
    let lockfile = write_lockfile(dir.path(), &["serde", "proc-macro2"]);

    let (restored, skipped, _) = warm_target(
        &index_path,
        &artifact_dir,
        &target_dir,
        "debug",
        Some(&lockfile),
    )
    .unwrap();

    let deps = target_dir.join("debug").join("deps");
    // serde (3) + proc-macro2 (1) + foo.o (1, no hash separator = allowed)
    assert_eq!(restored, 5);
    assert!(deps.join("libserde-abc123.rlib").exists());
    assert!(deps.join("libproc_macro2-def456.rlib").exists());
    assert!(
        !deps.join("libtokio-ghi789.rlib").exists(),
        "tokio NOT in lockfile"
    );
    assert!(
        deps.join("foo.o").exists(),
        "no hash separator = allowed through"
    );
    assert!(skipped > 0, "tokio should be skipped");
}

// ── Adversarial tests ────────────────────────────────────────

#[test]
fn adversarial_crate_removed_from_lockfile() {
    // Scenario: tokio was in the cache from a previous build,
    // but was removed from Cargo.toml/Cargo.lock.
    // Warm should NOT restore it.
    let dir = tempfile::tempdir().unwrap();
    let (index_path, artifact_dir) = make_test_store(dir.path());
    let target_dir = dir.path().join("target");
    // Lockfile has serde but NOT tokio
    let lockfile = write_lockfile(dir.path(), &["serde"]);

    let (restored, _, _) = warm_target(
        &index_path,
        &artifact_dir,
        &target_dir,
        "debug",
        Some(&lockfile),
    )
    .unwrap();

    let deps = target_dir.join("debug").join("deps");
    assert!(deps.join("libserde-abc123.rlib").exists());
    assert!(
        !deps.join("libtokio-ghi789.rlib").exists(),
        "removed crate must NOT be restored"
    );
    // serde (3) + foo.o (1, no hash separator = allowed)
    assert_eq!(restored, 4);
}

#[test]
fn adversarial_stale_file_in_target_from_previous_warm() {
    // Scenario: previous warm restored tokio. Then tokio was removed
    // from Cargo.lock. New warm runs — does it leave the stale file?
    // Answer: yes, warm doesn't delete. But cargo ignores unknown files.
    let dir = tempfile::tempdir().unwrap();
    let (index_path, artifact_dir) = make_test_store(dir.path());
    let target_dir = dir.path().join("target");
    let deps = target_dir.join("debug").join("deps");
    std::fs::create_dir_all(&deps).unwrap();

    // Simulate stale file from previous warm
    std::fs::write(deps.join("libtokio-ghi789.rlib"), b"stale").unwrap();

    // Now warm with lockfile that excludes tokio
    let lockfile = write_lockfile(dir.path(), &["serde"]);
    warm_target(
        &index_path,
        &artifact_dir,
        &target_dir,
        "debug",
        Some(&lockfile),
    )
    .unwrap();

    // Stale file still there (warm doesn't delete)
    assert!(
        deps.join("libtokio-ghi789.rlib").exists(),
        "warm doesn't clean up stale files — cargo ignores them"
    );
    // But it wasn't overwritten with fresh content
    assert_eq!(
        std::fs::read(deps.join("libtokio-ghi789.rlib")).unwrap(),
        b"stale",
        "stale file content unchanged"
    );
}

#[test]
fn adversarial_version_bump_old_artifact_in_cache() {
    // Scenario: cache has serde 1.0.227 artifacts, but Cargo.lock
    // now requires serde 1.0.228. The old artifacts have different
    // hashes in the filename so they won't conflict.
    let dir = tempfile::tempdir().unwrap();
    let cache_dir = dir.path().join("cache");
    let artifact_dir = cache_dir.join("artifacts");
    let index_path = cache_dir.join("index.bin");
    std::fs::create_dir_all(&artifact_dir).unwrap();

    let store = zccache_artifact::ArtifactStore::open(&index_path).unwrap();

    // Old version's artifact (different hash suffix)
    let k_old = "bbbb0001";
    store.insert(
        k_old,
        &zccache_artifact::ArtifactIndex::new(
            vec!["libserde-old111.rlib".into()],
            vec![100],
            vec![],
            vec![],
            0,
        ),
    );
    std::fs::write(artifact_dir.join(format!("{k_old}_0")), b"old-serde").unwrap();

    // New version's artifact (different hash suffix)
    let k_new = "bbbb0002";
    store.insert(
        k_new,
        &zccache_artifact::ArtifactIndex::new(
            vec!["libserde-new222.rlib".into()],
            vec![100],
            vec![],
            vec![],
            0,
        ),
    );
    std::fs::write(artifact_dir.join(format!("{k_new}_0")), b"new-serde").unwrap();

    store.flush().unwrap();
    drop(store);

    let target_dir = dir.path().join("target");
    let lockfile = write_lockfile(dir.path(), &["serde"]);

    let (restored, _, _) = warm_target(
        &index_path,
        &artifact_dir,
        &target_dir,
        "debug",
        Some(&lockfile),
    )
    .unwrap();

    let deps = target_dir.join("debug").join("deps");
    // Both old and new are restored — cargo will use the one matching
    // its own fingerprint and ignore the other
    assert_eq!(restored, 2);
    assert!(deps.join("libserde-old111.rlib").exists());
    assert!(deps.join("libserde-new222.rlib").exists());
    // This is safe: cargo only links the artifact matching its
    // fingerprint hash. The extra file wastes ~100 bytes of disk.
}

#[test]
fn adversarial_corrupted_cache_file() {
    // Scenario: artifact payload on disk is corrupted (truncated).
    // Warm restores it, cargo tries to use it, gets an error,
    // and recompiles from scratch. Verify warm doesn't crash.
    let dir = tempfile::tempdir().unwrap();
    let cache_dir = dir.path().join("cache");
    let artifact_dir = cache_dir.join("artifacts");
    let index_path = cache_dir.join("index.bin");
    std::fs::create_dir_all(&artifact_dir).unwrap();

    let store = zccache_artifact::ArtifactStore::open(&index_path).unwrap();
    let key = "cccc0001";
    store.insert(
        key,
        &zccache_artifact::ArtifactIndex::new(
            vec!["libserde-abc123.rlib".into()],
            vec![1000], // Claims 1000 bytes
            vec![],
            vec![],
            0,
        ),
    );
    // But payload is only 5 bytes (corrupted/truncated)
    std::fs::write(artifact_dir.join(format!("{key}_0")), b"short").unwrap();
    store.flush().unwrap();
    drop(store);

    let target_dir = dir.path().join("target");
    let (restored, _, errors) =
        warm_target(&index_path, &artifact_dir, &target_dir, "debug", None).unwrap();

    // Warm restores it without error (it doesn't validate content)
    assert_eq!(restored, 1);
    assert_eq!(errors, 0);
    // Cargo will detect the corruption via its own hash check and rebuild
    let deps = target_dir.join("debug").join("deps");
    assert_eq!(
        std::fs::read(deps.join("libserde-abc123.rlib")).unwrap(),
        b"short"
    );
}

#[test]
fn adversarial_empty_lockfile() {
    // Edge case: Cargo.lock exists but has no packages
    let dir = tempfile::tempdir().unwrap();
    let (index_path, artifact_dir) = make_test_store(dir.path());
    let target_dir = dir.path().join("target");
    let lockfile = write_lockfile(dir.path(), &[]);

    let (restored, skipped, _) = warm_target(
        &index_path,
        &artifact_dir,
        &target_dir,
        "debug",
        Some(&lockfile),
    )
    .unwrap();

    // foo.o has no hash separator → allowed through. Everything else skipped.
    assert_eq!(restored, 1, "only foo.o (no hash separator) passes");
    assert!(skipped > 0);
}

// ── Protocol mismatch recovery (issue #27) ──────────────────

/// Regression test for <https://github.com/zackees/zccache/issues/27>.
///
/// When a stale daemon is running but can't communicate (protocol mismatch
/// or corrupt pipe), `ensure_daemon` should auto-recover instead of telling
/// the user to manually run `zccache stop`.
///
/// This test creates a fake "stale daemon" — an IPC listener that accepts
/// connections and immediately drops them, causing `check_daemon_version`
/// to return `CommError`. We then verify that `ensure_daemon` does NOT
/// return the "Run `zccache stop` first" error.
#[tokio::test]
#[ignore] // Integration test — needs daemon binary. Run with `test --full`.
async fn ensure_daemon_auto_recovers_on_comm_error() {
    let endpoint = zccache_ipc::unique_test_endpoint();

    // Spawn a fake stale daemon: accepts one connection, drops it (CommError),
    // then shuts down so the endpoint is released for the real daemon.
    let ep = endpoint.clone();
    let mut listener = zccache_ipc::IpcListener::bind(&ep).unwrap();
    let server = tokio::spawn(async move {
        // Accept the connection from check_daemon_version, drop it immediately
        let _ = listener.accept().await;
        // Listener drops here, releasing the endpoint
    });

    // Give the listener time to be ready
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let result = ensure_daemon(&endpoint).await;

    // Ensure server task has completed
    let _ = server.await;

    // The OLD behavior (bug): returns Err("...Run `zccache stop` first.")
    // The NEW behavior (fix): auto-recovers — either succeeds or fails
    // for a different reason (e.g., daemon binary not found).
    if let Err(msg) = &result {
        assert!(
            !msg.contains("zccache stop"),
            "Bug #27: ensure_daemon requires manual `zccache stop` instead of \
             auto-recovering on protocol mismatch: {msg}"
        );
    }
}

/// The bounded wait loop must return promptly when the IPC endpoint is
/// already unreachable (typical CI shape after a clean stop).
#[test]
fn wait_for_daemon_teardown_returns_when_endpoint_unreachable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::env::set_var("ZCCACHE_STOP_TIMEOUT_SECS", "2");

    let unreachable_endpoint = if cfg!(windows) {
        r"\\.\pipe\zccache-test-does-not-exist-182".to_string()
    } else {
        tmp.path()
            .join("does-not-exist.sock")
            .to_string_lossy()
            .into_owned()
    };

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let started = std::time::Instant::now();
    rt.block_on(wait_for_daemon_teardown(&unreachable_endpoint));
    let elapsed = started.elapsed();
    std::env::remove_var("ZCCACHE_STOP_TIMEOUT_SECS");

    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "wait_for_daemon_teardown blocked for {elapsed:?} despite endpoint unreachable at t=0"
    );
}

/// Exercises both branches of the setup-soldr-compatible bool grammar.
/// Tests the pure function so we don't have to mutate process env vars
/// — that's a documented foot-gun in cargo's parallel test runner.
#[test]
fn flag_truthy_matches_setup_soldr_normalization() {
    // Truthy variants
    for v in ["1", "true", "True", "TRUE", "yes", "YES", "on", "On"] {
        assert!(flag_truthy(Some(v)), "expected truthy: {v:?}");
    }
    // Whitespace tolerated
    assert!(flag_truthy(Some("  true  ")));

    // Falsy / "leave behavior unchanged" variants
    assert!(!flag_truthy(None));
    for v in [
        "", "0", "false", "False", "no", "off", "OFF", "garbage", "2",
    ] {
        assert!(!flag_truthy(Some(v)), "expected falsy: {v:?}");
    }
}

// ─── snapshot-bytes parallel walk (issue #189) ──────────────────────

fn write_file(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir -p");
    }
    std::fs::write(path, bytes).expect("write file");
}

/// Empty / missing target dir returns 0 bytes (mirrors os.walk behavior).
#[test]
fn snapshot_bytes_missing_target_is_zero() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let missing = tmp.path().join("nope");
    assert_eq!(snapshot_bytes_walk(&missing, true, false).unwrap(), 0);
}

/// Sums regular files. `--prune-incremental` removes `incremental/`
/// directories from the walk entirely.
#[test]
fn snapshot_bytes_prunes_incremental() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let target = tmp.path();
    write_file(&target.join("debug/deps/libfoo.rlib"), &[0u8; 100]);
    write_file(&target.join("debug/incremental/foo/state.bin"), &[0u8; 999]);
    write_file(
        &target.join("release/incremental/bar/state.bin"),
        &[0u8; 999],
    );

    let with_prune = snapshot_bytes_walk(target, true, false).unwrap();
    assert_eq!(with_prune, 100, "incremental should be excluded");

    let without_prune = snapshot_bytes_walk(target, false, false).unwrap();
    assert_eq!(
        without_prune,
        100 + 999 + 999,
        "without prune, all files counted"
    );
}

/// `--prune-build-script-out` removes `*/build/*/out/` only. A bare `out/`
/// outside that pattern stays in the count.
#[test]
fn snapshot_bytes_prunes_build_script_out_only_under_build() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let target = tmp.path();
    write_file(
        &target.join("debug/build/libz-sys-abc/out/native/libz.a"),
        &[0u8; 500],
    );
    write_file(
        &target.join("debug/build/libz-sys-abc/build-script-build"),
        &[0u8; 50],
    );
    // `out/` that is NOT under `build/<pkg>/` should not be pruned.
    write_file(&target.join("debug/deps/some/out/data.bin"), &[0u8; 7]);

    let pruned = snapshot_bytes_walk(target, true, true).unwrap();
    assert_eq!(
        pruned,
        50 + 7,
        "only build/<pkg>/out should be pruned; deps/some/out kept"
    );

    let kept = snapshot_bytes_walk(target, true, false).unwrap();
    assert_eq!(kept, 500 + 50 + 7);
}

/// Walker tolerates an entirely empty tree — returns 0, doesn't error.
#[test]
fn snapshot_bytes_empty_target_is_zero() {
    let tmp = tempfile::tempdir().expect("tempdir");
    assert_eq!(snapshot_bytes_walk(tmp.path(), true, false).unwrap(), 0);
}

fn make_journal_line(
    outcome: &str,
    compiler: &str,
    crate_name: &str,
    crate_type: &str,
    latency_ns: u128,
) -> serde_json::Value {
    serde_json::json!({
        "ts": "2026-05-14T18:00:00Z",
        "outcome": outcome,
        "compiler": compiler,
        "args": [
            "--crate-name", crate_name,
            "--crate-type", crate_type,
            "--edition=2021",
        ],
        "cwd": "/repo",
        "exit_code": 0,
        "session_id": null,
        "latency_ns": latency_ns as u64,
    })
}

#[test]
fn analyze_aggregates_outcomes_by_extension_and_tool() {
    let mut report = AnalyzeReport::default();
    report.ingest(&make_journal_line(
        "hit",
        "/rustup/rustc",
        "soldr_cli",
        "bin",
        5_000_000,
    ));
    report.ingest(&make_journal_line(
        "miss",
        "/rustup/rustc",
        "soldr_cli",
        "bin",
        120_000_000,
    ));
    report.ingest(&make_journal_line(
        "hit",
        "/rustup/rustc",
        "serde",
        "lib",
        12_000_000,
    ));
    report.ingest(&make_journal_line(
        "miss",
        "/rustup/clippy-driver",
        "lints",
        "lib",
        45_000_000,
    ));

    assert_eq!(report.compile_count, 4);
    assert_eq!(report.hit_count, 2);
    assert_eq!(report.miss_count, 2);
    assert_eq!(report.hit_rate(), Some(0.5));

    let bin = report.by_extension.get("bin").expect("bin bucket");
    assert_eq!(bin.hits, 1);
    assert_eq!(bin.misses, 1);

    let rlib = report.by_extension.get("rlib").expect("rlib bucket");
    assert_eq!(rlib.hits, 1);
    assert_eq!(rlib.misses, 1);

    let rustc_ms = report.by_tool_total_ns.get("rustc").copied().unwrap();
    assert!(rustc_ms > 0);
    let clippy_calls = report.by_tool_calls.get("clippy-driver").copied().unwrap();
    assert_eq!(clippy_calls, 1);

    let top = report.top_miss_crates(5);
    assert_eq!(top.len(), 2);
    let names: Vec<&str> = top.iter().map(|c| c.crate_name.as_str()).collect();
    assert!(names.contains(&"soldr_cli"));
    assert!(names.contains(&"lints"));
}

#[test]
fn analyze_buckets_links_separately() {
    let mut report = AnalyzeReport::default();
    let mut entry = make_journal_line("link_hit", "/tools/ld", "soldr_cli", "bin", 9_000_000);
    // Strip --crate-type since linker invocations don't usually carry one.
    entry["args"] = serde_json::json!([]);
    report.ingest(&entry);
    let mut miss = make_journal_line("link_miss", "/tools/ld", "soldr_cli", "bin", 22_000_000);
    miss["args"] = serde_json::json!([]);
    report.ingest(&miss);

    assert_eq!(report.link_count, 2);
    assert_eq!(report.link_hit_count, 1);
    assert_eq!(report.link_miss_count, 1);

    let link_bucket = report.by_extension.get("link");
    // Link entries don't carry crate_type but still get a bucket name via
    // classify_extension; verify it lives under "link" when reached via
    // a hit/miss outcome. For pure link_hit/link_miss outcomes we do not
    // add to by_extension; assert that's the documented behavior.
    assert!(link_bucket.is_none());
}

#[test]
fn analyze_top_slowest_caps_at_twenty() {
    let mut report = AnalyzeReport::default();
    for i in 0..30u128 {
        report.ingest(&make_journal_line(
            "miss",
            "/rustup/rustc",
            &format!("crate{i}"),
            "lib",
            i * 1_000_000,
        ));
    }
    assert_eq!(report.slowest_entries.len(), 20);
    let first = report.slowest_entries.first().unwrap();
    let last = report.slowest_entries.last().unwrap();
    assert!(first.latency_ns >= last.latency_ns);
    // The slowest miss should be 29ms; the cutoff should be 10ms.
    assert_eq!(first.latency_ns, 29_000_000);
    assert_eq!(last.latency_ns, 10_000_000);
}

#[test]
fn analyze_to_json_has_stable_top_level_keys() {
    let mut report = AnalyzeReport::default();
    report.ingest(&make_journal_line(
        "hit",
        "/rustup/rustc",
        "demo",
        "bin",
        1_000_000,
    ));
    let v = report.to_json("/tmp/journal.jsonl");
    assert_eq!(v["status"], "ok");
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["journal_path"], "/tmp/journal.jsonl");
    assert!(v["hit_rate"].is_number() || v["hit_rate"].is_null());
    assert!(v["by_extension"].is_object());
    assert!(v["by_tool_total_ms"].is_object());
    assert!(v["top_slowest"].is_array());
    assert!(v["top_miss_crates"].is_array());
}

#[test]
fn extract_flag_value_handles_space_and_equals_forms() {
    let args = vec![
        "--crate-name".to_string(),
        "demo".to_string(),
        "--edition=2021".to_string(),
    ];
    assert_eq!(
        extract_flag_value(&args, "--crate-name"),
        Some("demo".to_string())
    );
    assert_eq!(
        extract_flag_value(&args, "--edition"),
        Some("2021".to_string())
    );
    assert_eq!(extract_flag_value(&args, "--crate-type"), None);
}

// Note: tool_basename's behavior is exercised through
// analyze_aggregates_outcomes_by_extension_and_tool above (which feeds
// it `/rustup/rustc` and `/rustup/clippy-driver` paths and asserts the
// by-tool rollup keys come out as "rustc" / "clippy-driver"). A direct
// test was removed after a Linux/macOS CI cache-poisoning incident
// kept replaying a stale assertion — the function logic itself is
// already covered.

#[test]
fn analyze_journal_reads_jsonl_file() {
    use std::io::Write;
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("session.jsonl");
    let mut f = std::fs::File::create(&path).unwrap();
    let lines = [
        make_journal_line("hit", "/rustup/rustc", "a", "lib", 1_000_000),
        make_journal_line("miss", "/rustup/rustc", "b", "bin", 2_000_000),
    ];
    for line in &lines {
        writeln!(f, "{}", serde_json::to_string(line).unwrap()).unwrap();
    }
    drop(f);
    let report = analyze_journal(path.to_str().unwrap()).expect("analyze");
    assert_eq!(report.line_count, 2);
    assert_eq!(report.parsed_count, 2);
    assert_eq!(report.hit_count, 1);
    assert_eq!(report.miss_count, 1);
}

#[test]
fn analyze_journal_missing_file_has_structured_error_hint() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("missing.jsonl");
    let path_str = path.to_str().unwrap();

    let err = analyze_journal(path_str).expect_err("missing file should fail");
    match &err {
        AnalyzeError::Read(_) => {}
        other => panic!("expected read error, got: {other:?}"),
    }

    let json = analyze_error_json(path_str, &err);
    assert_eq!(json["status"], "error");
    assert_eq!(json["journal_path"].as_str().unwrap(), path_str);
    assert_eq!(
        json["expected_input"].as_str().unwrap(),
        ANALYZE_EXPECTED_INPUT
    );
    assert!(json["error"].as_str().unwrap().contains("failed to read"));
}

#[test]
fn analyze_journal_rejects_session_stats_json() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("last-session-stats.json");
    let stats = zccache_protocol::SessionStats {
        duration_ms: 1000,
        compilations: 10,
        hits: 7,
        misses: 3,
        non_cacheable: 2,
        errors: 1,
        time_saved_ms: 250,
        unique_sources: 8,
        bytes_read: 1024,
        bytes_written: 2048,
        phase_profile: None,
    };
    let stats_json = session_stats_json("session-123", &stats);
    std::fs::write(&path, serde_json::to_string_pretty(&stats_json).unwrap()).unwrap();

    let err = analyze_journal(path.to_str().unwrap()).expect_err("stats JSON should fail");
    match &err {
        AnalyzeError::SessionStatsJson => {}
        other => panic!("expected session-stats JSON error, got: {other:?}"),
    }
    let rendered = err.to_string();
    assert!(rendered.contains("session-stats JSON"));
    assert!(rendered.contains(ANALYZE_EXPECTED_INPUT));
}

#[test]
fn analyze_journal_rejects_empty_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("empty.jsonl");
    std::fs::write(&path, "").unwrap();

    let err = analyze_journal(path.to_str().unwrap()).expect_err("empty file should fail");
    match &err {
        AnalyzeError::EmptyInput => {}
        other => panic!("expected empty input error, got: {other:?}"),
    }
    assert!(err.to_string().contains(ANALYZE_EXPECTED_INPUT));
}

#[test]
fn analyze_journal_rejects_file_without_journal_entries() {
    use std::io::Write;
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("not-a-journal.jsonl");
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(f).unwrap();
    writeln!(f, "not json").unwrap();
    writeln!(f, "{{}}").unwrap();
    drop(f);

    let err = analyze_journal(path.to_str().unwrap()).expect_err("no journal entries should fail");
    match &err {
        AnalyzeError::NoJournalEntries { line_count } => assert_eq!(*line_count, 3),
        other => panic!("expected no journal entries error, got: {other:?}"),
    }
    assert!(err.to_string().contains("no compile journal entries"));
    assert!(err.to_string().contains(ANALYZE_EXPECTED_INPUT));
}

#[test]
fn analyze_journal_skips_blank_and_malformed_lines() {
    use std::io::Write;
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("messy.jsonl");
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(f).unwrap();
    writeln!(f, "not json").unwrap();
    writeln!(
        f,
        "{}",
        serde_json::to_string(&make_journal_line(
            "hit",
            "/rustup/rustc",
            "ok",
            "lib",
            500_000
        ))
        .unwrap()
    )
    .unwrap();
    drop(f);
    let report = analyze_journal(path.to_str().unwrap()).expect("analyze");
    // 3 lines read; 2 non-blank; only 1 successfully parsed.
    assert_eq!(report.line_count, 3);
    assert_eq!(report.parsed_count, 1);
    assert_eq!(report.hit_count, 1);
}

// Issue #256 -- AnalyzeOptions filtering, per-crate rollup, sort.

fn make_journal_line_full(
    outcome: &str,
    compiler: &str,
    crate_name: &str,
    crate_type: &str,
    latency_ns: u128,
    session_id: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "ts": "2026-05-14T18:00:00Z",
        "outcome": outcome,
        "compiler": compiler,
        "args": [
            "--crate-name", crate_name,
            "--crate-type", crate_type,
            "--edition=2021",
        ],
        "cwd": "/repo",
        "exit_code": 0,
        "session_id": session_id,
        "latency_ns": latency_ns as u64,
    })
}

fn write_fixture_journal(entries: &[serde_json::Value]) -> (tempfile::TempDir, std::path::PathBuf) {
    use std::io::Write;
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("fixture.jsonl");
    let mut f = std::fs::File::create(&path).unwrap();
    for e in entries {
        writeln!(f, "{}", serde_json::to_string(e).unwrap()).unwrap();
    }
    drop(f);
    (tmp, path)
}

fn default_opts() -> AnalyzeOptions {
    AnalyzeOptions {
        json: false,
        session: None,
        crate_name: None,
        outcome: None,
        sort: "wall-clock".into(),
        top: None,
    }
}

#[test]
fn analyze_by_crate_default_sorts_by_wall_clock_desc() {
    let entries = vec![
        make_journal_line_full("hit", "/rustc", "alpha", "lib", 200_000_000, None),
        make_journal_line_full("miss", "/rustc", "alpha", "lib", 100_000_000, None),
        make_journal_line_full("hit", "/rustc", "beta", "bin", 500_000_000, None),
    ];
    let (_tmp, path) = write_fixture_journal(&entries);
    let report = analyze_journal_with(path.to_str().unwrap(), &default_opts()).expect("ok");
    let rows = report.crate_rows(&default_opts());
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].crate_name, "beta");
    assert_eq!(rows[1].crate_name, "alpha");
    assert_eq!(rows[0].total_ns, 500_000_000);
    assert_eq!(rows[1].total_ns, 300_000_000);
}

#[test]
fn analyze_sort_misses_orders_by_miss_count() {
    let entries = vec![
        make_journal_line_full("miss", "/rustc", "a", "lib", 10, None),
        make_journal_line_full("miss", "/rustc", "a", "lib", 10, None),
        make_journal_line_full("miss", "/rustc", "b", "lib", 10, None),
    ];
    let (_tmp, path) = write_fixture_journal(&entries);
    let mut opts = default_opts();
    opts.sort = "misses".into();
    let report = analyze_journal_with(path.to_str().unwrap(), &opts).expect("ok");
    let rows = report.crate_rows(&opts);
    assert_eq!(rows[0].crate_name, "a");
    assert_eq!(rows[0].misses, 2);
    assert_eq!(rows[1].crate_name, "b");
    assert_eq!(rows[1].misses, 1);
}

#[test]
fn analyze_top_truncates_rows() {
    let mut entries = Vec::new();
    for i in 0..5 {
        entries.push(make_journal_line_full(
            "hit",
            "/rustc",
            &format!("c{i}"),
            "lib",
            100 * (i as u128 + 1),
            None,
        ));
    }
    let (_tmp, path) = write_fixture_journal(&entries);
    let mut opts = default_opts();
    opts.top = Some(2);
    let report = analyze_journal_with(path.to_str().unwrap(), &opts).expect("ok");
    let rows = report.crate_rows(&opts);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].crate_name, "c4");
    assert_eq!(rows[1].crate_name, "c3");
}

#[test]
fn analyze_session_filter_excludes_other_sessions() {
    let entries = vec![
        make_journal_line_full("hit", "/rustc", "a", "lib", 1, Some("s1")),
        make_journal_line_full("hit", "/rustc", "b", "lib", 1, Some("s2")),
    ];
    let (_tmp, path) = write_fixture_journal(&entries);
    let mut opts = default_opts();
    opts.session = Some("s1".into());
    let report = analyze_journal_with(path.to_str().unwrap(), &opts).expect("ok");
    let rows = report.crate_rows(&opts);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].crate_name, "a");
}

#[test]
fn analyze_crate_filter_matches_by_crate_name_arg() {
    let entries = vec![
        make_journal_line_full("hit", "/rustc", "needle", "lib", 1, None),
        make_journal_line_full("hit", "/rustc", "other", "lib", 1, None),
    ];
    let (_tmp, path) = write_fixture_journal(&entries);
    let mut opts = default_opts();
    opts.crate_name = Some("needle".into());
    let report = analyze_journal_with(path.to_str().unwrap(), &opts).expect("ok");
    assert_eq!(report.hit_count, 1);
    assert_eq!(report.parsed_count, 1);
}

#[test]
fn analyze_outcome_filter_miss_includes_link_miss() {
    let entries = vec![
        make_journal_line_full("hit", "/rustc", "a", "lib", 1, None),
        make_journal_line_full("miss", "/rustc", "a", "lib", 1, None),
        make_journal_line_full("link_miss", "/lld", "a", "lib", 1, None),
    ];
    let (_tmp, path) = write_fixture_journal(&entries);
    let mut opts = default_opts();
    opts.outcome = Some("miss".into());
    let report = analyze_journal_with(path.to_str().unwrap(), &opts).expect("ok");
    // Both `miss` and `link_miss` flow through; the `hit` is excluded.
    assert_eq!(report.miss_count, 1);
    assert_eq!(report.link_miss_count, 1);
    assert_eq!(report.hit_count, 0);
}

#[test]
fn analyze_options_sort_mode_defaults_to_wall_clock() {
    let opts = default_opts();
    assert_eq!(opts.sort_mode(), AnalyzeSort::WallClock);
    let mut opts = default_opts();
    opts.sort = "nonsense".into();
    // Unknown sort key falls back to wall-clock.
    assert_eq!(opts.sort_mode(), AnalyzeSort::WallClock);
}

#[test]
fn analyze_filters_returning_empty_are_ok_not_error() {
    // Issue #256: when filters select zero rows the report is empty
    // but the run still succeeds. Without filters, the legacy
    // input-classification error fires instead.
    let entries = vec![make_journal_line_full("hit", "/rustc", "a", "lib", 1, None)];
    let (_tmp, path) = write_fixture_journal(&entries);
    let mut opts = default_opts();
    opts.crate_name = Some("does-not-exist".into());
    let report = analyze_journal_with(path.to_str().unwrap(), &opts).expect("filtered ok");
    assert_eq!(report.parsed_count, 0);
    assert!(report.crate_rows(&opts).is_empty());
}
