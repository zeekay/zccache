"""`dylint` gate — nightly driver + `python -m ci.lint --dylint-only`.

Unix-only; default target only (musl/cross would just re-run identical
output). Toolchain install + driver build only happens on a cache miss.
"""

from __future__ import annotations

import os
import subprocess
import sys

from ._common import (
    REPO_ROOT,
    env_with,
    heading,
    is_platform,
    skip,
    soldr,
    soldr_cargo,
)

NIGHTLY = "nightly-2026-03-26"


def run() -> int:
    heading("dylint")
    if is_platform("windows"):
        return skip("dylint", "windows skipped (nightly driver assumes unix rustup)")
    if os.environ.get("CARGO_TARGET_FLAG", "").strip():
        return skip("dylint", "default-target only")

    soldr(
        "rustup",
        "toolchain",
        "install",
        NIGHTLY,
        "--component",
        "llvm-tools-preview",
        "--component",
        "rust-src",
        "--component",
        "rustc-dev",
        "--profile",
        "minimal",
    )

    # On a setup-soldr cache hit, the driver + cargo-dylint are already
    # installed; skip the install steps. Mirrors the GHA workflow logic.
    if os.environ.get("SETUP_SOLDR_DYLINT_CACHE_HIT") != "true":
        soldr_cargo("install", "cargo-dylint", "--version", "5.0.0", "--locked")
        soldr_cargo("install", "dylint-link", "--version", "5.0.0", "--locked")
        if (REPO_ROOT / "ci" / "build_dylint_driver.py").exists():
            rc = subprocess.run(
                [sys.executable, "ci/build_dylint_driver.py"],
                cwd=REPO_ROOT,
            ).returncode
            if rc != 0:
                return rc

    # Format-check each dylint library.
    for lib in ("ban_std_pathbuf", "ban_unrooted_tempdir"):
        manifest = REPO_ROOT / "dylints" / lib / "Cargo.toml"
        rc = soldr_cargo(
            "fmt",
            "--manifest-path",
            str(manifest),
            "--all",
            "--",
            "--check",
        )
        if rc != 0:
            return rc

    # Test each dylint library against the nightly driver.
    for lib in ("ban_std_pathbuf", "ban_unrooted_tempdir"):
        manifest = REPO_ROOT / "dylints" / lib / "Cargo.toml"
        env = env_with(("PATH", os.environ.get("CARGO_HOME", "") + "/bin:" + os.environ.get("PATH", "")))
        rc = soldr(
            "rustup",
            "run",
            NIGHTLY,
            "cargo",
            "test",
            "--manifest-path",
            str(manifest),
            env=env,
        )
        if rc != 0:
            return rc

    # Final: run the dylint pass via the workspace's existing wrapper.
    return subprocess.run(
        [sys.executable, "-m", "ci.lint", "--dylint-only"],
        cwd=REPO_ROOT,
    ).returncode
