"""Fail CI when zccache performance drops below compiler-cache speed floors."""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from ci import benchmark_stats


REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_OUTPUT_DIR = REPO_ROOT / "perf-guard-output"
DEFAULT_BARE_THRESHOLD = 1.5
DEFAULT_SCCACHE_THRESHOLD = 1.5
DEFAULT_THRESHOLD = DEFAULT_BARE_THRESHOLD
DEFAULT_ATTEMPTS = 3
REQUIRED_LANGUAGES = ("c", "c++", "rust")
REQUIRED_MODES = ("cold", "warm")


@dataclass(frozen=True)
class ScenarioKey:
    benchmark: str
    scenario: str
    baseline: str


@dataclass
class ScenarioStatus:
    key: ScenarioKey
    benchmark_label: str
    language: str
    mode: str
    scenario: str
    baseline: str
    baseline_label: str
    threshold: float
    best_ratio: float | None = None
    best_attempt: int | None = None
    attempts_seen: int = 0

    @property
    def passed(self) -> bool:
        return self.best_ratio is not None and self.best_ratio >= self.threshold


@dataclass
class GuardReport:
    statuses: list[ScenarioStatus]
    missing_requirements: list[str]
    command_failures: list[int]
    attempt_count: int

    @property
    def passed(self) -> bool:
        return (
            not self.missing_requirements
            and len(self.command_failures) < self.attempt_count
            and bool(self.statuses)
            and all(status.passed for status in self.statuses)
        )


def _benchmark_binary_command(benchmark_binary: Path, test_name: str | None = None) -> list[str]:
    command = [str(benchmark_binary)]
    if test_name is not None:
        command.append(test_name)
    command.extend(["--nocapture", "--ignored", "--test-threads=1"])
    return command


def _benchmark_commands(
    language: str | None,
    benchmark_binary: Path | None = None,
) -> list[list[str]]:
    if benchmark_binary is not None:
        if language is None:
            return [_benchmark_binary_command(benchmark_binary)]
        return [
            _benchmark_binary_command(benchmark_binary, test_name)
            for test_name in benchmark_stats.BENCHMARK_TESTS_BY_LANGUAGE[language]
        ]
    if language is None:
        return [benchmark_stats.BENCHMARK_COMMAND]
    return benchmark_stats.benchmark_commands_for_language(language)


def run_benchmarks_once(
    log_path: Path,
    language: str | None = None,
    benchmark_binary: Path | None = None,
) -> tuple[int, str]:
    cache_dir = Path(tempfile.mkdtemp(prefix="zccache-perf-guard-cache-"))
    env = os.environ.copy()
    env["ZCCACHE_CACHE_DIR"] = str(cache_dir)
    env.pop("RUSTC_WRAPPER", None)

    try:
        outputs: list[str] = []
        returncode = 0
        for command in _benchmark_commands(language, benchmark_binary):
            result = subprocess.run(
                command,
                cwd=REPO_ROOT,
                env=env,
                text=True,
                encoding="utf-8",
                errors="replace",
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                check=False,
            )
            outputs.append(result.stdout)
            if result.returncode != 0 and returncode == 0:
                returncode = result.returncode
    finally:
        shutil.rmtree(cache_dir, ignore_errors=True)

    output = "".join(outputs)
    log_path.parent.mkdir(parents=True, exist_ok=True)
    log_path.write_text(output, encoding="utf-8")
    print(output, end="")
    return returncode, output


def _required_rows(
    rows: list[dict[str, Any]],
    languages: tuple[str, ...],
) -> list[dict[str, Any]]:
    return [
        row
        for row in rows
        if row.get("language") in languages and row.get("mode") in REQUIRED_MODES
    ]


def evaluate_attempts(
    attempts: list[list[dict[str, Any]]],
    *,
    threshold: float | None = None,
    bare_threshold: float = DEFAULT_BARE_THRESHOLD,
    sccache_threshold: float = DEFAULT_SCCACHE_THRESHOLD,
    command_failures: list[int] | None = None,
    languages: tuple[str, ...] = REQUIRED_LANGUAGES,
) -> GuardReport:
    if threshold is not None:
        bare_threshold = threshold
        sccache_threshold = threshold
    statuses: dict[ScenarioKey, ScenarioStatus] = {}
    language_modes: set[tuple[str, str]] = set()

    for attempt_index, rows in enumerate(attempts, start=1):
        for row in _required_rows(rows, languages):
            language = str(row["language"])
            mode = str(row["mode"])
            language_modes.add((language, mode))
            comparisons = (
                ("bare", str(row["bare_label"]), row.get("zccache_vs_bare_ratio"), bare_threshold),
                (
                    "sccache",
                    "sccache",
                    row.get("zccache_vs_sccache_ratio"),
                    sccache_threshold,
                ),
            )
            for baseline, baseline_label, ratio, ratio_threshold in comparisons:
                key = ScenarioKey(str(row["benchmark"]), str(row["scenario"]), baseline)
                status = statuses.get(key)
                if status is None:
                    status = ScenarioStatus(
                        key=key,
                        benchmark_label=str(row["benchmark_label"]),
                        language=language,
                        mode=mode,
                        scenario=str(row["scenario"]),
                        baseline=baseline,
                        baseline_label=baseline_label,
                        threshold=ratio_threshold,
                    )
                    statuses[key] = status
                status.attempts_seen += 1

                if isinstance(ratio, int | float):
                    if status.best_ratio is None or ratio > status.best_ratio:
                        status.best_ratio = float(ratio)
                        status.best_attempt = attempt_index

    missing = [
        f"{language} {mode}"
        for language in languages
        for mode in REQUIRED_MODES
        if (language, mode) not in language_modes
    ]

    ordered = sorted(
        statuses.values(),
        key=lambda item: (
            item.language,
            item.benchmark_label,
            item.mode,
            item.scenario,
            item.baseline,
        ),
    )
    return GuardReport(
        statuses=ordered,
        missing_requirements=missing,
        command_failures=command_failures or [],
        attempt_count=len(attempts),
    )


def format_report(
    report: GuardReport,
    bare_threshold: float,
    sccache_threshold: float,
) -> str:
    lines = [
        "## zccache perf guard",
        "",
        f"Bare threshold: bare compiler / zccache >= {bare_threshold:.2f}x",
        f"sccache threshold: pinned sccache / zccache >= {sccache_threshold:.2f}x",
        "",
        "| Status | Language | Benchmark | Scenario | Baseline | Best ratio | Threshold | Attempt | Seen |",
        "|---|---|---|---|---|---:|---:|---:|---:|",
    ]
    for status in report.statuses:
        state = "PASS" if status.passed else "FAIL"
        ratio = "n/a" if status.best_ratio is None else f"{status.best_ratio:.3f}x"
        attempt = "n/a" if status.best_attempt is None else str(status.best_attempt)
        lines.append(
            "| "
            f"{state} | {status.language} | {status.benchmark_label} | "
            f"{status.scenario} | {status.baseline_label} | {ratio} | "
            f"{status.threshold:.2f}x | {attempt} | {status.attempts_seen} |"
        )

    if report.missing_requirements:
        lines.extend(["", "### Missing required coverage", ""])
        lines.extend(f"- {item}" for item in report.missing_requirements)

    if report.command_failures:
        lines.extend(["", "### Failed benchmark attempts", ""])
        lines.extend(f"- Attempt {attempt}" for attempt in report.command_failures)

    return "\n".join(lines) + "\n"


def format_report_json(
    report: GuardReport,
    bare_threshold: float,
    sccache_threshold: float,
    languages: tuple[str, ...] = REQUIRED_LANGUAGES,
) -> dict[str, Any]:
    return {
        "schema_version": 1,
        "passed": report.passed,
        "languages": list(languages),
        "thresholds": {
            "bare": bare_threshold,
            "sccache": sccache_threshold,
        },
        "attempt_count": report.attempt_count,
        "command_failures": report.command_failures,
        "missing_requirements": report.missing_requirements,
        "statuses": [
            {
                "passed": status.passed,
                "benchmark": status.key.benchmark,
                "benchmark_label": status.benchmark_label,
                "language": status.language,
                "mode": status.mode,
                "scenario": status.scenario,
                "baseline": status.baseline,
                "baseline_label": status.baseline_label,
                "threshold": status.threshold,
                "best_ratio": status.best_ratio,
                "best_attempt": status.best_attempt,
                "attempts_seen": status.attempts_seen,
            }
            for status in report.statuses
        ],
    }


def _write_json(path: Path, payload: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def write_attempt_json(
    output_dir: Path,
    attempt: int,
    rows: list[dict[str, Any]],
    returncode: int | None,
    *,
    source: str,
    language: str | None = None,
) -> None:
    _write_json(
        output_dir / f"attempt-{attempt}.json",
        {
            "schema_version": 1,
            "attempt": attempt,
            "source": source,
            "language": language,
            "returncode": returncode,
            "row_count": len(rows),
            "rows": rows,
        },
    )


def write_report_json(
    output_dir: Path,
    report: GuardReport,
    bare_threshold: float,
    sccache_threshold: float,
    languages: tuple[str, ...] = REQUIRED_LANGUAGES,
) -> None:
    _write_json(
        output_dir / "perf-guard-summary.json",
        format_report_json(report, bare_threshold, sccache_threshold, languages),
    )


def _append_step_summary(markdown: str) -> None:
    summary_path = os.environ.get("GITHUB_STEP_SUMMARY")
    if not summary_path:
        return
    with open(summary_path, "a", encoding="utf-8") as handle:
        handle.write(markdown)


def _load_input_log(path: Path) -> list[list[dict[str, Any]]]:
    text = path.read_text(encoding="utf-8")
    return [benchmark_stats.parse_benchmark_log(text)]


def _run_attempts(
    output_dir: Path,
    attempts: int,
    bare_threshold: float,
    sccache_threshold: float,
    languages: tuple[str, ...],
    benchmark_language: str | None,
    benchmark_binary: Path | None,
) -> tuple[list[list[dict[str, Any]]], list[int]]:
    parsed_attempts: list[list[dict[str, Any]]] = []
    command_failures: list[int] = []

    for attempt in range(1, attempts + 1):
        log_path = output_dir / f"attempt-{attempt}.log"
        print(f"=== Perf guard attempt {attempt}/{attempts} ===")
        returncode, output = run_benchmarks_once(log_path, benchmark_language, benchmark_binary)
        rows = benchmark_stats.parse_benchmark_log(output)
        parsed_attempts.append(rows)
        write_attempt_json(
            output_dir,
            attempt,
            rows,
            returncode,
            source="benchmark-binary" if benchmark_binary else "benchmark-run",
            language=benchmark_language,
        )
        if returncode != 0:
            command_failures.append(attempt)

        interim = evaluate_attempts(
            parsed_attempts,
            bare_threshold=bare_threshold,
            sccache_threshold=sccache_threshold,
            command_failures=command_failures,
            languages=languages,
        )
        if returncode == 0 and interim.passed:
            break

    return parsed_attempts, command_failures


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input-log", type=Path, help="Evaluate a saved benchmark log.")
    parser.add_argument("--run-benchmarks", action="store_true", help="Run perf benchmarks.")
    parser.add_argument("--output-dir", type=Path, default=DEFAULT_OUTPUT_DIR)
    parser.add_argument("--threshold", type=float, help="Set both thresholds to this value.")
    parser.add_argument("--bare-threshold", type=float, default=DEFAULT_BARE_THRESHOLD)
    parser.add_argument("--sccache-threshold", type=float, default=DEFAULT_SCCACHE_THRESHOLD)
    parser.add_argument("--attempts", type=int, default=DEFAULT_ATTEMPTS)
    parser.add_argument(
        "--benchmark-binary",
        type=Path,
        help="Run a prebuilt perf_bench_test binary instead of invoking cargo test.",
    )
    parser.add_argument(
        "--language",
        choices=REQUIRED_LANGUAGES,
        help="Run and require only one benchmark language.",
    )
    args = parser.parse_args()

    if args.threshold is not None:
        args.bare_threshold = args.threshold
        args.sccache_threshold = args.threshold
    if args.bare_threshold <= 0:
        parser.error("--bare-threshold must be greater than zero")
    if args.sccache_threshold <= 0:
        parser.error("--sccache-threshold must be greater than zero")
    if args.attempts < 1:
        parser.error("--attempts must be at least 1")
    if bool(args.input_log) == bool(args.run_benchmarks):
        parser.error("use exactly one of --input-log or --run-benchmarks")
    if args.benchmark_binary and not args.run_benchmarks:
        parser.error("--benchmark-binary requires --run-benchmarks")
    if args.benchmark_binary and not args.benchmark_binary.is_file():
        parser.error(f"--benchmark-binary does not exist: {args.benchmark_binary}")

    languages = (args.language,) if args.language else REQUIRED_LANGUAGES
    args.output_dir.mkdir(parents=True, exist_ok=True)
    if args.input_log:
        attempts = _load_input_log(args.input_log)
        write_attempt_json(
            args.output_dir,
            1,
            attempts[0],
            None,
            source="input-log",
            language=args.language,
        )
        command_failures: list[int] = []
    else:
        attempts, command_failures = _run_attempts(
            args.output_dir,
            args.attempts,
            args.bare_threshold,
            args.sccache_threshold,
            languages,
            args.language,
            args.benchmark_binary,
        )

    report = evaluate_attempts(
        attempts,
        bare_threshold=args.bare_threshold,
        sccache_threshold=args.sccache_threshold,
        command_failures=command_failures,
        languages=languages,
    )
    markdown = format_report(report, args.bare_threshold, args.sccache_threshold)
    report_path = args.output_dir / "perf-guard-summary.md"
    report_path.write_text(markdown, encoding="utf-8")
    write_report_json(
        args.output_dir,
        report,
        args.bare_threshold,
        args.sccache_threshold,
        languages,
    )
    print(markdown)
    _append_step_summary(markdown)

    return 0 if report.passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
