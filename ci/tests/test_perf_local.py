"""Tests for ci/perf_local.py — the local Docker perf harness orchestrator.

Scope: pure functions only (formatting helpers + result-summary rendering).
Docker invocation, image build, and container run are NOT tested here —
they'd require a working Docker daemon and would duplicate what the GHA
perf cluster already covers end-to-end.
"""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path


def _load_perf_local():
    module_path = Path(__file__).resolve().parents[1] / "perf_local.py"
    spec = importlib.util.spec_from_file_location("perf_local", module_path)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


perf_local = _load_perf_local()


# ── fmt_ms ───────────────────────────────────────────────────────────────────


def test_fmt_ms_minutes_seconds():
    assert perf_local.fmt_ms(120_000) == "2m00s"


def test_fmt_ms_seconds_with_fraction():
    # >= 1000 ms shows seconds with two decimals
    assert perf_local.fmt_ms(1_500) == "1.50s"


def test_fmt_ms_sub_second_shows_ms():
    assert perf_local.fmt_ms(750) == "750ms"


def test_fmt_ms_zero_ms():
    assert perf_local.fmt_ms(0) == "0ms"


def test_fmt_ms_missing_value():
    assert perf_local.fmt_ms(None) == "—"
    assert perf_local.fmt_ms("") == "—"


def test_fmt_ms_just_under_minute():
    # 59999 ms is just under the minute boundary — should still be seconds
    assert perf_local.fmt_ms(59_999) == "59.999s"[:5]


def test_fmt_ms_exactly_one_minute():
    assert perf_local.fmt_ms(60_000) == "1m00s"


# ── fmt_bytes ────────────────────────────────────────────────────────────────


def test_fmt_bytes_gib():
    assert perf_local.fmt_bytes(2 * (1 << 30)) == "2.00 GiB"


def test_fmt_bytes_mib():
    assert perf_local.fmt_bytes(5 * (1 << 20)) == "5.0 MiB"


def test_fmt_bytes_kib():
    assert perf_local.fmt_bytes(3 * (1 << 10)) == "3.0 KiB"


def test_fmt_bytes_under_kib():
    assert perf_local.fmt_bytes(512) == "512 B"


def test_fmt_bytes_zero():
    assert perf_local.fmt_bytes(0) == "0 B"


def test_fmt_bytes_missing_value():
    assert perf_local.fmt_bytes(None) == "—"
    assert perf_local.fmt_bytes("") == "—"


# ── fmt_count_pct ────────────────────────────────────────────────────────────


def test_fmt_count_pct_with_total():
    # 95 / 118 ≈ 80.5%
    assert perf_local.fmt_count_pct(95, 118) == "95 (80.5%)"


def test_fmt_count_pct_zero_count():
    assert perf_local.fmt_count_pct(0, 100) == "0 (0.0%)"


def test_fmt_count_pct_full_hit():
    assert perf_local.fmt_count_pct(100, 100) == "100 (100.0%)"


def test_fmt_count_pct_missing_count():
    assert perf_local.fmt_count_pct(None, 100) == "—"
    assert perf_local.fmt_count_pct("", 100) == "—"


def test_fmt_count_pct_missing_total_returns_bare_count():
    # If the total is unavailable we can't compute a %, fall back to the count
    assert perf_local.fmt_count_pct(95, None) == "95"
    assert perf_local.fmt_count_pct(95, "") == "95"
    assert perf_local.fmt_count_pct(95, 0) == "95"


# ── render_summary integration ───────────────────────────────────────────────
#
# render_summary reads result.json + warm-cache-report.json from a directory
# and prints the rich table. Tests below build a fake directory structure
# and assert the function's exit code (0 = PASS, 1 = FAIL) matches the
# expected verdict for the given speedup.


def _write_scenario_results(
    tmp_path: Path,
    scenario: str,
    *,
    cold_ms: int,
    warm_ms: int,
    cache_report: dict | None,
) -> Path:
    """Build a fake `results_dir` shape matching what the runner container
    writes. Returns the directory."""
    results_dir = tmp_path / scenario
    results_dir.mkdir()

    # Per-scenario key naming, matching perf-rust-cluster.yml's evaluate step.
    if scenario == "worktree-share":
        cold_key, warm_key = "a_ms", "b_ms"
    else:
        cold_key, warm_key = "cold_ms", "warm_ms"

    result = {
        "scenario": scenario,
        cold_key: cold_ms,
        warm_key: warm_ms,
        "peak_daemon_rss_bytes": 8_000_000,
        "peak_compile_rss_bytes": 500_000_000,
    }
    (results_dir / "result.json").write_text(json.dumps(result))

    if cache_report is not None:
        report_name = "b-cache-report.json" if scenario == "worktree-share" else "warm-cache-report.json"
        (results_dir / report_name).write_text(
            json.dumps({"last_session": cache_report})
        )

    return results_dir


def test_render_summary_pass_at_threshold(tmp_path, capsys):
    """speedup >= 3.0x must return 0 (PASS)."""
    results_dir = _write_scenario_results(
        tmp_path,
        "cold-tar-untar-warm",
        cold_ms=60_000,
        warm_ms=20_000,  # 3.0x exactly
        cache_report={
            "compilations": 100,
            "hits": 95,
            "misses": 3,
            "non_cacheable": 2,
            "errors": 0,
            "hit_rate": 0.95,
            "bytes_written": 100 * (1 << 20),
            "bytes_read": 50 * (1 << 20),
            "time_saved_ms": 1_500,
            "unique_sources": 95,
        },
    )
    rc = perf_local.render_summary(results_dir, "cold-tar-untar-warm", "medium")
    assert rc == 0
    out = capsys.readouterr().out
    assert "**PASS**" in out
    assert "3.00x" in out
    assert "95 (95.0%)" in out  # hits cell shows count + ratio inline


def test_render_summary_fail_below_threshold(tmp_path, capsys):
    """speedup < 3.0x must return 1 (FAIL)."""
    results_dir = _write_scenario_results(
        tmp_path,
        "cold-tar-untar-warm",
        cold_ms=60_000,
        warm_ms=52_000,  # 1.15x — same numbers as the cluster pre-fix
        cache_report={
            "compilations": 146,
            "hits": 0,
            "misses": 115,
            "non_cacheable": 31,
            "errors": 3,
            "hit_rate": 0.0,
            "bytes_written": 210 * (1 << 20),
            "bytes_read": 0,
            "time_saved_ms": 0,
            "unique_sources": 115,
        },
    )
    rc = perf_local.render_summary(results_dir, "cold-tar-untar-warm", "medium")
    assert rc == 1
    out = capsys.readouterr().out
    assert "**FAIL**" in out
    assert "1.15x" in out
    # Hits cell shows 0 with the percentage; misses with theirs
    assert "0 (0.0%)" in out
    assert "115 (78.8%)" in out


def test_render_summary_handles_missing_cache_report(tmp_path, capsys):
    """When warm-cache-report.json is absent, the cache-counter columns
    fall back to em-dashes rather than crashing. The PASS/FAIL verdict
    still computes from the timing keys in result.json."""
    results_dir = _write_scenario_results(
        tmp_path,
        "cold-tar-untar-warm",
        cold_ms=60_000,
        warm_ms=10_000,  # 6.0x — well above threshold
        cache_report=None,
    )
    rc = perf_local.render_summary(results_dir, "cold-tar-untar-warm", "medium")
    assert rc == 0
    out = capsys.readouterr().out
    assert "**PASS**" in out
    # Hits/Misses/Ignored cells are em-dashes when the report is missing
    assert "| — |" in out


def test_render_summary_handles_worktree_share_a_b_keys(tmp_path, capsys):
    """The worktree-share scenario uses `a_ms`/`b_ms` instead of
    cold_ms/warm_ms, and `b-cache-report.json` for the warm-side report.
    Verify the orchestrator looks up the right keys."""
    results_dir = _write_scenario_results(
        tmp_path,
        "worktree-share",
        cold_ms=12_000,
        warm_ms=3_000,  # b/a = 4x
        cache_report={
            "compilations": 50,
            "hits": 40,
            "misses": 8,
            "non_cacheable": 2,
            "errors": 0,
            "hit_rate": 0.83,
            "bytes_written": 0,
            "bytes_read": 0,
            "time_saved_ms": 500,
            "unique_sources": 40,
        },
    )
    rc = perf_local.render_summary(results_dir, "worktree-share", "medium")
    assert rc == 0
    out = capsys.readouterr().out
    assert "**PASS**" in out
    assert "4.00x" in out


def test_render_summary_missing_result_json_fails(tmp_path, capsys):
    """If result.json itself is missing, the run failed before emit. Return 1
    with a clear message."""
    results_dir = tmp_path / "broken"
    results_dir.mkdir()
    rc = perf_local.render_summary(results_dir, "cold-tar-untar-warm", "medium")
    assert rc == 1
    out = capsys.readouterr().out
    assert "result.json missing" in out


def test_render_summary_bad_timing_fails(tmp_path, capsys):
    """0-ms or negative warm_ms is a measurement bug, not a win. Return 1."""
    results_dir = _write_scenario_results(
        tmp_path,
        "cold-tar-untar-warm",
        cold_ms=60_000,
        warm_ms=0,
        cache_report=None,
    )
    rc = perf_local.render_summary(results_dir, "cold-tar-untar-warm", "medium")
    assert rc == 1
    out = capsys.readouterr().out
    assert "bad timing" in out


# ── docker_available / image_exists smoke checks ─────────────────────────────
#
# These hit subprocess. We only assert they return a bool and don't raise
# when docker isn't installed — full behavior requires a docker daemon and
# would belong in a separate integration suite.


def test_docker_available_returns_bool():
    result = perf_local.docker_available()
    assert isinstance(result, bool)


def test_image_exists_returns_bool_when_docker_missing():
    """When docker isn't on PATH, image_exists should still return a bool
    (not raise) — the orchestrator's docker_available check fires first
    in main() so this is a defensive check."""
    if not perf_local.docker_available():
        # We're testing the no-docker path. image_exists may shell out and
        # get a non-zero exit; either way it should return False, not raise.
        result = perf_local.image_exists("definitely-not-a-real-image-tag-12345")
        assert result is False
