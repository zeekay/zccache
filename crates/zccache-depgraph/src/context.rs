//! Compilation context and cache key computation.
//!
//! The context key identifies a unique (source + flags) combination
//! and maps to an include list. The artifact key incorporates content
//! hashes of all files for artifact store lookup.

use std::path::PathBuf;

use zccache_hash::ContentHash;

use crate::args::ParsedArgs;
use crate::search_paths::IncludeSearchPaths;

/// blake3 hash identifying a (source + include_dirs + defines + flags) combination.
/// Same context key = same set of resolved headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContextKey(ContentHash);

impl ContextKey {
    /// Returns the underlying hash.
    #[must_use]
    pub fn hash(&self) -> &ContentHash {
        &self.0
    }
}

impl std::fmt::Display for ContextKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ctx:{}", self.0.to_hex())
    }
}

/// blake3 hash identifying a specific compilation output.
/// Same artifact key = the exact same `.o` file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ArtifactKey(ContentHash);

impl ArtifactKey {
    /// Returns the underlying hash.
    #[must_use]
    pub fn hash(&self) -> &ContentHash {
        &self.0
    }
}

impl std::fmt::Display for ArtifactKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "art:{}", self.0.to_hex())
    }
}

/// All inputs defining a compilation context.
#[derive(Debug, Clone)]
pub struct CompileContext {
    /// Absolute path to the source file.
    pub source_file: PathBuf,
    /// Ordered include search paths.
    pub include_search: IncludeSearchPaths,
    /// Sorted defines (-D flags).
    pub defines: Vec<String>,
    /// Sorted cache-relevant flags (-std, -O, -f, etc.).
    pub flags: Vec<String>,
    /// Force-included files (-include).
    pub force_includes: Vec<PathBuf>,
}

impl CompileContext {
    /// Build a `CompileContext` from parsed arguments.
    #[must_use]
    pub fn from_parsed_args(args: &ParsedArgs) -> Self {
        let mut defines = args.defines.clone();
        defines.sort();
        let mut flags = args.flags.clone();
        flags.sort();

        Self {
            source_file: args.source_file.clone(),
            include_search: args.include_search.clone(),
            defines,
            flags,
            force_includes: args.force_includes.clone(),
        }
    }

    /// Compute the context key.
    ///
    /// Includes: source file path, include dirs (in order), sorted defines,
    /// sorted flags, force includes.
    #[must_use]
    pub fn context_key(&self) -> ContextKey {
        let mut hasher = blake3::Hasher::new();

        // Domain separation.
        hasher.update(b"zccache-context-key-v1\0");

        // Source file (absolute path).
        hasher.update(self.source_file.to_string_lossy().as_bytes());
        hasher.update(b"\0");

        // Include dirs in order (order matters for resolution!).
        hasher.update(b"iquote\0");
        for dir in &self.include_search.iquote {
            hasher.update(dir.to_string_lossy().as_bytes());
            hasher.update(b"\0");
        }
        hasher.update(b"user\0");
        for dir in &self.include_search.user {
            hasher.update(dir.to_string_lossy().as_bytes());
            hasher.update(b"\0");
        }
        hasher.update(b"system\0");
        for dir in &self.include_search.system {
            hasher.update(dir.to_string_lossy().as_bytes());
            hasher.update(b"\0");
        }
        hasher.update(b"after\0");
        for dir in &self.include_search.after {
            hasher.update(dir.to_string_lossy().as_bytes());
            hasher.update(b"\0");
        }

        // Sorted defines.
        hasher.update(b"defines\0");
        for def in &self.defines {
            hasher.update(def.as_bytes());
            hasher.update(b"\0");
        }

        // Sorted flags.
        hasher.update(b"flags\0");
        for flag in &self.flags {
            hasher.update(flag.as_bytes());
            hasher.update(b"\0");
        }

        // Force includes.
        hasher.update(b"force-include\0");
        for fi in &self.force_includes {
            hasher.update(fi.to_string_lossy().as_bytes());
            hasher.update(b"\0");
        }

        ContextKey(ContentHash::from_bytes(*hasher.finalize().as_bytes()))
    }
}

/// Compute the artifact key from a context key and file content hashes.
///
/// The artifact key uniquely identifies a specific compilation output.
/// `file_hashes` should contain `(path, content_hash)` pairs for the
/// source file and all resolved headers, sorted by path.
pub fn compute_artifact_key(
    context_key: &ContextKey,
    file_hashes: &mut [(PathBuf, ContentHash)],
) -> ArtifactKey {
    // Sort by path for determinism.
    file_hashes.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"zccache-artifact-key-v1\0");
    hasher.update(context_key.0.as_bytes());
    hasher.update(b"\0");

    for (path, hash) in file_hashes.iter() {
        hasher.update(path.to_string_lossy().as_bytes());
        hasher.update(b"\0");
        hasher.update(hash.as_bytes());
        hasher.update(b"\0");
    }

    ArtifactKey(ContentHash::from_bytes(*hasher.finalize().as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::UserDepFlags;

    fn make_context(source: &str, user_dirs: &[&str], defines: &[&str]) -> CompileContext {
        CompileContext {
            source_file: PathBuf::from(source),
            include_search: IncludeSearchPaths {
                user: user_dirs.iter().map(PathBuf::from).collect(),
                ..Default::default()
            },
            defines: {
                let mut d: Vec<String> = defines.iter().map(|s| s.to_string()).collect();
                d.sort();
                d
            },
            flags: Vec::new(),
            force_includes: Vec::new(),
        }
    }

    #[test]
    fn context_key_deterministic() {
        let ctx = make_context("/src/foo.c", &["/inc"], &["DEBUG"]);
        let k1 = ctx.context_key();
        let k2 = ctx.context_key();
        assert_eq!(k1, k2);
    }

    #[test]
    fn different_source_different_key() {
        let k1 = make_context("/src/a.c", &["/inc"], &[]).context_key();
        let k2 = make_context("/src/b.c", &["/inc"], &[]).context_key();
        assert_ne!(k1, k2);
    }

    #[test]
    fn different_defines_different_key() {
        let k1 = make_context("/src/a.c", &["/inc"], &["DEBUG"]).context_key();
        let k2 = make_context("/src/a.c", &["/inc"], &["RELEASE"]).context_key();
        assert_ne!(k1, k2);
    }

    #[test]
    fn define_order_irrelevant() {
        let k1 = make_context("/src/a.c", &[], &["AAA", "BBB"]).context_key();
        let k2 = make_context("/src/a.c", &[], &["BBB", "AAA"]).context_key();
        assert_eq!(k1, k2, "define order should not affect context key");
    }

    #[test]
    fn include_dir_order_matters() {
        let k1 = make_context("/src/a.c", &["/first", "/second"], &[]).context_key();
        let k2 = make_context("/src/a.c", &["/second", "/first"], &[]).context_key();
        assert_ne!(k1, k2, "include dir order should affect context key");
    }

    #[test]
    fn artifact_key_changes_with_content() {
        let ctx = make_context("/src/a.c", &[], &[]);
        let ck = ctx.context_key();

        let hash_a = zccache_hash::hash_bytes(b"content A");
        let hash_b = zccache_hash::hash_bytes(b"content B");

        let ak1 = compute_artifact_key(&ck, &mut [(PathBuf::from("/src/a.c"), hash_a)]);
        let ak2 = compute_artifact_key(&ck, &mut [(PathBuf::from("/src/a.c"), hash_b)]);
        assert_ne!(ak1, ak2);
    }

    #[test]
    fn artifact_key_stable_same_content() {
        let ctx = make_context("/src/a.c", &[], &[]);
        let ck = ctx.context_key();

        let hash = zccache_hash::hash_bytes(b"content");

        let ak1 = compute_artifact_key(&ck, &mut [(PathBuf::from("/src/a.c"), hash)]);
        let ak2 = compute_artifact_key(&ck, &mut [(PathBuf::from("/src/a.c"), hash)]);
        assert_eq!(ak1, ak2);
    }

    #[test]
    fn artifact_key_file_order_irrelevant() {
        let ctx = make_context("/src/a.c", &[], &[]);
        let ck = ctx.context_key();

        let h1 = zccache_hash::hash_bytes(b"content 1");
        let h2 = zccache_hash::hash_bytes(b"content 2");

        let ak1 = compute_artifact_key(
            &ck,
            &mut [(PathBuf::from("/a.h"), h1), (PathBuf::from("/b.h"), h2)],
        );
        let ak2 = compute_artifact_key(
            &ck,
            &mut [(PathBuf::from("/b.h"), h2), (PathBuf::from("/a.h"), h1)],
        );
        assert_eq!(ak1, ak2, "file order should not affect artifact key");
    }

    #[test]
    fn context_key_display() {
        let ctx = make_context("/src/a.c", &[], &[]);
        let key = ctx.context_key();
        let display = format!("{key}");
        assert!(display.starts_with("ctx:"));
        assert_eq!(display.len(), 4 + 64); // "ctx:" + 64 hex chars
    }

    #[test]
    fn from_parsed_args_sorts() {
        let args = ParsedArgs {
            source_file: PathBuf::from("/src/a.c"),
            output_file: None,
            include_search: IncludeSearchPaths::default(),
            defines: vec!["ZZZ".into(), "AAA".into()],
            undefines: Vec::new(),
            flags: vec!["-Wall".into(), "-O2".into()],
            force_includes: Vec::new(),
            compiler: None,
            dep_flags: UserDepFlags::default(),
        };
        let ctx = CompileContext::from_parsed_args(&args);
        assert_eq!(ctx.defines, vec!["AAA", "ZZZ"]);
        assert_eq!(ctx.flags, vec!["-O2", "-Wall"]);
    }

    #[test]
    fn different_flags_different_key() {
        let mut ctx1 = make_context("/src/a.c", &[], &[]);
        ctx1.flags = vec!["-std=c++17".into()];
        let mut ctx2 = make_context("/src/a.c", &[], &[]);
        ctx2.flags = vec!["-std=c++20".into()];
        assert_ne!(ctx1.context_key(), ctx2.context_key());
    }

    #[test]
    fn force_include_affects_key() {
        let ctx1 = make_context("/src/a.c", &[], &[]);
        let mut ctx2 = make_context("/src/a.c", &[], &[]);
        ctx2.force_includes = vec![PathBuf::from("/pch.h")];
        assert_ne!(ctx1.context_key(), ctx2.context_key());
    }
}
