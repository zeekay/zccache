# zccache-daemon benches

Criterion micro-benchmarks for hot-path I/O fan-out in the daemon. Each bench
defines a `serial` and `parallel` implementation of the same workload and runs
them under criterion so the speedup ratio is visible in one report.

| Bench | Measures | Why it exists |
|---|---|---|
| `write_payloads` | hardlink-or-copy of N cache files to N output paths | Cache-hit payload write fan-out (see `write_cached_output` in `src/server.rs`) |
| `read_outputs` | `fs::read` of N output files into in-memory buffers | Link cache-populate read fan-out (see `handle_link_ephemeral` in `src/server.rs`) |

Run all benches:

```bash
soldr cargo bench -p zccache-daemon --bench write_payloads
soldr cargo bench -p zccache-daemon --bench read_outputs
```

`N = 1` is the regression guard for the single-output `.o` compile path —
the parallel variant must not be meaningfully slower than the serial variant
when there is only one item.
