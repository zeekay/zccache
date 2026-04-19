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


def _run_rustup_which(tool_name: str, env: dict[str, str]) -> Path | None:
    rustup = shutil.which("rustup", path=env.get("PATH"))
    if rustup is None:
        return None

    result = subprocess.run(
        [rustup, "which", tool_name],
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        env=env,
    )
    if result.returncode != 0:
        return None

    tool_path = result.stdout.strip()
    if not tool_path:
        return None
    return Path(tool_path)


def toolchain_bin() -> Path | None:
    env = os.environ.copy()
    env.setdefault("CARGO_HOME", str(cargo_home()))
    env.setdefault("RUSTUP_HOME", str(rustup_home()))
    tool_path = _run_rustup_which("cargo", env)
    if tool_path is None:
        return None
    return tool_path.parent


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

    env = clean_env()
    resolved_via_rustup = _run_rustup_which(tool_name, env)
    if resolved_via_rustup is not None and resolved_via_rustup.exists():
        return resolved_via_rustup

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

    current_path = env.get("PATH", "")
    path_parts = current_path.split(os.pathsep) if current_path else []
    prefix_parts: list[str] = []

    bin_dir = cargo_bin()
    if bin_dir.is_dir():
        prefix_parts.append(str(bin_dir))

    toolchain_dir = toolchain_bin()
    if toolchain_dir is not None and toolchain_dir.is_dir():
        # Keep rustup's proxy shims ahead of the resolved toolchain binaries so
        # nested `rust-toolchain.toml` files can still select their own channel.
        prefix_parts.append(str(toolchain_dir))

    normalized_prefixes = {
        os.path.normcase(os.path.normpath(part)) for part in prefix_parts
    }
    filtered_parts = [
        part
        for part in path_parts
        if part
        and os.path.normcase(os.path.normpath(part)) not in normalized_prefixes
    ]
    env["PATH"] = os.pathsep.join([*prefix_parts, *filtered_parts])


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
