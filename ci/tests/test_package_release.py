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


def _repo_text(*parts: str) -> str:
    return (Path(__file__).resolve().parents[2] / Path(*parts)).read_text(
        encoding="utf-8"
    )


def _matrix_entry(workflow_text: str, target: str) -> str:
    marker = f"          - target: {target}\n"
    start = workflow_text.index(marker)
    next_start = workflow_text.find("\n          - target:", start + len(marker))
    steps_start = workflow_text.find("\n    steps:", start)
    end_candidates = [pos for pos in (next_start, steps_start) if pos != -1]
    assert end_candidates
    return workflow_text[start : min(end_candidates)]


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


def test_stage_debug_tree_packages_dwp_files(tmp_path: Path) -> None:
    debug_input_dir = tmp_path / "staging-debug"
    output_dir = tmp_path / "output"
    debug_input_dir.mkdir()
    output_dir.mkdir()

    for name in package_release.INCLUDE:
        (debug_input_dir / f"{name}.dwp").write_bytes(b"dwp\n")

    result = package_release.stage_debug_tree(
        version="1.3.10",
        target="x86_64-unknown-linux-gnu",
        debug_input_dir=debug_input_dir,
        output_dir=output_dir,
    )
    assert result is not None
    debug_stage_dir, debug_archive_base = result
    archive = package_release.write_tarball(debug_stage_dir, debug_archive_base)

    assert archive.name == "zccache-v1.3.10-x86_64-unknown-linux-gnu-debug.tar.gz"
    with tarfile.open(archive, "r:gz") as tf:
        members = {member.name for member in tf.getmembers()}
        for name in package_release.INCLUDE:
            assert (
                f"zccache-v1.3.10-x86_64-unknown-linux-gnu-debug/{name}.dwp" in members
            )


def test_stage_debug_tree_handles_dsym_directories(tmp_path: Path) -> None:
    debug_input_dir = tmp_path / "staging-debug"
    output_dir = tmp_path / "output"
    debug_input_dir.mkdir()
    output_dir.mkdir()

    dsym = debug_input_dir / "zccache.dSYM"
    (dsym / "Contents/Resources/DWARF").mkdir(parents=True)
    (dsym / "Contents/Resources/DWARF/zccache").write_bytes(b"dwarf\n")

    result = package_release.stage_debug_tree(
        version="1.3.10",
        target="x86_64-apple-darwin",
        debug_input_dir=debug_input_dir,
        output_dir=output_dir,
    )
    assert result is not None
    debug_stage_dir, debug_archive_base = result
    archive = package_release.write_tarball(debug_stage_dir, debug_archive_base)

    with tarfile.open(archive, "r:gz") as tf:
        members = {member.name for member in tf.getmembers()}
        assert (
            "zccache-v1.3.10-x86_64-apple-darwin-debug/zccache.dSYM/"
            "Contents/Resources/DWARF/zccache" in members
        )


def test_build_target_dereferences_debug_sidecar_symlinks() -> None:
    action = _repo_text(".github/actions/build-target/action.yml")

    assert 'cp -RL "$src" staging-debug/' in action
    assert 'cp -L "$src" staging-debug/' in action


def test_build_target_uses_isolated_stamp_target_dir() -> None:
    action = _repo_text(".github/actions/build-target/action.yml")

    assert "HOST_TARGET=$(soldr rustc -vV" in action
    assert (
        '--target "$HOST_TARGET" --target-dir "$STAMP_TARGET_DIR" '
        "-p zccache --bin zccache-stamp"
    ) in action
    assert 'echo "ZCCACHE_STAMP_HOST_TARGET=$HOST_TARGET"' in action
    assert (
        'STAMP="$STAMP_TARGET_DIR/$STAMP_HOST_TARGET/release/'
        'zccache-stamp$STAMP_EXT"'
    ) in action
    assert "target/release/zccache-stamp" not in action


def test_build_target_exposes_cross_cache_controls() -> None:
    action = _repo_text(".github/actions/build-target/action.yml")

    assert "prebuild_deps:" in action
    assert "clear_target_after_setup:" in action
    assert "prebuild-deps: ${{ inputs.prebuild_deps }}" in action
    assert "if: inputs.clear_target_after_setup == 'true'" in action
    assert 'TARGET_DIR="target/${{ inputs.target }}"' in action
    assert 'rm -rf "$TARGET_DIR"' in action


def test_build_target_can_synthesize_macos_dsym_sidecars() -> None:
    action = _repo_text(".github/actions/build-target/action.yml")

    assert "copy_or_create_dsym()" in action
    assert 'dsymutil "$TARGET_DIR/$bin" -o "staging-debug/$dsym"' in action
    assert 'copy_or_create_dsym "zccache" "zccache.dSYM"' in action
    assert 'copy_or_create_dsym "zccache-daemon" "zccache-daemon.dSYM"' in action
    assert 'copy_or_create_dsym "zccache-fp" "zccache-fp.dSYM"' in action


def test_release_and_build_workflows_disable_cook_cache_for_cross_targets() -> None:
    cross_targets = {
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl",
        "aarch64-unknown-linux-gnu",
        "aarch64-apple-darwin",
        "aarch64-pc-windows-msvc",
    }
    native_targets = {
        "x86_64-unknown-linux-gnu",
        "x86_64-apple-darwin",
        "x86_64-pc-windows-msvc",
    }

    for workflow_path in (
        ".github/workflows/build.yml",
        ".github/workflows/release-auto.yml",
    ):
        workflow = _repo_text(workflow_path)
        assert "prebuild_deps: ${{ matrix.prebuild_deps || 'soldr-cook' }}" in workflow
        assert (
            "clear_target_after_setup: "
            "${{ matrix.clear_target_after_setup || 'false' }}"
        ) in workflow

        for target in cross_targets:
            block = _matrix_entry(workflow, target)
            assert "prebuild_deps: none" in block
            assert 'clear_target_after_setup: "true"' in block

        for target in native_targets:
            block = _matrix_entry(workflow, target)
            assert "prebuild_deps: none" not in block
            assert "clear_target_after_setup:" not in block


def test_stage_debug_tree_skips_empty_input(tmp_path: Path) -> None:
    debug_input_dir = tmp_path / "staging-debug"
    output_dir = tmp_path / "output"
    debug_input_dir.mkdir()
    output_dir.mkdir()

    assert (
        package_release.stage_debug_tree(
            version="1.3.10",
            target="x86_64-unknown-linux-gnu",
            debug_input_dir=debug_input_dir,
            output_dir=output_dir,
        )
        is None
    )


def test_stage_debug_tree_skips_missing_input(tmp_path: Path) -> None:
    missing = tmp_path / "nope"
    output_dir = tmp_path / "output"
    output_dir.mkdir()

    assert (
        package_release.stage_debug_tree(
            version="1.3.10",
            target="x86_64-unknown-linux-gnu",
            debug_input_dir=missing,
            output_dir=output_dir,
        )
        is None
    )
