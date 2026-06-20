"""`docs` gate — `soldr cargo doc --workspace --no-deps` with -D warnings."""

from __future__ import annotations

from ._common import env_with, heading, is_platform, skip, soldr_cargo


def run() -> int:
    heading("docs")
    # Docs are platform-agnostic; running on Linux x86 is sufficient
    # coverage. Other platforms would emit identical rustdoc output.
    if not is_platform("linux"):
        return skip("docs", "linux-only (rustdoc output is platform-agnostic)")
    env = env_with(("RUSTDOCFLAGS", "-D warnings"))
    return soldr_cargo("doc", "--workspace", "--no-deps", env=env)
