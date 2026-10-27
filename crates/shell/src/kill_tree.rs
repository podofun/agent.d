//! Kill a running command together with everything it started.
//!
//! Each backend puts the command in a container the whole tree inherits: a
//! process group on Unix, a Job Object on Windows. A [`KillSwitch`] holds that
//! container while the exec is in flight, and a [`KillOnDrop`] guard fires it
//! when the exec future is dropped, which is also how a timeout stops it.

use std::sync::{Arc, Mutex};

/// What to kill. On Unix, the process group id (the group leader's pid).
#[cfg(unix)]
pub(crate) type Target = i32;
#[cfg(windows)]
pub(crate) type Target = Arc<Job>;

/// Shared handle to the tree of one exec. Clones refer to the same tree, so a
/// blocking backend thread can arm it while the async side owns the guard.
#[derive(Clone, Default)]
pub(crate) struct KillSwitch(Arc<Mutex<Option<Target>>>);

impl KillSwitch {
    /// Record the tree to kill once the command is running.
    pub(crate) fn arm(&self, target: Target) {
        *self.lock() = Some(target);
    }

    /// Forget the tree; the exec finished on its own.
    pub(crate) fn disarm(&self) {
        self.lock().take();
    }

    /// Kill the whole tree, if one is armed. Holding the lock while signalling
    /// keeps a concurrent `disarm` from racing the kill.
    pub(crate) fn fire(&self) {
        let mut slot = self.lock();
        if let Some(target) = slot.take() {
            kill(&target);
        }
    }

    /// A guard that fires this switch when dropped.
    pub(crate) fn on_drop(&self) -> KillOnDrop {
        KillOnDrop(self.clone())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Target>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Fires its [`KillSwitch`] on drop. A no-op once the switch is disarmed.
pub(crate) struct KillOnDrop(KillSwitch);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        self.0.fire();
    }
}

#[cfg(unix)]
fn kill(pgid: &i32) {
    // SAFETY: killpg(2) takes plain integers. The group is ours: its leader was
    // spawned by this exec and is still armed, so the id has not been reused.
    unsafe {
        libc::killpg(*pgid, libc::SIGKILL);
    }
}

#[cfg(windows)]
fn kill(job: &Arc<Job>) {
    job.terminate();
}

#[cfg(windows)]
pub(crate) use job::Job;

#[cfg(windows)]
mod job {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, TerminateJobObject,
    };
    use windows::core::PCWSTR;

    /// An anonymous Job Object. Every process the command starts joins it, so
    /// terminating the job stops the whole tree.
    pub(crate) struct Job(HANDLE);

    // SAFETY: a job handle is a kernel object reference, usable from any thread.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        pub(crate) fn new() -> std::io::Result<Self> {
            // SAFETY: an unnamed job with default security; the handle is owned
            // by the returned value and closed on drop.
            unsafe { CreateJobObjectW(None, PCWSTR::null()) }
                .map(Job)
                .map_err(std::io::Error::other)
        }

        /// Put a process in the job. Start it suspended and resume it after
        /// this, so nothing it spawns can escape first.
        pub(crate) fn assign(&self, process: HANDLE) -> std::io::Result<()> {
            // SAFETY: both handles are live for the duration of the call.
            unsafe { AssignProcessToJobObject(self.0, process) }.map_err(std::io::Error::other)
        }

        pub(crate) fn terminate(&self) {
            // SAFETY: the handle is live until drop.
            unsafe {
                let _ = TerminateJobObject(self.0, 1);
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: the handle is owned by this value and closed once.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}
