//! Multi-file compile handler: handle_compile_multi, check_unit_cache, PendingWrite, UnitCacheResult.

use super::*;

#[path = "handle_compile_multi/staged.rs"]
mod staged;
#[cfg(test)]
pub(super) use staged::materialize_multi_hit;
use staged::{
    handle_staged_multi_misses, materialize_multi_hit_observed, prepare_staged_multi_plan,
};

/// A deferred output write for a cache hit.
pub(super) struct PendingWrite {
    out_path: NormalizedPath,
    cache_file: NormalizedPath,
    data: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct InputStamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
    created: Option<std::time::SystemTime>,
    file_id: Option<FileId>,
    change_marker: Option<i128>,
}

fn input_stamp(path: &Path) -> Option<InputStamp> {
    let metadata = std::fs::metadata(path).ok()?;
    Some(InputStamp {
        len: metadata.len(),
        modified: metadata.modified().ok(),
        created: metadata.created().ok(),
        file_id: get_file_id(path),
        change_marker: get_file_change_marker(path),
    })
}

/// Result of a per-unit cache check in multi-file compile.
pub(super) enum UnitCacheResult {
    /// Cache hit â€” output write is deferred for batching.
    Hit {
        stdout: Arc<Vec<u8>>,
        stderr: Arc<Vec<u8>>,
        artifact_bytes: u64,
        source_path: NormalizedPath,
        pending_writes: Vec<PendingWrite>,
    },
    /// Cache miss â€” needs compilation.
    Miss {
        source_path: NormalizedPath,
        context_key: ContextKey,
        ctx: Box<CompileContext>,
        pre_hashes: HashMap<NormalizedPath, ContentHash>,
        pre_hash_complete: bool,
        pre_stamps: HashMap<NormalizedPath, InputStamp>,
        pre_clock: Clock,
    },
}

/// Check cache for a single compilation unit. Returns Hit (output written) or Miss.
///
/// If `shared_base` is provided, the CompileContext is built by cloning it and
/// overriding the source_file, avoiding redundant arg parsing for multi-file
/// compilations where all units share the same flags.
pub(super) fn check_unit_cache(
    state: &SharedState,
    compilation: &crate::compiler::CacheableCompilation,
    cwd_path: &Path,
    key_root: &NormalizedPath,
    system_includes: &[NormalizedPath],
    shared_base: Option<&CompileContext>,
    cache_now: Instant,
) -> UnitCacheResult {
    let t0 = std::time::Instant::now();
    let snap_clock = state.cache_system.current_clock();
    state.stats.record_compilation();

    let source_path = if compilation.source_file.is_absolute() {
        compilation.source_file.clone()
    } else {
        cwd_path.join(&compilation.source_file).into()
    };
    let output_path = if compilation.output_file.is_absolute() {
        compilation.output_file.clone()
    } else {
        cwd_path.join(&compilation.output_file).into()
    };

    let (ctx, _dep_flags) = if let Some(base) = shared_base {
        let mut ctx = base.clone();
        ctx.source_file = source_path.clone();
        (
            ctx,
            UserDepFlags {
                has_md: false,
                mf_path: None,
            },
        )
    } else {
        match build_compile_context(
            compilation,
            cwd_path,
            system_includes,
            &[],
            &state.compiler_hash_cache,
        ) {
            BuildContextResult::Cc { ctx, dep_flags } => (ctx, dep_flags),
            BuildContextResult::Rustc { compat_ctx, .. } => (compat_ctx, UserDepFlags::default()),
        }
    };
    let t_ctx = t0.elapsed();
    // Issue #474: PCH / MSVC compiles take a per-worktree salt so the
    // resulting cache entry can't be cross-served between sibling
    // worktrees. Mirror of the single-file gate in
    // `pipeline.rs::handle_compile_request`.
    let source_mode_for_key = if matches!(
        compilation
            .output_file
            .as_path()
            .extension()
            .and_then(|e| e.to_str()),
        Some("pch") | Some("gch")
    ) {
        crate::compiler::SourceMode::Header
    } else {
        crate::compiler::SourceMode::Normal
    };
    let worktree_salt = if requires_worktree_in_key(compilation.family, source_mode_for_key) {
        Some(key_root.as_path())
    } else {
        None
    };
    let context_key = state.dep_graph.load().register_with_root_and_salt(
        ctx.clone(),
        Some(key_root.clone()),
        worktree_salt,
    );
    let t_register = t0.elapsed();

    // â”€â”€ Ultra-fast path: per-file freshness skip â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
    // If the watcher is active and none of the source/header files have
    // changed since the last verified hit, skip ALL hash/depgraph work.
    if state.watcher_active.load(Ordering::Acquire) {
        if let Some(entry) = state.fast_hit_cache.get(&context_key) {
            if cache_entry_fresh_at(cache_now, entry.cached_at, FAST_HIT_MAX_AGE)
                && context_files_fresh(state, &context_key, &source_path, entry.clock)
            {
                let artifact_key_hex = &entry.artifact_key_hex;
                // Write outputs directly from DashMap reference â€” eliminates
                // cloning all .o data (~50-200KB per file) into PendingWrite.
                // Each check_unit_cache runs in its own spawn_blocking task,
                // so writes are already parallel across units.
                if let Some(mut cached_ref) =
                    lookup_artifact_with_disk_fallback(state, artifact_key_hex)
                {
                    cached_ref.last_used = std::time::Instant::now();
                    let loaded =
                        ensure_payloads(&mut cached_ref, &state.artifact_dir, artifact_key_hex)
                            .is_some();
                    if loaded {
                        #[expect(
                            clippy::expect_used,
                            reason = "ensure_payloads on the preceding line returned Some, which is the contract guaranteeing cached_ref.payloads is now populated"
                        )]
                        let payloads = Arc::clone(
                            cached_ref
                                .payloads
                                .as_ref()
                                .expect("ensure_payloads above returned without error"),
                        );
                        let names = Arc::clone(&cached_ref.meta.output_names);
                        let artifact_bytes: u64 = cached_ref.meta.total_size;
                        let stdout = cached_ref.stdout.clone();
                        let stderr = cached_ref.stderr.clone();
                        drop(cached_ref);

                        let targets: Vec<(NormalizedPath, NormalizedPath)> = (0..payloads.len())
                            .map(|i| {
                                let out: NormalizedPath = if i == 0 {
                                    output_path.clone()
                                } else {
                                    output_path
                                        .parent()
                                        .unwrap_or(cwd_path)
                                        .join(&names[i])
                                        .into()
                                };
                                let cache_file =
                                    state.artifact_dir.join(format!("{artifact_key_hex}_{i}"));
                                (out, cache_file)
                            })
                            .collect();
                        if materialize_multi_hit_observed(state, &targets, &payloads) {
                            state.stats.record_hit(0, artifact_bytes);
                            state.profiler.record_hit(&HitPhases {
                                parse_args_ns: 0,
                                build_context_ns: t_ctx.as_nanos() as u64,
                                hash_source_ns: 0,
                                hash_headers_ns: 0,
                                depgraph_check_ns: 0,
                                request_cache_lookup_ns: 0,
                                cross_root_validate_ns: 0,
                                artifact_lookup_ns: 0,
                                write_output_ns: 0,
                                bookkeeping_ns: 0,
                                total_ns: t0.elapsed().as_nanos() as u64,
                            });
                            return UnitCacheResult::Hit {
                                stdout,
                                stderr,
                                artifact_bytes,
                                source_path,
                                pending_writes: Vec::new(),
                            };
                        }
                        let evicted: std::collections::HashSet<String> =
                            std::iter::once(artifact_key_hex.clone()).collect();
                        state.dep_graph.load().invalidate_artifact_keys(&evicted);
                        state.fast_hit_cache.remove(&context_key);
                    }
                }
            }
        }
    }

    // Hash source
    let source_hash = match hash_file(&state.cache_system, &source_path, snap_clock) {
        Ok(h) => h,
        Err(_) => {
            return UnitCacheResult::Miss {
                source_path,
                context_key,
                ctx: Box::new(ctx),
                pre_hashes: HashMap::new(),
                pre_hash_complete: false,
                pre_stamps: HashMap::new(),
                pre_clock: snap_clock,
            };
        }
    };
    let t_hash_source = t0.elapsed();

    // Hash known headers + force-includes in parallel
    let mut hash_map: HashMap<NormalizedPath, ContentHash> = HashMap::new();
    hash_map.insert(source_path.clone(), source_hash);
    {
        use rayon::prelude::*;
        let includes = state.dep_graph.load().get_includes(&context_key);
        let include_iter = includes.iter().flat_map(|v| v.iter());
        let all_paths: Vec<&NormalizedPath> =
            include_iter.chain(ctx.force_includes.iter()).collect();
        let hashes: Vec<_> = all_paths
            .par_iter()
            .filter_map(|path| {
                hash_file(&state.cache_system, path, snap_clock)
                    .ok()
                    .map(|h| ((*path).clone(), h))
            })
            .collect();
        for (path, h) in hashes {
            hash_map.insert(path, h);
        }
    }
    let t_hash_headers = t0.elapsed();

    // Depgraph check
    let verdict = {
        let is_fresh = |p: &Path| {
            let path = NormalizedPath::new(p);
            !state
                .cache_system
                .journal()
                .changed_since(&path, snap_clock)
        };
        let get_hash = |p: &Path| {
            let path = NormalizedPath::new(p);
            hash_map.get(&path).copied()
        };
        state
            .dep_graph
            .load()
            .check(&context_key, is_fresh, get_hash)
    };
    let t_depgraph = t0.elapsed();

    // Try to serve from cache
    let depgraph_claimed_hit = matches!(verdict, crate::depgraph::CacheVerdict::Hit { .. });
    if let crate::depgraph::CacheVerdict::Hit { artifact_key }
    | crate::depgraph::CacheVerdict::SourceChanged { artifact_key } = verdict
    {
        let artifact_key_hex = artifact_key.hash().to_hex();
        if let Some(mut cached_ref) = lookup_artifact_with_disk_fallback(state, &artifact_key_hex) {
            cached_ref.last_used = std::time::Instant::now();
            let t_lookup = t0.elapsed();
            let loaded =
                ensure_payloads(&mut cached_ref, &state.artifact_dir, &artifact_key_hex).is_some();
            if loaded {
                #[expect(
                    clippy::expect_used,
                    reason = "ensure_payloads on the preceding line returned Some, which is the contract guaranteeing cached_ref.payloads is now populated"
                )]
                let payloads = Arc::clone(
                    cached_ref
                        .payloads
                        .as_ref()
                        .expect("ensure_payloads above returned without error"),
                );
                let names = Arc::clone(&cached_ref.meta.output_names);
                let artifact_bytes: u64 = cached_ref.meta.total_size;
                let stdout = cached_ref.stdout.clone();
                let stderr = cached_ref.stderr.clone();
                drop(cached_ref);

                let targets: Vec<(NormalizedPath, NormalizedPath)> = (0..payloads.len())
                    .map(|i| {
                        let out: NormalizedPath = if i == 0 {
                            output_path.clone()
                        } else {
                            output_path
                                .parent()
                                .unwrap_or(cwd_path)
                                .join(&names[i])
                                .into()
                        };
                        let cache_file = state.artifact_dir.join(format!("{artifact_key_hex}_{i}"));
                        (out, cache_file)
                    })
                    .collect();
                if materialize_multi_hit_observed(state, &targets, &payloads) {
                    state.stats.record_hit(0, artifact_bytes);

                    // Populate fast-hit cache for future requests
                    let tracked_paths =
                        request_cache_input_paths(state, &context_key, &source_path, &ctx);
                    state.cache_system.register_tracked(&tracked_paths);
                    let current_clock = state.cache_system.current_clock();
                    state.fast_hit_cache.insert(
                        context_key,
                        FastHitEntry {
                            clock: current_clock,
                            artifact_key_hex: artifact_key_hex.clone(),
                            cached_at: std::time::Instant::now(),
                        },
                    );

                    let total_ns = t0.elapsed().as_nanos() as u64;
                    state.profiler.record_hit(&HitPhases {
                        parse_args_ns: 0,
                        build_context_ns: t_ctx.as_nanos() as u64,
                        hash_source_ns: (t_hash_source - t_register).as_nanos() as u64,
                        hash_headers_ns: (t_hash_headers - t_hash_source).as_nanos() as u64,
                        depgraph_check_ns: (t_depgraph - t_hash_headers).as_nanos() as u64,
                        request_cache_lookup_ns: 0,
                        cross_root_validate_ns: 0,
                        artifact_lookup_ns: (t_lookup - t_depgraph).as_nanos() as u64,
                        write_output_ns: 0,
                        bookkeeping_ns: 0,
                        total_ns,
                    });

                    return UnitCacheResult::Hit {
                        stdout,
                        stderr,
                        artifact_bytes,
                        source_path,
                        pending_writes: Vec::new(),
                    };
                }
            }
        }
        if depgraph_claimed_hit {
            let evicted: std::collections::HashSet<String> =
                std::iter::once(artifact_key_hex).collect();
            state.dep_graph.load().invalidate_artifact_keys(&evicted);
        }
    }

    state.fast_hit_cache.remove(&context_key);
    // Complete the pre-spawn snapshot even for a first compile whose depgraph
    // has no known include set yet. Sequential private units may run long
    // enough for sources or headers to change between cache check and publish.
    let pre_scan = crate::depgraph::scanner::scan_recursive(&source_path, &ctx.include_search);
    let mut pre_hash_complete = true;
    for path in &pre_scan.resolved {
        if hash_map.contains_key(path) {
            continue;
        }
        match hash_file(&state.cache_system, path, snap_clock) {
            Ok(hash) => {
                hash_map.insert(path.clone(), hash);
            }
            Err(_) => pre_hash_complete = false,
        }
    }
    let pre_stamps: HashMap<NormalizedPath, InputStamp> = hash_map
        .keys()
        .filter_map(|path| input_stamp(path).map(|stamp| (path.clone(), stamp)))
        .collect();
    pre_hash_complete &= pre_stamps.len() == hash_map.len();
    UnitCacheResult::Miss {
        source_path,
        context_key,
        ctx: Box::new(ctx),
        pre_hashes: hash_map,
        pre_hash_complete,
        pre_stamps,
        pre_clock: snap_clock,
    }
}

/// Handle a multi-file compile: check cache per-unit in parallel, serve hits, batch misses.
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_compile_multi(
    state: Arc<SharedState>,
    sid: SessionId,
    compiler: NormalizedPath,
    compilations: Vec<crate::compiler::CacheableCompilation>,
    original_args: Arc<[String]>,
    source_arguments: Vec<crate::compiler::MultiFileSourceArgument>,
    output_layout: crate::compiler::MultiFileOutputLayout,
    cwd_path: NormalizedPath,
    worktree_root: Option<NormalizedPath>,
    system_includes: Vec<NormalizedPath>,
    client_env: Option<Vec<(String, String)>>,
    compile_start: Instant,
) -> Response {
    let key_root = worktree_root.as_ref().unwrap_or(&cwd_path).clone();

    // Classify the complete output set before cache lookup or materialization.
    // Unsupported/shared shapes must execute once with their original argv;
    // object hits cannot stand in for side effects that are not modeled.
    let staged_plan = match prepare_staged_multi_plan(
        &state,
        compilations[0].family,
        &compilations,
        &original_args,
        &source_arguments,
        &output_layout,
        &cwd_path,
    ) {
        Ok(Some(plan)) => plan,
        Ok(None) => {
            for compilation in &compilations {
                let output = if compilation.output_file.is_absolute() {
                    compilation.output_file.clone()
                } else {
                    cwd_path.join(&compilation.output_file)
                };
                if let Err(error) = break_output_hardlink_before_compile(&output) {
                    return Response::Error {
                        message: format!(
                            "failed to detach hardlinked multi-source output {}: {error}",
                            output.display()
                        ),
                    };
                }
                state.stats.record_compilation();
            }
            let response = run_compiler_direct(
                &compiler,
                &original_args,
                &cwd_path,
                &state.sessions,
                &sid,
                &client_env,
                &[],
                &state.depfile_tmpdir,
            )
            .await;
            if matches!(
                &response,
                Response::CompileResult { exit_code, .. } if *exit_code != 0
            ) {
                state.stats.record_error();
                record_session_stat(&state.sessions, &sid, |stats| stats.record_error());
            }
            return response;
        }
        Err(response) => return response,
    };

    // â”€â”€ Pre-parse shared args once for all units â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
    // All units share the same original_args (via Arc) â€” only source/output
    // differ. Parse the flags once and reuse the base CompileContext, avoiding
    // redundant arg parsing for each of the N compilation units.
    let shared_base: Arc<CompileContext> = {
        let first = &compilations[0];
        let parsed = match first.family {
            crate::compiler::CompilerFamily::Msvc => {
                crate::depgraph::msvc_args::parse_msvc_args(&first.original_args, &cwd_path)
            }
            _ => crate::depgraph::args::parse_gnu_args(&first.original_args, &cwd_path),
        };
        let mut base = CompileContext::from_parsed_args(parsed);
        for path in &system_includes {
            if !base.include_search.system.contains(path) {
                base.include_search.system.push(path.clone());
            }
        }
        Arc::new(base)
    };

    // â”€â”€ Phase 1: Check cache for each unit (parallel, as-completed) â”€â”€
    let mut join_set = tokio::task::JoinSet::new();
    for (idx, compilation) in compilations.iter().enumerate() {
        let state = Arc::clone(&state);
        let cwd_path = cwd_path.clone();
        let key_root = key_root.clone();
        let system_includes = system_includes.clone();
        let compilation = compilation.clone();
        let shared_base = Arc::clone(&shared_base);
        let cache_now = compile_start;
        join_set.spawn_blocking(move || {
            (
                idx,
                check_unit_cache(
                    &state,
                    &compilation,
                    &cwd_path,
                    &key_root,
                    &system_includes,
                    Some(&shared_base),
                    cache_now,
                ),
            )
        });
    }

    // Collect results in original order
    let mut indexed_results: Vec<(usize, UnitCacheResult)> = Vec::with_capacity(compilations.len());
    while let Some(result) = join_set.join_next().await {
        match result {
            Ok(pair) => indexed_results.push(pair),
            Err(e) => {
                return Response::Error {
                    message: format!("cache check task panicked: {e}"),
                };
            }
        }
    }
    indexed_results.sort_by_key(|(idx, _)| *idx);

    let mut unit_results: Vec<UnitCacheResult> = Vec::with_capacity(indexed_results.len());
    let mut all_pending_writes: Vec<PendingWrite> = Vec::new();
    for (_, mut result) in indexed_results {
        match &result {
            UnitCacheResult::Hit {
                artifact_bytes,
                source_path,
                ..
            } => {
                let src = source_path.clone();
                let bytes = *artifact_bytes;
                record_session_stat(&state.sessions, &sid, move |stats| {
                    stats.record_hit(src, 0, bytes);
                });
            }
            UnitCacheResult::Miss { source_path, .. } => {
                write_session_log(
                    &state.sessions,
                    &sid,
                    &format!("multi-file cache miss: {}", source_path.display()),
                );
            }
        }
        if let UnitCacheResult::Hit {
            ref mut pending_writes,
            ..
        } = result
        {
            all_pending_writes.append(pending_writes);
        }
        unit_results.push(result);
    }

    // Execute all deferred legacy byte writes in parallel. V2 file hits are
    // already materialized by check_unit_cache and leave this list empty.
    if !all_pending_writes.is_empty() {
        let mut write_set = tokio::task::JoinSet::new();
        for pending in all_pending_writes {
            write_set.spawn_blocking(move || {
                let _ = write_cached_output(&pending.out_path, &pending.cache_file, &pending.data);
            });
        }
        while write_set.join_next().await.is_some() {}
    }

    // For cache HIT outputs: downgrade metadata without advancing clock
    // (same artifact content). For cache MISS outputs: apply_changes is
    // done later after real compilation. This preserves fast-hit cache
    // validity for unrelated source files.
    {
        let mut output_dirs = HashSet::new();
        for (idx, comp) in compilations.iter().enumerate() {
            let out = if comp.output_file.is_absolute() {
                comp.output_file.clone()
            } else {
                cwd_path.join(&comp.output_file)
            };
            if let Some(parent) = out.parent() {
                output_dirs.insert(parent.into());
            }
            if matches!(&unit_results[idx], UnitCacheResult::Hit { .. }) {
                state.cache_system.metadata().downgrade(&out);
            }
        }
        let dirs: Vec<NormalizedPath> = output_dirs.into_iter().collect();
        watch_directories(&state, &dirs).await;
    }

    let miss_sources: Vec<&NormalizedPath> = unit_results
        .iter()
        .filter_map(|r| match r {
            UnitCacheResult::Miss { source_path, .. } => Some(source_path),
            UnitCacheResult::Hit { .. } => None,
        })
        .collect();

    if miss_sources.is_empty() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        for unit in &unit_results {
            if let UnitCacheResult::Hit {
                stdout: one_stdout,
                stderr: one_stderr,
                ..
            } = unit
            {
                stdout.extend_from_slice(one_stdout);
                stderr.extend_from_slice(one_stderr);
            }
        }
        return Response::CompileResult {
            exit_code: 0,
            stdout: Arc::new(stdout),
            stderr: Arc::new(stderr),
            cached: true,
        };
    }

    write_session_log(
        &state.sessions,
        &sid,
        &format!(
            "multi-file: compiling {} of {} files",
            miss_sources.len(),
            compilations.len()
        ),
    );

    let response_family = staged_plan.response_family;
    handle_staged_multi_misses(
        state,
        sid,
        compiler,
        response_family,
        unit_results,
        staged_plan,
        cwd_path,
        client_env,
    )
    .await
}
