# zccache-watcher benches

Criterion micro-benchmark for the polling watcher's cold-scan metadata fetch.

| Bench | Measures |
|---|---|
| `scan_metadata` | Per-file `path.metadata()` over N pre-discovered paths, serial vs `rayon::par_iter` |

Run:

```bash
soldr cargo bench -p zccache-watcher --bench scan_metadata
```

Each metadata syscall is independent. On Windows the per-stat cost is
dominated by Defender / antivirus interception (5–20 µs each), so this
is the dominant cost in `polling_watcher::scan_snapshot` on cold start
for repos with thousands of tracked files.

Counts: 100 / 1000 / 5000 — covers tiny / typical / large repos.
