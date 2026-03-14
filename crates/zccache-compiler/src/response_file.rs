//! Response file (`@file`) expansion for compiler arguments.
//!
//! Both GCC and Clang support `@filename` syntax where the file contains
//! additional command-line arguments. This module handles reading and
//! expanding those files into the argument list.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Maximum nesting depth for response files to prevent stack overflow.
const MAX_DEPTH: usize = 10;

/// Errors that can occur during response file expansion.
#[derive(Debug, thiserror::Error)]
pub enum ResponseFileError {
    /// The response file could not be read.
    #[error("failed to read response file '{path}': {source}")]
    ReadError {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Circular reference detected among response files.
    #[error("circular response file reference: '{path}'")]
    CircularReference { path: PathBuf },
    /// Response file nesting exceeded the maximum depth.
    #[error("response file nesting too deep (max {MAX_DEPTH}): '{path}'")]
    TooDeep { path: PathBuf },
}

/// Parse the content of a response file into individual arguments.
///
/// Follows GCC/Clang response file conventions:
/// - Arguments are separated by whitespace (spaces, tabs, newlines)
/// - Arguments can be quoted with single (`'`) or double (`"`) quotes
/// - Inside double quotes, backslash escapes `\\`, `\"`, and `\n`
/// - Inside single quotes, no escape processing (literal content)
/// - Unquoted backslash is literal (important for Windows paths)
#[must_use]
pub fn parse_response_file_content(content: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = String::new();
    let mut in_arg = false;
    let mut chars = content.chars().peekable();

    while let Some(&ch) = chars.peek() {
        match ch {
            // Whitespace separates arguments
            ' ' | '\t' | '\n' | '\r' => {
                if in_arg {
                    result.push(std::mem::take(&mut current));
                    in_arg = false;
                }
                chars.next();
            }
            // Double-quoted string
            '"' => {
                in_arg = true;
                chars.next(); // consume opening quote
                loop {
                    match chars.next() {
                        Some('"') | None => break,
                        Some('\\') => match chars.next() {
                            Some('n') => current.push('\n'),
                            Some(c) => current.push(c),
                            None => break,
                        },
                        Some(c) => current.push(c),
                    }
                }
            }
            // Single-quoted string (no escapes)
            '\'' => {
                in_arg = true;
                chars.next(); // consume opening quote
                loop {
                    match chars.next() {
                        Some('\'') | None => break,
                        Some(c) => current.push(c),
                    }
                }
            }
            // Regular character (backslash is literal in unquoted context)
            _ => {
                in_arg = true;
                current.push(ch);
                chars.next();
            }
        }
    }

    if in_arg {
        result.push(current);
    }

    result
}

/// Expand response file references (`@filename`) in an argument list.
///
/// Scans `args` for arguments starting with `@`. For each such argument,
/// reads the referenced file, parses its contents, and splices the
/// resulting arguments into the list. Supports nested response files
/// up to [`MAX_DEPTH`] levels. Detects circular references.
///
/// Arguments that are exactly `@` (with no filename) are passed through
/// unchanged, as they are not valid response file references.
pub fn expand_response_files(args: &[String]) -> Result<Vec<String>, ResponseFileError> {
    let mut seen = HashSet::new();
    expand_recursive(args, &mut seen, 0)
}

fn expand_recursive(
    args: &[String],
    seen: &mut HashSet<PathBuf>,
    depth: usize,
) -> Result<Vec<String>, ResponseFileError> {
    let mut result = Vec::new();

    for arg in args {
        if let Some(filename) = arg.strip_prefix('@') {
            if filename.is_empty() {
                // Bare `@` is not a response file reference
                result.push(arg.clone());
                continue;
            }

            let path = Path::new(filename);
            let canonical = path
                .canonicalize()
                .map_err(|e| ResponseFileError::ReadError {
                    path: path.to_path_buf(),
                    source: e,
                })?;

            if !seen.insert(canonical.clone()) {
                return Err(ResponseFileError::CircularReference {
                    path: path.to_path_buf(),
                });
            }

            if depth >= MAX_DEPTH {
                return Err(ResponseFileError::TooDeep {
                    path: path.to_path_buf(),
                });
            }

            let content =
                std::fs::read_to_string(&canonical).map_err(|e| ResponseFileError::ReadError {
                    path: path.to_path_buf(),
                    source: e,
                })?;

            let expanded_args = parse_response_file_content(&content);
            let nested = expand_recursive(&expanded_args, seen, depth + 1)?;
            result.extend(nested);

            // Remove from seen so the same file can appear in sibling branches
            // (circular = same file in ancestor chain, not sibling)
            seen.remove(&canonical);
        } else {
            result.push(arg.clone());
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    // ── parse_response_file_content tests ──

    #[test]
    fn parse_simple_whitespace_separated() {
        let result = parse_response_file_content("-c foo.cpp -o foo.o");
        assert_eq!(result, s(&["-c", "foo.cpp", "-o", "foo.o"]));
    }

    #[test]
    fn parse_newline_separated() {
        let result = parse_response_file_content("-c\nfoo.cpp\n-o\nfoo.o\n");
        assert_eq!(result, s(&["-c", "foo.cpp", "-o", "foo.o"]));
    }

    #[test]
    fn parse_mixed_whitespace() {
        let result = parse_response_file_content("  -c \t foo.cpp \n -O2  ");
        assert_eq!(result, s(&["-c", "foo.cpp", "-O2"]));
    }

    #[test]
    fn parse_double_quoted_string() {
        let result = parse_response_file_content(r#"-DMSG="hello world" -c foo.c"#);
        assert_eq!(result, s(&["-DMSG=hello world", "-c", "foo.c"]));
    }

    #[test]
    fn parse_single_quoted_string() {
        let result = parse_response_file_content("-DMSG='hello world' -c foo.c");
        assert_eq!(result, s(&["-DMSG=hello world", "-c", "foo.c"]));
    }

    #[test]
    fn parse_escaped_backslash_in_double_quotes() {
        let result = parse_response_file_content(r#""-I C:\\path\\to\\include""#);
        assert_eq!(result, s(&["-I C:\\path\\to\\include"]));
    }

    #[test]
    fn parse_escaped_quote_in_double_quotes() {
        let result = parse_response_file_content(r#""-DMSG=\"hi\"""#);
        assert_eq!(result, s(&[r#"-DMSG="hi""#]));
    }

    #[test]
    fn parse_escaped_newline_in_double_quotes() {
        let result = parse_response_file_content(r#""-DMSG=line1\nline2""#);
        assert_eq!(result, s(&["-DMSG=line1\nline2"]));
    }

    #[test]
    fn parse_single_quotes_no_escapes() {
        // Single quotes are literal — backslash is not special
        let result = parse_response_file_content(r"'-DMSG=a\nb'");
        assert_eq!(result, s(&[r"-DMSG=a\nb"]));
    }

    #[test]
    fn parse_unquoted_backslash_literal() {
        // Backslash is literal in unquoted context (important for Windows paths)
        let result = parse_response_file_content(r"-IC:\Users\include");
        assert_eq!(result, s(&[r"-IC:\Users\include"]));
    }

    #[test]
    fn parse_empty_content() {
        let result = parse_response_file_content("");
        assert!(result.is_empty());
    }

    #[test]
    fn parse_only_whitespace() {
        let result = parse_response_file_content("   \n\t\r\n  ");
        assert!(result.is_empty());
    }

    #[test]
    fn parse_empty_quoted_string() {
        let result = parse_response_file_content(r#""""#);
        assert_eq!(result, s(&[""]));
    }

    #[test]
    fn parse_adjacent_quoted_and_unquoted() {
        // -I"path with spaces" should produce -Ipath with spaces as one arg
        let result = parse_response_file_content(r#"-I"path with spaces""#);
        assert_eq!(result, s(&["-Ipath with spaces"]));
    }

    // ── expand_response_files tests ──

    #[test]
    fn expand_no_at_files() {
        let args = s(&["-c", "foo.cpp", "-o", "foo.o"]);
        let result = expand_response_files(&args).unwrap();
        assert_eq!(result, args);
    }

    #[test]
    fn expand_bare_at_passthrough() {
        // A bare `@` with no filename is not a response file reference
        let args = s(&["-c", "@", "foo.cpp"]);
        let result = expand_response_files(&args).unwrap();
        assert_eq!(result, args);
    }

    #[test]
    fn expand_single_response_file() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "-O2 -Wall -DNDEBUG").unwrap();

        let path = f.path().to_str().unwrap();
        let args = s(&["-c", "foo.cpp", &format!("@{path}")]);
        let result = expand_response_files(&args).unwrap();
        assert_eq!(result, s(&["-c", "foo.cpp", "-O2", "-Wall", "-DNDEBUG"]));
    }

    #[test]
    fn expand_response_file_with_quoted_args() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, r#"-DMSG="hello world" -I"/path/to/include""#).unwrap();

        let path = f.path().to_str().unwrap();
        let args = s(&["-c", "foo.cpp", &format!("@{path}")]);
        let result = expand_response_files(&args).unwrap();
        assert_eq!(
            result,
            s(&["-c", "foo.cpp", "-DMSG=hello world", "-I/path/to/include"])
        );
    }

    #[test]
    fn expand_multiple_response_files() {
        let mut f1 = NamedTempFile::new().unwrap();
        writeln!(f1, "-O2 -Wall").unwrap();
        let mut f2 = NamedTempFile::new().unwrap();
        writeln!(f2, "-DNDEBUG -std=c++17").unwrap();

        let p1 = f1.path().to_str().unwrap();
        let p2 = f2.path().to_str().unwrap();
        let args = s(&["-c", "foo.cpp", &format!("@{p1}"), &format!("@{p2}")]);
        let result = expand_response_files(&args).unwrap();
        assert_eq!(
            result,
            s(&["-c", "foo.cpp", "-O2", "-Wall", "-DNDEBUG", "-std=c++17"])
        );
    }

    #[test]
    fn expand_nested_response_files() {
        let mut inner = NamedTempFile::new().unwrap();
        writeln!(inner, "-DINNER=1").unwrap();

        let inner_path = inner.path().to_str().unwrap();
        let mut outer = NamedTempFile::new().unwrap();
        writeln!(outer, "-DOUTER=1 @{inner_path}").unwrap();

        let outer_path = outer.path().to_str().unwrap();
        let args = s(&["-c", "foo.cpp", &format!("@{outer_path}")]);
        let result = expand_response_files(&args).unwrap();
        assert_eq!(result, s(&["-c", "foo.cpp", "-DOUTER=1", "-DINNER=1"]));
    }

    #[test]
    fn expand_missing_file_errors() {
        let args = s(&["-c", "foo.cpp", "@/nonexistent/file.rsp"]);
        let result = expand_response_files(&args);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ResponseFileError::ReadError { .. }));
    }

    #[test]
    fn expand_circular_reference_errors() {
        // Create two files that reference each other
        let dir = tempfile::tempdir().unwrap();
        let path_a = dir.path().join("a.rsp");
        let path_b = dir.path().join("b.rsp");

        std::fs::write(&path_a, format!("@{}", path_b.display())).unwrap();
        std::fs::write(&path_b, format!("@{}", path_a.display())).unwrap();

        let args = s(&["-c", "foo.cpp", &format!("@{}", path_a.display())]);
        let result = expand_response_files(&args);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, ResponseFileError::CircularReference { .. }),
            "expected CircularReference, got: {err}"
        );
    }

    #[test]
    fn expand_self_reference_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("self.rsp");
        std::fs::write(&path, format!("-O2 @{}", path.display())).unwrap();

        let args = s(&[&format!("@{}", path.display())]);
        let result = expand_response_files(&args);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, ResponseFileError::CircularReference { .. }),
            "expected CircularReference, got: {err}"
        );
    }

    #[test]
    fn expand_preserves_arg_order() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "-B -C").unwrap();

        let path = f.path().to_str().unwrap();
        let args = s(&["-A", &format!("@{path}"), "-D"]);
        let result = expand_response_files(&args).unwrap();
        assert_eq!(result, s(&["-A", "-B", "-C", "-D"]));
    }

    #[test]
    fn expand_same_file_in_siblings_ok() {
        // The same file referenced twice at the same depth is OK
        // (circular = same file in ancestor chain)
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "-O2").unwrap();

        let path = f.path().to_str().unwrap();
        let args = s(&[&format!("@{path}"), &format!("@{path}")]);
        let result = expand_response_files(&args).unwrap();
        assert_eq!(result, s(&["-O2", "-O2"]));
    }

    #[test]
    fn expand_integration_with_parse_invocation() {
        // End-to-end: expand response files, then parse the invocation
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "-O2 -Wall -DNDEBUG").unwrap();

        let path = f.path().to_str().unwrap();
        let args = s(&["-c", "foo.cpp", "-o", "foo.o", &format!("@{path}")]);
        let expanded = expand_response_files(&args).unwrap();
        let result = crate::parse_invocation("gcc", &expanded);
        match result {
            crate::ParsedInvocation::Cacheable(c) => {
                assert_eq!(c.source_file, PathBuf::from("foo.cpp"));
                assert_eq!(c.output_file, PathBuf::from("foo.o"));
                assert!(c.original_args.contains(&"-O2".to_string()));
                assert!(c.original_args.contains(&"-Wall".to_string()));
                assert!(c.original_args.contains(&"-DNDEBUG".to_string()));
            }
            _ => panic!("unexpected variant"),
        }
    }
}
