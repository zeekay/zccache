#!/usr/bin/env python3
"""PreToolUse hook: blocks bare Rust commands and bare python/pip.

All cargo/rustc must go through ./run (ensures correct toolchain).
All python must go through uv (ensures correct environment).

Exit codes:
  0 - Allow (outputs JSON hookSpecificOutput to deny if needed)
"""

import json
import re
import sys


RUST_TOOLS = {"cargo", "rustc", "rustfmt", "clippy-driver", "cargo-clippy", "cargo-fmt"}
PYTHON_TOOLS = {"python", "python3", "pip", "pip3"}

WRAPPED_PREFIXES = ("./run ", "uv run ", "uv pip ")


def check_command(command):
    """Check a command string for forbidden bare invocations.

    Returns (tool, reason) if forbidden, None if allowed.
    """
    segments = re.split(r"&&|\|\||;", command)

    for seg in segments:
        seg = seg.strip()
        if not seg:
            continue

        # Skip if properly wrapped
        if any(seg.startswith(p) for p in WRAPPED_PREFIXES):
            continue

        first_word = seg.split()[0] if seg.split() else ""

        if first_word in RUST_TOOLS:
            return (
                first_word,
                f"Use `./run {first_word} ...` instead of bare `{first_word}`. "
                f"The ./run wrapper ensures the correct Rust toolchain is used.",
            )

        if first_word in PYTHON_TOOLS:
            if first_word.startswith("pip"):
                suggestion = f"uv pip {' '.join(seg.split()[1:])}" if len(seg.split()) > 1 else "uv pip ..."
                return (
                    first_word,
                    f"Use `{suggestion}` instead of bare `{first_word}`. "
                    f"All pip operations must go through uv.",
                )
            return (
                first_word,
                f"Use `uv run python ...` instead of bare `{first_word}`. "
                f"All Python must be executed through uv.",
            )

    return None


def deny(reason):
    """Output a JSON deny response."""
    json.dump({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    }, sys.stdout)


def main():
    try:
        data = json.load(sys.stdin)
    except json.JSONDecodeError:
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
