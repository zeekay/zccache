"""`cargo-registry` gate — hash determinism + save (`--output`) round-trip.

Skipped on non-default targets. Uses the `--output PATH` flag added in
PR #833 to bypass `SOLDR_SKIP_CARGO_REGISTRY_SAVE=1` (which setup-soldr
sets in its warm-path config); the smoke test is the explicit caller
so the save runs against an isolated tempfile and we assert the file
landed there.
"""

from __future__ import annotations

import os
import subprocess
import tempfile
from pathlib import Path

from ._common import (
    REPO_ROOT,
    find_built_binary,
    heading,
    skip,
    soldr_cargo,
)


def run() -> int:
    heading("cargo-registry")
    if os.environ.get("CARGO_TARGET_FLAG", "").strip():
        return skip("cargo-registry", "default-target only")

    # Ensure the binary exists; cargo build is cheap if the build gate
    # already populated target/ (which it will have in the normal
    # `ci.py all` flow).
    soldr_cargo("build", "-p", "zccache", "--bin", "zccache")
    zcc = find_built_binary("zccache")
    if zcc is None:
        print("FAIL: zccache binary not found under target/")
        return 1

    def zcc_run(*args: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            [str(zcc), *args], capture_output=True, text=True, cwd=REPO_ROOT
        )

    # Hash must be deterministic + 16 hex chars (action.yml + soldr
    # depend on this).
    h1 = zcc_run("cargo-registry", "hash", "--lockfile", "Cargo.lock")
    h2 = zcc_run("cargo-registry", "hash", "--lockfile", "Cargo.lock")
    if h1.returncode != 0 or h2.returncode != 0:
        print(f"FAIL: hash subcommand exited non-zero: {h1.stderr}{h2.stderr}")
        return 1
    h1s, h2s = h1.stdout.strip(), h2.stdout.strip()
    if h1s != h2s:
        print(f"FAIL: hash not deterministic ({h1s} vs {h2s})")
        return 1
    if len(h1s) != 16:
        print(f"FAIL: hash length {len(h1s)} != 16: {h1s!r}")
        return 1

    # Seed the cargo registry so save has something to archive.
    soldr_cargo("fetch")

    runner_tmp = os.environ.get("RUNNER_TEMP", tempfile.gettempdir())
    archive = Path(runner_tmp) / f"cargo-registry-smoke-{os.getpid()}.tar.gz"
    try:
        rc = subprocess.run(
            [
                str(zcc),
                "cargo-registry",
                "save",
                "--key",
                "ignored-when-output-set",
                "--output",
                str(archive),
            ],
            cwd=REPO_ROOT,
        ).returncode
        if rc != 0 or not archive.is_file():
            print(f"FAIL: save did not produce archive at {archive}")
            return 1
        size = archive.stat().st_size
        if size < 1024:
            print(f"FAIL: archive suspiciously small ({size} bytes) at {archive}")
            return 1
    finally:
        if archive.exists():
            archive.unlink()

    return 0
