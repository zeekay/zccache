# zccache-depgraph benches

Criterion micro-benchmarks for the recursive `#include` scanner.

| Bench | Measures |
|---|---|
| `scan_recursive` | `scan_recursive` over a synthetic header tree (small + large fixtures) |

Run:

```bash
soldr cargo bench -p zccache-depgraph --bench scan_recursive
```

Capture a baseline on the pre-change commit, then re-run on the implementation
commit to see the speedup:

```bash
soldr cargo bench -p zccache-depgraph --bench scan_recursive -- --save-baseline pre
# ... apply the impl change ...
soldr cargo bench -p zccache-depgraph --bench scan_recursive -- --baseline pre
```

The `small_depth3_fanout3` fixture is the regression guard for small TUs;
`large_depth5_fanout4` (~250 headers) is the workload that demonstrates the
parallel BFS win.
