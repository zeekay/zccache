//! Compiler detection and argument parsing for zccache.
//!
//! Handles identifying compilers, parsing their command-line arguments
//! to determine cacheability, and extracting cache-relevant information.

#![allow(clippy::missing_errors_doc)]

pub mod parse_archiver;
pub mod parse_linker;
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
}

impl CompilerFamily {
    /// Whether this compiler supports `-MD -MF` for depfile generation.
    /// MSVC uses `/showIncludes` instead.
    #[must_use]
    pub fn supports_depfile(&self) -> bool {
        matches!(self, CompilerFamily::Gcc | CompilerFamily::Clang)
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
}

/// Check if a `-x` language value is a header language (PCH generation).
/// Uses exact match — does not match hypothetical values like `c-header-unit`.
fn is_header_language(lang: &str) -> bool {
    matches!(lang, "c-header" | "c++-header")
}

/// Source file extensions we recognize as C/C++.
const SOURCE_EXTENSIONS: &[&str] = &["c", "cc", "cpp", "cxx", "c++", "C", "m", "mm", "i", "ii"];

/// Detect the compiler family from the compiler path.
#[must_use]
pub fn detect_family(compiler: &str) -> CompilerFamily {
    let name = std::path::Path::new(compiler)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(compiler);
    if name.contains("clang") {
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

/// Flags that take a following argument (value in next argv element).
const FLAGS_WITH_VALUE: &[&str] = &[
    "-o",
    "-D",
    "-U",
    "-I",
    "-isystem",
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
    let mut has_c_flag = false;
    let mut source_files: Vec<(String, usize)> = Vec::new();
    let mut output_file: Option<String> = None;
    let mut header_mode = false;

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

        // -o takes the next arg as output file
        if arg == "-o" {
            i += 1;
            if i < args.len() {
                output_file = Some(args[i].clone());
            }
            i += 1;
            continue;
        }

        // Flags that take a value in the next arg — skip both flag and value
        if let Some(&flag) = FLAGS_WITH_VALUE.iter().find(|&&f| f == arg.as_str()) {
            if flag == "-x" && i + 1 < args.len() {
                header_mode = is_header_language(&args[i + 1]);
            }
            i += 2;
            continue;
        }

        // Any flag starting with - (including unknown flags) — skip
        if arg.starts_with('-') {
            i += 1;
            continue;
        }

        // Positional arg — source file candidate
        if is_source_file(arg) || header_mode {
            source_files.push((arg.clone(), i));
        }

        i += 1;
    }

    // `-x c++-header` / `-x c-header` implies compilation (PCH generation)
    // even without an explicit `-c` flag. Clang treats header mode as
    // "compile to PCH, don't link", so `-c` is redundant.
    if !has_c_flag && !header_mode {
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
        let source_indices: Vec<usize> = source_files.iter().map(|(_, idx)| *idx).collect();
        let shared_args: Arc<[String]> = Arc::from(args.to_vec());
        let compilations = source_files
            .iter()
            .map(|(src, _)| {
                let stem = std::path::Path::new(src)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("a");
                CacheableCompilation {
                    compiler: PathBuf::from(compiler),
                    family,
                    source_file: PathBuf::from(src),
                    output_file: PathBuf::from(format!("{stem}.o")),
                    original_args: Arc::clone(&shared_args),
                }
            })
            .collect();
        return ParsedInvocation::MultiFile {
            compilations,
            original_args: shared_args,
            source_indices,
        };
    }

    // Single source file
    let (source, _) = source_files.into_iter().next().unwrap();
    let output = output_file.unwrap_or_else(|| {
        let stem = std::path::Path::new(&source)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("a");
        format!("{stem}.o")
    });

    ParsedInvocation::Cacheable(CacheableCompilation {
        compiler: PathBuf::from(compiler),
        family,
        source_file: PathBuf::from(source),
        output_file: PathBuf::from(output),
        original_args: Arc::from(args.to_vec()),
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
    fn header_language_exact_match_no_prefix() {
        // `-x c-header-unit` should NOT activate header_mode (exact match only).
        // With old starts_with("c-header"), this would have set header_mode=true.
        let result = parse_invocation(
            "clang++",
            &args(&["-x", "c-header-unit", "foo.h", "-o", "foo.pcm"]),
        );
        assert!(
            matches!(result, ParsedInvocation::NonCacheable { .. }),
            "c-header-unit should not activate header mode, got: {result:?}"
        );
    }

    #[test]
    fn header_language_exact_match_cpp_no_prefix() {
        // `-x c++-header-unit` should NOT activate header_mode.
        let result = parse_invocation(
            "clang++",
            &args(&["-x", "c++-header-unit", "foo.h", "-o", "foo.pcm"]),
        );
        assert!(
            matches!(result, ParsedInvocation::NonCacheable { .. }),
            "c++-header-unit should not activate header mode, got: {result:?}"
        );
    }

    #[test]
    fn detect_clang_family() {
        assert_eq!(detect_family("clang++"), CompilerFamily::Clang);
        assert_eq!(detect_family("/usr/bin/clang"), CompilerFamily::Clang);
        assert_eq!(detect_family("gcc"), CompilerFamily::Gcc);
        assert_eq!(detect_family("g++"), CompilerFamily::Gcc);
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
}
