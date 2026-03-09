//! Compiler detection and argument parsing for zccache.
//!
//! Handles identifying compilers, parsing their command-line arguments
//! to determine cacheability, and extracting cache-relevant information.

#![allow(clippy::missing_errors_doc)]

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

/// The result of parsing a compiler invocation.
#[derive(Debug, Clone)]
pub enum ParsedInvocation {
    /// A cacheable compilation (single source to single object).
    Cacheable(CacheableCompilation),
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
    let mut source_file: Option<String> = None;
    let mut output_file: Option<String> = None;
    let mut cache_relevant_args = Vec::new();
    let mut multiple_sources = false;

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
            if source_file.is_some() {
                multiple_sources = true;
            }
            source_file = Some(arg.clone());
        }

        i += 1;
    }

    if multiple_sources {
        return ParsedInvocation::NonCacheable {
            reason: "multiple source files".to_string(),
        };
    }

    if !has_c_flag {
        return ParsedInvocation::NonCacheable {
            reason: "no -c flag (likely a link invocation)".to_string(),
        };
    }

    let source = match source_file {
        Some(s) => s,
        None => {
            return ParsedInvocation::NonCacheable {
                reason: "no source file found".to_string(),
            };
        }
    };

    // Default output: source stem + .o
    let output = output_file.unwrap_or_else(|| {
        let stem = std::path::Path::new(&source)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("a");
        format!("{stem}.o")
    });

    let family = detect_family(compiler);

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
            ParsedInvocation::NonCacheable { reason } => {
                panic!("expected cacheable, got: {reason}")
            }
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
    fn multiple_sources_non_cacheable() {
        let result = parse_invocation("gcc", &args(&["-c", "a.cpp", "b.cpp"]));
        assert!(matches!(result, ParsedInvocation::NonCacheable { .. }));
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
    fn detect_clang_family() {
        assert_eq!(detect_family("clang++"), CompilerFamily::Clang);
        assert_eq!(detect_family("/usr/bin/clang"), CompilerFamily::Clang);
        assert_eq!(detect_family("gcc"), CompilerFamily::Gcc);
        assert_eq!(detect_family("g++"), CompilerFamily::Gcc);
    }
}
