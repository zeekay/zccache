"""Fail CI when zccache performance drops below the bare-compiler floor."""

from __future__ import annotations

import argparse
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
DEFAULT_THRESHOLD = 1.5
DEFAULT_ATTEMPTS = 3
REQUIRED_LANGUAGES = ("c", "c++", "rust")
REQUIRED_MODES = ("cold", "warm")


@dataclass(frozen=True)
class ScenarioKey:
    benchmark: str
    scenario: str


@dataclass
class ScenarioStatus:
    key: ScenarioKey
    benchmark_label: str
    language: str
    mode: str
    scenario: str
    bare_label: str
    best_ratio: float | None = None
    best_attempt: int | None = None
    attempts_seen: int = 0

    @property
    def passed(self) -> bool:
        return self.best_ratio is not None and self.best_ratio >= self.threshold

    threshold: float = DEFAULT_THRESHOLD


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
            and self.statuses
            and all(status.passed for status in self.statuses)
        )


def run_benchmarks_once(log_path: Path) -> tuple[int, str]:
    cache_dir = Path(tempfile.mkdtemp(prefix="zccache-perf-guard-cache-"))
    env = os.environ.copy()
    env["ZCCACHE_CACHE_DIR"] = str(cache_dir)
    env.pop("RUSTC_WRAPPER", None)

    try:
        result = subprocess.run(
            benchmark_stats.BENCHMARK_COMMAND,
            cwd=REPO_ROOT,
            env=env,
            text=True,
            encoding="utf-8",
            errors="replace",
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
        )
    finally:
        shutil.rmtree(cache_dir, ignore_errors=True)

    output = result.stdout
    log_path.parent.mkdir(parents=True, exist_ok=True)
    log_path.write_text(output, encoding="utf-8")
    print(output, end="")
    return result.returncode, output


def _required_rows(rows: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return [
        row
        for row in rows
        if row.get("language") in REQUIRED_LANGUAGES and row.get("mode") in REQUIRED_MODES
    ]


def evaluate_attempts(
    attempts: list[list[dict[str, Any]]],
    *,
    threshold: float = DEFAULT_THRESHOLD,
    command_failures: list[int] | None = None,
) -> GuardReport:
    statuses: dict[ScenarioKey, ScenarioStatus] = {}
    language_modes: set[tuple[str, str]] = set()

    for attempt_index, rows in enumerate(attempts, start=1):
        for row in _required_rows(rows):
            language = str(row["language"])
            mode = str(row["mode"])
            language_modes.add((language, mode))
            key = ScenarioKey(str(row["benchmark"]), str(row["scenario"]))
            status = statuses.get(key)
            if status is None:
                status = ScenarioStatus(
                    key=key,
                    benchmark_label=str(row["benchmark_label"]),
                    language=language,
                    mode=mode,
                    scenario=str(row["scenario"]),
                    bare_label=str(row["bare_label"]),
                    threshold=threshold,
                )
                statuses[key] = status
            status.attempts_seen += 1

            ratio = row.get("zccache_vs_bare_ratio")
            if isinstance(ratio, int | float):
                if status.best_ratio is None or ratio > status.best_ratio:
                    status.best_ratio = float(ratio)
                    status.best_attempt = attempt_index

    missing = [
        f"{language} {mode}"
        for language in REQUIRED_LANGUAGES
        for mode in REQUIRED_MODES
        if (language, mode) not in language_modes
    ]

    ordered = sorted(
        statuses.values(),
        key=lambda item: (item.language, item.benchmark_label, item.mode, item.scenario),
    )
    return GuardReport(
        statuses=ordered,
        missing_requirements=missing,
        command_failures=command_failures or [],
        attempt_count=len(attempts),
    )


def format_report(report: GuardReport, threshold: float) -> str:
    lines = [
        "## zccache perf guard",
        "",
        f"Threshold: bare compiler / zccache >= {threshold:.2f}x",
        "",
        "| Status | Language | Benchmark | Scenario | Best ratio | Attempt | Seen |",
        "|---|---|---|---|---:|---:|---:|",
    ]
    for status in report.statuses:
        state = "PASS" if status.passed else "FAIL"
        ratio = "n/a" if status.best_ratio is None else f"{status.best_ratio:.3f}x"
        attempt = "n/a" if status.best_attempt is None else str(status.best_attempt)
        lines.append(
            "| "
            f"{state} | {status.language} | {status.benchmark_label} | "
            f"{status.scenario} | {ratio} | {attempt} | {status.attempts_seen} |"
        )

    if report.missing_requirements:
        lines.extend(["", "### Missing required coverage", ""])
        lines.extend(f"- {item}" for item in report.missing_requirements)

    if report.command_failures:
        lines.extend(["", "### Failed benchmark attempts", ""])
        lines.extend(f"- Attempt {attempt}" for attempt in report.command_failures)

    return "\n".join(lines) + "\n"


def _append_step_summary(markdown: str) -> None:
    summary_path = os.environ.get("GITHUB_STEP_SUMMARY")
    if not summary_path:
        return
    with open(summary_path, "a", encoding="utf-8") as handle:
        handle.write(markdown)


def _load_input_log(path: Path) -> list[list[dict[str, Any]]]:
    text = path.read_text(encoding="utf-8")
    return [benchmark_stats.parse_benchmark_log(text)]


def _run_attempts(output_dir: Path, attempts: int) -> tuple[list[list[dict[str, Any]]], list[int]]:
    parsed_attempts: list[list[dict[str, Any]]] = []
    command_failures: list[int] = []

    for attempt in range(1, attempts + 1):
        log_path = output_dir / f"attempt-{attempt}.log"
        print(f"=== Perf guard attempt {attempt}/{attempts} ===")
        returncode, output = run_benchmarks_once(log_path)
        rows = benchmark_stats.parse_benchmark_log(output)
        parsed_attempts.append(rows)
        if returncode != 0:
            command_failures.append(attempt)

        interim = evaluate_attempts(parsed_attempts, command_failures=command_failures)
        if returncode == 0 and interim.passed:
            break

    return parsed_attempts, command_failures


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input-log", type=Path, help="Evaluate a saved benchmark log.")
    parser.add_argument("--run-benchmarks", action="store_true", help="Run perf benchmarks.")
    parser.add_argument("--output-dir", type=Path, default=DEFAULT_OUTPUT_DIR)
    parser.add_argument("--threshold", type=float, default=DEFAULT_THRESHOLD)
    parser.add_argument("--attempts", type=int, default=DEFAULT_ATTEMPTS)
    args = parser.parse_args()

    if args.threshold <= 0:
        parser.error("--threshold must be greater than zero")
    if args.attempts < 1:
        parser.error("--attempts must be at least 1")
    if bool(args.input_log) == bool(args.run_benchmarks):
        parser.error("use exactly one of --input-log or --run-benchmarks")

    args.output_dir.mkdir(parents=True, exist_ok=True)
    if args.input_log:
        attempts = _load_input_log(args.input_log)
        command_failures: list[int] = []
    else:
        attempts, command_failures = _run_attempts(args.output_dir, args.attempts)

    report = evaluate_attempts(
        attempts,
        threshold=args.threshold,
        command_failures=command_failures,
    )
    markdown = format_report(report, args.threshold)
    report_path = args.output_dir / "perf-guard-summary.md"
    report_path.write_text(markdown, encoding="utf-8")
    print(markdown)
    _append_step_summary(markdown)

    return 0 if report.passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
