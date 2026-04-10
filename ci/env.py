"""Rust toolchain environment activation."""

from __future__ import annotations

import os
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent


def _repo_tool_home(dirname: str) -> Path | None:
    candidate = REPO_ROOT / dirname
    if candidate.is_dir():
        return candidate
    return None


def cargo_home() -> Path:
    if os.environ.get("CARGO_HOME"):
        return Path(os.environ["CARGO_HOME"]).expanduser()
    repo_cargo = _repo_tool_home(".cargo")
    if repo_cargo is not None:
        return repo_cargo
    return Path.home() / ".cargo"


def rustup_home() -> Path:
    if os.environ.get("RUSTUP_HOME"):
        return Path(os.environ["RUSTUP_HOME"]).expanduser()
    repo_rustup = _repo_tool_home(".rustup")
    if repo_rustup is not None:
        return repo_rustup
    return Path.home() / ".rustup"


def cargo_bin() -> Path:
    return cargo_home() / "bin"


def find_cargo_bin() -> str | None:
    bin_dir = cargo_bin()
    if bin_dir.is_dir():
        return str(bin_dir)
    return None


def activate() -> None:
    os.environ.setdefault("CARGO_HOME", str(cargo_home()))
    os.environ.setdefault("RUSTUP_HOME", str(rustup_home()))

    bin_dir = cargo_bin()
    if not bin_dir.is_dir():
        return

    current_path = os.environ.get("PATH", "")
    path_parts = current_path.split(os.pathsep) if current_path else []
    normalized_bin = os.path.normcase(os.path.normpath(str(bin_dir)))
    filtered_parts = [
        part
        for part in path_parts
        if part and os.path.normcase(os.path.normpath(part)) != normalized_bin
    ]
    os.environ["PATH"] = os.pathsep.join([str(bin_dir), *filtered_parts])


def clean_env() -> dict[str, str]:
    activate()
    env = os.environ.copy()
    env.setdefault("CARGO_HOME", str(cargo_home()))
    env.setdefault("RUSTUP_HOME", str(rustup_home()))
    env.pop("VIRTUAL_ENV", None)
    return env
