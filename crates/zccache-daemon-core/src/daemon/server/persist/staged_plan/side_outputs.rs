//! Conservative C/C++ options that can create undeclared or shared outputs.

pub(super) fn cc_has_unsupported_side_outputs(args: &[String]) -> bool {
    args.iter().any(|arg| {
        let lower = arg.to_ascii_lowercase();
        lower == "--serialize-diagnostics"
            || lower.starts_with("--serialize-diagnostics=")
            || lower.starts_with("-dependency-file")
            || lower == "-mj"
            || lower.starts_with("-mj")
            || lower == "/sourcedependencies"
            || lower.starts_with("/sourcedependencies:")
            || lower == "-sourcedependencies"
            || lower.starts_with("-sourcedependencies:")
            || lower == "-save-temps"
            || lower.starts_with("-save-temps=")
            || lower.starts_with("-gsplit-dwarf")
            || lower.starts_with("-fdump-")
            || lower == "-ftime-trace"
            || lower.starts_with("-ftime-trace=")
            || lower == "--coverage"
            || lower == "-fprofile-arcs"
            || lower == "-ftest-coverage"
            || lower == "-fstack-usage"
            || lower.starts_with("-fcallgraph-info")
            || lower.starts_with("-fopt-info")
            || lower == "-fsave-optimization-record"
            || lower.starts_with("-foptimization-record-file")
            || lower.starts_with("-fmodule")
            || lower.starts_with("-Winvalid-pch")
            || lower.starts_with("/ifcoutput")
            || lower.starts_with("/headerunit")
            || lower.starts_with("/reference")
            || lower.starts_with("/fr")
            || lower.starts_with("/doc")
            || lower == "/zi"
            || lower.starts_with("/yc")
            || lower.starts_with("/fd")
            || lower.starts_with("/fp")
            || lower.starts_with("/fa")
            || lower.starts_with("/fi")
    })
}
