# Agent Hooks

Python scripts invoked by Claude Code and Codex hooks at well-defined points in the agent lifecycle. Claude Code loads `.claude/settings.json`; Codex loads `.codex/hooks.json`. All hooks are executed via `uv run` so the Python interpreter, version, and dep set are deterministic across maintainer machines.

## What lives here

| Script | Phase | What it enforces |
|---|---|---|
| [`tool_guard.py`](tool_guard.py) | PreToolUse (Bash) | Blocks bare `cargo` / `rustc` / `rustfmt` / `clippy-driver` / `rustup` / `rustdoc` / `python` / `pip` invocations. The repo's contract is that every Rust command goes through `soldr cargo …` (so the rustup-managed toolchain pinned by `rust-toolchain.toml` is used) and every Python command goes through `uv run …` / `uv pip …` (so deps are isolated). The hook scans the command line, refuses the call, and prints the canonical replacement. |
| [`readme_guard.py`](readme_guard.py) | PostToolUse (Edit\|Write) | After any file edit/write, asserts the containing directory has a `README.md` AND that file is at least 50 lines. The minimum-size floor exists to prevent the placeholder-README pattern; a 50-line floor forces enough prose to actually orient a new reader. Editing a `README.md` directly triggers the same size check on the freshly-written file. |
| [`check-on-start.py`](check-on-start.py) | SessionStart | Captures a snapshot of the git fingerprint (HEAD + working-tree status) so subsequent session-end checks can compare against the start state. Useful for diffing what an autonomous loop actually touched. |
| [`lint.py`](lint.py) | (legacy) | Earlier per-edit lint stub, kept for reference. Real linting now lives in `ci/gates/` (see [`../gates/README.md`](../gates/README.md)) and is invoked via `./ci.sh <gate>`. |

(Two scripts previously lived here — `loc_guard.py` for the per-file line-count budget — and were migrated to `ci/gates/loc.py` so CI catches budget breaches regardless of whether the agent or a human made the edit.)

## Exit-code contract

Every hook follows the same contract so the agent (Claude Code / Codex) can decide whether to surface the message or stop:

- `0` — pass; the hook produced no output the agent needs to act on, or only a warning that did not require blocking.
- `2` — block; stderr is fed back to the agent as a hard error. The tool call is treated as failed; the agent must either fix the underlying issue and retry, or abandon the action.

Other non-zero exits are treated as bugs in the hook itself — they do not block the agent, but they should be investigated since the hook silently lost its enforcement.

## How a hook learns what tool ran

Each hook reads a single JSON object from stdin. The schema mirrors the Claude Code hook spec:

- `tool_name` — `Bash` / `Edit` / `Write` / etc.
- `tool_input` — the verbatim tool args (`command`, `file_path`, `content`, `new_string`, …).

A hook that doesn't care about a particular shape (e.g. `readme_guard.py` ignores `Bash` calls) returns `0` on the spot rather than parsing the rest.

## Why these are hooks and not lint gates

The split is deliberate: lint gates (`ci/gates/*.py`) check the **workspace** state — same view a fresh clone in CI sees. Hooks check the **agent's intent** at the moment of an action: what command is about to run, what file is about to be written. The two surfaces catch different failure modes:

- `tool_guard.py` blocks `cargo build` *before* the build runs; a lint gate would only know the build happened after the fact.
- `readme_guard.py` fires when a brand-new directory gets its first file; a workspace walk wouldn't know which directories were newly-populated by *this* session.
- `loc_guard` *used to* live here, but its workspace shape was strictly better than its per-edit shape — it's now `ci/gates/loc.py`, where a `git push` from a developer's terminal trips the same check the agent would.

When in doubt: if the rule is about the state of the repo (size, presence, content correctness), it belongs in `ci/gates/`. If the rule is about what the agent is *doing* (command shape, write target, session lifecycle), it belongs here.

## Adding a new hook

1. Drop the script under `ci/hooks/<name>.py`.
2. Register it in `.claude/settings.json` (and `.codex/hooks.json` if it's worth running in Codex too) with the appropriate `matcher` and `timeout`.
3. Update the table above so future maintainers can find it by phase.
4. If the hook would be equally useful as a workspace-level lint, mirror it as a `ci/gates/<name>.py` so CI catches the same failure mode for non-Claude commits.

## Related docs

- [`../gates/README.md`](../gates/README.md) — the CI lint gates that complement these hooks; same enforcement style, workspace-wide scope.
- [`../../.claude/settings.json`](../../.claude/settings.json) — where Claude Code wires these hooks into the PreToolUse / PostToolUse / SessionStart phases.

