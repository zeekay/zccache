#!/usr/bin/env python3
"""PostToolUse hook: runs rustfmt + clippy on edited Rust files.

Runs after every Edit/Write. For .rs files: auto-formats with rustfmt,
then runs clippy on the affected crate. Feeds errors back to Claude.

Exit codes:
  0 - Success or non-Rust file
  2 - Clippy violations found (stderr fed back to Claude)
"""

import json
import os
import subprocess
import sys
from pathlib import Path

SCRIPT_DIR = Path(__file__).parent.resolve()
PROJECT_ROOT = SCRIPT_DIR.parent.parent


def main():
    try:
        data = json.load(sys.stdin)
    except json.JSONDecodeError:
        return 0

    file_path = data.get("tool_input", {}).get("file_path", "")
    if not file_path:
        return 0

    # Normalize path
    file_path = file_path.replace("\\", "/")

    # Only lint Rust files
    if not file_path.endswith(".rs"):
        return 0

    # Skip deleted files
    if not os.path.isfile(file_path):
        return 0

    os.chdir(str(PROJECT_ROOT))
    run_py = str(PROJECT_ROOT / "run.py")

    # Auto-format
    subprocess.run(
        ["uv", "run", "python", run_py, "cargo", "fmt", "--all"],
        capture_output=True,
    )

    # Determine which crate was edited
    crate_arg = []
    if "crates/" in file_path:
        parts = file_path.split("crates/")
        if len(parts) > 1:
            crate_dir = parts[1].split("/")[0]
            if crate_dir:
                crate_arg = ["-p", crate_dir]

    # Run clippy
    cmd = ["uv", "run", "python", run_py, "cargo", "clippy"]
    if crate_arg:
        cmd += crate_arg
    else:
        cmd += ["--workspace"]
    cmd += ["--all-targets", "--", "-D", "warnings"]

    result = subprocess.run(cmd, capture_output=True, text=True)
    if result.returncode != 0:
        crate_name = crate_arg[1] if crate_arg else "workspace"
        print(f"clippy warnings in {crate_name} after editing {file_path}:", file=sys.stderr)
        output = result.stderr or result.stdout
        for line in output.strip().splitlines()[-40:]:
            print(line, file=sys.stderr)
        return 2

    return 0


if __name__ == "__main__":
    sys.exit(main())
