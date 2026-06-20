"""`fmt` gate — `soldr cargo fmt --all -- --check`."""

from __future__ import annotations

from ._common import heading, soldr_cargo


def run() -> int:
    heading("fmt")
    return soldr_cargo("fmt", "--all", "--", "--check")
