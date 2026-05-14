"""Generate durable zccache benchmark stats artifacts.

The benchmark runner reuses `perf_bench_test` and turns its markdown tables into
three generated files suitable for publishing from an orphan branch:

- `index.html` for humans
- `latest.json` for machines
- `benchmark.jpg` for README embedding
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import platform
import re
import shutil
import subprocess
import sys
import tempfile
from html import escape
from pathlib import Path
from typing import Any


REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_OUTPUT_DIR = REPO_ROOT / "benchmark-stats"
DEFAULT_PAGES_URL = "https://zackees.github.io/zccache/"
DEFAULT_RAW_IMAGE_URL = (
    "https://raw.githubusercontent.com/zackees/zccache/benchmark-stats/benchmark.jpg"
)
BENCHMARK_BASE_COMMAND = [
    "soldr",
    "--no-cache",
    "cargo",
    "test",
    "-p",
    "zccache-daemon",
    "--test",
    "perf_bench_test",
]
BENCHMARK_TESTS_BY_LANGUAGE = {
    "c": ("perf_c_zccache_vs_bare",),
    "c++": ("perf_warm_cache_zccache_vs_sccache", "perf_response_file"),
    "rust": ("perf_rustc_zccache_vs_sccache",),
}
BENCHMARK_COMMAND = [
    *BENCHMARK_BASE_COMMAND,
    "--",
    "--nocapture",
    "--ignored",
    "--test-threads=1",
]


TABLES = {
    "## C Benchmark:": {
        "id": "c-inline",
        "label": "C inline args",
        "language": "c",
        "bare_label": "Bare clang",
    },
    "## Benchmark:": {
        "id": "cpp-inline",
        "label": "C++ inline args",
        "language": "c++",
        "bare_label": "Bare clang",
    },
    "## Response-File Benchmark:": {
        "id": "cpp-response-file",
        "label": "C++ response files",
        "language": "c++",
        "bare_label": "Bare clang",
    },
    "## Rust Benchmark:": {
        "id": "rust",
        "label": "Rust rustc",
        "language": "rust",
        "bare_label": "Bare rustc",
    },
}


def benchmark_command_for_test(test_name: str) -> list[str]:
    return [
        *BENCHMARK_BASE_COMMAND,
        "--",
        test_name,
        "--nocapture",
        "--ignored",
        "--test-threads=1",
    ]


def benchmark_commands_for_language(language: str) -> list[list[str]]:
    try:
        test_names = BENCHMARK_TESTS_BY_LANGUAGE[language]
    except KeyError as exc:
        supported = ", ".join(sorted(BENCHMARK_TESTS_BY_LANGUAGE))
        message = f"unsupported benchmark language {language!r}; expected {supported}"
        raise ValueError(message) from exc
    return [benchmark_command_for_test(test_name) for test_name in test_names]


def _utc_now() -> str:
    return dt.datetime.now(dt.timezone.utc).replace(microsecond=0).isoformat()


def _run_quiet(cmd: list[str], cwd: Path = REPO_ROOT) -> str | None:
    try:
        result = subprocess.run(
            cmd,
            cwd=cwd,
            text=True,
            encoding="utf-8",
            errors="replace",
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
        )
    except OSError:
        return None
    output = result.stdout.strip()
    if result.returncode != 0:
        return output or None
    return output or None


def _first_line(value: str | None) -> str | None:
    if not value:
        return None
    for line in value.splitlines():
        line = line.strip()
        if line:
            return line
    return None


def _git_sha() -> str | None:
    return os.environ.get("GITHUB_SHA") or _first_line(_run_quiet(["git", "rev-parse", "HEAD"]))


def _git_ref() -> str | None:
    return os.environ.get("GITHUB_REF_NAME") or _first_line(
        _run_quiet(["git", "rev-parse", "--abbrev-ref", "HEAD"])
    )


def collect_metadata() -> dict[str, Any]:
    run_id = os.environ.get("GITHUB_RUN_ID")
    repository = os.environ.get("GITHUB_REPOSITORY", "zackees/zccache")
    run_url = None
    if run_id:
        run_url = f"https://github.com/{repository}/actions/runs/{run_id}"

    return {
        "generated_at": _utc_now(),
        "repository": repository,
        "git_sha": _git_sha(),
        "git_ref": _git_ref(),
        "run_url": run_url,
        "runner": {
            "os": os.environ.get("RUNNER_OS") or platform.system(),
            "arch": platform.machine(),
            "platform": platform.platform(),
            "cpu_count": os.cpu_count(),
        },
        "versions": {
            "soldr": _first_line(_run_quiet(["soldr", "version"])),
            "rustc": _first_line(_run_quiet(["soldr", "--no-cache", "rustc", "--version"])),
            "clang": _first_line(_run_quiet(["clang++", "--version"])),
            "sccache": _first_line(_run_quiet(["sccache", "--version"])),
        },
        "benchmark_command": " ".join(BENCHMARK_COMMAND),
        "pages_url": os.environ.get("ZCCACHE_BENCHMARK_PAGES_URL", DEFAULT_PAGES_URL),
        "raw_image_url": os.environ.get("ZCCACHE_BENCHMARK_IMAGE_URL", DEFAULT_RAW_IMAGE_URL),
    }


def run_benchmarks(log_path: Path) -> str:
    cache_dir = Path(tempfile.mkdtemp(prefix="zccache-benchmark-cache-"))
    env = os.environ.copy()
    env["ZCCACHE_CACHE_DIR"] = str(cache_dir)
    env.pop("RUSTC_WRAPPER", None)

    try:
        result = subprocess.run(
            BENCHMARK_COMMAND,
            cwd=REPO_ROOT,
            env=env,
            text=True,
            encoding="utf-8",
            errors="replace",
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
        )
    finally:
        shutil.rmtree(cache_dir, ignore_errors=True)

    output = result.stdout
    log_path.parent.mkdir(parents=True, exist_ok=True)
    log_path.write_text(output, encoding="utf-8")
    print(output, end="")
    if result.returncode != 0:
        raise SystemExit(result.returncode)
    return output


def _clean_cell(cell: str) -> str:
    text = cell.strip()
    text = text.replace("**", "")
    text = text.replace("`", "")
    return text.strip()


def _duration_seconds(value: str) -> float | None:
    text = _clean_cell(value)
    if not text or text in {"-", "\u2014", "n/a", "N/A"}:
        return None
    match = re.fullmatch(r"([0-9]+(?:\.[0-9]+)?)\s*(ms|s)", text)
    if match is None:
        return None
    number = float(match.group(1))
    if match.group(2) == "ms":
        return round(number / 1000.0, 6)
    return round(number, 6)


def _ratio(baseline: float | None, candidate: float | None) -> float | None:
    if baseline is None or candidate is None or candidate <= 0:
        return None
    return round(baseline / candidate, 3)


def parse_benchmark_log(text: str) -> list[dict[str, Any]]:
    current: dict[str, str] | None = None
    results: list[dict[str, Any]] = []

    for raw_line in text.splitlines():
        line = raw_line.strip()
        for prefix, table in TABLES.items():
            if line.startswith(prefix):
                current = table
                break
        else:
            if current is None or not line.startswith("|"):
                continue
            cells = [_clean_cell(cell) for cell in line.strip("|").split("|")]
            if len(cells) < 6 or cells[0] == "Scenario" or set(cells[0]) <= {":", "-"}:
                continue

            bare_seconds = _duration_seconds(cells[1])
            sccache_seconds = _duration_seconds(cells[2])
            zccache_seconds = _duration_seconds(cells[3])
            scenario = cells[0]
            mode = "warm" if "warm" in scenario.lower() else "cold"
            result = {
                "benchmark": current["id"],
                "benchmark_label": current["label"],
                "language": current["language"],
                "scenario": scenario,
                "mode": mode,
                "bare_label": current["bare_label"],
                "bare_seconds": bare_seconds,
                "sccache_seconds": sccache_seconds,
                "zccache_seconds": zccache_seconds,
                "zccache_vs_sccache_ratio": _ratio(sccache_seconds, zccache_seconds),
                "zccache_vs_bare_ratio": _ratio(bare_seconds, zccache_seconds),
                "vs_sccache_text": cells[4],
                "vs_bare_text": cells[5],
            }
            results.append(result)

    return results


def _format_seconds(value: float | None) -> str:
    return "n/a" if value is None else f"{value:.3f}s"


def _format_ratio(value: float | None) -> str:
    if value is None:
        return "n/a"
    if value >= 1:
        return f"{value:.1f}x faster"
    return f"{1 / value:.1f}x slower"


def build_summary(results: list[dict[str, Any]]) -> dict[str, Any]:
    warm = [row for row in results if row["mode"] == "warm"]
    cold = [row for row in results if row["mode"] == "cold"]
    best_vs_sccache = max(
        (row for row in warm if row["zccache_vs_sccache_ratio"] is not None),
        key=lambda row: row["zccache_vs_sccache_ratio"],
        default=None,
    )
    best_vs_bare = max(
        (row for row in warm if row["zccache_vs_bare_ratio"] is not None),
        key=lambda row: row["zccache_vs_bare_ratio"],
        default=None,
    )
    return {
        "row_count": len(results),
        "warm_row_count": len(warm),
        "cold_row_count": len(cold),
        "best_warm_vs_sccache": best_vs_sccache,
        "best_warm_vs_bare": best_vs_bare,
    }


def build_payload(results: list[dict[str, Any]], metadata: dict[str, Any]) -> dict[str, Any]:
    return {
        "schema_version": 1,
        "metadata": metadata,
        "summary": build_summary(results),
        "results": results,
    }


def _grouped_results(results: list[dict[str, Any]]) -> dict[str, list[dict[str, Any]]]:
    groups: dict[str, list[dict[str, Any]]] = {}
    for result in results:
        groups.setdefault(result["benchmark_label"], []).append(result)
    return groups


def render_html(payload: dict[str, Any]) -> str:
    metadata = payload["metadata"]
    rows_html: list[str] = []
    for group, rows in _grouped_results(payload["results"]).items():
        rows_html.append(
            "<tr class=\"group\"><th colspan=\"6\">" + escape(group) + "</th></tr>"
        )
        for row in rows:
            rows_html.append(
                "<tr>"
                f"<td>{escape(row['scenario'])}</td>"
                f"<td>{escape(_format_seconds(row['bare_seconds']))}</td>"
                f"<td>{escape(_format_seconds(row['sccache_seconds']))}</td>"
                f"<td class=\"strong\">{escape(_format_seconds(row['zccache_seconds']))}</td>"
                f"<td>{escape(_format_ratio(row['zccache_vs_sccache_ratio']))}</td>"
                f"<td>{escape(_format_ratio(row['zccache_vs_bare_ratio']))}</td>"
                "</tr>"
            )

    versions = metadata["versions"]
    version_items = "\n".join(
        f"<li><strong>{escape(name)}</strong>: {escape(value or 'n/a')}</li>"
        for name, value in versions.items()
    )
    run_link = ""
    if metadata.get("run_url"):
        run_link = f" | <a href=\"{escape(metadata['run_url'])}\">workflow run</a>"

    return f"""<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>zccache benchmark stats</title>
    <style>
      body {{
        margin: 0;
        padding: 32px 20px 44px;
        font-family: Arial, sans-serif;
        color: #202426;
        background: #f7f8f8;
      }}
      main {{
        max-width: 1080px;
        margin: 0 auto;
      }}
      h1 {{
        margin: 0 0 10px;
        font-size: 34px;
      }}
      h2 {{
        margin: 28px 0 10px;
        font-size: 22px;
      }}
      p, li {{
        line-height: 1.5;
      }}
      .meta {{
        color: #556166;
      }}
      .note {{
        padding: 12px 14px;
        background: #eef2f3;
        border: 1px solid #d9e0e3;
      }}
      .table-wrap {{
        overflow-x: auto;
      }}
      table {{
        width: 100%;
        min-width: 820px;
        border-collapse: collapse;
        margin-top: 18px;
        background: white;
      }}
      th, td {{
        border: 1px solid #d9e0e3;
        padding: 10px 12px;
        text-align: left;
        font-size: 14px;
      }}
      thead th, tr.group th {{
        background: #e9eef0;
      }}
      .strong {{
        font-weight: 700;
      }}
      img {{
        max-width: 100%;
        border: 1px solid #d9e0e3;
      }}
    </style>
  </head>
  <body>
    <main>
      <h1>zccache benchmark stats</h1>
      <p class="meta">
        Generated {escape(metadata['generated_at'])} |
        ref {escape(metadata.get('git_ref') or 'n/a')} |
        sha {escape((metadata.get('git_sha') or 'n/a')[:12])}{run_link}
      </p>
      <p class="note">
        Raw machine-readable data: <a href="latest.json">latest.json</a>.
        README image: <a href="benchmark.jpg">benchmark.jpg</a>.
      </p>
      <p><img src="benchmark.jpg" alt="Latest zccache benchmark summary" /></p>
      <div class="table-wrap">
        <table>
          <thead>
            <tr>
              <th>Scenario</th>
              <th>Bare compiler</th>
              <th>sccache</th>
              <th>zccache</th>
              <th>zccache vs sccache</th>
              <th>zccache vs bare</th>
            </tr>
          </thead>
          <tbody>
            {''.join(rows_html)}
          </tbody>
        </table>
      </div>
      <h2>Environment</h2>
      <ul>
        <li><strong>Runner</strong>: {escape(metadata['runner']['platform'])}</li>
        <li><strong>Benchmark command</strong>: <code>{escape(metadata['benchmark_command'])}</code></li>
        {version_items}
      </ul>
    </main>
  </body>
</html>
"""


def _font(size: int, bold: bool = False) -> Any:
    candidates = [
        "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf" if bold else "",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "C:/Windows/Fonts/arialbd.ttf" if bold else "",
        "C:/Windows/Fonts/arial.ttf",
    ]
    for candidate in candidates:
        if candidate and Path(candidate).exists():
            try:
                from PIL import ImageFont

                return ImageFont.truetype(candidate, size)
            except OSError:
                continue
    from PIL import ImageFont

    return ImageFont.load_default()


def render_jpg(payload: dict[str, Any], path: Path) -> None:
    try:
        from PIL import Image, ImageDraw
    except ImportError as exc:
        raise SystemExit(
            "Pillow is required to write benchmark.jpg. Install it with `uv run --with pillow` "
            "or `python -m pip install Pillow`."
        ) from exc

    width, height = 1200, 630
    image = Image.new("RGB", (width, height), "#f7f8f8")
    draw = ImageDraw.Draw(image)
    title_font = _font(42, bold=True)
    subtitle_font = _font(22)
    header_font = _font(20, bold=True)
    row_font = _font(18)
    small_font = _font(15)

    draw.rectangle((0, 0, width, 96), fill="#202426")
    draw.text((42, 26), "zccache benchmark stats", font=title_font, fill="#ffffff")
    metadata = payload["metadata"]
    sha = (metadata.get("git_sha") or "n/a")[:12]
    draw.text(
        (42, 106),
        f"Generated {metadata['generated_at']} | {metadata.get('git_ref') or 'n/a'} | {sha}",
        font=subtitle_font,
        fill="#38464b",
    )

    warm_rows = [row for row in payload["results"] if row["mode"] == "warm"]
    display_rows = warm_rows[:6]
    x0, y0 = 42, 166
    table_w = width - 84
    row_h = 54
    headers = ["Scenario", "Bare", "sccache", "zccache", "vs sccache"]
    widths = [430, 145, 145, 145, 220]

    draw.rounded_rectangle((x0, y0, x0 + table_w, y0 + 42), radius=8, fill="#e9eef0")
    x = x0 + 16
    for header, col_w in zip(headers, widths):
        draw.text((x, y0 + 11), header, font=header_font, fill="#202426")
        x += col_w

    y = y0 + 42
    for index, row in enumerate(display_rows):
        fill = "#ffffff" if index % 2 == 0 else "#f1f4f5"
        draw.rectangle((x0, y, x0 + table_w, y + row_h), fill=fill)
        values = [
            f"{row['benchmark_label']} - {row['scenario']}",
            _format_seconds(row["bare_seconds"]),
            _format_seconds(row["sccache_seconds"]),
            _format_seconds(row["zccache_seconds"]),
            _format_ratio(row["zccache_vs_sccache_ratio"]),
        ]
        x = x0 + 16
        for value, col_w in zip(values, widths):
            color = "#0f6b44" if value == values[3] else "#202426"
            draw.text((x, y + 15), value[:44], font=row_font, fill=color)
            x += col_w
        y += row_h

    best = payload["summary"].get("best_warm_vs_sccache")
    if best:
        line = (
            f"Best warm result: {best['benchmark_label']} {best['scenario']} "
            f"at {_format_ratio(best['zccache_vs_sccache_ratio'])} than sccache."
        )
    else:
        line = "Benchmark data is not available yet."
    draw.rounded_rectangle((42, height - 92, width - 42, height - 34), radius=8, fill="#202426")
    draw.text((62, height - 75), line[:118], font=small_font, fill="#ffffff")

    path.parent.mkdir(parents=True, exist_ok=True)
    image.save(path, format="JPEG", quality=90, optimize=True)


def write_outputs(payload: dict[str, Any], output_dir: Path) -> None:
    output_dir.mkdir(parents=True, exist_ok=True)
    (output_dir / "latest.json").write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    (output_dir / "index.html").write_text(render_html(payload), encoding="utf-8")
    (output_dir / ".nojekyll").write_text("", encoding="utf-8")
    render_jpg(payload, output_dir / "benchmark.jpg")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output-dir", type=Path, default=DEFAULT_OUTPUT_DIR)
    parser.add_argument("--input-log", type=Path, help="Parse an existing perf log instead of running benchmarks.")
    parser.add_argument(
        "--run-benchmarks",
        action="store_true",
        help="Run the ignored perf benchmark suite before generating artifacts.",
    )
    parser.add_argument("--log-path", type=Path, default=DEFAULT_OUTPUT_DIR / "benchmark.log")
    args = parser.parse_args(argv)

    if args.input_log:
        text = args.input_log.read_text(encoding="utf-8")
    elif args.run_benchmarks:
        text = run_benchmarks(args.log_path)
    else:
        parser.error("use --run-benchmarks or --input-log")

    results = parse_benchmark_log(text)
    if not results:
        raise SystemExit("no benchmark result rows found in benchmark output")
    payload = build_payload(results, collect_metadata())
    write_outputs(payload, args.output_dir)
    print(f"wrote benchmark stats to {args.output_dir}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
