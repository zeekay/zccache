//! Helpers for daemon-owned child processes.

use std::io;
use std::process::Output;

#[cfg(windows)]
use std::sync::OnceLock;

#[cfg(windows)]
use std::os::windows::io::AsRawHandle;

pub(crate) const COMPILE_PRIORITY_ENV: &str = "ZCCACHE_COMPILE_PRIORITY";

/// Priority policy for compiler/linker child processes owned by the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompilePriority {
    Normal,
    Low,
    Idle,
    High,
}

impl Default for CompilePriority {
    fn default() -> Self {
        Self::Low
    }
}

impl CompilePriority {
    pub(crate) fn parse(value: &str) -> Result<Self, CompilePriorityParseError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "normal" => Ok(Self::Normal),
            "low" => Ok(Self::Low),
            "idle" => Ok(Self::Idle),
            "high" => Ok(Self::High),
            other => Err(CompilePriorityParseError {
                value: other.to_string(),
            }),
        }
    }

    pub(crate) fn from_client_env(env: Option<&[(String, String)]>) -> Self {
        if let Some(value) = env.and_then(|vars| {
            vars.iter()
                .find(|(key, _)| key == COMPILE_PRIORITY_ENV)
                .map(|(_, value)| value.as_str())
        }) {
            return Self::parse_or_warn(value);
        }

        match std::env::var(COMPILE_PRIORITY_ENV) {
            Ok(value) => Self::parse_or_warn(&value),
            Err(_) => Self::Low,
        }
    }

    fn parse_or_warn(value: &str) -> Self {
        match Self::parse(value) {
            Ok(priority) => priority,
            Err(e) => {
                tracing::warn!(
                    env = COMPILE_PRIORITY_ENV,
                    value = %e.value,
                    "invalid compiler child priority; using low"
                );
                Self::Low
            }
        }
    }

    #[cfg(test)]
    fn parse_optional(value: Option<&str>) -> Result<Self, CompilePriorityParseError> {
        match value {
            Some(value) => Self::parse(value),
            None => Ok(Self::Low),
        }
    }

    #[cfg(unix)]
    fn unix_nice_value(self) -> Option<i32> {
        match self {
            Self::Normal => None,
            Self::Low => Some(10),
            Self::Idle => Some(19),
            // Higher priorities commonly require extra privileges; failures are
            // logged and compilation continues at the inherited priority.
            Self::High => Some(-5),
        }
    }

    #[cfg(windows)]
    fn windows_priority_class(self) -> Option<u32> {
        use windows_sys::Win32::System::Threading::{
            BELOW_NORMAL_PRIORITY_CLASS, HIGH_PRIORITY_CLASS, IDLE_PRIORITY_CLASS,
        };

        match self {
            Self::Normal => None,
            Self::Low => Some(BELOW_NORMAL_PRIORITY_CLASS),
            Self::Idle => Some(IDLE_PRIORITY_CLASS),
            Self::High => Some(HIGH_PRIORITY_CLASS),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompilePriorityParseError {
    value: String,
}

/// Wait for a synchronous command after applying a compiler child priority.
pub(crate) fn command_output_with_priority(
    cmd: &mut std::process::Command,
    priority: CompilePriority,
) -> io::Result<Output> {
    #[cfg(windows)]
    {
        use std::process::Stdio;

        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let child = cmd.spawn()?;
        assign_child_to_daemon_job(child.as_raw_handle());
        apply_priority_to_child_windows(child.as_raw_handle(), priority);
        child.wait_with_output()
    }

    #[cfg(unix)]
    {
        use std::process::Stdio;

        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let child = cmd.spawn()?;
        apply_priority_to_child_unix(child.id(), priority);
        child.wait_with_output()
    }

    #[cfg(not(any(unix, windows)))]
    {
        if priority != CompilePriority::Normal {
            tracing::debug!(
                ?priority,
                "compiler child priority is unsupported on this platform"
            );
        }
        cmd.output()
    }
}

/// Wait for an async command after applying a compiler child priority.
pub(crate) async fn tokio_command_output_with_priority(
    cmd: &mut tokio::process::Command,
    priority: CompilePriority,
) -> io::Result<Output> {
    #[cfg(windows)]
    {
        use std::process::Stdio;

        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let child = cmd.spawn()?;
        if let Some(handle) = child.raw_handle() {
            assign_child_to_daemon_job(handle);
            apply_priority_to_child_windows(handle, priority);
        }
        child.wait_with_output().await
    }

    #[cfg(unix)]
    {
        use std::process::Stdio;

        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let child = cmd.spawn()?;
        if let Some(pid) = child.id() {
            apply_priority_to_child_unix(pid, priority);
        }
        child.wait_with_output().await
    }

    #[cfg(not(any(unix, windows)))]
    {
        cmd.output().await
    }
}

#[cfg(windows)]
fn assign_child_to_daemon_job(raw_handle: std::os::windows::io::RawHandle) {
    let Some(job) = DAEMON_JOB.get_or_init(WindowsJob::new).as_ref() else {
        return;
    };

    if let Err(e) = job.assign(raw_handle) {
        tracing::debug!("failed to assign child process to daemon job: {e}");
    }
}

#[cfg(unix)]
fn apply_priority_to_child_unix(pid: u32, priority: CompilePriority) {
    let Some(nice) = priority.unix_nice_value() else {
        return;
    };

    let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, pid as libc::id_t, nice) };
    if rc != 0 {
        tracing::debug!(
            ?priority,
            pid,
            nice,
            error = %io::Error::last_os_error(),
            "failed to set compiler child priority"
        );
    }
}

#[cfg(windows)]
fn apply_priority_to_child_windows(
    raw_handle: std::os::windows::io::RawHandle,
    priority: CompilePriority,
) {
    let Some(priority_class) = priority.windows_priority_class() else {
        return;
    };

    use windows_sys::Win32::System::Threading::SetPriorityClass;

    let ok = unsafe { SetPriorityClass(raw_handle.cast::<std::ffi::c_void>(), priority_class) };
    if ok == 0 {
        tracing::debug!(
            ?priority,
            error = %io::Error::last_os_error(),
            "failed to set compiler child priority"
        );
    }
}

#[cfg(windows)]
static DAEMON_JOB: OnceLock<Option<WindowsJob>> = OnceLock::new();

#[cfg(windows)]
struct WindowsJob {
    handle: usize,
}

#[cfg(windows)]
impl WindowsJob {
    fn new() -> Option<Self> {
        use std::mem::size_of;
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };

        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            tracing::debug!(
                "failed to create daemon job object: {}",
                io::Error::last_os_error()
            );
            return None;
        }

        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

        let ok = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            tracing::debug!(
                "failed to configure daemon job object: {}",
                io::Error::last_os_error()
            );
            unsafe {
                CloseHandle(handle);
            }
            return None;
        }

        tracing::debug!("created daemon child-process job object");
        Some(Self {
            handle: handle as usize,
        })
    }

    fn assign(&self, raw_handle: std::os::windows::io::RawHandle) -> io::Result<()> {
        use windows_sys::Win32::Foundation::HANDLE;
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

        let ok = unsafe {
            AssignProcessToJobObject(self.handle as HANDLE, raw_handle.cast::<std::ffi::c_void>())
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for WindowsJob {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        unsafe {
            CloseHandle(self.handle as HANDLE);
        }
    }
}

#[cfg(windows)]
unsafe impl Send for WindowsJob {}

#[cfg(windows)]
unsafe impl Sync for WindowsJob {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_compile_priority_values() {
        assert_eq!(
            CompilePriority::parse("normal").unwrap(),
            CompilePriority::Normal
        );
        assert_eq!(CompilePriority::parse("LOW").unwrap(), CompilePriority::Low);
        assert_eq!(
            CompilePriority::parse(" idle ").unwrap(),
            CompilePriority::Idle
        );
        assert_eq!(
            CompilePriority::parse("high").unwrap(),
            CompilePriority::High
        );
        assert!(CompilePriority::parse("fast").is_err());
    }

    #[test]
    fn absent_compile_priority_defaults_to_low() {
        assert_eq!(
            CompilePriority::parse_optional(None).unwrap(),
            CompilePriority::Low
        );
    }

    #[test]
    fn client_env_selects_high_mode() {
        let env = vec![(COMPILE_PRIORITY_ENV.to_string(), "high".to_string())];
        assert_eq!(
            CompilePriority::from_client_env(Some(&env)),
            CompilePriority::High
        );
    }

    #[test]
    fn client_env_invalid_value_falls_back_to_low() {
        let env = vec![(COMPILE_PRIORITY_ENV.to_string(), "fast".to_string())];
        assert_eq!(
            CompilePriority::from_client_env(Some(&env)),
            CompilePriority::Low
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_priority_mapping_is_explicit() {
        assert_eq!(CompilePriority::Normal.unix_nice_value(), None);
        assert_eq!(CompilePriority::Low.unix_nice_value(), Some(10));
        assert_eq!(CompilePriority::Idle.unix_nice_value(), Some(19));
        assert_eq!(CompilePriority::High.unix_nice_value(), Some(-5));
    }

    #[cfg(windows)]
    #[test]
    fn windows_priority_mapping_is_explicit() {
        use windows_sys::Win32::System::Threading::{
            BELOW_NORMAL_PRIORITY_CLASS, HIGH_PRIORITY_CLASS, IDLE_PRIORITY_CLASS,
        };

        assert_eq!(CompilePriority::Normal.windows_priority_class(), None);
        assert_eq!(
            CompilePriority::Low.windows_priority_class(),
            Some(BELOW_NORMAL_PRIORITY_CLASS)
        );
        assert_eq!(
            CompilePriority::Idle.windows_priority_class(),
            Some(IDLE_PRIORITY_CLASS)
        );
        assert_eq!(
            CompilePriority::High.windows_priority_class(),
            Some(HIGH_PRIORITY_CLASS)
        );
    }
}
