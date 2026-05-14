import json
from pathlib import Path

from ci import benchmark_stats, perf_guard


PASSING_LOG = """
## C Benchmark: 50 .c files, 5 warm trials

| Scenario | Bare clang | sccache | zccache | vs sccache | vs bare clang |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| Single-file, Cold | 3.000s | 3.200s | 2.000s | 1.6x faster | 1.5x faster |
| Single-file, Warm | 3.000s | 2.000s | **1.000s** | **2.0x faster** | **3.0x faster** |

## Benchmark: 50 C++ files, 5 warm trials

| Scenario | Bare Clang | sccache | zccache | vs sccache | vs bare clang |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| Single-file, Cold | 6.000s | 7.000s | 4.000s | 1.8x faster | 1.5x faster |
| Single-file, Warm | 6.000s | 1.500s | **0.050s** | **30x faster** | **120x faster** |

## Rust Benchmark: 50 .rs files, 5 warm trials

| Scenario | Bare rustc | sccache | zccache | vs sccache | vs bare rustc |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| Build, Cold | 9.000s | 10.000s | 6.000s | 1.7x faster | 1.5x faster |
| Build, Warm | 9.000s | 8.000s | **0.100s** | **80x faster** | **90x faster** |
"""


FAILING_RUST_LOG = PASSING_LOG.replace(
    "| Build, Cold | 9.000s | 10.000s | 6.000s | 1.7x faster | 1.5x faster |",
    "| Build, Cold | 9.000s | 10.000s | 7.000s | 1.4x faster | 1.3x faster |",
)

FAILING_SCCACHE_LOG = PASSING_LOG.replace(
    "| Single-file, Cold | 6.000s | 7.000s | 4.000s | 1.8x faster | 1.5x faster |",
    "| Single-file, Cold | 6.000s | 5.000s | 4.000s | 1.2x faster | 1.5x faster |",
)


MISSING_C_LOG = """
## Benchmark: 50 C++ files, 5 warm trials

| Scenario | Bare Clang | sccache | zccache | vs sccache | vs bare clang |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| Single-file, Cold | 6.000s | 7.000s | 4.000s | 1.8x faster | 1.5x faster |
| Single-file, Warm | 6.000s | 1.500s | **0.050s** | **30x faster** | **120x faster** |

## Rust Benchmark: 50 .rs files, 5 warm trials

| Scenario | Bare rustc | sccache | zccache | vs sccache | vs bare rustc |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| Build, Cold | 9.000s | 10.000s | 6.000s | 1.7x faster | 1.5x faster |
| Build, Warm | 9.000s | 8.000s | **0.100s** | **80x faster** | **90x faster** |
"""


NEAR_BARE_COLD_C_LOG = """
## C Benchmark: 50 .c files, 5 warm trials

| Scenario | Bare clang | sccache | zccache | vs sccache | vs bare clang |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| Single-file, Cold | 3.000s | 4.000s | 3.300s | 1.2x faster | 1.1x slower |
| Single-file, Warm | 3.000s | 2.000s | **0.100s** | **20x faster** | **30x faster** |
"""


def rows(log: str):
    return benchmark_stats.parse_benchmark_log(log)


def test_exact_threshold_passes():
    report = perf_guard.evaluate_attempts([rows(PASSING_LOG)], threshold=1.5)

    assert report.passed
    assert not report.missing_requirements


def test_below_threshold_fails():
    report = perf_guard.evaluate_attempts([rows(FAILING_RUST_LOG)], threshold=1.5)

    assert not report.passed
    failing = [status for status in report.statuses if not status.passed]
    assert [(status.language, status.scenario, status.baseline) for status in failing] == [
        ("rust", "Build, Cold", "bare"),
        ("rust", "Build, Cold", "sccache"),
    ]


def test_sccache_threshold_is_enforced_separately_from_bare():
    report = perf_guard.evaluate_attempts([rows(FAILING_SCCACHE_LOG)], threshold=1.5)

    assert not report.passed
    failing = [status for status in report.statuses if not status.passed]
    assert [(status.language, status.scenario, status.baseline) for status in failing] == [
        ("c++", "Single-file, Cold", "sccache")
    ]


def test_default_cold_thresholds_allow_near_bare_misses():
    report = perf_guard.evaluate_attempts(
        [rows(NEAR_BARE_COLD_C_LOG)],
        languages=("c",),
    )

    assert report.passed
    cold_statuses = [status for status in report.statuses if status.mode == "cold"]
    assert [(status.baseline, status.threshold) for status in cold_statuses] == [
        ("bare", 0.85),
        ("sccache", 1.0),
    ]


def test_retry_passes_when_later_attempt_clears_threshold():
    report = perf_guard.evaluate_attempts(
        [rows(FAILING_RUST_LOG), rows(PASSING_LOG)],
        threshold=1.5,
    )

    assert report.passed
    rust_cold = [
        status
        for status in report.statuses
        if status.language == "rust" and status.scenario == "Build, Cold"
    ][0]
    assert rust_cold.best_attempt == 2
    assert rust_cold.best_ratio == 1.5


def test_missing_required_language_mode_fails():
    report = perf_guard.evaluate_attempts([rows(MISSING_C_LOG)], threshold=1.5)

    assert not report.passed
    assert report.missing_requirements == ["c cold", "c warm"]


def test_language_filter_requires_only_selected_language():
    report = perf_guard.evaluate_attempts(
        [rows(MISSING_C_LOG)],
        threshold=1.5,
        languages=("rust",),
    )

    assert report.passed
    assert not report.missing_requirements
    assert {status.language for status in report.statuses} == {"rust"}


def test_all_command_failures_fail_even_when_rows_parse():
    report = perf_guard.evaluate_attempts(
        [rows(PASSING_LOG), rows(PASSING_LOG)],
        threshold=1.5,
        command_failures=[1, 2],
    )

    assert not report.passed


def test_report_marks_failed_rows():
    report = perf_guard.evaluate_attempts([rows(FAILING_RUST_LOG)], threshold=1.5)
    markdown = perf_guard.format_report(report, 1.5, 1.5, 1.5, 1.5)

    assert "| FAIL | rust | Rust rustc | Build, Cold | Bare rustc | 1.286x | 1.50x | 1 | 1 |" in markdown
    assert "### Benchmark summary" in markdown
    assert "#### Failed checks" in markdown
    assert "#### Passed checks" in markdown


def test_benchmark_summary_lists_passes_and_failures():
    report = perf_guard.evaluate_attempts([rows(FAILING_SCCACHE_LOG)], threshold=1.5)

    summary = perf_guard.format_benchmark_summary(report)

    assert "- Passed checks: 11" in summary
    assert "- Failed checks: 1" in summary
    assert "- Missing coverage: none" in summary
    assert "- Failed benchmark attempts: none" in summary
    assert (
        "- FAIL: c++ C++ inline args / Single-file, Cold vs sccache: "
        "expected >= 1.50x, actual 1.250x (best attempt 1; seen 1)"
    ) in summary
    assert (
        "- PASS: c C inline args / Single-file, Warm vs Bare clang: "
        "expected >= 1.50x, actual 3.000x (best attempt 1; seen 1)"
    ) in summary


def test_final_status_explains_pass_with_weakest_check():
    report = perf_guard.evaluate_attempts(
        [rows(NEAR_BARE_COLD_C_LOG)],
        languages=("c",),
    )

    final_status = perf_guard.format_final_status(report)

    assert final_status == (
        "PERF GUARD OK: all checks meet configured floors; weakest check "
        "c C inline args / Single-file, Cold vs Bare clang: expected >= 0.85x, "
        "actual 0.909x."
    )


def test_final_status_explains_worst_failed_floor():
    report = perf_guard.evaluate_attempts([rows(FAILING_SCCACHE_LOG)], threshold=1.5)

    final_status = perf_guard.format_final_status(report)

    assert final_status == (
        "PERF GUARD FAILED: 1 check below floor; worst c++ C++ inline args / "
        "Single-file, Cold vs sccache: expected >= 1.50x, actual 1.250x."
    )


def test_final_status_explains_missing_coverage():
    report = perf_guard.evaluate_attempts([rows(MISSING_C_LOG)], threshold=1.5)

    final_status = perf_guard.format_final_status(report)

    assert final_status == (
        "PERF GUARD FAILED: missing required benchmark coverage for c cold, c warm."
    )


def test_report_json_marks_thresholds_and_failed_statuses():
    report = perf_guard.evaluate_attempts([rows(FAILING_RUST_LOG)], threshold=1.5)
    payload = perf_guard.format_report_json(
        report,
        1.5,
        1.5,
        cold_bare_threshold=1.5,
        cold_sccache_threshold=1.5,
    )

    assert payload["schema_version"] == 1
    assert payload["passed"] is False
    assert payload["languages"] == ["c", "c++", "rust"]
    assert payload["thresholds"] == {
        "bare": 1.5,
        "sccache": 1.5,
        "cold_bare": 1.5,
        "cold_sccache": 1.5,
    }
    failing = [
        status
        for status in payload["statuses"]
        if status["language"] == "rust"
        and status["scenario"] == "Build, Cold"
        and not status["passed"]
    ]
    assert {status["baseline"] for status in failing} == {"bare", "sccache"}


def test_report_json_passed_is_boolean_when_no_rows_parse():
    report = perf_guard.evaluate_attempts([[]], threshold=1.5)
    payload = perf_guard.format_report_json(report, 1.5, 1.5)

    assert payload["passed"] is False


def test_writes_perf_guard_json_artifacts(tmp_path):
    parsed_rows = rows(PASSING_LOG)
    report = perf_guard.evaluate_attempts([parsed_rows], threshold=1.5)

    perf_guard.write_attempt_json(
        tmp_path,
        1,
        parsed_rows,
        0,
        source="unit-test",
        language="c",
    )
    perf_guard.write_report_json(
        tmp_path,
        report,
        1.5,
        1.5,
        ("c",),
        cold_bare_threshold=1.5,
        cold_sccache_threshold=1.5,
    )

    attempt_payload = json.loads((tmp_path / "attempt-1.json").read_text(encoding="utf-8"))
    summary_payload = json.loads(
        (tmp_path / "perf-guard-summary.json").read_text(encoding="utf-8")
    )

    assert attempt_payload["source"] == "unit-test"
    assert attempt_payload["language"] == "c"
    assert attempt_payload["returncode"] == 0
    assert attempt_payload["row_count"] == len(parsed_rows)
    assert summary_payload["passed"] is True
    assert summary_payload["languages"] == ["c"]
    assert all(status["passed"] for status in summary_payload["statuses"])


def test_writes_perf_guard_final_status_artifact(tmp_path):
    final_status = "PERF GUARD OK: all checks meet configured floors."

    perf_guard.write_final_status(tmp_path, final_status)

    assert (tmp_path / "perf-guard-result.txt").read_text(encoding="utf-8") == (
        final_status + "\n"
    )


def test_benchmark_language_commands_are_filtered():
    c_commands = benchmark_stats.benchmark_commands_for_language("c")
    cpp_commands = benchmark_stats.benchmark_commands_for_language("c++")
    rust_commands = benchmark_stats.benchmark_commands_for_language("rust")

    assert [command[-4] for command in c_commands] == ["perf_c_zccache_vs_bare"]
    assert [command[-4] for command in cpp_commands] == [
        "perf_warm_cache_zccache_vs_sccache",
        "perf_response_file",
        "perf_cpp_sibling_remap_warm",
    ]
    assert [command[-4] for command in rust_commands] == [
        "perf_rustc_zccache_vs_sccache",
        "perf_rustc_sibling_remap_warm",
    ]


def test_prebuilt_benchmark_binary_commands_are_filtered():
    commands = perf_guard._benchmark_commands("c++", Path("perf_bench_test"))

    assert commands == [
        [
            "perf_bench_test",
            "perf_warm_cache_zccache_vs_sccache",
            "--nocapture",
            "--ignored",
            "--test-threads=1",
        ],
        [
            "perf_bench_test",
            "perf_response_file",
            "--nocapture",
            "--ignored",
            "--test-threads=1",
        ],
        [
            "perf_bench_test",
            "perf_cpp_sibling_remap_warm",
            "--nocapture",
            "--ignored",
            "--test-threads=1",
        ],
    ]


def test_benchmark_env_uses_auto_priority_and_profiles_rust(
    tmp_path, monkeypatch
):
    monkeypatch.setenv("RUSTC_WRAPPER", "sccache")

    env = perf_guard._benchmark_env(tmp_path, "rust")

    assert env["ZCCACHE_CACHE_DIR"] == str(tmp_path)
    assert env["ZCCACHE_COMPILE_PRIORITY"] == "auto"
    assert env["ZCCACHE_PROFILE_RUST_MISS"] == "1"
    assert "RUSTC_WRAPPER" not in env


def test_benchmark_env_does_not_enable_rust_profile_for_other_languages(tmp_path):
    env = perf_guard._benchmark_env(tmp_path, "c++")

    assert env["ZCCACHE_COMPILE_PRIORITY"] == "auto"
    assert "ZCCACHE_PROFILE_RUST_MISS" not in env
