# Rust Artifact Plan Contract

This document defines the zccache-side contract for Rust artifact plan execution. It is intentionally about ownership, semantics, and diagnostics rather than Rust compiler integration details.

The goal is to let `soldr` produce a versioned Rust build plan and let `zccache` execute that plan by restoring and saving artifacts without having to re-derive Cargo workspace semantics from scratch.

---

## Scope and Ownership

### zccache owns

- Artifact persistence.
- Restore and save mechanics.
- Artifact archive / bundle format.
- Cache backend behavior, including local disk and GitHub Actions integration.
- Session stats, journals, cache hit / miss reporting, and miss diagnostics.
- Validation and execution of `thin` and `full` Rust cache plans.

### soldr owns

- Cargo invocation context.
- Cargo metadata and workspace interpretation.
- Producing the versioned Rust cache plan.

### setup-soldr owns

- Public action inputs.
- CI presentation and user-facing wiring.

The important boundary is that zccache consumes a structured plan. It should validate that plan, execute it, and report what happened. It should not need to infer the whole Cargo workspace model for the MVP.

---

## Plan Inputs

The plan is versioned and should carry the minimum information needed for deterministic restore/save decisions:

- selected mode: `thin` or `full`
- workspace root
- target directory
- `rustc`, `cargo`, and toolchain identity
- target triple
- profile
- feature, `rustflags`, and environment inputs that affect outputs
- lockfile, config, and manifest hashes
- selected package IDs
- workspace and path dependency exclusions
- allowed artifact classes
- cache schema version

zccache should treat these fields as the source of truth for compatibility checks and plan execution. When a plan is unsupported, the failure should be explicit and versioned.

---

## Thin vs Full

### `thin`

`thin` is the bounded dependency-artifact mode.

It is intended to restore and save the subset of artifacts needed to make dependency crates fresh without recreating unsafe transient state. In practice, that means:

- only the artifact classes explicitly allowed by the plan are eligible
- transient build state stays out of the bundle
- restore should be conservative when a field is missing or mismatched
- save should only persist what the plan says is safe to reuse

`thin` is the mode that supports the common CI flow: restore dependency artifacts, rebuild only the workspace crates that actually changed, and then save the updated reusable state.

### `full`

`full` is explicit whole-target caching.

It is the mode for a plan that wants the full target artifact set restored and saved as a unit. Unlike `thin`, `full` does not try to stay narrow. It still remains plan-driven: zccache saves and restores exactly what the plan describes, not whatever it can infer opportunistically.

### Shared rule

Both modes are bounded by the plan. zccache should not guess at Cargo semantics beyond the inputs it is given.

---

## Backend Direction

### Local backend

The local backend is the primary backend. It owns the on-disk artifact store and the same bundle format used by restore and save.

That means local cache behavior should be the reference implementation for:

- archive layout
- integrity checks
- manifest handling
- size accounting
- reuse diagnostics

### GitHub Actions backend

The GHA backend is a transport / persistence adapter around the same artifact contract.

The design direction is:

- local and GHA backends share the same artifact format
- GHA is an export/import path, not a separate artifact model
- cache compatibility should be decided from the plan and the bundle metadata, not from backend-specific assumptions
- backend failures should surface as diagnostics, not silent reuse misses

This keeps the backend choice orthogonal to the Rust plan semantics. zccache owns the format; the backend only determines where the bundle lives.

---

## Diagnostics

zccache is the authoritative source for whether plan reuse worked.

For every session, the user should be able to inspect:

- restored artifact count and bytes
- saved artifact count and bytes
- skipped artifacts and skip reasons
- compile-cache hits and misses
- target artifact hits and misses
- plan or schema compatibility failures
- key input mismatches
- journal or log path for detailed audit

When reuse is unexpectedly low, the diagnostics should be able to classify misses into a small set of useful reasons:

- artifact absent from the restored plan
- Cargo fingerprint dirty
- toolchain, profile, `rustflags`, or target mismatch
- build-script output or fingerprint mismatch
- compile-cache miss even though the `rustc` command was equivalent

The important nuance is that a successful thin restore can make dependency `rustc` invocations disappear entirely because Cargo considers those dependencies fresh. For that reason, compile-cache hit rate alone is not enough. zccache must report artifact restore effectiveness separately from `rustc` compile-cache reuse.

---

## Proposed CLI

The proposed CLI shape is:

```text
zccache rust-plan validate
zccache rust-plan restore
zccache rust-plan save
```

The commands are intentionally narrow:

- `validate` checks that a versioned Rust plan is supported and internally consistent, but makes no cache changes.
- `restore` applies the plan against the selected backend and restores eligible artifacts.
- `save` captures eligible artifacts and persists or exports them according to the backend.

Suggested behavior:

- all three commands should emit structured diagnostics
- `validate` should fail fast on schema or compatibility errors
- `restore` should report what was restored, what was skipped, and why
- `save` should report what was persisted, what was rejected, and why

This CLI is a contract proposal. The exact flags and plan-file plumbing can be finalized later, but the mode split should remain stable because it maps directly to the ownership boundary and to the CI flow.

---

## Acceptance Shape

The implementation is on the right track when:

- zccache accepts and validates a versioned Rust artifact plan
- `thin` and `full` are distinct and documented execution modes
- local and GHA behavior share the same artifact format
- restore/save outcomes are visible in session stats and journals
- miss diagnostics explain whether reuse failed because of the plan, the backend, or the underlying Cargo / `rustc` inputs

