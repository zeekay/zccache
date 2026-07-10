//! `DepGraph::record_env_deps` / `DepGraph::env_deps_match` — the env-dep
//! gate for rustc contexts (issue #1021).
//!
//! Carved out of `mod.rs` to keep each file under the 1k-LOC guard. See
//! `crate::env_deps` for the fingerprint itself and the rationale.

use super::super::context::ContextKey;
use super::super::env_deps::env_dep_fingerprint;
use super::DepGraph;
use zccache_hash::ContentHash;

impl DepGraph {
    /// Record the env-dep names scanned from rustc dep-info after a compile,
    /// plus the fingerprint of their values in that compile's client env.
    ///
    /// Called *after* `update()` on the store path: a concurrent reader in
    /// the window between the two sees the OLD fingerprint and fails the
    /// gate, which degrades to a safe recompile rather than a stale hit.
    pub fn record_env_deps(
        &self,
        key: &ContextKey,
        env_deps: Vec<String>,
        env_dep_fp: Option<ContentHash>,
    ) {
        if let Some(mut entry) = self.contexts.get_mut(key) {
            entry.env_deps = env_deps;
            entry.env_dep_fp = env_dep_fp;
        }
    }

    /// Check whether the context's recorded env-dep values match the current
    /// request env. Returns `true` when the context has no recorded env deps
    /// (C/C++ contexts, rustc crates without `env!()`), or is unknown — the
    /// gate only ever *blocks* hits on a known mismatch.
    ///
    /// Every hit path must consult this: the request-level and context-level
    /// zero-hash fast paths never recompute keys, so a changed value (e.g.
    /// vergen's `cargo:rustc-env=GIT_SHA`) is otherwise invisible to them.
    #[must_use]
    pub fn env_deps_match(
        &self,
        key: &ContextKey,
        client_env: Option<&[(String, String)]>,
    ) -> bool {
        let Some(entry) = self.contexts.get(key) else {
            return true;
        };
        if entry.env_deps.is_empty() {
            return true;
        }
        env_dep_fingerprint(&entry.env_deps, client_env) == entry.env_dep_fp
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::context::CompileContext;
    use super::super::DepGraph;
    use super::env_dep_fingerprint;
    use zccache_core::NormalizedPath;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn register_ctx(graph: &DepGraph) -> super::ContextKey {
        let ctx = CompileContext {
            source_file: NormalizedPath::from("/tmp/envdep.rs"),
            include_search: Default::default(),
            defines: Vec::new(),
            flags: Vec::new(),
            force_includes: Vec::new(),
            unknown_flags: Vec::new(),
        };
        graph.register(ctx)
    }

    #[test]
    fn unknown_context_and_no_env_deps_pass_the_gate() {
        let graph = DepGraph::new();
        let key = register_ctx(&graph);
        let bogus = super::ContextKey::from_raw([9u8; 32]);
        assert!(graph.env_deps_match(&bogus, None));
        assert!(graph.env_deps_match(&key, Some(&env(&[("STAMP", "one")]))));
    }

    #[test]
    fn value_change_fails_the_gate_and_same_value_passes() {
        let graph = DepGraph::new();
        let key = register_ctx(&graph);
        let names = vec!["STAMP".to_string()];
        let stored = env(&[("STAMP", "one")]);
        let fp = env_dep_fingerprint(&names, Some(&stored));
        graph.record_env_deps(&key, names, fp);

        assert!(graph.env_deps_match(&key, Some(&env(&[("STAMP", "one")]))));
        assert!(!graph.env_deps_match(&key, Some(&env(&[("STAMP", "two")]))));
        assert!(!graph.env_deps_match(&key, Some(&env(&[]))));
        assert!(!graph.env_deps_match(&key, None));
    }

    #[test]
    fn recorded_names_with_missing_fingerprint_fail_closed() {
        let graph = DepGraph::new();
        let key = register_ctx(&graph);
        graph.record_env_deps(&key, vec!["STAMP".to_string()], None);
        assert!(!graph.env_deps_match(&key, Some(&env(&[("STAMP", "one")]))));
    }
}
