# Depfile-Based Include Scanning

Cold cache misses spend 49% on compiler exec and 47% on include scanning.
Strategy: have GCC/Clang emit `-MD -MF <tmpfile>`, parse `.d` file instead of `scan_recursive()`.

## Plan

- [x] Step 1: Depfile parser (`crates/zccache-depgraph/src/depfile.rs`)
- [x] Step 2: Track user dep flags (`UserDepFlags` in args.rs)
- [x] Step 3: `CompilerFamily::supports_depfile()` in compiler crate
- [x] Step 4: Depfile strategy (prepare_depfile)
- [x] Step 5: Real compiler integration tests
- [x] Step 6: Wire into single-file miss path (server.rs)
- [x] Step 7: Wire into multi-file miss path (server.rs)
- [x] Bug fix: depfile parser split_and_unescape didn't split on newlines
- [x] Verify: clippy clean, all tests pass (486 tests + 6 integration)

## Bug Found

Depfile parser `split_and_unescape()` only split on space/tab, not `\n`/`\r`.
Last token in depfile retained trailing newline -> `canonicalize("late.h\n")` failed ->
non-canonicalized path stored -> path mismatch on cache lookup -> false misses.
Fix: split on all whitespace including `\n` and `\r`.
