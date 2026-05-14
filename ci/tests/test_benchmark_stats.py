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
"""


def test_parse_benchmark_log_extracts_all_tables():
    rows = benchmark_stats.parse_benchmark_log(SAMPLE_LOG)

    assert len(rows) == 8
    assert {row["benchmark"] for row in rows} == {
        "c-inline",
        "cpp-inline",
        "cpp-response-file",
        "rust",
    }

    c_warm = [row for row in rows if row["benchmark"] == "c-inline" and row["mode"] == "warm"][0]
    assert c_warm["language"] == "c"
    assert c_warm["zccache_vs_bare_ratio"] == 60.0

    rust_warm = [row for row in rows if row["scenario"] == "Build, Warm"][0]
    assert rust_warm["language"] == "rust"
    assert rust_warm["zccache_seconds"] == 0.123
    assert rust_warm["zccache_vs_sccache_ratio"] == 66.959


def test_render_html_links_json_stats():
    rows = benchmark_stats.parse_benchmark_log(SAMPLE_LOG)
    payload = benchmark_stats.build_payload(
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
            "raw_image_url": "https://raw.githubusercontent.com/zackees/zccache/benchmark-stats/benchmark.jpg",
        },
    )

    html = benchmark_stats.render_html(payload)

    assert 'href="latest.json"' in html
    assert 'src="benchmark.jpg"' in html
    assert "Rust rustc" in html
