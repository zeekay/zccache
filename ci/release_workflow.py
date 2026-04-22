"""Workflow-only release helpers for PyPI, crates.io, and wheel assembly."""

from __future__ import annotations

import argparse
import base64
import csv
import hashlib
import io
import json
import os
import shutil
import stat
import subprocess
import sys
import time
import tomllib
import urllib.error
import urllib.request
import zipfile
from pathlib import Path
from typing import Any

from ci.env import clean_env
from ci.release_checks import (
    RUST_PUBLISH_ORDER,
    ReleaseCheckError,
    read_workspace_version,
    stamp_internal_dependency_versions,
    validate_release_metadata,
)

ROOT = Path(__file__).resolve().parent.parent
DIST_DIR = ROOT / "dist"
WHEEL_DIR = DIST_DIR / "wheels"

ARTIFACT_MAP: dict[str, str] = {
    "binaries-x86_64-unknown-linux-gnu": "linux-x86_64-gnu",
    "binaries-aarch64-unknown-linux-gnu": "linux-aarch64-gnu",
    "binaries-x86_64-apple-darwin": "macos-x86_64",
    "binaries-aarch64-apple-darwin": "macos-aarch64",
    "binaries-x86_64-pc-windows-msvc": "windows-x86_64",
    "binaries-aarch64-pc-windows-msvc": "windows-arm64",
}

PLATFORMS: dict[str, list[str]] = {
    "linux-x86_64-gnu": ["manylinux_2_17_x86_64"],
    "linux-aarch64-gnu": ["manylinux_2_17_aarch64"],
    "macos-x86_64": ["macosx_10_12_x86_64"],
    "macos-aarch64": ["macosx_11_0_arm64"],
    "windows-x86_64": ["win_amd64"],
    "windows-arm64": ["win_arm64"],
}

REQUIRED_PLATFORM_FILES: dict[str, tuple[str, ...]] = {
    "linux-x86_64-gnu": ("zccache", "zccache-daemon", "zccache-fp"),
    "linux-aarch64-gnu": ("zccache", "zccache-daemon", "zccache-fp"),
    "macos-x86_64": ("zccache", "zccache-daemon", "zccache-fp"),
    "macos-aarch64": ("zccache", "zccache-daemon", "zccache-fp"),
    "windows-x86_64": ("zccache.exe", "zccache-daemon.exe", "zccache-fp.exe"),
    "windows-arm64": ("zccache.exe", "zccache-daemon.exe", "zccache-fp.exe"),
}

REQUIRED_NATIVE_FILES: dict[str, tuple[str, ...]] = {
    "linux-x86_64-gnu": (
        "python/zccache/_native.so",
        "python/zccache/fingerprint/_native.so",
        "python/zccache/watcher/_native.so",
    ),
    "linux-aarch64-gnu": (
        "python/zccache/_native.so",
        "python/zccache/fingerprint/_native.so",
        "python/zccache/watcher/_native.so",
    ),
    "macos-x86_64": (
        "python/zccache/_native.so",
        "python/zccache/fingerprint/_native.so",
        "python/zccache/watcher/_native.so",
    ),
    "macos-aarch64": (
        "python/zccache/_native.so",
        "python/zccache/fingerprint/_native.so",
        "python/zccache/watcher/_native.so",
    ),
    "windows-x86_64": (
        "python/zccache/_native.pyd",
        "python/zccache/fingerprint/_native.pyd",
        "python/zccache/watcher/_native.pyd",
    ),
    "windows-arm64": (
        "python/zccache/_native.pyd",
        "python/zccache/fingerprint/_native.pyd",
        "python/zccache/watcher/_native.pyd",
    ),
}


def log(message: str) -> None:
    print(message, file=sys.stderr, flush=True)


def run(cmd: list[str], **kwargs: Any) -> subprocess.CompletedProcess[Any]:
    log(f"  $ {' '.join(cmd)}")
    kwargs.setdefault("env", clean_env())
    return subprocess.run(cmd, check=True, **kwargs)


def read_project_meta() -> tuple[str, str, str, str, str]:
    with open(ROOT / "pyproject.toml", "rb") as f:
        data = tomllib.load(f)
    project = data["project"]
    readme = ""
    readme_field = project.get("readme", "")
    if readme_field:
        readme_path = ROOT / (
            readme_field
            if isinstance(readme_field, str)
            else readme_field.get("file", "")
        )
        if readme_path.exists():
            readme = readme_path.read_text(encoding="utf-8")
    return (
        project["name"],
        read_workspace_version(),
        project.get("description", ""),
        project.get("requires-python", ">=3.9"),
        readme,
    )


def record_hash(data: bytes) -> str:
    digest = hashlib.sha256(data).digest()
    return "sha256=" + base64.urlsafe_b64encode(digest).rstrip(b"=").decode()


def get_pypi_release_filenames(name: str, version: str) -> set[str] | None:
    url = f"https://pypi.org/pypi/{name}/json"
    try:
        with urllib.request.urlopen(url, timeout=10) as resp:
            data = json.loads(resp.read())
        release = data.get("releases", {}).get(version)
        if release is None:
            return None
        return {file["filename"] for file in release}
    except urllib.error.HTTPError as e:
        if e.code == 404:
            return None
        raise


def expected_pypi_wheel_filenames(name: str, version: str) -> set[str]:
    name_norm = name.replace("-", "_")
    return {
        f"{name_norm}-{version}-py3-none-{'.'.join(plat_tags)}.whl"
        for plat_tags in PLATFORMS.values()
    }


def check_pypi_version(name: str, version: str) -> set[str]:
    log(f"\n=== Pre-check PyPI for {name} {version} ===")
    try:
        url = f"https://pypi.org/pypi/{name}/json"
        with urllib.request.urlopen(url, timeout=10) as resp:
            data = json.loads(resp.read())
        latest_version = data.get("info", {}).get("version")
        existing = set(data.get("releases", {}).keys())
        release_files = get_pypi_release_filenames(name, version)
        if release_files is not None:
            expected_files = expected_pypi_wheel_filenames(name, version)
            if latest_version == version and expected_files.issubset(release_files):
                log(
                    f"  ERROR: {version} is already the latest PyPI release and all "
                    "expected wheels are present."
                )
                raise SystemExit(1)
            log(
                f"  {name} {version} already exists on PyPI with {len(release_files)} "
                "file(s); missing files may still be published."
            )
            return release_files
        log(f"  {name} {version} is available (existing: {', '.join(sorted(existing)) or 'none'})")
        return set()
    except urllib.error.HTTPError as e:
        if e.code == 404:
            log(f"  {name} is not yet published on PyPI")
            return set()
        log(f"  WARNING: PyPI check failed with HTTP {e.code}; continuing")
        return set()
    except (urllib.error.URLError, TimeoutError) as e:
        log(f"  WARNING: Could not reach PyPI ({e}); continuing")
        return set()


def crate_version_exists(name: str, version: str) -> bool:
    url = f"https://crates.io/api/v1/crates/{name}/{version}"
    try:
        with urllib.request.urlopen(url, timeout=10) as resp:
            json.loads(resp.read())
        return True
    except urllib.error.HTTPError as e:
        if e.code == 404:
            return False
        raise


def check_crates_versions(version: str) -> set[str]:
    log(f"\n=== Pre-check crates.io for Rust crates {version} ===")
    existing: list[str] = []
    for crate in RUST_PUBLISH_ORDER:
        try:
            if crate_version_exists(crate, version):
                existing.append(crate)
                log(f"  EXISTS: {crate} {version}")
            else:
                log(f"  OK: {crate} {version} is available")
        except (urllib.error.URLError, TimeoutError) as e:
            log(f"  WARNING: Could not reach crates.io for {crate} ({e})")

    if len(existing) == len(RUST_PUBLISH_ORDER):
        log("  ERROR: all publishable crates already exist on crates.io")
        raise SystemExit(1)

    if existing:
        log("  Resuming partial crates.io release; already-published crates will be skipped.")

    return set(existing)


def organize_downloaded_artifacts(artifacts_root: Path) -> None:
    if DIST_DIR.exists():
        shutil.rmtree(DIST_DIR)
    DIST_DIR.mkdir()

    missing_artifacts: list[str] = []
    for artifact_name, subdir in ARTIFACT_MAP.items():
        src = artifacts_root / artifact_name
        if not src.exists():
            missing_artifacts.append(artifact_name)
            continue

        dest = DIST_DIR / subdir
        dest.mkdir(parents=True, exist_ok=True)

        for source in src.iterdir():
            target = dest / source.name
            if source.is_dir():
                shutil.copytree(source, target, dirs_exist_ok=True)
                for nested in target.rglob("*"):
                    if nested.is_file() and not nested.name.endswith(".exe"):
                        nested.chmod(0o755)
                continue

            shutil.copy2(source, target)
            if not source.name.endswith(".exe"):
                target.chmod(0o755)

    if missing_artifacts:
        missing_text = ", ".join(missing_artifacts)
        raise SystemExit(f"ERROR: missing artifacts for {missing_text}")

    missing_files: list[str] = []
    for subdir, filenames in REQUIRED_PLATFORM_FILES.items():
        base_dir = DIST_DIR / subdir
        for filename in filenames:
            if not (base_dir / filename).is_file():
                missing_files.append(f"{subdir}/{filename}")
        for relpath in REQUIRED_NATIVE_FILES[subdir]:
            if not (base_dir / relpath).is_file():
                missing_files.append(f"{subdir}/{relpath}")

    if missing_files:
        preview = ", ".join(missing_files[:8])
        suffix = "" if len(missing_files) <= 8 else ", ..."
        raise SystemExit(f"ERROR: staged artifacts are incomplete: {preview}{suffix}")


def build_wheel(
    name: str,
    version: str,
    summary: str,
    requires_python: str,
    readme: str,
    platform_subdir: str,
    plat_tags: list[str],
) -> Path | None:
    bin_dir = DIST_DIR / platform_subdir
    if not bin_dir.exists():
        return None

    binaries = sorted(path for path in bin_dir.iterdir() if path.is_file())
    native_root = bin_dir / "python"
    if not binaries:
        return None

    name_norm = name.replace("-", "_")
    tag_plat = ".".join(plat_tags)
    wheel_filename = f"{name_norm}-{version}-py3-none-{tag_plat}.whl"
    data_dir = f"{name_norm}-{version}.data"
    dist_info = f"{name_norm}-{version}.dist-info"

    metadata = (
        "Metadata-Version: 2.1\n"
        f"Name: {name}\n"
        f"Version: {version}\n"
        f"Summary: {summary}\n"
        f"Requires-Python: {requires_python}\n"
    )
    if readme:
        metadata += f"Description-Content-Type: text/markdown\n\n{readme}\n"

    wheel_meta = "Wheel-Version: 1.0\nGenerator: zccache-release-workflow\nRoot-Is-Purelib: false\n"
    for plat_tag in plat_tags:
        wheel_meta += f"Tag: py3-none-{plat_tag}\n"

    exec_perms = (
        stat.S_IFREG
        | stat.S_IRUSR
        | stat.S_IWUSR
        | stat.S_IXUSR
        | stat.S_IRGRP
        | stat.S_IXGRP
        | stat.S_IROTH
        | stat.S_IXOTH
    )

    WHEEL_DIR.mkdir(parents=True, exist_ok=True)
    wheel_path = WHEEL_DIR / wheel_filename
    record_rows: list[tuple[str, str, int]] = []

    with zipfile.ZipFile(wheel_path, "w", zipfile.ZIP_DEFLATED) as whl:
        for binary in binaries:
            data = binary.read_bytes()
            arcname = f"{data_dir}/scripts/{binary.name}"
            info = zipfile.ZipInfo(arcname)
            info.create_system = 3
            info.external_attr = exec_perms << 16
            info.compress_type = zipfile.ZIP_DEFLATED
            whl.writestr(info, data)
            record_rows.append((arcname, record_hash(data), len(data)))

        source_root = ROOT / "python"
        for source in sorted(source_root.rglob("*")):
            if not source.is_file():
                continue
            rel = source.relative_to(source_root).as_posix()
            data = source.read_bytes()
            whl.writestr(rel, data)
            record_rows.append((rel, record_hash(data), len(data)))

        if native_root.exists():
            for native in sorted(native_root.rglob("*")):
                if not native.is_file():
                    continue
                rel = native.relative_to(native_root).as_posix()
                data = native.read_bytes()
                whl.writestr(rel, data)
                record_rows.append((rel, record_hash(data), len(data)))

        metadata_bytes = metadata.encode()
        whl.writestr(f"{dist_info}/METADATA", metadata_bytes)
        record_rows.append(
            (f"{dist_info}/METADATA", record_hash(metadata_bytes), len(metadata_bytes))
        )

        wheel_bytes = wheel_meta.encode()
        whl.writestr(f"{dist_info}/WHEEL", wheel_bytes)
        record_rows.append((f"{dist_info}/WHEEL", record_hash(wheel_bytes), len(wheel_bytes)))

        buf = io.StringIO()
        writer = csv.writer(buf, lineterminator="\n")
        for row in record_rows:
            writer.writerow(row)
        writer.writerow((f"{dist_info}/RECORD", "", ""))
        whl.writestr(f"{dist_info}/RECORD", buf.getvalue().encode())

    return wheel_path


def build_all_wheels(
    name: str,
    version: str,
    summary: str,
    requires_python: str,
    readme: str,
) -> list[Path]:
    if WHEEL_DIR.exists():
        shutil.rmtree(WHEEL_DIR)

    wheels: list[Path] = []
    missing_platforms: list[str] = []
    for subdir, plat_tags in PLATFORMS.items():
        wheel = build_wheel(name, version, summary, requires_python, readme, subdir, plat_tags)
        if wheel is None:
            missing_platforms.append(subdir)
        else:
            wheels.append(wheel)

    if missing_platforms:
        raise SystemExit(
            "ERROR: missing wheels for platform(s): " + ", ".join(sorted(missing_platforms))
        )
    if not wheels:
        raise SystemExit("ERROR: no wheels were built")
    return wheels


def verify_rust_crates_locally() -> None:
    try:
        validate_release_metadata()
        stamped = stamp_internal_dependency_versions()
    except ReleaseCheckError as e:
        raise SystemExit(f"ERROR: {e}") from e

    if stamped:
        log(
            "  Stamped exact internal dependency versions for crates.io publish: "
            + ", ".join(stamped)
        )

    for crate in RUST_PUBLISH_ORDER:
        run(["cargo", "package", "--allow-dirty", "--no-verify", "-p", crate, "--list"], cwd=ROOT)


def verify_crate_visible(crate: str, version: str) -> None:
    url = f"https://crates.io/api/v1/crates/{crate}/{version}"
    timeout = 300
    interval = 10
    start = time.time()

    while time.time() - start < timeout:
        try:
            with urllib.request.urlopen(url, timeout=10) as resp:
                data = json.loads(resp.read())
            crate_data = data.get("version") or {}
            if crate_data.get("num") == version:
                return
        except (urllib.error.HTTPError, urllib.error.URLError, TimeoutError):
            pass
        time.sleep(interval)

    raise SystemExit(f"ERROR: timed out waiting for {crate} {version} on crates.io")


def publish_rust_crates(version: str, existing_crates: set[str] | None = None) -> None:
    verify_rust_crates_locally()
    existing_crates = existing_crates or set()
    for crate in RUST_PUBLISH_ORDER:
        if crate in existing_crates:
            log(f"  Skipping {crate} {version}; already published")
            continue
        run(["cargo", "publish", "--allow-dirty", "--no-verify", "-p", crate], cwd=ROOT)
        verify_crate_visible(crate, version)


def command_check_registries(_: argparse.Namespace) -> None:
    name, version, *_ = read_project_meta()
    check_pypi_version(name, version)
    check_crates_versions(version)


def command_build_wheels(args: argparse.Namespace) -> None:
    artifacts_root = Path(args.artifact_download_dir).expanduser().resolve()
    if not artifacts_root.is_dir():
        raise SystemExit(f"ERROR: artifact directory does not exist: {artifacts_root}")

    name, version, summary, requires_python, readme = read_project_meta()
    organize_downloaded_artifacts(artifacts_root)
    build_all_wheels(name, version, summary, requires_python, readme)


def command_publish_crates(_: argparse.Namespace) -> None:
    if not os.environ.get("CARGO_REGISTRY_TOKEN"):
        raise SystemExit("ERROR: CARGO_REGISTRY_TOKEN is required for crates.io publish")
    version = read_workspace_version()
    existing_crates = check_crates_versions(version)
    publish_rust_crates(version, existing_crates)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Workflow-only release helpers for zccache."
    )
    subparsers = parser.add_subparsers(dest="command", required=True)

    check_parser = subparsers.add_parser(
        "check-registries",
        help="Fail fast if the current version is already fully published.",
    )
    check_parser.set_defaults(func=command_check_registries)

    wheels_parser = subparsers.add_parser(
        "build-wheels",
        help="Build PyPI wheels from downloaded binaries-* artifacts.",
    )
    wheels_parser.add_argument(
        "--artifact-download-dir",
        required=True,
        help="Directory containing downloaded binaries-* artifacts.",
    )
    wheels_parser.set_defaults(func=command_build_wheels)

    crates_parser = subparsers.add_parser(
        "publish-crates",
        help="Publish workspace crates in dependency order, skipping existing versions.",
    )
    crates_parser.set_defaults(func=command_publish_crates)

    return parser


def main() -> None:
    parser = build_parser()
    args = parser.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
