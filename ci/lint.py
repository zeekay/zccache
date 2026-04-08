"""Run workspace linting: rustfmt check + clippy.

Usage:
    ./lint              # full workspace lint
    ./lint --fix        # auto-fix formatting + clippy
    ./lint <file.rs>    # single-file rustfmt + per-crate clippy
"""

import os
import subprocess
import sys
from shutil import which
from pathlib import Path

from ci.env import activate, clean_env

SCRIPT_DIR = Path(__file__).parent.parent.resolve()


def run_cmd(cmd):
    """Run a command rooted at the project directory."""
    return subprocess.run(
        cmd,
        text=True,
        encoding="utf-8",
        errors="replace",
        cwd=str(SCRIPT_DIR),
        env=clean_env(),
    )


def detect_crate(file_path):
    """Extract crate name from a file path under crates/."""
    normalized = file_path.replace("\\", "/")
    if "crates/" in normalized:
        parts = normalized.split("crates/")
        if len(parts) > 1:
            crate_dir = parts[1].split("/")[0]
            if crate_dir:
                return crate_dir
    return None


def lint_single_file(file_path):
    """Lint a single .rs file: rustfmt + per-crate clippy."""
    file_path = os.path.abspath(file_path)

    if not file_path.endswith(".rs"):
        print(f"Skipping non-Rust file: {file_path}", file=sys.stderr)
        return 0

    if not os.path.isfile(file_path):
        print(f"File not found: {file_path}", file=sys.stderr)
        return 1

    result = run_cmd(["rustfmt", file_path])
    if result.returncode != 0:
        return result.returncode

    crate = detect_crate(file_path)
    cmd = ["cargo", "clippy"]
    if crate:
        cmd += ["-p", crate]
    else:
        cmd += ["--workspace"]
    cmd += ["--all-targets", "--", "-D", "warnings"]

    result = run_cmd(cmd)
    return result.returncode


def lint_workspace():
    """Full workspace lint: fmt check + clippy + doc check."""
    result = run_cmd(["cargo", "fmt", "--all", "--check"])
    if result.returncode != 0:
        print("Formatting issues found. Run './lint --fix' to auto-fix.", file=sys.stderr)
        return result.returncode

    result = run_cmd([
        "cargo", "fmt",
        "--manifest-path", "dylints/ban_std_pathbuf/Cargo.toml",
        "--all", "--check",
    ])
    if result.returncode != 0:
        print("Dylint library formatting issues found.", file=sys.stderr)
        return result.returncode

    result = run_cmd([
        "cargo", "clippy", "--workspace", "--all-targets",
        "--", "-D", "warnings",
    ])
    if result.returncode != 0:
        return result.returncode

    if which("cargo-dylint") is None:
        print(
            "cargo-dylint is required for workspace linting. Install with "
            "'cargo install cargo-dylint dylint-link'.",
            file=sys.stderr,
        )
        return 1

    result = run_cmd([
        "cargo", "dylint", "--all", "--workspace",
    ])
    if result.returncode != 0:
        return result.returncode

    env = clean_env()
    env["RUSTDOCFLAGS"] = "-D warnings"
    result = subprocess.run(
        ["cargo", "doc", "--workspace", "--no-deps"],
        text=True,
        encoding="utf-8",
        errors="replace",
        cwd=str(SCRIPT_DIR),
        env=env,
    )
    return result.returncode


def main():
    activate()
    args = sys.argv[1:]

    if "--fix" in args:
        args.remove("--fix")
        result = run_cmd(["cargo", "fmt", "--all"])
        if result.returncode != 0:
            return result.returncode
        if not args:
            result = run_cmd([
                "cargo", "clippy", "--workspace", "--all-targets",
                "--", "-D", "warnings",
            ])
            return result.returncode

    if args and args[0].endswith(".rs"):
        return lint_single_file(args[0])

    return lint_workspace()


if __name__ == "__main__":
    sys.exit(main())
