# zccache

A fast compiler cache — **sccache but designed for speed**.

`zccache` wraps your C/C++ compiler and caches the compiled output.  Repeat
builds with identical source and flags return instantly from cache instead of
recompiling.

## Why zccache?

| Feature | sccache | zccache |
|---------|---------|---------|
| Hash algorithm | SHA-256 / MD4 | **BLAKE3** (≈ 3× faster) |
| Cache backend | local / S3 / GCS / … | **local-only** (zero network overhead) |
| Server process | yes (daemon) | **no** (direct file I/O) |
| Hard-link optimisation | no | **yes** (zero-copy cache hits on same FS) |
| Dep-file caching | partial | **yes** (`.d` files cached alongside objects) |

## Installation

```bash
cargo install --path .
```

Or build a release binary:

```bash
cargo build --release
# binary at: target/release/zccache
```

## Usage

### Wrap a compiler directly

```bash
zccache gcc -c hello.c -o hello.o
zccache g++ -c -O2 -std=c++20 main.cpp -o main.o
```

### Use as the compiler in a build system

```bash
# Make
CC="zccache gcc" CXX="zccache g++" make

# CMake
cmake -DCMAKE_C_COMPILER_LAUNCHER=zccache \
      -DCMAKE_CXX_COMPILER_LAUNCHER=zccache ..
```

### Manage the cache

```bash
# Show cache statistics
zccache --show-stats

# Clear all cached objects (stats are preserved)
zccache --clear-cache

# Reset statistics to zero
zccache --zero-stats
```

## Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `ZCCACHE_DIR` | `~/.cache/zccache` | Override the cache directory |
| `ZCCACHE_DISABLE` | *(unset)* | Set to any value to bypass the cache entirely |
| `ZCCACHE_DEBUG` | *(unset)* | Set to any value to print hit/miss diagnostics to stderr |

## How it works

1. **Parse** the compiler invocation.  Only compile-only steps (`-c`) with a
   single source file are cached; link steps pass through unchanged.
2. **Preprocess** the source file (`-E`) to expand all `#include`s.  This
   ensures that changes to any included header are detected.
3. **Hash** the preprocessed source with **BLAKE3** together with the compiler
   identity (binary content + version string) and the relevant flags.
4. **Lookup** the cache.  On a hit the cached object (and optional `.d`
   dependency file) are restored via a hard link (or file copy as fallback).
5. On a **miss** the real compiler runs normally and the outputs are stored in
   the cache for next time.

## Running tests

```bash
cargo test
```

Unit tests cover argument parsing, hashing, cache operations and statistics.
Integration tests exercise the full cache round-trip with the system `gcc`.

## License

MIT
