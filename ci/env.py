"""Rust toolchain environment activation."""

from __future__ import annotations

import os
from pathlib import Path

def cargo_home() -> Path:
    if os.environ.get("CARGO_HOME"):
        return Path(os.environ["CARGO_HOME"]).expanduser()
    return Path.home() / ".cargo"


def rustup_home() -> Path:
    if os.environ.get("RUSTUP_HOME"):
        return Path(os.environ["RUSTUP_HOME"]).expanduser()
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
    normalized_parts = {
        os.path.normcase(os.path.normpath(part))
        for part in path_parts
        if part
    }
    if normalized_bin in normalized_parts:
        return
    os.environ["PATH"] = str(bin_dir) + (os.pathsep + current_path if current_path else "")


def clean_env() -> dict[str, str]:
    activate()
    env = os.environ.copy()
    env.setdefault("CARGO_HOME", str(cargo_home()))
    env.setdefault("RUSTUP_HOME", str(rustup_home()))
    env.pop("VIRTUAL_ENV", None)
    return env
