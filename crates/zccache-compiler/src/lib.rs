//! Compiler detection and argument parsing for zccache.
//!
//! Handles identifying compilers, parsing their command-line arguments
//! to determine cacheability, and extracting cache-relevant information.

#![allow(clippy::missing_errors_doc)]

pub mod parse_archiver;
pub mod parse_linker;
pub mod parse_rustfmt;
pub mod response_file;

use std::path::PathBuf;
use std::sync::Arc;

/// Supported compiler families.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompilerFamily {
    /// GCC (gcc, g++)
    Gcc,
    /// Clang (clang, clang++)
    Clang,
    /// MSVC (cl.exe)
    Msvc,
    /// Rust compiler (rustc)
    Rustc,
    /// Rust formatter (rustfmt) — not a compiler, but cacheable as a tool.
    Rustfmt,
}

impl CompilerFamily {
    /// Whether this compiler supports `-MD -MF` for depfile generation.
    /// MSVC uses `/showIncludes` instead. Rustc uses `--emit=dep-info`.
    #[must_use]
    pub fn supports_depfile(&self) -> bool {
        matches!(self, CompilerFamily::Gcc | CompilerFamily::Clang)
    }

    /// Default PCH output extension (without dot) for this compiler family.
    /// Returns `None` for MSVC (uses /Yc + /Fp mechanism instead), Rustc, and Rustfmt.
    #[must_use]
    pub fn pch_extension(&self) -> Option<&'static str> {
        match self {
            CompilerFamily::Gcc => Some("gch"),
            CompilerFamily::Clang => Some("pch"),
            CompilerFamily::Msvc | CompilerFamily::Rustc | CompilerFamily::Rustfmt => None,
        }
    }

    /// Whether this is a formatter tool (not a compiler).
    #[must_use]
    pub fn is_formatter(&self) -> bool {
        matches!(self, CompilerFamily::Rustfmt)
    }
}

/// The result of parsing a compiler invocation.
#[derive(Debug, Clone)]
pub enum ParsedInvocation {
    /// A cacheable compilation (single source to single object).
    Cacheable(CacheableCompilation),
    /// Multiple source files with `-c` — each is independently cacheable.
    MultiFile {
        /// One entry per source file, each with its own output path.
        compilations: Vec<CacheableCompilation>,
        /// The original full argument list (for batched compiler invocation of misses).
        original_args: Arc<[String]>,
        /// Indices of source files in `original_args`, so the daemon can filter
        /// out cache-hit sources without reconstructing args.
        source_indices: Vec<usize>,
    },
    /// A non-cacheable invocation (linking, preprocessing, etc.).
    NonCacheable {
        /// Reason why this invocation is not cacheable.
        reason: String,
    },
}

/// A cacheable compilation invocation.
#[derive(Debug, Clone)]
pub struct CacheableCompilation {
    /// The compiler executable path.
    pub compiler: PathBuf,
    /// The detected compiler family.
    pub family: CompilerFamily,
    /// The source file being compiled.
    pub source_file: PathBuf,
    /// The output file path.
    pub output_file: PathBuf,
    /// The full original argument list — always passed to the compiler as-is.
    pub original_args: Arc<[String]>,
    /// Flags not recognized by the parser but still part of the invocation.
    /// Preserved for completeness and consistency with the linker/archiver/
    /// depgraph parsers which all track unknown flags.
    pub unknown_flags: Vec<String>,
}

/// The language mode for a source file, as determined by `-x <lang>` or file extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceMode {
    /// Normal C/C++ source (`.c`, `.cpp`, etc.) — compiles to `.o`.
    Normal,
    /// PCH header (`-x c-header` / `-x c++-header`) — compiles to `.pch`/`.gch`.
    Header,
    /// Header unit (`-x c-header-unit` / `-x c++-header-unit`) — compiles to `.pcm`.
    HeaderUnit,
    /// Module interface (`-x c++-module` or `.cppm`/`.ixx`) — `.pcm` with `--precompile`, `.o` with `-c`.
    Module,
}

impl SourceMode {
    /// Whether this mode implies compilation without an explicit `-c` or `--precompile` flag.
    /// Header and header-unit modes imply compilation (like PCH generation).
    /// Module mode does NOT — it requires `-c` or `--precompile`.
    pub(crate) fn implies_compilation(self) -> bool {
        matches!(self, SourceMode::Header | SourceMode::HeaderUnit)
    }
}

/// Map a `-x <lang>` value to the corresponding source mode.
/// Returns `None` for unrecognized language values (no special mode).
fn source_mode_from_language(lang: &str) -> Option<SourceMode> {
    match lang {
        "c-header" | "c++-header" => Some(SourceMode::Header),
        "c-header-unit" | "c++-header-unit" => Some(SourceMode::HeaderUnit),
        "c++-module" => Some(SourceMode::Module),
        _ => None,
    }
}

/// Source file extensions we recognize as C/C++.
const SOURCE_EXTENSIONS: &[&str] = &[
    "c", "cc", "cpp", "cxx", "c++", "C", "m", "mm", "i", "ii", "cppm", "ixx",
];

/// File extensions that imply module-interface mode even without `-x c++-module`.
const MODULE_EXTENSIONS: &[&str] = &["cppm", "ixx"];

/// Determine the source mode implied by a file extension.
fn source_mode_from_extension(path: &str) -> SourceMode {
    if let Some(ext) = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
    {
        if MODULE_EXTENSIONS.contains(&ext) {
            return SourceMode::Module;
        }
    }
    SourceMode::Normal
}

/// Detect the compiler family from the compiler path.
#[must_use]
pub fn detect_family(compiler: &str) -> CompilerFamily {
    // Split on both `/` and `\` so Windows-style paths work on all platforms.
    let basename = compiler.rsplit(['/', '\\']).next().unwrap_or(compiler);
    let name = match basename.rsplit_once('.') {
        Some((stem, _)) => stem,
        None => basename,
    };
    if name == "rustfmt" || name.starts_with("rustfmt-") {
        CompilerFamily::Rustfmt
    } else if name == "rustc"
        || name.starts_with("rustc-")
        || name == "clippy-driver"
        || name.starts_with("clippy-driver-")
    {
        CompilerFamily::Rustc
    } else if name.contains("clang") || name == "emcc" || name == "em++" {
        CompilerFamily::Clang
    } else if name.eq_ignore_ascii_case("cl") {
        CompilerFamily::Msvc
    } else {
        CompilerFamily::Gcc
    }
}

/// Check if a path looks like a C/C++ source file.
fn is_source_file(path: &str) -> bool {
    if let Some(ext) = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
    {
        SOURCE_EXTENSIONS.contains(&ext)
    } else {
        false
    }
}

/// Compute the default output path when `-o` is absent.
///
/// For both normal compilation and PCH generation, the output is placed in
/// the current working directory using just the filename — never preserving
/// directory components from the source path.
///
/// Normal compilation: `src/foo.cpp` → `foo.o`
/// PCH generation:     `src/pch.h`   → `pch.h.pch`  (clang)
///                     `src/pch.h`   → `pch.h.gch`  (gcc)
///
/// Note: real compilers place PCH output next to the source file
/// (`src/pch.h` → `src/pch.h.pch`), but zccache intentionally uses only
/// the filename. This prevents spurious `.pch` files from being written
/// into the source tree when a compilation falls back to `default_output`
/// (e.g., during cache restoration without an explicit `-o` flag).
fn default_output(
    source: &str,
    family: CompilerFamily,
    mode: SourceMode,
    has_precompile: bool,
) -> String {
    match mode {
        SourceMode::Header => {
            // PCH: filename.pch (Clang) or filename.gch (GCC)
            if let Some(ext) = family.pch_extension() {
                let filename = std::path::Path::new(source)
                    .file_name()
                    .and_then(|f| f.to_str())
                    .unwrap_or(source);
                return format!("{filename}.{ext}");
            }
        }
        SourceMode::HeaderUnit => {
            // Header unit: filename.pcm
            let filename = std::path::Path::new(source)
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or(source);
            return format!("{filename}.pcm");
        }
        SourceMode::Module | SourceMode::Normal => {
            if has_precompile {
                // --precompile: stem.pcm
                let stem = std::path::Path::new(source)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("a");
                return format!("{stem}.pcm");
            }
        }
    }
    // Default: stem.o
    let stem = std::path::Path::new(source)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("a");
    format!("{stem}.o")
}

/// Flags that take a following argument (value in next argv element).
const FLAGS_WITH_VALUE: &[&str] = &[
    "-o",
    "-D",
    "-U",
    "-I",
    "-isystem",
    "-iquote",
    "-idirafter",
    "-include",
    "-include-pch",
    "-isysroot",
    "-target",
    "--target",
    "-MF",
    "-MQ",
    "-MT",
    "-std",
    "-x",
    "-arch",
    "-Xclang",
    "-mllvm",
    "--serialize-diagnostics",
];

/// Parse a compiler invocation's arguments to determine cacheability.
///
/// Returns a `ParsedInvocation` indicating whether the invocation is
/// cacheable, and if so, extracts the relevant information.
///
/// Arg parsing is read-only analysis — it never modifies what goes to
/// the compiler. The compiler always receives the exact original args.
#[must_use]
pub fn parse_invocation(compiler: &str, args: &[String]) -> ParsedInvocation {
    let family = detect_family(compiler);
    // Rustfmt is not a compiler — reject here, CLI handles it separately.
    if family == CompilerFamily::Rustfmt {
        return ParsedInvocation::NonCacheable {
            reason: "rustfmt is handled via the format cache path, not compile cache".to_string(),
        };
    }
    // Rustc has a completely different invocation model — dispatch early.
    if family == CompilerFamily::Rustc {
        return parse_rustc_invocation(compiler, args);
    }

    let mut has_c_flag = false;
    let mut has_precompile_flag = false;
    let mut source_files: Vec<(String, usize, SourceMode)> = Vec::new();
    let mut output_file: Option<String> = None;
    let mut current_mode = SourceMode::Normal;
    let mut unknown_flags: Vec<String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];

        // Check for non-cacheable flags
        if arg == "-E" || arg == "-M" || arg == "-MM" {
            return ParsedInvocation::NonCacheable {
                reason: format!("preprocessing-only flag: {arg}"),
            };
        }

        if arg == "-" {
            return ParsedInvocation::NonCacheable {
                reason: "stdin source not cacheable".to_string(),
            };
        }

        if arg == "-c" {
            has_c_flag = true;
            i += 1;
            continue;
        }

        // --precompile (Clang): compile module interface to BMI (.pcm).
        // Acts like -c but for module output.
        if arg == "--precompile" {
            has_precompile_flag = true;
            i += 1;
            continue;
        }

        // -o takes the next arg as output file, or -o<path> (concatenated)
        if arg == "-o" {
            if let Some(next) = args.get(i + 1) {
                output_file = Some(next.clone());
                i += 2;
            } else {
                i += 1;
            }
            continue;
        } else if let Some(path) = arg.strip_prefix("-o") {
            output_file = Some(path.to_string());
            i += 1;
            continue;
        }

        // Flags that take a value in the next arg — skip both flag and value
        if let Some(&flag) = FLAGS_WITH_VALUE.iter().find(|&&f| f == arg.as_str()) {
            if flag == "-x" && i + 1 < args.len() {
                current_mode =
                    source_mode_from_language(&args[i + 1]).unwrap_or(SourceMode::Normal);
            }
            i += 2;
            continue;
        }

        // Any flag starting with - (including unknown flags) — preserve
        if arg.starts_with('-') {
            unknown_flags.push(arg.clone());
            i += 1;
            continue;
        }

        // Positional arg — source file candidate.
        // In a special mode (Header/HeaderUnit/Module), any positional arg is a source.
        // Otherwise, check by file extension. Module extensions (.cppm/.ixx) also set
        // the effective mode to Module for correct default output.
        let effective_mode = if current_mode != SourceMode::Normal {
            current_mode
        } else {
            source_mode_from_extension(arg)
        };
        if is_source_file(arg) || current_mode != SourceMode::Normal {
            source_files.push((arg.clone(), i, effective_mode));
        }

        i += 1;
    }

    // Header and header-unit modes imply compilation (no -c needed).
    // Module mode does NOT imply compilation alone — requires -c or --precompile.
    // --precompile also implies compilation (like -c but for BMI output).
    if !has_c_flag && !has_precompile_flag && !current_mode.implies_compilation() {
        return ParsedInvocation::NonCacheable {
            reason: "no -c flag (likely a link invocation)".to_string(),
        };
    }

    if source_files.is_empty() {
        return ParsedInvocation::NonCacheable {
            reason: "no source file found".to_string(),
        };
    }

    let family = detect_family(compiler);

    // Multi-file: `-o` is invalid with `-c` and multiple sources (compiler rejects it),
    // so each source gets its default output name (stem.o).
    if source_files.len() > 1 {
        let source_indices: Vec<usize> = source_files.iter().map(|(_, idx, _)| *idx).collect();
        let shared_args: Arc<[String]> = Arc::from(args.to_vec());
        let compilations = source_files
            .iter()
            .map(|(src, _, mode)| CacheableCompilation {
                compiler: PathBuf::from(compiler),
                family,
                source_file: PathBuf::from(src),
                output_file: PathBuf::from(default_output(src, family, *mode, has_precompile_flag)),
                original_args: Arc::clone(&shared_args),
                unknown_flags: unknown_flags.clone(),
            })
            .collect();
        return ParsedInvocation::MultiFile {
            compilations,
            original_args: shared_args,
            source_indices,
        };
    }

    // Single source file
    let (source, _, mode) = source_files.into_iter().next().unwrap();
    let output =
        output_file.unwrap_or_else(|| default_output(&source, family, mode, has_precompile_flag));

    ParsedInvocation::Cacheable(CacheableCompilation {
        compiler: PathBuf::from(compiler),
        family,
        source_file: PathBuf::from(source),
        output_file: PathBuf::from(output),
        original_args: Arc::from(args.to_vec()),
        unknown_flags,
    })
}

/// Cacheable rustc crate types: these don't invoke the system linker.
const RUSTC_CACHEABLE_CRATE_TYPES: &[&str] = &["lib", "rlib", "staticlib"];

/// Rustc flags that take a following argument (value in next argv element).
const RUSTC_FLAGS_WITH_VALUE: &[&str] = &[
    "--edition",
    "--crate-type",
    "--crate-name",
    "--emit",
    "--out-dir",
    "--target",
    "--cap-lints",
    "--extern",
    "--error-format",
    "--json",
    "--color",
    "--diagnostic-width",
    "--sysroot",
    "--cfg",
    "--check-cfg",
    "-o",
    "-L",
    "-C",
    "-A",
    "-W",
    "-D",
    "-F",
    "--codegen",
    "--remap-path-prefix",
    "--env-set",
];

/// Parse a rustc invocation to determine cacheability.
///
/// Cacheable: `--crate-type` is `lib`, `rlib`, or `staticlib` (no system linker).
/// Non-cacheable: `bin`, `dylib`, `cdylib`, `proc-macro`, or `-C incremental`.
fn parse_rustc_invocation(compiler: &str, args: &[String]) -> ParsedInvocation {
    let mut crate_types: Vec<String> = Vec::new();
    let mut source_file: Option<String> = None;
    let mut output_file: Option<String> = None;
    let mut out_dir: Option<String> = None;
    let mut crate_name: Option<String> = None;
    let mut extra_filename: Option<String> = None;
    let mut emit_types: Vec<String> = Vec::new();
    let mut unknown_flags: Vec<String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];

        // --crate-type <type> or --crate-type=<type>
        // Rustc accepts comma-separated types: --crate-type lib,rlib
        if arg == "--crate-type" {
            if let Some(next) = args.get(i + 1) {
                crate_types.extend(next.split(',').map(|s| s.to_string()));
                i += 2;
                continue;
            }
        } else if let Some(val) = arg.strip_prefix("--crate-type=") {
            crate_types.extend(val.split(',').map(|s| s.to_string()));
            i += 1;
            continue;
        }

        // --crate-name <name> or --crate-name=<name>
        if arg == "--crate-name" {
            if let Some(next) = args.get(i + 1) {
                crate_name = Some(next.clone());
                i += 2;
                continue;
            }
        } else if let Some(val) = arg.strip_prefix("--crate-name=") {
            crate_name = Some(val.to_string());
            i += 1;
            continue;
        }

        // --emit <types> or --emit=<types>
        if arg == "--emit" {
            if let Some(next) = args.get(i + 1) {
                emit_types.extend(next.split(',').map(|s| {
                    // Handle --emit=dep-info=path form
                    s.split('=').next().unwrap_or(s).to_string()
                }));
                i += 2;
                continue;
            }
        } else if let Some(val) = arg.strip_prefix("--emit=") {
            emit_types.extend(
                val.split(',')
                    .map(|s| s.split('=').next().unwrap_or(s).to_string()),
            );
            i += 1;
            continue;
        }

        // --out-dir <path> or --out-dir=<path>
        if arg == "--out-dir" {
            if let Some(next) = args.get(i + 1) {
                out_dir = Some(next.clone());
                i += 2;
                continue;
            }
        } else if let Some(val) = arg.strip_prefix("--out-dir=") {
            out_dir = Some(val.to_string());
            i += 1;
            continue;
        }

        // -o <path>
        if arg == "-o" {
            if let Some(next) = args.get(i + 1) {
                output_file = Some(next.clone());
                i += 2;
                continue;
            }
        }

        // -C <option> or -C<option> or --codegen <option>
        if arg == "-C" || arg == "--codegen" {
            if let Some(next) = args.get(i + 1) {
                if let Some(val) = next.strip_prefix("extra-filename=") {
                    extra_filename = Some(val.to_string());
                }
                i += 2;
                continue;
            }
        } else if let Some(rest) = arg.strip_prefix("-C") {
            if !rest.is_empty() {
                if let Some(val) = rest.strip_prefix("extra-filename=") {
                    extra_filename = Some(val.to_string());
                }
                i += 1;
                continue;
            }
        }

        // Known flags that take a value — skip both
        if let Some(&_flag) = RUSTC_FLAGS_WITH_VALUE.iter().find(|&&f| f == arg.as_str()) {
            i += 2;
            continue;
        }

        // Flags with = form (e.g., --edition=2021, --cfg=feature)
        if arg.starts_with("--") && arg.contains('=') {
            i += 1;
            continue;
        }

        // Any flag starting with -
        if arg.starts_with('-') {
            unknown_flags.push(arg.clone());
            i += 1;
            continue;
        }

        // Positional arg — source file candidate (.rs)
        if arg.ends_with(".rs") {
            source_file = Some(arg.clone());
        }

        i += 1;
    }

    // No source file → non-cacheable (e.g., `rustc --version`)
    let source = match source_file {
        Some(s) => s,
        None => {
            return ParsedInvocation::NonCacheable {
                reason: "no .rs source file found".to_string(),
            };
        }
    };

    // Note: -C incremental is ignored for caching purposes.
    // The incremental dir is excluded from the cache key, and we let rustc
    // use it on a miss (doesn't affect output determinism for rlib/rmeta).
    // sccache also allows incremental — cargo always passes it.

    // Default crate type is bin if not specified
    if crate_types.is_empty() {
        crate_types.push("bin".to_string());
    }

    // Check all crate types are cacheable
    for ct in &crate_types {
        if !RUSTC_CACHEABLE_CRATE_TYPES.contains(&ct.as_str()) {
            return ParsedInvocation::NonCacheable {
                reason: format!("non-cacheable crate type: {ct}"),
            };
        }
    }

    // Determine primary output extension based on --emit and --crate-type.
    // If --emit includes "link", the primary is rlib/staticlib.
    // If --emit is metadata-only (no link), the primary is rmeta.
    let has_link_emit = emit_types.iter().any(|t| t == "link");
    let primary_ext = if !has_link_emit && emit_types.iter().any(|t| t == "metadata") {
        "rmeta"
    } else {
        match crate_types.first().map(|s| s.as_str()) {
            Some("staticlib") => "a",
            _ => "rlib",
        }
    };

    // Derive output path
    let output = if let Some(o) = output_file {
        o
    } else if let Some(ref dir) = out_dir {
        let name = crate_name.as_deref().unwrap_or("unknown");
        let suffix = extra_filename.as_deref().unwrap_or("");
        // Use PathBuf::join to handle platform path separators correctly
        PathBuf::from(dir)
            .join(format!("lib{name}{suffix}.{primary_ext}"))
            .to_string_lossy()
            .into_owned()
    } else {
        let name = crate_name.as_deref().unwrap_or_else(|| {
            std::path::Path::new(&source)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
        });
        format!("lib{name}.{primary_ext}")
    };

    ParsedInvocation::Cacheable(CacheableCompilation {
        compiler: PathBuf::from(compiler),
        family: CompilerFamily::Rustc,
        source_file: PathBuf::from(source),
        output_file: PathBuf::from(output),
        original_args: Arc::from(args.to_vec()),
        unknown_flags,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn basic_cacheable_compilation() {
        let result = parse_invocation("clang++", &args(&["-c", "hello.cpp", "-o", "hello.o"]));
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, PathBuf::from("hello.cpp"));
                assert_eq!(c.output_file, PathBuf::from("hello.o"));
                assert_eq!(c.family, CompilerFamily::Clang);
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn no_c_flag_is_non_cacheable() {
        let result = parse_invocation("gcc", &args(&["hello.cpp", "-o", "hello"]));
        assert!(matches!(result, ParsedInvocation::NonCacheable { .. }));
    }

    #[test]
    fn preprocessing_only_non_cacheable() {
        let result = parse_invocation("gcc", &args(&["-E", "hello.cpp"]));
        assert!(matches!(result, ParsedInvocation::NonCacheable { .. }));
    }

    #[test]
    fn multi_file_split() {
        let result = parse_invocation("gcc", &args(&["-c", "a.cpp", "b.cpp"]));
        match result {
            ParsedInvocation::MultiFile {
                compilations,
                source_indices,
                ..
            } => {
                assert_eq!(compilations.len(), 2);
                assert_eq!(compilations[0].source_file, PathBuf::from("a.cpp"));
                assert_eq!(compilations[0].output_file, PathBuf::from("a.o"));
                assert_eq!(compilations[1].source_file, PathBuf::from("b.cpp"));
                assert_eq!(compilations[1].output_file, PathBuf::from("b.o"));
                assert_eq!(source_indices, vec![1, 2]);
            }
            other => panic!("expected MultiFile, got: {other:?}"),
        }
    }

    #[test]
    fn multi_file_with_flags() {
        let result = parse_invocation(
            "g++",
            &args(&["-c", "-O2", "main.cpp", "-Wall", "util.cpp"]),
        );
        match result {
            ParsedInvocation::MultiFile {
                compilations,
                original_args,
                source_indices,
            } => {
                assert_eq!(compilations.len(), 2);
                assert_eq!(compilations[0].source_file, PathBuf::from("main.cpp"));
                assert_eq!(compilations[1].source_file, PathBuf::from("util.cpp"));
                // Flags are in original_args, not per-compilation
                assert!(original_args.contains(&"-O2".to_string()));
                assert!(original_args.contains(&"-Wall".to_string()));
                assert_eq!(source_indices, vec![2, 4]);
            }
            other => panic!("expected MultiFile, got: {other:?}"),
        }
    }

    #[test]
    fn multi_file_mixed_extensions() {
        let result = parse_invocation("gcc", &args(&["-c", "file1.c", "file2.cpp"]));
        match result {
            ParsedInvocation::MultiFile { compilations, .. } => {
                assert_eq!(compilations.len(), 2);
                assert_eq!(compilations[0].source_file, PathBuf::from("file1.c"));
                assert_eq!(compilations[1].source_file, PathBuf::from("file2.cpp"));
            }
            other => panic!("expected MultiFile, got: {other:?}"),
        }
    }

    #[test]
    fn stdin_non_cacheable() {
        let result = parse_invocation("gcc", &args(&["-c", "-"]));
        assert!(matches!(result, ParsedInvocation::NonCacheable { .. }));
    }

    #[test]
    fn default_output_name() {
        let result = parse_invocation("gcc", &args(&["-c", "foo.cpp"]));
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.output_file, PathBuf::from("foo.o"));
            }
            _ => panic!("expected cacheable"),
        }
    }

    #[test]
    fn original_args_preserved() {
        let input = args(&["-c", "hello.cpp", "-O2", "-std=c++17", "-DNDEBUG", "-Wall"]);
        let result = parse_invocation("clang++", &input);
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(*c.original_args, *input);
            }
            _ => panic!("expected cacheable"),
        }
    }

    #[test]
    fn unknown_flags_preserved_in_original_args() {
        let input = args(&[
            "-c",
            "hello.cpp",
            "--deploy-dependencies",
            "--custom-flag=value",
            "-o",
            "hello.o",
        ]);
        let result = parse_invocation("clang++", &input);
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(*c.original_args, *input);
                assert_eq!(c.source_file, PathBuf::from("hello.cpp"));
                assert_eq!(c.output_file, PathBuf::from("hello.o"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn include_pch_flag_with_value() {
        let result = parse_invocation(
            "clang++",
            &args(&["-c", "foo.cpp", "-include-pch", "pch.h.pch", "-o", "foo.o"]),
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                // PCH path is NOT treated as a source file
                assert_eq!(c.source_file, PathBuf::from("foo.cpp"));
                // Original args preserved
                assert!(c.original_args.contains(&"-include-pch".to_string()));
                assert!(c.original_args.contains(&"pch.h.pch".to_string()));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn pch_generation_cpp_header_is_cacheable() {
        // `clang -x c++-header -c pch.h -o pch.h.pch` should be cacheable
        let result = parse_invocation(
            "clang++",
            &args(&["-x", "c++-header", "-c", "pch.h", "-o", "pch.h.pch"]),
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, PathBuf::from("pch.h"));
                assert_eq!(c.output_file, PathBuf::from("pch.h.pch"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn pch_generation_c_header_is_cacheable() {
        // `gcc -x c-header -c stdafx.h -o stdafx.h.gch` should be cacheable
        let result = parse_invocation(
            "gcc",
            &args(&["-x", "c-header", "-c", "stdafx.h", "-o", "stdafx.h.gch"]),
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, PathBuf::from("stdafx.h"));
                assert_eq!(c.output_file, PathBuf::from("stdafx.h.gch"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn pch_generation_without_c_flag_is_cacheable() {
        // Meson invokes PCH generation without -c:
        // `clang++ -x c++-header header.h -o header.pch`
        // `-x c++-header` implies compilation, so -c is redundant.
        let result = parse_invocation(
            "clang++",
            &args(&["-x", "c++-header", "FastLED.h", "-o", "FastLED.h.pch"]),
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, PathBuf::from("FastLED.h"));
                assert_eq!(c.output_file, PathBuf::from("FastLED.h.pch"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn pch_generation_c_header_without_c_flag_is_cacheable() {
        let result = parse_invocation(
            "gcc",
            &args(&["-x", "c-header", "stdafx.h", "-o", "stdafx.h.gch"]),
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, PathBuf::from("stdafx.h"));
                assert_eq!(c.output_file, PathBuf::from("stdafx.h.gch"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn pch_generation_with_meson_flags_is_cacheable() {
        // Full Meson-style PCH invocation with extra flags
        let result = parse_invocation(
            "ctc-clang++",
            &args(&[
                "-x",
                "c++-header",
                "FastLED.h",
                "-o",
                "FastLED.h.pch",
                "-MD",
                "-MF",
                "FastLED.h.pch.d",
                "-fPIC",
                "-Iinclude",
                "-Werror=invalid-pch",
            ]),
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, PathBuf::from("FastLED.h"));
                assert_eq!(c.output_file, PathBuf::from("FastLED.h.pch"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn header_without_x_flag_is_not_source() {
        // Without `-x c++-header`, a .h file should NOT be recognized as a source
        let result = parse_invocation("clang++", &args(&["-c", "pch.h"]));
        assert!(
            matches!(result, ParsedInvocation::NonCacheable { .. }),
            "bare .h without -x header mode should be non-cacheable"
        );
    }

    #[test]
    fn x_flag_reset_disables_header_mode() {
        // `-x c++-header pch.h -x c++ main.cpp -c -o main.o`
        // After `-x c++`, header_mode resets — main.cpp is a normal source,
        // pch.h was collected as header-mode source → multi-file.
        let result = parse_invocation(
            "clang++",
            &args(&[
                "-x",
                "c++-header",
                "pch.h",
                "-x",
                "c++",
                "main.cpp",
                "-c",
                "-o",
                "main.o",
            ]),
        );
        match result {
            ParsedInvocation::MultiFile { compilations, .. } => {
                assert_eq!(compilations.len(), 2);
                assert_eq!(compilations[0].source_file, PathBuf::from("pch.h"));
                assert_eq!(compilations[1].source_file, PathBuf::from("main.cpp"));
            }
            other => panic!("expected MultiFile, got: {other:?}"),
        }
    }

    #[test]
    fn x_cpp_after_header_is_normal_compilation() {
        // `-x c++-header -x c++ main.cpp -c -o main.o`
        // No header file between the two -x flags. The second -x c++ resets
        // header_mode, so main.cpp is a normal compilation.
        let result = parse_invocation(
            "clang++",
            &args(&[
                "-x",
                "c++-header",
                "-x",
                "c++",
                "main.cpp",
                "-c",
                "-o",
                "main.o",
            ]),
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, PathBuf::from("main.cpp"));
                assert_eq!(c.output_file, PathBuf::from("main.o"));
            }
            other => panic!("expected Cacheable, got: {other:?}"),
        }
    }

    // ─── Regression tests: sticky header_mode bug ─────────────────────

    #[test]
    fn sticky_header_mode_cpp_not_spuriously_pch() {
        // BUG: old code set header_mode=true on `-x c++-header` but never
        // reset it on `-x c++`, so main.cpp was treated as a header file
        // needing PCH generation. After the fix, `-x c++` resets header_mode,
        // and main.cpp is a normal source — not a PCH candidate.
        let result = parse_invocation(
            "clang++",
            &args(&[
                "-x",
                "c++-header",
                "pch.h",
                "-o",
                "pch.h.pch",
                "-x",
                "c++",
                "-c",
                "main.cpp",
                "-o",
                "main.o",
            ]),
        );
        // main.cpp must be recognized as a normal source via its extension,
        // NOT via header_mode. With the old bug, header_mode stayed true and
        // both pch.h and main.cpp were header-mode sources.
        match &result {
            ParsedInvocation::MultiFile { compilations, .. } => {
                assert_eq!(compilations.len(), 2);
                // pch.h picked up in header_mode
                assert_eq!(compilations[0].source_file, PathBuf::from("pch.h"));
                // main.cpp picked up by extension after reset
                assert_eq!(compilations[1].source_file, PathBuf::from("main.cpp"));
            }
            other => panic!("expected MultiFile, got: {other:?}"),
        }
    }

    #[test]
    fn sticky_header_mode_non_source_not_captured_after_reset() {
        // BUG: with sticky header_mode, a positional arg like "README.txt"
        // after `-x c++` would be treated as a source file because
        // `is_source_file(arg) || header_mode` was true. After fix,
        // header_mode is false after `-x c++`, so non-source extensions
        // are correctly ignored.
        let result = parse_invocation(
            "clang++",
            &args(&["-x", "c++-header", "pch.h", "-x", "c++", "-c", "main.cpp"]),
        );
        match &result {
            ParsedInvocation::MultiFile { compilations, .. } => {
                assert_eq!(compilations.len(), 2);
                assert_eq!(compilations[0].source_file, PathBuf::from("pch.h"));
                assert_eq!(compilations[1].source_file, PathBuf::from("main.cpp"));
            }
            other => panic!("expected MultiFile, got: {other:?}"),
        }
    }

    #[test]
    fn sticky_header_mode_no_c_flag_after_reset_is_non_cacheable() {
        // BUG: with sticky header_mode, the `!has_c_flag && !header_mode`
        // check at the end would pass (header_mode was still true),
        // making a link invocation appear cacheable. After fix, `-x c++`
        // resets header_mode so without -c it's correctly non-cacheable.
        let result = parse_invocation(
            "clang++",
            &args(&["-x", "c++-header", "-x", "c++", "main.cpp", "-o", "main"]),
        );
        assert!(
            matches!(result, ParsedInvocation::NonCacheable { .. }),
            "after -x c++ reset, no -c should be non-cacheable, got: {result:?}"
        );
    }

    #[test]
    fn header_unit_c_is_cacheable() {
        // `-x c-header-unit` activates header-unit mode (C++20 module support).
        // Header-unit mode implies compilation, producing .pcm output.
        let result = parse_invocation(
            "clang++",
            &args(&["-x", "c-header-unit", "foo.h", "-o", "foo.pcm"]),
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, PathBuf::from("foo.h"));
                assert_eq!(c.output_file, PathBuf::from("foo.pcm"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn header_unit_cpp_is_cacheable() {
        // `-x c++-header-unit` activates header-unit mode (C++20 module support).
        let result = parse_invocation(
            "clang++",
            &args(&["-x", "c++-header-unit", "foo.h", "-o", "foo.pcm"]),
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, PathBuf::from("foo.h"));
                assert_eq!(c.output_file, PathBuf::from("foo.pcm"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn detect_clang_family() {
        assert_eq!(detect_family("clang++"), CompilerFamily::Clang);
        assert_eq!(detect_family("/usr/bin/clang"), CompilerFamily::Clang);
        assert_eq!(detect_family("gcc"), CompilerFamily::Gcc);
        assert_eq!(detect_family("g++"), CompilerFamily::Gcc);
    }

    #[test]
    fn detect_emcc_family() {
        assert_eq!(detect_family("emcc"), CompilerFamily::Clang);
        assert_eq!(detect_family("em++"), CompilerFamily::Clang);
        assert_eq!(detect_family("/usr/bin/emcc"), CompilerFamily::Clang);
        assert_eq!(detect_family("emcc.exe"), CompilerFamily::Clang);
        // emcc supports -MD -MF (same as clang)
        assert!(CompilerFamily::Clang.supports_depfile());
    }

    #[test]
    fn detect_msvc_family() {
        assert_eq!(detect_family("cl"), CompilerFamily::Msvc);
        assert_eq!(detect_family("C:\\MSVC\\cl"), CompilerFamily::Msvc);
    }

    #[test]
    fn detect_msvc_case_insensitive() {
        // MSVC cl.exe is commonly invoked in uppercase on Windows.
        // Bug: detect_family used case-sensitive `name == "cl"`, so
        // CL.EXE was misclassified as Gcc.
        assert_eq!(detect_family("CL"), CompilerFamily::Msvc);
        assert_eq!(detect_family("CL.EXE"), CompilerFamily::Msvc);
        assert_eq!(detect_family("Cl.exe"), CompilerFamily::Msvc);
        assert_eq!(detect_family("C:\\MSVC\\CL.EXE"), CompilerFamily::Msvc);
        assert_eq!(
            detect_family("C:\\Program Files\\MSVC\\cl.EXE"),
            CompilerFamily::Msvc
        );
    }

    #[test]
    fn gcc_supports_depfile() {
        assert!(CompilerFamily::Gcc.supports_depfile());
    }

    #[test]
    fn clang_supports_depfile() {
        assert!(CompilerFamily::Clang.supports_depfile());
    }

    #[test]
    fn msvc_no_depfile() {
        assert!(!CompilerFamily::Msvc.supports_depfile());
    }

    // ─── PCH default output tests ────────────────────────────────────

    #[test]
    fn pch_default_output_clang() {
        // `clang++ -x c++-header src/pch.h` → output `pch.h.pch` (filename only, no dir)
        let result = parse_invocation("clang++", &args(&["-x", "c++-header", "src/pch.h"]));
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, PathBuf::from("src/pch.h"));
                assert_eq!(c.output_file, PathBuf::from("pch.h.pch"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn pch_default_output_gcc() {
        // `gcc -x c-header src/pch.h` → output `pch.h.gch` (filename only, no dir)
        let result = parse_invocation("gcc", &args(&["-x", "c-header", "src/pch.h"]));
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, PathBuf::from("src/pch.h"));
                assert_eq!(c.output_file, PathBuf::from("pch.h.gch"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn pch_default_output_strips_directory() {
        // `clang++ -x c++-header src/fl/audio/fft/fft.h` → output uses filename only.
        // Regression: old code produced `src/fl/audio/fft/fft.h.pch`, causing spurious
        // PCH files to be written into the source tree during cache restoration.
        let result = parse_invocation(
            "clang++",
            &args(&["-x", "c++-header", "src/fl/audio/fft/fft.h"]),
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, PathBuf::from("src/fl/audio/fft/fft.h"));
                assert_eq!(c.output_file, PathBuf::from("fft.h.pch"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn pch_default_output_absolute_path_strips_to_filename() {
        // Absolute source path must also produce filename-only output.
        // Regression: old code produced `/abs/path/src/pch.h.pch` which
        // the daemon resolved as an absolute write into the source tree.
        let result = parse_invocation(
            "clang++",
            &args(&["-x", "c++-header", "/abs/path/src/pch.h"]),
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, PathBuf::from("/abs/path/src/pch.h"));
                assert_eq!(c.output_file, PathBuf::from("pch.h.pch"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn pch_default_output_explicit_o_unchanged() {
        // Explicit `-o` still honored — no change in behavior
        let result = parse_invocation(
            "clang++",
            &args(&["-x", "c++-header", "pch.h", "-o", "build/pch.h.pch"]),
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, PathBuf::from("pch.h"));
                assert_eq!(c.output_file, PathBuf::from("build/pch.h.pch"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn normal_compile_default_output_unchanged() {
        // Regression guard: normal compilation still defaults to stem.o
        let result = parse_invocation("gcc", &args(&["-c", "foo.cpp"]));
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.output_file, PathBuf::from("foo.o"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    // ─── Concatenated -o flag tests ──────────────────────────────────

    #[test]
    fn concatenated_o_flag_parsed() {
        // `-obuild/foo.o` (no space) is valid for clang/gcc and must be recognized.
        let result = parse_invocation("clang", &args(&["-c", "foo.cpp", "-obuild/foo.o"]));
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.output_file, PathBuf::from("build/foo.o"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn concatenated_o_flag_pch() {
        // PCH compilation with concatenated -o must preserve the build directory path.
        // This is the root cause of BUG_LINKER.md: `-opath` was silently dropped
        // as an unknown flag, causing default_output() to be used instead.
        let result = parse_invocation(
            "clang++",
            &args(&["-x", "c++-header", "pch.h", "-obuild/pch.h.pch"]),
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.output_file, PathBuf::from("build/pch.h.pch"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    // ─── Unknown flags preservation tests ────────────────────────────

    #[test]
    fn all_flags_preserved() {
        // Every arg must be accounted for: either recognized by the parser
        // (source, output, known flag) or captured in unknown_flags.
        // Nothing is silently dropped.
        let input = args(&[
            "-c",
            "foo.cpp",
            "-o",
            "foo.o",
            "-Wall",
            "-Wextra",
            "-O2",
            "-Xclang",
            "-fno-spell-checking",
            "-std=c++17",
            "-DFOO=bar",
            "-I/usr/include",
            "-isystem",
            "/usr/local/include",
            "-unknown-future-flag",
        ]);
        let result = parse_invocation("clang++", &input);
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, PathBuf::from("foo.cpp"));
                assert_eq!(c.output_file, PathBuf::from("foo.o"));
                // Unknown flags are preserved, not dropped
                assert!(c.unknown_flags.contains(&"-Wall".to_string()));
                assert!(c.unknown_flags.contains(&"-Wextra".to_string()));
                assert!(c.unknown_flags.contains(&"-O2".to_string()));
                assert!(c
                    .unknown_flags
                    .contains(&"-unknown-future-flag".to_string()));
                // Concatenated known flags end up in unknown_flags
                // (parser only extracts -o value; -D/-I/-std with joined
                // values are not in FLAGS_WITH_VALUE so they go here)
                assert!(c.unknown_flags.contains(&"-std=c++17".to_string()));
                assert!(c.unknown_flags.contains(&"-DFOO=bar".to_string()));
                assert!(c.unknown_flags.contains(&"-I/usr/include".to_string()));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn xclang_value_not_misidentified_as_source() {
        // -Xclang takes the next arg as a pass-through value.
        // Without FLAGS_WITH_VALUE coverage, the value could be
        // misidentified as a source file.
        let result = parse_invocation(
            "clang++",
            &args(&[
                "-c",
                "foo.cpp",
                "-Xclang",
                "-fno-spell-checking",
                "-o",
                "foo.o",
            ]),
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, PathBuf::from("foo.cpp"));
                // Only one source file — -fno-spell-checking must NOT be treated as source
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn mllvm_value_not_misidentified_as_source() {
        let result = parse_invocation(
            "clang++",
            &args(&["-c", "foo.cpp", "-mllvm", "-some-llvm-opt", "-o", "foo.o"]),
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, PathBuf::from("foo.cpp"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    // ─── PCH output path mismatch repro (BUG_LINKER.md) ─────────────

    #[test]
    fn pch_output_path_mismatch_repro() {
        // Repro for BUG_LINKER.md: when no -o is provided for a nested
        // source header, default_output() returns filename-only which
        // doesn't match where clang actually writes (next to source).
        // The caller (build system) should always provide -o; the parser
        // must recognize all forms of -o (space-separated and concatenated).
        let result = parse_invocation(
            "clang++",
            &args(&["-x", "c++-header", "src/fl/fx/2d/flowfield_q31.h"]),
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.output_file, PathBuf::from("flowfield_q31.h.pch"));
                assert_eq!(c.source_file, PathBuf::from("src/fl/fx/2d/flowfield_q31.h"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    // ─── Rustc detection tests ──────────────────────────────────────

    #[test]
    fn detect_rustc_family() {
        assert_eq!(detect_family("rustc"), CompilerFamily::Rustc);
        assert_eq!(detect_family("/usr/bin/rustc"), CompilerFamily::Rustc);
        assert_eq!(detect_family("rustc.exe"), CompilerFamily::Rustc);
        assert_eq!(
            detect_family("C:\\rustup\\rustc.exe"),
            CompilerFamily::Rustc
        );
    }

    #[test]
    fn rustc_no_depfile_support() {
        // Rustc uses --emit=dep-info, not -MD -MF
        assert!(!CompilerFamily::Rustc.supports_depfile());
    }

    #[test]
    fn rustc_no_pch_extension() {
        assert_eq!(CompilerFamily::Rustc.pch_extension(), None);
    }

    // ─── Rustc cacheability tests ───────────────────────────────────

    #[test]
    fn rustc_lib_crate_is_cacheable() {
        let result = parse_invocation(
            "rustc",
            &args(&[
                "--edition",
                "2021",
                "--crate-type",
                "lib",
                "--emit=dep-info,metadata,link",
                "-C",
                "opt-level=2",
                "src/lib.rs",
            ]),
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.family, CompilerFamily::Rustc);
                assert_eq!(c.source_file, PathBuf::from("src/lib.rs"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn rustc_rlib_crate_is_cacheable() {
        let result = parse_invocation("rustc", &args(&["--crate-type", "rlib", "src/lib.rs"]));
        assert!(matches!(result, ParsedInvocation::Cacheable(_)));
    }

    #[test]
    fn rustc_staticlib_crate_is_cacheable() {
        let result = parse_invocation("rustc", &args(&["--crate-type", "staticlib", "src/lib.rs"]));
        assert!(matches!(result, ParsedInvocation::Cacheable(_)));
    }

    #[test]
    fn rustc_bin_crate_is_non_cacheable() {
        let result = parse_invocation("rustc", &args(&["--crate-type", "bin", "src/main.rs"]));
        assert!(matches!(result, ParsedInvocation::NonCacheable { .. }));
    }

    #[test]
    fn rustc_dylib_is_non_cacheable() {
        let result = parse_invocation("rustc", &args(&["--crate-type", "dylib", "src/lib.rs"]));
        assert!(matches!(result, ParsedInvocation::NonCacheable { .. }));
    }

    #[test]
    fn rustc_proc_macro_is_non_cacheable() {
        let result = parse_invocation(
            "rustc",
            &args(&["--crate-type", "proc-macro", "src/lib.rs"]),
        );
        assert!(matches!(result, ParsedInvocation::NonCacheable { .. }));
    }

    #[test]
    fn rustc_cdylib_is_non_cacheable() {
        let result = parse_invocation("rustc", &args(&["--crate-type", "cdylib", "src/lib.rs"]));
        assert!(matches!(result, ParsedInvocation::NonCacheable { .. }));
    }

    #[test]
    fn rustc_no_crate_type_defaults_to_bin_non_cacheable() {
        // Without --crate-type, rustc defaults to bin
        let result = parse_invocation("rustc", &args(&["src/main.rs"]));
        assert!(matches!(result, ParsedInvocation::NonCacheable { .. }));
    }

    #[test]
    fn rustc_incremental_is_cacheable() {
        // Cargo always passes -C incremental. We allow it (ignored for cache key).
        let result = parse_invocation(
            "rustc",
            &args(&[
                "--crate-type",
                "lib",
                "-C",
                "incremental=/tmp/incr",
                "src/lib.rs",
            ]),
        );
        assert!(matches!(result, ParsedInvocation::Cacheable(_)));
    }

    #[test]
    fn rustc_no_source_is_non_cacheable() {
        let result = parse_invocation("rustc", &args(&["--version"]));
        assert!(matches!(result, ParsedInvocation::NonCacheable { .. }));
    }

    #[test]
    fn rustc_emit_metadata_is_cacheable() {
        // cargo check uses --emit=metadata
        let result = parse_invocation(
            "rustc",
            &args(&["--crate-type", "lib", "--emit=metadata", "src/lib.rs"]),
        );
        assert!(matches!(result, ParsedInvocation::Cacheable(_)));
    }

    #[test]
    fn rustc_output_with_explicit_o() {
        let result = parse_invocation(
            "rustc",
            &args(&["--crate-type", "lib", "src/lib.rs", "-o", "libfoo.rlib"]),
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.output_file, PathBuf::from("libfoo.rlib"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn rustc_metadata_only_output_is_rmeta() {
        // cargo check: --emit=dep-info,metadata (no link) → primary output is .rmeta
        let result = parse_invocation(
            "rustc",
            &args(&[
                "--crate-type",
                "lib",
                "--crate-name",
                "mylib",
                "--emit=dep-info,metadata",
                "--out-dir",
                "/target/debug/deps",
                "-C",
                "extra-filename=-abc123",
                "src/lib.rs",
            ]),
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(
                    c.output_file,
                    PathBuf::from("/target/debug/deps/libmylib-abc123.rmeta")
                );
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn rustc_output_from_out_dir() {
        let result = parse_invocation(
            "rustc",
            &args(&[
                "--crate-type",
                "lib",
                "--crate-name",
                "mylib",
                "--out-dir",
                "/target/debug/deps",
                "-C",
                "extra-filename=-abc123",
                "src/lib.rs",
            ]),
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(
                    c.output_file,
                    PathBuf::from("/target/debug/deps/libmylib-abc123.rlib")
                );
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn rustc_full_cargo_invocation_cacheable() {
        // Realistic cargo-generated rustc command
        let result = parse_invocation(
            "rustc",
            &args(&[
                "--edition",
                "2021",
                "--crate-type",
                "lib",
                "--crate-name",
                "serde",
                "--emit=dep-info,metadata,link",
                "-C",
                "opt-level=2",
                "-C",
                "metadata=abc123def",
                "-C",
                "extra-filename=-abc123def",
                "--out-dir",
                "/target/release/deps",
                "-L",
                "dependency=/target/release/deps",
                "--extern",
                "serde_derive=/target/release/deps/libserde_derive-xyz.so",
                "--cap-lints",
                "allow",
                "--cfg",
                "feature=\"derive\"",
                "--cfg",
                "feature=\"std\"",
                "src/lib.rs",
            ]),
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.family, CompilerFamily::Rustc);
                assert_eq!(c.source_file, PathBuf::from("src/lib.rs"));
                assert_eq!(
                    c.output_file,
                    PathBuf::from("/target/release/deps/libserde-abc123def.rlib")
                );
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn rustc_original_args_preserved() {
        let input = args(&["--edition", "2021", "--crate-type", "lib", "src/lib.rs"]);
        let result = parse_invocation("rustc", &input);
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(*c.original_args, *input);
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn rustc_equal_form_crate_type() {
        let result = parse_invocation("rustc", &args(&["--crate-type=lib", "src/lib.rs"]));
        assert!(matches!(result, ParsedInvocation::Cacheable(_)));
    }

    #[test]
    fn rustc_concatenated_c_incremental_is_cacheable() {
        // -Cincremental= form (no space after -C) — still cacheable
        let result = parse_invocation(
            "rustc",
            &args(&["--crate-type", "lib", "-Cincremental=/tmp", "src/lib.rs"]),
        );
        assert!(matches!(result, ParsedInvocation::Cacheable(_)));
    }

    #[test]
    fn rustc_comma_separated_crate_type_all_cacheable() {
        let result = parse_invocation("rustc", &args(&["--crate-type", "lib,rlib", "src/lib.rs"]));
        assert!(matches!(result, ParsedInvocation::Cacheable(_)));
    }

    #[test]
    fn rustc_comma_separated_crate_type_mixed_non_cacheable() {
        // lib is cacheable but dylib is not
        let result = parse_invocation("rustc", &args(&["--crate-type", "lib,dylib", "src/lib.rs"]));
        assert!(matches!(result, ParsedInvocation::NonCacheable { .. }));
    }

    #[test]
    fn rustc_comma_separated_crate_type_equals_form() {
        let result = parse_invocation(
            "rustc",
            &args(&["--crate-type=lib,staticlib", "src/lib.rs"]),
        );
        assert!(matches!(result, ParsedInvocation::Cacheable(_)));
    }

    #[test]
    fn rustc_test_flag_makes_non_cacheable() {
        // --test compiles a test harness (implicitly bin, not cacheable)
        let result = parse_invocation(
            "rustc",
            &args(&["--crate-type", "lib", "--test", "src/lib.rs"]),
        );
        // --test gets captured as unknown_flag. Since --crate-type lib is specified
        // the compilation IS cacheable. The --test flag is in unknown_flags which
        // is part of the cache key, so different --test values produce different keys.
        // This is correct: `--test` with `--crate-type lib` is a valid cacheable invocation.
        assert!(matches!(result, ParsedInvocation::Cacheable(_)));
    }

    // ─── clippy-driver detection and caching tests ──────────────────

    #[test]
    fn detect_clippy_driver_family() {
        assert_eq!(detect_family("clippy-driver"), CompilerFamily::Rustc);
        assert_eq!(
            detect_family(
                "/home/user/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/clippy-driver"
            ),
            CompilerFamily::Rustc
        );
        assert_eq!(
            detect_family("C:\\Users\\user\\.rustup\\toolchains\\stable-x86_64-pc-windows-msvc\\bin\\clippy-driver.exe"),
            CompilerFamily::Rustc
        );
    }

    #[test]
    fn detect_clippy_driver_versioned() {
        // Versioned clippy-driver (e.g., from rustup with custom toolchains)
        assert_eq!(detect_family("clippy-driver-1.78"), CompilerFamily::Rustc);
    }

    #[test]
    fn clippy_driver_cacheable_lib() {
        // cargo clippy invokes: clippy-driver --crate-type lib --crate-name foo src/lib.rs ...
        let result = parse_invocation(
            "clippy-driver",
            &args(&[
                "--crate-name",
                "mycrate",
                "--crate-type",
                "lib",
                "--emit=metadata,dep-info",
                "--out-dir",
                "target/debug/deps",
                "-C",
                "extra-filename=-abc123",
                "src/lib.rs",
            ]),
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.family, CompilerFamily::Rustc);
                assert_eq!(c.source_file, PathBuf::from("src/lib.rs"));
                // metadata-only emit → .rmeta extension
                assert!(c.output_file.to_str().unwrap().ends_with(".rmeta"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn clippy_driver_non_cacheable_bin() {
        // Binary crate type is not cacheable (same as rustc)
        let result = parse_invocation(
            "clippy-driver",
            &args(&[
                "--crate-name",
                "mybin",
                "--crate-type",
                "bin",
                "src/main.rs",
            ]),
        );
        assert!(matches!(result, ParsedInvocation::NonCacheable { .. }));
    }

    #[test]
    fn clippy_driver_with_lint_flags() {
        // clippy-specific lint flags are standard rustc -W/-A/-D flags
        let result = parse_invocation(
            "clippy-driver",
            &args(&[
                "--crate-name",
                "mycrate",
                "--crate-type",
                "lib",
                "-W",
                "clippy::all",
                "-D",
                "clippy::unwrap_used",
                "-A",
                "clippy::too_many_arguments",
                "src/lib.rs",
            ]),
        );
        assert!(matches!(result, ParsedInvocation::Cacheable(_)));
    }

    // ─── C++20 Module support tests ─────────────────────────────────

    // Group A: Source extension recognition (.cppm, .ixx)

    #[test]
    fn cppm_extension_is_cacheable() {
        let result = parse_invocation("clang++", &args(&["-c", "module.cppm", "-o", "module.pcm"]));
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, PathBuf::from("module.cppm"));
                assert_eq!(c.output_file, PathBuf::from("module.pcm"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn ixx_extension_is_cacheable() {
        let result = parse_invocation("g++", &args(&["-c", "module.ixx", "-o", "module.o"]));
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, PathBuf::from("module.ixx"));
                assert_eq!(c.output_file, PathBuf::from("module.o"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn cppm_default_output_with_precompile_is_pcm() {
        // --precompile without -o should produce stem.pcm
        let result = parse_invocation("clang++", &args(&["--precompile", "module.cppm"]));
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, PathBuf::from("module.cppm"));
                assert_eq!(c.output_file, PathBuf::from("module.pcm"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn cppm_default_output_with_c_flag_is_object() {
        // -c on a .cppm without -o should produce stem.o (normal object)
        let result = parse_invocation("clang++", &args(&["-c", "module.cppm"]));
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, PathBuf::from("module.cppm"));
                assert_eq!(c.output_file, PathBuf::from("module.o"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn cppm_multi_file() {
        let result = parse_invocation("clang++", &args(&["-c", "a.cppm", "b.cppm"]));
        match result {
            ParsedInvocation::MultiFile { compilations, .. } => {
                assert_eq!(compilations.len(), 2);
                assert_eq!(compilations[0].source_file, PathBuf::from("a.cppm"));
                assert_eq!(compilations[1].source_file, PathBuf::from("b.cppm"));
            }
            other => panic!("expected MultiFile, got: {other:?}"),
        }
    }

    // Group B: -x c++-module language mode

    #[test]
    fn x_cpp_module_with_precompile_is_cacheable() {
        let result = parse_invocation(
            "clang++",
            &args(&[
                "-x",
                "c++-module",
                "--precompile",
                "interface.cpp",
                "-o",
                "interface.pcm",
            ]),
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, PathBuf::from("interface.cpp"));
                assert_eq!(c.output_file, PathBuf::from("interface.pcm"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn x_cpp_module_with_c_flag_is_cacheable() {
        let result = parse_invocation(
            "clang++",
            &args(&[
                "-x",
                "c++-module",
                "-c",
                "interface.cpp",
                "-o",
                "interface.o",
            ]),
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, PathBuf::from("interface.cpp"));
                assert_eq!(c.output_file, PathBuf::from("interface.o"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn x_cpp_module_without_c_or_precompile_is_non_cacheable() {
        // Module mode alone does NOT imply compilation (unlike header mode).
        // Without -c or --precompile, this is a link invocation.
        let result = parse_invocation(
            "clang++",
            &args(&["-x", "c++-module", "interface.cpp", "-o", "interface"]),
        );
        assert!(
            matches!(result, ParsedInvocation::NonCacheable { .. }),
            "-x c++-module without -c or --precompile should be non-cacheable, got: {result:?}"
        );
    }

    #[test]
    fn x_cpp_module_accepts_non_source_extension() {
        // -x c++-module should allow any positional arg as a source file
        // (same behavior as -x c++-header with non-standard extensions).
        let result = parse_invocation(
            "clang++",
            &args(&["-x", "c++-module", "--precompile", "interface.mpp"]),
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, PathBuf::from("interface.mpp"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn x_cpp_module_default_output_precompile() {
        // --precompile without -o → stem.pcm
        let result = parse_invocation(
            "clang++",
            &args(&["-x", "c++-module", "--precompile", "interface.cpp"]),
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.output_file, PathBuf::from("interface.pcm"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn x_cpp_module_default_output_c_flag() {
        // -c without -o → stem.o (even in module mode)
        let result = parse_invocation(
            "clang++",
            &args(&["-x", "c++-module", "-c", "interface.cpp"]),
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.output_file, PathBuf::from("interface.o"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn x_cpp_module_reset_by_x_cpp() {
        // -x c++ resets module mode, just like it resets header mode.
        let result = parse_invocation(
            "clang++",
            &args(&[
                "-x",
                "c++-module",
                "--precompile",
                "interface.mpp",
                "-x",
                "c++",
                "-c",
                "main.cpp",
            ]),
        );
        match result {
            ParsedInvocation::MultiFile { compilations, .. } => {
                assert_eq!(compilations.len(), 2);
                assert_eq!(compilations[0].source_file, PathBuf::from("interface.mpp"));
                assert_eq!(compilations[1].source_file, PathBuf::from("main.cpp"));
            }
            other => panic!("expected MultiFile, got: {other:?}"),
        }
    }

    #[test]
    fn x_cpp_module_implies_compilation_with_precompile() {
        // --precompile without -c is still a cacheable compilation.
        let result = parse_invocation(
            "clang++",
            &args(&[
                "-x",
                "c++-module",
                "--precompile",
                "interface.cpp",
                "-o",
                "interface.pcm",
            ]),
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, PathBuf::from("interface.cpp"));
                assert_eq!(c.output_file, PathBuf::from("interface.pcm"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    // Group C: Header units (-x c++-header-unit / -x c-header-unit)

    #[test]
    fn x_cpp_header_unit_with_precompile_is_cacheable() {
        let result = parse_invocation(
            "clang++",
            &args(&[
                "-x",
                "c++-header-unit",
                "--precompile",
                "foo.h",
                "-o",
                "foo.pcm",
            ]),
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, PathBuf::from("foo.h"));
                assert_eq!(c.output_file, PathBuf::from("foo.pcm"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn x_c_header_unit_with_c_flag_is_cacheable() {
        let result = parse_invocation(
            "gcc",
            &args(&["-x", "c-header-unit", "-c", "foo.h", "-o", "foo.pcm"]),
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, PathBuf::from("foo.h"));
                assert_eq!(c.output_file, PathBuf::from("foo.pcm"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn x_cpp_header_unit_default_output_is_pcm() {
        // Header unit without -o → filename.pcm
        let result = parse_invocation(
            "clang++",
            &args(&["-x", "c++-header-unit", "--precompile", "foo.h"]),
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.output_file, PathBuf::from("foo.h.pcm"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn x_cpp_header_unit_implies_compilation() {
        // Header-unit mode implies compilation (no -c needed), like header mode.
        let result = parse_invocation(
            "clang++",
            &args(&["-x", "c++-header-unit", "foo.h", "-o", "foo.pcm"]),
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, PathBuf::from("foo.h"));
                assert_eq!(c.output_file, PathBuf::from("foo.pcm"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    // Group D: --precompile flag handling

    #[test]
    fn precompile_on_normal_cpp_is_cacheable() {
        // --precompile on a .cpp (with export module inside) is valid.
        let result = parse_invocation("clang++", &args(&["--precompile", "foo.cpp"]));
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, PathBuf::from("foo.cpp"));
                assert_eq!(c.output_file, PathBuf::from("foo.pcm"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn precompile_without_source_is_non_cacheable() {
        let result = parse_invocation("clang++", &args(&["--precompile", "-O2"]));
        assert!(
            matches!(result, ParsedInvocation::NonCacheable { .. }),
            "--precompile without source should be non-cacheable, got: {result:?}"
        );
    }

    #[test]
    fn precompile_and_c_flag_together() {
        // Both --precompile and -c can coexist. --precompile takes precedence
        // for default output (produces .pcm, not .o).
        let result = parse_invocation(
            "clang++",
            &args(&["--precompile", "-c", "module.cppm", "-o", "module.pcm"]),
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, PathBuf::from("module.cppm"));
                assert_eq!(c.output_file, PathBuf::from("module.pcm"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    // Group E: GCC -fmodules-ts interaction

    #[test]
    fn gcc_fmodules_ts_with_cppm_is_cacheable() {
        // -fmodules-ts falls through to unknown_flags, which is fine.
        let result = parse_invocation(
            "g++",
            &args(&["-fmodules-ts", "-c", "module.cppm", "-o", "module.o"]),
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, PathBuf::from("module.cppm"));
                assert_eq!(c.output_file, PathBuf::from("module.o"));
                assert!(c.unknown_flags.contains(&"-fmodules-ts".to_string()));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }

    #[test]
    fn gcc_fmodules_ts_with_x_module_precompile() {
        let result = parse_invocation(
            "g++",
            &args(&[
                "-fmodules-ts",
                "-x",
                "c++-module",
                "--precompile",
                "interface.cpp",
            ]),
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, PathBuf::from("interface.cpp"));
                assert_eq!(c.output_file, PathBuf::from("interface.pcm"));
            }
            other => panic!("expected cacheable, got: {other:?}"),
        }
    }
}
