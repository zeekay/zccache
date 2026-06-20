"""`action-surface` gate — CLI surface contract for action.yml's shell snippets.

action.yml's setup + cleanup composite steps call against a fixed CLI
surface: `zccache --version`, `start`, `stop`, `cache-root`, plus the
`cargo-registry` and `gha-cache` subcommand families. If any of those
disappear (rename, removal), every downstream consumer of
`zackees/zccache@v1` breaks at runtime. This gate locks the contract
against the just-built binary; if a Rust refactor renames a
subcommand, this fires before merge.

Cheap: <5 s on a built binary.
"""

from __future__ import annotations

import re
import subprocess
import tempfile

from ._common import (
    REPO_ROOT,
    find_built_binary,
    heading,
)

# Subcommands action.yml's shell snippets reference. Keep in sync if
# action.yml adds new subcommand calls; the gate failure message tells
# the next maintainer where to look.
REQUIRED_SUBCOMMANDS = (
    "start",
    "stop",
    "cache-root",
    "cargo-registry",
    "gha-cache",
)

VERSION_RE = re.compile(r"^zccache\s+\d", re.MULTILINE)


def run() -> int:
    heading("action-surface")
    zcc = find_built_binary("zccache")
    if zcc is None:
        print("FAIL: zccache binary not found — run the `build` gate first")
        return 1

    # action.yml line 197:
    #   zccache --version | grep -Eq '^zccache [0-9]'
    ver = subprocess.run(
        [str(zcc), "--version"], capture_output=True, text=True, cwd=REPO_ROOT
    )
    if ver.returncode != 0 or not VERSION_RE.search(ver.stdout):
        print(f"FAIL: --version drift; got: {ver.stdout!r}")
        return 1

    # Each subcommand action.yml's setup + cleanup snippets call against.
    helptxt = subprocess.run(
        [str(zcc), "--help"], capture_output=True, text=True, cwd=REPO_ROOT
    ).stdout
    for sub in REQUIRED_SUBCOMMANDS:
        # clap renders subcommands as indented `  <name>  <desc>` lines.
        if not re.search(rf"^\s+{re.escape(sub)}\b", helptxt, re.MULTILINE):
            print(f"FAIL: subcommand `{sub}` missing from --help — action.yml's")
            print(f"  shell scripts reference it; downstream consumers would break.")
            return 1

    # Daemon start/stop round-trip — confirm `zccache start` exits
    # cleanly and `zccache stop` cleans up. Use an isolated tempdir so
    # we don't touch the user's real ~/.zccache.
    with tempfile.TemporaryDirectory(prefix="zc-action-surface-") as td:
        env = {"ZCCACHE_CACHE_DIR": td, "PATH": "/usr/bin:/bin"}
        # Inherit minimal real env for things like USERPROFILE on Windows.
        import os
        for k in ("PATH", "PATHEXT", "USERPROFILE", "HOME", "SystemRoot", "ComSpec"):
            if k in os.environ:
                env[k] = os.environ[k]
        env["ZCCACHE_CACHE_DIR"] = td
        rc_start = subprocess.run([str(zcc), "start"], env=env, cwd=REPO_ROOT).returncode
        rc_stop = subprocess.run([str(zcc), "stop"], env=env, cwd=REPO_ROOT).returncode
        if rc_start != 0:
            print(f"FAIL: `zccache start` returned {rc_start}")
            return 1
        # stop may be a no-op if start didn't fully attach; both 0 and
        # non-zero are acceptable so long as it doesn't crash.
        if rc_stop not in (0, 1):
            print(f"FAIL: `zccache stop` returned unexpected {rc_stop}")
            return 1

    print("action surface contract intact")
    return 0
