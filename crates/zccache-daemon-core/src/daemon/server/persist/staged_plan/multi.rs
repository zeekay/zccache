//! All-or-nothing private output plans for multi-source C/C++ requests.

use super::*;
use crate::compiler::{
    CacheableCompilation, CompilerFamily, MultiFileOutputLayout, MultiFileSourceArgument,
};
use std::collections::HashSet;

#[derive(Debug)]
pub(in crate::daemon::server) struct StagedMultiUnitPlan {
    pub(in crate::daemon::server) compilation_index: usize,
    pub(in crate::daemon::server) rewritten_args: Vec<String>,
    pub(in crate::daemon::server) outputs: Vec<StagedOutputPlan>,
    pub(in crate::daemon::server) staged_depfile: Option<NormalizedPath>,
}

impl StagedMultiUnitPlan {
    pub(in crate::daemon::server) fn staged_paths(&self) -> Vec<NormalizedPath> {
        self.outputs
            .iter()
            .map(|output| output.staged.clone())
            .collect()
    }

    pub(in crate::daemon::server) fn materialize(&self) -> io::Result<StagedMaterializationStats> {
        let mut observed = StagedMaterializationStats::default();
        for (index, output) in self.outputs.iter().enumerate() {
            #[cfg(test)]
            inject_staged_fault(
                &output.requested,
                StagedFaultPoint::MaterializeOutput(index),
            )?;
            #[cfg(not(test))]
            let _ = index;
            let one = materialize_independent_with_stats(&output.staged, &output.requested)
                .map_err(|error| materialization_error(error, observed))?;
            observed.add(one);
        }
        Ok(observed)
    }
}

#[derive(Debug)]
pub(in crate::daemon::server) struct StagedMultiCompilePlan {
    pub(in crate::daemon::server) units: Vec<StagedMultiUnitPlan>,
    pub(in crate::daemon::server) response_family: CompilerFamily,
    root: PathBuf,
}

fn remove_output_flags(args: &[String], msvc_syntax: bool) -> Vec<String> {
    let mut rewritten = Vec::with_capacity(args.len());
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let msvc_fo = msvc_syntax
            && arg.get(..3).is_some_and(|prefix| {
                prefix.eq_ignore_ascii_case("/fo") || prefix.eq_ignore_ascii_case("-fo")
            });
        if msvc_fo {
            if arg.len() == 3 {
                index = index.saturating_add(2);
            } else {
                index += 1;
            }
            continue;
        }
        if arg == "-o" {
            index = index.saturating_add(2);
            continue;
        }
        if arg.starts_with("-o") && arg.len() > 2 {
            index += 1;
            continue;
        }
        rewritten.push(arg.clone());
        index += 1;
    }
    rewritten
}

fn has_explicit_depfile(args: &[String]) -> bool {
    args.iter()
        .any(|arg| arg == "-MF" || arg.starts_with("-MF"))
}

fn user_requested_default_depfile(args: &[String]) -> bool {
    args.iter()
        .any(|arg| matches!(arg.as_str(), "-MD" | "-MMD"))
}

impl StagedMultiCompilePlan {
    #[allow(clippy::too_many_arguments)]
    pub(in crate::daemon::server) fn build(
        staging_dir: &Path,
        family: CompilerFamily,
        compilations: &[CacheableCompilation],
        original_args: &[String],
        source_arguments: &[MultiFileSourceArgument],
        output_layout: &MultiFileOutputLayout,
        cwd: &Path,
    ) -> StagedPlanOutcome<Self> {
        if !staged_lane_enabled(family) {
            return StagedPlanOutcome::Unsupported(StagedPlanReason::LaneDisabled);
        }
        if compilations.is_empty() || compilations.len() != source_arguments.len() {
            return StagedPlanOutcome::Unsupported(StagedPlanReason::NoDeclaredOutputs);
        }
        if matches!(output_layout, MultiFileOutputLayout::InvalidSingleOutput(_)) {
            return StagedPlanOutcome::Unsupported(StagedPlanReason::AmbiguousOutputArgument);
        }
        let msvc_syntax = family == CompilerFamily::Msvc
            || matches!(
                output_layout,
                MultiFileOutputLayout::MsvcPerSourceDefault
                    | MultiFileOutputLayout::MsvcDirectory(_)
            );
        if cc_has_unsupported_side_outputs(original_args) || has_explicit_depfile(original_args) {
            return StagedPlanOutcome::Unsupported(StagedPlanReason::UnmodeledSideOutput);
        }
        if compilations.iter().any(|compilation| {
            crate::compiler::OutputClassification::for_compiler(
                family,
                &compilation.output_file.to_string_lossy(),
            )
            .role
                != crate::compiler::OutputRole::Object
        }) {
            return StagedPlanOutcome::Unsupported(StagedPlanReason::UnsupportedOutputRole);
        }

        let requested_outputs: Vec<NormalizedPath> = compilations
            .iter()
            .map(|compilation| absolute(&compilation.output_file, cwd))
            .collect();
        let unique: HashSet<_> = requested_outputs.iter().collect();
        if unique.len() != requested_outputs.len() {
            return StagedPlanOutcome::Unsupported(StagedPlanReason::OutputNameCollision);
        }

        let root = staging_dir.join(format!(
            ".compile-multi-{}-{}",
            std::process::id(),
            PLAN_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        if let Err(source) = std::fs::create_dir_all(&root) {
            return StagedPlanOutcome::Error(planning_error(
                StagedPlanReason::StagingDirectoryCreate,
                source,
            ));
        }
        let result = (|| -> Result<StagedPlanOutcome<Self>, StagedPlanError> {
            let all_source_indices: HashSet<usize> = source_arguments
                .iter()
                .flat_map(|source| source.argument_indices.iter().copied())
                .collect();
            let preserve_depfile =
                family.supports_depfile() && user_requested_default_depfile(original_args);
            let has_dep_target = original_args.iter().any(|arg| {
                matches!(arg.as_str(), "-MT" | "-MQ")
                    || arg.starts_with("-MT")
                    || arg.starts_with("-MQ")
            });
            let mut units = Vec::with_capacity(compilations.len());
            for (compilation_index, (compilation, source_argument)) in
                compilations.iter().zip(source_arguments).enumerate()
            {
                let unit_root = root.join(format!("unit-{compilation_index}"));
                std::fs::create_dir_all(&unit_root).map_err(|source| {
                    planning_error(StagedPlanReason::StagingDirectoryCreate, source)
                })?;
                let requested = requested_outputs[compilation_index].clone();
                let file_name = requested
                    .file_name()
                    .ok_or_else(|| missing_filename(StagedPlanReason::OutputMissingFilename))?;
                let staged: NormalizedPath = unit_root.join(file_name).into();
                let keep: HashSet<usize> =
                    source_argument.argument_indices.iter().copied().collect();
                let filtered: Vec<String> = original_args
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| {
                        !all_source_indices.contains(index) || keep.contains(index)
                    })
                    .map(|(_, arg)| arg.clone())
                    .collect();
                let mut rewritten_args = remove_output_flags(&filtered, msvc_syntax);
                let mut private_flags = Vec::new();
                if msvc_syntax {
                    private_flags.push(format!("/Fo{}", staged.display()));
                } else {
                    private_flags.push("-o".to_string());
                    private_flags.push(staged.to_string_lossy().into_owned());
                }

                let mut outputs = vec![StagedOutputPlan {
                    requested: requested.clone(),
                    staged,
                }];
                let staged_depfile = (!msvc_syntax && family.supports_depfile()).then(|| {
                    let name = format!("{}.d", compilation_index);
                    NormalizedPath::from(unit_root.join(name))
                });
                if let Some(depfile) = staged_depfile.as_ref() {
                    if !preserve_depfile {
                        private_flags.push("-MD".to_string());
                    }
                    private_flags.push("-MF".to_string());
                    private_flags.push(depfile.to_string_lossy().into_owned());
                    if !has_dep_target {
                        private_flags.push("-MT".to_string());
                        private_flags.push(compilation.output_file.to_string_lossy().into_owned());
                    }
                    if preserve_depfile {
                        let requested_depfile: NormalizedPath =
                            requested.as_path().with_extension("d").into();
                        outputs.push(StagedOutputPlan {
                            requested: requested_depfile,
                            staged: depfile.clone(),
                        });
                    }
                }
                if msvc_syntax {
                    rewritten_args.extend(private_flags);
                } else {
                    let separator = rewritten_args
                        .iter()
                        .position(|arg| arg == "--")
                        .unwrap_or(rewritten_args.len());
                    rewritten_args.splice(separator..separator, private_flags);
                }
                units.push(StagedMultiUnitPlan {
                    compilation_index,
                    rewritten_args,
                    outputs,
                    staged_depfile,
                });
            }
            Ok(StagedPlanOutcome::Enabled(Self {
                units,
                response_family: if msvc_syntax {
                    CompilerFamily::Msvc
                } else {
                    family
                },
                root: root.clone(),
            }))
        })();
        if !matches!(result, Ok(StagedPlanOutcome::Enabled(_))) {
            let _ = std::fs::remove_dir_all(&root);
        }
        result.unwrap_or_else(StagedPlanOutcome::Error)
    }

    pub(in crate::daemon::server) fn cleanup(&self) -> io::Result<()> {
        std::fs::remove_dir_all(&self.root).or_else(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(error)
            }
        })
    }
}

impl Drop for StagedMultiCompilePlan {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compilation(source: &str, output: &str, args: &[String]) -> CacheableCompilation {
        CacheableCompilation {
            compiler: "clang".into(),
            family: CompilerFamily::Clang,
            source_file: source.into(),
            output_file: output.into(),
            original_args: args.to_vec().into(),
            unknown_flags: Vec::new(),
        }
    }

    #[test]
    fn gcc_units_keep_one_source_and_map_private_object_and_depfile() {
        let temp = tempfile::tempdir().unwrap();
        let args: Vec<String> = ["-c", "a.c", "b.c", "-MMD"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let compilations = [
            compilation("a.c", "a.o", &args),
            compilation("b.c", "b.o", &args),
        ];
        let source_arguments = [
            MultiFileSourceArgument {
                argument_indices: vec![1],
            },
            MultiFileSourceArgument {
                argument_indices: vec![2],
            },
        ];
        let plan = match StagedMultiCompilePlan::build(
            temp.path(),
            CompilerFamily::Clang,
            &compilations,
            &args,
            &source_arguments,
            &MultiFileOutputLayout::PerSourceDefault,
            temp.path(),
        ) {
            StagedPlanOutcome::Enabled(plan) => plan,
            other => panic!("expected enabled plan, got {other:?}"),
        };
        assert_eq!(plan.units.len(), 2);
        assert!(plan.units[0].rewritten_args.contains(&"a.c".to_string()));
        assert!(!plan.units[0].rewritten_args.contains(&"b.c".to_string()));
        assert!(plan.units[1].rewritten_args.contains(&"b.c".to_string()));
        assert!(!plan.units[1].rewritten_args.contains(&"a.c".to_string()));
        for unit in &plan.units {
            assert_eq!(unit.outputs.len(), 2);
            assert!(unit
                .rewritten_args
                .windows(2)
                .any(|pair| { pair[0] == "-MT" && !pair[1].contains(".compile-multi-") }));
            assert!(unit
                .staged_depfile
                .as_ref()
                .is_some_and(|path| { path.as_path().starts_with(temp.path()) }));
        }
    }

    #[test]
    fn gcc_private_flags_stay_before_end_of_options_separator() {
        let temp = tempfile::tempdir().unwrap();
        let args: Vec<String> = ["-c", "--", "-left.c", "-right.c"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let compilations = [
            compilation("-left.c", "-left.o", &args),
            compilation("-right.c", "-right.o", &args),
        ];
        let source_arguments = [
            MultiFileSourceArgument {
                argument_indices: vec![2],
            },
            MultiFileSourceArgument {
                argument_indices: vec![3],
            },
        ];
        let plan = match StagedMultiCompilePlan::build(
            temp.path(),
            CompilerFamily::Clang,
            &compilations,
            &args,
            &source_arguments,
            &MultiFileOutputLayout::PerSourceDefault,
            temp.path(),
        ) {
            StagedPlanOutcome::Enabled(plan) => plan,
            other => panic!("expected enabled plan, got {other:?}"),
        };
        for (index, unit) in plan.units.iter().enumerate() {
            let separator = unit
                .rewritten_args
                .iter()
                .position(|arg| arg == "--")
                .unwrap();
            for flag in ["-o", "-MF", "-MT"] {
                assert!(
                    unit.rewritten_args
                        .iter()
                        .position(|arg| arg == flag)
                        .unwrap()
                        < separator
                );
            }
            assert_eq!(
                &unit.rewritten_args[separator + 1],
                if index == 0 { "-left.c" } else { "-right.c" }
            );
        }
    }

    #[test]
    fn invalid_shared_output_and_explicit_depfile_fall_back_before_root_creation() {
        let temp = tempfile::tempdir().unwrap();
        let args = vec!["-c".into(), "a.c".into(), "b.c".into()];
        let compilations = [
            compilation("a.c", "a.o", &args),
            compilation("b.c", "b.o", &args),
        ];
        let source_arguments = [
            MultiFileSourceArgument {
                argument_indices: vec![1],
            },
            MultiFileSourceArgument {
                argument_indices: vec![2],
            },
        ];
        assert!(matches!(
            StagedMultiCompilePlan::build(
                temp.path(),
                CompilerFamily::Clang,
                &compilations,
                &args,
                &source_arguments,
                &MultiFileOutputLayout::InvalidSingleOutput("one.o".into()),
                temp.path(),
            ),
            StagedPlanOutcome::Unsupported(StagedPlanReason::AmbiguousOutputArgument)
        ));
        let mut explicit = args;
        explicit.extend(["-MF".into(), "shared.d".into()]);
        assert!(matches!(
            StagedMultiCompilePlan::build(
                temp.path(),
                CompilerFamily::Clang,
                &compilations,
                &explicit,
                &source_arguments,
                &MultiFileOutputLayout::PerSourceDefault,
                temp.path(),
            ),
            StagedPlanOutcome::Unsupported(StagedPlanReason::UnmodeledSideOutput)
        ));
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    #[test]
    fn same_stem_sources_in_different_directories_fall_back_on_output_collision() {
        let temp = tempfile::tempdir().unwrap();
        let args = vec!["-c".into(), "left/foo.c".into(), "right/foo.c".into()];
        let compilations = [
            compilation("left/foo.c", "foo.o", &args),
            compilation("right/foo.c", "foo.o", &args),
        ];
        let source_arguments = [
            MultiFileSourceArgument {
                argument_indices: vec![1],
            },
            MultiFileSourceArgument {
                argument_indices: vec![2],
            },
        ];

        assert!(matches!(
            StagedMultiCompilePlan::build(
                temp.path(),
                CompilerFamily::Clang,
                &compilations,
                &args,
                &source_arguments,
                &MultiFileOutputLayout::PerSourceDefault,
                temp.path(),
            ),
            StagedPlanOutcome::Unsupported(StagedPlanReason::OutputNameCollision)
        ));
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    #[test]
    fn shared_and_implicit_side_output_flags_fall_back_before_spawn() {
        let temp = tempfile::tempdir().unwrap();
        let sources = ["a.c", "b.c"];
        let source_arguments = [
            MultiFileSourceArgument {
                argument_indices: vec![1],
            },
            MultiFileSourceArgument {
                argument_indices: vec![2],
            },
        ];
        for flag in [
            "--serialize-diagnostics=shared.dia",
            "-dependency-file",
            "-MJshared.json",
            "/sourceDependencies:shared.json",
            "-save-temps=obj",
            "-gsplit-dwarf",
            "-gsplit-dwarf=single",
            "-fdump-tree-all",
            "-ftime-trace",
            "--coverage",
            "-fstack-usage",
            "-fcallgraph-info=su",
            "-fopt-info-vec=optimization.txt",
            "-fsave-optimization-record",
            "-foptimization-record-file=optimization.yaml",
            "/ifcOutputmodules",
            "/Zi",
            "/Ycprefix.h",
            "/FRbrowse.sbr",
            "/doccomments.xml",
        ] {
            let args = vec![
                "-c".into(),
                sources[0].into(),
                sources[1].into(),
                flag.into(),
            ];
            let compilations = [
                compilation(sources[0], "a.o", &args),
                compilation(sources[1], "b.o", &args),
            ];
            assert!(
                matches!(
                    StagedMultiCompilePlan::build(
                        temp.path(),
                        CompilerFamily::Clang,
                        &compilations,
                        &args,
                        &source_arguments,
                        &MultiFileOutputLayout::PerSourceDefault,
                        temp.path(),
                    ),
                    StagedPlanOutcome::Unsupported(StagedPlanReason::UnmodeledSideOutput)
                ),
                "flag must fall back: {flag}"
            );
        }
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    #[test]
    fn msvc_directory_plan_removes_original_fo_and_filters_source_spans() {
        let temp = tempfile::tempdir().unwrap();
        let args: Vec<String> = ["/c", "/Tc", "a.c", "/Tpb.cpp", "/Foobjects\\"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let mut first = compilation("a.c", "objects/a.obj", &args);
        first.family = CompilerFamily::Msvc;
        let mut second = compilation("b.cpp", "objects/b.obj", &args);
        second.family = CompilerFamily::Msvc;
        let source_arguments = [
            MultiFileSourceArgument {
                argument_indices: vec![1, 2],
            },
            MultiFileSourceArgument {
                argument_indices: vec![3],
            },
        ];
        let plan = match StagedMultiCompilePlan::build(
            temp.path(),
            CompilerFamily::Msvc,
            &[first, second],
            &args,
            &source_arguments,
            &MultiFileOutputLayout::MsvcDirectory("objects".into()),
            temp.path(),
        ) {
            StagedPlanOutcome::Enabled(plan) => plan,
            other => panic!("expected enabled plan, got {other:?}"),
        };
        assert!(plan.units[0]
            .rewritten_args
            .windows(2)
            .any(|pair| { pair[0].eq_ignore_ascii_case("/Tc") && pair[1] == "a.c" }));
        assert!(!plan.units[0]
            .rewritten_args
            .iter()
            .any(|arg| arg.eq_ignore_ascii_case("/Tpb.cpp")));
        assert!(!plan.units[0]
            .rewritten_args
            .iter()
            .any(|arg| arg.eq_ignore_ascii_case("/Foobjects\\")));
        assert!(plan.units[0].rewritten_args.iter().any(|arg| {
            arg.get(..3)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("/Fo"))
                && arg.contains(".compile-multi-")
        }));
    }
}
