"""Rust toolchain environment activation."""

from __future__ import annotations

import os
import shutil
import subprocess
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
WINDOWS_MSVC_SUFFIX = "-pc-windows-msvc"


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


def _tool_filename(tool_name: str) -> str:
    if os.name == "nt" and not tool_name.endswith(".exe"):
        return f"{tool_name}.exe"
    return tool_name


def find_tool_path(tool_name: str) -> Path | None:
    bin_dir = cargo_bin()
    if bin_dir.is_dir():
        candidate = bin_dir / _tool_filename(tool_name)
        if candidate.exists():
            return candidate

    resolved = shutil.which(tool_name)
    if resolved:
        return Path(resolved)
    return None


def require_tool_path(tool_name: str) -> Path:
    candidate = find_tool_path(tool_name)
    if candidate is None:
        raise FileNotFoundError(f"Cannot find {tool_name}. Ensure rustup is installed.")
    return candidate


def _activate_env(env: dict[str, str]) -> None:
    env.setdefault("CARGO_HOME", str(cargo_home()))
    env.setdefault("RUSTUP_HOME", str(rustup_home()))

    bin_dir = cargo_bin()
    if not bin_dir.is_dir():
        return

    current_path = env.get("PATH", "")
    path_parts = current_path.split(os.pathsep) if current_path else []
    normalized_bin = os.path.normcase(os.path.normpath(str(bin_dir)))
    filtered_parts = [
        part
        for part in path_parts
        if part and os.path.normcase(os.path.normpath(part)) != normalized_bin
    ]
    env["PATH"] = os.pathsep.join([str(bin_dir), *filtered_parts])


def activate() -> None:
    _activate_env(os.environ)


def clean_env() -> dict[str, str]:
    env = os.environ.copy()
    _activate_env(env)
    env.pop("VIRTUAL_ENV", None)
    return env


def rustc_host() -> str | None:
    rustc = require_tool_path("rustc")
    result = subprocess.run(
        [str(rustc), "-vV"],
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        env=clean_env(),
    )
    if result.returncode != 0:
        return None

    for line in result.stdout.splitlines():
        if line.startswith("host:"):
            return line.split(":", 1)[1].strip()
    return None


def ensure_windows_msvc() -> None:
    if os.name != "nt":
        return

    host = rustc_host()
    if host is None:
        raise RuntimeError("Failed to resolve rustc host via the rustup proxy.")
    if not host.endswith(WINDOWS_MSVC_SUFFIX):
        raise RuntimeError(
            "Windows Rust toolchain resolved to "
            f"{host}, expected an MSVC host ending in {WINDOWS_MSVC_SUFFIX}. "
            "Ensure rustup's cargo bin is used instead of Chocolatey or GNU shims."
        )
