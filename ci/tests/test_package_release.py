from __future__ import annotations

import importlib.util
import tarfile
import zipfile
from pathlib import Path


def _load_package_release():
    module_path = Path(__file__).resolve().parents[1] / "package_release.py"
    spec = importlib.util.spec_from_file_location("package_release", module_path)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


package_release = _load_package_release()


def _write_fake_binary(path: Path) -> None:
    path.write_bytes(b"binary\n")


def test_write_tarball_preserves_full_version_and_target(tmp_path: Path) -> None:
    input_dir = tmp_path / "input"
    output_dir = tmp_path / "output"
    input_dir.mkdir()
    output_dir.mkdir()

    for name in package_release.INCLUDE:
        _write_fake_binary(input_dir / name)

    stage_dir, archive_base = package_release.stage_tree(
        version="1.3.10",
        target="x86_64-unknown-linux-musl",
        binary_ext="",
        input_dir=input_dir,
        output_dir=output_dir,
    )
    archive = package_release.write_tarball(stage_dir, archive_base)

    assert archive.name == "zccache-v1.3.10-x86_64-unknown-linux-musl.tar.gz"

    with tarfile.open(archive, "r:gz") as tf:
        assert tf.getmember("zccache-v1.3.10-x86_64-unknown-linux-musl/zccache")


def test_write_zip_preserves_full_version_and_target(tmp_path: Path) -> None:
    input_dir = tmp_path / "input"
    output_dir = tmp_path / "output"
    input_dir.mkdir()
    output_dir.mkdir()

    for name in package_release.INCLUDE:
        _write_fake_binary(input_dir / f"{name}.exe")

    stage_dir, archive_base = package_release.stage_tree(
        version="1.3.10",
        target="x86_64-pc-windows-msvc",
        binary_ext=".exe",
        input_dir=input_dir,
        output_dir=output_dir,
    )
    archive = package_release.write_zip(stage_dir, archive_base)

    assert archive.name == "zccache-v1.3.10-x86_64-pc-windows-msvc.zip"

    with zipfile.ZipFile(archive) as zf:
        assert "zccache-v1.3.10-x86_64-pc-windows-msvc/zccache.exe" in zf.namelist()
