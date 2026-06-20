"""`test-compile` gate — pre-compile test binaries without running them.

Splits the heavy test-binary link off the actual `cargo test` run so a
broken test compile shows up as its own gate (instead of getting
folded into `unit`'s output). Cheap on warm caches.
"""

from __future__ import annotations

from ._common import cargo_target_flag, heading, soldr_cargo


def run() -> int:
    heading("test-compile")
    return soldr_cargo(
        "test",
        "--workspace",
        "--lib",
        "--bins",
        "--no-fail-fast",
        "--no-run",
        *cargo_target_flag(),
    )
