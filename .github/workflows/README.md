# CI/CD Workflows

GitHub Actions workflow definitions.

- **ci.yml** — Runs fmt, Dylint, MSRV, and doc builds on main and pull requests.
- **ci-check.yml** — Reusable check/test workflow used by the OS-specific CI workflows.
- **clippy.yml** — Runs Clippy on pushes to main for the README status badge.
