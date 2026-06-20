#!/usr/bin/env python3
"""PostToolUse hook: enforces README.md presence + minimum size in every directory.

After any Edit/Write, checks:

  1. The directory containing the edited file has a README.md, AND
  2. That README.md is at least MIN_README_LINES lines long.

Both checks feed an error back to Claude when violated. The second
check exists to prevent the "placeholder README" pattern where a
README.md gets created just to satisfy this hook and never receives
real content — a 50-line floor forces enough prose that a reader
actually learns what the directory is for.

Exit codes:
  0 - README.md exists, has >= MIN_README_LINES lines, or check not applicable
  2 - README.md missing OR exists but is too short (stderr fed back to Claude)
"""

import json
import os
import sys
from pathlib import Path

EXCLUDED_DIRS = {".git", ".github", "target", ".loop", "__pycache__", ".venv", "node_modules"}

# Minimum line count for a README.md to count as "real content".
# Chosen to be just past the natural placeholder size (a one-line title
# plus a brief paragraph) so a README either gets meaningful prose or
# trips the guard. Counts every line including blanks/headings — the
# point is reading effort, not net content.
MIN_README_LINES = 50


def _count_lines(path: Path) -> int:
    """Count newlines in `path`; returns 0 on read error."""
    try:
        with path.open("rb") as f:
            return sum(1 for _ in f)
    except OSError:
        return 0


def _readme_too_short_message(readme_path: Path, loc: int) -> str:
    return (
        f"README.md too short: {readme_path} has {loc} line(s), "
        f"minimum is {MIN_README_LINES}. Expand it with what's in this "
        f"directory + why + key entry points (files, public types, how "
        f"the agent should navigate). A {MIN_README_LINES}-line floor "
        f"forces enough prose to actually orient a new reader."
    )


def main():
    try:
        data = json.load(sys.stdin)
    except json.JSONDecodeError:
        return 0

    file_path = data.get("tool_input", {}).get("file_path", "")
    if not file_path:
        return 0

    # Normalize path
    norm_file_path = file_path.replace("\\", "/")
    filename = os.path.basename(norm_file_path)
    directory = os.path.dirname(norm_file_path)

    # If the file being written IS a README.md, check ITS line count and
    # return early either way — the directory clearly has a README now,
    # so the presence check is satisfied; the only thing left to enforce
    # is the size floor on the freshly-written file.
    if filename == "README.md":
        readme_loc = _count_lines(Path(file_path))
        if 0 < readme_loc < MIN_README_LINES:
            print(_readme_too_short_message(Path(file_path), readme_loc), file=sys.stderr)
            return 2
        return 0

    # Skip excluded directories
    parts = Path(norm_file_path).parts
    if any(part in EXCLUDED_DIRS for part in parts):
        return 0

    # Locate the README.md in the edited file's directory. Try the
    # normalized path first, then the original (un-normalized) for the
    # path-separator-sensitive Windows case.
    candidate_dirs: list[str] = []
    if directory:
        candidate_dirs.append(directory)
    orig_path = data.get("tool_input", {}).get("file_path", "")
    orig_dir = os.path.dirname(orig_path)
    if orig_dir and orig_dir not in candidate_dirs:
        candidate_dirs.append(orig_dir)

    for d in candidate_dirs:
        readme = Path(d) / "README.md"
        if readme.is_file():
            # README exists — now enforce the size floor.
            readme_loc = _count_lines(readme)
            if readme_loc < MIN_README_LINES:
                print(_readme_too_short_message(readme, readme_loc), file=sys.stderr)
                return 2
            return 0

    print(f"Missing README.md in directory: {directory}", file=sys.stderr)
    print("Every directory must have a README.md. Please create one.", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main())
