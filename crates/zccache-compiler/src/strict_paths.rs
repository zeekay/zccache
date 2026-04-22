//! Strict validation for compiler path flags.

use std::collections::HashMap;
use std::path::Path;

/// Environment variable used by the CLI to enable strict path validation.
pub const STRICT_PATHS_ENV: &str = "ZCCACHE_STRICT_PATHS";

/// Strict path validation policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrictPathMode {
    /// Disable validation.
    Off,
    /// Reject inconsistent include path spellings that can defeat `#pragma once`.
    Consistent,
    /// Require forward-slash absolute include paths with no `.` or `..` segments.
    Absolute,
}

impl StrictPathMode {
    /// Parse a mode string.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "" | "off" | "0" | "false" | "no" => Some(Self::Off),
            "consistent" => Some(Self::Consistent),
            "absolute" | "strict" | "1" | "true" | "yes" => Some(Self::Absolute),
            _ => None,
        }
    }

    /// Return the canonical CLI spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Consistent => "consistent",
            Self::Absolute => "absolute",
        }
    }
}

/// A strict path validation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrictPathViolation {
    /// Flag that introduced the offending path.
    pub flag: String,
    /// Offending path value.
    pub path: String,
    /// Active validation mode.
    pub mode: StrictPathMode,
    /// Human-readable reason.
    pub reason: String,
}

/// Validate compiler path flags according to `mode`.
pub fn validate_strict_paths(
    args: &[String],
    cwd: &Path,
    mode: StrictPathMode,
) -> Result<(), StrictPathViolation> {
    if mode == StrictPathMode::Off {
        return Ok(());
    }

    let paths = collect_path_flags(args);
    if mode == StrictPathMode::Absolute {
        for path_flag in &paths {
            validate_absolute(path_flag, mode)?;
        }
    }

    if mode == StrictPathMode::Consistent || mode == StrictPathMode::Absolute {
        validate_consistent(&paths, cwd, mode)?;
    }

    Ok(())
}

#[derive(Debug)]
struct PathFlag {
    flag: String,
    path: String,
}

fn collect_path_flags(args: &[String]) -> Vec<PathFlag> {
    let mut result = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];

        if takes_next_path(arg) {
            if let Some(path) = args.get(i + 1) {
                result.push(PathFlag {
                    flag: arg.clone(),
                    path: path.clone(),
                });
                i += 2;
                continue;
            }
        }

        if let Some((flag, path)) = split_joined_path_flag(arg) {
            result.push(PathFlag {
                flag: flag.to_string(),
                path: path.to_string(),
            });
        }

        i += 1;
    }
    result
}

fn takes_next_path(arg: &str) -> bool {
    matches!(
        arg,
        "-I" | "/I"
            | "-isystem"
            | "-iquote"
            | "-idirafter"
            | "-include"
            | "-include-pch"
            | "-isysroot"
            | "-imsvc"
            | "/imsvc"
            | "-F"
            | "-iframework"
    )
}

fn split_joined_path_flag(arg: &str) -> Option<(&'static str, &str)> {
    for prefix in ["-isystem", "-iquote", "-idirafter", "-imsvc", "/imsvc"] {
        if let Some(path) = arg.strip_prefix(prefix) {
            if !path.is_empty() {
                return Some((prefix, path));
            }
        }
    }

    if let Some(path) = arg.strip_prefix("--include-directory=") {
        if !path.is_empty() {
            return Some(("--include-directory", path));
        }
    }

    if let Some(path) = arg.strip_prefix("/I") {
        if !path.is_empty() {
            return Some(("/I", path));
        }
    }

    if let Some(path) = arg.strip_prefix("-I") {
        if !path.is_empty() {
            return Some(("-I", path));
        }
    }

    if let Some(path) = arg.strip_prefix("-F") {
        if !path.is_empty() {
            return Some(("-F", path));
        }
    }

    None
}

fn validate_absolute(
    path_flag: &PathFlag,
    mode: StrictPathMode,
) -> Result<(), StrictPathViolation> {
    if path_flag.path.contains('\\') {
        return Err(violation(
            path_flag,
            mode,
            "expected forward-slash absolute path",
        ));
    }
    if !is_absolute_like(&path_flag.path) {
        return Err(violation(
            path_flag,
            mode,
            "expected forward-slash absolute path",
        ));
    }
    if has_dot_segment(&path_flag.path) {
        return Err(violation(
            path_flag,
            mode,
            "expected normalized path without /./ or /../ segments",
        ));
    }
    Ok(())
}

fn validate_consistent(
    paths: &[PathFlag],
    cwd: &Path,
    mode: StrictPathMode,
) -> Result<(), StrictPathViolation> {
    let mut style: Option<SeparatorStyle> = None;
    let mut seen: HashMap<String, &PathFlag> = HashMap::new();

    for path_flag in paths {
        match separator_style(&path_flag.path) {
            SeparatorStyle::Mixed => {
                return Err(violation(
                    path_flag,
                    mode,
                    "mixed forward-slash and backslash separators",
                ));
            }
            SeparatorStyle::Forward | SeparatorStyle::Backslash => {
                let current = separator_style(&path_flag.path);
                if let Some(expected) = style {
                    if current != expected {
                        return Err(violation(
                            path_flag,
                            mode,
                            "include path separator style differs from earlier path flags",
                        ));
                    }
                } else {
                    style = Some(current);
                }
            }
            SeparatorStyle::None => {}
        }

        let key = canonical_key(&path_flag.path, cwd);
        if let Some(previous) = seen.get(&key) {
            if previous.path != path_flag.path {
                return Err(StrictPathViolation {
                    flag: path_flag.flag.clone(),
                    path: path_flag.path.clone(),
                    mode,
                    reason: format!(
                        "same canonical path was already passed as `{}` via {}",
                        previous.path, previous.flag
                    ),
                });
            }
        } else {
            seen.insert(key, path_flag);
        }
    }

    Ok(())
}

fn violation(path_flag: &PathFlag, mode: StrictPathMode, reason: &str) -> StrictPathViolation {
    StrictPathViolation {
        flag: path_flag.flag.clone(),
        path: path_flag.path.clone(),
        mode,
        reason: reason.to_string(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SeparatorStyle {
    None,
    Forward,
    Backslash,
    Mixed,
}

fn separator_style(path: &str) -> SeparatorStyle {
    match (path.contains('/'), path.contains('\\')) {
        (false, false) => SeparatorStyle::None,
        (true, false) => SeparatorStyle::Forward,
        (false, true) => SeparatorStyle::Backslash,
        (true, true) => SeparatorStyle::Mixed,
    }
}

fn is_absolute_like(path: &str) -> bool {
    let bytes = path.as_bytes();
    path.starts_with('/')
        || path.starts_with("//")
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && (bytes[2] == b'/' || bytes[2] == b'\\'))
}

fn has_dot_segment(path: &str) -> bool {
    path.split(['/', '\\'])
        .any(|segment| segment == "." || segment == "..")
}

fn canonical_key(path: &str, cwd: &Path) -> String {
    let mut value = path.replace('\\', "/");
    let windows_like = has_drive_prefix(&value) || path.contains('\\');
    if !is_absolute_like(&value) {
        let cwd = cwd.to_string_lossy().replace('\\', "/");
        value = format!("{cwd}/{value}");
    }

    let mut prefix = String::new();
    let mut rest = value.as_str();
    if has_drive_prefix(rest) {
        prefix = rest[..2].to_ascii_lowercase();
        rest = &rest[2..];
    } else if rest.starts_with("//") {
        prefix = "//".to_string();
        rest = rest.trim_start_matches('/');
    } else if rest.starts_with('/') {
        prefix = "/".to_string();
        rest = rest.trim_start_matches('/');
    }

    let mut segments: Vec<&str> = Vec::new();
    for segment in rest.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            _ => segments.push(segment),
        }
    }

    let mut key = if prefix.is_empty() {
        segments.join("/")
    } else if prefix == "/" || prefix == "//" {
        format!("{prefix}{}", segments.join("/"))
    } else {
        format!("{prefix}/{}", segments.join("/"))
    };
    if windows_like {
        key.make_ascii_lowercase();
    }
    key
}

fn has_drive_prefix(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn absolute_rejects_relative_include() {
        let err = validate_strict_paths(
            &args(&["-c", "main.cpp", "-Iinclude"]),
            Path::new("/work"),
            StrictPathMode::Absolute,
        )
        .unwrap_err();
        assert_eq!(err.flag, "-I");
        assert_eq!(err.path, "include");
        assert!(err.reason.contains("absolute"));
    }

    #[test]
    fn absolute_rejects_dot_segments() {
        let err = validate_strict_paths(
            &args(&["-IC:/Users/me/project/./src", "main.cpp"]),
            Path::new("C:/Users/me/project"),
            StrictPathMode::Absolute,
        )
        .unwrap_err();
        assert_eq!(err.path, "C:/Users/me/project/./src");
        assert!(err.reason.contains("normalized"));
    }

    #[test]
    fn absolute_rejects_backslashes() {
        let err = validate_strict_paths(
            &args(&["-I", r"C:\Users\me\project\src", "main.cpp"]),
            Path::new("C:/Users/me/project"),
            StrictPathMode::Absolute,
        )
        .unwrap_err();
        assert_eq!(err.flag, "-I");
        assert!(err.reason.contains("forward-slash"));
    }

    #[test]
    fn consistent_rejects_mixed_separators_in_one_path() {
        let err = validate_strict_paths(
            &args(&["-Ici/meson/native\\fastled.dll.p", "main.cpp"]),
            Path::new("C:/Users/me/project"),
            StrictPathMode::Consistent,
        )
        .unwrap_err();
        assert_eq!(err.flag, "-I");
        assert!(err.reason.contains("mixed"));
    }

    #[test]
    fn consistent_rejects_same_path_with_different_spellings() {
        let err = validate_strict_paths(
            &args(&[
                "-IC:/Users/me/fastled6/./src",
                "-I",
                "C:/Users/me/fastled6/src",
                "main.cpp",
            ]),
            Path::new("C:/Users/me/fastled6"),
            StrictPathMode::Consistent,
        )
        .unwrap_err();
        assert_eq!(err.flag, "-I");
        assert_eq!(err.path, "C:/Users/me/fastled6/src");
        assert!(err.reason.contains("same canonical path"));
    }

    #[test]
    fn consistent_accepts_repeated_identical_path() {
        validate_strict_paths(
            &args(&["-I", "src", "-I", "src", "main.cpp"]),
            Path::new("/work"),
            StrictPathMode::Consistent,
        )
        .unwrap();
    }

    #[test]
    fn strict_paths_catches_paths_inside_response_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("flags.rsp"),
            "-IC:/Users/me/fastled6/./src -IC:/Users/me/fastled6/src",
        )
        .unwrap();
        let expanded =
            crate::response_file::expand_response_files_in(&args(&["@flags.rsp"]), dir.path())
                .unwrap();

        let err =
            validate_strict_paths(&expanded, dir.path(), StrictPathMode::Consistent).unwrap_err();

        assert_eq!(err.flag, "-I");
        assert_eq!(err.path, "C:/Users/me/fastled6/src");
        assert!(err.reason.contains("same canonical path"));
    }

    #[test]
    fn off_ignores_violations() {
        validate_strict_paths(
            &args(&["-Ici/meson/native\\fastled.dll.p", "main.cpp"]),
            Path::new("/work"),
            StrictPathMode::Off,
        )
        .unwrap();
    }
}
