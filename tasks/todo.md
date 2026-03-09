# Warm Cache Hit Optimization

Profile showed 425µs/hit with these bottlenecks:
- write_output (fs::write): 250µs (59%) — 3 syscalls (remove + exists + hardlink)
- hash_headers: 63µs (15%) — per-file stat/DashMap lookups
- depgraph_check: 33µs (8%) — redundant artifact key recomputation

## Plan

- [x] Add dashmap dependency to zccache-daemon
- [x] Add clock-based fast path: skip hash_source + hash_headers + depgraph_check when journal clock hasn't advanced since last verified hit
- [x] Optimize write_output: try hardlink first (1 syscall), remove+retry only on failure
- [x] Update profiler to track fast-path hits (zero hash/depgraph phases)
- [x] Invalidate fast-path cache on miss
- [x] Run profile test to measure improvement
- [x] Run full test suite (484 tests pass)
- [x] Run bench.py to measure end-to-end improvement

## Results

### Profile test (daemon-side, debug build)
- BEFORE: 425µs/hit, IPC round-trip 709µs
- AFTER:  288µs/hit, IPC round-trip 538µs (32% faster)
- hash_source: 9µs → 0µs, hash_headers: 63µs → 1µs, depgraph: 33µs → 0µs

### Benchmark (release build, end-to-end)
| Tool | Warm Build | Speedup vs Bare |
|------|-----------|-----------------|
| bare clang | 1.148s | 1.0x |
| sccache | 0.035s | 32.5x |
| **zccache (before)** | **0.018s** | **33.8x** |
| **zccache (after)** | **0.008s** | **141.6x** |

zccache warm hits: 18ms → 8ms (2.25x faster), now 4.4x faster than sccache.
