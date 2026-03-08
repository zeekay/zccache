use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Describes a cacheable compilation invocation.
pub struct Invocation {
    /// The single source file to compile (unused in core path but available for callers).
    #[allow(dead_code)]
    pub input_file: PathBuf,
    /// Where the compiled object should land.
    pub output_file: PathBuf,
    /// Path where the dependency (.d) file should be written, if requested.
    pub dep_file: Option<PathBuf>,
    /// Compiler arguments that contribute to the cache key.
    /// Excludes: -o, -MF/-MT/-MQ values, -MD/-MMD/-MP/-MG flags.
    pub hash_args: Vec<String>,
}

/// Source file extensions supported for caching.
fn is_source_ext(ext: &str) -> bool {
    matches!(ext, "c" | "cc" | "cpp" | "cxx" | "c++" | "m" | "mm")
}

fn is_source_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(is_source_ext)
        .unwrap_or(false)
}

/// Parse compiler arguments and return an `Invocation` if this is a cacheable
/// compile-only command (`-c` flag present, exactly one source file).
///
/// Returns `None` for link steps, multi-source compiles, or anything we cannot
/// safely cache.
pub fn parse_args(compiler_args: &[String]) -> Option<Invocation> {
    let mut has_compile_flag = false;
    let mut input_files: Vec<PathBuf> = Vec::new();
    let mut output_file: Option<PathBuf> = None;
    let mut dep_file: Option<PathBuf> = None;
    let mut hash_args: Vec<String> = Vec::new();

    let mut i = 0;
    while i < compiler_args.len() {
        let arg = compiler_args[i].as_str();

        match arg {
            "-c" => {
                has_compile_flag = true;
                hash_args.push(arg.to_string());
            }
            "-o" => {
                // -o <file>: skip both, not part of cache key
                i += 1;
                if i < compiler_args.len() {
                    output_file = Some(PathBuf::from(&compiler_args[i]));
                }
            }
            "-MF" => {
                // -MF <depfile>: not part of cache key
                i += 1;
                if i < compiler_args.len() {
                    dep_file = Some(PathBuf::from(&compiler_args[i]));
                }
            }
            "-MT" | "-MQ" => {
                // Dependency target overrides: not part of cache key
                i += 1;
            }
            // Dependency-generation flags that don't affect the object output
            "-MD" | "-MMD" | "-MP" | "-MG" => {}
            _ => {
                if arg.starts_with("-o") && arg.len() > 2 {
                    // -o<file> (no space)
                    output_file = Some(PathBuf::from(&arg[2..]));
                } else if !arg.starts_with('-') {
                    let path = PathBuf::from(arg);
                    if is_source_file(&path) {
                        input_files.push(path);
                    }
                    // Non-flag, non-source args (e.g. object files passed to compile
                    // step) still contribute to the hash for safety.
                    hash_args.push(arg.to_string());
                } else {
                    hash_args.push(arg.to_string());
                }
            }
        }
        i += 1;
    }

    // We only cache compile-only steps with a single source file.
    if !has_compile_flag || input_files.len() != 1 {
        return None;
    }

    let input_file = input_files.remove(0);
    let output_file = output_file.unwrap_or_else(|| input_file.with_extension("o"));

    Some(Invocation {
        input_file,
        output_file,
        dep_file,
        hash_args,
    })
}

/// Run the preprocessor on `invocation.input_file` and return the preprocessed bytes.
///
/// We strip `-c` and replace it with `-E`, drop `-o`/`-MF`/`-MT`/`-MQ`, and
/// route output to stdout.  The result is the expanded translation unit that
/// uniquely identifies what the compiler will receive.
pub fn preprocess(compiler: &str, compiler_args: &[String]) -> Result<Vec<u8>> {
    let mut cmd = Command::new(compiler);
    cmd.arg("-E"); // preprocess only

    let mut i = 0;
    while i < compiler_args.len() {
        let arg = compiler_args[i].as_str();
        match arg {
            "-c" => {} // drop compile flag – we already added -E
            "-o" => {
                i += 1; // skip -o <file>
            }
            "-MF" | "-MT" | "-MQ" => {
                i += 1; // skip flag + its argument
            }
            "-MD" | "-MMD" | "-MP" | "-MG" => {} // drop dep-generation flags
            _ => {
                if arg.starts_with("-o") && arg.len() > 2 {
                    // drop -o<file> form
                } else {
                    cmd.arg(arg);
                }
            }
        }
        i += 1;
    }

    // Route preprocessed output to stdout, suppress -fno-diagnostics-color noise
    cmd.arg("-o").arg("-");
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::null()); // suppress include warnings; errors caught via exit status

    let output = cmd.output().context("Failed to spawn preprocessor")?;

    if !output.status.success() {
        bail!(
            "Preprocessor exited with status {}",
            output.status.code().unwrap_or(-1)
        );
    }

    Ok(output.stdout)
}

/// Invoke the real compiler (original args, no modification) and return its exit code.
pub fn run_direct(compiler: &str, compiler_args: &[String]) -> i32 {
    match Command::new(compiler).args(compiler_args).status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            eprintln!("zccache: failed to execute '{}': {}", compiler, e);
            1
        }
    }
}

/// Run the compiler with the original args and, on success, cache the outputs.
pub fn compile_and_cache(
    compiler: &str,
    compiler_args: &[String],
    invocation: &Invocation,
    cache_dir: &Path,
    cache_key: &str,
) -> Result<i32> {
    let exit_code = run_direct(compiler, compiler_args);

    if exit_code == 0 {
        crate::cache::store(
            cache_dir,
            cache_key,
            &invocation.output_file,
            invocation.dep_file.as_deref(),
        )
        .context("Failed to store compilation result in cache")?;
    }

    Ok(exit_code)
}
