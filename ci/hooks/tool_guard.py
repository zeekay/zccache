#!/usr/bin/env python3
"""PreToolUse hook: blocks bare Rust commands and bare python/pip."""

import json
import re
import sys


RUST_TOOLS = {
    "cargo",
    "rustc",
    "rustfmt",
    "clippy-driver",
    "cargo-clippy",
    "cargo-fmt",
    # `rustup` is also gated — `rustup run <toolchain> cargo ...` and
    # `rustup which ...` are the usual escape hatches around soldr's
    # toolchain selection. `soldr rustup` is a documented passthrough.
    "rustup",
    "rustdoc",
    "rust-gdb",
    "rust-lldb",
    "rust-analyzer",
}
PYTHON_TOOLS = {"python", "python3", "pip", "pip3"}
LEGACY_RUST_TRAMPOLINES = {"_cargo", "_rustc", "_rustfmt"}

# Path-shape: any path component named `bench`, `test`, or `tests` followed
# by a `.py` file. Used as a per-argument check on segments we've already
# identified as Python invocations — NOT a top-level command-string match
# (that's what made the old check too broad).
#
# Anchored to `(^|sep)` so `tests-fixture/foo.py` (no path separator
# directly before `tests`) doesn't false-positive. Backslash + forward-slash
# both count as separators so Windows paths match identically.
FORBIDDEN_PATH_RE = re.compile(
    r"""(?:^|[/\\])        # start or path separator
        (?:bench|tests?)   # forbidden directory name
        [/\\]              # path separator
        \S*\.py            # any .py file
    """,
    re.VERBOSE,
)

DENY_PYTHON_IN_CODE = (
    "Do not use Python for benchmarks or tests. "
    "Write them in Rust instead. Python is only for CI scripts and packaging."
)

SHELL_TOOL_NAMES = {
    "Bash",
    "Shell",
    "PowerShell",
    "shell_command",
    "functions.shell_command",
}


def _args_contain_forbidden_test_path(words):
    """True if any argument matches a `bench/` or `tests/` Python file path."""
    return any(FORBIDDEN_PATH_RE.search(w) for w in words)


def _is_env_assignment(word):
    return re.match(r"^[A-Za-z_][A-Za-z0-9_]*=", word) is not None


def _command_words(seg):
    words = seg.split()
    if words and words[0] == "env":
        words = words[1:]
    while words and _is_env_assignment(words[0]):
        words = words[1:]
    return words


def _resolve_uv_run_tool(seg):
    m = re.match(r"uv\s+run\s+--script\s+(\S+)", seg)
    if m:
        return m.group(1)
    m = re.match(r"uv\s+run\s+(\S+)", seg)
    return m.group(1) if m else None


def check_command(command):
    """Return (tool, reason) if forbidden, otherwise None.

    The forbidden-test-path check (Python-on-bench/tests-*.py) runs per
    segment, after env-var stripping, and ONLY when the segment is a
    Python invocation. This is the deliberate narrowing from a prior
    implementation that matched the test-path regex against the entire
    command string — that approach false-positive-blocked `gh api .../
    tests/foo.py`, `wc -l tests/foo.py`, `uv run pytest tests/foo.py`,
    and any other command that happened to reference a test path as an
    argument. Now only `python tests/foo.py` (and `uv run python
    tests/foo.py`) trigger the block.
    """
    segments = re.split(r"&&|\|\||;", command)

    for seg in segments:
        seg = seg.strip()
        if not seg:
            continue

        words = _command_words(seg)
        if not words:
            continue

        first = words[0]
        bare = first.lstrip("./\\")
        normalized = " ".join(words)

        if bare in LEGACY_RUST_TRAMPOLINES:
            return (
                bare,
                f"Use `soldr {bare[1:]} ...` instead of legacy `./{bare}`. "
                "The root Rust trampolines have been removed.",
            )

        if normalized.startswith("uv run ") or normalized.startswith("uv  run "):
            tool = _resolve_uv_run_tool(normalized)
            if tool is None:
                continue
            tool_bare = tool.lstrip("./\\")
            if tool_bare in LEGACY_RUST_TRAMPOLINES:
                return (
                    tool_bare,
                    f"Use `soldr {tool_bare[1:]} ...` instead of legacy `{tool}`. "
                    "The root Rust trampolines have been removed.",
                )
            if tool in RUST_TOOLS:
                return (
                    tool,
                    f"Use `soldr {tool} ...` instead of `uv run {tool} ...`. "
                    "`uv run <rust-tool>` bypasses soldr's toolchain selection.",
                )
            # `uv run python tests/foo.py` — Python is the executor and a
            # forbidden path is in the args. Block.
            if tool in PYTHON_TOOLS and not tool.startswith("pip"):
                if _args_contain_forbidden_test_path(words):
                    return ("python", DENY_PYTHON_IN_CODE)
            # `uv run --script tests/foo.py` — `tool` IS the path being
            # executed as a Python script. Block when it's under bench/
            # or tests/.
            if FORBIDDEN_PATH_RE.search(tool):
                return ("python", DENY_PYTHON_IN_CODE)
            continue

        if normalized.startswith("uv pip "):
            continue

        if normalized.startswith("soldr "):
            continue

        if first in RUST_TOOLS:
            return (
                first,
                f"Use `soldr {first} ...` instead of bare `{first}`. "
                "soldr resolves the pinned rustup-managed toolchain.",
            )

        if first in PYTHON_TOOLS:
            if first.startswith("pip"):
                suggestion = (
                    f"uv pip {' '.join(seg.split()[1:])}"
                    if len(seg.split()) > 1
                    else "uv pip ..."
                )
                return (
                    first,
                    f"Use `{suggestion}` instead of bare `{first}`. "
                    "All pip operations must go through uv.",
                )
            # `python tests/foo.py` — give the specific test-file message
            # rather than the generic "use uv run". Both block, the
            # specific one tells the author WHY their test file shouldn't
            # be Python.
            if _args_contain_forbidden_test_path(words):
                return ("python", DENY_PYTHON_IN_CODE)
            return (
                first,
                f"Use `uv run ...` instead of bare `{first}`. "
                "All Python must be executed through uv.",
            )

    return None


def deny(reason):
    json.dump(
        {
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }
        },
        sys.stdout,
    )


def main():
    try:
        data = json.load(sys.stdin)
    except json.JSONDecodeError:
        sys.exit(0)

    if data.get("tool_name", "") not in SHELL_TOOL_NAMES:
        sys.exit(0)

    command = data.get("tool_input", {}).get("command", "")
    if not command:
        sys.exit(0)

    result = check_command(command)
    if result:
        _, reason = result
        deny(reason)

    sys.exit(0)


if __name__ == "__main__":
    main()
