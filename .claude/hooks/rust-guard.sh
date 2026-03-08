#!/usr/bin/env bash
# rust-guard.sh — PreToolUse hook that blocks bare Rust commands.
#
# Ensures all cargo/rustc/rustfmt/clippy invocations go through ./run
# so the correct rustup-managed toolchain is always used.

set -eo pipefail

INPUT="$(cat)"
COMMAND="$(echo "$INPUT" | jq -r '.tool_input.command // empty')"

# Nothing to check if command is empty
[ -z "$COMMAND" ] && exit 0

# Check if any line in the command starts with a bare Rust tool.
# We need to handle: simple commands, chained commands (&&, ||, ;), and pipes.
# Split on common command separators and check each segment.
check_segment() {
    local seg="$1"
    # Trim leading whitespace
    seg="$(echo "$seg" | sed 's/^[[:space:]]*//')"
    # Get the first word
    local first_word
    first_word="$(echo "$seg" | awk '{print $1}')"

    case "$first_word" in
        cargo|rustc|rustfmt|clippy-driver|cargo-clippy|cargo-fmt)
            return 0  # This is a bare Rust command
            ;;
    esac
    return 1
}

# Check if the command is already wrapped with ./run or bash run
is_wrapped() {
    local seg="$1"
    seg="$(echo "$seg" | sed 's/^[[:space:]]*//')"
    case "$seg" in
        ./run\ *|bash\ run\ *|"bash run "*)
            return 0 ;;
    esac
    return 1
}

# Split command on &&, ||, ; and check each part
# Use a simple approach: replace separators with newlines and check each line
SEGMENTS="$(echo "$COMMAND" | sed 's/&&/\n/g; s/||/\n/g; s/;/\n/g')"

while IFS= read -r segment; do
    [ -z "$segment" ] && continue
    if check_segment "$segment" && ! is_wrapped "$segment"; then
        # Found a bare Rust command — deny it
        TOOL="$(echo "$segment" | sed 's/^[[:space:]]*//' | awk '{print $1}')"
        # Detect platform for the error message
        case "$(uname -s 2>/dev/null)" in
            MINGW*|MSYS*|CYGWIN*|*_NT*)
                MSG="Use \`./run ${TOOL} ...\` instead of bare \`${TOOL}\`. The ./run wrapper ensures the correct Rust toolchain is used." ;;
            *)
                MSG="Use \`./run ${TOOL} ...\` instead of bare \`${TOOL}\`. The ./run wrapper ensures the correct Rust toolchain is used." ;;
        esac

        # Output structured deny response
        jq -n --arg reason "$MSG" '{
            hookSpecificOutput: {
                hookEventName: "PreToolUse",
                permissionDecision: "deny",
                permissionDecisionReason: $reason
            }
        }'
        exit 0
    fi
done <<< "$SEGMENTS"

# Command is fine — allow it
exit 0
