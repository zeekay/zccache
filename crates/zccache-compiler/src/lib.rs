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

/// Parse a compiler invocation's arguments to determine cacheability.
///
/// Returns a `ParsedInvocation` indicating whether the invocation is
/// cacheable, and if so, extracts the relevant information.
#[must_use]
pub fn parse_invocation(compiler: &str, args: &[String]) -> ParsedInvocation {
    // TODO: Implement real argument parsing for gcc/clang.
    // For now, this is a stub that marks everything as non-cacheable.
    let _ = compiler;
    let _ = args;
    ParsedInvocation::NonCacheable {
        reason: String::from("argument parsing not yet implemented"),
    }
}
