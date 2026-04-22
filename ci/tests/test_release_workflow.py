from __future__ import annotations

import csv
import io
import os
import zipfile
from pathlib import Path

import pytest

from ci.release_workflow import (
    assert_installed_wheel_scripts_executable,
    assert_wheel_script_metadata,
)


def _write_wheel(
    path: Path,
    *,
    create_system: int,
    mode: int,
    include_dist_info: bool = False,
) -> None:
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as whl:
        info = zipfile.ZipInfo("zccache-1.2.3.data/scripts/zccache")
        info.create_system = create_system
        info.external_attr = mode << 16
        info.compress_type = zipfile.ZIP_DEFLATED
        whl.writestr(info, b"#!/bin/sh\n")
        if include_dist_info:
            metadata = b"Metadata-Version: 2.1\nName: zccache\nVersion: 1.2.3\n"
            wheel = (
                b"Wheel-Version: 1.0\n"
                b"Generator: zccache-test\n"
                b"Root-Is-Purelib: false\n"
                b"Tag: py3-none-any\n"
            )
            whl.writestr("zccache-1.2.3.dist-info/METADATA", metadata)
            whl.writestr("zccache-1.2.3.dist-info/WHEEL", wheel)
            record = io.StringIO()
            writer = csv.writer(record, lineterminator="\n")
            writer.writerow(("zccache-1.2.3.data/scripts/zccache", "", ""))
            writer.writerow(("zccache-1.2.3.dist-info/METADATA", "", ""))
            writer.writerow(("zccache-1.2.3.dist-info/WHEEL", "", ""))
            writer.writerow(("zccache-1.2.3.dist-info/RECORD", "", ""))
            whl.writestr("zccache-1.2.3.dist-info/RECORD", record.getvalue())


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


@pytest.mark.skipif(
    os.name == "nt",
    reason="Windows install targets do not expose POSIX execute bits",
)
def test_assert_installed_wheel_scripts_executable_accepts_pip_target_install(
    tmp_path: Path,
) -> None:
    wheel_path = tmp_path / "zccache-1.2.3-py3-none-any.whl"
    _write_wheel(
        wheel_path,
        create_system=3,
        mode=0o100755,
        include_dist_info=True,
    )

    assert_installed_wheel_scripts_executable(wheel_path)
