# CI/CD Workflows

GitHub Actions workflow definitions.

- **ci.yml** - Runs fmt, Dylint, MSRV, and doc builds on main and pull requests.
- **ci-check.yml** - Reusable check/test workflow used by the OS-specific CI workflows.
- **clippy.yml** - Runs Clippy on pushes to main for the README status badge.
- **benchmark-stats.yml** - Manual/scheduled zccache vs bare compiler vs sccache benchmark publisher for the README image and rendered stats page.
- **perf-guard.yml** - Manual/automatic Rust, C, and C++ perf-regression guard that fails below the zccache vs bare compiler or pinned-sccache speed floors and uploads Markdown/JSON run artifacts.
