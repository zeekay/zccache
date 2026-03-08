#!/usr/bin/env python3
"""Execute a command using this project's Rust toolchain.

Usage (via ./run wrapper):
    ./run cargo check --workspace
    ./run cargo test -p zccache-hash
    ./run rustc --version

Ensures the rustup-managed toolchain is used regardless of system PATH.
"""

import os
import shutil
import subprocess
import sys


def find_cargo_bin():
    """Find the rustup .cargo/bin directory."""
    cargo_home = os.environ.get("CARGO_HOME")
    if cargo_home:
        bin_dir = os.path.join(cargo_home, "bin")
        if os.path.isdir(bin_dir):
            return bin_dir

    home = os.path.expanduser("~")
    bin_dir = os.path.join(home, ".cargo", "bin")
    if os.path.isdir(bin_dir):
        return bin_dir

    userprofile = os.environ.get("USERPROFILE")
    if userprofile:
        bin_dir = os.path.join(userprofile, ".cargo", "bin")
        if os.path.isdir(bin_dir):
            return bin_dir

    return None


def main():
    if len(sys.argv) < 2:
        print("usage: ./run <command> [args...]", file=sys.stderr)
        print("  e.g. ./run cargo check --workspace", file=sys.stderr)
        sys.exit(1)

    cargo_bin = find_cargo_bin()
    if not cargo_bin:
        print("error: Cannot find .cargo/bin. Run ./install first.", file=sys.stderr)
        sys.exit(1)

    os.environ["PATH"] = cargo_bin + os.pathsep + os.environ.get("PATH", "")

    if not shutil.which("rustup"):
        print(f"error: rustup not found at {cargo_bin}. Run ./install first.", file=sys.stderr)
        sys.exit(1)

    result = subprocess.run(sys.argv[1:])
    sys.exit(result.returncode)


if __name__ == "__main__":
    main()
