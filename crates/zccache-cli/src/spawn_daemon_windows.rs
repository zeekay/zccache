//! Windows-only sanitized daemon spawn.
//!
//! `std::process::Command::spawn` ultimately calls `CreateProcessW` with
//! `bInheritHandles = TRUE`. The kernel then duplicates *every* handle in
//! the parent's table whose `HANDLE_FLAG_INHERIT` is set into the child —
//! including orphaned handles that no longer have a `STD_*_HANDLE` slot
//! pointing at them. In the zccache pipeline
//!
//! ```text
//! Python (Popen stdout=PIPE)
//!   └─ soldr (Python's pipe write end inheritable in soldr's table)
//!       └─ cargo
//!           └─ rustc
//!               └─ zccache-cli           <-- we are here
//!                   └─ zccache-daemon    <-- this spawn must NOT inherit the pipe
//! ```
//!
//! the Python pipe-write-end is still alive and still inheritable in
//! zccache-cli's table, even though zccache-cli's own `STD_OUTPUT_HANDLE`
//! points at a different (soldr-internal) handle. A `SetHandleInformation`
//! call against `std::io::stdout()` cannot reach the orphan, so the
//! daemon would inherit it, hold it open, and the Python parent's
//! `proc.wait()` would block until the daemon exits or is killed.
//!
//! The canonical Microsoft-blessed fix is `STARTUPINFOEX` with
//! `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`. That tells `CreateProcessW` to
//! ignore the "duplicate all inheritable handles" behavior and only
//! duplicate the explicitly-listed handles. We list three fresh
//! inheritable NUL handles for stdin/stdout/stderr; the Python pipe is
//! not on the list, so it never crosses into the daemon.
//!
//! See issue #289 for the full root-cause analysis.

#![cfg(windows)]

use std::ffi::OsStr;
use std::io;
use std::mem::{size_of, size_of_val, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, InitializeProcThreadAttributeList,
    UpdateProcThreadAttribute, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW,
    CREATE_UNICODE_ENVIRONMENT, DETACHED_PROCESS, EXTENDED_STARTUPINFO_PRESENT,
    PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, STARTF_USESTDHANDLES, STARTUPINFOEXW,
};

/// Spawn a child process with a sanitized handle table.
///
/// Equivalent to `std::process::Command::new(bin).args(args).envs(env_overrides).spawn()`
/// **except** the child only inherits the three NUL handles we wire to its
/// stdio, never any orphaned inheritable handles from the parent's table.
///
/// On success, the returned `Ok(())` indicates the child was launched;
/// the child handle is closed (we don't reap or wait on it — the daemon
/// is expected to run independently and we already have a separate
/// readiness check via `connect_client`).
pub fn spawn_daemon_sanitized(
    bin: &Path,
    args: &[&str],
    env_overrides: &[(String, String)],
) -> Result<(), String> {
    // SAFETY: every raw Win32 call below is paired with cleanup of any
    // handles or attribute lists it produced. Strings are kept alive in
    // local Vecs for the duration of the call.
    unsafe {
        // 1) Open three inheritable NUL handles for the child's stdio.
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: null_mut(),
            bInheritHandle: 1,
        };
        let nul: Vec<u16> = OsStr::new("NUL").encode_wide().chain(Some(0)).collect();
        let open = |access| {
            CreateFileW(
                nul.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                &sa as *const _ as _,
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        let h_in = open(GENERIC_READ);
        let h_out = open(GENERIC_WRITE);
        let h_err = open(GENERIC_WRITE);
        let close_nuls = || {
            for &h in &[h_in, h_out, h_err] {
                if !h.is_null() && h != INVALID_HANDLE_VALUE {
                    CloseHandle(h);
                }
            }
        };
        if h_in.is_null()
            || h_in == INVALID_HANDLE_VALUE
            || h_out.is_null()
            || h_out == INVALID_HANDLE_VALUE
            || h_err.is_null()
            || h_err == INVALID_HANDLE_VALUE
        {
            let err = io::Error::last_os_error();
            close_nuls();
            return Err(format!("CreateFileW(NUL) failed: {err}"));
        }

        // 2) Allocate and initialize the attribute list with one slot for
        //    PROC_THREAD_ATTRIBUTE_HANDLE_LIST.
        let mut attr_size: usize = 0;
        let _ = InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut attr_size);
        // First call always returns 0 with ERROR_INSUFFICIENT_BUFFER; the
        // size is written to attr_size. Ignore the return value.
        let mut attr_buf: Vec<u8> = vec![0; attr_size];
        let attr_list = attr_buf.as_mut_ptr() as _;
        if InitializeProcThreadAttributeList(attr_list, 1, 0, &mut attr_size) == 0 {
            let err = io::Error::last_os_error();
            close_nuls();
            return Err(format!("InitializeProcThreadAttributeList failed: {err}"));
        }

        let handles = [h_in, h_out, h_err];
        if UpdateProcThreadAttribute(
            attr_list,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            handles.as_ptr() as _,
            size_of_val(&handles),
            null_mut(),
            null_mut(),
        ) == 0
        {
            let err = io::Error::last_os_error();
            DeleteProcThreadAttributeList(attr_list);
            close_nuls();
            return Err(format!("UpdateProcThreadAttribute failed: {err}"));
        }

        // 3) STARTUPINFOEXW pointing at the attribute list and the three
        //    NUL handles.
        let mut si: STARTUPINFOEXW = zeroed();
        si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = h_in;
        si.StartupInfo.hStdOutput = h_out;
        si.StartupInfo.hStdError = h_err;
        si.lpAttributeList = attr_list;

        // 4) Build the command line, quoting where necessary.
        let mut cmd_line_w = build_command_line(bin, args);

        // 5) Build the merged environment block: inherit current env,
        //    override the keys the caller specified.
        let env_block = build_env_block(env_overrides);

        // 6) CreateProcessW. bInheritHandles must be TRUE for the listed
        //    handles to be duplicated; the attribute list restricts what
        //    *else* the child sees (i.e., nothing).
        //
        // Flag rationale:
        //   EXTENDED_STARTUPINFO_PRESENT - we're passing STARTUPINFOEXW
        //   DETACHED_PROCESS             - child has no console at all; survives parent exit
        //   CREATE_NO_WINDOW             - no transient console window flash during launch
        //   CREATE_NEW_PROCESS_GROUP     - isolates the daemon from CTRL_C_EVENT / CTRL_BREAK_EVENT
        //                                  sent to the spawning console (defense in depth on top
        //                                  of DETACHED_PROCESS, in case anything later attaches
        //                                  a console via AttachConsole)
        //   CREATE_UNICODE_ENVIRONMENT   - our env_block is UTF-16
        //
        // bInheritHandles=TRUE is required for the *listed* handles to cross;
        // the attribute list restricts what else does (i.e., nothing).
        let mut pi: PROCESS_INFORMATION = zeroed();
        let ok = CreateProcessW(
            null(),
            cmd_line_w.as_mut_ptr(),
            null_mut(),
            null_mut(),
            1, // bInheritHandles = TRUE
            EXTENDED_STARTUPINFO_PRESENT
                | DETACHED_PROCESS
                | CREATE_NO_WINDOW
                | CREATE_NEW_PROCESS_GROUP
                | CREATE_UNICODE_ENVIRONMENT,
            env_block.as_ptr() as _,
            null(),
            &si.StartupInfo,
            &mut pi,
        );

        // 7) Cleanup. Close our copies of all handles; the child has its
        //    own duplicated handles.
        DeleteProcThreadAttributeList(attr_list);
        close_nuls();

        if ok != 0 {
            CloseHandle(pi.hProcess);
            CloseHandle(pi.hThread);
            Ok(())
        } else {
            Err(format!(
                "CreateProcessW failed: {}",
                io::Error::last_os_error()
            ))
        }
    }
}

/// Build a UTF-16 NUL-terminated command line in the CommandLineToArgvW
/// quoting rules. Arguments containing whitespace, double quotes, or
/// backslashes are quoted and escaped.
fn build_command_line(bin: &Path, args: &[&str]) -> Vec<u16> {
    let mut out = String::new();
    push_quoted(&mut out, &bin.to_string_lossy());
    for a in args {
        out.push(' ');
        push_quoted(&mut out, a);
    }
    OsStr::new(&out).encode_wide().chain(Some(0)).collect()
}

fn push_quoted(out: &mut String, s: &str) {
    let needs_quotes = s.is_empty() || s.chars().any(|c| c == ' ' || c == '\t' || c == '"');
    if !needs_quotes {
        out.push_str(s);
        return;
    }
    out.push('"');
    let mut backslashes = 0usize;
    for c in s.chars() {
        match c {
            '\\' => {
                backslashes += 1;
            }
            '"' => {
                // Escape preceding backslashes AND the quote.
                for _ in 0..(backslashes * 2 + 1) {
                    out.push('\\');
                }
                out.push('"');
                backslashes = 0;
            }
            _ => {
                for _ in 0..backslashes {
                    out.push('\\');
                }
                backslashes = 0;
                out.push(c);
            }
        }
    }
    // Trailing backslashes before closing quote must be doubled.
    for _ in 0..(backslashes * 2) {
        out.push('\\');
    }
    out.push('"');
}

/// Build a Windows environment block as a UTF-16 string of
/// `KEY=VALUE\0KEY=VALUE\0...\0` (double-NUL-terminated).
///
/// Inherits the current process environment and applies the supplied
/// overrides (insert or replace, case-insensitive lookup). Windows
/// requires env-block entries sorted alphabetically by key
/// (case-insensitive Unicode); a `BTreeMap` keyed on the uppercased key
/// gives us that ordering.
fn build_env_block(overrides: &[(String, String)]) -> Vec<u16> {
    use std::collections::BTreeMap;
    // Key: uppercase form (for sort + case-insensitive override).
    // Value: (original-cased key, value).
    let mut map: BTreeMap<String, (String, String)> = BTreeMap::new();
    for (k, v) in std::env::vars() {
        map.insert(k.to_uppercase(), (k, v));
    }
    for (k, v) in overrides {
        map.insert(k.to_uppercase(), (k.clone(), v.clone()));
    }
    let mut block: Vec<u16> = Vec::new();
    for (k, v) in map.values() {
        let entry = format!("{k}={v}");
        block.extend(OsStr::new(&entry).encode_wide());
        block.push(0);
    }
    // Final NUL terminator for the block.
    block.push(0);
    block
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::thread;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::System::Pipes::CreatePipe;

    /// Poll for `path` to exist, returning true once it appears or false
    /// after `deadline`. Sleeps 25 ms between probes — fast enough to keep
    /// short-running child tests under ~100 ms in the happy case while
    /// still allowing a generous wall-clock budget for slow CI hosts.
    fn wait_for_file(path: &Path, deadline: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < deadline {
            if path.exists() {
                return true;
            }
            thread::sleep(Duration::from_millis(25));
        }
        path.exists()
    }

    fn cmd_exe() -> std::ffi::OsString {
        std::env::var_os("ComSpec").unwrap_or_else(|| "cmd.exe".into())
    }

    /// Regression test for issue #289: a child spawned via
    /// `spawn_daemon_sanitized` must NOT inherit the parent's orphaned
    /// inheritable pipe write-end, even though the kernel's default
    /// `bInheritHandles = TRUE` behavior would duplicate it.
    ///
    /// Setup mirrors the real failure mode: the parent (this test)
    /// creates an inheritable anonymous pipe, then spawns a long-lived
    /// child process. With a vanilla `CreateProcessW(...,
    /// bInheritHandles=TRUE, ...)` the child would inherit the pipe
    /// write-end; the test would hang on `read()` until the child exits.
    /// With `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` whitelisting only the NUL
    /// stdio handles, the child does not get the write-end, so closing
    /// the parent's copy drives the refcount to zero and the read EOFs
    /// immediately.
    #[test]
    fn sanitized_spawn_does_not_inherit_orphan_pipe() {
        // 1) Create an inheritable anonymous pipe.
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: null_mut(),
            bInheritHandle: 1,
        };
        let mut read_h: HANDLE = std::ptr::null_mut();
        let mut write_h: HANDLE = std::ptr::null_mut();
        let ok = unsafe { CreatePipe(&mut read_h, &mut write_h, &sa, 0) };
        assert!(ok != 0, "CreatePipe failed: {}", io::Error::last_os_error());

        // 2) Spawn a long-running child via the sanitized path. We use
        //    `cmd /C ping -n 6 127.0.0.1 > NUL` because `cmd` ships with
        //    every Windows host and the ping reliably keeps the process
        //    alive for ~5 seconds.
        let res = spawn_daemon_sanitized(
            Path::new(&cmd_exe()),
            &["/C", "ping", "-n", "6", "127.0.0.1", ">", "NUL"],
            &[],
        );
        if let Err(e) = &res {
            unsafe {
                CloseHandle(read_h);
                CloseHandle(write_h);
            }
            panic!("spawn_daemon_sanitized failed: {e}");
        }

        // 3) Close our copy of the write end. If the child inherited the
        //    pipe (the bug), the kernel-side refcount stays > 0 and the
        //    read() below will block until the child exits ~5 s later.
        unsafe {
            CloseHandle(write_h);
        }

        // 4) Read from the read end. We expect EOF (0 bytes) within
        //    1 second. If the bug returns, this takes 5+ seconds.
        let mut file = unsafe {
            <std::fs::File as std::os::windows::io::FromRawHandle>::from_raw_handle(read_h as _)
        };
        let start = Instant::now();
        let mut buf = [0u8; 16];
        let n = file.read(&mut buf).expect("read");
        let elapsed = start.elapsed();
        assert_eq!(n, 0, "expected EOF, got {n} bytes");
        assert!(
            elapsed < Duration::from_secs(2),
            "read took {elapsed:?}, child must have inherited the pipe write-end \
             (regression of #289)"
        );
        // file (and its inner read_h) drops here.
    }

    /// Happy path: spawn cmd.exe with a benign `copy NUL <tempfile>` and
    /// observe the side-effect file. Proves that
    ///   - the function returns `Ok(())` for a real binary,
    ///   - the child's command line is parsed correctly by cmd.exe,
    ///   - a specific positional arg (the tempfile path) reaches the child verbatim,
    ///   - the spawn is detached well enough that the child runs to completion
    ///     without the parent waiting on a handle.
    #[test]
    fn sanitized_spawn_runs_child_and_passes_args() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("ran.flag");
        let marker_str = marker.to_string_lossy().to_string();
        let res = spawn_daemon_sanitized(
            Path::new(&cmd_exe()),
            &["/C", "copy", "NUL", marker_str.as_str()],
            &[],
        );
        assert!(res.is_ok(), "spawn failed: {:?}", res);
        assert!(
            wait_for_file(&marker, Duration::from_secs(5)),
            "child never created marker file {marker_str} - either spawn was \
             silently broken or args were mangled"
        );
    }

    /// Env overrides: the child must see env vars supplied via the
    /// `env_overrides` parameter. We use `cmd /C if defined <KEY> ...` to
    /// observe presence without relying on stdio (which is NUL-piped).
    /// Variant: a key that we did NOT supply must NOT be visible (smoke
    /// check that we are not silently leaking the test process's env in
    /// a way that would mask a real failure).
    #[test]
    fn sanitized_spawn_applies_env_overrides() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("env-was-set.flag");
        let marker_str = marker.to_string_lossy().to_string();

        // Unique key so concurrent / repeated test runs don't collide.
        let key = format!("ZCCACHE_SPAWN_TEST_KEY_{}", std::process::id());

        // `cmd /C if defined KEY copy NUL "<marker>"` writes the file iff
        // the env var is set. cmd's `if defined` is the cleanest "did the
        // child see this env var?" probe that avoids stdout entirely.
        let res = spawn_daemon_sanitized(
            Path::new(&cmd_exe()),
            &[
                "/C",
                "if",
                "defined",
                key.as_str(),
                "copy",
                "NUL",
                marker_str.as_str(),
            ],
            &[(key.clone(), "1".to_string())],
        );
        assert!(res.is_ok(), "spawn failed: {:?}", res);
        assert!(
            wait_for_file(&marker, Duration::from_secs(5)),
            "child did not see env override {key}=1; marker {marker_str} \
             was never created"
        );
    }

    /// Negative control: when `env_overrides` does NOT contain the key,
    /// the child must NOT observe it. Without this control, the previous
    /// test could pass even if `env_overrides` were silently ignored and
    /// the var were leaking from the test process's environment via the
    /// inherited block.
    #[test]
    fn sanitized_spawn_does_not_invent_env_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("env-was-set.flag");
        let marker_str = marker.to_string_lossy().to_string();

        // Use a key we explicitly do not set anywhere. The chance of it
        // already existing in the test process's env is effectively zero,
        // but we still make it process-unique.
        let key = format!("ZCCACHE_SPAWN_TEST_UNSET_{}", std::process::id());

        let res = spawn_daemon_sanitized(
            Path::new(&cmd_exe()),
            &[
                "/C",
                "if",
                "defined",
                key.as_str(),
                "copy",
                "NUL",
                marker_str.as_str(),
            ],
            &[], // No overrides.
        );
        assert!(res.is_ok(), "spawn failed: {:?}", res);

        // Give cmd time to run to completion. 1 s is more than enough on
        // any host that can launch cmd.exe at all.
        thread::sleep(Duration::from_secs(1));
        assert!(
            !marker.exists(),
            "child saw env key {key} we never set - env block construction \
             is leaking unrelated keys into the child"
        );
    }

    /// Error path: an obviously-bogus binary path must surface a clean
    /// `Err(String)` from `CreateProcessW`, not a panic or silent success.
    /// Guards against the helper returning Ok for a no-op spawn.
    #[test]
    fn sanitized_spawn_fails_for_missing_binary() {
        // A path that definitely doesn't exist. The drive letter form
        // avoids any current-directory resolution side-effects.
        let bogus = Path::new("C:\\zccache-this-path-does-not-exist\\nope.exe");
        let res = spawn_daemon_sanitized(bogus, &[], &[]);
        assert!(
            res.is_err(),
            "spawn of missing binary unexpectedly succeeded"
        );
        let msg = res.unwrap_err();
        assert!(
            msg.contains("CreateProcessW failed"),
            "error message should identify the failing call, got: {msg}"
        );
    }
}
