"""Rust toolchain environment activation.

Single entry point for PATH normalization. All CI scripts import this:

    from ci.env import activate
    activate()

This prepends ~/.cargo/bin to PATH so rustup's cargo/rustc are always
found before any other Rust installations (e.g. Chocolatey).
"""

import os


def find_cargo_bin():
    """Find the rustup .cargo/bin directory."""
    for candidate in [
        os.environ.get("CARGO_HOME", ""),
        os.path.join(os.path.expanduser("~"), ".cargo"),
        os.path.join(os.environ.get("USERPROFILE", ""), ".cargo"),
    ]:
        if candidate:
            bin_dir = os.path.join(candidate, "bin")
            if os.path.isdir(bin_dir):
                return bin_dir
    return None


def activate():
    """Prepend .cargo/bin to PATH so rustup toolchain is always found first."""
    cargo_bin = find_cargo_bin()
    if cargo_bin:
        os.environ["PATH"] = cargo_bin + os.pathsep + os.environ.get("PATH", "")


def clean_env():
    """Return env dict with .cargo/bin on PATH and VIRTUAL_ENV removed."""
    activate()
    env = os.environ.copy()
    env.pop("VIRTUAL_ENV", None)
    return env
