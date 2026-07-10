//! Tests for `# env-dep:` extraction from rustc dep-info (issue #1021).
//!
//! rustc records every `env!()` / `option_env!()` read as a comment line in
//! dep-info; `parse_rustc_depinfo` must surface the names (sorted, deduped,
//! volatile path-valued names filtered) and must never misparse comment
//! lines as file dependencies.

use super::super::*;

fn parse(content: &str) -> RustcDepScan {
    let cwd = std::env::temp_dir();
    parse_rustc_depinfo(content, Path::new("src/lib.rs"), &cwd)
}

#[test]
fn env_dep_lines_extract_names_only() {
    let scan = parse(
        "libfoo.rlib: src/lib.rs\n\
         src/lib.rs:\n\
         # env-dep:STAMP=one\n\
         # env-dep:GIT_SHA=abc123\n",
    );
    assert_eq!(
        scan.env_deps,
        vec!["GIT_SHA".to_string(), "STAMP".to_string()]
    );
}

#[test]
fn env_dep_without_value_records_name() {
    // option_env!() on an unset var emits a bare name.
    let scan = parse("# env-dep:MAYBE_SET\n");
    assert_eq!(scan.env_deps, vec!["MAYBE_SET".to_string()]);
}

#[test]
fn env_dep_names_are_deduped_and_sorted() {
    let scan = parse(
        "# env-dep:B=2\n\
         # env-dep:A=1\n\
         # env-dep:B=2\n",
    );
    assert_eq!(scan.env_deps, vec!["A".to_string(), "B".to_string()]);
}

#[test]
fn volatile_env_dep_names_are_filtered() {
    let scan = parse(
        "# env-dep:OUT_DIR=/target/debug/build/foo-abc/out\n\
         # env-dep:CARGO_MANIFEST_DIR=/workspace/foo\n\
         # env-dep:STAMP=x\n",
    );
    assert_eq!(scan.env_deps, vec!["STAMP".to_string()]);
}

#[test]
fn env_dep_value_containing_equals_keeps_full_name_split_at_first() {
    let scan = parse("# env-dep:FLAGS=-C opt-level=3\n");
    assert_eq!(scan.env_deps, vec!["FLAGS".to_string()]);
}

#[test]
fn comment_lines_are_never_file_deps() {
    // An env-dep value that looks like an existing path must not leak into
    // the resolved file-dependency set.
    let tmp = tempfile::tempdir().unwrap();
    let real = tmp.path().join("real.h");
    std::fs::write(&real, "x").unwrap();
    let content = format!(
        "libfoo.rlib: src/lib.rs\n# env-dep:CONF={}\n# some other comment\n",
        real.display()
    );
    let scan = parse_rustc_depinfo(&content, Path::new("src/lib.rs"), tmp.path());
    assert!(
        scan.scan.resolved.is_empty(),
        "comment lines must not contribute file deps, got {:?}",
        scan.scan.resolved
    );
    assert_eq!(scan.env_deps, vec!["CONF".to_string()]);
}

#[test]
fn file_deps_still_parse_alongside_env_deps() {
    let tmp = tempfile::tempdir().unwrap();
    let dep = tmp.path().join("other.rs");
    std::fs::write(&dep, "x").unwrap();
    let content = format!(
        "libfoo.rlib: src/lib.rs {}\n# env-dep:STAMP=one\n",
        dep.display()
    );
    let scan = parse_rustc_depinfo(&content, Path::new("src/lib.rs"), tmp.path());
    assert_eq!(scan.scan.resolved.len(), 1);
    assert_eq!(scan.env_deps, vec!["STAMP".to_string()]);
}
