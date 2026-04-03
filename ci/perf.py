#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# ///
"""Run performance benchmarks (zccache vs sccache vs bare clang).

Two benchmarks:
  - perf_warm_cache_zccache_vs_sccache: inline args (single-file + multi-file)
  - perf_response_file: large nested response files (~283 expanded args)

Usage:
    ./perf               # run all benchmarks
    ./perf --nocapture   # (default) show output as it runs
"""

import os
import subprocess
import sys
from pathlib import Path

SCRIPT_DIR = Path(__file__).parent.parent.resolve()


def _clean_env():
    """Return env with VIRTUAL_ENV removed to avoid uv mismatch warnings."""
    env = os.environ.copy()
    env.pop("VIRTUAL_ENV", None)
    return env


def main():
    cmd = [
        "uv", "run", "cargo", "test",
        "-p", "zccache-daemon",
        "--test", "perf_bench_test",
        "--",
        "--nocapture",
        "--ignored",
        "--test-threads=1",
    ]

    result = subprocess.run(
        cmd,
        text=True,
        encoding="utf-8",
        errors="replace",
        cwd=str(SCRIPT_DIR),
        env=_clean_env(),
    )
    return result.returncode


if __name__ == "__main__":
    sys.exit(main())
