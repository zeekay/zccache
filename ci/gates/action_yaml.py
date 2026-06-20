"""`action-yaml` gate — structural validity of action.yml + action/cleanup/action.yml.

Replaces the deleted test-action.yml's structural assertions with a
sub-second YAML parse + spot-check. If action.yml structure drifts,
every downstream consumer of `zackees/zccache@v1` would break — this
gate fires before that happens.
"""

from __future__ import annotations

from ._common import REPO_ROOT, heading

# Inputs the action's documented contract requires. Adding an entry
# here is the right move when action.yml gains a load-bearing input
# whose absence would break downstream consumers.
REQUIRED_INPUTS = (
    "cache-cargo-registry",
    "cache-compilation",
    "cache-target",
    "zccache-version",
    "shared-key",
)


def run() -> int:
    heading("action-yaml")
    try:
        import yaml
    except ImportError:
        print("FAIL: PyYAML missing — declare it in ci.py's PEP 723 deps")
        return 1

    for path in (REPO_ROOT / "action.yml", REPO_ROOT / "action" / "cleanup" / "action.yml"):
        if not path.is_file():
            print(f"FAIL: missing {path}")
            return 1
        try:
            doc = yaml.safe_load(path.read_text(encoding="utf-8"))
        except yaml.YAMLError as e:
            print(f"FAIL: {path} is not valid YAML: {e}")
            return 1
        runs = (doc or {}).get("runs", {})
        if runs.get("using") != "composite":
            print(f"FAIL: {path} must declare `runs.using: composite`, got {runs.get('using')!r}")
            return 1

    # Top-level action.yml inputs are the public contract.
    action = yaml.safe_load((REPO_ROOT / "action.yml").read_text(encoding="utf-8"))
    inputs = (action or {}).get("inputs", {})
    missing = [name for name in REQUIRED_INPUTS if name not in inputs]
    if missing:
        print(f"FAIL: action.yml missing required inputs: {missing}")
        return 1

    print("action.yml + action/cleanup/action.yml structurally valid")
    return 0
