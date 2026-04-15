#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Build and publish zccache to PyPI and crates.io.

Zero-argument release pipeline:
  1. Pre-check: fail fast if version already exists on PyPI or crates.io
  2. Trigger GitHub Actions to build native binaries for all platforms
  3. Wait for builds to complete, download artifacts
  4. Assemble platform-specific wheels (native binaries, no Python runtime)
  5. Upload to PyPI
  6. Dry-run and publish Rust crates to crates.io in dependency order

Usage:
    ./publish                  # publish PyPI and crates.io
    ./publish --dry-run        # build and verify publishability without uploading
    ./publish --skip-pypi      # publish only Rust crates
    ./publish --skip-rust      # publish only PyPI wheels
"""

from __future__ import annotations

import argparse
import base64
import csv
import hashlib
import io
import json
import os
import platform
import re
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

SCRIPT_ROOT = Path(__file__).resolve().parent.parent
if str(SCRIPT_ROOT) not in sys.path:
    sys.path.insert(0, str(SCRIPT_ROOT))

from ci.env import activate, clean_env
from ci.release_checks import (
    RUST_PUBLISH_ORDER,
    ReleaseCheckError,
    validate_release_metadata,
)

ROOT = SCRIPT_ROOT
DIST_DIR = ROOT / "dist"
WHEEL_DIR = DIST_DIR / "wheels"
WORKFLOW_FILE = "build.yml"
# GitHub artifact name -> dist/ subdir
ARTIFACT_MAP: dict[str, str] = {
    "binaries-x86_64-unknown-linux-gnu": "linux-x86_64-gnu",
    "binaries-aarch64-unknown-linux-gnu": "linux-aarch64-gnu",
    "binaries-x86_64-apple-darwin": "macos-x86_64",
    "binaries-aarch64-apple-darwin": "macos-aarch64",
    "binaries-x86_64-pc-windows-msvc": "windows-x86_64",
    "binaries-aarch64-pc-windows-msvc": "windows-arm64",
}

# dist/ subdir -> wheel platform tags
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

IGNORED_PUBLISH_STATUS_PATTERNS = (
    "POST_HOOK_FAILURE_",
)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def log(msg: str) -> None:
    print(msg, file=sys.stderr, flush=True)


def run(cmd: list[str], **kwargs: Any) -> subprocess.CompletedProcess[Any]:
    log(f"  $ {' '.join(cmd)}")
    kwargs.setdefault("env", clean_env())
    return subprocess.run(cmd, check=True, **kwargs)


def run_capture(cmd: list[str]) -> str:
    result: subprocess.CompletedProcess[str] = run(cmd, capture_output=True, text=True)
    return result.stdout.strip()


def run_capture_retry(
    cmd: list[str],
    *,
    retries: int = 3,
    delay_seconds: float = 2.0,
) -> str:
    """Run a command and retry transient non-zero exits."""
    attempts = retries + 1
    for attempt in range(1, attempts + 1):
        try:
            return run_capture(cmd)
        except subprocess.CalledProcessError as e:
            if attempt >= attempts:
                raise
            log(
                f"  WARNING: command failed (attempt {attempt}/{attempts}): "
                f"{' '.join(cmd)}"
            )
            stdout = (e.stdout or "").strip()
            stderr = (e.stderr or "").strip()
            if stderr:
                log(f"    stderr: {stderr}")
            elif stdout:
                log(f"    stdout: {stdout}")
            time.sleep(delay_seconds)
    raise AssertionError("unreachable")


def configure_publish_env() -> None:
    """Use repo rustup proxies, but prefer the user's cargo credentials store."""
    repo_cargo_home = ROOT / ".cargo"
    current_cargo_home = Path(os.environ.get("CARGO_HOME", repo_cargo_home)).expanduser()
    home_cargo_home = Path.home() / ".cargo"

    current_has_credentials = any(
        (current_cargo_home / name).exists() for name in ("credentials.toml", "credentials")
    )
    home_has_credentials = any(
        (home_cargo_home / name).exists() for name in ("credentials.toml", "credentials")
    )

    if current_cargo_home == repo_cargo_home and not current_has_credentials and home_has_credentials:
        os.environ["CARGO_HOME"] = str(home_cargo_home)


def has_cargo_publish_token() -> bool:
    """Return True when cargo publish credentials appear to be configured."""
    if os.environ.get("CARGO_REGISTRY_TOKEN"):
        return True

    cargo_home = Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo"))
    for credentials_path in (cargo_home / "credentials.toml", cargo_home / "credentials"):
        if not credentials_path.exists():
            continue
        try:
            text = credentials_path.read_text(encoding="utf-8")
        except OSError:
            continue
        if "token" in text:
            return True
    return False


def get_publish_blocking_dirty_entries() -> list[str]:
    """Return git status entries that should block publishing."""
    result = subprocess.run(
        ["git", "status", "--porcelain"],
        capture_output=True,
        text=True,
        env=clean_env(),
        check=True,
    )
    entries = [line for line in result.stdout.splitlines() if line.strip()]
    blocking: list[str] = []
    for entry in entries:
        path = entry[3:] if len(entry) > 3 else entry
        if any(path.startswith(prefix) for prefix in IGNORED_PUBLISH_STATUS_PATTERNS):
            continue
        blocking.append(entry)
    return blocking


def read_workspace_version() -> str:
    """Read version from [workspace.package] in Cargo.toml (single source of truth)."""
    cargo_text = (ROOT / "Cargo.toml").read_text()
    match = re.search(r'\[workspace\.package\]\s*\nversion\s*=\s*"([^"]+)"', cargo_text)
    if not match:
        raise SystemExit("ERROR: Could not find [workspace.package] version in Cargo.toml")
    return match.group(1)


def read_project_meta() -> tuple[str, str, str, str, str]:
    """Return (name, version, summary, requires_python, readme) from pyproject.toml + Cargo.toml."""
    with open(ROOT / "pyproject.toml", "rb") as f:
        data = tomllib.load(f)
    proj = data["project"]
    readme = ""
    readme_field = proj.get("readme", "")
    if readme_field:
        readme_path = ROOT / (readme_field if isinstance(readme_field, str) else readme_field.get("file", ""))
        if readme_path.exists():
            readme = readme_path.read_text(encoding="utf-8")
    return (
        proj["name"],
        read_workspace_version(),
        proj.get("description", ""),
        proj.get("requires-python", ">=3.9"),
        readme,
    )


def download_failed_logs(repo: str, run_id: int) -> list[Path]:
    """Download logs for failed jobs, organized per target.

    Returns a list of log file paths that were saved.
    """
    log(f"\n==> Downloading logs for failed jobs in run {run_id}")

    logs_dir = DIST_DIR / "logs"
    logs_dir.mkdir(parents=True, exist_ok=True)

    # Identify which jobs failed
    try:
        jobs_raw = run_capture_retry([
            "gh", "run", "view", str(run_id),
            "--repo", repo,
            "--json", "jobs",
        ])
        jobs = json.loads(jobs_raw).get("jobs", [])
    except (subprocess.CalledProcessError, json.JSONDecodeError):
        jobs = []

    failed_jobs: dict[str, str] = {}  # job name -> target triple
    for job in jobs:
        if job.get("conclusion") == "failure":
            name = job.get("name", "")
            # Extract target triple from job name like "Build (x86_64-unknown-linux-musl)"
            m = re.search(r"\(([^)]+)\)", name)
            target = m.group(1) if m else name
            failed_jobs[name] = target

    if not failed_jobs:
        log("  No failed jobs found in run metadata.")
        return []

    log(f"  Failed targets: {', '.join(failed_jobs.values())}")

    # Download the failed logs (tab-delimited: job\tstep\tlog_line)
    try:
        result = subprocess.run(
            ["gh", "run", "view", str(run_id), "--repo", repo, "--log-failed"],
            capture_output=True, text=True, timeout=120,
        )
        raw_output = result.stdout or result.stderr or ""
    except (subprocess.TimeoutExpired, FileNotFoundError) as e:
        log(f"  WARNING: Could not download logs: {e}")
        return []

    # Group log lines by job name
    per_job: dict[str, list[str]] = {name: [] for name in failed_jobs}
    for line in raw_output.splitlines():
        parts = line.split("\t", 2)
        if len(parts) >= 2:
            job_name = parts[0]
            log_line = parts[2] if len(parts) == 3 else parts[1]
            if job_name in per_job:
                per_job[job_name].append(log_line)
            else:
                # Fuzzy match — gh sometimes abbreviates job names
                for known in per_job:
                    if known.startswith(job_name) or job_name.startswith(known):
                        per_job[known].append(log_line)
                        break

    # Save per-target log files and display
    saved: list[Path] = []
    preview_lines = 30
    for job_name, target in failed_jobs.items():
        lines = per_job.get(job_name, [])
        log_file = logs_dir / f"failed-{target}-{run_id}.log"
        log_file.write_text("\n".join(lines) + "\n" if lines else "(no log output)\n", encoding="utf-8")
        saved.append(log_file)

        log(f"\n  --- {target} ({len(lines)} lines) ---")
        log(f"  Log: {log_file}")
        if len(lines) > preview_lines:
            log(f"  ... (showing last {preview_lines} of {len(lines)} lines)")
            for l in lines[-preview_lines:]:
                log(f"  | {l}")
        else:
            for l in lines:
                log(f"  | {l}")

    return saved


def detect_repo() -> str:
    """Detect owner/repo from git remote origin."""
    url = run_capture(["git", "remote", "get-url", "origin"])
    if url.startswith("git@"):
        url = url.split(":", 1)[1]
    elif "github.com" in url:
        url = url.split("github.com/", 1)[1]
    return url.removesuffix(".git")


def record_hash(data: bytes) -> str:
    digest = hashlib.sha256(data).digest()
    return "sha256=" + base64.urlsafe_b64encode(digest).rstrip(b"=").decode()


def write_dist_manifest(repo: str, run_id: int | None, head_sha: str, version: str) -> None:
    manifest = {
        "repo": repo,
        "run_id": run_id,
        "head_sha": head_sha,
        "version": version,
        "platforms": {
            subdir: {
                "wheel_plats": PLATFORMS[subdir],
                "files": sorted(
                    str(path.relative_to(DIST_DIR)).replace("\\", "/")
                    for path in (DIST_DIR / subdir).rglob("*")
                    if path.is_file()
                ),
            }
            for subdir in PLATFORMS
            if (DIST_DIR / subdir).exists()
        },
    }
    manifest_path = DIST_DIR / "manifest.json"
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")


def read_dist_manifest() -> dict[str, Any] | None:
    manifest_path = DIST_DIR / "manifest.json"
    if not manifest_path.exists():
        return None
    try:
        return json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None


def missing_local_dist_files() -> list[str]:
    missing: list[str] = []
    for subdir, filenames in REQUIRED_PLATFORM_FILES.items():
        base_dir = DIST_DIR / subdir
        for filename in filenames:
            candidate = base_dir / filename
            if not candidate.is_file() or candidate.stat().st_size == 0:
                missing.append(str(candidate.relative_to(DIST_DIR)).replace("\\", "/"))
        for relpath in REQUIRED_NATIVE_FILES[subdir]:
            candidate = DIST_DIR / subdir / relpath
            if not candidate.is_file() or candidate.stat().st_size == 0:
                missing.append(str(candidate.relative_to(DIST_DIR)).replace("\\", "/"))
    return missing


def detect_host_platform_subdir() -> str | None:
    machine = platform.machine().lower()
    if sys.platform == "win32":
        if machine in {"amd64", "x86_64"}:
            return "windows-x86_64"
        if machine in {"arm64", "aarch64"}:
            return "windows-arm64"
        return None
    if sys.platform == "darwin":
        if machine in {"x86_64", "amd64"}:
            return "macos-x86_64"
        if machine in {"arm64", "aarch64"}:
            return "macos-aarch64"
        return None
    if sys.platform.startswith("linux"):
        if machine in {"x86_64", "amd64"}:
            return "linux-x86_64-gnu"
        if machine in {"arm64", "aarch64"}:
            return "linux-aarch64-gnu"
    return None


def host_binaries_match_version(expected_version: str) -> bool:
    host_subdir = detect_host_platform_subdir()
    if host_subdir is None:
        return False

    bin_dir = DIST_DIR / host_subdir
    required = REQUIRED_PLATFORM_FILES.get(host_subdir, ())
    if not required:
        return False

    for filename in required:
        binary = bin_dir / filename
        if not binary.is_file() or binary.stat().st_size == 0:
            return False
        try:
            output = run_capture([str(binary), "--version"])
        except (FileNotFoundError, subprocess.CalledProcessError, OSError):
            return False
        if not output.endswith(f" {expected_version}"):
            return False
    return True


def should_reuse_local_dist(repo: str, head_sha: str, version: str) -> bool:
    if not DIST_DIR.exists():
        return False

    missing = missing_local_dist_files()
    if missing:
        preview = ", ".join(missing[:6])
        if len(missing) > 6:
            preview += ", ..."
        log(f"  Local dist/ is incomplete: {preview}")
        return False

    manifest = read_dist_manifest()
    if manifest is not None:
        manifest_repo = manifest.get("repo")
        manifest_sha = manifest.get("head_sha")
        manifest_version = manifest.get("version")
        if manifest_repo == repo and manifest_sha == head_sha and manifest_version == version:
            log("  Reusing local dist/ (manifest matches repo, commit, and version)")
            return True
        log(
            "  Local dist/ manifest does not match current release "
            f"(repo={manifest_repo!r}, sha={manifest_sha!r}, version={manifest_version!r})"
        )

    if host_binaries_match_version(version):
        log("  Reusing local dist/ (host binaries report the expected version)")
        return True

    log("  Local dist/ could not be validated for this release")
    return False


def list_run_artifacts(repo: str, run_id: int) -> list[str]:
    """Return artifact names produced by a workflow run."""
    try:
        raw = run_capture_retry([
            "gh",
            "api",
            f"repos/{repo}/actions/runs/{run_id}/artifacts",
        ])
        data = json.loads(raw)
    except (subprocess.CalledProcessError, json.JSONDecodeError):
        return []
    return [artifact.get("name", "") for artifact in data.get("artifacts", [])]


def find_existing_build_run(repo: str, head_sha: str) -> int | None:
    """Return a successful build workflow run for this commit if artifacts are complete."""
    try:
        raw = run_capture_retry([
            "gh",
            "run",
            "list",
            "--repo",
            repo,
            "--workflow",
            WORKFLOW_FILE,
            "--commit",
            head_sha,
            "--limit",
            "20",
            "--json",
            "databaseId,status,conclusion,headSha",
        ])
        runs = json.loads(raw)
    except (subprocess.CalledProcessError, json.JSONDecodeError):
        return None

    expected = set(ARTIFACT_MAP)
    for run_info in runs:
        if run_info.get("headSha") != head_sha:
            continue
        if run_info.get("status") != "completed" or run_info.get("conclusion") != "success":
            continue
        run_id = run_info["databaseId"]
        artifacts = set(list_run_artifacts(repo, run_id))
        if expected.issubset(artifacts):
            log(f"  Reusing successful build run {run_id} for {head_sha[:12]}")
            return run_id
        missing = sorted(expected - artifacts)
        log(f"  Run {run_id} is missing artifacts: {', '.join(missing)}")

    return None


# ---------------------------------------------------------------------------
# Step 1: PyPI version pre-check
# ---------------------------------------------------------------------------


def get_pypi_release_filenames(name: str, version: str) -> set[str] | None:
    """Return PyPI filenames for an existing version, or None if the version is absent."""
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
    """Return the full wheel set this project publishes to PyPI."""
    name_norm = name.replace("-", "_")
    return {
        f"{name_norm}-{version}-py3-none-{'.'.join(plat_tags)}.whl"
        for plat_tags in PLATFORMS.values()
    }


def check_pypi_version(name: str, version: str) -> set[str]:
    """Report existing PyPI state and return any already-published filenames."""
    log(f"\n=== Step 1: Pre-check PyPI for {name} {version} ===")
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
                    f"  ERROR: Current version {version} already matches the latest PyPI release "
                    "and all expected wheels are present."
                )
                log("  Bump the package version before running the full publish flow again.")
                log("  Use --skip-pypi if you only need to continue the crates.io release.")
                sys.exit(1)
            log(
                f"  {name} {version} already exists on PyPI with {len(release_files)} "
                "file(s); missing files will be uploaded"
            )
            return release_files
        log(f"  {name} {version} is available (existing: {', '.join(sorted(existing)) or 'none'})")
        return set()
    except urllib.error.HTTPError as e:
        if e.code == 404:
            log(f"  {name} not yet on PyPI (first publish)")
            return set()
        else:
            log(f"  WARNING: PyPI check failed (HTTP {e.code}), continuing anyway")
            return set()
    except (urllib.error.URLError, TimeoutError):
        log("  WARNING: Could not reach PyPI, continuing anyway")
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
    log(f"\n=== Step 1b: Pre-check crates.io for Rust crates {version} ===")
    existing: list[str] = []
    for crate in RUST_PUBLISH_ORDER:
        try:
            if crate_version_exists(crate, version):
                existing.append(crate)
                log(f"  EXISTS: {crate} {version}")
            else:
                log(f"  OK: {crate} {version} is available")
        except (urllib.error.URLError, TimeoutError) as e:
            log(f"  WARNING: Could not reach crates.io for {crate}: {e}")

    if existing:
        if len(existing) == len(RUST_PUBLISH_ORDER):
            log("  ERROR: These crates already exist on crates.io:")
            for crate in existing:
                log(f"    - {crate} {version}")
            log("  Bump the workspace version in Cargo.toml before publishing.")
            sys.exit(1)

        log("  Resuming partial crates.io release; already-published crates will be skipped:")
        for crate in existing:
            log(f"    - {crate} {version}")

    return set(existing)


# ---------------------------------------------------------------------------
# Step 2: Trigger GitHub Actions build
# ---------------------------------------------------------------------------


def trigger_and_wait(repo: str, version: str) -> int | None:
    """Reuse local dist/, or a successful build for HEAD, or trigger one and wait."""
    log(f"\n=== Step 2: Build native binaries ({repo}) ===")

    head_sha = run_capture(["git", "rev-parse", "HEAD"])
    branch = run_capture(["git", "rev-parse", "--abbrev-ref", "HEAD"])
    log(f"  Branch: {branch} ({head_sha[:12]})")

    if should_reuse_local_dist(repo, head_sha, version):
        return None

    existing_run = find_existing_build_run(repo, head_sha)
    if existing_run is not None:
        return existing_run

    # Snapshot existing runs to detect the new one
    existing_raw = run_capture_retry(
        [
            "gh",
            "run",
            "list",
            "--repo",
            repo,
            "--workflow",
            WORKFLOW_FILE,
            "--limit",
            "1",
            "--json",
            "databaseId",
        ]
    )
    existing_ids: set[int] = {r["databaseId"] for r in json.loads(existing_raw)} if existing_raw else set()

    # Trigger
    log(f"  Triggering {WORKFLOW_FILE} on {branch}...")
    run(["gh", "workflow", "run", WORKFLOW_FILE, "--repo", repo, "--ref", branch])

    # Wait for run to appear
    log("  Waiting for run to start...")
    run_id = None
    for _ in range(30):
        time.sleep(2)
        result = run_capture_retry(
            [
                "gh",
                "run",
                "list",
                "--repo",
                repo,
                "--workflow",
                WORKFLOW_FILE,
                "--limit",
                "5",
                "--json",
                "databaseId,status",
            ]
        )
        for r in json.loads(result):
            if r["databaseId"] not in existing_ids:
                run_id = r["databaseId"]
                break
        if run_id:
            break

    if not run_id:
        log("  ERROR: Timed out waiting for workflow run to appear.")
        sys.exit(1)

    log(f"  Run {run_id} started")
    log(f"  https://github.com/{repo}/actions/runs/{run_id}")

    # Wait for completion (30 min timeout)
    timeout = 1800
    start = time.time()
    while time.time() - start < timeout:
        result = run_capture_retry(
            [
                "gh",
                "run",
                "view",
                str(run_id),
                "--repo",
                repo,
                "--json",
                "status,conclusion",
            ]
        )
        data = json.loads(result)

        if data["status"] == "completed":
            if data.get("conclusion") == "success":
                elapsed = int(time.time() - start)
                log(f"  Build completed in {elapsed}s")
                return run_id
            log(f"  ERROR: Build failed: {data.get('conclusion')}")
            log(f"  https://github.com/{repo}/actions/runs/{run_id}")
            download_failed_logs(repo, run_id)
            sys.exit(1)

        elapsed = int(time.time() - start)
        log(f"  [{elapsed}s] {data['status']}...")
        time.sleep(15)

    log(f"  ERROR: Build timed out after {timeout}s")
    sys.exit(1)


# ---------------------------------------------------------------------------
# Step 3: Download artifacts
# ---------------------------------------------------------------------------


def download_artifacts(repo: str, run_id: int, head_sha: str, version: str) -> None:
    """Download build artifacts and organize into dist/."""
    log(f"\n=== Step 3: Download artifacts from run {run_id} ===")

    if DIST_DIR.exists():
        shutil.rmtree(DIST_DIR)
    DIST_DIR.mkdir()

    tmp = DIST_DIR / "_tmp"
    tmp.mkdir()
    run(["gh", "run", "download", str(run_id), "--repo", repo, "--dir", str(tmp)])

    found = 0
    missing: list[str] = []
    for artifact_name, subdir in ARTIFACT_MAP.items():
        src = tmp / artifact_name
        if not src.exists():
            log(f"  MISSING: {artifact_name}")
            missing.append(artifact_name)
            continue

        dest = DIST_DIR / subdir
        dest.mkdir(parents=True, exist_ok=True)

        for f in src.iterdir():
            target = dest / f.name
            if f.is_dir():
                shutil.copytree(f, target, dirs_exist_ok=True)
                for nested in target.rglob("*"):
                    if nested.is_file() and not nested.name.endswith(".exe"):
                        nested.chmod(0o755)
                log(f"  {subdir}/{f.name}/")
                continue

            shutil.copy2(f, target)
            if not f.name.endswith(".exe"):
                target.chmod(0o755)
            size_mb = target.stat().st_size / (1024 * 1024)
            log(f"  {subdir}/{f.name} ({size_mb:.1f} MB)")

        found += 1

    shutil.rmtree(tmp)
    log(f"  {found}/{len(ARTIFACT_MAP)} platforms downloaded")

    if missing:
        log(f"  ERROR: Missing artifacts for: {', '.join(missing)}")
        log("  All platforms must build successfully before publishing.")
        sys.exit(1)

    write_dist_manifest(repo, run_id, head_sha, version)


# ---------------------------------------------------------------------------
# Step 4: Build wheels
# ---------------------------------------------------------------------------


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

    metadata = f"Metadata-Version: 2.1\nName: {name}\nVersion: {version}\nSummary: {summary}\nRequires-Python: {requires_python}\n"
    if readme:
        metadata += f"Description-Content-Type: text/markdown\n\n{readme}\n"

    wheel_meta = "Wheel-Version: 1.0\nGenerator: zccache-publish\nRoot-Is-Purelib: false\n"
    for pt in plat_tags:
        wheel_meta += f"Tag: py3-none-{pt}\n"

    exec_perms = stat.S_IRUSR | stat.S_IWUSR | stat.S_IXUSR | stat.S_IRGRP | stat.S_IXGRP | stat.S_IROTH | stat.S_IXOTH

    WHEEL_DIR.mkdir(parents=True, exist_ok=True)
    wheel_path = WHEEL_DIR / wheel_filename
    record_rows: list[tuple[str, str, int]] = []

    with zipfile.ZipFile(wheel_path, "w", zipfile.ZIP_DEFLATED) as whl:
        for binary in binaries:
            data = binary.read_bytes()
            arcname = f"{data_dir}/scripts/{binary.name}"
            info = zipfile.ZipInfo(arcname)
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

        meta_bytes = metadata.encode()
        whl.writestr(f"{dist_info}/METADATA", meta_bytes)
        record_rows.append((f"{dist_info}/METADATA", record_hash(meta_bytes), len(meta_bytes)))

        wheel_bytes = wheel_meta.encode()
        whl.writestr(f"{dist_info}/WHEEL", wheel_bytes)
        record_rows.append((f"{dist_info}/WHEEL", record_hash(wheel_bytes), len(wheel_bytes)))

        buf = io.StringIO()
        writer = csv.writer(buf, lineterminator="\n")
        for row in record_rows:
            writer.writerow(row)
        writer.writerow((f"{dist_info}/RECORD", "", ""))
        whl.writestr(f"{dist_info}/RECORD", buf.getvalue().encode())

    size_mb = wheel_path.stat().st_size / (1024 * 1024)
    log(f"  {wheel_filename} ({size_mb:.1f} MB)")
    return wheel_path


def build_all_wheels(name: str, version: str, summary: str, requires_python: str, readme: str) -> list[Path]:
    log(f"\n=== Step 4: Build wheels ({name} {version}) ===")

    if WHEEL_DIR.exists():
        shutil.rmtree(WHEEL_DIR)

    wheels: list[Path] = []
    missing_platforms: list[str] = []
    for subdir, plat_tags in PLATFORMS.items():
        whl = build_wheel(name, version, summary, requires_python, readme, subdir, plat_tags)
        if whl:
            wheels.append(whl)
        else:
            missing_platforms.append(subdir)

    if missing_platforms:
        log(
            "  ERROR: Missing wheels for platform(s): "
            + ", ".join(sorted(missing_platforms))
        )
        log("  Refusing to publish a partial wheel set.")
        sys.exit(1)

    if not wheels:
        log("  ERROR: No wheels were built.")
        sys.exit(1)

    log(f"  {len(wheels)} wheel(s) ready")
    return wheels


# ---------------------------------------------------------------------------
# Step 5: Upload
# ---------------------------------------------------------------------------


def upload_wheels(wheels: list[Path], name: str, version: str) -> None:
    log("\n=== Step 5: Upload to PyPI ===")
    upload_cmd = ["uv", "publish"]
    upload_cmd.extend(str(w) for w in sorted(wheels))
    run(upload_cmd)
    log(f"\n  Published: https://pypi.org/project/{name}/{version}/")


# ---------------------------------------------------------------------------
# Step 6: Post-upload verification
# ---------------------------------------------------------------------------


def verify_pypi_wheels(name: str, version: str, expected_wheels: list[Path]) -> None:
    """Poll PyPI until all uploaded wheels are visible (CDN propagation)."""
    log("\n=== Step 6: Verify wheels on PyPI ===")

    expected_filenames = {w.name for w in expected_wheels}
    url = f"https://pypi.org/pypi/{name}/{version}/json"
    timeout = 300  # 5 minutes
    interval = 10
    start = time.time()
    available: set[str] = set()

    while time.time() - start < timeout:
        try:
            with urllib.request.urlopen(url, timeout=10) as resp:
                data = json.loads(resp.read())
            available = {f["filename"] for f in data.get("urls", [])}
            missing = expected_filenames - available
            if not missing:
                elapsed = int(time.time() - start)
                log(f"  All {len(expected_filenames)} wheels verified on PyPI ({elapsed}s)")
                return
            elapsed = int(time.time() - start)
            log(f"  [{elapsed}s] Waiting for {len(missing)} wheel(s): {', '.join(sorted(missing))}")
        except (urllib.error.HTTPError, urllib.error.URLError, TimeoutError) as e:
            elapsed = int(time.time() - start)
            log(f"  [{elapsed}s] PyPI check failed ({e}), retrying...")

        time.sleep(interval)

    log(f"  ERROR: After {timeout}s, these wheels are still not visible on PyPI:")
    for f in sorted(expected_filenames - available):
        log(f"    - {f}")
    log("  The upload may have partially failed or CDN propagation is unusually slow.")
    log(f"  Check https://pypi.org/project/{name}/{version}/#files manually.")
    sys.exit(1)


def verify_rust_crates_locally() -> None:
    log("\n=== Step 7: Verify Rust crates locally ===")
    try:
        validate_release_metadata()
    except ReleaseCheckError as e:
        log(f"  ERROR: {e}")
        sys.exit(1)
    # Skip local cargo check — CI build matrix is the compile/lint/test gate.
    # If the GH Actions build produced binaries, the code compiles.

    log("\n=== Step 7b: Verify Rust crate packaging ===")
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
                elapsed = int(time.time() - start)
                log(f"  Verified {crate} {version} on crates.io ({elapsed}s)")
                return
        except (urllib.error.HTTPError, urllib.error.URLError, TimeoutError) as e:
            elapsed = int(time.time() - start)
            log(f"  [{elapsed}s] Waiting for {crate} {version} on crates.io ({e})")
            time.sleep(interval)
            continue

        elapsed = int(time.time() - start)
        log(f"  [{elapsed}s] Waiting for {crate} {version} on crates.io")
        time.sleep(interval)

    log(f"  ERROR: Timed out waiting for {crate} {version} to appear on crates.io")
    log(f"  Check https://crates.io/crates/{crate}")
    sys.exit(1)


def publish_rust_crates(version: str, dry_run: bool, existing_crates: set[str] | None = None) -> None:
    existing_crates = existing_crates or set()
    if dry_run:
        verify_rust_crates_locally()
        log("\n=== Step 8: Upload Rust crates (skipped - dry run) ===")
        for crate in RUST_PUBLISH_ORDER:
            if crate not in existing_crates:
                log(f"  {crate} {version}")
        return

    verify_rust_crates_locally()
    log("\n=== Step 8: Publish Rust crates to crates.io ===")
    for crate in RUST_PUBLISH_ORDER:
        if crate in existing_crates:
            log(f"  Skipping {crate} {version}; already on crates.io")
            continue
        run(["cargo", "publish", "--allow-dirty", "--no-verify", "-p", crate], cwd=ROOT)
        verify_crate_visible(crate, version)


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------


def main() -> None:
    activate()
    configure_publish_env()
    parser = argparse.ArgumentParser(description="Build and publish zccache to PyPI and crates.io")
    parser.add_argument("--dry-run", action="store_true", help="Build wheels but do not upload.")
    parser.add_argument("--skip-pypi", action="store_true", help="Skip the PyPI release flow.")
    parser.add_argument("--skip-rust", action="store_true", help="Skip the crates.io release flow.")
    args = parser.parse_args()

    run_pypi = not args.skip_pypi
    run_rust = not args.skip_rust
    if not run_pypi and not run_rust:
        log("ERROR: Nothing to do. Remove one of --skip-pypi / --skip-rust.")
        sys.exit(1)

    if run_pypi:
        try:
            run_capture(["gh", "--version"])
        except FileNotFoundError:
            log("ERROR: 'gh' (GitHub CLI) is not installed.")
            sys.exit(1)

    try:
        validate_release_metadata()
    except ReleaseCheckError as e:
        log(f"ERROR: {e}")
        sys.exit(1)

    name, version, summary, requires_python, readme = read_project_meta()

    repo = detect_repo() if run_pypi else ""
    targets: list[str] = []
    existing_pypi_files: set[str] = set()
    if run_pypi:
        targets.append("PyPI")
    if run_rust:
        targets.append("crates.io")
    log(f"Publishing {name} {version} to {', '.join(targets)}")

    existing_rust_crates: set[str] = set()
    if not args.dry_run:
        if run_pypi:
            existing_pypi_files = check_pypi_version(name, version)

        if run_rust and not has_cargo_publish_token():
            log(
                "ERROR: crates.io publish requested but no cargo registry token was found. "
                "Set CARGO_REGISTRY_TOKEN or run `cargo login` before publishing."
            )
            sys.exit(1)

        if run_pypi:
            # GH Actions builds from the remote branch, so local-only changes
            # produce binaries with stale version strings baked in.
            dirty_entries = get_publish_blocking_dirty_entries()
            dirty = "\n".join(dirty_entries)
            if dirty:
                log(f"ERROR: Working tree is dirty. Commit and push before publishing PyPI artifacts.\n{dirty}")
                sys.exit(1)

            local_sha = run_capture(["git", "rev-parse", "HEAD"])
            remote_sha = run_capture(["git", "rev-parse", "@{u}"])
            if local_sha != remote_sha:
                log(f"ERROR: Local HEAD ({local_sha[:12]}) differs from remote ({remote_sha[:12]}). Push before publishing PyPI artifacts.")
                sys.exit(1)

        if run_rust:
            existing_rust_crates = check_crates_versions(version)

    if run_pypi:
        head_sha = run_capture(["git", "rev-parse", "HEAD"])
        run_id = trigger_and_wait(repo, version)
        if run_id is not None:
            download_artifacts(repo, run_id, head_sha, version)
        else:
            write_dist_manifest(repo, None, head_sha, version)
        wheels = build_all_wheels(name, version, summary, requires_python, readme)
        wheels_to_upload = [wheel for wheel in wheels if wheel.name not in existing_pypi_files]

        # Step 5: Upload
        if args.dry_run:
            log("\n=== Step 5: Upload (skipped - dry run) ===")
            for w in wheels:
                log(f"  {w.name}")
        else:
            if wheels_to_upload:
                log(f"  Uploading {len(wheels_to_upload)}/{len(wheels)} wheel(s) missing from PyPI")
                upload_wheels(wheels_to_upload, name, version)
            else:
                log("\n=== Step 5: Upload ===")
                log("  All expected wheels are already present on PyPI")
            # Step 6: Verify all wheels are visible on PyPI (CDN propagation)
            verify_pypi_wheels(name, version, wheels)

    if run_rust:
        publish_rust_crates(version, args.dry_run, existing_rust_crates)

    log("\n=== Done ===")


if __name__ == "__main__":
    main()
