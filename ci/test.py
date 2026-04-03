#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# ///
"""Run workspace tests.

Usage:
    ./test                          # unit tests only (fast, no compiler needed)
    ./test --integration            # integration tests only (need clang)
    ./test --full                   # unit + integration + stress tests
    ./test -p zccache-hash          # single crate
    ./test -p zccache-hash -- name  # single test by name
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
    cmd = ["uv", "run", "cargo", "test"]

    args = sys.argv[1:]
    full = "--full" in args
    integration = "--integration" in args
    if full:
        args.remove("--full")
    if integration:
        args.remove("--integration")

    # Split on "--" to separate cargo args from test-binary args.
    if "--" in args:
        sep = args.index("--")
        cargo_args = args[:sep]
        test_args = args[sep + 1:]
    else:
        cargo_args = args
        test_args = []

    if not cargo_args:
        cmd += ["--workspace"]

    cmd += cargo_args
    cmd += ["--"]
    if full:
        cmd += ["--include-ignored"]
    elif integration:
        cmd += ["--ignored"]
    cmd += test_args

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
