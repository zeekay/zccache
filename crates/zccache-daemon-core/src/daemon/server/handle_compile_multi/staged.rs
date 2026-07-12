//! Private execution and v2 publication for supported multi-source misses.

use super::*;
use crate::daemon::lineage::Lineage;
use crate::daemon::process;

#[cfg(test)]
pub(in crate::daemon::server) fn materialize_multi_hit(
    targets: &[(NormalizedPath, NormalizedPath)],
    payloads: &[CachedPayload],
) -> bool {
    write_payloads_par(targets, payloads)
}

pub(super) fn materialize_multi_hit_observed(
    state: &SharedState,
    targets: &[(NormalizedPath, NormalizedPath)],
    payloads: &[CachedPayload],
) -> bool {
    let has_staged_payload = payloads.iter().any(|payload| {
        matches!(payload, CachedPayload::File(path) if is_staged_artifact_path(path.as_path()))
    });
    let started = std::time::Instant::now();
    let observed = write_payloads_par_observed(targets, payloads);
    if has_staged_payload {
        record_staged_hit_materialization(state, targets.len(), started, observed)
    } else {
        observed.is_some()
    }
}

#[allow(clippy::too_many_arguments)]
#[expect(
    clippy::result_large_err,
    reason = "the shared protocol Response is the handler error contract"
)]
pub(super) fn prepare_staged_multi_plan(
    state: &SharedState,
    family: crate::compiler::CompilerFamily,
    compilations: &[crate::compiler::CacheableCompilation],
    original_args: &[String],
    source_arguments: &[crate::compiler::MultiFileSourceArgument],
    output_layout: &crate::compiler::MultiFileOutputLayout,
    cwd: &Path,
) -> Result<Option<StagedMultiCompilePlan>, Response> {
    use crate::daemon::staged_stats::{StagedCounter, StagedTiming};
    let started = std::time::Instant::now();
    state.profiler.staged.count(StagedCounter::PlanAttempted);
    let result = StagedMultiCompilePlan::build(
        state.staging.path(),
        family,
        compilations,
        original_args,
        source_arguments,
        output_layout,
        cwd,
    );
    state
        .profiler
        .staged
        .timing(StagedTiming::Planning, started.elapsed().as_nanos() as u64);
    match result {
        StagedPlanOutcome::Enabled(plan) => {
            state.profiler.staged.count(StagedCounter::PlanEnabled);
            Ok(Some(plan))
        }
        StagedPlanOutcome::Unsupported(reason) => {
            state.profiler.staged.count(StagedCounter::PlanUnsupported);
            state.profiler.staged.failure(reason.failure());
            Ok(None)
        }
        StagedPlanOutcome::Error(error) => {
            state.profiler.staged.count(StagedCounter::PlanError);
            state.profiler.staged.failure(error.reason.failure());
            Err(Response::Error {
                message: format!(
                    "failed to prepare private multi-source staging: {}",
                    error.source
                ),
            })
        }
    }
}

fn publish_and_materialize_multi_unit(
    state: &SharedState,
    unit: &StagedMultiUnitPlan,
    key: Option<&str>,
    metadata: Option<ArtifactIndex>,
) -> std::io::Result<bool> {
    // Requested paths become complete before the generation/pointer/index can
    // become visible. If materialization fails, publication is never started.
    materialize_multi_unit_observed(state, unit, None)?;
    let publication = match (key, metadata) {
        (Some(key), Some(metadata)) => Some(publish_artifact_paths_observed(
            state,
            key,
            metadata,
            &unit.staged_paths(),
        )),
        _ => None,
    };
    if let Some(reason) = publication
        .as_ref()
        .and_then(|result| result.as_ref().err())
        .map(|reason| reason.id())
    {
        record_prepublication_salvage_success(state, unit.outputs.len(), reason);
    }
    Ok(publication.as_ref().is_some_and(Result::is_ok))
}

struct PreparedUnit {
    compilation_index: usize,
    source_path: NormalizedPath,
    context_key: ContextKey,
    dep_dirs: Vec<NormalizedPath>,
    artifact_key_hex: Option<String>,
    artifact_bytes: u64,
    metadata: Option<ArtifactIndex>,
}

type UnitStreams = (Arc<Vec<u8>>, Arc<Vec<u8>>);

fn flatten_unit_streams(streams: &[UnitStreams]) -> UnitStreams {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    for (one_stdout, one_stderr) in streams {
        stdout.extend_from_slice(one_stdout);
        stderr.extend_from_slice(one_stderr);
    }
    (Arc::new(stdout), Arc::new(stderr))
}

fn validate_staged_outputs(unit_plan: &StagedMultiUnitPlan) -> Result<(), String> {
    for output in &unit_plan.outputs {
        let metadata = std::fs::metadata(&output.staged).map_err(|error| {
            format!(
                "successful multi-source compiler omitted {}: {error}",
                output.staged.display()
            )
        })?;
        if metadata.is_dir() {
            return Err(format!(
                "successful multi-source compiler produced a directory at {}",
                output.staged.display()
            ));
        }
        if metadata.len() == 0 {
            return Err(format!(
                "successful multi-source compiler produced an empty output at {}",
                output.staged.display()
            ));
        }
    }
    Ok(())
}

async fn run_private_unit(
    state: &SharedState,
    compiler: &NormalizedPath,
    family: crate::compiler::CompilerFamily,
    args: &[String],
    cwd: &NormalizedPath,
    client_env: &Option<Vec<(String, String)>>,
    lineage: &Lineage,
) -> Result<std::process::Output, String> {
    let rsp_guard = crate::compiler::response_file::write_response_file_if_needed(
        args,
        &state.depfile_tmpdir,
        family,
    )
    .map_err(|error| format!("failed to write private multi-source response file: {error}"))?;
    let mut command = tokio::process::Command::new(compiler);
    if let Some(response) = rsp_guard.as_ref() {
        command.arg(response.at_arg());
    } else {
        command.args(args);
    }
    command.current_dir(cwd);
    apply_client_env(&mut command, client_env, lineage);
    let priority = CompilePriority::from_client_env(client_env.as_deref());
    process::tokio_command_output_with_priority(&mut command, priority)
        .await
        .map_err(|error| format!("failed to run private multi-source compiler: {error}"))
}

fn prepare_unit(
    state: &SharedState,
    unit_result: &UnitCacheResult,
    unit_plan: &StagedMultiUnitPlan,
    cwd: &NormalizedPath,
    unit_stdout: Arc<Vec<u8>>,
    unit_stderr: Arc<Vec<u8>>,
) -> Result<PreparedUnit, String> {
    let (source_path, context_key, ctx, pre_hashes, pre_hash_complete, pre_stamps, pre_clock) =
        match unit_result {
            UnitCacheResult::Miss {
                source_path,
                context_key,
                ctx,
                pre_hashes,
                pre_hash_complete,
                pre_stamps,
                pre_clock,
            } => (
                source_path,
                *context_key,
                ctx.as_ref(),
                pre_hashes,
                *pre_hash_complete,
                pre_stamps,
                *pre_clock,
            ),
            UnitCacheResult::Hit { .. } => {
                return Err("staged unit unexpectedly refers to a hit".into())
            }
        };
    validate_staged_outputs(unit_plan)?;

    let mut scan_result = if let Some(depfile) = unit_plan.staged_depfile.as_ref() {
        match crate::depgraph::depfile::parse_depfile_path(depfile, source_path, cwd) {
            Ok(result) => result,
            Err(error) => {
                tracing::warn!(
                    source = %source_path.display(),
                    depfile = %depfile.display(),
                    "private multi-source depfile parse failed; scanning source: {error}"
                );
                crate::depgraph::scanner::scan_recursive(source_path, &ctx.include_search)
            }
        }
    } else {
        crate::depgraph::scanner::scan_recursive(source_path, &ctx.include_search)
    };
    if unit_plan.outputs.len() == 1 {
        if let Some(depfile) = unit_plan.staged_depfile.as_ref() {
            let _ = std::fs::remove_file(depfile);
        }
    }

    let mut tracked: HashSet<NormalizedPath> = pre_hashes.keys().cloned().collect();
    tracked.insert(source_path.clone());
    tracked.extend(scan_result.resolved.iter().cloned());
    tracked.extend(ctx.force_includes.iter().cloned());
    let tracked_paths: Vec<NormalizedPath> = tracked.into_iter().collect();
    let resolved: HashSet<NormalizedPath> = scan_result.resolved.iter().cloned().collect();
    scan_result.resolved.extend(
        pre_hashes
            .keys()
            .filter(|path| **path != *source_path && !resolved.contains(*path))
            .cloned(),
    );
    state.cache_system.register_tracked(&tracked_paths);
    let dep_dirs = tracked_paths
        .iter()
        .filter_map(|path| path.parent().map(NormalizedPath::from))
        .collect();
    let current_clock = state.cache_system.current_clock();
    let hash_map: HashMap<NormalizedPath, ContentHash> = {
        use rayon::prelude::*;
        tracked_paths
            .par_iter()
            .filter_map(|path| {
                hash_file(&state.cache_system, path, current_clock)
                    .ok()
                    .map(|hash| (path.clone(), hash))
            })
            .collect()
    };
    let inputs_stable = pre_hash_complete
        && pre_hashes.len() == hash_map.len()
        && pre_stamps.len() == tracked_paths.len()
        && pre_hashes
            .iter()
            .all(|(path, before)| hash_map.get(path) == Some(before))
        && tracked_paths.iter().all(|path| {
            input_stamp(path).as_ref() == pre_stamps.get(path)
                && !state.cache_system.journal().changed_since(path, pre_clock)
        });
    let get_hash = |path: &Path| hash_map.get(&NormalizedPath::new(path)).copied();
    let artifact_key = inputs_stable
        .then(|| {
            state
                .dep_graph
                .load()
                .update(&context_key, scan_result, get_hash)
        })
        .flatten();

    let mut output_names = Vec::with_capacity(unit_plan.outputs.len());
    let mut output_sizes = Vec::with_capacity(unit_plan.outputs.len());
    for output in &unit_plan.outputs {
        output_names.push(
            output
                .requested
                .file_name()
                .ok_or_else(|| {
                    format!(
                        "requested output has no filename: {}",
                        output.requested.display()
                    )
                })?
                .to_string_lossy()
                .into_owned(),
        );
        output_sizes.push(
            std::fs::metadata(&output.staged)
                .map_err(|error| format!("failed to stat {}: {error}", output.staged.display()))?
                .len(),
        );
    }
    let artifact_bytes = output_sizes.iter().copied().sum();
    let metadata = artifact_key.as_ref().map(|_| {
        ArtifactIndex::new(
            output_names,
            output_sizes,
            Arc::clone(&unit_stdout),
            Arc::clone(&unit_stderr),
            0,
        )
    });
    Ok(PreparedUnit {
        compilation_index: unit_plan.compilation_index,
        source_path: source_path.clone(),
        context_key,
        dep_dirs,
        artifact_key_hex: artifact_key.map(|key| key.hash().to_hex()),
        artifact_bytes,
        metadata,
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_staged_multi_misses(
    state: Arc<SharedState>,
    sid: SessionId,
    compiler: NormalizedPath,
    family: crate::compiler::CompilerFamily,
    unit_results: Vec<UnitCacheResult>,
    staged_plan: StagedMultiCompilePlan,
    cwd: NormalizedPath,
    client_env: Option<Vec<(String, String)>>,
) -> Response {
    use crate::daemon::staged_stats::{StagedCounter, StagedTiming};
    let lineage = Lineage::current(session_client_pid(&state, &sid), Some(sid.to_string()));
    let empty = Arc::new(Vec::new());
    let mut unit_streams = vec![(Arc::clone(&empty), Arc::clone(&empty)); unit_results.len()];
    for (index, result) in unit_results.iter().enumerate() {
        if let UnitCacheResult::Hit { stdout, stderr, .. } = result {
            unit_streams[index] = (Arc::clone(stdout), Arc::clone(stderr));
        }
    }
    let compiler_started = std::time::Instant::now();
    for unit in &staged_plan.units {
        if !matches!(
            unit_results[unit.compilation_index],
            UnitCacheResult::Miss { .. }
        ) {
            continue;
        }
        let output = match run_private_unit(
            &state,
            &compiler,
            family,
            &unit.rewritten_args,
            &cwd,
            &client_env,
            &lineage,
        )
        .await
        {
            Ok(output) => output,
            Err(message) => return Response::Error { message },
        };
        let exit_code = output.status.code().unwrap_or(-1);
        unit_streams[unit.compilation_index] = (Arc::new(output.stdout), Arc::new(output.stderr));
        if exit_code != 0 {
            state.profiler.staged.count(StagedCounter::CompilerStaged);
            state.profiler.staged.timing(
                StagedTiming::Compiler,
                compiler_started.elapsed().as_nanos() as u64,
            );
            state.stats.record_error();
            record_session_stat(&state.sessions, &sid, |stats| stats.record_error());
            let (stdout, stderr) = flatten_unit_streams(&unit_streams);
            return Response::CompileResult {
                exit_code,
                stdout,
                stderr,
                cached: false,
            };
        }
    }
    state.profiler.staged.count(StagedCounter::CompilerStaged);
    state.profiler.staged.timing(
        StagedTiming::Compiler,
        compiler_started.elapsed().as_nanos() as u64,
    );

    let mut prepared = Vec::new();
    for unit in &staged_plan.units {
        if !matches!(
            unit_results[unit.compilation_index],
            UnitCacheResult::Miss { .. }
        ) {
            continue;
        }
        match prepare_unit(
            &state,
            &unit_results[unit.compilation_index],
            unit,
            &cwd,
            Arc::clone(&unit_streams[unit.compilation_index].0),
            Arc::clone(&unit_streams[unit.compilation_index].1),
        ) {
            Ok(one) => prepared.push(one),
            Err(message) => return Response::Error { message },
        }
    }

    let mut dep_dirs: HashSet<NormalizedPath> = HashSet::new();
    let mut changed_outputs = Vec::new();
    for prepared in prepared {
        let unit = &staged_plan.units[prepared.compilation_index];
        let cacheable = match publish_and_materialize_multi_unit(
            &state,
            unit,
            prepared.artifact_key_hex.as_deref(),
            prepared.metadata.clone(),
        ) {
            Ok(cacheable) => cacheable,
            Err(error) => {
                return Response::Error {
                    message: format!("failed to materialize multi-source outputs: {error}"),
                };
            }
        };
        if prepared.artifact_key_hex.is_some() && !cacheable {
            if let Some(key) = prepared.artifact_key_hex.as_ref() {
                state
                    .dep_graph
                    .load()
                    .invalidate_artifact_keys(&HashSet::from([key.clone()]));
            }
        }
        if cacheable {
            let (Some(key), Some(metadata)) = (
                prepared.artifact_key_hex.as_ref(),
                prepared.metadata.as_ref(),
            ) else {
                return Response::Error {
                    message: "published multi-source artifact lost its identity".into(),
                };
            };
            let paths = match load_staged_artifact_paths(
                &state.artifact_dir,
                key,
                &metadata.output_sizes,
            ) {
                Ok(Some(paths)) => paths,
                Ok(None) => {
                    return Response::Error {
                        message: "published multi-source generation was not visible".into(),
                    };
                }
                Err(error) => {
                    return Response::Error {
                        message: format!(
                            "failed to load published multi-source generation: {error}"
                        ),
                    };
                }
            };
            let (unit_stdout, unit_stderr) = &unit_streams[prepared.compilation_index];
            let cached = CachedArtifact {
                meta: metadata.clone(),
                stdout: Arc::clone(unit_stdout),
                stderr: Arc::clone(unit_stderr),
                payloads: Some(Arc::from(
                    paths
                        .into_iter()
                        .map(CachedPayload::File)
                        .collect::<Vec<_>>(),
                )),
                last_used: std::time::Instant::now(),
            };
            state.artifacts.insert(key.clone(), cached);
            state.fast_hit_cache.insert(
                prepared.context_key,
                FastHitEntry {
                    clock: state.cache_system.current_clock(),
                    artifact_key_hex: key.clone(),
                    cached_at: std::time::Instant::now(),
                },
            );
        }
        state.stats.record_miss(0, prepared.artifact_bytes);
        let source = prepared.source_path.clone();
        let bytes = prepared.artifact_bytes;
        record_session_stat(&state.sessions, &sid, move |stats| {
            stats.record_miss(source, bytes);
        });
        dep_dirs.extend(prepared.dep_dirs);
        changed_outputs.extend(unit.outputs.iter().map(|output| output.requested.clone()));
    }
    watch_directories(&state, &dep_dirs.into_iter().collect::<Vec<_>>()).await;
    if !changed_outputs.is_empty() {
        state.cache_system.apply_changes(changed_outputs);
    }
    let (stdout, stderr) = flatten_unit_streams(&unit_streams);
    Response::CompileResult {
        exit_code: 0,
        stdout,
        stderr,
        cached: false,
    }
}

#[cfg(test)]
#[path = "staged_tests.rs"]
mod tests;
