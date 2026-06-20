"""`integration` gate — `soldr cargo test --workspace --no-fail-fast`.

Linux x86 only: integration tests are heavy + cross-platform paths are
already covered by per-platform unit runs. Running integration on every
matrix entry would more than double total CI minutes for redundant
coverage.
"""

from __future__ import annotations

import os
import tempfile
from pathlib import Path

from ._common import (
    env_with,
    heading,
    is_platform,
    skip,
    soldr,
    soldr_cargo,
)


def run() -> int:
    heading("integration")
    if not is_platform("linux"):
        return skip("integration", "linux-only by convention")
    if os.environ.get("CARGO_TARGET_FLAG", "").strip():
        return skip("integration", "build-only on non-default targets")

    runner_tmp = os.environ.get("RUNNER_TEMP", tempfile.gettempdir())
    cache_dir = Path(runner_tmp) / "zccache-self-tests" / "integration-unified"
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
        "--no-fail-fast",
        env=env,
    )
    soldr(
        "cache",
        "shutdown",
        "--archive-logs",
        str(cache_dir / "cache" / "zccache" / "logs" / "archive"),
        env=env,
    )
    return rc
