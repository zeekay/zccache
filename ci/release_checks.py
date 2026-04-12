"""Shared release metadata validation for lint, test, and publish."""

from __future__ import annotations

import json
import subprocess
import tomllib
from pathlib import Path
from typing import Any

ROOT = Path(__file__).parent.parent.resolve()
RUST_PUBLISH_ORDER = [
    "zccache-core",
    "zccache-gha",
    "zccache-hash",
    "zccache-protocol",
    "zccache-download",
    "zccache-download-protocol",
    "zccache-ipc",
    "zccache-download-client",
    "zccache-download-daemon",
    "zccache-download-cli",
    "zccache-fscache",
    "zccache-artifact",
    "zccache-depgraph",
    "zccache-compiler",
    "zccache-watcher",
    "zccache-fingerprint",
    "zccache-test-support",
    "zccache-cli",
    "zccache-daemon",
]
VERSIONED_PYPROJECTS = (
    ROOT / "pyproject.toml",
    ROOT / "crates" / "zccache-watcher" / "pyproject.toml",
    ROOT / "crates" / "zccache-fingerprint" / "pyproject.toml",
)


class ReleaseCheckError(RuntimeError):
    """Raised when release metadata is inconsistent."""


def _read_toml(path: Path) -> dict[str, Any]:
    with open(path, "rb") as f:
        return tomllib.load(f)


def read_workspace_version() -> str:
    data = _read_toml(ROOT / "Cargo.toml")
    return data["workspace"]["package"]["version"]


def read_workspace_metadata() -> dict[str, Any]:
    result = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=True,
    )
    return json.loads(result.stdout)


def validate_release_versions() -> None:
    workspace_data = _read_toml(ROOT / "Cargo.toml")
    workspace_version = workspace_data["workspace"]["package"]["version"]
    errors: list[str] = []

    for path in VERSIONED_PYPROJECTS:
        project_version = _read_toml(path)["project"]["version"]
        if project_version != workspace_version:
            rel_path = path.relative_to(ROOT)
            errors.append(
                f"{rel_path} has version {project_version}, expected {workspace_version}"
            )

    # Workspace path dependencies: version pin is optional.
    # If present, it must match. If absent, that's fine (cargo resolves by path).
    expected_dependency_version = f"={workspace_version}"
    workspace_deps = workspace_data["workspace"].get("dependencies", {})
    for name, spec in workspace_deps.items():
        if not name.startswith("zccache-"):
            continue
        if not isinstance(spec, dict) or "path" not in spec:
            continue
        actual = spec.get("version")
        if actual is not None and actual != expected_dependency_version:
            errors.append(
                f"workspace dependency {name} has version {actual}, "
                f"expected {expected_dependency_version}"
            )

    if errors:
        raise ReleaseCheckError(
            "Release version checks failed:\n  - " + "\n  - ".join(errors)
        )


def validate_rust_publish_order() -> None:
    metadata = read_workspace_metadata()
    packages = metadata.get("packages", [])
    package_by_name = {pkg["name"]: pkg for pkg in packages}
    publishable = {
        pkg["name"]
        for pkg in packages
        if pkg.get("publish") != []
    }
    configured = set(RUST_PUBLISH_ORDER)

    missing = sorted(publishable - configured)
    extra = sorted(configured - publishable)
    errors: list[str] = []
    if missing:
        errors.append(
            f"RUST_PUBLISH_ORDER is missing publishable crates: {', '.join(missing)}"
        )
    if extra:
        errors.append(
            f"RUST_PUBLISH_ORDER contains non-publishable crates: {', '.join(extra)}"
        )

    if not errors:
        order_index = {crate: i for i, crate in enumerate(RUST_PUBLISH_ORDER)}
        for crate in RUST_PUBLISH_ORDER:
            pkg = package_by_name[crate]
            for dep in pkg.get("dependencies", []):
                dep_name = dep["name"]
                if dep_name not in publishable:
                    continue
                if not dep.get("path"):
                    continue
                if dep.get("kind") not in (None, "build"):
                    continue
                if order_index[dep_name] >= order_index[crate]:
                    errors.append(
                        f"RUST_PUBLISH_ORDER schedules {crate} before its dependency {dep_name}"
                    )

    if errors:
        raise ReleaseCheckError(
            "Rust publish-order checks failed:\n  - " + "\n  - ".join(errors)
        )


def validate_release_metadata() -> None:
    validate_release_versions()
    validate_rust_publish_order()
