"""Rust toolchain environment activation.

Single entry point for PATH normalization. All CI scripts import this:

    from ci.env import activate
    activate()

This makes the repo-local rustup installation canonical by default:
`./.cargo/bin` and `./.rustup`. Home-directory fallbacks remain for
compatibility when a repo-local toolchain has not been bootstrapped yet.
"""

import os
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parent.parent
PROJECT_CARGO_HOME = PROJECT_ROOT / ".cargo"
PROJECT_RUSTUP_HOME = PROJECT_ROOT / ".rustup"
USER_CARGO_HOME = Path(os.path.expanduser("~")) / ".cargo"
USER_RUSTUP_HOME = Path(os.path.expanduser("~")) / ".rustup"


def _cargo_exe_name():
    return "cargo.exe" if os.name == "nt" else "cargo"


def _bin_dir_has_cargo(bin_dir: Path) -> bool:
    return bin_dir.is_dir() and (bin_dir / _cargo_exe_name()).exists()


def preferred_toolchain_homes():
    """Return the best matching (cargo_home, rustup_home) pair to use."""
    if _bin_dir_has_cargo(PROJECT_CARGO_HOME / "bin"):
        return PROJECT_CARGO_HOME, PROJECT_RUSTUP_HOME
    if _bin_dir_has_cargo(USER_CARGO_HOME / "bin"):
        return USER_CARGO_HOME, USER_RUSTUP_HOME
    return None, None


def _candidate_cargo_homes():
    yield os.environ.get("CARGO_HOME", "")
    cargo_home, _ = preferred_toolchain_homes()
    if cargo_home:
        yield str(cargo_home)
    yield os.path.join(os.path.expanduser("~"), ".cargo")
    yield os.path.join(os.environ.get("USERPROFILE", ""), ".cargo")


def find_cargo_bin():
    """Find the preferred rustup .cargo/bin directory."""
    for candidate in _candidate_cargo_homes():
        if candidate:
            bin_dir = os.path.join(candidate, "bin")
            if os.path.isdir(bin_dir):
                return bin_dir
    return None


def activate():
    """Activate the preferred rustup toolchain for child processes."""
    cargo_home, rustup_home = preferred_toolchain_homes()
    if cargo_home and rustup_home:
        os.environ["CARGO_HOME"] = str(cargo_home)
        os.environ["RUSTUP_HOME"] = str(rustup_home)
    cargo_bin = find_cargo_bin()
    if cargo_bin:
        os.environ["PATH"] = cargo_bin + os.pathsep + os.environ.get("PATH", "")


def clean_env():
    """Return env dict with the preferred toolchain env and PATH applied."""
    activate()
    env = os.environ.copy()
    env.pop("VIRTUAL_ENV", None)
    return env
