"""`loc` gate — workspace-wide line-count budget.

Walks every tracked source file (.rs, .py, .ts, ...) and:
  - emits a warning when LOC > WARN
  - fails the gate when LOC > ERROR

This is the same enforcement the deleted PostToolUse hook used to apply
after every Edit/Write, but now lives in the lint pipeline so CI catches
budget breaches regardless of how the file got into the tree (commits
landed without Claude in the loop, rebases that re-merged a split file,
generated code, etc).

Refactor convention when this fires: `foo.rs` -> `foo/mod.rs` +
per-domain submodules, with `pub use` re-exports in `mod.rs` so the
public path is unchanged.
"""

from __future__ import annotations

import sys
from pathlib import Path

from ._common import REPO_ROOT, heading

WARN_THRESHOLD = 1000
ERROR_THRESHOLD = 1500

SOURCE_EXTS = {
    ".rs",
    ".py",
    ".ts",
    ".tsx",
    ".js",
    ".jsx",
    ".go",
    ".java",
    ".kt",
    ".swift",
    ".c",
    ".cc",
    ".cpp",
    ".cxx",
    ".h",
    ".hh",
    ".hpp",
}

EXCLUDED_DIRS = {
    ".git",
    "target",
    ".cargo",
    ".rustup",
    ".venv",
    "node_modules",
    "__pycache__",
    "dist",
    ".claude",
    ".extern-repos",
    ".perf-local",
}


def _count_lines(path: Path) -> int:
    try:
        with path.open("rb") as f:
            return sum(1 for _ in f)
    except OSError:
        return 0


def _walk_source_files(root: Path):
    """Yield every source-file path under `root`, skipping excluded dirs.

    Walks manually so we can prune EXCLUDED_DIRS at every level rather
    than paying the cost of walking into `target/` (>1 GB on a warm
    checkout) just to filter it out at the leaves.
    """
    stack: list[Path] = [root]
    while stack:
        d = stack.pop()
        try:
            entries = list(d.iterdir())
        except (OSError, PermissionError):
            continue
        for entry in entries:
            if entry.is_dir():
                if entry.name in EXCLUDED_DIRS:
                    continue
                stack.append(entry)
            elif entry.is_file() and entry.suffix in SOURCE_EXTS:
                yield entry


def run() -> int:
    heading("loc")
    warnings: list[tuple[Path, int]] = []
    errors: list[tuple[Path, int]] = []
    for src in _walk_source_files(REPO_ROOT):
        n = _count_lines(src)
        if n > ERROR_THRESHOLD:
            errors.append((src.relative_to(REPO_ROOT), n))
        elif n > WARN_THRESHOLD:
            warnings.append((src.relative_to(REPO_ROOT), n))

    for path, n in warnings:
        sys.stderr.write(
            f"WARN: {path} has {n} lines (> {WARN_THRESHOLD}). "
            f"Refactor down before it crosses {ERROR_THRESHOLD}.\n"
        )

    if errors:
        sys.stderr.write("\n")
        for path, n in errors:
            sys.stderr.write(
                f"ERROR: {path} has {n} lines (> {ERROR_THRESHOLD}). "
                f"Split into focused submodules (foo.rs -> foo/mod.rs + per-"
                f"domain files, with `pub use` re-exports preserving the "
                f"public path).\n"
            )
        return 1

    if warnings:
        sys.stdout.write(
            f"loc: {len(warnings)} file(s) over {WARN_THRESHOLD}-line warn "
            f"threshold; none over {ERROR_THRESHOLD}-line error threshold.\n"
        )
    else:
        sys.stdout.write(
            f"loc: all source files within {WARN_THRESHOLD}-line budget.\n"
        )
    return 0
