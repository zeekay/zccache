"""Regression tests for the embedded-zccache perf-cluster bootstrap."""

from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = ROOT / ".github" / "workflows" / "perf-rust-cluster.yml"
LOCAL_ENTRYPOINT = ROOT / "ci" / "docker" / "perf_entrypoint.sh"
LOCAL_RUNNER = ROOT / "ci" / "docker" / "runner.Dockerfile"
LOCAL_ORCHESTRATOR = ROOT / "ci" / "perf_local.py"


def workflow_text() -> str:
    return WORKFLOW.read_text(encoding="utf-8")


def test_perf_cluster_builds_soldr_with_the_zccache_commit_under_test() -> None:
    workflow = workflow_text()
    pin_step_name = "Pin zccache source into soldr build"
    pin_step = workflow.split(f"- name: {pin_step_name}", 1)[1].split(
        "\n      - name:", 1
    )[0]

    assert 'git -C soldr-src/_vender/zccache fetch origin "$GITHUB_SHA"' in pin_step
    assert (
        'git -C soldr-src/_vender/zccache checkout --detach "$GITHUB_SHA"' in pin_step
    )
    assert 'rev-parse HEAD)" = "$GITHUB_SHA"' in pin_step
    assert workflow.index(pin_step_name) < workflow.index("Build soldr (release)")
    assert "${{ github.sha }}" in workflow.split("key: soldr-", 1)[1].splitlines()[0]


def test_perf_cluster_does_not_use_removed_runtime_zccache_pinning() -> None:
    workflow = workflow_text()

    assert "soldr update-zccache" not in workflow


def test_perf_local_does_not_use_removed_runtime_zccache_pinning() -> None:
    local_harness = "\n".join(
        path.read_text(encoding="utf-8")
        for path in (LOCAL_ENTRYPOINT, LOCAL_RUNNER, LOCAL_ORCHESTRATOR)
    )

    assert "soldr update-zccache" not in local_harness
    assert "/zccache-bin" not in local_harness
    assert '"--skip-soldr-build",' not in LOCAL_ORCHESTRATOR.read_text(encoding="utf-8")


def test_perf_local_lists_daemon_runtime_without_copying_special_files() -> None:
    entrypoint = LOCAL_ENTRYPOINT.read_text(encoding="utf-8")

    assert "warm-daemon-files.txt" in entrypoint
    assert (
        'copy_if_exists "${scenario_root}/cache-warm/cache/soldr-daemon"'
        not in entrypoint
    )


def test_perf_cluster_pins_cache_action_to_an_immutable_commit() -> None:
    workflow = workflow_text()
    expected_sha = "0057852bfaa89a56745cba8c7296529d2fc39830"
    cache_refs = [
        line.split("actions/cache@", 1)[1].split()[0]
        for line in workflow.splitlines()
        if "uses: actions/cache@" in line
    ]

    assert cache_refs == [expected_sha, expected_sha]


def test_perf_cluster_final_rollout_matrix_and_defaults_are_required() -> None:
    workflow = workflow_text()

    assert workflow.count("default: all") >= 3
    assert "platform: [linux, mac-arm, win]" in workflow
    for runner in ("ubuntu-24.04", "macos-14", "windows-2025"):
        assert workflow.count(f"runs_on: {runner}") >= 3
    assert "fixture: [medium, sqlite-link]" in workflow
    assert (
        "cold-tar-untar-warm|restore-no-clean-warm|worktree-share|touch-no-change) "
        'echo "fail"'
    ) in workflow


def test_perf_cluster_gates_staged_metrics_and_restore_noop() -> None:
    workflow = workflow_text()

    for required in (
        "MAX_STAGED_OVERHEAD_MS",
        "MAX_MATERIALIZATION_COPIED_BYTES",
        "publication_success",
        "materialize_reflink",
        "materialize_hardlink_shared",
        "materialize_copy",
        "salvage_attempt",
        "materialize_failure",
        "warm_compilations",
        "restore warm build was not a no-op",
    ):
        assert required in workflow


def test_perf_cluster_normalizes_windows_temp_for_git_bash_tools() -> None:
    workflow = workflow_text()
    run_step = workflow.split("- name: Run selected scenarios", 1)[1].split(
        "\n      - name:", 1
    )[0]

    assert 'runner_temp="$(cygpath -u "${RUNNER_TEMP}")"' in run_step
    assert 'out_root="${runner_temp}/perf-${M_PLATFORM}-${M_FIXTURE}"' in run_step
    assert 'artifact_root="$(cygpath -w "${out_root}")"' in run_step
    assert 'echo "out_root=${artifact_root}" >> "$GITHUB_OUTPUT"' in run_step
