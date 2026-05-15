"""Regression tests for ci/build_dylint_driver.py.

The driver build must not invoke `cargo +<toolchain>` directly because
CI runners front PATH with `cargo` shims (e.g. soldr's) that do not
understand the `+<toolchain>` directive — only the rustup-managed
`cargo` wrapper does. Use `rustup run <toolchain> cargo ...` instead so
the toolchain is selected via rustup regardless of PATH ordering.

See: failing CI run
https://github.com/zackees/zccache/actions/runs/25892828715/job/76099518521
"""

from __future__ import annotations

from pathlib import Path

from ci import build_dylint_driver


SCRIPT_PATH = Path(build_dylint_driver.__file__)
SCRIPT_TEXT = SCRIPT_PATH.read_text(encoding="utf-8")


def test_script_does_not_invoke_cargo_with_plus_toolchain() -> None:
    """`cargo +<toolchain>` is broken under PATH shims; must use rustup run."""
    assert 'f"+{TOOLCHAIN_CHANNEL}"' not in SCRIPT_TEXT, (
        "build_dylint_driver.py must not use `cargo +<toolchain>` or "
        "`rustc +<toolchain>` — those only work through the rustup wrapper, "
        "and CI runners may have a non-rustup `cargo`/`rustc` shim first on "
        "PATH. Use `rustup run <toolchain> cargo ...` instead."
    )


def test_script_invokes_cargo_via_rustup_run() -> None:
    assert '"rustup", "run", TOOLCHAIN_CHANNEL, "cargo", "build"' in SCRIPT_TEXT, (
        "build_dylint_driver.py should invoke cargo as "
        "`rustup run <TOOLCHAIN_CHANNEL> cargo build`."
    )


def test_script_invokes_rustc_via_rustup_run() -> None:
    assert '"rustup", "run", TOOLCHAIN_CHANNEL, "rustc", "-vV"' in SCRIPT_TEXT, (
        "build_dylint_driver.py should invoke rustc as "
        "`rustup run <TOOLCHAIN_CHANNEL> rustc -vV`."
    )


def test_toolchain_channel_is_pinned() -> None:
    # Sanity: the constant exists and is a non-empty nightly channel.
    channel = build_dylint_driver.TOOLCHAIN_CHANNEL
    assert isinstance(channel, str) and channel.startswith("nightly-"), channel
