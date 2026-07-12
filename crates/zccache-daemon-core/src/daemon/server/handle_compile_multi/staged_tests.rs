//! Fault tests for multi-source publication and salvage.

use super::*;

fn unit(requested: NormalizedPath, staged: NormalizedPath) -> StagedMultiUnitPlan {
    StagedMultiUnitPlan {
        compilation_index: 0,
        rewritten_args: Vec::new(),
        outputs: vec![StagedOutputPlan { requested, staged }],
        staged_depfile: None,
    }
}

fn metadata() -> ArtifactIndex {
    let empty = Arc::new(Vec::new());
    ArtifactIndex::new(
        vec!["result.o".into()],
        vec![21],
        Arc::clone(&empty),
        empty,
        0,
    )
}

#[tokio::test]
async fn input_aba_mutation_after_precheck_skips_publication_identity() {
    let temp = tempfile::tempdir().unwrap();
    let cache_dir: NormalizedPath = temp.path().join("cache").into();
    let server =
        DaemonServer::bind_with_cache_dir(&crate::ipc::unique_test_endpoint(), &cache_dir).unwrap();
    let source: NormalizedPath = temp.path().join("input.c").into();
    let requested: NormalizedPath = temp.path().join("input.o").into();
    std::fs::write(&source, "int value(void) { return 1; }\n").unwrap();
    let args = Arc::<[String]>::from(vec!["-c".into(), source.to_string_lossy().into_owned()]);
    let compilation = crate::compiler::CacheableCompilation {
        compiler: "clang".into(),
        family: crate::compiler::CompilerFamily::Clang,
        source_file: source.clone(),
        output_file: requested.clone(),
        original_args: args,
        unknown_flags: Vec::new(),
    };
    let original_mtime =
        filetime::FileTime::from_last_modification_time(&std::fs::metadata(&source).unwrap());
    let checked = check_unit_cache(
        &server.state,
        &compilation,
        temp.path(),
        &NormalizedPath::from(temp.path()),
        &[],
        None,
        std::time::Instant::now(),
    );
    assert!(matches!(checked, UnitCacheResult::Miss { .. }));
    std::fs::write(&source, "int value(void) { return 222; }\n").unwrap();
    std::fs::write(&source, "int value(void) { return 1; }\n").unwrap();
    filetime::set_file_mtime(&source, original_mtime).unwrap();
    let staged: NormalizedPath = temp.path().join("private/input.o").into();
    std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
    std::fs::write(&staged, b"object built from the old source").unwrap();
    let prepared = prepare_unit(
        &server.state,
        &checked,
        &unit(requested, staged),
        &NormalizedPath::from(temp.path()),
        Arc::new(Vec::new()),
        Arc::new(Vec::new()),
    )
    .unwrap();
    assert!(prepared.artifact_key_hex.is_none());
    assert!(prepared.metadata.is_none());
}

#[test]
fn successful_exit_with_empty_output_is_rejected_before_publication() {
    let temp = tempfile::tempdir().unwrap();
    let requested: NormalizedPath = temp.path().join("result.o").into();
    let staged: NormalizedPath = temp.path().join("private/result.o").into();
    std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
    std::fs::write(&staged, []).unwrap();

    let error = validate_staged_outputs(&unit(requested, staged)).unwrap_err();
    assert!(error.contains("empty output"), "unexpected error: {error}");
}

#[tokio::test]
async fn publication_failure_salvages_multi_output_without_cacheability() {
    let temp = tempfile::tempdir().unwrap();
    let cache_dir: NormalizedPath = temp.path().join("cache").into();
    let server =
        DaemonServer::bind_with_cache_dir(&crate::ipc::unique_test_endpoint(), &cache_dir).unwrap();
    let requested: NormalizedPath = temp.path().join("result.o").into();
    let staged: NormalizedPath = temp.path().join("private/result.o").into();
    std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
    std::fs::write(&staged, b"complete multi output").unwrap();
    let unit = unit(requested.clone(), staged);
    let fault = StagedFaultGuard::arm(&server.state.artifact_dir, [StagedFaultPoint::IndexCommit]);

    let cacheable = publish_and_materialize_multi_unit(
        &server.state,
        &unit,
        Some(&"4".repeat(64)),
        Some(metadata()),
    )
    .unwrap();
    assert!(!cacheable);
    assert_eq!(std::fs::read(requested).unwrap(), b"complete multi output");
    let profile = server.state.profiler.staged.snapshot();
    assert_eq!(profile.counters["publication_failure"], 1);
    assert_eq!(profile.counters["salvage_success"], 1);
    fault.assert_all_consumed();
}

#[tokio::test]
async fn failed_materialization_prevents_publication_visibility() {
    let temp = tempfile::tempdir().unwrap();
    let cache_dir: NormalizedPath = temp.path().join("cache").into();
    let server =
        DaemonServer::bind_with_cache_dir(&crate::ipc::unique_test_endpoint(), &cache_dir).unwrap();
    let requested: NormalizedPath = temp.path().join("result.o").into();
    let staged: NormalizedPath = temp.path().join("private/result.o").into();
    std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
    std::fs::write(&staged, b"complete multi output").unwrap();
    let unit = unit(requested.clone(), staged);
    let materialize_fault =
        StagedFaultGuard::arm(&requested, [StagedFaultPoint::MaterializeOutput(0)]);

    publish_and_materialize_multi_unit(
        &server.state,
        &unit,
        Some(&"5".repeat(64)),
        Some(metadata()),
    )
    .unwrap_err();
    assert!(!requested.exists());
    let profile = server.state.profiler.staged.snapshot();
    assert_eq!(profile.counters["publication_success"], 0);
    assert_eq!(profile.counters["publication_failure"], 0);
    assert_eq!(profile.counters["materialize_failure"], 1);
    assert!(
        load_staged_artifact_paths(&server.state.artifact_dir, &"5".repeat(64), &[21])
            .unwrap()
            .is_none()
    );
    materialize_fault.assert_all_consumed();
}
