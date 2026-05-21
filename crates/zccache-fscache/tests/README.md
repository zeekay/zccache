# zccache-fscache integration tests

Integration tests for `zccache-fscache` that need to run as separate
binaries (so they exercise the public API the way downstream crates do).

## Tests

- **`persistence_perf_test.rs`** — Perf regression tests for
  `MetadataCache::save_to_disk` / `load_from_disk`. Locks in the
  warm-side fast-path behaviour for the
  `cold-tar-untar-warm × medium` perf-rust-cluster cell: without
  persistence, every fresh daemon (including the one spawned after
  `soldr load` restores a cache dir) starts with an empty `DashMap`
  and pays full stat+blake3 on every header lookup. The tests assert
  (a) a 200-entry snapshot loads in under 50 ms and (b) the
  `get_cached_hash_if_stat_valid` fast path returns the cached hash
  after a `save → drop → load` round-trip against a real file.

Run with:

```
soldr cargo test -p zccache-fscache --test persistence_perf_test -- --nocapture
```
