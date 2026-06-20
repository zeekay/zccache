"""Per-gate runners for the unified CI pipeline.

Each module under `ci.gates.*` exposes a single `run() -> int` function
that performs one CI gate (fmt, clippy, dylint, build, unit, ...) and
returns the gate's exit code. `ci.py` at the repo root dispatches to
these by name.

The split exists so:
- `.github/workflows/ci.yml` steps are one-liners
  (`uv run --script ci.py <gate>`) — all the gate logic lives in
  testable, lintable, locally-reproducible Python.
- The same gates can be invoked locally via `uv run --script ci.py all`
  (or any subset) without GitHub Actions, soldr-cook, or the maturin
  build the parent pyproject.toml would otherwise trigger.
"""

from __future__ import annotations

from typing import Callable, Dict

from . import (
    action_surface,
    action_yaml,
    build,
    cargo_registry,
    clippy,
    docs,
    dylint,
    fmt,
    gha_cache,
    integration,
    test_compile,
    unit,
    wrapper_e2e,
)

# Insertion order matters: `ci.py all` runs gates in this order. Build
# is the only fatal-on-failure gate (everything downstream needs a
# compiled tree); the rest accumulate failures via the runner in
# `ci.py`.
GATES: Dict[str, Callable[[], int]] = {
    "fmt": fmt.run,
    "clippy": clippy.run,
    "dylint": dylint.run,
    "docs": docs.run,
    "build": build.run,
    "test-compile": test_compile.run,
    "unit": unit.run,
    "integration": integration.run,
    "cargo-registry": cargo_registry.run,
    "gha-cache": gha_cache.run,
    "wrapper-e2e": wrapper_e2e.run,
    "action-yaml": action_yaml.run,
    "action-surface": action_surface.run,
}

# Gates that, on failure, should halt `ci.py all` immediately because
# every downstream gate depends on the compiled tree they produce.
FATAL_GATES = frozenset({"build"})
