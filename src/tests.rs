/// Unit tests for zccache core modules.
///
/// Run with: `cargo test`
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

// ── helpers ──────────────────────────────────────────────────────────────────

fn tmp_dir() -> TempDir {
    tempfile::tempdir().expect("Failed to create temp dir")
}

// ── compiler arg parsing ─────────────────────────────────────────────────────

mod parse_args {
    use super::*;
    use crate::compiler::parse_args;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn basic_compile_only() {
        let args = s(&["-c", "hello.c", "-o", "hello.o"]);
        let inv = parse_args(&args).expect("should be cacheable");
        assert_eq!(inv.input_file, PathBuf::from("hello.c"));
        assert_eq!(inv.output_file, PathBuf::from("hello.o"));
        assert!(inv.dep_file.is_none());
    }

    #[test]
    fn default_output_name() {
        let args = s(&["-c", "foo.cpp"]);
        let inv = parse_args(&args).expect("should be cacheable");
        assert_eq!(inv.output_file, PathBuf::from("foo.o"));
    }

    #[test]
    fn dep_file_parsing() {
        let args = s(&["-c", "a.c", "-o", "a.o", "-MF", "a.d"]);
        let inv = parse_args(&args).expect("should be cacheable");
        assert_eq!(inv.dep_file, Some(PathBuf::from("a.d")));
    }

    #[test]
    fn link_step_not_cacheable() {
        // No -c flag → link step → not cacheable
        let args = s(&["a.o", "b.o", "-o", "prog"]);
        assert!(parse_args(&args).is_none());
    }

    #[test]
    fn multiple_sources_not_cacheable() {
        // Multiple sources are not cached
        let args = s(&["-c", "a.c", "b.c"]);
        assert!(parse_args(&args).is_none());
    }

    #[test]
    fn hash_args_exclude_output() {
        let args = s(&["-c", "hello.c", "-o", "hello.o", "-O2"]);
        let inv = parse_args(&args).expect("should be cacheable");
        // -o and its value should NOT appear in hash_args
        assert!(!inv.hash_args.contains(&"-o".to_string()));
        assert!(!inv.hash_args.contains(&"hello.o".to_string()));
        // -O2 and -c should appear
        assert!(inv.hash_args.contains(&"-O2".to_string()));
        assert!(inv.hash_args.contains(&"-c".to_string()));
    }

    #[test]
    fn hash_args_exclude_dep_flags() {
        let args = s(&["-c", "x.c", "-MD", "-MF", "x.d", "-MT", "x.o"]);
        let inv = parse_args(&args).expect("should be cacheable");
        // Dep-related flags/values must not pollute the hash
        for excluded in &["-MD", "-MF", "x.d", "-MT", "x.o"] {
            assert!(
                !inv.hash_args.contains(&excluded.to_string()),
                "expected '{}' to be excluded from hash_args",
                excluded
            );
        }
    }
}

// ── cache key computation ─────────────────────────────────────────────────────

mod hash_keys {
    use crate::hash::compute_key;

    #[test]
    fn same_inputs_same_key() {
        let id = b"compiler-id";
        let src = b"int main(){}";
        let args = vec!["-c".to_string(), "-O2".to_string()];
        let k1 = compute_key(id, src, &args);
        let k2 = compute_key(id, src, &args);
        assert_eq!(k1, k2);
    }

    #[test]
    fn different_source_different_key() {
        let id = b"compiler-id";
        let args = vec!["-c".to_string()];
        let k1 = compute_key(id, b"int a = 1;", &args);
        let k2 = compute_key(id, b"int a = 2;", &args);
        assert_ne!(k1, k2);
    }

    #[test]
    fn different_flags_different_key() {
        let id = b"compiler-id";
        let src = b"void f(){}";
        let k1 = compute_key(id, src, &["-O0".to_string()]);
        let k2 = compute_key(id, src, &["-O2".to_string()]);
        assert_ne!(k1, k2);
    }

    #[test]
    fn different_compiler_different_key() {
        let src = b"void f(){}";
        let args: Vec<String> = vec![];
        let k1 = compute_key(b"gcc-13", src, &args);
        let k2 = compute_key(b"clang-17", src, &args);
        assert_ne!(k1, k2);
    }

    #[test]
    fn arg_order_invariant() {
        // Flags sorted before hashing – order should not matter.
        let id = b"compiler-id";
        let src = b"void f(){}";
        let k1 = compute_key(id, src, &["-O2".to_string(), "-Wall".to_string()]);
        let k2 = compute_key(id, src, &["-Wall".to_string(), "-O2".to_string()]);
        assert_eq!(k1, k2);
    }

    #[test]
    fn key_is_hex_string() {
        let key = compute_key(b"id", b"src", &[]);
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(key.len(), 64); // BLAKE3 → 32 bytes → 64 hex chars
    }
}

// ── cache storage and lookup ──────────────────────────────────────────────────

mod cache_ops {
    use super::*;
    use crate::cache;

    #[test]
    fn store_and_lookup_hit() {
        let cache_tmp = tmp_dir();
        let cache_dir = cache_tmp.path();

        // Create a fake object file
        let src_tmp = tmp_dir();
        let obj = src_tmp.path().join("a.o");
        fs::write(&obj, b"fake object data").unwrap();

        let key = "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890";

        cache::store(cache_dir, key, &obj, None).expect("store should succeed");
        let result = cache::lookup(cache_dir, key).expect("lookup should not error");
        assert!(result.is_some(), "expected cache hit");
    }

    #[test]
    fn lookup_miss() {
        let cache_tmp = tmp_dir();
        let cache_dir = cache_tmp.path();

        let key = "1111111111111111111111111111111111111111111111111111111111111111";
        let result = cache::lookup(cache_dir, key).expect("lookup should not error");
        assert!(result.is_none(), "expected cache miss");
    }

    #[test]
    fn store_and_restore() {
        let cache_tmp = tmp_dir();
        let cache_dir = cache_tmp.path();
        let src_tmp = tmp_dir();

        let obj = src_tmp.path().join("a.o");
        let content = b"my object content";
        fs::write(&obj, content).unwrap();

        let key = "aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111";
        cache::store(cache_dir, key, &obj, None).unwrap();

        let cached = cache::lookup(cache_dir, key).unwrap().unwrap();
        let dest = src_tmp.path().join("restored.o");
        cache::restore(&cached, &dest).unwrap();

        assert_eq!(fs::read(&dest).unwrap(), content);
    }

    #[test]
    fn dep_file_stored_and_accessible() {
        let cache_tmp = tmp_dir();
        let cache_dir = cache_tmp.path();
        let src_tmp = tmp_dir();

        let obj = src_tmp.path().join("b.o");
        fs::write(&obj, b"obj").unwrap();
        let dep = src_tmp.path().join("b.d");
        fs::write(&dep, b"b.o: b.c header.h").unwrap();

        let key = "bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222";
        cache::store(cache_dir, key, &obj, Some(&dep)).unwrap();

        let dep_cached = cache::dep_path(cache_dir, key);
        assert!(dep_cached.exists(), "cached dep file should exist");
        assert_eq!(fs::read(&dep_cached).unwrap(), b"b.o: b.c header.h");
    }

    #[test]
    fn clear_removes_objects() {
        let cache_tmp = tmp_dir();
        let cache_dir = cache_tmp.path();
        let src_tmp = tmp_dir();

        let obj = src_tmp.path().join("c.o");
        fs::write(&obj, b"data").unwrap();
        let key = "cccc3333cccc3333cccc3333cccc3333cccc3333cccc3333cccc3333cccc3333";
        cache::store(cache_dir, key, &obj, None).unwrap();

        cache::clear(cache_dir).unwrap();

        let result = cache::lookup(cache_dir, key).unwrap();
        assert!(result.is_none(), "cache should be empty after clear");
    }
}

// ── stats ─────────────────────────────────────────────────────────────────────

mod stats_tests {
    use super::*;
    use crate::stats;

    #[test]
    fn initial_stats_are_zero() {
        let tmp = tmp_dir();
        let s = stats::load(tmp.path()).unwrap();
        assert_eq!(s.cache_hits, 0);
        assert_eq!(s.cache_misses, 0);
        assert_eq!(s.cache_errors, 0);
    }

    #[test]
    fn record_hit_increments() {
        let tmp = tmp_dir();
        stats::record_hit(tmp.path()).unwrap();
        stats::record_hit(tmp.path()).unwrap();
        let s = stats::load(tmp.path()).unwrap();
        assert_eq!(s.cache_hits, 2);
        assert_eq!(s.cache_misses, 0);
    }

    #[test]
    fn record_miss_increments() {
        let tmp = tmp_dir();
        stats::record_miss(tmp.path()).unwrap();
        let s = stats::load(tmp.path()).unwrap();
        assert_eq!(s.cache_misses, 1);
    }

    #[test]
    fn zero_stats_resets() {
        let tmp = tmp_dir();
        stats::record_hit(tmp.path()).unwrap();
        stats::record_miss(tmp.path()).unwrap();
        stats::zero(tmp.path()).unwrap();
        let s = stats::load(tmp.path()).unwrap();
        assert_eq!(s.cache_hits, 0);
        assert_eq!(s.cache_misses, 0);
    }
}
