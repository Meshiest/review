//! Ending the program's whole process tree when its session closes.
//!
//! On Windows the child joins a job object that kills everything in it when
//! the last handle closes, so the tree also dies if the app does. Closing the
//! ConPTY asks the shell to go; a hard kill follows after a short grace period.
//! On Unix the child leads its own process group: SIGHUP, then SIGKILL.

use std::sync::Arc;
use std::time::Duration;

use alacritty_terminal::event_loop::Msg;

use crate::listener::Shared;

/// How long a program gets to end after being asked to.
const GRACE: Duration = Duration::from_millis(500);
/// How long to wait for the exit status after a hard kill.
const KILL_WAIT: Duration = Duration::from_secs(2);

#[cfg(unix)]
pub(crate) use unix::Tree;
#[cfg(windows)]
pub(crate) use windows::Tree;

/// Closes the PTY and ends the process tree, on a thread of its own.
pub(crate) fn spawn_reaper(tree: Option<Tree>, shared: Arc<Shared>) {
    let reaper = std::thread::Builder::new()
        .name("term-close".to_owned())
        .spawn({
            let shared = Arc::clone(&shared);
            move || reap(tree, &shared)
        });
    if reaper.is_err() {
        // No thread to wait on: still close the PTY.
        send_shutdown(&shared);
    }
}

fn send_shutdown(shared: &Shared) {
    if let Some(sender) = shared.sender() {
        let _ = sender.send(Msg::Shutdown);
    }
}

#[cfg(windows)]
fn reap(tree: Option<Tree>, shared: &Shared) {
    // Closing the ConPTY makes the console host tell its clients to quit.
    send_shutdown(shared);
    let Some(tree) = tree else {
        shared.finish_and_wake(None);
        return;
    };
    let status = tree.wait(GRACE).or_else(|| {
        tree.kill();
        tree.wait(KILL_WAIT)
    });
    shared.finish_and_wake(status);
}

#[cfg(unix)]
fn reap(tree: Option<Tree>, shared: &Shared) {
    use crate::ExitState;

    let exited = |limit: Duration| {
        let step = Duration::from_millis(20);
        let mut waited = Duration::ZERO;
        while shared.exit_state() == ExitState::Running && waited < limit {
            std::thread::sleep(step);
            waited += step;
        }
        shared.exit_state() != ExitState::Running
    };
    if let Some(tree) = &tree {
        // The event loop stays up meanwhile, so it reports the exit status.
        tree.signal("HUP");
        if !exited(GRACE) {
            tree.signal("KILL");
            exited(KILL_WAIT);
        }
    }
    send_shutdown(shared);
    shared.finish_and_wake(None);
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows {
    //! The only unsafe code in the crate: a process handle and a job object.

    use std::ffi::c_void;
    use std::mem::size_of;
    use std::os::windows::process::ExitStatusExt;
    use std::process::{Command, ExitStatus};
    use std::ptr;
    use std::time::Duration;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA,
        PROCESS_TERMINATE, WaitForSingleObject,
    };

    /// The access right to wait on a process handle.
    const SYNCHRONIZE: u32 = 0x0010_0000;
    const WAIT_OBJECT_0: u32 = 0;
    const KILLED_EXIT_CODE: u32 = 1;

    /// A handle on the child and the job that holds it and its descendants.
    /// Holding the process handle keeps its id from being reused.
    pub(crate) struct Tree {
        process: HANDLE,
        /// Null when the process could not be put in a job.
        job: HANDLE,
        pid: u32,
    }

    // SAFETY: a kernel handle is usable from any thread.
    unsafe impl Send for Tree {}

    impl Tree {
        /// Opens `pid` and puts it in a new kill-on-close job. `None` when the
        /// process cannot be opened (it is gone already).
        pub(crate) fn adopt(pid: u32) -> Option<Self> {
            // SAFETY: plain calls with valid arguments; each handle is checked
            // and either closed on a failure path or owned by the result.
            unsafe {
                let process = OpenProcess(
                    PROCESS_SET_QUOTA
                        | PROCESS_TERMINATE
                        | PROCESS_QUERY_LIMITED_INFORMATION
                        | SYNCHRONIZE,
                    0,
                    pid,
                );
                if process.is_null() {
                    return None;
                }
                let mut tree = Self {
                    process,
                    job: ptr::null_mut(),
                    pid,
                };
                let job = CreateJobObjectW(ptr::null(), ptr::null());
                if job.is_null() {
                    return Some(tree);
                }
                let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let configured = SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    ptr::from_ref(&limits).cast::<c_void>(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ) != 0;
                if !configured || AssignProcessToJobObject(job, process) == 0 {
                    // `kill` falls back to `taskkill` without a job.
                    CloseHandle(job);
                    return Some(tree);
                }
                tree.job = job;
                Some(tree)
            }
        }

        /// Waits for the child to exit; its status, or `None` on timeout.
        pub(crate) fn wait(&self, limit: Duration) -> Option<ExitStatus> {
            let millis = u32::try_from(limit.as_millis()).unwrap_or(u32::MAX - 1);
            // SAFETY: `process` is a valid handle owned by `self`.
            unsafe {
                if WaitForSingleObject(self.process, millis) != WAIT_OBJECT_0 {
                    return None;
                }
                let mut code = 0_u32;
                (GetExitCodeProcess(self.process, &mut code) != 0)
                    .then(|| ExitStatus::from_raw(code))
            }
        }

        /// Ends the child and everything it started.
        pub(crate) fn kill(&self) {
            if self.job.is_null() {
                let _ = Command::new("taskkill")
                    .args(["/F", "/T", "/PID", &self.pid.to_string()])
                    .output();
                return;
            }
            // SAFETY: `job` is a valid handle owned by `self`.
            unsafe {
                TerminateJobObject(self.job, KILLED_EXIT_CODE);
            }
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            // SAFETY: both handles are owned by `self` and closed once. Closing
            // the job ends any process still in it.
            unsafe {
                if !self.job.is_null() {
                    CloseHandle(self.job);
                }
                CloseHandle(self.process);
            }
        }
    }
}

#[cfg(unix)]
mod unix {
    use std::process::Command;

    /// The child, which leads its own process group.
    pub(crate) struct Tree {
        pgid: u32,
    }

    impl Tree {
        pub(crate) fn adopt(pid: u32) -> Option<Self> {
            Some(Self { pgid: pid })
        }

        /// Sends `signal` (a name such as `HUP`) to the whole group.
        pub(crate) fn signal(&self, signal: &str) {
            let _ = Command::new("kill")
                .args(["-s", signal, "--", &format!("-{}", self.pgid)])
                .output();
        }
    }
}
