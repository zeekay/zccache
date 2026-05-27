//! Plan-driven Rust target artifact save/restore.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Component, Path};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::core::{normalize_for_key, NormalizedPath};
use prost::Message;
use rayon::prelude::*;
use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize};

/// Upper bound on save-time worker threads. Beyond this Windows filter-driver
/// serialization dominates and extra threads stop helping (see issue #177 and
/// the linked soldr#272 analysis).
const DEFAULT_RUST_PLAN_TAR_THREADS_CAP: usize = 8;
/// Hard upper bound regardless of caller request — protects small runners from
/// per-thread buffer blowup if someone passes a huge value.
const MAX_RUST_PLAN_TAR_THREADS: usize = 64;

/// Supported Rust artifact plan schema version.
pub const RUST_ARTIFACT_PLAN_SCHEMA_VERSION: u32 = 1;
/// Supported cache bundle schema versions soldr may send. v1 is the legacy
/// shape (thin-v1 / full). v2 is the `thin-v2` opt-in described in soldr#461:
/// it adds the `cache_profile` and `dropped_artifact_classes` fields and
/// splits the legacy `cargo_fingerprint` class into `cargo_fingerprint_meta`
/// (kept) and `cargo_fingerprint_outputs` (dropped). zccache accepts both so
/// older soldr builds keep working unchanged.
pub const SUPPORTED_RUST_ARTIFACT_CACHE_SCHEMA_VERSIONS: &[u32] = &[1, 2];
/// Cache schema version zccache writes into bundle manifests it creates.
/// Pinned at 1 so the on-disk manifest format stays stable across the
/// thin-v2 opt-in — the v2 wire fields are inputs to the save walker, not
/// outputs in the manifest itself.
pub const RUST_ARTIFACT_CACHE_SCHEMA_VERSION: u32 = 1;

const BUNDLE_MANIFEST_NAME: &str = "manifest.pb";
const LEGACY_BUNDLE_MANIFEST_NAME: &str = "manifest.json";
const BUNDLE_FILES_DIR: &str = "files";

/// Errors returned by plan loading and execution.
#[derive(Debug, thiserror::Error)]
pub enum RustPlanError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("protobuf encode error: {0}")]
    ProtobufEncode(#[from] prost::EncodeError),
    #[error("protobuf decode error: {0}")]
    ProtobufDecode(#[from] prost::DecodeError),
    #[error(
        "unsupported Rust artifact plan schema version {found}; supported version is {supported}"
    )]
    UnsupportedSchemaVersion { found: u32, supported: u32 },
    #[error(
        "unsupported Rust artifact cache schema version {found}; supported version is {supported}"
    )]
    UnsupportedCacheSchemaVersion { found: u32, supported: u32 },
    #[error("invalid Rust artifact plan: {0}")]
    InvalidPlan(String),
    #[error("Rust artifact bundle is missing: {0}")]
    BundleMissing(NormalizedPath),
    #[error("invalid Rust artifact bundle manifest: {0}")]
    InvalidManifest(String),
    #[error("unsafe relative artifact path in bundle: {0}")]
    UnsafeRelativePath(String),
}

/// Rust artifact plan mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RustPlanMode {
    /// Restore/save bounded dependency artifacts and Cargo freshness metadata.
    Thin,
    /// Restore/save the target tree explicitly, except transient state.
    Full,
}

impl std::fmt::Display for RustPlanMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Thin => write!(f, "thin"),
            Self::Full => write!(f, "full"),
        }
    }
}

/// Artifact classes that a plan may allow.
///
/// New variants added for the soldr `thin-v2` profile (soldr#461):
/// `CargoFingerprintMeta` / `CargoFingerprintOutputs` split the legacy
/// `CargoFingerprint` umbrella into freshness inputs vs. outputs;
/// `Incremental` / `BuildScriptBuild` / `Dwo` / `Pdb` / `Dsym` enumerate
/// categories that thin-v2 explicitly drops so the wire-format
/// `dropped_artifact_classes` list can name them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RustArtifactClass {
    Rlib,
    Rmeta,
    DepInfo,
    ProcMacro,
    SharedLib,
    /// Legacy thin-v1 umbrella over the `.fingerprint/<crate>/` directory.
    /// Kept for backwards compatibility; thin-v2 splits this into
    /// `CargoFingerprintMeta` (freshness inputs cargo reads) and
    /// `CargoFingerprintOutputs` (everything else in that directory).
    CargoFingerprint,
    /// Freshness-input files inside `<profile>/.fingerprint/<crate>-<hash>/`:
    /// `invoked.timestamp`, `dep-*`, `output-*`, `lib-*`, `bin-*`. Cargo
    /// reads these to decide skip-vs-rebuild, so thin-v2 keeps them.
    CargoFingerprintMeta,
    /// Non-meta files inside `<profile>/.fingerprint/<crate>-<hash>/`. Dropped
    /// by thin-v2 — they are outputs of past compilations, not inputs to the
    /// next freshness decision.
    CargoFingerprintOutputs,
    BuildScriptMetadata,
    BuildScriptOutput,
    /// Compiled build-script binary at `target/<profile>/build/*/build-script-build*`.
    /// thin-v2 drops these — they are cheap to regenerate from cached deps.
    BuildScriptBuild,
    /// Anything under `target/<profile>/incremental/`. Always transient state;
    /// thin-v2 names it explicitly so the drop list is exhaustive.
    Incremental,
    /// `.dwo` split-DWARF files under `deps/`. Dropped by thin-v2.
    Dwo,
    /// `.pdb` Windows debug files under `deps/`. Dropped by thin-v2.
    Pdb,
    /// `.dSYM/` macOS debug bundles under `deps/` (directory bundles — every
    /// file inside the bundle is classified as `Dsym`). Dropped by thin-v2.
    Dsym,
    FullTarget,
}

/// Toolchain identity supplied by soldr.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RustToolchainIdentity {
    pub rustc: String,
    pub cargo: String,
    pub channel: String,
    pub host: String,
}

/// Input hashes that affect Cargo build outputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RustPlanInputs {
    pub features_hash: String,
    pub rustflags_hash: String,
    pub env_hash: String,
    pub lockfile_hash: String,
    pub cargo_config_hash: String,
    #[serde(default)]
    pub manifest_hashes: Vec<String>,
}

/// Package IDs selected or excluded by the planner.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RustPlanPackages {
    #[serde(default)]
    pub selected_package_ids: Vec<String>,
    #[serde(default)]
    pub workspace_package_ids: Vec<String>,
    #[serde(default)]
    pub excluded_path_package_ids: Vec<String>,
}

/// Versioned v1 Rust artifact cache plan.
///
/// Wire-compat note (soldr#461): the previous version used
/// `#[serde(deny_unknown_fields)]`, which made every future soldr addition
/// a coordinated breaking change. The `thin-v2` rollout added
/// `cache_profile` and `dropped_artifact_classes` as new top-level fields,
/// so we now accept (and ignore) unknown fields here. The explicit fields
/// below cover everything zccache acts on; anything else soldr ships in a
/// future plan is silently dropped during deserialization rather than
/// crashing the save/restore.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RustArtifactPlanV1 {
    pub schema_version: u32,
    pub mode: RustPlanMode,
    pub workspace_root: NormalizedPath,
    pub target_dir: NormalizedPath,
    pub toolchain: RustToolchainIdentity,
    pub target_triple: String,
    pub profile: String,
    pub inputs: RustPlanInputs,
    pub packages: RustPlanPackages,
    #[serde(default)]
    pub allowed_artifact_classes: Vec<RustArtifactClass>,
    pub cache_schema_version: u32,
    #[serde(default)]
    pub journal_log_path: Option<NormalizedPath>,
    /// Thin-slice pruning profile selected by soldr. `None` for legacy plans
    /// (thin-v1 / full) that predate soldr#461; `Some("thin-v2")` opts in to
    /// the fingerprint-aware prune.
    #[serde(default)]
    pub cache_profile: Option<String>,
    /// Artifact classes soldr explicitly wants dropped from the saved bundle.
    /// Honored regardless of what `allowed_artifact_classes` says — a file
    /// whose class appears here is skipped even if its class is also in the
    /// allow-list. Empty for legacy plans.
    #[serde(default)]
    pub dropped_artifact_classes: Vec<RustArtifactClass>,
}

impl RustArtifactPlanV1 {
    /// Load, version-check, and validate a plan from a protobuf or legacy JSON file.
    pub fn load(path: &Path) -> Result<Self, RustPlanError> {
        let raw = std::fs::read(path)?;
        if is_probably_json_plan(&raw) {
            let raw = std::str::from_utf8(&raw).map_err(|err| {
                RustPlanError::InvalidPlan(format!("JSON plan is not valid UTF-8: {err}"))
            })?;
            return Self::from_json_str(raw);
        }
        Self::from_proto_bytes(&raw)
    }

    /// Load, version-check, and validate a plan from a JSON string.
    pub fn from_json_str(raw: &str) -> Result<Self, RustPlanError> {
        let value: serde_json::Value = serde_json::from_str(raw.trim_start_matches('\u{feff}'))?;
        Self::from_json_value(value)
    }

    /// Load, version-check, and validate a plan from protobuf bytes.
    pub fn from_proto_bytes(raw: &[u8]) -> Result<Self, RustPlanError> {
        let proto = rust_plan_proto::RustArtifactPlanV1::decode(raw)?;
        plan_from_proto(proto)
    }

    /// Encode this plan as compact protobuf bytes.
    pub fn to_proto_bytes(&self) -> Result<Vec<u8>, RustPlanError> {
        let mut bytes = Vec::new();
        plan_to_proto(self).encode(&mut bytes)?;
        Ok(bytes)
    }

    /// Load, version-check, and validate a plan from a JSON value.
    pub fn from_json_value(value: serde_json::Value) -> Result<Self, RustPlanError> {
        let schema_version = json_u32_field(&value, "schema_version")?;
        if schema_version != RUST_ARTIFACT_PLAN_SCHEMA_VERSION {
            return Err(RustPlanError::UnsupportedSchemaVersion {
                found: schema_version,
                supported: RUST_ARTIFACT_PLAN_SCHEMA_VERSION,
            });
        }

        let cache_schema_version = json_u32_field(&value, "cache_schema_version")?;
        ensure_supported_cache_schema_version(cache_schema_version)?;

        let plan: Self = serde_json::from_value(value)?;
        plan.validate()?;
        Ok(plan)
    }

    /// Validate fields whose constraints are outside serde's type checks.
    pub fn validate(&self) -> Result<(), RustPlanError> {
        let mut errors = Vec::new();
        if self.profile.trim().is_empty() {
            errors.push("profile must not be empty");
        }
        if self.target_triple.trim().is_empty() {
            errors.push("target_triple must not be empty");
        }
        if self.toolchain.rustc.trim().is_empty() {
            errors.push("toolchain.rustc must not be empty");
        }
        if self.toolchain.cargo.trim().is_empty() {
            errors.push("toolchain.cargo must not be empty");
        }
        if self.toolchain.channel.trim().is_empty() {
            errors.push("toolchain.channel must not be empty");
        }
        if self.toolchain.host.trim().is_empty() {
            errors.push("toolchain.host must not be empty");
        }
        if self.workspace_root.as_os_str().is_empty() {
            errors.push("workspace_root must not be empty");
        }
        if self.target_dir.as_os_str().is_empty() {
            errors.push("target_dir must not be empty");
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(RustPlanError::InvalidPlan(errors.join("; ")))
        }
    }

    /// Effective allowed classes for thin mode.
    #[must_use]
    pub fn effective_allowed_classes(&self) -> BTreeSet<RustArtifactClass> {
        if self.allowed_artifact_classes.is_empty() {
            default_thin_classes()
        } else {
            self.allowed_artifact_classes.iter().copied().collect()
        }
    }
}

fn is_probably_json_plan(raw: &[u8]) -> bool {
    let without_bom = raw.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(raw);
    without_bom
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
        .is_some_and(|byte| byte == b'{')
}

/// Backend operation represented in summaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RustPlanOperation {
    Validate,
    Restore,
    Save,
}

/// Compatibility section in a machine-readable summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RustPlanCompatibility {
    pub status: String,
    #[serde(default)]
    pub errors: Vec<String>,
}

/// Representative skipped artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RustPlanSkippedSample {
    pub path: String,
    pub reason: String,
}

/// Artifact restore effectiveness, independent from compile-cache hit rate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RustPlanArtifactEffectiveness {
    pub eligible_file_count: u64,
    pub restored_file_count: u64,
    pub reuse_ratio: f64,
}

/// Machine-readable operation summary for soldr/setup-soldr.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RustPlanSummary {
    pub operation: RustPlanOperation,
    pub mode: RustPlanMode,
    pub plan_schema_version: u32,
    pub cache_schema_version: u32,
    pub compatibility: RustPlanCompatibility,
    pub restored_file_count: u64,
    pub restored_bytes: u64,
    pub saved_file_count: u64,
    pub saved_bytes: u64,
    pub skipped_count: u64,
    #[serde(default)]
    pub skipped_reasons: BTreeMap<String, u64>,
    #[serde(default)]
    pub skipped_samples: Vec<RustPlanSkippedSample>,
    #[serde(default)]
    pub key_input_mismatches: Vec<String>,
    #[serde(default)]
    pub miss_classifications: BTreeMap<String, u64>,
    pub backend: String,
    pub cache_key: String,
    pub backend_cache_key: Option<String>,
    pub backend_cache_version: Option<String>,
    pub archive_path: Option<NormalizedPath>,
    pub journal_log_path: Option<NormalizedPath>,
    pub target_artifact_effectiveness: RustPlanArtifactEffectiveness,
    pub compile_cache_stats: Option<serde_json::Value>,
}

impl Serialize for RustPlanSummary {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let miss_classifications = self.computed_miss_classifications();
        let mut state = serializer.serialize_struct("RustPlanSummary", 22)?;
        state.serialize_field("operation", &self.operation)?;
        state.serialize_field("mode", &self.mode)?;
        state.serialize_field("plan_schema_version", &self.plan_schema_version)?;
        state.serialize_field("cache_schema_version", &self.cache_schema_version)?;
        state.serialize_field("compatibility", &self.compatibility)?;
        state.serialize_field("restored_file_count", &self.restored_file_count)?;
        state.serialize_field("restored_bytes", &self.restored_bytes)?;
        state.serialize_field("saved_file_count", &self.saved_file_count)?;
        state.serialize_field("saved_bytes", &self.saved_bytes)?;
        state.serialize_field("skipped_count", &self.skipped_count)?;
        state.serialize_field("skipped_reasons", &self.skipped_reasons)?;
        state.serialize_field("skipped_samples", &self.skipped_samples)?;
        state.serialize_field("key_input_mismatches", &self.key_input_mismatches)?;
        state.serialize_field("miss_classifications", &miss_classifications)?;
        state.serialize_field("backend", &self.backend)?;
        state.serialize_field("cache_key", &self.cache_key)?;
        state.serialize_field("backend_cache_key", &self.backend_cache_key)?;
        state.serialize_field("backend_cache_version", &self.backend_cache_version)?;
        state.serialize_field("archive_path", &self.archive_path)?;
        state.serialize_field("journal_log_path", &self.journal_log_path)?;
        state.serialize_field(
            "target_artifact_effectiveness",
            &self.target_artifact_effectiveness,
        )?;
        state.serialize_field("compile_cache_stats", &self.compile_cache_stats)?;
        state.end()
    }
}

impl RustPlanSummary {
    /// Create a validation-only success summary.
    #[must_use]
    pub fn validation_success(plan: &RustArtifactPlanV1, cache_dir: &Path) -> Self {
        let cache_key = rust_plan_cache_key(plan);
        let archive_path = rust_plan_bundle_dir(cache_dir, &cache_key);
        Self::new(
            RustPlanOperation::Validate,
            plan.mode,
            plan.schema_version,
            plan.cache_schema_version,
            cache_key,
            Some(archive_path),
            plan.journal_log_path.clone(),
        )
    }

    /// Create a compatibility failure summary for JSON CLI output.
    #[must_use]
    pub fn compatibility_failure(operation: RustPlanOperation, err: &RustPlanError) -> Self {
        Self {
            operation,
            mode: RustPlanMode::Thin,
            plan_schema_version: 0,
            cache_schema_version: 0,
            compatibility: RustPlanCompatibility {
                status: "error".to_string(),
                errors: vec![err.to_string()],
            },
            restored_file_count: 0,
            restored_bytes: 0,
            saved_file_count: 0,
            saved_bytes: 0,
            skipped_count: 0,
            skipped_reasons: BTreeMap::new(),
            skipped_samples: Vec::new(),
            key_input_mismatches: Vec::new(),
            miss_classifications: BTreeMap::new(),
            backend: "unknown".to_string(),
            cache_key: String::new(),
            backend_cache_key: None,
            backend_cache_version: None,
            archive_path: None,
            journal_log_path: None,
            target_artifact_effectiveness: RustPlanArtifactEffectiveness {
                eligible_file_count: 0,
                restored_file_count: 0,
                reuse_ratio: 0.0,
            },
            compile_cache_stats: None,
        }
    }

    fn new(
        operation: RustPlanOperation,
        mode: RustPlanMode,
        plan_schema_version: u32,
        cache_schema_version: u32,
        cache_key: String,
        archive_path: Option<NormalizedPath>,
        journal_log_path: Option<NormalizedPath>,
    ) -> Self {
        Self {
            operation,
            mode,
            plan_schema_version,
            cache_schema_version,
            compatibility: RustPlanCompatibility {
                status: "ok".to_string(),
                errors: Vec::new(),
            },
            restored_file_count: 0,
            restored_bytes: 0,
            saved_file_count: 0,
            saved_bytes: 0,
            skipped_count: 0,
            skipped_reasons: BTreeMap::new(),
            skipped_samples: Vec::new(),
            key_input_mismatches: Vec::new(),
            miss_classifications: BTreeMap::new(),
            backend: "local".to_string(),
            cache_key,
            backend_cache_key: None,
            backend_cache_version: None,
            archive_path,
            journal_log_path,
            target_artifact_effectiveness: RustPlanArtifactEffectiveness {
                eligible_file_count: 0,
                restored_file_count: 0,
                reuse_ratio: 0.0,
            },
            compile_cache_stats: None,
        }
    }

    fn skip(&mut self, path: impl Into<String>, reason: &'static str) {
        self.skipped_count += 1;
        *self.skipped_reasons.entry(reason.to_string()).or_insert(0) += 1;
        if self.skipped_samples.len() < 16 {
            self.skipped_samples.push(RustPlanSkippedSample {
                path: path.into(),
                reason: reason.to_string(),
            });
        }
        self.refresh_miss_classifications();
    }

    /// Record a skipped artifact or backend miss in an operation summary.
    pub fn record_skip(&mut self, path: impl Into<String>, reason: &'static str) {
        self.skip(path, reason);
    }

    /// Set the backend identity fields in an operation summary.
    pub fn set_backend(
        &mut self,
        backend: impl Into<String>,
        backend_cache_key: Option<String>,
        backend_cache_version: Option<String>,
    ) {
        self.backend = backend.into();
        self.backend_cache_key = backend_cache_key;
        self.backend_cache_version = backend_cache_version;
    }

    fn refresh_effectiveness(&mut self, eligible: u64) {
        self.target_artifact_effectiveness.eligible_file_count = eligible;
        self.target_artifact_effectiveness.restored_file_count = self.restored_file_count;
        self.target_artifact_effectiveness.reuse_ratio = if eligible == 0 {
            0.0
        } else {
            self.restored_file_count as f64 / eligible as f64
        };
        self.refresh_miss_classifications();
    }

    /// Recompute low-reuse classifications from already-recorded diagnostics.
    pub fn refresh_miss_classifications(&mut self) {
        self.miss_classifications = self.computed_miss_classifications();
    }

    #[must_use]
    pub fn computed_miss_classifications(&self) -> BTreeMap<String, u64> {
        let mut classifications = BTreeMap::new();

        for (reason, count) in &self.skipped_reasons {
            if let Some(classification) = skip_reason_miss_classification(reason) {
                add_miss_classification(&mut classifications, classification, *count);
            }
        }

        for mismatch in &self.key_input_mismatches {
            for classification in key_mismatch_classifications(mismatch) {
                add_miss_classification(&mut classifications, classification, 1);
            }
        }

        if let Some(stats) = &self.compile_cache_stats {
            let misses = compile_cache_misses(stats);
            if misses > 0 {
                add_miss_classification(
                    &mut classifications,
                    "zccache_compile_cache_miss_despite_equivalent_rustc_command",
                    misses,
                );
            }
        }

        classifications
    }
}

fn add_miss_classification(
    classifications: &mut BTreeMap<String, u64>,
    classification: &'static str,
    count: u64,
) {
    *classifications
        .entry(classification.to_string())
        .or_insert(0) += count;
}

fn skip_reason_miss_classification(reason: &str) -> Option<&'static str> {
    match reason {
        "artifact_absent_from_restored_plan" => Some("artifact_absent_from_restored_plan"),
        "artifact_class_disallowed_by_plan" => Some("artifact_class_disallowed_by_plan"),
        "workspace_or_path_dependency_excluded_by_plan" => {
            Some("workspace_or_path_dependency_excluded_by_plan")
        }
        "restored_payload_missing_or_corrupt" => Some("restored_payload_missing_or_corrupt"),
        "backend_cache_miss" => Some("backend_cache_miss"),
        _ => None,
    }
}

fn key_mismatch_classifications(mismatch: &str) -> Vec<&'static str> {
    let lower = mismatch.to_ascii_lowercase();
    let mut classifications = Vec::new();

    if lower.contains("cache key")
        || lower.contains("mode")
        || lower.contains("toolchain")
        || lower.contains("profile")
        || lower.contains("rustflags")
        || lower.contains("target")
    {
        classifications.push("toolchain_profile_rustflags_target_mismatch");
    }

    if lower.contains("cache key")
        || lower.contains("input hash")
        || lower.contains("lockfile")
        || lower.contains("config")
        || lower.contains("manifest")
    {
        classifications.push("lockfile_config_manifest_hash_mismatch");
    }

    classifications
}

fn compile_cache_misses(stats: &serde_json::Value) -> u64 {
    stats
        .get("misses")
        .and_then(serde_json::Value::as_u64)
        .or_else(|| {
            stats
                .get("cache_misses")
                .and_then(serde_json::Value::as_u64)
        })
        .unwrap_or(0)
}

/// File stored in a Rust artifact bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RustBundledArtifact {
    pub relative_path: String,
    pub class: RustArtifactClass,
    pub size: u64,
    pub content_hash: String,
    #[serde(default)]
    pub mtime_unix_nanos: u64,
}

/// Layer kind for a Rust artifact bundle manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RustArtifactBundleLayerKind {
    Complete,
    Base,
    Delta,
}

/// Manifest for zccache-owned Rust artifact bundles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RustArtifactBundleManifest {
    pub manifest_schema_version: u32,
    pub plan_schema_version: u32,
    pub cache_schema_version: u32,
    pub mode: RustPlanMode,
    pub cache_key: String,
    pub created_at_secs: u64,
    pub plan_identity_hash: String,
    pub artifacts: Vec<RustBundledArtifact>,
    #[serde(default = "default_bundle_layer_kind")]
    pub layer_kind: RustArtifactBundleLayerKind,
    #[serde(default)]
    pub base_cache_key: Option<String>,
    #[serde(default)]
    pub deleted_paths: Vec<String>,
}

fn default_bundle_layer_kind() -> RustArtifactBundleLayerKind {
    RustArtifactBundleLayerKind::Complete
}

#[allow(clippy::derive_partial_eq_without_eq)]
mod rust_plan_proto {
    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct RustArtifactPlanV1 {
        #[prost(uint32, tag = "1")]
        pub schema_version: u32,
        #[prost(uint32, tag = "2")]
        pub mode: u32,
        #[prost(string, tag = "3")]
        pub workspace_root: String,
        #[prost(string, tag = "4")]
        pub target_dir: String,
        #[prost(message, optional, tag = "5")]
        pub toolchain: Option<RustToolchainIdentity>,
        #[prost(string, tag = "6")]
        pub target_triple: String,
        #[prost(string, tag = "7")]
        pub profile: String,
        #[prost(message, optional, tag = "8")]
        pub inputs: Option<RustPlanInputs>,
        #[prost(message, optional, tag = "9")]
        pub packages: Option<RustPlanPackages>,
        #[prost(uint32, repeated, tag = "10")]
        pub allowed_artifact_classes: Vec<u32>,
        #[prost(uint32, tag = "11")]
        pub cache_schema_version: u32,
        #[prost(string, tag = "12")]
        pub journal_log_path: String,
        #[prost(string, tag = "13")]
        pub cache_profile: String,
        #[prost(uint32, repeated, tag = "14")]
        pub dropped_artifact_classes: Vec<u32>,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct RustToolchainIdentity {
        #[prost(string, tag = "1")]
        pub rustc: String,
        #[prost(string, tag = "2")]
        pub cargo: String,
        #[prost(string, tag = "3")]
        pub channel: String,
        #[prost(string, tag = "4")]
        pub host: String,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct RustPlanInputs {
        #[prost(string, tag = "1")]
        pub features_hash: String,
        #[prost(string, tag = "2")]
        pub rustflags_hash: String,
        #[prost(string, tag = "3")]
        pub env_hash: String,
        #[prost(string, tag = "4")]
        pub lockfile_hash: String,
        #[prost(string, tag = "5")]
        pub cargo_config_hash: String,
        #[prost(string, repeated, tag = "6")]
        pub manifest_hashes: Vec<String>,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct RustPlanPackages {
        #[prost(string, repeated, tag = "1")]
        pub selected_package_ids: Vec<String>,
        #[prost(string, repeated, tag = "2")]
        pub workspace_package_ids: Vec<String>,
        #[prost(string, repeated, tag = "3")]
        pub excluded_path_package_ids: Vec<String>,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct RustArtifactBundleManifest {
        #[prost(uint32, tag = "1")]
        pub manifest_schema_version: u32,
        #[prost(uint32, tag = "2")]
        pub plan_schema_version: u32,
        #[prost(uint32, tag = "3")]
        pub cache_schema_version: u32,
        #[prost(uint32, tag = "4")]
        pub mode: u32,
        #[prost(string, tag = "5")]
        pub cache_key: String,
        #[prost(uint64, tag = "6")]
        pub created_at_secs: u64,
        #[prost(string, tag = "7")]
        pub plan_identity_hash: String,
        #[prost(message, repeated, tag = "8")]
        pub artifacts: Vec<RustBundledArtifact>,
        #[prost(uint32, tag = "9")]
        pub layer_kind: u32,
        #[prost(string, tag = "10")]
        pub base_cache_key: String,
        #[prost(string, repeated, tag = "11")]
        pub deleted_paths: Vec<String>,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct RustBundledArtifact {
        #[prost(string, tag = "1")]
        pub relative_path: String,
        #[prost(uint32, tag = "2")]
        pub class: u32,
        #[prost(uint64, tag = "3")]
        pub size: u64,
        #[prost(string, tag = "4")]
        pub content_hash: String,
        #[prost(uint64, tag = "5")]
        pub mtime_unix_nanos: u64,
    }
}

fn plan_mode_to_proto(mode: RustPlanMode) -> u32 {
    match mode {
        RustPlanMode::Thin => 1,
        RustPlanMode::Full => 2,
    }
}

fn plan_mode_from_proto(value: u32) -> Result<RustPlanMode, RustPlanError> {
    match value {
        1 => Ok(RustPlanMode::Thin),
        2 => Ok(RustPlanMode::Full),
        other => Err(RustPlanError::InvalidManifest(format!(
            "unknown plan mode code {other}"
        ))),
    }
}

fn artifact_class_to_proto(class: RustArtifactClass) -> u32 {
    match class {
        RustArtifactClass::Rlib => 1,
        RustArtifactClass::Rmeta => 2,
        RustArtifactClass::DepInfo => 3,
        RustArtifactClass::ProcMacro => 4,
        RustArtifactClass::SharedLib => 5,
        RustArtifactClass::CargoFingerprint => 6,
        RustArtifactClass::CargoFingerprintMeta => 7,
        RustArtifactClass::CargoFingerprintOutputs => 8,
        RustArtifactClass::BuildScriptMetadata => 9,
        RustArtifactClass::BuildScriptOutput => 10,
        RustArtifactClass::BuildScriptBuild => 11,
        RustArtifactClass::Incremental => 12,
        RustArtifactClass::Dwo => 13,
        RustArtifactClass::Pdb => 14,
        RustArtifactClass::Dsym => 15,
        RustArtifactClass::FullTarget => 16,
    }
}

fn artifact_class_from_proto(value: u32) -> Result<RustArtifactClass, RustPlanError> {
    match value {
        1 => Ok(RustArtifactClass::Rlib),
        2 => Ok(RustArtifactClass::Rmeta),
        3 => Ok(RustArtifactClass::DepInfo),
        4 => Ok(RustArtifactClass::ProcMacro),
        5 => Ok(RustArtifactClass::SharedLib),
        6 => Ok(RustArtifactClass::CargoFingerprint),
        7 => Ok(RustArtifactClass::CargoFingerprintMeta),
        8 => Ok(RustArtifactClass::CargoFingerprintOutputs),
        9 => Ok(RustArtifactClass::BuildScriptMetadata),
        10 => Ok(RustArtifactClass::BuildScriptOutput),
        11 => Ok(RustArtifactClass::BuildScriptBuild),
        12 => Ok(RustArtifactClass::Incremental),
        13 => Ok(RustArtifactClass::Dwo),
        14 => Ok(RustArtifactClass::Pdb),
        15 => Ok(RustArtifactClass::Dsym),
        16 => Ok(RustArtifactClass::FullTarget),
        other => Err(RustPlanError::InvalidManifest(format!(
            "unknown artifact class code {other}"
        ))),
    }
}

fn layer_kind_to_proto(kind: RustArtifactBundleLayerKind) -> u32 {
    match kind {
        RustArtifactBundleLayerKind::Complete => 0,
        RustArtifactBundleLayerKind::Base => 1,
        RustArtifactBundleLayerKind::Delta => 2,
    }
}

fn layer_kind_from_proto(value: u32) -> Result<RustArtifactBundleLayerKind, RustPlanError> {
    match value {
        0 => Ok(RustArtifactBundleLayerKind::Complete),
        1 => Ok(RustArtifactBundleLayerKind::Base),
        2 => Ok(RustArtifactBundleLayerKind::Delta),
        other => Err(RustPlanError::InvalidManifest(format!(
            "unknown layer kind code {other}"
        ))),
    }
}

fn normalized_path_to_proto(path: &NormalizedPath) -> String {
    path.as_path().to_string_lossy().into_owned()
}

fn optional_normalized_path_to_proto(path: &Option<NormalizedPath>) -> String {
    path.as_ref()
        .map(normalized_path_to_proto)
        .unwrap_or_default()
}

fn optional_normalized_path_from_proto(raw: String) -> Option<NormalizedPath> {
    if raw.is_empty() {
        None
    } else {
        Some(NormalizedPath::new(raw))
    }
}

fn optional_string_from_proto(raw: String) -> Option<String> {
    if raw.is_empty() {
        None
    } else {
        Some(raw)
    }
}

fn plan_to_proto(plan: &RustArtifactPlanV1) -> rust_plan_proto::RustArtifactPlanV1 {
    rust_plan_proto::RustArtifactPlanV1 {
        schema_version: plan.schema_version,
        mode: plan_mode_to_proto(plan.mode),
        workspace_root: normalized_path_to_proto(&plan.workspace_root),
        target_dir: normalized_path_to_proto(&plan.target_dir),
        toolchain: Some(rust_plan_proto::RustToolchainIdentity {
            rustc: plan.toolchain.rustc.clone(),
            cargo: plan.toolchain.cargo.clone(),
            channel: plan.toolchain.channel.clone(),
            host: plan.toolchain.host.clone(),
        }),
        target_triple: plan.target_triple.clone(),
        profile: plan.profile.clone(),
        inputs: Some(rust_plan_proto::RustPlanInputs {
            features_hash: plan.inputs.features_hash.clone(),
            rustflags_hash: plan.inputs.rustflags_hash.clone(),
            env_hash: plan.inputs.env_hash.clone(),
            lockfile_hash: plan.inputs.lockfile_hash.clone(),
            cargo_config_hash: plan.inputs.cargo_config_hash.clone(),
            manifest_hashes: plan.inputs.manifest_hashes.clone(),
        }),
        packages: Some(rust_plan_proto::RustPlanPackages {
            selected_package_ids: plan.packages.selected_package_ids.clone(),
            workspace_package_ids: plan.packages.workspace_package_ids.clone(),
            excluded_path_package_ids: plan.packages.excluded_path_package_ids.clone(),
        }),
        allowed_artifact_classes: plan
            .allowed_artifact_classes
            .iter()
            .copied()
            .map(artifact_class_to_proto)
            .collect(),
        cache_schema_version: plan.cache_schema_version,
        journal_log_path: optional_normalized_path_to_proto(&plan.journal_log_path),
        cache_profile: plan.cache_profile.clone().unwrap_or_default(),
        dropped_artifact_classes: plan
            .dropped_artifact_classes
            .iter()
            .copied()
            .map(artifact_class_to_proto)
            .collect(),
    }
}

fn plan_from_proto(
    proto: rust_plan_proto::RustArtifactPlanV1,
) -> Result<RustArtifactPlanV1, RustPlanError> {
    if proto.schema_version != RUST_ARTIFACT_PLAN_SCHEMA_VERSION {
        return Err(RustPlanError::UnsupportedSchemaVersion {
            found: proto.schema_version,
            supported: RUST_ARTIFACT_PLAN_SCHEMA_VERSION,
        });
    }
    ensure_supported_cache_schema_version(proto.cache_schema_version)?;

    let toolchain = proto
        .toolchain
        .ok_or_else(|| RustPlanError::InvalidPlan("toolchain is required".to_string()))?;
    let inputs = proto
        .inputs
        .ok_or_else(|| RustPlanError::InvalidPlan("inputs are required".to_string()))?;
    let packages = proto.packages.unwrap_or_default();
    let plan = RustArtifactPlanV1 {
        schema_version: proto.schema_version,
        mode: plan_mode_from_proto(proto.mode)?,
        workspace_root: NormalizedPath::new(proto.workspace_root),
        target_dir: NormalizedPath::new(proto.target_dir),
        toolchain: RustToolchainIdentity {
            rustc: toolchain.rustc,
            cargo: toolchain.cargo,
            channel: toolchain.channel,
            host: toolchain.host,
        },
        target_triple: proto.target_triple,
        profile: proto.profile,
        inputs: RustPlanInputs {
            features_hash: inputs.features_hash,
            rustflags_hash: inputs.rustflags_hash,
            env_hash: inputs.env_hash,
            lockfile_hash: inputs.lockfile_hash,
            cargo_config_hash: inputs.cargo_config_hash,
            manifest_hashes: inputs.manifest_hashes,
        },
        packages: RustPlanPackages {
            selected_package_ids: packages.selected_package_ids,
            workspace_package_ids: packages.workspace_package_ids,
            excluded_path_package_ids: packages.excluded_path_package_ids,
        },
        allowed_artifact_classes: proto
            .allowed_artifact_classes
            .into_iter()
            .map(artifact_class_from_proto)
            .collect::<Result<Vec<_>, RustPlanError>>()?,
        cache_schema_version: proto.cache_schema_version,
        journal_log_path: optional_normalized_path_from_proto(proto.journal_log_path),
        cache_profile: optional_string_from_proto(proto.cache_profile),
        dropped_artifact_classes: proto
            .dropped_artifact_classes
            .into_iter()
            .map(artifact_class_from_proto)
            .collect::<Result<Vec<_>, RustPlanError>>()?,
    };
    plan.validate()?;
    Ok(plan)
}

fn manifest_to_proto(
    manifest: &RustArtifactBundleManifest,
) -> rust_plan_proto::RustArtifactBundleManifest {
    rust_plan_proto::RustArtifactBundleManifest {
        manifest_schema_version: manifest.manifest_schema_version,
        plan_schema_version: manifest.plan_schema_version,
        cache_schema_version: manifest.cache_schema_version,
        mode: plan_mode_to_proto(manifest.mode),
        cache_key: manifest.cache_key.clone(),
        created_at_secs: manifest.created_at_secs,
        plan_identity_hash: manifest.plan_identity_hash.clone(),
        artifacts: manifest
            .artifacts
            .iter()
            .map(|artifact| rust_plan_proto::RustBundledArtifact {
                relative_path: artifact.relative_path.clone(),
                class: artifact_class_to_proto(artifact.class),
                size: artifact.size,
                content_hash: artifact.content_hash.clone(),
                mtime_unix_nanos: artifact.mtime_unix_nanos,
            })
            .collect(),
        layer_kind: layer_kind_to_proto(manifest.layer_kind),
        base_cache_key: manifest.base_cache_key.clone().unwrap_or_default(),
        deleted_paths: manifest.deleted_paths.clone(),
    }
}

fn manifest_from_proto(
    proto: rust_plan_proto::RustArtifactBundleManifest,
) -> Result<RustArtifactBundleManifest, RustPlanError> {
    Ok(RustArtifactBundleManifest {
        manifest_schema_version: proto.manifest_schema_version,
        plan_schema_version: proto.plan_schema_version,
        cache_schema_version: proto.cache_schema_version,
        mode: plan_mode_from_proto(proto.mode)?,
        cache_key: proto.cache_key,
        created_at_secs: proto.created_at_secs,
        plan_identity_hash: proto.plan_identity_hash,
        artifacts: proto
            .artifacts
            .into_iter()
            .map(|artifact| {
                Ok(RustBundledArtifact {
                    relative_path: artifact.relative_path,
                    class: artifact_class_from_proto(artifact.class)?,
                    size: artifact.size,
                    content_hash: artifact.content_hash,
                    mtime_unix_nanos: artifact.mtime_unix_nanos,
                })
            })
            .collect::<Result<Vec<_>, RustPlanError>>()?,
        layer_kind: layer_kind_from_proto(proto.layer_kind)?,
        base_cache_key: if proto.base_cache_key.is_empty() {
            None
        } else {
            Some(proto.base_cache_key)
        },
        deleted_paths: proto.deleted_paths,
    })
}

fn write_bundle_manifest(
    bundle_dir: &Path,
    manifest: &RustArtifactBundleManifest,
) -> Result<(), RustPlanError> {
    let mut bytes = Vec::new();
    manifest_to_proto(manifest).encode(&mut bytes)?;
    std::fs::write(bundle_dir.join(BUNDLE_MANIFEST_NAME), bytes)?;
    let legacy_manifest = bundle_dir.join(LEGACY_BUNDLE_MANIFEST_NAME);
    if legacy_manifest.exists() {
        std::fs::remove_file(legacy_manifest)?;
    }
    Ok(())
}

fn read_bundle_manifest(bundle_dir: &Path) -> Result<RustArtifactBundleManifest, RustPlanError> {
    let manifest_path = bundle_dir.join(BUNDLE_MANIFEST_NAME);
    if manifest_path.exists() {
        let proto = rust_plan_proto::RustArtifactBundleManifest::decode(
            std::fs::read(manifest_path)?.as_slice(),
        )?;
        return manifest_from_proto(proto);
    }

    let legacy_manifest_path = bundle_dir.join(LEGACY_BUNDLE_MANIFEST_NAME);
    let manifest: RustArtifactBundleManifest =
        serde_json::from_slice(&std::fs::read(legacy_manifest_path)?)?;
    Ok(manifest)
}

/// Compute the stable cache key for a plan.
#[must_use]
pub fn rust_plan_cache_key(plan: &RustArtifactPlanV1) -> String {
    let identity = rust_plan_identity_hash(plan);
    format!("rust-plan-v1-{}", &identity[..32])
}

/// Compute the stable identity hash used by manifests.
///
/// The hash folds in `cache_profile` and `dropped_artifact_classes` (added
/// in soldr#461) so a thin-v1 bundle and a thin-v2 bundle for the same
/// otherwise-identical inputs get different keys and never alias each
/// other — they ship different file sets and would corrupt each other's
/// restore expectations if the cache_key collided.
#[must_use]
pub fn rust_plan_identity_hash(plan: &RustArtifactPlanV1) -> String {
    let mut dropped: Vec<RustArtifactClass> = plan.dropped_artifact_classes.clone();
    dropped.sort();
    dropped.dedup();
    let payload = serde_json::json!({
        "schema_version": plan.schema_version,
        "mode": plan.mode,
        "workspace_root": normalize_for_key(plan.workspace_root.as_path()),
        "target_dir": normalize_for_key(plan.target_dir.as_path()),
        "toolchain": plan.toolchain,
        "target_triple": plan.target_triple,
        "profile": plan.profile,
        "inputs": plan.inputs,
        "packages": plan.packages,
        "allowed_artifact_classes": plan.effective_allowed_classes().into_iter().collect::<Vec<_>>(),
        "cache_schema_version": plan.cache_schema_version,
        "cache_profile": plan.cache_profile,
        "dropped_artifact_classes": dropped,
    });
    let bytes = serde_json::to_vec(&payload).unwrap_or_default();
    crate::hash::hash_bytes(&bytes).to_hex()
}

/// Bundle directory for a plan cache key.
#[must_use]
pub fn rust_plan_bundle_dir(cache_dir: &Path, cache_key: &str) -> NormalizedPath {
    NormalizedPath::new(cache_dir.join("rust-plan").join(cache_key))
}

/// Execute local bundle save for a validated plan.
pub fn save_rust_plan_local(
    plan: &RustArtifactPlanV1,
    cache_dir: &Path,
) -> Result<RustPlanSummary, RustPlanError> {
    plan.validate()?;
    ensure_supported_cache_schema_version(plan.cache_schema_version)?;
    let cache_key = rust_plan_cache_key(plan);
    let bundle_dir = rust_plan_bundle_dir(cache_dir, &cache_key);
    let files_dir = bundle_dir.join(BUNDLE_FILES_DIR);
    let mut summary = RustPlanSummary::new(
        RustPlanOperation::Save,
        plan.mode,
        plan.schema_version,
        plan.cache_schema_version,
        cache_key.clone(),
        Some(bundle_dir.clone()),
        plan.journal_log_path.clone(),
    );

    let mut candidates = Vec::new();
    collect_files(plan.target_dir.as_path(), &mut candidates)?;
    candidates.sort();

    let selected = select_artifacts(plan, candidates, &mut summary);

    if bundle_dir.exists() {
        std::fs::remove_dir_all(&bundle_dir)?;
    }
    std::fs::create_dir_all(&files_dir)?;

    let artifacts = bundle_selected_artifacts(&selected, &files_dir)?;
    summary.saved_file_count += artifacts.len() as u64;
    summary.saved_bytes += artifacts.iter().map(|a| a.size).sum::<u64>();

    let manifest = RustArtifactBundleManifest {
        manifest_schema_version: RUST_ARTIFACT_CACHE_SCHEMA_VERSION,
        plan_schema_version: plan.schema_version,
        cache_schema_version: plan.cache_schema_version,
        mode: plan.mode,
        cache_key,
        created_at_secs: now_secs(),
        plan_identity_hash: rust_plan_identity_hash(plan),
        artifacts,
        layer_kind: RustArtifactBundleLayerKind::Complete,
        base_cache_key: None,
        deleted_paths: Vec::new(),
    };
    write_bundle_manifest(&bundle_dir, &manifest)?;
    Ok(summary)
}

/// Copy + stat + hash every selected artifact into `files_dir`, returning the
/// manifest entries in input order (which `select_artifacts` has already sorted
/// by `relative_path` for determinism). Reads `resolve_rust_plan_tar_threads()`
/// for the parallelism setting — see issue #177 for the Windows-CI motivation.
fn bundle_selected_artifacts(
    selected: &[SelectedArtifact],
    files_dir: &Path,
) -> Result<Vec<RustBundledArtifact>, RustPlanError> {
    bundle_selected_artifacts_with_threads(selected, files_dir, resolve_rust_plan_tar_threads())
}

/// Same as `bundle_selected_artifacts`, but with `threads` injected so tests
/// can exercise the parallel path without racing on process-global env vars.
fn bundle_selected_artifacts_with_threads(
    selected: &[SelectedArtifact],
    files_dir: &Path,
    threads: usize,
) -> Result<Vec<RustBundledArtifact>, RustPlanError> {
    if threads <= 1 || selected.len() < 2 {
        return selected
            .iter()
            .map(|sel| bundle_one_artifact(sel, files_dir))
            .collect();
    }

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|idx| format!("zccache-rust-plan-{idx}"))
        .build()
        .map_err(|err| {
            RustPlanError::Io(std::io::Error::other(format!(
                "failed to build rust-plan thread pool: {err}"
            )))
        })?;

    pool.install(|| {
        selected
            .par_iter()
            .map(|sel| bundle_one_artifact(sel, files_dir))
            .collect()
    })
}

fn bundle_one_artifact(
    sel: &SelectedArtifact,
    files_dir: &Path,
) -> Result<RustBundledArtifact, RustPlanError> {
    let dst = safe_join(files_dir, &sel.relative_path)?;
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(&sel.source_path, &dst)?;
    snapshot_selected_artifact(sel)
}

fn snapshot_selected_artifact(
    sel: &SelectedArtifact,
) -> Result<RustBundledArtifact, RustPlanError> {
    let metadata = std::fs::metadata(&sel.source_path)?;
    let size = metadata.len();
    let content_hash = crate::hash::hash_file(&sel.source_path)?.to_hex();
    Ok(RustBundledArtifact {
        relative_path: sel.relative_path.clone(),
        class: sel.class,
        size,
        content_hash,
        mtime_unix_nanos: metadata
            .modified()
            .ok()
            .map(system_time_to_unix_nanos)
            .unwrap_or(0),
    })
}

/// Decide how many worker threads to use for the rust-plan save copy+hash loop.
///
/// Grammar (mirrors `SOLDR_TARGET_CACHE_TAR_THREADS` validated upstream by
/// soldr#273):
/// - unset / `auto` / empty / unparseable → vCPU-bounded, capped at 8
/// - `1` → sequential (regression escape hatch)
/// - positive integer N → `min(N, MAX_RUST_PLAN_TAR_THREADS)`
///
/// `ZCCACHE_RUST_PLAN_TAR_THREADS` takes precedence over the soldr-side var so
/// direct zccache invocations can override without touching the soldr env.
pub fn resolve_rust_plan_tar_threads() -> usize {
    let raw = std::env::var("ZCCACHE_RUST_PLAN_TAR_THREADS")
        .ok()
        .or_else(|| std::env::var("SOLDR_TARGET_CACHE_TAR_THREADS").ok());
    parse_rust_plan_tar_threads(raw.as_deref())
}

fn parse_rust_plan_tar_threads(raw: Option<&str>) -> usize {
    let trimmed = raw.map(str::trim).filter(|s| !s.is_empty());
    match trimmed {
        None => default_rust_plan_tar_threads(),
        Some(s) if s.eq_ignore_ascii_case("auto") => default_rust_plan_tar_threads(),
        Some(s) => match s.parse::<usize>() {
            Ok(0) => default_rust_plan_tar_threads(),
            Ok(n) => n.min(MAX_RUST_PLAN_TAR_THREADS),
            Err(_) => default_rust_plan_tar_threads(),
        },
    }
}

fn default_rust_plan_tar_threads() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
        .min(DEFAULT_RUST_PLAN_TAR_THREADS_CAP)
}

/// Execute local bundle restore for a validated plan.
pub fn restore_rust_plan_local(
    plan: &RustArtifactPlanV1,
    cache_dir: &Path,
) -> Result<RustPlanSummary, RustPlanError> {
    plan.validate()?;
    ensure_supported_cache_schema_version(plan.cache_schema_version)?;
    let cache_key = rust_plan_cache_key(plan);
    let bundle_dir = rust_plan_bundle_dir(cache_dir, &cache_key);
    let mut summary = RustPlanSummary::new(
        RustPlanOperation::Restore,
        plan.mode,
        plan.schema_version,
        plan.cache_schema_version,
        cache_key.clone(),
        Some(bundle_dir.clone()),
        plan.journal_log_path.clone(),
    );

    if !bundle_dir.exists() {
        summary.skip("<bundle>", "artifact_absent_from_restored_plan");
        summary.refresh_effectiveness(0);
        return Ok(summary);
    }

    let manifest = read_bundle_manifest(&bundle_dir)?;
    if !validate_manifest(plan, &cache_key, &manifest, &mut summary)? {
        summary.refresh_effectiveness(0);
        return Ok(summary);
    }

    let eligible = manifest.artifacts.len() as u64;
    restore_manifest_artifacts(plan, &bundle_dir, &manifest, &mut summary)?;

    summary.refresh_effectiveness(eligible);
    Ok(summary)
}

fn restore_manifest_artifacts(
    plan: &RustArtifactPlanV1,
    bundle_dir: &Path,
    manifest: &RustArtifactBundleManifest,
    summary: &mut RustPlanSummary,
) -> Result<(), RustPlanError> {
    let files_dir = bundle_dir.join(BUNDLE_FILES_DIR);
    for deleted_path in &manifest.deleted_paths {
        let dst = match safe_join(plan.target_dir.as_path(), deleted_path) {
            Ok(path) => path,
            Err(err) => {
                summary.skip(deleted_path, "path_traversal");
                summary.compatibility.errors.push(err.to_string());
                continue;
            }
        };
        if dst.exists() {
            std::fs::remove_file(&dst)?;
        }
    }

    for artifact in &manifest.artifacts {
        let src = match safe_join(&files_dir, &artifact.relative_path) {
            Ok(path) => path,
            Err(err) => {
                summary.skip(&artifact.relative_path, "path_traversal");
                summary.compatibility.errors.push(err.to_string());
                continue;
            }
        };
        let dst = match safe_join(plan.target_dir.as_path(), &artifact.relative_path) {
            Ok(path) => path,
            Err(err) => {
                summary.skip(&artifact.relative_path, "path_traversal");
                summary.compatibility.errors.push(err.to_string());
                continue;
            }
        };
        let Ok(metadata) = std::fs::metadata(&src) else {
            summary.skip(
                &artifact.relative_path,
                "restored_payload_missing_or_corrupt",
            );
            continue;
        };
        if metadata.len() != artifact.size {
            summary.skip(
                &artifact.relative_path,
                "restored_payload_missing_or_corrupt",
            );
            continue;
        }
        let Ok(content_hash) = crate::hash::hash_file(&src).map(|hash| hash.to_hex()) else {
            summary.skip(
                &artifact.relative_path,
                "restored_payload_missing_or_corrupt",
            );
            continue;
        };
        if content_hash != artifact.content_hash {
            summary.skip(
                &artifact.relative_path,
                "restored_payload_missing_or_corrupt",
            );
            continue;
        }
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if dst.exists() {
            std::fs::remove_file(&dst)?;
        }
        if std::fs::hard_link(&src, &dst).is_err() {
            std::fs::copy(&src, &dst)?;
        }
        if let Ok(file) = std::fs::File::open(&dst) {
            let modified = if artifact.mtime_unix_nanos == 0 {
                SystemTime::now()
            } else {
                unix_nanos_to_system_time(artifact.mtime_unix_nanos)
            };
            let file_times = std::fs::FileTimes::new()
                .set_accessed(modified)
                .set_modified(modified);
            let _ = file.set_times(file_times);
        }
        summary.restored_file_count += 1;
        summary.restored_bytes += artifact.size;
    }

    Ok(())
}

fn validate_manifest(
    plan: &RustArtifactPlanV1,
    cache_key: &str,
    manifest: &RustArtifactBundleManifest,
    summary: &mut RustPlanSummary,
) -> Result<bool, RustPlanError> {
    if manifest.manifest_schema_version != RUST_ARTIFACT_CACHE_SCHEMA_VERSION {
        return Err(RustPlanError::UnsupportedCacheSchemaVersion {
            found: manifest.manifest_schema_version,
            supported: RUST_ARTIFACT_CACHE_SCHEMA_VERSION,
        });
    }
    let mut compatible = true;
    if manifest.cache_key != cache_key {
        summary
            .key_input_mismatches
            .push("bundle cache key does not match requested plan".to_string());
        compatible = false;
    }
    if manifest.mode != plan.mode {
        summary
            .key_input_mismatches
            .push("bundle mode does not match requested plan".to_string());
        compatible = false;
    }
    let plan_identity_hash = rust_plan_identity_hash(plan);
    if manifest.plan_identity_hash != plan_identity_hash {
        summary
            .key_input_mismatches
            .push("bundle input hash does not match requested plan".to_string());
        compatible = false;
    }
    if manifest.layer_kind == RustArtifactBundleLayerKind::Delta
        && manifest
            .base_cache_key
            .as_deref()
            .is_some_and(|base_cache_key| base_cache_key != cache_key)
    {
        summary
            .key_input_mismatches
            .push("delta base cache key does not match requested plan".to_string());
        compatible = false;
    }
    if compatible {
        Ok(true)
    } else {
        summary.compatibility.status = "warning".to_string();
        summary.compatibility.errors = summary.key_input_mismatches.clone();
        Ok(false)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SelectedArtifact {
    source_path: NormalizedPath,
    relative_path: String,
    class: RustArtifactClass,
}

/// Save only artifacts that differ from an already-restored base bundle.
pub fn save_rust_plan_delta_local(
    plan: &RustArtifactPlanV1,
    base_cache_dir: &Path,
    delta_cache_dir: &Path,
) -> Result<RustPlanSummary, RustPlanError> {
    plan.validate()?;
    ensure_supported_cache_schema_version(plan.cache_schema_version)?;
    let cache_key = rust_plan_cache_key(plan);
    let base_bundle_dir = rust_plan_bundle_dir(base_cache_dir, &cache_key);
    let delta_bundle_dir = rust_plan_bundle_dir(delta_cache_dir, &cache_key);
    let delta_files_dir = delta_bundle_dir.join(BUNDLE_FILES_DIR);
    let mut summary = RustPlanSummary::new(
        RustPlanOperation::Save,
        plan.mode,
        plan.schema_version,
        plan.cache_schema_version,
        cache_key.clone(),
        Some(delta_bundle_dir.clone()),
        plan.journal_log_path.clone(),
    );

    let base_manifest = if base_bundle_dir.exists() {
        let manifest = read_bundle_manifest(&base_bundle_dir)?;
        if validate_manifest(plan, &cache_key, &manifest, &mut summary)? {
            Some(manifest)
        } else {
            None
        }
    } else {
        summary.record_skip("<base-bundle>", "base_bundle_missing_for_delta");
        None
    };
    let base_artifacts: BTreeMap<String, RustBundledArtifact> = base_manifest
        .as_ref()
        .map(|manifest| {
            manifest
                .artifacts
                .iter()
                .map(|artifact| (artifact.relative_path.clone(), artifact.clone()))
                .collect()
        })
        .unwrap_or_default();

    let mut candidates = Vec::new();
    collect_files(plan.target_dir.as_path(), &mut candidates)?;
    candidates.sort();
    let selected = select_artifacts(plan, candidates, &mut summary);

    if delta_bundle_dir.exists() {
        std::fs::remove_dir_all(&delta_bundle_dir)?;
    }
    std::fs::create_dir_all(&delta_files_dir)?;

    let mut current_paths = BTreeSet::new();
    let mut artifacts = Vec::new();
    for sel in &selected {
        current_paths.insert(sel.relative_path.clone());
        let snapshot = snapshot_selected_artifact(sel)?;
        let unchanged = base_artifacts
            .get(&sel.relative_path)
            .map(|base| {
                base.size == snapshot.size
                    && base.content_hash == snapshot.content_hash
                    && base.mtime_unix_nanos == snapshot.mtime_unix_nanos
            })
            .unwrap_or(false);
        if unchanged {
            continue;
        }
        let dst = safe_join(&delta_files_dir, &sel.relative_path)?;
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&sel.source_path, dst)?;
        artifacts.push(snapshot);
    }

    let deleted_paths = base_artifacts
        .keys()
        .filter(|path| !current_paths.contains(*path))
        .cloned()
        .collect::<Vec<_>>();

    summary.saved_file_count += artifacts.len() as u64;
    summary.saved_bytes += artifacts.iter().map(|artifact| artifact.size).sum::<u64>();

    let manifest = RustArtifactBundleManifest {
        manifest_schema_version: RUST_ARTIFACT_CACHE_SCHEMA_VERSION,
        plan_schema_version: plan.schema_version,
        cache_schema_version: plan.cache_schema_version,
        mode: plan.mode,
        cache_key: cache_key.clone(),
        created_at_secs: now_secs(),
        plan_identity_hash: rust_plan_identity_hash(plan),
        artifacts,
        layer_kind: RustArtifactBundleLayerKind::Delta,
        base_cache_key: Some(cache_key),
        deleted_paths,
    };
    write_bundle_manifest(&delta_bundle_dir, &manifest)?;
    Ok(summary)
}

/// Restore a base bundle and then overlay a delta bundle.
pub fn restore_rust_plan_layered_local(
    plan: &RustArtifactPlanV1,
    base_cache_dir: &Path,
    delta_cache_dir: &Path,
) -> Result<RustPlanSummary, RustPlanError> {
    plan.validate()?;
    ensure_supported_cache_schema_version(plan.cache_schema_version)?;
    let cache_key = rust_plan_cache_key(plan);
    let base_bundle_dir = rust_plan_bundle_dir(base_cache_dir, &cache_key);
    let delta_bundle_dir = rust_plan_bundle_dir(delta_cache_dir, &cache_key);
    let mut summary = RustPlanSummary::new(
        RustPlanOperation::Restore,
        plan.mode,
        plan.schema_version,
        plan.cache_schema_version,
        cache_key.clone(),
        Some(delta_bundle_dir.clone()),
        plan.journal_log_path.clone(),
    );

    let mut eligible = 0_u64;
    if base_bundle_dir.exists() {
        let manifest = read_bundle_manifest(&base_bundle_dir)?;
        if validate_manifest(plan, &cache_key, &manifest, &mut summary)? {
            eligible += manifest.artifacts.len() as u64;
            restore_manifest_artifacts(plan, &base_bundle_dir, &manifest, &mut summary)?;
        }
    } else {
        summary.record_skip("<base-bundle>", "base_bundle_missing_for_layered_restore");
    }

    if delta_bundle_dir.exists() {
        let manifest = read_bundle_manifest(&delta_bundle_dir)?;
        if validate_manifest(plan, &cache_key, &manifest, &mut summary)? {
            eligible += manifest.artifacts.len() as u64;
            restore_manifest_artifacts(plan, &delta_bundle_dir, &manifest, &mut summary)?;
        }
    } else {
        summary.record_skip("<delta-bundle>", "delta_bundle_missing_for_layered_restore");
    }

    summary.refresh_effectiveness(eligible);
    Ok(summary)
}

fn select_artifacts(
    plan: &RustArtifactPlanV1,
    candidates: Vec<NormalizedPath>,
    summary: &mut RustPlanSummary,
) -> Vec<SelectedArtifact> {
    let allowed = plan.effective_allowed_classes();
    let dropped: BTreeSet<RustArtifactClass> =
        plan.dropped_artifact_classes.iter().copied().collect();
    let excluded_names = excluded_package_names(&plan.packages);
    let thin_v2 = plan.cache_profile.as_deref() == Some("thin-v2");
    let mut selected = Vec::new();

    for path in candidates {
        let rel_path = match path.strip_prefix(plan.target_dir.as_path()) {
            Ok(rel) => rel,
            Err(_) => {
                summary.skip(path.display().to_string(), "outside_target_dir");
                continue;
            }
        };
        let rel = relative_path_string(rel_path);

        if has_component(rel_path, "incremental") {
            // Always-transient; reported as `transient_state` for back-compat
            // with existing summary consumers regardless of whether thin-v2
            // also listed `Incremental` in `dropped_artifact_classes`.
            summary.skip(rel, "transient_state");
            continue;
        }

        let class = classify_artifact(rel_path, plan.mode, thin_v2);

        if plan.mode == RustPlanMode::Thin {
            let Some(class) = class else {
                summary.skip(rel, "artifact_class_disallowed_by_plan");
                continue;
            };
            // soldr#461: honor the drop list before consulting the allow list.
            // A file matching any dropped class is skipped even if its class is
            // also listed under `allowed_artifact_classes`. This is the
            // load-bearing change that lets thin-v2 actually prune the bundle.
            if dropped.contains(&class) {
                summary.skip(rel, "artifact_class_disallowed_by_plan");
                continue;
            }
            if !allowed.contains(&class) {
                summary.skip(rel, "artifact_class_disallowed_by_plan");
                continue;
            }
            if artifact_matches_excluded_package(rel_path, &excluded_names) {
                summary.skip(rel, "workspace_or_path_dependency_excluded_by_plan");
                continue;
            }
            selected.push(SelectedArtifact {
                source_path: path,
                relative_path: rel,
                class,
            });
            continue;
        }

        selected.push(SelectedArtifact {
            source_path: path,
            relative_path: rel,
            class: class.unwrap_or(RustArtifactClass::FullTarget),
        });
    }

    selected.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    selected
}

fn classify_artifact(rel: &Path, mode: RustPlanMode, thin_v2: bool) -> Option<RustArtifactClass> {
    // .dSYM/ is a directory bundle on macOS; every file *inside* an enclosing
    // `*.dSYM` ancestor component is dsym. Check first so we don't try to
    // classify `Contents/Info.plist` etc. by extension.
    if path_has_dsym_ancestor(rel) {
        return Some(RustArtifactClass::Dsym);
    }

    if has_component(rel, ".fingerprint") {
        // thin-v2 splits the legacy umbrella into Meta (kept) vs Outputs
        // (dropped). Older plans keep the legacy single-class behavior so
        // existing thin-v1 callers see no semantic change.
        if thin_v2 {
            if is_fingerprint_meta_file(rel) {
                return Some(RustArtifactClass::CargoFingerprintMeta);
            }
            return Some(RustArtifactClass::CargoFingerprintOutputs);
        }
        return Some(RustArtifactClass::CargoFingerprint);
    }
    if has_component(rel, "build") {
        if has_component(rel, "out") {
            return Some(RustArtifactClass::BuildScriptOutput);
        }
        if let Some(name) = rel.file_name().and_then(OsStr::to_str) {
            if matches!(name, "output" | "invoked.timestamp" | "root-output") {
                return Some(RustArtifactClass::BuildScriptMetadata);
            }
            // soldr#461: name the compiled build-script binaries so the
            // drop list can reach them. Cargo emits them as
            // `target/<profile>/build/<crate>-<hash>/build-script-build`
            // (possibly with a `.exe` suffix on Windows).
            if is_build_script_build_file(name) {
                return Some(RustArtifactClass::BuildScriptBuild);
            }
        }
    }

    match rel.extension().and_then(OsStr::to_str) {
        Some("rlib") => Some(RustArtifactClass::Rlib),
        Some("rmeta") => Some(RustArtifactClass::Rmeta),
        Some("d") => Some(RustArtifactClass::DepInfo),
        Some("dwo") if has_component(rel, "deps") => Some(RustArtifactClass::Dwo),
        Some("pdb") if has_component(rel, "deps") => Some(RustArtifactClass::Pdb),
        Some("so" | "dylib" | "dll") if is_likely_proc_macro_dylib(rel) => {
            Some(RustArtifactClass::ProcMacro)
        }
        Some("so" | "dylib" | "dll") => Some(RustArtifactClass::SharedLib),
        _ if mode == RustPlanMode::Full => Some(RustArtifactClass::FullTarget),
        _ => None,
    }
}

/// True when `rel` has any ancestor path component ending in `.dSYM`. The
/// match is case-insensitive on the suffix to tolerate filesystems that
/// preserve the historical mixed case but mount case-folded.
fn path_has_dsym_ancestor(rel: &Path) -> bool {
    rel.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .map(|name| {
                let lower = name.to_ascii_lowercase();
                lower.ends_with(".dsym")
            })
            .unwrap_or(false)
    })
}

/// True for files cargo writes inside `.fingerprint/<crate>-<hash>/` that
/// feed its freshness decision. soldr's `docs/THIN_TARGET_CACHE_PRUNING.md`
/// Section 4.3 enumerates these prefixes. Everything else in the directory
/// (notably the `*.json` debug files) is treated as output.
fn is_fingerprint_meta_file(rel: &Path) -> bool {
    let Some(name) = rel.file_name().and_then(OsStr::to_str) else {
        return false;
    };
    if name == "invoked.timestamp" {
        return true;
    }
    matches!(
        name.split('-').next(),
        Some("dep" | "output" | "lib" | "bin")
    ) && name.contains('-')
}

/// True for cargo's compiled build-script binaries. The base name is
/// `build-script-build`; the `.exe` suffix appears on Windows targets.
fn is_build_script_build_file(name: &str) -> bool {
    let stem = name.strip_suffix(".exe").unwrap_or(name);
    stem == "build-script-build" || stem.starts_with("build-script-build-")
}

fn is_likely_proc_macro_dylib(rel: &Path) -> bool {
    if !has_component(rel, "deps") {
        return false;
    }

    rel.file_stem()
        .and_then(OsStr::to_str)
        .map(|stem| {
            let stem = stem.to_ascii_lowercase();
            stem.contains("proc_macro") || stem.contains("proc-macro")
        })
        .unwrap_or(false)
}

fn collect_files(root: &Path, files: &mut Vec<NormalizedPath>) -> Result<(), RustPlanError> {
    if !root.exists() {
        return Ok(());
    }

    let mut entries = Vec::new();
    for entry in std::fs::read_dir(root)? {
        entries.push(entry?);
    }
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = NormalizedPath::new(entry.path());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_files(path.as_path(), files)?;
        } else if file_type.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

fn safe_join(root: &Path, relative: &str) -> Result<NormalizedPath, RustPlanError> {
    let rel = Path::new(relative);
    if rel.as_os_str().is_empty() {
        return Err(RustPlanError::UnsafeRelativePath(relative.to_string()));
    }
    for component in rel.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(RustPlanError::UnsafeRelativePath(relative.to_string()));
            }
        }
    }
    Ok(NormalizedPath::new(root.join(rel)))
}

fn default_thin_classes() -> BTreeSet<RustArtifactClass> {
    [
        RustArtifactClass::Rlib,
        RustArtifactClass::Rmeta,
        RustArtifactClass::DepInfo,
        RustArtifactClass::ProcMacro,
        RustArtifactClass::SharedLib,
        RustArtifactClass::CargoFingerprint,
        RustArtifactClass::BuildScriptMetadata,
        RustArtifactClass::BuildScriptOutput,
    ]
    .into_iter()
    .collect()
}

fn json_u32_field(value: &serde_json::Value, field: &'static str) -> Result<u32, RustPlanError> {
    let Some(raw) = value.get(field) else {
        return Err(RustPlanError::InvalidPlan(format!("{field} is required")));
    };
    let Some(n) = raw.as_u64() else {
        return Err(RustPlanError::InvalidPlan(format!(
            "{field} must be an unsigned integer"
        )));
    };
    u32::try_from(n).map_err(|_| RustPlanError::InvalidPlan(format!("{field} is too large")))
}

fn ensure_supported_cache_schema_version(cache_schema_version: u32) -> Result<(), RustPlanError> {
    if SUPPORTED_RUST_ARTIFACT_CACHE_SCHEMA_VERSIONS.contains(&cache_schema_version) {
        Ok(())
    } else {
        // Report the most recent supported version in the error message so
        // operators see "expected 2" once thin-v2 lands instead of always
        // seeing the legacy "expected 1". The error type still carries the
        // single canonical version field for compat with existing
        // pattern-matching consumers.
        let supported = SUPPORTED_RUST_ARTIFACT_CACHE_SCHEMA_VERSIONS
            .iter()
            .copied()
            .max()
            .unwrap_or(RUST_ARTIFACT_CACHE_SCHEMA_VERSION);
        Err(RustPlanError::UnsupportedCacheSchemaVersion {
            found: cache_schema_version,
            supported,
        })
    }
}
fn relative_path_string(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            Component::CurDir => None,
            _ => Some(component.as_os_str().to_string_lossy().into_owned()),
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn has_component(path: &Path, needle: &str) -> bool {
    path.components()
        .any(|component| component.as_os_str() == OsStr::new(needle))
}

fn excluded_package_names(packages: &RustPlanPackages) -> BTreeSet<String> {
    packages
        .workspace_package_ids
        .iter()
        .chain(packages.excluded_path_package_ids.iter())
        .filter_map(|id| package_name_from_id(id))
        .collect()
}

fn package_name_from_id(id: &str) -> Option<String> {
    let candidate = if let Some(after_hash) = id.rsplit_once('#').map(|(_, right)| right) {
        after_hash.split('@').next().unwrap_or(after_hash)
    } else if let Some((left, _)) = id.split_once(' ') {
        left
    } else {
        id
    };
    let candidate = candidate
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .replace('-', "_");
    if candidate.is_empty()
        || candidate.contains('/')
        || candidate.contains('\\')
        || candidate.contains(':')
    {
        None
    } else {
        Some(candidate)
    }
}

fn artifact_matches_excluded_package(rel: &Path, excluded_names: &BTreeSet<String>) -> bool {
    if excluded_names.is_empty() {
        return false;
    }
    rel.components().any(|component| {
        let name = component.as_os_str().to_string_lossy();
        excluded_names.iter().any(|package| {
            let without_lib = name.strip_prefix("lib").unwrap_or(&name);
            without_lib == package
                || without_lib.starts_with(&format!("{package}-"))
                || without_lib.starts_with(&format!("{package}."))
        })
    })
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn system_time_to_unix_nanos(time: SystemTime) -> u64 {
    let Ok(duration) = time.duration_since(UNIX_EPOCH) else {
        return 0;
    };
    duration
        .as_secs()
        .saturating_mul(1_000_000_000)
        .saturating_add(u64::from(duration.subsec_nanos()))
}

fn unix_nanos_to_system_time(nanos: u64) -> SystemTime {
    UNIX_EPOCH + std::time::Duration::new(nanos / 1_000_000_000, (nanos % 1_000_000_000) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_plan(root: &Path, mode: RustPlanMode) -> RustArtifactPlanV1 {
        RustArtifactPlanV1 {
            schema_version: 1,
            mode,
            workspace_root: root.into(),
            target_dir: root.join("target").into(),
            toolchain: RustToolchainIdentity {
                rustc: "rustc 1.94.1".to_string(),
                cargo: "cargo 1.94.1".to_string(),
                channel: "1.94.1".to_string(),
                host: "x86_64-pc-windows-msvc".to_string(),
            },
            target_triple: "x86_64-pc-windows-msvc".to_string(),
            profile: "debug".to_string(),
            inputs: RustPlanInputs {
                features_hash: "features".to_string(),
                rustflags_hash: "rustflags".to_string(),
                env_hash: "env".to_string(),
                lockfile_hash: "lock".to_string(),
                cargo_config_hash: "config".to_string(),
                manifest_hashes: vec!["manifest".to_string()],
            },
            packages: RustPlanPackages {
                selected_package_ids: vec!["app 0.1.0".to_string()],
                workspace_package_ids: vec!["app 0.1.0".to_string()],
                excluded_path_package_ids: vec!["local_dep 0.1.0".to_string()],
            },
            allowed_artifact_classes: vec![
                RustArtifactClass::Rlib,
                RustArtifactClass::Rmeta,
                RustArtifactClass::DepInfo,
                RustArtifactClass::CargoFingerprint,
                RustArtifactClass::BuildScriptMetadata,
                RustArtifactClass::BuildScriptOutput,
            ],
            cache_schema_version: 1,
            journal_log_path: Some(root.join("zccache-session.jsonl").into()),
            cache_profile: None,
            dropped_artifact_classes: Vec::new(),
        }
    }

    fn write(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    fn set_mtime_nanos(path: &Path, nanos: u64) {
        let time = unix_nanos_to_system_time(nanos);
        let file_times = std::fs::FileTimes::new()
            .set_accessed(time)
            .set_modified(time);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        file.set_times(file_times).unwrap();
    }

    fn file_mtime_nanos(path: &Path) -> u64 {
        system_time_to_unix_nanos(std::fs::metadata(path).unwrap().modified().unwrap())
    }

    fn load_manifest(bundle_dir: &Path) -> RustArtifactBundleManifest {
        read_bundle_manifest(bundle_dir).unwrap()
    }

    fn write_manifest(bundle_dir: &Path, manifest: &RustArtifactBundleManifest) {
        write_bundle_manifest(bundle_dir, manifest).unwrap();
    }

    fn synthetic_target(root: &Path) {
        let target = root.join("target").join("debug");
        write(
            &target.join("deps").join("libserde-abc.rlib"),
            b"serde rlib",
        );
        write(
            &target.join("deps").join("libserde-abc.rmeta"),
            b"serde rmeta",
        );
        write(&target.join("deps").join("serde-abc.d"), b"serde depinfo");
        write(
            &target.join("deps").join("libapp-abc.rlib"),
            b"workspace rlib",
        );
        write(
            &target.join("deps").join("liblocal_dep-abc.rlib"),
            b"path dep rlib",
        );
        write(
            &target
                .join(".fingerprint")
                .join("serde-abc")
                .join("dep-lib-serde"),
            b"fingerprint",
        );
        write(
            &target
                .join("build")
                .join("serde-abc")
                .join("invoked.timestamp"),
            b"timestamp",
        );
        write(
            &target
                .join("build")
                .join("serde-abc")
                .join("out")
                .join("gen.rs"),
            b"generated",
        );
        write(&target.join("incremental").join("state.bin"), b"transient");
    }

    fn synthetic_target_with_final_binary(root: &Path) {
        synthetic_target(root);
        let target = root.join("target").join("debug");
        #[cfg(windows)]
        write(&target.join("app.exe"), b"final binary");
        #[cfg(not(windows))]
        write(&target.join("app"), b"final binary");
    }

    fn synthetic_target_with_proc_macro_outputs(root: &Path) {
        synthetic_target(root);
        let target = root.join("target").join("debug");
        #[cfg(windows)]
        let proc_macro = target.join("deps").join("libproc_macro2-def456.dll");
        #[cfg(not(windows))]
        let proc_macro = target.join("deps").join("libproc_macro2-def456.so");
        write(&proc_macro, b"proc-macro dylib");

        #[cfg(windows)]
        let shared_lib = target.join("deps").join("libserde_shared-def456.dll");
        #[cfg(not(windows))]
        let shared_lib = target.join("deps").join("libserde_shared-def456.so");
        write(&shared_lib, b"shared lib");
    }

    fn synthetic_target_with_package_exclusions(root: &Path) {
        synthetic_target(root);
        let target = root.join("target").join("debug");
        write(
            &target.join("deps").join("libapp-abc.rmeta"),
            b"workspace rmeta",
        );
        write(&target.join("deps").join("app-abc.d"), b"workspace depinfo");
        write(
            &target.join("deps").join("liblocal_dep-abc.rmeta"),
            b"path dep rmeta",
        );
        write(
            &target.join("deps").join("local_dep-abc.d"),
            b"path dep depinfo",
        );
        write(
            &target
                .join(".fingerprint")
                .join("app-abc")
                .join("dep-lib-app"),
            b"workspace fingerprint",
        );
        write(
            &target
                .join(".fingerprint")
                .join("local_dep-abc")
                .join("dep-lib-local_dep"),
            b"path dep fingerprint",
        );
        write(
            &target
                .join("build")
                .join("app-abc")
                .join("invoked.timestamp"),
            b"workspace timestamp",
        );
        write(
            &target
                .join("build")
                .join("local_dep-abc")
                .join("invoked.timestamp"),
            b"path dep timestamp",
        );
        write(
            &target
                .join("build")
                .join("app-abc")
                .join("out")
                .join("gen.rs"),
            b"workspace generated",
        );
        write(
            &target
                .join("build")
                .join("local_dep-abc")
                .join("out")
                .join("gen.rs"),
            b"path dep generated",
        );
    }

    #[test]
    fn rejects_unsupported_schema_before_deserializing_unknown_fields() {
        let raw = serde_json::json!({
            "schema_version": 99,
            "cache_schema_version": 1,
            "unexpected_future_field": true
        });
        let err = RustArtifactPlanV1::from_json_value(raw).unwrap_err();
        assert!(matches!(
            err,
            RustPlanError::UnsupportedSchemaVersion {
                found: 99,
                supported: 1
            }
        ));
    }

    #[test]
    fn rejects_unsupported_cache_schema_before_filesystem_mutation() {
        let dir = tempfile::tempdir().unwrap();
        synthetic_target(dir.path());
        let plan = RustArtifactPlanV1 {
            cache_schema_version: 99,
            ..sample_plan(dir.path(), RustPlanMode::Thin)
        };
        let cache = dir.path().join("cache");

        let err = save_rust_plan_local(&plan, &cache).unwrap_err();
        // soldr#461: zccache now accepts both v1 (legacy) and v2 (thin-v2)
        // wire schemas; the error reports the most-recent supported version.
        assert!(matches!(
            err,
            RustPlanError::UnsupportedCacheSchemaVersion {
                found: 99,
                supported: 2
            }
        ));
        assert!(!cache.exists());
    }

    #[test]
    fn restore_rejects_unsupported_cache_schema_before_filesystem_mutation() {
        let dir = tempfile::tempdir().unwrap();
        synthetic_target(dir.path());
        let plan = RustArtifactPlanV1 {
            cache_schema_version: 99,
            ..sample_plan(dir.path(), RustPlanMode::Thin)
        };
        let cache = dir.path().join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::create_dir_all(plan.target_dir.as_path()).unwrap();
        let sentinel = plan.target_dir.join("sentinel.txt");
        std::fs::write(&sentinel, b"keep me").unwrap();

        let err = restore_rust_plan_local(&plan, &cache).unwrap_err();
        assert!(matches!(
            err,
            RustPlanError::UnsupportedCacheSchemaVersion {
                found: 99,
                supported: 2
            }
        ));
        assert!(sentinel.exists());
    }

    #[test]
    fn omitted_or_empty_allowed_classes_default_to_thin_classes() {
        let dir = tempfile::tempdir().unwrap();
        let mut plan_value =
            serde_json::to_value(sample_plan(dir.path(), RustPlanMode::Thin)).unwrap();

        plan_value
            .as_object_mut()
            .unwrap()
            .remove("allowed_artifact_classes");
        let omitted = RustArtifactPlanV1::from_json_value(plan_value.clone()).unwrap();
        assert_eq!(omitted.effective_allowed_classes(), default_thin_classes());

        plan_value.as_object_mut().unwrap().insert(
            "allowed_artifact_classes".to_string(),
            serde_json::json!([]),
        );
        let empty = RustArtifactPlanV1::from_json_value(plan_value).unwrap();
        assert_eq!(empty.effective_allowed_classes(), default_thin_classes());
    }

    #[test]
    fn rust_plan_load_accepts_protobuf_plan() {
        let dir = tempfile::tempdir().unwrap();
        let mut plan = sample_plan(dir.path(), RustPlanMode::Thin);
        plan.cache_schema_version = 2;
        plan.cache_profile = Some("thin-v2".to_string());
        plan.dropped_artifact_classes = vec![
            RustArtifactClass::CargoFingerprintOutputs,
            RustArtifactClass::BuildScriptBuild,
            RustArtifactClass::Dwo,
        ];
        let plan_path = dir.path().join("plan.pb");

        std::fs::write(&plan_path, plan.to_proto_bytes().unwrap()).unwrap();
        let loaded = RustArtifactPlanV1::load(&plan_path).unwrap();

        assert_eq!(loaded, plan);
    }

    #[test]
    fn thin_save_restore_selects_dependency_artifacts_and_metadata() {
        let dir = tempfile::tempdir().unwrap();
        synthetic_target(dir.path());
        let plan = sample_plan(dir.path(), RustPlanMode::Thin);
        let cache = dir.path().join("cache");

        let saved = save_rust_plan_local(&plan, &cache).unwrap();
        assert_eq!(saved.saved_file_count, 6);
        assert_eq!(
            saved
                .skipped_reasons
                .get("workspace_or_path_dependency_excluded_by_plan"),
            Some(&2)
        );
        assert_eq!(saved.skipped_reasons.get("transient_state"), Some(&1));

        std::fs::remove_dir_all(plan.target_dir.as_path()).unwrap();
        let restored = restore_rust_plan_local(&plan, &cache).unwrap();
        assert_eq!(restored.restored_file_count, 6);
        assert!(plan
            .target_dir
            .join("debug/deps/libserde-abc.rlib")
            .exists());
        assert!(plan
            .target_dir
            .join("debug/.fingerprint/serde-abc/dep-lib-serde")
            .exists());
        assert!(!plan.target_dir.join("debug/deps/libapp-abc.rlib").exists());
        assert!(!plan.target_dir.join("debug/incremental/state.bin").exists());
    }

    #[test]
    fn save_writes_protobuf_manifest_and_restore_preserves_mtime() {
        let dir = tempfile::tempdir().unwrap();
        synthetic_target(dir.path());
        let plan = sample_plan(dir.path(), RustPlanMode::Thin);
        let cache = dir.path().join("cache");
        let selected_file = plan.target_dir.join("debug/deps/libserde-abc.rlib");
        let expected_mtime = 1_700_000_000_000_000_000;
        set_mtime_nanos(&selected_file, expected_mtime);

        save_rust_plan_local(&plan, &cache).unwrap();
        let bundle_dir = rust_plan_bundle_dir(&cache, &rust_plan_cache_key(&plan));
        assert!(bundle_dir.join(BUNDLE_MANIFEST_NAME).exists());
        assert!(!bundle_dir.join(LEGACY_BUNDLE_MANIFEST_NAME).exists());
        let manifest = load_manifest(&bundle_dir);
        let artifact = manifest
            .artifacts
            .iter()
            .find(|artifact| artifact.relative_path == "debug/deps/libserde-abc.rlib")
            .unwrap();
        assert_eq!(artifact.mtime_unix_nanos, expected_mtime);

        std::fs::remove_dir_all(plan.target_dir.as_path()).unwrap();
        restore_rust_plan_local(&plan, &cache).unwrap();
        assert_eq!(file_mtime_nanos(&selected_file), expected_mtime);
    }

    #[test]
    fn delta_save_and_layered_restore_overlay_base_with_changes_and_tombstones() {
        let dir = tempfile::tempdir().unwrap();
        synthetic_target(dir.path());
        let plan = sample_plan(dir.path(), RustPlanMode::Thin);
        let base_cache = dir.path().join("base-cache");
        let delta_cache = dir.path().join("delta-cache");

        save_rust_plan_local(&plan, &base_cache).unwrap();

        let unchanged_large = plan.target_dir.join("debug/deps/libserde-abc.rlib");
        let changed = plan.target_dir.join("debug/deps/libserde-abc.rmeta");
        let deleted = plan.target_dir.join("debug/deps/serde-abc.d");
        write(&changed, b"serde rmeta changed");
        let changed_mtime = 1_700_000_100_000_000_000;
        set_mtime_nanos(&changed, changed_mtime);
        std::fs::remove_file(&deleted).unwrap();

        let saved_delta = save_rust_plan_delta_local(&plan, &base_cache, &delta_cache).unwrap();
        assert_eq!(saved_delta.saved_file_count, 1);
        let delta_bundle = rust_plan_bundle_dir(&delta_cache, &rust_plan_cache_key(&plan));
        let delta_manifest = load_manifest(&delta_bundle);
        assert_eq!(
            delta_manifest.layer_kind,
            RustArtifactBundleLayerKind::Delta
        );
        assert_eq!(delta_manifest.artifacts.len(), 1);
        assert_eq!(
            delta_manifest.artifacts[0].relative_path,
            "debug/deps/libserde-abc.rmeta"
        );
        assert_eq!(delta_manifest.artifacts[0].mtime_unix_nanos, changed_mtime);
        assert!(delta_manifest
            .deleted_paths
            .contains(&"debug/deps/serde-abc.d".to_string()));
        assert!(!delta_bundle
            .join(BUNDLE_FILES_DIR)
            .join("debug/deps/libserde-abc.rlib")
            .exists());

        std::fs::remove_dir_all(plan.target_dir.as_path()).unwrap();
        let restored = restore_rust_plan_layered_local(&plan, &base_cache, &delta_cache).unwrap();
        assert_eq!(restored.restored_file_count, 7);
        assert_eq!(std::fs::read(&changed).unwrap(), b"serde rmeta changed");
        assert_eq!(file_mtime_nanos(&changed), changed_mtime);
        assert_eq!(std::fs::read(&unchanged_large).unwrap(), b"serde rlib");
        assert!(!deleted.exists());
    }

    #[test]
    fn delta_save_treats_mtime_only_changes_as_different() {
        let dir = tempfile::tempdir().unwrap();
        synthetic_target(dir.path());
        let plan = sample_plan(dir.path(), RustPlanMode::Thin);
        let base_cache = dir.path().join("base-cache");
        let delta_cache = dir.path().join("delta-cache");
        let changed = plan.target_dir.join("debug/deps/libserde-abc.rlib");

        save_rust_plan_local(&plan, &base_cache).unwrap();
        let changed_mtime = 1_700_000_200_000_000_000;
        set_mtime_nanos(&changed, changed_mtime);

        let saved_delta = save_rust_plan_delta_local(&plan, &base_cache, &delta_cache).unwrap();
        assert_eq!(saved_delta.saved_file_count, 1);
        let delta_bundle = rust_plan_bundle_dir(&delta_cache, &rust_plan_cache_key(&plan));
        let delta_manifest = load_manifest(&delta_bundle);
        assert_eq!(
            delta_manifest.artifacts[0].relative_path,
            "debug/deps/libserde-abc.rlib"
        );
        assert_eq!(delta_manifest.artifacts[0].mtime_unix_nanos, changed_mtime);
    }

    #[test]
    fn thin_plan_skips_final_binary_outputs() {
        let dir = tempfile::tempdir().unwrap();
        synthetic_target_with_final_binary(dir.path());
        let plan = sample_plan(dir.path(), RustPlanMode::Thin);
        let cache = dir.path().join("cache");

        let saved = save_rust_plan_local(&plan, &cache).unwrap();
        assert_eq!(saved.saved_file_count, 6);
        assert_eq!(
            saved
                .skipped_reasons
                .get("artifact_class_disallowed_by_plan"),
            Some(&1)
        );
        assert!(saved.skipped_samples.iter().any(|sample| {
            sample.path.ends_with("debug/app.exe") || sample.path.ends_with("debug/app")
        }));

        std::fs::remove_dir_all(plan.target_dir.as_path()).unwrap();
        let restored = restore_rust_plan_local(&plan, &cache).unwrap();
        assert_eq!(restored.restored_file_count, 6);
        assert!(!plan.target_dir.join("debug/app.exe").exists());
        assert!(!plan.target_dir.join("debug/app").exists());
    }

    #[test]
    fn thin_plan_respects_explicit_class_gates_for_dependency_metadata_and_outputs() {
        let dir = tempfile::tempdir().unwrap();
        synthetic_target(dir.path());
        let mut plan = sample_plan(dir.path(), RustPlanMode::Thin);
        plan.allowed_artifact_classes = vec![RustArtifactClass::Rlib, RustArtifactClass::Rmeta];
        let cache = dir.path().join("cache");

        let saved = save_rust_plan_local(&plan, &cache).unwrap();
        assert_eq!(saved.saved_file_count, 2);
        assert_eq!(
            saved
                .skipped_reasons
                .get("artifact_class_disallowed_by_plan"),
            Some(&4)
        );
        assert_eq!(
            saved
                .skipped_reasons
                .get("workspace_or_path_dependency_excluded_by_plan"),
            Some(&2)
        );
        assert!(saved
            .skipped_samples
            .iter()
            .any(|sample| sample.path.ends_with("debug/deps/serde-abc.d")));
        assert!(saved.skipped_samples.iter().any(|sample| sample
            .path
            .ends_with("debug/.fingerprint/serde-abc/dep-lib-serde")));
        assert!(saved.skipped_samples.iter().any(|sample| sample
            .path
            .ends_with("debug/build/serde-abc/invoked.timestamp")));
        assert!(saved
            .skipped_samples
            .iter()
            .any(|sample| sample.path.ends_with("debug/build/serde-abc/out/gen.rs")));
    }

    #[test]
    fn thin_plan_saves_and_restores_likely_proc_macro_dylibs_without_shared_libs() {
        let dir = tempfile::tempdir().unwrap();
        synthetic_target_with_proc_macro_outputs(dir.path());
        let mut plan = sample_plan(dir.path(), RustPlanMode::Thin);
        plan.allowed_artifact_classes = vec![
            RustArtifactClass::Rlib,
            RustArtifactClass::Rmeta,
            RustArtifactClass::DepInfo,
            RustArtifactClass::ProcMacro,
            RustArtifactClass::CargoFingerprint,
            RustArtifactClass::BuildScriptMetadata,
            RustArtifactClass::BuildScriptOutput,
        ];
        let cache = dir.path().join("cache");

        let saved = save_rust_plan_local(&plan, &cache).unwrap();
        assert_eq!(saved.saved_file_count, 7);
        assert_eq!(
            saved
                .skipped_reasons
                .get("artifact_class_disallowed_by_plan"),
            Some(&1)
        );
        assert!(saved.skipped_samples.iter().any(|sample| {
            sample
                .path
                .ends_with("debug/deps/libserde_shared-def456.dll")
                || sample
                    .path
                    .ends_with("debug/deps/libserde_shared-def456.so")
        }));

        std::fs::remove_dir_all(plan.target_dir.as_path()).unwrap();
        let restored = restore_rust_plan_local(&plan, &cache).unwrap();
        assert_eq!(restored.restored_file_count, 7);
        assert!(plan
            .target_dir
            .join(if cfg!(windows) {
                "debug/deps/libproc_macro2-def456.dll"
            } else {
                "debug/deps/libproc_macro2-def456.so"
            })
            .exists());
        assert!(!plan
            .target_dir
            .join(if cfg!(windows) {
                "debug/deps/libserde_shared-def456.dll"
            } else {
                "debug/deps/libserde_shared-def456.so"
            })
            .exists());
    }

    #[test]
    fn thin_plan_skips_likely_proc_macro_dylibs_when_disallowed() {
        let dir = tempfile::tempdir().unwrap();
        synthetic_target_with_proc_macro_outputs(dir.path());
        let mut plan = sample_plan(dir.path(), RustPlanMode::Thin);
        plan.allowed_artifact_classes = vec![
            RustArtifactClass::Rlib,
            RustArtifactClass::Rmeta,
            RustArtifactClass::DepInfo,
            RustArtifactClass::SharedLib,
            RustArtifactClass::CargoFingerprint,
            RustArtifactClass::BuildScriptMetadata,
            RustArtifactClass::BuildScriptOutput,
        ];
        let cache = dir.path().join("cache");

        let saved = save_rust_plan_local(&plan, &cache).unwrap();
        assert_eq!(saved.saved_file_count, 7);
        assert_eq!(
            saved
                .skipped_reasons
                .get("artifact_class_disallowed_by_plan"),
            Some(&1)
        );
        assert!(saved.skipped_samples.iter().any(|sample| {
            sample
                .path
                .ends_with("debug/deps/libproc_macro2-def456.dll")
                || sample.path.ends_with("debug/deps/libproc_macro2-def456.so")
        }));
    }

    #[test]
    fn restore_skips_mismatched_bundles_without_mutating_target_dir() {
        let dir = tempfile::tempdir().unwrap();
        synthetic_target(dir.path());
        let plan = sample_plan(dir.path(), RustPlanMode::Thin);
        let cache = dir.path().join("cache");

        save_rust_plan_local(&plan, &cache).unwrap();
        let bundle_dir = rust_plan_bundle_dir(&cache, &rust_plan_cache_key(&plan));
        let mut manifest = load_manifest(&bundle_dir);
        manifest.cache_key = "rust-plan-v1-deadbeefdeadbeefdeadbeefdeadbeef".to_string();
        manifest.mode = RustPlanMode::Full;
        manifest.plan_identity_hash =
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string();
        write_manifest(&bundle_dir, &manifest);

        std::fs::remove_dir_all(plan.target_dir.as_path()).unwrap();
        std::fs::create_dir_all(plan.target_dir.as_path()).unwrap();

        let restored = restore_rust_plan_local(&plan, &cache).unwrap();
        assert_eq!(restored.restored_file_count, 0);
        assert_eq!(restored.compatibility.status, "warning");
        assert_eq!(restored.key_input_mismatches.len(), 3);
        assert_eq!(
            restored
                .miss_classifications
                .get("toolchain_profile_rustflags_target_mismatch"),
            Some(&2)
        );
        assert_eq!(
            restored
                .miss_classifications
                .get("lockfile_config_manifest_hash_mismatch"),
            Some(&2)
        );
        assert!(std::fs::read_dir(plan.target_dir.as_path())
            .unwrap()
            .next()
            .is_none());
    }

    #[test]
    fn full_save_restore_includes_workspace_outputs_but_prunes_incremental() {
        let dir = tempfile::tempdir().unwrap();
        synthetic_target(dir.path());
        let plan = sample_plan(dir.path(), RustPlanMode::Full);
        let cache = dir.path().join("cache");

        let saved = save_rust_plan_local(&plan, &cache).unwrap();
        assert_eq!(saved.skipped_reasons.get("transient_state"), Some(&1));
        assert!(saved.saved_file_count > 6);

        std::fs::remove_dir_all(plan.target_dir.as_path()).unwrap();
        let restored = restore_rust_plan_local(&plan, &cache).unwrap();
        assert!(restored.restored_file_count > 6);
        assert!(plan.target_dir.join("debug/deps/libapp-abc.rlib").exists());
        assert!(!plan.target_dir.join("debug/incremental/state.bin").exists());
    }

    #[test]
    fn restore_missing_bundle_is_a_diagnostic_cache_miss() {
        let dir = tempfile::tempdir().unwrap();
        let plan = sample_plan(dir.path(), RustPlanMode::Thin);
        let summary = restore_rust_plan_local(&plan, &dir.path().join("cache")).unwrap();
        assert_eq!(summary.backend, "local");
        assert_eq!(summary.restored_file_count, 0);
        assert_eq!(
            summary
                .skipped_reasons
                .get("artifact_absent_from_restored_plan"),
            Some(&1)
        );
        assert_eq!(
            summary
                .miss_classifications
                .get("artifact_absent_from_restored_plan"),
            Some(&1)
        );
    }

    #[test]
    fn summary_records_backend_identity_and_manual_skips() {
        let dir = tempfile::tempdir().unwrap();
        let plan = sample_plan(dir.path(), RustPlanMode::Thin);
        let mut summary = RustPlanSummary::validation_success(&plan, &dir.path().join("cache"));
        assert_eq!(summary.backend, "local");
        assert!(summary.backend_cache_key.is_none());

        summary.set_backend(
            "gha",
            Some("rust-plan-v1-key".to_string()),
            Some("version".to_string()),
        );
        summary.record_skip("<gha-cache>", "backend_cache_miss");

        assert_eq!(summary.backend, "gha");
        assert_eq!(
            summary.backend_cache_key.as_deref(),
            Some("rust-plan-v1-key")
        );
        assert_eq!(summary.backend_cache_version.as_deref(), Some("version"));
        assert_eq!(summary.skipped_reasons.get("backend_cache_miss"), Some(&1));
        assert_eq!(
            summary.miss_classifications.get("backend_cache_miss"),
            Some(&1)
        );
    }

    #[test]
    fn summary_derives_miss_classifications_from_existing_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let plan = sample_plan(dir.path(), RustPlanMode::Thin);
        let mut summary = RustPlanSummary::validation_success(&plan, &dir.path().join("cache"));

        summary.record_skip("debug/deps/app.exe", "artifact_class_disallowed_by_plan");
        summary.record_skip(
            "debug/deps/libapp-abc.rlib",
            "workspace_or_path_dependency_excluded_by_plan",
        );
        summary
            .key_input_mismatches
            .push("bundle mode does not match requested plan".to_string());
        summary
            .key_input_mismatches
            .push("bundle input hash does not match requested plan".to_string());
        summary.compile_cache_stats = Some(serde_json::json!({
            "compilations": 4,
            "hits": 1,
            "misses": 3,
        }));
        summary.refresh_miss_classifications();

        assert_eq!(
            summary
                .miss_classifications
                .get("artifact_class_disallowed_by_plan"),
            Some(&1)
        );
        assert_eq!(
            summary
                .miss_classifications
                .get("workspace_or_path_dependency_excluded_by_plan"),
            Some(&1)
        );
        assert_eq!(
            summary
                .miss_classifications
                .get("toolchain_profile_rustflags_target_mismatch"),
            Some(&1)
        );
        assert_eq!(
            summary
                .miss_classifications
                .get("lockfile_config_manifest_hash_mismatch"),
            Some(&1)
        );
        assert_eq!(
            summary
                .miss_classifications
                .get("zccache_compile_cache_miss_despite_equivalent_rustc_command"),
            Some(&3)
        );
    }

    #[test]
    fn serialized_summary_recomputes_miss_classifications_from_session_stats() {
        let dir = tempfile::tempdir().unwrap();
        let plan = sample_plan(dir.path(), RustPlanMode::Thin);
        let mut summary = RustPlanSummary::validation_success(&plan, &dir.path().join("cache"));
        summary.record_skip("<gha-cache>", "backend_cache_miss");

        summary.compile_cache_stats = Some(serde_json::json!({
            "status": "ok",
            "cache_misses": 2,
        }));

        let json = serde_json::to_value(&summary).unwrap();
        assert_eq!(
            json["miss_classifications"]["backend_cache_miss"].as_u64(),
            Some(1)
        );
        assert_eq!(
            json["miss_classifications"]
                ["zccache_compile_cache_miss_despite_equivalent_rustc_command"]
                .as_u64(),
            Some(2)
        );
    }

    #[test]
    fn restore_skips_missing_wrong_size_and_wrong_hash_payloads() {
        let dir = tempfile::tempdir().unwrap();
        synthetic_target(dir.path());
        let plan = sample_plan(dir.path(), RustPlanMode::Thin);
        let cache = dir.path().join("cache");

        let saved = save_rust_plan_local(&plan, &cache).unwrap();
        assert_eq!(saved.saved_file_count, 6);

        let bundle_dir = rust_plan_bundle_dir(&cache, &rust_plan_cache_key(&plan));
        let mut manifest = load_manifest(&bundle_dir);

        std::fs::remove_file(
            bundle_dir
                .join(BUNDLE_FILES_DIR)
                .join("debug/deps/libserde-abc.rlib"),
        )
        .unwrap();
        manifest.artifacts[1].size += 1;
        manifest.artifacts[2].content_hash = "not-the-right-hash".to_string();
        write_manifest(&bundle_dir, &manifest);

        std::fs::remove_dir_all(plan.target_dir.as_path()).unwrap();
        let restored = restore_rust_plan_local(&plan, &cache).unwrap();

        assert_eq!(restored.restored_file_count, 3);
        assert_eq!(
            restored
                .skipped_reasons
                .get("restored_payload_missing_or_corrupt"),
            Some(&3)
        );
        assert_eq!(
            restored
                .miss_classifications
                .get("restored_payload_missing_or_corrupt"),
            Some(&3)
        );
        assert!(!plan
            .target_dir
            .join("debug/deps/libserde-abc.rlib")
            .exists());
    }

    #[test]
    fn restore_skips_manifest_path_traversal_entries() {
        let dir = tempfile::tempdir().unwrap();
        synthetic_target(dir.path());
        let plan = sample_plan(dir.path(), RustPlanMode::Thin);
        let cache = dir.path().join("cache");

        save_rust_plan_local(&plan, &cache).unwrap();

        let bundle_dir = rust_plan_bundle_dir(&cache, &rust_plan_cache_key(&plan));
        let mut manifest = load_manifest(&bundle_dir);
        manifest.artifacts[0].relative_path = "../escape.txt".to_string();
        write_manifest(&bundle_dir, &manifest);

        std::fs::remove_dir_all(plan.target_dir.as_path()).unwrap();
        let restored = restore_rust_plan_local(&plan, &cache).unwrap();

        assert_eq!(restored.restored_file_count, 5);
        assert_eq!(restored.skipped_count, 1);
        assert_eq!(restored.skipped_reasons.get("path_traversal"), Some(&1));
        assert!(!dir.path().join("escape.txt").exists());
    }

    #[test]
    fn safe_join_rejects_path_traversal() {
        let err = safe_join(Path::new("root"), "../outside").unwrap_err();
        assert!(matches!(err, RustPlanError::UnsafeRelativePath(_)));
    }

    #[test]
    fn package_name_parsing_handles_cargo_package_id_shapes() {
        assert_eq!(
            package_name_from_id(
                "registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0"
            ),
            Some("serde".to_string())
        );
        assert_eq!(
            package_name_from_id("path+file:///repo#my-crate@0.1.0"),
            Some("my_crate".to_string())
        );
    }

    #[test]
    fn thin_package_exclusions_match_deps_fingerprint_and_build_by_package_stem() {
        let dir = tempfile::tempdir().unwrap();
        synthetic_target_with_package_exclusions(dir.path());
        let mut plan = sample_plan(dir.path(), RustPlanMode::Thin);
        plan.packages.workspace_package_ids =
            vec!["registry+https://github.com/rust-lang/crates.io-index#app@0.1.0".to_string()];
        plan.packages.excluded_path_package_ids =
            vec!["path+file:///workspace/local_dep#local-dep@0.1.0".to_string()];
        let cache = dir.path().join("cache");

        let saved = save_rust_plan_local(&plan, &cache).unwrap();
        assert_eq!(saved.saved_file_count, 6);
        assert_eq!(
            saved
                .skipped_reasons
                .get("workspace_or_path_dependency_excluded_by_plan"),
            Some(&12)
        );
        assert_eq!(saved.skipped_reasons.get("transient_state"), Some(&1));
        assert!(saved
            .skipped_samples
            .iter()
            .any(|sample| sample.path.ends_with("debug/deps/libapp-abc.rlib")));
        assert!(saved.skipped_samples.iter().any(|sample| sample
            .path
            .ends_with("debug/.fingerprint/app-abc/dep-lib-app")));
        assert!(saved.skipped_samples.iter().any(|sample| sample
            .path
            .ends_with("debug/build/local_dep-abc/out/gen.rs")));
    }
    #[test]
    fn from_json_str_accepts_utf8_bom() {
        let dir = tempfile::tempdir().unwrap();
        let plan = sample_plan(dir.path(), RustPlanMode::Thin);
        let json = serde_json::to_string(&plan).unwrap();
        let loaded = RustArtifactPlanV1::from_json_str(&format!("\u{feff}{json}")).unwrap();
        assert_eq!(loaded.schema_version, 1);
    }

    // ─── rust-plan tar thread resolver (issue #177) ──────────────────────

    #[test]
    fn tar_threads_parser_accepts_grammar_from_soldr_273() {
        // unset / auto / empty / whitespace → default (vCPU-bounded, capped at 8)
        let default = default_rust_plan_tar_threads();
        assert!((1..=DEFAULT_RUST_PLAN_TAR_THREADS_CAP).contains(&default));
        assert_eq!(parse_rust_plan_tar_threads(None), default);
        assert_eq!(parse_rust_plan_tar_threads(Some("auto")), default);
        assert_eq!(parse_rust_plan_tar_threads(Some("AUTO")), default);
        assert_eq!(parse_rust_plan_tar_threads(Some("")), default);
        assert_eq!(parse_rust_plan_tar_threads(Some("   ")), default);

        // 1 → sequential escape hatch
        assert_eq!(parse_rust_plan_tar_threads(Some("1")), 1);

        // Positive integer → clamped to MAX_RUST_PLAN_TAR_THREADS
        assert_eq!(parse_rust_plan_tar_threads(Some("4")), 4);
        assert_eq!(
            parse_rust_plan_tar_threads(Some("9999")),
            MAX_RUST_PLAN_TAR_THREADS
        );

        // Garbage / 0 → default (defensive)
        assert_eq!(parse_rust_plan_tar_threads(Some("0")), default);
        assert_eq!(parse_rust_plan_tar_threads(Some("not-a-number")), default);
        assert_eq!(parse_rust_plan_tar_threads(Some("-1")), default);
    }

    #[test]
    fn parallel_bundling_matches_sequential_byte_for_byte() {
        // `select_artifacts` pre-sorts by relative_path; with rayon's ordered
        // `par_iter().collect()` we must end up with the same artifact list,
        // same hashes, same sizes — regardless of thread count.
        fn bundle_with(threads: usize) -> Vec<RustBundledArtifact> {
            let dir = tempfile::tempdir().unwrap();
            synthetic_target(dir.path());
            let plan = sample_plan(dir.path(), RustPlanMode::Thin);

            let mut candidates = Vec::new();
            collect_files(plan.target_dir.as_path(), &mut candidates).unwrap();
            candidates.sort();
            let mut summary = RustPlanSummary::new(
                RustPlanOperation::Save,
                plan.mode,
                plan.schema_version,
                plan.cache_schema_version,
                rust_plan_cache_key(&plan),
                None,
                None,
            );
            let selected = select_artifacts(&plan, candidates, &mut summary);

            let files_dir = dir.path().join("out").join(format!("t{threads}"));
            std::fs::create_dir_all(&files_dir).unwrap();
            bundle_selected_artifacts_with_threads(&selected, &files_dir, threads).unwrap()
        }

        let sequential = bundle_with(1);
        let parallel = bundle_with(4);

        assert!(!sequential.is_empty());
        assert_eq!(sequential.len(), parallel.len());
        for (seq, par) in sequential.iter().zip(parallel.iter()) {
            assert_eq!(seq.relative_path, par.relative_path);
            assert_eq!(seq.size, par.size);
            assert_eq!(seq.content_hash, par.content_hash);
            assert_eq!(seq.class, par.class);
        }
    }

    // ─── soldr#461: thin-v2 wire-format support ──────────────────────────

    /// Builds the full thin-v2 plan shape soldr emits today: explicit
    /// `cache_profile`, the published allow-list, the published drop-list,
    /// and `cache_schema_version: 2`. Tests below mutate this baseline.
    fn sample_thin_v2_plan(root: &Path) -> RustArtifactPlanV1 {
        RustArtifactPlanV1 {
            allowed_artifact_classes: vec![
                RustArtifactClass::CargoFingerprintMeta,
                RustArtifactClass::DepInfo,
                RustArtifactClass::BuildScriptMetadata,
                RustArtifactClass::BuildScriptOutput,
            ],
            cache_schema_version: 2,
            cache_profile: Some("thin-v2".to_string()),
            dropped_artifact_classes: vec![
                RustArtifactClass::Incremental,
                RustArtifactClass::BuildScriptBuild,
                RustArtifactClass::Rlib,
                RustArtifactClass::Rmeta,
                RustArtifactClass::ProcMacro,
                RustArtifactClass::Dwo,
                RustArtifactClass::Pdb,
                RustArtifactClass::Dsym,
                RustArtifactClass::CargoFingerprintOutputs,
            ],
            ..sample_plan(root, RustPlanMode::Thin)
        }
    }

    #[test]
    fn from_json_value_accepts_thin_v2_cache_profile_and_drop_list() {
        let dir = tempfile::tempdir().unwrap();
        let plan = sample_thin_v2_plan(dir.path());
        let value = serde_json::to_value(&plan).unwrap();
        let loaded = RustArtifactPlanV1::from_json_value(value).unwrap();
        assert_eq!(loaded.cache_profile.as_deref(), Some("thin-v2"));
        assert_eq!(loaded.cache_schema_version, 2);
        assert!(loaded
            .dropped_artifact_classes
            .contains(&RustArtifactClass::Rlib));
        assert!(loaded
            .allowed_artifact_classes
            .contains(&RustArtifactClass::CargoFingerprintMeta));
    }

    #[test]
    fn from_json_value_ignores_unknown_forward_compat_fields() {
        // soldr#461 dropped `#[serde(deny_unknown_fields)]` so future soldr
        // versions can add fields without coordinated zccache releases.
        let dir = tempfile::tempdir().unwrap();
        let plan = sample_plan(dir.path(), RustPlanMode::Thin);
        let mut value = serde_json::to_value(&plan).unwrap();
        value.as_object_mut().unwrap().insert(
            "future_soldr_field_that_does_not_exist_yet".to_string(),
            serde_json::json!({"any": "shape", "version": 9001}),
        );
        let loaded =
            RustArtifactPlanV1::from_json_value(value).expect("unknown fields must be ignored");
        assert_eq!(loaded.schema_version, 1);
    }

    #[test]
    fn legacy_thin_v1_plan_without_new_fields_still_deserializes() {
        // The legacy wire shape (no `cache_profile`, no
        // `dropped_artifact_classes`, `cache_schema_version: 1`) must remain
        // round-trippable so older soldr keeps working unchanged.
        let dir = tempfile::tempdir().unwrap();
        let plan = sample_plan(dir.path(), RustPlanMode::Thin);
        let mut value = serde_json::to_value(&plan).unwrap();
        let obj = value.as_object_mut().unwrap();
        obj.remove("cache_profile");
        obj.remove("dropped_artifact_classes");
        let loaded = RustArtifactPlanV1::from_json_value(value).unwrap();
        assert!(loaded.cache_profile.is_none());
        assert!(loaded.dropped_artifact_classes.is_empty());
        assert_eq!(loaded.cache_schema_version, 1);
    }

    #[test]
    fn from_json_value_accepts_cache_schema_version_2() {
        let dir = tempfile::tempdir().unwrap();
        let mut plan = sample_plan(dir.path(), RustPlanMode::Thin);
        plan.cache_schema_version = 2;
        let value = serde_json::to_value(&plan).unwrap();
        let loaded = RustArtifactPlanV1::from_json_value(value).unwrap();
        assert_eq!(loaded.cache_schema_version, 2);
    }

    #[test]
    fn thin_v2_save_drops_rlib_rmeta_even_when_allowed_list_lists_them() {
        // Confirms the load-bearing property from soldr#461: a file whose
        // class appears in `dropped_artifact_classes` is skipped during
        // save even if the same class is also in `allowed_artifact_classes`.
        let dir = tempfile::tempdir().unwrap();
        synthetic_target(dir.path());
        let mut plan = sample_thin_v2_plan(dir.path());
        // Force the conflict explicitly: keep the drop list, but ALSO put
        // rlib/rmeta into the allow list so the drop semantics has to win.
        plan.allowed_artifact_classes = vec![
            RustArtifactClass::Rlib,
            RustArtifactClass::Rmeta,
            RustArtifactClass::DepInfo,
            RustArtifactClass::CargoFingerprintMeta,
            RustArtifactClass::BuildScriptMetadata,
            RustArtifactClass::BuildScriptOutput,
        ];
        let cache = dir.path().join("cache");

        let saved = save_rust_plan_local(&plan, &cache).unwrap();

        // No `.rlib` / `.rmeta` survives the walk even though they are
        // allowed — drop list wins.
        let bundle_dir = rust_plan_bundle_dir(&cache, &rust_plan_cache_key(&plan));
        let manifest = load_manifest(&bundle_dir);
        for artifact in &manifest.artifacts {
            assert!(
                !artifact.relative_path.ends_with(".rlib"),
                "thin-v2 drop list must skip .rlib; got {}",
                artifact.relative_path
            );
            assert!(
                !artifact.relative_path.ends_with(".rmeta"),
                "thin-v2 drop list must skip .rmeta; got {}",
                artifact.relative_path
            );
        }
        // The drops route through the existing summary skip reason so the
        // CI consumer doesn't need a new bucket.
        assert!(saved
            .skipped_reasons
            .get("artifact_class_disallowed_by_plan")
            .is_some_and(|n| *n >= 3));
    }

    #[test]
    fn thin_v2_save_keeps_fingerprint_meta_but_drops_fingerprint_outputs() {
        // Tests the split: files cargo reads to make a freshness decision
        // (`dep-*`, `lib-*`, `bin-*`, `output-*`, `invoked.timestamp`) are
        // kept; everything else in `.fingerprint/<crate>/` is dropped.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target").join("debug");
        // Meta files (kept):
        write(
            &target.join(".fingerprint/serde-abc/invoked.timestamp"),
            b"ts",
        );
        write(&target.join(".fingerprint/serde-abc/dep-lib-serde"), b"dep");
        write(
            &target.join(".fingerprint/serde-abc/lib-serde"),
            b"libstamp",
        );
        // Output file (dropped):
        write(
            &target.join(".fingerprint/serde-abc/serde-abc.json"),
            b"output",
        );
        // Need at least one classifiable, non-fingerprint file so the
        // walker has something else to think about (and to verify it
        // doesn't get accidentally swept into the drop bucket).
        write(&target.join("deps/serde-abc.d"), b"depinfo");
        // Drop a transient incremental file so we exercise that path too.
        write(&target.join("incremental/state.bin"), b"transient");

        let plan = sample_thin_v2_plan(dir.path());
        let cache = dir.path().join("cache");

        save_rust_plan_local(&plan, &cache).unwrap();

        let bundle_dir = rust_plan_bundle_dir(&cache, &rust_plan_cache_key(&plan));
        let manifest = load_manifest(&bundle_dir);
        let kept_paths: Vec<&str> = manifest
            .artifacts
            .iter()
            .map(|a| a.relative_path.as_str())
            .collect();

        assert!(
            kept_paths
                .iter()
                .any(|p| p.ends_with(".fingerprint/serde-abc/invoked.timestamp")),
            "invoked.timestamp must be kept; got {kept_paths:?}",
        );
        assert!(
            kept_paths
                .iter()
                .any(|p| p.ends_with(".fingerprint/serde-abc/dep-lib-serde")),
            "dep-* must be kept; got {kept_paths:?}",
        );
        assert!(
            kept_paths
                .iter()
                .any(|p| p.ends_with(".fingerprint/serde-abc/lib-serde")),
            "lib-* must be kept; got {kept_paths:?}",
        );
        assert!(
            !kept_paths
                .iter()
                .any(|p| p.ends_with(".fingerprint/serde-abc/serde-abc.json")),
            "fingerprint output .json must be dropped; got {kept_paths:?}",
        );
        assert!(
            kept_paths.iter().any(|p| p.ends_with("deps/serde-abc.d")),
            "dep_info must still be kept; got {kept_paths:?}",
        );
    }

    #[test]
    fn thin_v2_classifier_recognizes_new_classes() {
        // Smoke tests for the classifier branches the wire-format drop
        // list relies on. These run with `thin_v2 = true` to exercise the
        // `.fingerprint/` split; the other categories ignore the flag.
        let bsb = if cfg!(windows) {
            Path::new("debug/build/serde-abc/build-script-build.exe")
        } else {
            Path::new("debug/build/serde-abc/build-script-build")
        };
        assert_eq!(
            classify_artifact(bsb, RustPlanMode::Thin, true),
            Some(RustArtifactClass::BuildScriptBuild),
        );

        assert_eq!(
            classify_artifact(
                Path::new("debug/deps/libserde-abc.dwo"),
                RustPlanMode::Thin,
                true,
            ),
            Some(RustArtifactClass::Dwo),
        );
        assert_eq!(
            classify_artifact(
                Path::new("debug/deps/libserde-abc.pdb"),
                RustPlanMode::Thin,
                true,
            ),
            Some(RustArtifactClass::Pdb),
        );
        assert_eq!(
            classify_artifact(
                Path::new("debug/deps/app.dSYM/Contents/Info.plist"),
                RustPlanMode::Thin,
                true,
            ),
            Some(RustArtifactClass::Dsym),
        );

        // thin-v2 fingerprint split:
        assert_eq!(
            classify_artifact(
                Path::new("debug/.fingerprint/serde-abc/invoked.timestamp"),
                RustPlanMode::Thin,
                true,
            ),
            Some(RustArtifactClass::CargoFingerprintMeta),
        );
        assert_eq!(
            classify_artifact(
                Path::new("debug/.fingerprint/serde-abc/serde-abc.json"),
                RustPlanMode::Thin,
                true,
            ),
            Some(RustArtifactClass::CargoFingerprintOutputs),
        );
        // Legacy (thin_v2 = false) keeps the umbrella class so older
        // callers see no behavior change.
        assert_eq!(
            classify_artifact(
                Path::new("debug/.fingerprint/serde-abc/invoked.timestamp"),
                RustPlanMode::Thin,
                false,
            ),
            Some(RustArtifactClass::CargoFingerprint),
        );
    }

    #[test]
    fn thin_v2_and_thin_v1_identity_hashes_differ() {
        // Two plans with identical inputs but different `cache_profile`
        // values must produce distinct cache keys, otherwise a thin-v1
        // bundle could be served to a thin-v2 plan request (or vice
        // versa) — and the file sets are different, so restore would
        // produce a wrong target/ tree.
        let dir = tempfile::tempdir().unwrap();
        let v1 = sample_plan(dir.path(), RustPlanMode::Thin);
        let v2 = sample_thin_v2_plan(dir.path());
        assert_ne!(
            rust_plan_identity_hash(&v1),
            rust_plan_identity_hash(&v2),
            "thin-v1 and thin-v2 identity hashes must not collide",
        );
    }
}
