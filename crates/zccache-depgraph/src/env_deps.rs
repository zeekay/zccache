//! Env-var dependency fingerprinting for rustc compiles (issue #1021).
//!
//! rustc records every `env!()` / `option_env!()` the crate reads as a
//! `# env-dep:NAME[=VALUE]` line in its dep-info output. Cargo consumes those
//! lines to re-invoke rustc when a value changes; zccache must do the same or
//! it serves a stale artifact with the old value baked in (the vergen /
//! shadow-rs / built `cargo:rustc-env=` failure shape).
//!
//! The daemon records the env-dep *names* per context after each compile and
//! stores a fingerprint of their values in that compile's client env. Every
//! hit path recomputes the fingerprint against the *current* request env and
//! forces a recompile on mismatch. An env var's value is the content — this
//! mirrors how include files are content-hashed.

use zccache_hash::ContentHash;

/// Domain-separation tag for [`env_dep_fingerprint`].
const ENV_DEP_FP_DOMAIN: &[u8] = b"zccache-rustc-env-dep-fp-v1\0";

/// Env-dep names excluded from fingerprinting.
///
/// These are path-valued and volatile per checkout location. Their
/// *referenced content* is already covered elsewhere (`OUT_DIR` files appear
/// as path deps in the same dep-info and are content-hashed), so folding the
/// path string itself into the fingerprint would only cascade cache misses
/// across workspace moves/clones — the exact failure `VOLATILE_CARGO_ENV_VARS`
/// exists to prevent in the context key (issue #396).
pub const VOLATILE_ENV_DEP_NAMES: &[&str] = &[
    "OUT_DIR",
    "CARGO_MANIFEST_DIR",
    "CARGO_MANIFEST_PATH",
    "CARGO_TARGET_DIR",
];

/// Fingerprint the values of `names` as found in `client_env`.
///
/// `names` must be sorted and deduplicated (the dep-info parser guarantees
/// this) so the hash is order-independent. Returns `None` when `names` is
/// empty — contexts without env deps carry no fingerprint and skip the gate.
///
/// An unset variable hashes differently from one set to the empty string,
/// so `option_env!()` transitions are caught.
#[must_use]
pub fn env_dep_fingerprint(
    names: &[String],
    client_env: Option<&[(String, String)]>,
) -> Option<ContentHash> {
    if names.is_empty() {
        return None;
    }
    let lookup = |name: &str| -> Option<&str> {
        client_env?
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    let mut hasher = blake3::Hasher::new();
    hasher.update(ENV_DEP_FP_DOMAIN);
    for name in names {
        hasher.update(name.as_bytes());
        hasher.update(b"\0");
        match lookup(name) {
            Some(value) => {
                hasher.update(&[1]);
                hasher.update(value.as_bytes());
            }
            None => {
                hasher.update(&[0]);
            }
        }
        hasher.update(b"\0");
    }
    Some(ContentHash::from_bytes(*hasher.finalize().as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn empty_names_have_no_fingerprint() {
        assert_eq!(env_dep_fingerprint(&[], Some(&env(&[("A", "1")]))), None);
        assert_eq!(env_dep_fingerprint(&[], None), None);
    }

    #[test]
    fn value_change_changes_fingerprint() {
        let names = vec!["STAMP".to_string()];
        let one = env_dep_fingerprint(&names, Some(&env(&[("STAMP", "one")])));
        let two = env_dep_fingerprint(&names, Some(&env(&[("STAMP", "two")])));
        assert!(one.is_some());
        assert_ne!(one, two);
    }

    #[test]
    fn same_values_match_regardless_of_unrelated_env() {
        let names = vec!["STAMP".to_string()];
        let a = env_dep_fingerprint(&names, Some(&env(&[("STAMP", "x"), ("PATH", "p1")])));
        let b = env_dep_fingerprint(&names, Some(&env(&[("PATH", "p2"), ("STAMP", "x")])));
        assert_eq!(a, b);
    }

    #[test]
    fn unset_differs_from_empty_string() {
        let names = vec!["OPT".to_string()];
        let unset = env_dep_fingerprint(&names, Some(&env(&[])));
        let empty = env_dep_fingerprint(&names, Some(&env(&[("OPT", "")])));
        assert!(unset.is_some());
        assert_ne!(unset, empty);
        // No env at all behaves like unset.
        assert_eq!(unset, env_dep_fingerprint(&names, None));
    }

    #[test]
    fn multiple_names_all_participate() {
        let names = vec!["A".to_string(), "B".to_string()];
        let base = env_dep_fingerprint(&names, Some(&env(&[("A", "1"), ("B", "2")])));
        let a_changed = env_dep_fingerprint(&names, Some(&env(&[("A", "9"), ("B", "2")])));
        let b_changed = env_dep_fingerprint(&names, Some(&env(&[("A", "1"), ("B", "9")])));
        assert_ne!(base, a_changed);
        assert_ne!(base, b_changed);
        assert_ne!(a_changed, b_changed);
    }
}
