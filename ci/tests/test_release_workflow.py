from __future__ import annotations

import zipfile
from pathlib import Path

import pytest

from ci.release_workflow import assert_wheel_script_metadata


def _write_wheel(path: Path, *, create_system: int, mode: int) -> None:
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as whl:
        info = zipfile.ZipInfo("zccache-1.2.3.data/scripts/zccache")
        info.create_system = create_system
        info.external_attr = mode << 16
        info.compress_type = zipfile.ZIP_DEFLATED
        whl.writestr(info, b"#!/bin/sh\n")


def test_assert_wheel_script_metadata_accepts_executable_unix_entries(
    tmp_path: Path,
) -> None:
    wheel_path = tmp_path / "zccache-1.2.3-py3-none-manylinux_2_17_x86_64.whl"
    _write_wheel(wheel_path, create_system=3, mode=0o100755)

    assert_wheel_script_metadata(wheel_path)


def test_assert_wheel_script_metadata_rejects_bad_script_metadata(
    tmp_path: Path,
) -> None:
    wheel_path = tmp_path / "zccache-1.2.3-py3-none-manylinux_2_17_x86_64.whl"
    _write_wheel(wheel_path, create_system=0, mode=0o100644)

    with pytest.raises(
        SystemExit,
        match=(
            r"invalid wheel script metadata for "
            r"zccache-1\.2\.3-py3-none-manylinux_2_17_x86_64\.whl:"
            r"zccache-1\.2\.3\.data/scripts/zccache "
            r"\(create_system=0, mode=0o100644, "
            r"is_regular_file=True, has_exec_bit=False\)"
        ),
    ):
        assert_wheel_script_metadata(wheel_path)
