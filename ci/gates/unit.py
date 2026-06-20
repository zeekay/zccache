"""`unit` gate — `soldr cargo test --workspace --lib --bins --no-fail-fast`.

Skips when a non-default target is set (musl: build-only). Uses an
isolated SOLDR_CACHE_DIR per #240 so the daemon-isolated test phase
does not fight the build cache.
"""

from __future__ import annotations

import os
import tempfile
from pathlib import Path

from ._common import (
    cargo_target_flag,
    env_with,
    heading,
    skip,
    soldr,
    soldr_cargo,
)


def run() -> int:
    heading("unit")
    target = os.environ.get("CARGO_TARGET_FLAG", "").strip()
    if target:
        return skip("unit", f"build-only on {target} (musl test harness is build-only)")

    runner_tmp = os.environ.get("RUNNER_TEMP", tempfile.gettempdir())
    cache_dir = Path(runner_tmp) / "zccache-self-tests" / "unified-unit"
    cache_dir.mkdir(parents=True, exist_ok=True)

    env = env_with(
        ("SOLDR_CACHE_DIR", str(cache_dir)),
        ("SOLDR_CACHE_LIFECYCLE", "command"),
        ("SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS", "30"),
        ("ZCCACHE_CACHE_DIR", None),
    )
    rc = soldr_cargo(
        "test",
        "--workspace",
        "--lib",
        "--bins",
        "--no-fail-fast",
        *cargo_target_flag(),
        env=env,
    )
    # Best-effort daemon shutdown so the isolated cache flushes; failures
    # here don't affect the gate outcome.
    soldr(
        "cache",
        "shutdown",
        "--archive-logs",
        str(cache_dir / "cache" / "zccache" / "logs" / "archive"),
        env=env,
    )
    return rc
