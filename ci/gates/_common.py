"""Shared helpers for the per-gate runners."""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Optional

REPO_ROOT = Path(__file__).resolve().parents[2]


def cargo_target_flag() -> list[str]:
    """`--target <triple>` from the CARGO_BUILD_TARGET / CARGO_TARGET_FLAG env.

    CI sets `CARGO_TARGET_FLAG=--target <triple>` for the musl matrix
    entries; on default-target rows it stays empty. Mirroring the env
    rather than re-deriving from `runner.arch` keeps the gate runner
    OS-agnostic.
    """
    flag = os.environ.get("CARGO_TARGET_FLAG", "").strip()
    if not flag:
        return []
    # The env value is the literal `--target <triple>`; split for argv.
    return flag.split()


def soldr_cargo(*args: str, env: Optional[dict] = None) -> int:
    """Run `soldr cargo <args>` with stdin tied off; returns exit code."""
    cmd = ["soldr", "cargo", *args]
    sys.stdout.write(f"\n$ {' '.join(cmd)}\n")
    sys.stdout.flush()
    return subprocess.run(cmd, env=env or os.environ.copy(), cwd=REPO_ROOT).returncode


def soldr(*args: str, env: Optional[dict] = None) -> int:
    """Run `soldr <args>`; returns exit code."""
    cmd = ["soldr", *args]
    sys.stdout.write(f"\n$ {' '.join(cmd)}\n")
    sys.stdout.flush()
    return subprocess.run(cmd, env=env or os.environ.copy(), cwd=REPO_ROOT).returncode


def find_built_binary(name: str) -> Optional[Path]:
    """Locate a Cargo-built binary under target/, ignoring deps + build-script outputs.

    Returns the first match for `<name>` or `<name>.exe`.
    """
    candidates = [name, f"{name}.exe"]
    for cand in candidates:
        for p in (REPO_ROOT / "target").rglob(cand):
            sp = str(p)
            if "/deps/" in sp or "\\deps\\" in sp:
                continue
            if "/build/" in sp or "\\build\\" in sp:
                continue
            if p.is_file() and (cand.endswith(".exe") or os.access(p, os.X_OK)):
                return p
    return None


def env_with(*pairs: tuple[str, Optional[str]]) -> dict:
    """Build a copy of the current env with overrides; `None` value removes."""
    e = os.environ.copy()
    for k, v in pairs:
        if v is None:
            e.pop(k, None)
        else:
            e[k] = v
    return e


def is_platform(family: str) -> bool:
    """`linux`, `windows`, or `darwin` — what gate-runners gate themselves on.

    Use `sys.platform` rather than the env so `ci.py all` from a developer
    workstation does the right thing without needing GHA's matrix vars.
    """
    if family == "linux":
        return sys.platform.startswith("linux")
    if family == "windows":
        return sys.platform.startswith("win")
    if family == "darwin":
        return sys.platform == "darwin"
    raise ValueError(f"unknown platform family: {family}")


def have(tool: str) -> bool:
    """`shutil.which`, but typed/named."""
    return shutil.which(tool) is not None


def skip(gate: str, reason: str) -> int:
    """Mark a gate skipped — print a notice + exit 0 (treat as pass)."""
    sys.stdout.write(f"\n[skip] {gate}: {reason}\n")
    return 0


def heading(gate: str) -> None:
    """Match the existing GHA `$GITHUB_STEP_SUMMARY` shape on stdout."""
    sys.stdout.write(f"\n=== {gate} ===\n")
    sys.stdout.flush()
