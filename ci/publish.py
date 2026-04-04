#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Build and publish zccache to PyPI.

Zero-argument release pipeline:
  1. Pre-check: fail fast if version already exists on PyPI
  2. Trigger GitHub Actions to build native binaries for all platforms
  3. Wait for builds to complete, download artifacts
  4. Assemble platform-specific wheels (native binaries, no Python runtime)
  5. Upload to PyPI

Usage:
    ./publish              # full pipeline
    ./publish --dry-run    # everything except upload
"""

from __future__ import annotations

import argparse
import base64
import csv
import hashlib
import io
import json
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

ROOT = Path(__file__).resolve().parent.parent
DIST_DIR = ROOT / "dist"
WHEEL_DIR = DIST_DIR / "wheels"
WORKFLOW_FILE = "build.yml"

# GitHub artifact name -> dist/ subdir
ARTIFACT_MAP: dict[str, str] = {
    "binaries-x86_64-unknown-linux-musl": "linux-x86_64",
    "binaries-aarch64-unknown-linux-musl": "linux-aarch64",
    "binaries-x86_64-unknown-linux-gnu": "linux-x86_64-gnu",
    "binaries-aarch64-unknown-linux-gnu": "linux-aarch64-gnu",
    "binaries-x86_64-apple-darwin": "macos-x86_64",
    "binaries-aarch64-apple-darwin": "macos-aarch64",
    "binaries-x86_64-pc-windows-msvc": "windows-x86_64",
    "binaries-aarch64-pc-windows-msvc": "windows-arm64",
}

# dist/ subdir -> wheel platform tags
PLATFORMS: dict[str, list[str]] = {
    "linux-x86_64": ["musllinux_1_2_x86_64"],
    "linux-aarch64": ["musllinux_1_2_aarch64"],
    "linux-x86_64-gnu": ["manylinux_2_17_x86_64"],
    "linux-aarch64-gnu": ["manylinux_2_17_aarch64"],
    "macos-x86_64": ["macosx_10_12_x86_64"],
    "macos-aarch64": ["macosx_11_0_arm64"],
    "windows-x86_64": ["win_amd64"],
    "windows-arm64": ["win_arm64"],
}


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def log(msg: str) -> None:
    print(msg, file=sys.stderr, flush=True)


def run(cmd: list[str], **kwargs: Any) -> subprocess.CompletedProcess[Any]:
    log(f"  $ {' '.join(cmd)}")
    return subprocess.run(cmd, check=True, **kwargs)


def run_capture(cmd: list[str]) -> str:
    result: subprocess.CompletedProcess[str] = run(cmd, capture_output=True, text=True)
    return result.stdout.strip()


def read_project_meta() -> tuple[str, str, str, str, str]:
    """Return (name, version, summary, requires_python, readme) from pyproject.toml."""
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
        proj["version"],
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
        jobs_raw = run_capture([
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


# ---------------------------------------------------------------------------
# Step 1: PyPI version pre-check
# ---------------------------------------------------------------------------


def check_pypi_version(name: str, version: str) -> None:
    """Fail fast if this version already exists on PyPI."""
    log(f"\n=== Step 1: Pre-check PyPI for {name} {version} ===")
    url = f"https://pypi.org/pypi/{name}/json"
    try:
        with urllib.request.urlopen(url, timeout=10) as resp:
            data = json.loads(resp.read())
        existing = set(data.get("releases", {}).keys())
        if version in existing:
            log(f"  ERROR: {name} {version} already exists on PyPI.")
            log("  Bump the version in pyproject.toml before publishing.")
            sys.exit(1)
        log(f"  {name} {version} is available (existing: {', '.join(sorted(existing)) or 'none'})")
    except urllib.error.HTTPError as e:
        if e.code == 404:
            log(f"  {name} not yet on PyPI (first publish)")
        else:
            log(f"  WARNING: PyPI check failed (HTTP {e.code}), continuing anyway")
    except (urllib.error.URLError, TimeoutError):
        log("  WARNING: Could not reach PyPI, continuing anyway")


# ---------------------------------------------------------------------------
# Step 2: Trigger GitHub Actions build
# ---------------------------------------------------------------------------


def trigger_and_wait(repo: str) -> int:
    """Trigger build workflow on HEAD, wait for completion, return run ID."""
    log(f"\n=== Step 2: Build native binaries ({repo}) ===")

    head_sha = run_capture(["git", "rev-parse", "HEAD"])
    branch = run_capture(["git", "rev-parse", "--abbrev-ref", "HEAD"])
    log(f"  Branch: {branch} ({head_sha[:12]})")

    # Snapshot existing runs to detect the new one
    existing_raw = run_capture(
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
        result = run_capture(
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
        result = run_capture(
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


def download_artifacts(repo: str, run_id: int) -> None:
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

    binaries = sorted(bin_dir.iterdir())
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
    for subdir, plat_tags in PLATFORMS.items():
        whl = build_wheel(name, version, summary, requires_python, readme, subdir, plat_tags)
        if whl:
            wheels.append(whl)

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


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------


def main() -> None:
    parser = argparse.ArgumentParser(description="Build and publish zccache to PyPI")
    parser.add_argument("--dry-run", action="store_true", help="Build wheels but do not upload.")
    args = parser.parse_args()

    # Verify prerequisites
    try:
        run_capture(["gh", "--version"])
    except FileNotFoundError:
        log("ERROR: 'gh' (GitHub CLI) is not installed.")
        sys.exit(1)

    name, version, summary, requires_python, readme = read_project_meta()
    repo = detect_repo()
    log(f"Publishing {name} {version} from {repo}")

    if not args.dry_run:
        # Step 0: Fail fast if repo has uncommitted or unpushed changes.
        # GH Actions builds from the remote branch, so local-only changes
        # produce binaries with stale version strings baked in.
        dirty = subprocess.run(
            ["git", "status", "--porcelain"],
            capture_output=True,
            text=True,
        ).stdout.strip()
        if dirty:
            log(f"ERROR: Working tree is dirty. Commit and push before publishing.\n{dirty}")
            sys.exit(1)

        local_sha = run_capture(["git", "rev-parse", "HEAD"])
        remote_sha = run_capture(["git", "rev-parse", "@{u}"])
        if local_sha != remote_sha:
            log(f"ERROR: Local HEAD ({local_sha[:12]}) differs from remote ({remote_sha[:12]}). Push before publishing.")
            sys.exit(1)

        # Step 1: Fail fast if version exists
        check_pypi_version(name, version)

    # Step 2: Build native binaries on all platforms
    run_id = trigger_and_wait(repo)

    # Step 3: Download artifacts
    download_artifacts(repo, run_id)

    # Step 4: Build platform wheels
    wheels = build_all_wheels(name, version, summary, requires_python, readme)

    # Step 5: Upload
    if args.dry_run:
        log("\n=== Step 5: Upload (skipped — dry run) ===")
        for w in wheels:
            log(f"  {w.name}")
    else:
        upload_wheels(wheels, name, version)
        # Step 6: Verify all wheels are visible on PyPI (CDN propagation)
        verify_pypi_wheels(name, version, wheels)

    log("\n=== Done ===")


if __name__ == "__main__":
    main()
