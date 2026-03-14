//! Linker detection and argument parsing for zccache.
//!
//! Handles parsing command-line arguments for `ld`, `lld`, MSVC `link.exe`,
//! and compiler drivers (`gcc`, `clang`) to determine cacheability for
//! linking (shared libraries, DLLs, and executables).

use std::path::PathBuf;

/// Supported linker tool families.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkerFamily {
    /// GNU ld (ld, ld.bfd, ld.gold, x86_64-linux-gnu-ld, etc.)
    Ld,
    /// LLVM lld (lld, ld.lld, lld-link, etc.)
    Lld,
    /// MSVC link.exe
    MsvcLink,
    /// Compiler driver used as linker (gcc, clang, etc.)
    CompilerDriver,
}

/// The result of parsing a linker invocation.
#[derive(Debug, Clone)]
pub enum ParsedLinkerInvocation {
    /// A cacheable link (shared library, DLL, or executable).
    Cacheable(CacheableLink),
    /// A non-cacheable invocation.
    NonCacheable {
        /// Reason why this invocation is not cacheable.
        reason: String,
    },
}

/// A cacheable link invocation (shared library, DLL, or executable).
#[derive(Debug, Clone)]
pub struct CacheableLink {
    /// The linker executable path.
    pub tool: PathBuf,
    /// The detected linker family.
    pub family: LinkerFamily,
    /// Input object files and libraries (order preserved — matters for linker).
    pub input_files: Vec<PathBuf>,
    /// The output file path (shared library, DLL, or executable).
    pub output_file: PathBuf,
    /// Secondary output files produced alongside the primary output.
    /// E.g., MSVC `/IMPLIB:foo.lib` produces `foo.lib` + `foo.exp`.
    /// May not all exist after linking — the server should skip missing ones.
    pub secondary_outputs: Vec<PathBuf>,
    /// Flags relevant to cache keying (optimization, target, etc.).
    pub cache_relevant_flags: Vec<String>,
    /// The full original argument list (for fallback execution).
    pub original_args: Vec<String>,
    /// Whether non-deterministic output is detected.
    pub non_deterministic: bool,
}

/// Check if a tool name is a known linker (not archiver).
#[must_use]
pub fn is_linker(tool: &str) -> bool {
    detect_family(tool).is_some()
}

/// Check if a tool invocation is a link operation (shared lib, DLL, or executable).
///
/// This checks both direct linkers (ld, lld, link.exe) and compiler drivers
/// used for linking. For compiler drivers, returns true when no compile-only
/// flag (`-c`, `-E`, `-S`) is present — this routes exe links to the link path.
/// Cases like `gcc main.c -o main` (compile+link) will be routed here too,
/// but the parser will find no object inputs and return NonCacheable → passthrough.
#[must_use]
pub fn is_link_invocation(tool: &str, args: &[String]) -> bool {
    if detect_family(tool).is_some() {
        return true;
    }
    // Compiler driver: it's a link invocation if no compile-only flag is present
    is_compiler_driver(tool) && !args.iter().any(|a| a == "-c" || a == "-E" || a == "-S")
}

/// Detect the linker family from the tool path/name.
fn detect_family(tool: &str) -> Option<LinkerFamily> {
    let path = std::path::Path::new(tool);

    // Get the full filename (e.g., "ld.lld", "ld.bfd") for dotted-name checks,
    // and the stem (e.g., "ld", "link") for simple-name checks.
    let full_name = path.file_name().and_then(|s| s.to_str()).unwrap_or(tool);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(tool);

    // MSVC link.exe (case-insensitive) — check stem so "link.exe" matches
    if stem.eq_ignore_ascii_case("link") {
        return Some(LinkerFamily::MsvcLink);
    }

    // LLVM lld variants: lld, ld.lld, ld.lld-17, lld-17, etc.
    // Check full_name first for dotted names, then stem for simple names.
    // Must come before GNU ld to avoid "ld.lld" matching as "ld".
    if full_name == "ld.lld"
        || full_name.starts_with("ld.lld-")
        || stem == "lld"
        || stem.starts_with("lld-")
    {
        return Some(LinkerFamily::Lld);
    }

    // GNU ld variants: ld, ld.bfd, ld.gold, x86_64-linux-gnu-ld, etc.
    // Check full_name for dotted names (ld.bfd, ld.gold), stem for plain "ld".
    if full_name == "ld.bfd" || full_name == "ld.gold" || stem == "ld" || stem.ends_with("-ld") {
        return Some(LinkerFamily::Ld);
    }

    None
}

/// Check if a tool name is a compiler driver (gcc, g++, clang, clang++, cc, c++).
fn is_compiler_driver(tool: &str) -> bool {
    let stem = std::path::Path::new(tool)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(tool);

    // clang++, clang-17, x86_64-w64-mingw32-gcc, etc.
    matches!(stem, "cc" | "c++")
        || stem == "gcc"
        || stem == "g++"
        || stem.ends_with("-gcc")
        || stem.ends_with("-g++")
        || stem.contains("clang")
}

/// Parse a linker invocation's arguments to determine cacheability.
///
/// Handles direct linkers (ld, lld, link.exe) and compiler drivers used
/// for linking (gcc, clang). Both shared library and executable linking
/// are cacheable.
#[must_use]
pub fn parse_linker_invocation(tool: &str, args: &[String]) -> ParsedLinkerInvocation {
    // Try direct linker first
    if let Some(family) = detect_family(tool) {
        return match family {
            LinkerFamily::MsvcLink => parse_msvc_link(tool, args),
            LinkerFamily::Ld | LinkerFamily::Lld => parse_gnu_ld(tool, family, args),
            LinkerFamily::CompilerDriver => parse_compiler_driver_link(tool, args),
        };
    }

    // Try compiler driver (gcc -shared, clang -shared, etc.)
    if is_compiler_driver(tool) {
        return parse_compiler_driver_link(tool, args);
    }

    ParsedLinkerInvocation::NonCacheable {
        reason: format!("not a recognized linker: {tool}"),
    }
}

/// Parse GNU ld / lld arguments for linking.
///
/// Both shared library (`-shared` / `-dylib`) and executable linking are cacheable.
/// `-shared` / `-dylib` are kept as cache-relevant flags since they affect output.
fn parse_gnu_ld(tool: &str, family: LinkerFamily, args: &[String]) -> ParsedLinkerInvocation {
    if args.is_empty() {
        return ParsedLinkerInvocation::NonCacheable {
            reason: "no arguments".to_string(),
        };
    }

    let mut output_file: Option<PathBuf> = None;
    let mut input_files: Vec<PathBuf> = Vec::new();
    let mut cache_relevant_flags: Vec<String> = Vec::new();
    let mut has_build_id_uuid = false;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];

        // -shared or --shared — shared library mode (cache-relevant: affects output type)
        if arg == "-shared" || arg == "--shared" {
            cache_relevant_flags.push(arg.clone());
            i += 1;
            continue;
        }

        // macOS: -dylib (cache-relevant: affects output type)
        if arg == "-dylib" {
            cache_relevant_flags.push(arg.clone());
            i += 1;
            continue;
        }

        // -o <output> or --output=<output>
        if arg == "-o" {
            i += 1;
            if i < args.len() {
                output_file = Some(PathBuf::from(&args[i]));
            }
            i += 1;
            continue;
        }
        if let Some(rest) = arg.strip_prefix("--output=") {
            output_file = Some(PathBuf::from(rest));
            i += 1;
            continue;
        }

        // --build-id=uuid → non-deterministic
        if arg == "--build-id=uuid" {
            has_build_id_uuid = true;
            cache_relevant_flags.push(arg.clone());
            i += 1;
            continue;
        }

        // --build-id=<style> (sha1, md5, none, etc.) → deterministic
        if arg.starts_with("--build-id") {
            cache_relevant_flags.push(arg.clone());
            i += 1;
            continue;
        }

        // -soname <name> or --soname=<name>
        if arg == "-soname" || arg == "-h" {
            cache_relevant_flags.push(arg.clone());
            i += 1;
            if i < args.len() {
                cache_relevant_flags.push(args[i].clone());
            }
            i += 1;
            continue;
        }
        if let Some(rest) = arg.strip_prefix("--soname=") {
            cache_relevant_flags.push(format!("--soname={rest}"));
            i += 1;
            continue;
        }

        // macOS: -install_name <name>
        if arg == "-install_name" {
            cache_relevant_flags.push(arg.clone());
            i += 1;
            if i < args.len() {
                cache_relevant_flags.push(args[i].clone());
            }
            i += 1;
            continue;
        }

        // -L<path> or -L <path> — library search path (cache-relevant)
        if arg == "-L" {
            cache_relevant_flags.push(arg.clone());
            i += 1;
            if i < args.len() {
                cache_relevant_flags.push(args[i].clone());
            }
            i += 1;
            continue;
        }
        if arg.starts_with("-L") {
            cache_relevant_flags.push(arg.clone());
            i += 1;
            continue;
        }

        // -l<lib> — library dependency (cache-relevant, order matters)
        if arg.starts_with("-l") {
            cache_relevant_flags.push(arg.clone());
            i += 1;
            continue;
        }

        // -Wl, pass-through (from compiler driver)
        if arg.starts_with("-Wl,") {
            cache_relevant_flags.push(arg.clone());
            i += 1;
            continue;
        }

        // Flags that take a value in the next arg
        if arg == "-T" || arg == "--script" || arg == "-z" || arg == "--version-script" {
            cache_relevant_flags.push(arg.clone());
            i += 1;
            if i < args.len() {
                cache_relevant_flags.push(args[i].clone());
                // -T (linker script) and --version-script are input files that affect output
                if arg == "-T" || arg == "--script" || arg == "--version-script" {
                    input_files.push(PathBuf::from(&args[i]));
                }
            }
            i += 1;
            continue;
        }

        // Flags with = syntax
        if let Some(rest) = arg.strip_prefix("--version-script=") {
            cache_relevant_flags.push(arg.clone());
            input_files.push(PathBuf::from(rest));
            i += 1;
            continue;
        }

        // Other flags
        if arg.starts_with('-') {
            cache_relevant_flags.push(arg.clone());
            i += 1;
            continue;
        }

        // Positional argument — input file (object file or library)
        input_files.push(PathBuf::from(arg));
        i += 1;
    }

    let output_file = match output_file {
        Some(f) => f,
        None => {
            return ParsedLinkerInvocation::NonCacheable {
                reason: "no output file specified (-o)".to_string(),
            };
        }
    };

    if input_files.is_empty() {
        return ParsedLinkerInvocation::NonCacheable {
            reason: "no input files specified".to_string(),
        };
    }

    ParsedLinkerInvocation::Cacheable(CacheableLink {
        tool: PathBuf::from(tool),
        family,
        input_files,
        output_file,
        secondary_outputs: Vec::new(),
        cache_relevant_flags,
        original_args: args.to_vec(),
        non_deterministic: has_build_id_uuid,
    })
}

/// Parse MSVC link.exe arguments for linking (DLL or executable).
///
/// Both `/DLL` (DLL) and non-`/DLL` (executable) invocations are cacheable.
/// `/DLL` is kept as a cache-relevant flag since it affects output type.
fn parse_msvc_link(tool: &str, args: &[String]) -> ParsedLinkerInvocation {
    if args.is_empty() {
        return ParsedLinkerInvocation::NonCacheable {
            reason: "no arguments".to_string(),
        };
    }

    let mut is_dll = false;
    let mut output_file: Option<PathBuf> = None;
    let mut input_files: Vec<PathBuf> = Vec::new();
    let mut cache_relevant_flags: Vec<String> = Vec::new();
    let mut has_deterministic = false;
    let mut secondary_outputs: Vec<PathBuf> = Vec::new();

    for arg in args {
        let upper = arg.to_uppercase();

        // /DLL — DLL mode (cache-relevant: affects output type)
        if upper == "/DLL" || upper == "-DLL" {
            is_dll = true;
            cache_relevant_flags.push(arg.clone());
            continue;
        }

        // /OUT:filename
        if upper.starts_with("/OUT:") || upper.starts_with("-OUT:") {
            output_file = Some(PathBuf::from(&arg[5..]));
            continue;
        }

        // /DETERMINISTIC
        if upper == "/DETERMINISTIC" || upper == "-DETERMINISTIC" {
            has_deterministic = true;
            cache_relevant_flags.push(arg.clone());
            continue;
        }

        // /IMPLIB:filename — import library (secondary output)
        // MSVC also auto-generates a .exp alongside the .lib
        if upper.starts_with("/IMPLIB:") || upper.starts_with("-IMPLIB:") {
            let implib_path = PathBuf::from(&arg[8..]);
            let exp_path = implib_path.with_extension("exp");
            secondary_outputs.push(implib_path);
            secondary_outputs.push(exp_path);
            cache_relevant_flags.push(arg.clone());
            continue;
        }

        // Other flags
        if arg.starts_with('/') || arg.starts_with('-') {
            cache_relevant_flags.push(arg.clone());
            continue;
        }

        // Positional — input file
        input_files.push(PathBuf::from(arg));
    }

    if input_files.is_empty() {
        return ParsedLinkerInvocation::NonCacheable {
            reason: "no input files specified".to_string(),
        };
    }

    // If no /OUT:, link.exe defaults to first input with .dll/.exe extension
    let output_file = output_file.unwrap_or_else(|| {
        let first = &input_files[0];
        let ext = if is_dll { "dll" } else { "exe" };
        first.with_extension(ext)
    });

    ParsedLinkerInvocation::Cacheable(CacheableLink {
        tool: PathBuf::from(tool),
        family: LinkerFamily::MsvcLink,
        input_files,
        output_file,
        secondary_outputs,
        cache_relevant_flags,
        original_args: args.to_vec(),
        non_deterministic: !has_deterministic,
    })
}

/// Object/library file extensions recognized as linker inputs.
const OBJECT_EXTENSIONS: &[&str] = &["o", "obj", "a", "lib", "lo", "so", "dylib", "dll"];

/// Check if a path looks like a linker input file (object, archive, library).
fn is_linker_input(path: &str) -> bool {
    if let Some(ext) = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
    {
        OBJECT_EXTENSIONS.contains(&ext)
    } else {
        false
    }
}

/// Parse a compiler driver invocation used for linking.
///
/// Handles `gcc -shared -o libfoo.so a.o b.o`, `gcc -o main main.o`, and similar.
/// The compiler driver passes flags through to the linker internally,
/// so we treat the full invocation as a link operation. `-shared` is kept as a
/// cache-relevant flag since it affects output type.
fn parse_compiler_driver_link(tool: &str, args: &[String]) -> ParsedLinkerInvocation {
    if args.is_empty() {
        return ParsedLinkerInvocation::NonCacheable {
            reason: "no arguments".to_string(),
        };
    }

    let mut has_compile_only = false;
    let mut output_file: Option<PathBuf> = None;
    let mut input_files: Vec<PathBuf> = Vec::new();
    let mut cache_relevant_flags: Vec<String> = Vec::new();
    let mut has_build_id_uuid = false;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];

        // -shared — shared library mode (cache-relevant: affects output type)
        if arg == "-shared" || arg == "--shared" {
            cache_relevant_flags.push(arg.clone());
            i += 1;
            continue;
        }

        // -c — compile only, NOT linking
        if arg == "-c" {
            has_compile_only = true;
            i += 1;
            continue;
        }

        // -o <output>
        if arg == "-o" {
            i += 1;
            if i < args.len() {
                output_file = Some(PathBuf::from(&args[i]));
            }
            i += 1;
            continue;
        }

        // -Wl, pass-through to linker — check for non-determinism
        if arg.starts_with("-Wl,") {
            for part in arg.split(',') {
                if part == "--build-id=uuid" {
                    has_build_id_uuid = true;
                }
            }
            cache_relevant_flags.push(arg.clone());
            i += 1;
            continue;
        }

        // -L<path> or -L <path>
        if arg == "-L" {
            cache_relevant_flags.push(arg.clone());
            i += 1;
            if i < args.len() {
                cache_relevant_flags.push(args[i].clone());
            }
            i += 1;
            continue;
        }
        if arg.starts_with("-L") {
            cache_relevant_flags.push(arg.clone());
            i += 1;
            continue;
        }

        // -l<lib>
        if arg.starts_with("-l") {
            cache_relevant_flags.push(arg.clone());
            i += 1;
            continue;
        }

        // Flags with value: -target, -isysroot, etc.
        if arg == "-target" || arg == "--target" || arg == "-isysroot" {
            cache_relevant_flags.push(arg.clone());
            i += 1;
            if i < args.len() {
                cache_relevant_flags.push(args[i].clone());
            }
            i += 1;
            continue;
        }

        // Other flags
        if arg.starts_with('-') {
            cache_relevant_flags.push(arg.clone());
            i += 1;
            continue;
        }

        // Positional argument — input file (object or source)
        if is_linker_input(arg) {
            input_files.push(PathBuf::from(arg));
        }
        // Ignore non-object positional args (e.g., source files passed to gcc
        // during combined compile-and-link — too complex to cache)
        i += 1;
    }

    if has_compile_only {
        return ParsedLinkerInvocation::NonCacheable {
            reason: "-c flag present (compilation, not linking)".to_string(),
        };
    }

    let output_file = match output_file {
        Some(f) => f,
        None => {
            return ParsedLinkerInvocation::NonCacheable {
                reason: "no output file specified (-o)".to_string(),
            };
        }
    };

    if input_files.is_empty() {
        return ParsedLinkerInvocation::NonCacheable {
            reason: "no input files specified".to_string(),
        };
    }

    ParsedLinkerInvocation::Cacheable(CacheableLink {
        tool: PathBuf::from(tool),
        family: LinkerFamily::CompilerDriver,
        input_files,
        output_file,
        secondary_outputs: Vec::new(),
        cache_relevant_flags,
        original_args: args.to_vec(),
        non_deterministic: has_build_id_uuid,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    // ─── Detection ─────────────────────────────────────────────────────

    #[test]
    fn detect_gnu_ld() {
        assert_eq!(detect_family("ld"), Some(LinkerFamily::Ld));
        assert_eq!(detect_family("/usr/bin/ld"), Some(LinkerFamily::Ld));
        assert_eq!(detect_family("ld.bfd"), Some(LinkerFamily::Ld));
        assert_eq!(detect_family("ld.gold"), Some(LinkerFamily::Ld));
        assert_eq!(detect_family("x86_64-linux-gnu-ld"), Some(LinkerFamily::Ld));
        assert_eq!(
            detect_family("aarch64-linux-gnu-ld"),
            Some(LinkerFamily::Ld)
        );
    }

    #[test]
    fn detect_llvm_lld() {
        assert_eq!(detect_family("lld"), Some(LinkerFamily::Lld));
        assert_eq!(detect_family("lld-17"), Some(LinkerFamily::Lld));
        assert_eq!(detect_family("ld.lld"), Some(LinkerFamily::Lld));
        assert_eq!(detect_family("ld.lld-17"), Some(LinkerFamily::Lld));
        assert_eq!(detect_family("/usr/bin/lld"), Some(LinkerFamily::Lld));
    }

    #[test]
    fn detect_msvc_link() {
        assert_eq!(detect_family("link"), Some(LinkerFamily::MsvcLink));
        assert_eq!(detect_family("link.exe"), Some(LinkerFamily::MsvcLink));
        assert_eq!(detect_family("LINK"), Some(LinkerFamily::MsvcLink));
        assert_eq!(detect_family("LINK.EXE"), Some(LinkerFamily::MsvcLink));
    }

    #[test]
    fn detect_unknown_tool() {
        assert_eq!(detect_family("gcc"), None);
        assert_eq!(detect_family("clang"), None);
        assert_eq!(detect_family("ar"), None);
        assert_eq!(detect_family("lib.exe"), None);
    }

    #[test]
    fn is_linker_works() {
        assert!(is_linker("ld"));
        assert!(is_linker("lld"));
        assert!(is_linker("link.exe"));
        assert!(!is_linker("gcc"));
        assert!(!is_linker("ar"));
        assert!(!is_linker("lib.exe"));
    }

    // ─── GNU ld shared library parsing ────────────────────────────────

    #[test]
    fn basic_shared_lib() {
        let result =
            parse_linker_invocation("ld", &args(&["-shared", "-o", "libfoo.so", "a.o", "b.o"]));
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.family, LinkerFamily::Ld);
                assert_eq!(c.output_file, PathBuf::from("libfoo.so"));
                assert_eq!(c.input_files.len(), 2);
                assert_eq!(c.input_files[0], PathBuf::from("a.o"));
                assert_eq!(c.input_files[1], PathBuf::from("b.o"));
                assert!(!c.non_deterministic); // GNU ld is deterministic by default
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn shared_lib_with_soname() {
        let result = parse_linker_invocation(
            "ld",
            &args(&[
                "-shared",
                "-soname",
                "libfoo.so.1",
                "-o",
                "libfoo.so.1.0",
                "a.o",
            ]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.output_file, PathBuf::from("libfoo.so.1.0"));
                assert!(c.cache_relevant_flags.contains(&"-soname".to_string()));
                assert!(c.cache_relevant_flags.contains(&"libfoo.so.1".to_string()));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn shared_lib_with_libraries() {
        let result = parse_linker_invocation(
            "ld",
            &args(&[
                "-shared",
                "-o",
                "libfoo.so",
                "a.o",
                "-lm",
                "-lpthread",
                "-L/usr/lib",
            ]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.input_files, vec![PathBuf::from("a.o")]);
                assert!(c.cache_relevant_flags.contains(&"-lm".to_string()));
                assert!(c.cache_relevant_flags.contains(&"-lpthread".to_string()));
                assert!(c.cache_relevant_flags.contains(&"-L/usr/lib".to_string()));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn exe_link_cacheable() {
        // Executable linking (no -shared) should be cacheable
        let result = parse_linker_invocation("ld", &args(&["-o", "a.out", "main.o"]));
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.family, LinkerFamily::Ld);
                assert_eq!(c.output_file, PathBuf::from("a.out"));
                assert_eq!(c.input_files, vec![PathBuf::from("main.o")]);
                assert!(!c.non_deterministic);
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn no_output_non_cacheable() {
        let result = parse_linker_invocation("ld", &args(&["-shared", "a.o"]));
        assert!(matches!(
            result,
            ParsedLinkerInvocation::NonCacheable { .. }
        ));
    }

    #[test]
    fn no_inputs_non_cacheable() {
        let result = parse_linker_invocation("ld", &args(&["-shared", "-o", "libfoo.so"]));
        assert!(matches!(
            result,
            ParsedLinkerInvocation::NonCacheable { .. }
        ));
    }

    #[test]
    fn no_args_non_cacheable() {
        let result = parse_linker_invocation("ld", &args(&[]));
        assert!(matches!(
            result,
            ParsedLinkerInvocation::NonCacheable { .. }
        ));
    }

    #[test]
    fn preserves_input_order() {
        let result = parse_linker_invocation(
            "ld",
            &args(&["-shared", "-o", "libfoo.so", "z.o", "a.o", "m.o"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.input_files[0], PathBuf::from("z.o"));
                assert_eq!(c.input_files[1], PathBuf::from("a.o"));
                assert_eq!(c.input_files[2], PathBuf::from("m.o"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    // ─── Non-determinism (timestamps, build-id) ──────────────────────

    #[test]
    fn build_id_uuid_is_non_deterministic() {
        let result = parse_linker_invocation(
            "ld",
            &args(&["-shared", "--build-id=uuid", "-o", "libfoo.so", "a.o"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert!(
                    c.non_deterministic,
                    "--build-id=uuid produces random output — must be flagged"
                );
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn build_id_sha1_is_deterministic() {
        let result = parse_linker_invocation(
            "ld",
            &args(&["-shared", "--build-id=sha1", "-o", "libfoo.so", "a.o"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert!(
                    !c.non_deterministic,
                    "--build-id=sha1 is content-derived — deterministic"
                );
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn build_id_none_is_deterministic() {
        let result = parse_linker_invocation(
            "ld",
            &args(&["-shared", "--build-id=none", "-o", "libfoo.so", "a.o"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert!(!c.non_deterministic);
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn default_ld_is_deterministic() {
        // GNU ld without --build-id is deterministic (no random build ID inserted)
        let result = parse_linker_invocation("ld", &args(&["-shared", "-o", "libfoo.so", "a.o"]));
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert!(!c.non_deterministic);
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    // ─── macOS dylib ──────────────────────────────────────────────────

    #[test]
    fn macos_dylib() {
        let result =
            parse_linker_invocation("ld", &args(&["-dylib", "-o", "libfoo.dylib", "a.o", "b.o"]));
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.output_file, PathBuf::from("libfoo.dylib"));
                assert_eq!(c.input_files.len(), 2);
                assert!(!c.non_deterministic);
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn macos_dylib_with_install_name() {
        let result = parse_linker_invocation(
            "ld",
            &args(&[
                "-dylib",
                "-install_name",
                "@rpath/libfoo.dylib",
                "-o",
                "libfoo.dylib",
                "a.o",
            ]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert!(c
                    .cache_relevant_flags
                    .contains(&"-install_name".to_string()));
                assert!(c
                    .cache_relevant_flags
                    .contains(&"@rpath/libfoo.dylib".to_string()));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    // ─── LLD ──────────────────────────────────────────────────────────

    #[test]
    fn lld_shared_lib() {
        let result = parse_linker_invocation(
            "ld.lld",
            &args(&["-shared", "-o", "libfoo.so", "a.o", "b.o"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.family, LinkerFamily::Lld);
                assert_eq!(c.output_file, PathBuf::from("libfoo.so"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    // ─── Linker script and version script ─────────────────────────────

    #[test]
    fn with_linker_script() {
        let result = parse_linker_invocation(
            "ld",
            &args(&["-shared", "-T", "link.ld", "-o", "libfoo.so", "a.o"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                // Linker script is an input file (affects output)
                assert!(c.input_files.contains(&PathBuf::from("link.ld")));
                assert!(c.input_files.contains(&PathBuf::from("a.o")));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn with_version_script() {
        let result = parse_linker_invocation(
            "ld",
            &args(&[
                "-shared",
                "--version-script=libfoo.map",
                "-o",
                "libfoo.so",
                "a.o",
            ]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                // Version script is an input file
                assert!(c.input_files.contains(&PathBuf::from("libfoo.map")));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    // ─── MSVC link.exe DLL parsing ────────────────────────────────────

    #[test]
    fn basic_msvc_dll() {
        let result = parse_linker_invocation(
            "link.exe",
            &args(&["/DLL", "/OUT:foo.dll", "a.obj", "b.obj"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.family, LinkerFamily::MsvcLink);
                assert_eq!(c.output_file, PathBuf::from("foo.dll"));
                assert_eq!(c.input_files.len(), 2);
                assert_eq!(c.input_files[0], PathBuf::from("a.obj"));
                assert_eq!(c.input_files[1], PathBuf::from("b.obj"));
                assert!(c.non_deterministic); // no /DETERMINISTIC
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn msvc_dll_with_deterministic() {
        let result = parse_linker_invocation(
            "link.exe",
            &args(&["/DLL", "/DETERMINISTIC", "/OUT:foo.dll", "a.obj"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert!(!c.non_deterministic); // /DETERMINISTIC present
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn msvc_exe_cacheable() {
        // Executable linking (no /DLL) should be cacheable
        let result = parse_linker_invocation("link.exe", &args(&["/OUT:foo.exe", "main.obj"]));
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.family, LinkerFamily::MsvcLink);
                assert_eq!(c.output_file, PathBuf::from("foo.exe"));
                assert_eq!(c.input_files, vec![PathBuf::from("main.obj")]);
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn msvc_exe_default_output_name() {
        // Without /OUT: and without /DLL, defaults to first input with .exe extension
        let result = parse_linker_invocation("link.exe", &args(&["main.obj", "util.obj"]));
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.output_file, PathBuf::from("main.exe"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn msvc_dll_no_inputs() {
        let result = parse_linker_invocation("link.exe", &args(&["/DLL", "/OUT:foo.dll"]));
        assert!(matches!(
            result,
            ParsedLinkerInvocation::NonCacheable { .. }
        ));
    }

    #[test]
    fn msvc_dll_default_output_name() {
        // Without /OUT:, defaults to first input with .dll extension
        let result = parse_linker_invocation("link.exe", &args(&["/DLL", "a.obj", "b.obj"]));
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.output_file, PathBuf::from("a.dll"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn msvc_dll_preserves_input_order() {
        let result = parse_linker_invocation(
            "link.exe",
            &args(&["/DLL", "/OUT:foo.dll", "z.obj", "a.obj", "m.obj"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.input_files[0], PathBuf::from("z.obj"));
                assert_eq!(c.input_files[1], PathBuf::from("a.obj"));
                assert_eq!(c.input_files[2], PathBuf::from("m.obj"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn msvc_dll_with_implib() {
        let result = parse_linker_invocation(
            "link.exe",
            &args(&["/DLL", "/OUT:foo.dll", "/IMPLIB:foo.lib", "a.obj"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert!(c
                    .cache_relevant_flags
                    .contains(&"/IMPLIB:foo.lib".to_string()));
                // /IMPLIB: extracts secondary outputs: .lib + inferred .exp
                assert_eq!(c.secondary_outputs.len(), 2);
                assert_eq!(c.secondary_outputs[0], PathBuf::from("foo.lib"));
                assert_eq!(c.secondary_outputs[1], PathBuf::from("foo.exp"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn msvc_dll_without_implib_no_secondary() {
        let result = parse_linker_invocation("link.exe", &args(&["/DLL", "/OUT:foo.dll", "a.obj"]));
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert!(c.secondary_outputs.is_empty());
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn msvc_implib_dash_syntax() {
        let result = parse_linker_invocation(
            "link.exe",
            &args(&["/DLL", "-IMPLIB:mylib.lib", "/OUT:mylib.dll", "a.obj"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.secondary_outputs.len(), 2);
                assert_eq!(c.secondary_outputs[0], PathBuf::from("mylib.lib"));
                assert_eq!(c.secondary_outputs[1], PathBuf::from("mylib.exp"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn gnu_ld_no_secondary_outputs() {
        let result = parse_linker_invocation("ld", &args(&["-shared", "-o", "libfoo.so", "a.o"]));
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert!(c.secondary_outputs.is_empty());
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn gcc_no_secondary_outputs() {
        let result = parse_linker_invocation("gcc", &args(&["-shared", "-o", "libfoo.so", "a.o"]));
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert!(c.secondary_outputs.is_empty());
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn msvc_dll_with_flags() {
        let result = parse_linker_invocation(
            "link.exe",
            &args(&["/DLL", "/NOLOGO", "/MACHINE:X64", "/OUT:foo.dll", "a.obj"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert!(c.cache_relevant_flags.contains(&"/NOLOGO".to_string()));
                assert!(c.cache_relevant_flags.contains(&"/MACHINE:X64".to_string()));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn msvc_dll_dash_syntax() {
        // link.exe also accepts - prefix for flags
        let result = parse_linker_invocation(
            "link.exe",
            &args(&["-DLL", "-OUT:foo.dll", "-DETERMINISTIC", "a.obj"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.output_file, PathBuf::from("foo.dll"));
                assert!(!c.non_deterministic);
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn msvc_no_args() {
        let result = parse_linker_invocation("link.exe", &args(&[]));
        assert!(matches!(
            result,
            ParsedLinkerInvocation::NonCacheable { .. }
        ));
    }

    // ─── Unknown tool ──────────────────────────────────────────────────

    #[test]
    fn unknown_tool_non_cacheable() {
        let result =
            parse_linker_invocation("rustc", &args(&["-shared", "-o", "libfoo.so", "a.o"]));
        assert!(matches!(
            result,
            ParsedLinkerInvocation::NonCacheable { .. }
        ));
    }

    // ─── Cross-compile linker ──────────────────────────────────────────

    #[test]
    fn cross_compile_ld() {
        let result = parse_linker_invocation(
            "x86_64-linux-gnu-ld",
            &args(&["-shared", "-o", "libfoo.so", "a.o"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.family, LinkerFamily::Ld);
                assert_eq!(c.tool, PathBuf::from("x86_64-linux-gnu-ld"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    // ─── --output= syntax ──────────────────────────────────────────────

    #[test]
    fn output_equals_syntax() {
        let result =
            parse_linker_invocation("ld", &args(&["-shared", "--output=libfoo.so", "a.o"]));
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.output_file, PathBuf::from("libfoo.so"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    // ─── Edge cases: -z flags, -rpath, mixed inputs ───────────────────

    #[test]
    fn z_relro_and_now_flags() {
        let result = parse_linker_invocation(
            "ld",
            &args(&[
                "-shared",
                "-z",
                "relro",
                "-z",
                "now",
                "-o",
                "libfoo.so",
                "a.o",
            ]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert!(c.cache_relevant_flags.contains(&"-z".to_string()));
                assert!(c.cache_relevant_flags.contains(&"relro".to_string()));
                assert!(c.cache_relevant_flags.contains(&"now".to_string()));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn rpath_flag() {
        let result = parse_linker_invocation(
            "ld",
            &args(&["-shared", "-rpath", "/usr/lib", "-o", "libfoo.so", "a.o"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                // -rpath is consumed as a generic flag
                assert!(c.cache_relevant_flags.contains(&"-rpath".to_string()));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn mixed_object_and_archive_inputs() {
        let result = parse_linker_invocation(
            "ld",
            &args(&["-shared", "-o", "libfoo.so", "a.o", "libbar.a", "c.o"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.input_files.len(), 3);
                assert_eq!(c.input_files[0], PathBuf::from("a.o"));
                assert_eq!(c.input_files[1], PathBuf::from("libbar.a"));
                assert_eq!(c.input_files[2], PathBuf::from("c.o"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn soname_equals_syntax() {
        let result = parse_linker_invocation(
            "ld",
            &args(&[
                "-shared",
                "--soname=libfoo.so.1",
                "-o",
                "libfoo.so.1.0",
                "a.o",
            ]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert!(c
                    .cache_relevant_flags
                    .contains(&"--soname=libfoo.so.1".to_string()));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn version_script_separate_args() {
        let result = parse_linker_invocation(
            "ld",
            &args(&[
                "-shared",
                "--version-script",
                "libfoo.map",
                "-o",
                "libfoo.so",
                "a.o",
            ]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert!(c.input_files.contains(&PathBuf::from("libfoo.map")));
                assert!(c.input_files.contains(&PathBuf::from("a.o")));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn redundant_shared_flags() {
        // Multiple -shared flags are valid and shouldn't cause issues
        let result = parse_linker_invocation(
            "ld",
            &args(&["-shared", "--shared", "-o", "libfoo.so", "a.o"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.output_file, PathBuf::from("libfoo.so"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn wl_shared_inside_pass_through() {
        // -Wl,-shared inside a -Wl, pass-through should detect shared mode
        let result =
            parse_linker_invocation("ld", &args(&["-Wl,-shared", "-o", "libfoo.so", "a.o"]));
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.output_file, PathBuf::from("libfoo.so"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn msvc_def_file_as_flag() {
        let result = parse_linker_invocation(
            "link.exe",
            &args(&["/DLL", "/DEF:foo.def", "/OUT:foo.dll", "a.obj"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert!(c.cache_relevant_flags.contains(&"/DEF:foo.def".to_string()));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn msvc_case_insensitive_dll_flag() {
        let result = parse_linker_invocation("link.exe", &args(&["/dll", "/out:foo.dll", "a.obj"]));
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.output_file, PathBuf::from("foo.dll"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    // ─── Compiler driver as linker (gcc -shared, clang -shared) ───────

    #[test]
    fn compiler_driver_detection() {
        assert!(is_compiler_driver("gcc"));
        assert!(is_compiler_driver("g++"));
        assert!(is_compiler_driver("clang"));
        assert!(is_compiler_driver("clang++"));
        assert!(is_compiler_driver("clang-17"));
        assert!(is_compiler_driver("cc"));
        assert!(is_compiler_driver("c++"));
        assert!(is_compiler_driver("/usr/bin/gcc"));
        assert!(is_compiler_driver("x86_64-w64-mingw32-gcc"));
        assert!(is_compiler_driver("x86_64-w64-mingw32-g++"));
        assert!(!is_compiler_driver("ld"));
        assert!(!is_compiler_driver("ar"));
        assert!(!is_compiler_driver("rustc"));
    }

    #[test]
    fn gcc_shared_basic() {
        let result =
            parse_linker_invocation("gcc", &args(&["-shared", "-o", "libfoo.so", "a.o", "b.o"]));
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.family, LinkerFamily::CompilerDriver);
                assert_eq!(c.output_file, PathBuf::from("libfoo.so"));
                assert_eq!(c.input_files.len(), 2);
                assert!(!c.non_deterministic);
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn clang_shared_dll() {
        let result =
            parse_linker_invocation("clang", &args(&["-shared", "-o", "foo.dll", "a.o", "b.o"]));
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.family, LinkerFamily::CompilerDriver);
                assert_eq!(c.output_file, PathBuf::from("foo.dll"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn gpp_shared_with_flags() {
        let result = parse_linker_invocation(
            "g++",
            &args(&["-shared", "-fPIC", "-O2", "-o", "libfoo.so", "a.o", "-lm"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.input_files, vec![PathBuf::from("a.o")]);
                assert!(c.cache_relevant_flags.contains(&"-fPIC".to_string()));
                assert!(c.cache_relevant_flags.contains(&"-O2".to_string()));
                assert!(c.cache_relevant_flags.contains(&"-lm".to_string()));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn gcc_shared_wl_build_id_uuid_non_deterministic() {
        let result = parse_linker_invocation(
            "gcc",
            &args(&["-shared", "-Wl,--build-id=uuid", "-o", "libfoo.so", "a.o"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert!(c.non_deterministic, "-Wl,--build-id=uuid must be flagged");
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn gcc_with_compile_flag_non_cacheable() {
        // gcc -c -shared means compile only (not link), should not be cached as link
        let result =
            parse_linker_invocation("gcc", &args(&["-c", "-shared", "-o", "foo.o", "foo.c"]));
        assert!(matches!(
            result,
            ParsedLinkerInvocation::NonCacheable { .. }
        ));
    }

    #[test]
    fn gcc_exe_cacheable() {
        // gcc without -shared is executable linking — cacheable
        let result = parse_linker_invocation("gcc", &args(&["-o", "a.out", "main.o"]));
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.family, LinkerFamily::CompilerDriver);
                assert_eq!(c.output_file, PathBuf::from("a.out"));
                assert_eq!(c.input_files, vec![PathBuf::from("main.o")]);
                assert!(!c.non_deterministic);
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn gcc_shared_no_output_non_cacheable() {
        let result = parse_linker_invocation("gcc", &args(&["-shared", "a.o"]));
        assert!(matches!(
            result,
            ParsedLinkerInvocation::NonCacheable { .. }
        ));
    }

    #[test]
    fn gcc_shared_no_object_inputs_non_cacheable() {
        // Source files (.c) are not valid linker inputs — need pre-compiled .o
        let result =
            parse_linker_invocation("gcc", &args(&["-shared", "-o", "libfoo.so", "foo.c"]));
        assert!(matches!(
            result,
            ParsedLinkerInvocation::NonCacheable { .. }
        ));
    }

    #[test]
    fn cross_compile_gcc() {
        let result = parse_linker_invocation(
            "x86_64-w64-mingw32-gcc",
            &args(&["-shared", "-o", "foo.dll", "a.o"]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert_eq!(c.family, LinkerFamily::CompilerDriver);
                assert_eq!(c.tool, PathBuf::from("x86_64-w64-mingw32-gcc"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn gcc_shared_with_wl_soname() {
        let result = parse_linker_invocation(
            "gcc",
            &args(&[
                "-shared",
                "-Wl,-soname,libfoo.so.1",
                "-o",
                "libfoo.so.1.0",
                "a.o",
            ]),
        );
        match result {
            ParsedLinkerInvocation::Cacheable(c) => {
                assert!(c
                    .cache_relevant_flags
                    .contains(&"-Wl,-soname,libfoo.so.1".to_string()));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    // ─── is_link_invocation (combined tool + args check) ──────────────

    #[test]
    fn is_link_invocation_direct_linker() {
        // Direct linkers are always link invocations (args don't matter for detection)
        assert!(is_link_invocation(
            "ld",
            &args(&["-shared", "-o", "foo.so", "a.o"])
        ));
        assert!(is_link_invocation("ld", &args(&["-o", "a.out", "main.o"])));
        assert!(is_link_invocation(
            "link.exe",
            &args(&["/DLL", "/OUT:foo.dll", "a.obj"])
        ));
    }

    #[test]
    fn is_link_invocation_compiler_driver_shared() {
        assert!(is_link_invocation(
            "gcc",
            &args(&["-shared", "-o", "foo.so", "a.o"])
        ));
        assert!(is_link_invocation(
            "clang++",
            &args(&["-shared", "-o", "foo.so", "a.o"])
        ));
    }

    #[test]
    fn is_link_invocation_compiler_not_shared() {
        // gcc -c is compilation, NOT a link invocation
        assert!(!is_link_invocation(
            "gcc",
            &args(&["-c", "foo.c", "-o", "foo.o"])
        ));
        // gcc -E is preprocessing, NOT a link invocation
        assert!(!is_link_invocation("gcc", &args(&["-E", "foo.c"])));
        // gcc -S is assembly generation, NOT a link invocation
        assert!(!is_link_invocation("gcc", &args(&["-S", "foo.c"])));
        // gcc -o a.out main.o IS a link invocation (exe link)
        assert!(is_link_invocation("gcc", &args(&["-o", "a.out", "main.o"])));
    }

    #[test]
    fn is_link_invocation_unknown_tool() {
        assert!(!is_link_invocation(
            "rustc",
            &args(&["-shared", "-o", "foo.so"])
        ));
    }
}
