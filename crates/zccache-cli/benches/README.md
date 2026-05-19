# zccache-cli benches

Criterion micro-benchmark for the `zccache warm` cache-restore fan-out.

| Bench | Measures |
|---|---|
| `warm_restore` | Per-output `remove_file + hard_link + open + set_times` over N entries, serial vs `rayon::par_iter` |

Run:

```bash
soldr cargo bench -p zccache-cli --bench warm_restore
```

Each entry is independent; restore order doesn't matter. CI cache restores
can be 1k–5k outputs, so the per-file syscall cost dominates wall-clock time.

Counts: 100 / 1000 / 5000.
