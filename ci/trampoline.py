"""Rust toolchain trampolines.

Helper functions that activate the repo-local rustup toolchain before
executing Rust tools. The primary PATH setup is also shared via `ci.env`.
These trampolines are used by the remaining project scripts (`run_zccache`,
`run_zccache_daemon`) which wrap `cargo run` invocations.
"""

import os
import subprocess
import sys
from pathlib import Path

from ci.env import activate, ensure_windows_msvc, require_tool_path


def _run_tool(tool_name):
    """Prepend .cargo/bin to PATH and exec the given tool."""
    activate()
    try:
        tool = require_tool_path(tool_name)
        ensure_windows_msvc()
    except (FileNotFoundError, RuntimeError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        sys.exit(1)

    result = subprocess.run([str(tool)] + sys.argv[1:], env=os.environ.copy())
    sys.exit(result.returncode)


def cargo():
    _run_tool("cargo")


def rustc():
    _run_tool("rustc")


def rustfmt():
    _run_tool("rustfmt")


def clippy_driver():
    _run_tool("clippy-driver")


def _run_cargo_bin(package):
    """Run a cargo binary with the correct toolchain on PATH."""
    activate()
    try:
        cargo = require_tool_path("cargo")
        ensure_windows_msvc()
    except (FileNotFoundError, RuntimeError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        sys.exit(1)

    extra = sys.argv[1:]
    # Strip leading '--' that uv inserts
    if extra and extra[0] == "--":
        extra = extra[1:]
    cmd = [str(cargo), "run", "-p", package]
    if extra:
        cmd.append("--")
        cmd.extend(extra)
    result = subprocess.run(cmd, env=os.environ.copy())
    sys.exit(result.returncode)


def run_zccache():
    _run_cargo_bin("zccache-cli")


def run_zccache_daemon():
    _run_cargo_bin("zccache-daemon")


def run_zccache_fingerprint():
    _run_cargo_bin("zccache-fp")


def check_on_stop():
    _run_cargo_bin("zccache-ci")


def test():
    """Run workspace tests via ci/test.py."""
    script = Path(__file__).parent / "test.py"
    result = subprocess.run([sys.executable, str(script)] + sys.argv[1:])
    sys.exit(result.returncode)


def perf():
    """Run performance benchmarks via ci/perf.py."""
    script = Path(__file__).parent / "perf.py"
    result = subprocess.run([sys.executable, str(script)] + sys.argv[1:])
    sys.exit(result.returncode)
