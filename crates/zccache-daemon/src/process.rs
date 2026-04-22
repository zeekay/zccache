//! Helpers for daemon-owned child processes.

use std::io;
use std::process::Output;

#[cfg(windows)]
use std::sync::OnceLock;

#[cfg(windows)]
use std::os::windows::io::AsRawHandle;

/// Wait for a synchronous command while registering it with the daemon's
/// platform process group/container when supported.
pub(crate) fn command_output(cmd: &mut std::process::Command) -> io::Result<Output> {
    #[cfg(windows)]
    {
        use std::process::Stdio;

        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let child = cmd.spawn()?;
        assign_child_to_daemon_job(child.as_raw_handle());
        child.wait_with_output()
    }

    #[cfg(not(windows))]
    {
        cmd.output()
    }
}

/// Wait for an async command while registering it with the daemon's platform
/// process group/container when supported.
pub(crate) async fn tokio_command_output(cmd: &mut tokio::process::Command) -> io::Result<Output> {
    #[cfg(windows)]
    {
        use std::process::Stdio;

        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let child = cmd.spawn()?;
        if let Some(handle) = child.raw_handle() {
            assign_child_to_daemon_job(handle);
        }
        child.wait_with_output().await
    }

    #[cfg(not(windows))]
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
