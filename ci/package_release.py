#!/usr/bin/env python3
"""Package standalone release archives for GitHub Releases."""

from __future__ import annotations

import argparse
import shutil
import tarfile
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
INCLUDE = ("zccache", "zccache-daemon", "zccache-fp")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True, help="Release version, e.g. 1.2.3 or v1.2.3")
    parser.add_argument("--target", required=True, help="Rust target triple")
    parser.add_argument("--binary-ext", default="", help="Executable suffix, e.g. .exe")
    parser.add_argument("--input-dir", type=Path, required=True, help="Directory containing built binaries")
    parser.add_argument("--output-dir", type=Path, required=True, help="Directory to write archives into")
    return parser.parse_args()


def normalize_version(version: str) -> str:
    return version if version.startswith("v") else f"v{version}"


def stage_tree(version: str, target: str, binary_ext: str, input_dir: Path, output_dir: Path) -> tuple[Path, Path]:
    tag = normalize_version(version)
    root_name = f"zccache-{tag}-{target}"
    stage_dir = output_dir / root_name
    if stage_dir.exists():
        shutil.rmtree(stage_dir)
    stage_dir.mkdir(parents=True)

    for name in INCLUDE:
        source = input_dir / f"{name}{binary_ext}"
        if not source.exists():
            raise FileNotFoundError(source)
        shutil.copy2(source, stage_dir / source.name)

    readme = ROOT / "README.md"
    if readme.exists():
        shutil.copy2(readme, stage_dir / "README.md")

    return stage_dir, output_dir / root_name


def write_tarball(stage_dir: Path, archive_base: Path) -> Path:
    archive_path = archive_base.parent / f"{archive_base.name}.tar.gz"
    if archive_path.exists():
        archive_path.unlink()
    with tarfile.open(archive_path, "w:gz") as tar:
        tar.add(stage_dir, arcname=stage_dir.name)
    return archive_path


def write_zip(stage_dir: Path, archive_base: Path) -> Path:
    archive_path = archive_base.parent / f"{archive_base.name}.zip"
    if archive_path.exists():
        archive_path.unlink()
    with zipfile.ZipFile(archive_path, "w", compression=zipfile.ZIP_DEFLATED) as zf:
        for path in sorted(stage_dir.rglob("*")):
            zf.write(path, arcname=path.relative_to(stage_dir.parent))
    return archive_path


def main() -> None:
    args = parse_args()
    output_dir = args.output_dir.resolve()
    output_dir.mkdir(parents=True, exist_ok=True)

    stage_dir, archive_base = stage_tree(
        version=args.version,
        target=args.target,
        binary_ext=args.binary_ext,
        input_dir=args.input_dir.resolve(),
        output_dir=output_dir,
    )

    if args.binary_ext == ".exe":
        archive = write_zip(stage_dir, archive_base)
    else:
        archive = write_tarball(stage_dir, archive_base)

    print(archive)


if __name__ == "__main__":
    main()
