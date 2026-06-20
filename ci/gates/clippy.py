"""`clippy` gate — `soldr cargo clippy --workspace --all-targets -- -D warnings`."""

from __future__ import annotations

from ._common import cargo_target_flag, heading, soldr_cargo


def run() -> int:
    heading("clippy")
    return soldr_cargo(
        "clippy",
        "--workspace",
        "--all-targets",
        *cargo_target_flag(),
        "--",
        "-D",
        "warnings",
    )
