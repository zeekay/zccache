import re

import pytest

from ci import benchmark_stats


SAMPLE_LOG = """
## C Benchmark: 50 .c files, 5 warm trials

| Scenario | Bare clang | sccache | zccache | vs sccache | vs bare clang |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| Single-file, Cold | 3.000s | — | 2.000s | — | 1.5x faster |
| Single-file, Warm | 3.000s | — | **0.050s** | — | **60x faster** |

## Benchmark: 50 C++ files, 5 warm trials

| Scenario | Bare Clang | sccache | zccache | vs sccache | vs bare clang |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| Single-file, Cold | 12.641s | 20.632s | 13.430s | 1.5x faster | 1.1x slower |
| Single-file, Warm | 11.705s | 1.576s | **0.050s** | **32x faster** | **236x faster** |

## Response-File Benchmark: 50 C++ files, ~283 expanded args, 5 warm trials

| Scenario | Bare Clang | sccache | zccache | vs sccache | vs bare clang |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| Single-file RSP, Cold | 12.063s | 20.607s | 14.087s | 1.5x faster | 1.2x slower |
| Single-file RSP, Warm | 12.540s | 1.558s | **0.047s** | **33x faster** | **267x faster** |

## Rust Benchmark: 50 .rs files, 5 warm trials

| Scenario | Bare rustc | sccache | zccache | vs sccache | vs bare rustc |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| Build, Cold | 8.165s | 9.634s | 41.624s | 4.3x slower | 5.1x slower |
| Build, Warm | 7.018s | 8.236s | **0.123s** | **67x faster** | **57x faster** |

## C++ Sibling-Workspace Remap Benchmark: 50 .cpp files, 5 warm trials

| Scenario | Bare clang | sccache | zccache | vs sccache | vs bare clang |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| Sibling-workspace, Warm | 11.812s | 1.602s | **0.052s** | **31x faster** | **227x faster** |

## Rust Sibling-Workspace Remap Benchmark: 50 .rs files, 5 warm trials

| Scenario | Bare rustc | sccache | zccache | vs sccache | vs bare rustc |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| Sibling-workspace, Warm | 7.142s | 8.301s | **0.127s** | **65x faster** | **56x faster** |
"""


def sample_payload():
    rows = benchmark_stats.parse_benchmark_log(SAMPLE_LOG)
    return benchmark_stats.build_payload(
        rows,
        {
            "generated_at": "2026-05-13T00:00:00+00:00",
            "repository": "zackees/zccache",
            "git_sha": "abcdef1234567890",
            "git_ref": "main",
            "run_url": "https://example.invalid/run",
            "runner": {"platform": "test", "os": "test", "arch": "x64", "cpu_count": 1},
            "versions": {"soldr": None, "rustc": None, "clang": None, "sccache": None},
            "benchmark_command": "soldr --no-cache cargo test ...",
            "pages_url": "https://zackees.github.io/zccache/",
            "raw_image_base_url": benchmark_stats.DEFAULT_RAW_IMAGE_BASE_URL,
            "raw_image_urls": {
                language: f"{benchmark_stats.DEFAULT_RAW_IMAGE_BASE_URL}/{image_file}"
                for language, image_file in benchmark_stats.LANGUAGE_IMAGE_FILES.items()
            },
        },
    )


def test_parse_benchmark_log_extracts_all_tables():
    rows = benchmark_stats.parse_benchmark_log(SAMPLE_LOG)

    assert len(rows) == 10
    assert {row["benchmark"] for row in rows} == {
        "c-inline",
        "cpp-inline",
        "cpp-response-file",
        "cpp-sibling-remap",
        "rust",
        "rust-sibling-remap",
    }

    c_warm = [row for row in rows if row["benchmark"] == "c-inline" and row["mode"] == "warm"][0]
    assert c_warm["language"] == "c"
    assert c_warm["zccache_vs_bare_ratio"] == 60.0

    rust_warm = [row for row in rows if row["scenario"] == "Build, Warm"][0]
    assert rust_warm["language"] == "rust"
    assert rust_warm["zccache_seconds"] == 0.123
    assert rust_warm["zccache_vs_sccache_ratio"] == 66.959

    cpp_remap = [row for row in rows if row["benchmark"] == "cpp-sibling-remap"][0]
    assert cpp_remap["mode"] == "warm"
    assert cpp_remap["language"] == "c++"
    assert cpp_remap["zccache_seconds"] == 0.052

    rust_remap = [row for row in rows if row["benchmark"] == "rust-sibling-remap"][0]
    assert rust_remap["mode"] == "warm"
    assert rust_remap["language"] == "rust"
    assert rust_remap["zccache_seconds"] == 0.127


def test_group_results_by_language_returns_expected_buckets():
    rows = benchmark_stats.parse_benchmark_log(SAMPLE_LOG)

    groups = benchmark_stats.group_results_by_language(rows)

    assert list(groups) == ["c", "c++", "rust"]
    assert {benchmark_stats.LANGUAGE_LABELS[language] for language in groups} == {
        "C",
        "C++",
        "Rust",
    }
    assert [len(groups[language]) for language in groups] == [2, 5, 3]


def test_render_html_links_json_stats_and_language_images_only():
    payload = sample_payload()

    html = benchmark_stats.render_html(payload)
    jpg_references = re.findall(r'(?:href|src)="([^"]+\.jpg)"', html)

    assert 'href="latest.json"' in html
    assert set(jpg_references) == set(benchmark_stats.LANGUAGE_IMAGE_FILES.values())
    assert "benchmark.jpg" not in html
    assert "Rust rustc" in html


def test_image_rows_cover_c_cpp_and_rust_stats():
    rows = benchmark_stats.parse_benchmark_log(SAMPLE_LOG)
    image_rows = benchmark_stats.build_image_rows(rows)

    assert {row["language"] for row in image_rows} == {"C", "C++", "Rust"}
    assert any("C inline args - Single-file, Warm" == row["scenario"] for row in image_rows)
    assert any(
        row["compact_label"] == "inline args"
        and row["compact_scenario"] == "Single-file"
        for row in image_rows
    )
    assert any(
        "C++ response files - Single-file RSP, Cold" == row["scenario"]
        for row in image_rows
    )
    assert any(
        row["compact_label"] == "response files"
        and row["compact_scenario"] == "Single-file RSP"
        for row in image_rows
    )
    assert any("Rust rustc - Build, Warm" == row["scenario"] for row in image_rows)


def test_write_outputs_creates_timestamped_benchmark_image(tmp_path):
    pytest.importorskip("PIL")
    payload = sample_payload()
    stale_combined_image = tmp_path / "benchmark.jpg"
    stale_combined_image.write_text("stale", encoding="utf-8")

    benchmark_stats.write_outputs(payload, tmp_path)

    assert (tmp_path / "latest.json").is_file()
    assert (tmp_path / "index.html").is_file()
    assert (tmp_path / ".nojekyll").is_file()
    for image_file in benchmark_stats.LANGUAGE_IMAGE_FILES.values():
        assert (tmp_path / image_file).stat().st_size > 0
    assert not stale_combined_image.exists()


def test_readme_benchmark_images_link_to_results_branch():
    readme = (benchmark_stats.REPO_ROOT / "README.md").read_text(encoding="utf-8")

    for language, image_file in benchmark_stats.LANGUAGE_IMAGE_FILES.items():
        label = benchmark_stats.LANGUAGE_LABELS[language]
        image_url = f"{benchmark_stats.DEFAULT_RAW_IMAGE_BASE_URL}/{image_file}"
        expected = (
            f"[![Latest zccache {label} benchmark stats]"
            f"({image_url})]({benchmark_stats.BENCHMARK_STATS_BRANCH_URL})"
        )
        assert expected in readme
    assert "benchmark.jpg" not in readme


def test_ratio_tone_and_color_classify_faster_slower_and_neutral():
    assert benchmark_stats.ratio_tone(1.25) == "faster"
    assert benchmark_stats.ratio_color(1.25) == benchmark_stats.RATIO_COLORS["faster"]
    assert benchmark_stats.ratio_tone(0.75) == "slower"
    assert benchmark_stats.ratio_color(0.75) == benchmark_stats.RATIO_COLORS["slower"]
    assert benchmark_stats.ratio_tone(None) == "neutral"
    assert benchmark_stats.ratio_tone(1.0) == "neutral"
    assert benchmark_stats.ratio_color(None) == benchmark_stats.RATIO_COLORS["neutral"]


def test_percent_delta_format_covers_faster_slower_and_missing():
    assert benchmark_stats._format_percent_delta(1.25) == "25.0% faster"
    assert benchmark_stats._format_percent_delta(0.8) == "25.0% slower"
    assert benchmark_stats._format_percent_delta(1.0) == "0.0%"
    assert benchmark_stats._format_percent_delta(None) == "n/a"
