"""`gha-cache` gate — `zccache gha-cache status` smoke.

Linux x86 only (the gha-cache subcommand only does meaningful work
when GitHub's Actions Cache API endpoints are reachable, which is a
GHA-runner thing; running it elsewhere produces an unsurprising
`not on GHA` line, no value). The smoke just confirms the subcommand
parses + emits *something* without crashing.
"""

from __future__ import annotations

import subprocess

from ._common import (
    REPO_ROOT,
    find_built_binary,
    heading,
    is_platform,
    skip,
)


def run() -> int:
    heading("gha-cache")
    if not is_platform("linux"):
        return skip("gha-cache", "linux-only (subcommand contract is GHA-runner-shaped)")
    zcc = find_built_binary("zccache")
    if zcc is None:
        print("FAIL: zccache binary not found under target/")
        return 1
    proc = subprocess.run(
        [str(zcc), "gha-cache", "status"],
        capture_output=True,
        text=True,
        cwd=REPO_ROOT,
    )
    # Either exit code is acceptable — what we're locking is that the
    # subcommand parses + produces output. A non-zero exit on a
    # local-dev workstation just means "no GHA cache here", which is
    # fine. Catastrophic failure (subcommand was renamed, panicked,
    # etc.) would show as an empty/short output AND a panic-shaped
    # stderr; assert both are absent.
    out = (proc.stdout + proc.stderr).strip()
    if "panicked" in out or "not found" in out.lower() or len(out) < 5:
        print(f"FAIL: gha-cache status produced suspect output:\n{out}")
        return 1
    print(out)
    return 0
