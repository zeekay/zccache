mod cache;
mod compiler;
mod hash;
mod stats;

#[cfg(test)]
mod tests;

use std::env;
use std::process;

fn main() {
    let args: Vec<String> = env::args().collect();
    // args[0] is the zccache binary itself.

    if args.len() < 2 {
        print_usage();
        process::exit(1);
    }

    let exit_code = match args[1].as_str() {
        "--show-stats" | "-s" => handle_show_stats(),
        "--clear-cache" => handle_clear_cache(),
        "--zero-stats" => handle_zero_stats(),
        "--help" | "-h" => {
            print_usage();
            0
        }
        _ => {
            // Treat args[1] as the compiler and args[2..] as compiler arguments.
            let compiler = &args[1];
            let compiler_args = args[2..].to_vec();
            handle_compile(compiler, &compiler_args)
        }
    };

    process::exit(exit_code);
}

// ── zccache sub-commands ─────────────────────────────────────────────────────

fn handle_show_stats() -> i32 {
    let dir = match cache::cache_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("zccache: {}", e);
            return 1;
        }
    };
    match stats::show(&dir) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("zccache: error showing stats: {}", e);
            1
        }
    }
}

fn handle_clear_cache() -> i32 {
    let dir = match cache::cache_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("zccache: {}", e);
            return 1;
        }
    };
    match cache::clear(&dir) {
        Ok(()) => {
            println!("Cache cleared.");
            0
        }
        Err(e) => {
            eprintln!("zccache: error clearing cache: {}", e);
            1
        }
    }
}

fn handle_zero_stats() -> i32 {
    let dir = match cache::cache_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("zccache: {}", e);
            return 1;
        }
    };
    match stats::zero(&dir) {
        Ok(()) => {
            println!("Stats zeroed.");
            0
        }
        Err(e) => {
            eprintln!("zccache: error zeroing stats: {}", e);
            1
        }
    }
}

// ── compilation dispatch ─────────────────────────────────────────────────────

fn handle_compile(compiler: &str, compiler_args: &[String]) -> i32 {
    // Bypass the cache entirely when ZCCACHE_DISABLE is set.
    if env::var("ZCCACHE_DISABLE").is_ok() {
        return compiler::run_direct(compiler, compiler_args);
    }

    match try_cached_compile(compiler, compiler_args) {
        Ok(code) => code,
        Err(e) => {
            if env::var("ZCCACHE_DEBUG").is_ok() {
                eprintln!("zccache: falling back to direct compilation: {:#}", e);
            }
            // Record cache error and fall through to direct compilation.
            if let Ok(dir) = cache::cache_dir() {
                let _ = stats::record_error(&dir);
            }
            compiler::run_direct(compiler, compiler_args)
        }
    }
}

fn try_cached_compile(compiler: &str, compiler_args: &[String]) -> anyhow::Result<i32> {
    use anyhow::Context;

    // Parse the invocation; if it's not a cacheable compile-only step, pass through.
    let Some(invocation) = compiler::parse_args(compiler_args) else {
        return Ok(compiler::run_direct(compiler, compiler_args));
    };

    let cache_dir = cache::cache_dir().context("Failed to resolve cache directory")?;

    // --- fast path: compute cache key ------------------------------------------

    let compiler_id =
        hash::compiler_identity(compiler).context("Failed to hash compiler identity")?;

    let preprocessed =
        compiler::preprocess(compiler, compiler_args).context("Preprocessing failed")?;

    let cache_key = hash::compute_key(&compiler_id, &preprocessed, &invocation.hash_args);

    // --- cache lookup -----------------------------------------------------------

    if let Some(cached_obj) = cache::lookup(&cache_dir, &cache_key)? {
        // Cache hit – restore object file (and optional dep file).
        cache::restore(&cached_obj, &invocation.output_file)
            .context("Failed to restore cached object file")?;

        if let Some(ref dep_dest) = invocation.dep_file {
            let cached_dep = cache::dep_path(&cache_dir, &cache_key);
            if cached_dep.exists() {
                cache::restore(&cached_dep, dep_dest)
                    .context("Failed to restore cached dep file")?;
            }
        }

        stats::record_hit(&cache_dir)?;

        if env::var("ZCCACHE_DEBUG").is_ok() {
            eprintln!("zccache: cache hit  [{}]", &cache_key[..16]);
        }

        return Ok(0);
    }

    // --- cache miss: compile and store -----------------------------------------

    stats::record_miss(&cache_dir)?;

    if env::var("ZCCACHE_DEBUG").is_ok() {
        eprintln!("zccache: cache miss [{}]", &cache_key[..16]);
    }

    let exit_code =
        compiler::compile_and_cache(compiler, compiler_args, &invocation, &cache_dir, &cache_key)?;

    Ok(exit_code)
}

// ── usage ────────────────────────────────────────────────────────────────────

fn print_usage() {
    println!("zccache {}", env!("CARGO_PKG_VERSION"));
    println!("A fast compiler cache – sccache but designed for speed.");
    println!();
    println!("USAGE:");
    println!("  zccache <compiler> [compiler args...]");
    println!("  zccache --show-stats");
    println!("  zccache --clear-cache");
    println!("  zccache --zero-stats");
    println!("  zccache --help");
    println!();
    println!("ENVIRONMENT:");
    println!("  ZCCACHE_DIR      Override cache directory (default: ~/.cache/zccache)");
    println!("  ZCCACHE_DISABLE  Set to disable caching (pass through to compiler)");
    println!("  ZCCACHE_DEBUG    Set to enable debug output");
    println!();
    println!("EXAMPLES:");
    println!("  # Wrap gcc");
    println!("  zccache gcc -c hello.c -o hello.o");
    println!();
    println!("  # Use as a compiler wrapper in a build system");
    println!("  CC=\"zccache gcc\" make");
    println!();
    println!("  # Show cache statistics");
    println!("  zccache --show-stats");
}
