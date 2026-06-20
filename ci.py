#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["pyyaml>=6"]
# ///
"""ci.py — single-file dispatcher for every CI gate.

Usage:
    ./ci.sh <gate>
    ./ci.sh all

Gates (each maps 1:1 to a step in .github/workflows/ci.yml):
    fmt, clippy, dylint, docs, build, test-compile, unit,
    integration, cargo-registry, gha-cache, wrapper-e2e,
    action-yaml, action-surface

`all` runs every gate in declaration order with continue-past-failures
semantics. `build` is the one exception — if it fails, downstream gates
are skipped and the final summary reports only `build`. Other failures
accumulate; the summary prints `FAILED: <gate1> <gate2> ...` and exits 1.

Invoked via PEP 723 (`uv run --script`) so the surrounding
`pyproject.toml` (which would otherwise drag in the maturin build)
is NOT loaded. The script's own deps (pyyaml for `action-yaml`) come
from the inline script-metadata block above.

The script's directory is at sys.path[0] by Python's standard
interpreter rules, so `from ci.gates import GATES` resolves without
any manual sys.path manipulation.
"""

from __future__ import annotations

import sys

from ci.gates import FATAL_GATES, GATES


USAGE = f"""\
usage: ./ci.sh <gate>|all

gates: {' '.join(GATES)}
"""


def main(argv: list[str]) -> int:
    if not argv or argv[0] in ("-h", "--help", "help"):
        sys.stdout.write(USAGE)
        return 0

    target = argv[0]

    if target == "all":
        failures: list[str] = []
        for name, fn in GATES.items():
            rc = fn()
            if rc != 0:
                failures.append(name)
                if name in FATAL_GATES:
                    sys.stdout.write(
                        f"\n=== HALT: fatal gate `{name}` failed ===\n"
                        f"every downstream gate requires a compiled tree; "
                        f"skipping the rest.\n"
                    )
                    break
        sys.stdout.write("\n")
        if failures:
            sys.stdout.write(f"FAILED: {' '.join(failures)}\n")
            return 1
        sys.stdout.write("all gates passed\n")
        return 0

    fn = GATES.get(target)
    if fn is None:
        sys.stderr.write(f"unknown gate: {target}\n{USAGE}")
        return 2
    return fn()


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
