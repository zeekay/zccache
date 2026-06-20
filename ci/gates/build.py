"""`build` gate — `soldr cargo check --workspace --all-targets`.

The one gate that is fatal-on-failure (`FATAL_GATES` in `ci.gates`):
every downstream gate (clippy already ran, but unit/integration/
wrapper-e2e/cargo-registry) requires the workspace to compile. The
top-level `ci.py all` dispatcher halts here and reports `build` as
the broken gate.
"""

from __future__ import annotations

from ._common import cargo_target_flag, heading, soldr_cargo


def run() -> int:
    heading("build")
    return soldr_cargo(
        "check",
        "--workspace",
        "--all-targets",
        *cargo_target_flag(),
    )
