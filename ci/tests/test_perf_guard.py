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


def test_all_command_failures_fail_even_when_rows_parse():
    report = perf_guard.evaluate_attempts(
        [rows(PASSING_LOG), rows(PASSING_LOG)],
        threshold=1.5,
        command_failures=[1, 2],
    )

    assert not report.passed


def test_report_marks_failed_rows():
    report = perf_guard.evaluate_attempts([rows(FAILING_RUST_LOG)], threshold=1.5)
    markdown = perf_guard.format_report(report, 1.5, 1.5)

    assert "| FAIL | rust | Rust rustc | Build, Cold | Bare rustc | 1.286x | 1.50x | 1 | 1 |" in markdown
