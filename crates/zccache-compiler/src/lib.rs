//! Compiler detection and argument parsing for zccache.
//!
//! Handles identifying compilers, parsing their command-line arguments
//! to determine cacheability, and extracting cache-relevant information.

#![allow(clippy::missing_errors_doc)]

pub mod parse_archiver;
pub mod parse_linker;
pub mod response_file;

use std::path::PathBuf;

/// Supported compiler families.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompilerFamily {
    /// GCC (gcc, g++)
    Gcc,
    /// Clang (clang, clang++)
    Clang,
    // Future: Msvc, etc.
}

impl CompilerFamily {
    /// Whether this compiler supports `-MD -MF` for depfile generation.
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
    /// The shared flags are carried so cache misses can be batched into one compiler call.
    MultiFile {
        /// One entry per source file, each with its own output path.
        compilations: Vec<CacheableCompilation>,
        /// The original full argument list (for batched compiler invocation of misses).
        original_args: Vec<String>,
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
    /// Arguments relevant to cache keying (optimization, defines, includes, etc.).
    pub cache_relevant_args: Vec<String>,
    /// Arguments relevant to compilation but not cache keying.
    pub pass_through_args: Vec<String>,
    /// The full original argument list (for fallback execution).
    pub original_args: Vec<String>,
}

/// Source file extensions we recognize as C/C++.
const SOURCE_EXTENSIONS: &[&str] = &["c", "cc", "cpp", "cxx", "c++", "C", "m", "mm", "i", "ii"];

/// Detect the compiler family from the compiler path.
fn detect_family(compiler: &str) -> CompilerFamily {
    let name = std::path::Path::new(compiler)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(compiler);
    if name.contains("clang") {
        CompilerFamily::Clang
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
#[must_use]
pub fn parse_invocation(compiler: &str, args: &[String]) -> ParsedInvocation {
    let mut has_c_flag = false;
    let mut source_files: Vec<String> = Vec::new();
    let mut output_file: Option<String> = None;
    let mut cache_relevant_args = Vec::new();

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

        // Flags that take a value in the next arg
        if let Some(&flag) = FLAGS_WITH_VALUE.iter().find(|&&f| f == arg.as_str()) {
            if flag != "-o" {
                cache_relevant_args.push(arg.clone());
                i += 1;
                if i < args.len() {
                    cache_relevant_args.push(args[i].clone());
                }
            }
            i += 1;
            continue;
        }

        // Flags with = syntax (e.g., -std=c++17, --target=..., -D...)
        if arg.starts_with('-') {
            cache_relevant_args.push(arg.clone());
            i += 1;
            continue;
        }

        // Positional arg — should be a source file
        if is_source_file(arg) {
            source_files.push(arg.clone());
        }

        i += 1;
    }

    if !has_c_flag {
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
        let compilations = source_files
            .iter()
            .map(|src| {
                let stem = std::path::Path::new(src)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("a");
                // Each unit gets its own single-file args for correct context building.
                let mut unit_args = vec!["-c".to_string(), src.clone()];
                unit_args.extend(cache_relevant_args.iter().cloned());
                CacheableCompilation {
                    compiler: PathBuf::from(compiler),
                    family,
                    source_file: PathBuf::from(src),
                    output_file: PathBuf::from(format!("{stem}.o")),
                    cache_relevant_args: cache_relevant_args.clone(),
                    pass_through_args: Vec::new(),
                    original_args: unit_args,
                }
            })
            .collect();
        return ParsedInvocation::MultiFile {
            compilations,
            original_args: args.to_vec(),
        };
    }

    // Single source file
    let source = source_files.into_iter().next().unwrap();
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
        cache_relevant_args,
        pass_through_args: Vec::new(),
        original_args: args.to_vec(),
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
            ParsedInvocation::MultiFile { compilations, .. } => {
                assert_eq!(compilations.len(), 2);
                assert_eq!(compilations[0].source_file, PathBuf::from("a.cpp"));
                assert_eq!(compilations[0].output_file, PathBuf::from("a.o"));
                assert_eq!(compilations[1].source_file, PathBuf::from("b.cpp"));
                assert_eq!(compilations[1].output_file, PathBuf::from("b.o"));
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
            ParsedInvocation::MultiFile { compilations, .. } => {
                assert_eq!(compilations.len(), 2);
                assert_eq!(compilations[0].source_file, PathBuf::from("main.cpp"));
                assert_eq!(compilations[1].source_file, PathBuf::from("util.cpp"));
                // Shared flags are preserved on each compilation
                assert!(compilations[0]
                    .cache_relevant_args
                    .contains(&"-O2".to_string()));
                assert!(compilations[0]
                    .cache_relevant_args
                    .contains(&"-Wall".to_string()));
                assert!(compilations[1]
                    .cache_relevant_args
                    .contains(&"-O2".to_string()));
                assert!(compilations[1]
                    .cache_relevant_args
                    .contains(&"-Wall".to_string()));
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
    fn cache_relevant_args_extracted() {
        let result = parse_invocation(
            "clang++",
            &args(&["-c", "hello.cpp", "-O2", "-std=c++17", "-DNDEBUG", "-Wall"]),
        );
        match result {
            ParsedInvocation::Cacheable(c) => {
                assert!(c.cache_relevant_args.contains(&"-O2".to_string()));
                assert!(c.cache_relevant_args.contains(&"-std=c++17".to_string()));
                assert!(c.cache_relevant_args.contains(&"-DNDEBUG".to_string()));
                assert!(c.cache_relevant_args.contains(&"-Wall".to_string()));
            }
            _ => panic!("expected cacheable"),
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
                // PCH flag and its value are both in cache_relevant_args
                assert!(c.cache_relevant_args.contains(&"-include-pch".to_string()));
                assert!(c.cache_relevant_args.contains(&"pch.h.pch".to_string()));
                // PCH path is NOT treated as a source file
                assert_eq!(c.source_file, PathBuf::from("foo.cpp"));
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
    fn gcc_supports_depfile() {
        assert!(CompilerFamily::Gcc.supports_depfile());
    }

    #[test]
    fn clang_supports_depfile() {
        assert!(CompilerFamily::Clang.supports_depfile());
    }
}
