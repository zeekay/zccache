"""Run performance benchmarks (zccache vs sccache vs bare clang).

Two benchmarks:
  - perf_warm_cache_zccache_vs_sccache: inline args (single-file + multi-file)
  - perf_response_file: large nested response files (~283 expanded args)

Usage:
    ./perf               # run all benchmarks
    ./perf --nocapture   # (default) show output as it runs
"""

import subprocess
import sys
from pathlib import Path

from ci.env import activate, clean_env

SCRIPT_DIR = Path(__file__).parent.parent.resolve()


def main():
    activate()
    cmd = [
        "cargo", "test",
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
        env=clean_env(),
    )
    return result.returncode


if __name__ == "__main__":
    sys.exit(main())
