"""`wrapper-e2e` gate — SOLDR_RUSTC_WRAPPER end-to-end against a fresh cargo lib.

Builds zccache + zccache-daemon, installs them on PATH, points
`SOLDR_RUSTC_WRAPPER` at the installed `zccache`, then runs
`cargo build --verbose` in a fresh `cargo new --lib` project with
`serde --features derive` as a non-trivial dep. Verifies:

  - `zccache` appears in the cargo build log (proves the wrapper
    actually ran for every rustc invocation)
  - `<cache-root>/logs/` was created (proves the daemon wrote
    state per the documented contract)

Skipped on non-default targets (musl: build-only).
"""

from __future__ import annotations

import os
import shutil
import subprocess
import tempfile
from pathlib import Path

from ._common import (
    env_with,
    find_built_binary,
    heading,
    is_platform,
    skip,
    soldr_cargo,
)


def run() -> int:
    heading("wrapper-e2e")
    if os.environ.get("CARGO_TARGET_FLAG", "").strip():
        return skip("wrapper-e2e", "default-target only")

    soldr_cargo("build", "-p", "zccache", "--bin", "zccache")
    soldr_cargo("build", "-p", "zccache", "--bin", "zccache-daemon")
    zcc = find_built_binary("zccache")
    zccd = find_built_binary("zccache-daemon")
    if zcc is None or zccd is None:
        print(f"FAIL: missing binaries (zcc={zcc}, zccd={zccd})")
        return 1

    install_dir = (
        Path(os.environ.get("USERPROFILE", str(Path.home()))) / ".local" / "bin"
        if is_platform("windows")
        else Path.home() / ".local" / "bin"
    )
    install_dir.mkdir(parents=True, exist_ok=True)
    ext = ".exe" if is_platform("windows") else ""
    shutil.copy2(zcc, install_dir / f"zccache{ext}")
    shutil.copy2(zccd, install_dir / f"zccache-daemon{ext}")
    if not is_platform("windows"):
        os.chmod(install_dir / "zccache", 0o755)
        os.chmod(install_dir / "zccache-daemon", 0o755)

    runner_tmp = os.environ.get("RUNNER_TEMP", tempfile.gettempdir())
    cache_dir = Path(runner_tmp) / "wrapper-e2e-zccache"
    env = env_with(
        ("PATH", f"{install_dir}{os.pathsep}{os.environ.get('PATH', '')}"),
        ("SOLDR_RUSTC_WRAPPER", str(install_dir / f"zccache{ext}")),
        ("SOLDR_CACHE_LIFECYCLE", "command"),
        ("SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS", "30"),
        ("ZCCACHE_CACHE_DIR", str(cache_dir)),
    )

    with tempfile.TemporaryDirectory(prefix="zc-wrap-e2e-") as td:
        project = Path(td) / "hello"
        subprocess.run(
            ["soldr", "cargo", "new", "--lib", "hello"],
            cwd=td,
            env=env,
            check=True,
        )
        subprocess.run(
            ["soldr", "cargo", "add", "serde", "--features", "derive"],
            cwd=project,
            env=env,
            check=True,
        )
        build = subprocess.run(
            ["soldr", "cargo", "build", "--verbose"],
            cwd=project,
            env=env,
            capture_output=True,
            text=True,
        )
        log = build.stdout + build.stderr
        (project / "build.log").write_text(log, encoding="utf-8")
        if build.returncode != 0:
            print("FAIL: cargo build failed inside wrapper-e2e")
            print(log[-4000:])
            return 1
        if "zccache " not in log:
            print("FAIL: 'zccache ' not seen in build log — wrapper didn't run")
            return 1

    # Confirm the daemon left its log dir behind under the version-namespaced
    # cache root (#763).
    root_proc = subprocess.run(
        [str(install_dir / f"zccache{ext}"), "cache-root"],
        capture_output=True,
        text=True,
        env=env,
    )
    if root_proc.returncode != 0:
        print("FAIL: zccache cache-root failed after wrapper-e2e")
        return 1
    effective_root = Path(root_proc.stdout.strip())
    if not (effective_root / "logs").is_dir():
        print(f"FAIL: missing {effective_root}/logs after wrapper-e2e")
        return 1

    # Best-effort cleanup.
    subprocess.run([str(install_dir / f"zccache{ext}"), "stop"], env=env, check=False)
    return 0
