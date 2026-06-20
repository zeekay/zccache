#!/usr/bin/env bash
# Thin wrapper around `uv run --no-project --script ci.py`.
#
# - `--no-project`: do NOT discover the surrounding `pyproject.toml`.
#   Without this, uv (>= 0.4) walks up the tree, finds the workspace
#   pyproject, and triggers a maturin build before running anything.
#   The CI gates do not need the Python wheel of zccache — they call
#   soldr cargo directly — so the maturin build is pure waste.
# - `--script`: read inline PEP 723 deps from `ci.py` (pyyaml only)
#   and provision an isolated venv for them.
#
# Usage:
#   ./ci.sh fmt
#   ./ci.sh all
#   ./ci.sh --help
#
# The actual `cargo build` / test compile happens inside the
# soldr-managed cargo workspace via the gate functions — this wrapper
# is purely about how we LAUNCH ci.py, not what ci.py builds.
set -euo pipefail
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec uv run --no-project --script "$script_dir/ci.py" "$@"
