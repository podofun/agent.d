//! Kill a running command together with everything it started.
//!
//! Each backend puts the command in a container the whole tree inherits: a
//! process group on Unix, a Job Object on Windows, the broker session on the
//! macOS network path. The backend arms a [`KillSwitch`] with the way to kill
//! that container once it exists. `exec` fires the switch on timeout, and a
//! [`KillOnDrop`] guard fires it when the exec future is dropped.

use std::sync::{Arc, Mutex};

use crate::ShellError;

/// Kills one command's tree. Called at most once.
pub(crate) type Kill = Box<dyn FnOnce() + Send>;

enum State {
    /// Nothing running yet.
    Idle,
    /// The command runs; this kills it.
    Armed(Kill),
    /// A kill was asked for. Anything armed after this is killed at once.
    Fired,
}

/// Shared handle to the tree of one exec. Clones refer to the same tree, so a
/// blocking backend thread can arm it while `exec` fires it.
#[derive(Clone)]
pub(crate) struct KillSwitch(Arc<Mutex<State>>);

impl Default for KillSwitch {
    fn default() -> Self {
        KillSwitch(Arc::new(Mutex::new(State::Idle)))
    }
}

impl KillSwitch {
    /// Record how to kill the tree once the command is running. If a kill was
    /// already asked for, the tree is killed now and the backend must stop.
    pub(crate) fn arm(&self, kill: Kill) -> Result<(), ShellError> {
        let mut state = self.lock();
        if matches!(*state, State::Fired) {
            drop(state);
            kill();
            return Err(ShellError::Stopped);
        }
        *state = State::Armed(kill);
        Ok(())
    }

    /// Stop early when a kill was already asked for, so a backend skips
    /// setup for a command that would be killed at once.
    pub(crate) fn check(&self) -> Result<(), ShellError> {
        match *self.lock() {
            State::Fired => Err(ShellError::Stopped),
            _ => Ok(()),
        }
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn fired(&self) -> bool {
        self.check().is_err()
    }

    /// Forget the tree; the command finished, or is about to be reaped. A
    /// fired switch stays fired.
    pub(crate) fn disarm(&self) {
        let mut state = self.lock();
        if matches!(*state, State::Armed(_)) {
            *state = State::Idle;
        }
    }

    /// Kill the whole tree if one is armed, and kill anything armed later.
    /// Holding the lock while killing keeps a concurrent `disarm` (which
    /// precedes reaping) from racing the kill.
    pub(crate) fn fire(&self) {
        let mut state = self.lock();
        if let State::Armed(kill) = std::mem::replace(&mut *state, State::Fired) {
            kill();
        }
    }

    /// A guard that fires this switch when dropped.
    pub(crate) fn on_drop(&self) -> KillOnDrop {
        KillOnDrop(self.clone())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Fires its [`KillSwitch`] on drop. A no-op once the switch is disarmed.
pub(crate) struct KillOnDrop(KillSwitch);

impl KillOnDrop {
    /// The exec finished on its own; dropping the guard must not kill.
    pub(crate) fn defuse(self) {
        self.0.disarm();
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        self.0.fire();
    }
}

/// Kill a Unix process group. The group must belong to this exec and its
/// leader must be unreaped, so the id cannot have been reused.
#[cfg(unix)]
pub(crate) fn kill_group(pgid: i32) -> Kill {
    Box::new(move || {
        // SAFETY: killpg(2) takes plain integers and touches no memory.
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
    })
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

    // SAFETY: a job handle is a kernel object reference, usable from any
    // thread; `Job` only closes it once, on drop.
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

        /// Put a process in the job.
        pub(crate) fn assign(&self, process: HANDLE) -> std::io::Result<()> {
            // SAFETY: both handles are live for the duration of the call.
            unsafe { AssignProcessToJobObject(self.0, process) }.map_err(std::io::Error::other)
        }

        /// A kill for the switch. It owns the job, so the handle closes when
        /// the switch is disarmed or fired.
        pub(crate) fn into_kill(self) -> super::Kill {
            Box::new(move || self.terminate())
        }

        fn terminate(&self) {
            // SAFETY: the handle is live until `self` drops.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn recording() -> (Arc<AtomicBool>, Kill) {
        let killed = Arc::new(AtomicBool::new(false));
        let flag = killed.clone();
        (killed, Box::new(move || flag.store(true, Ordering::SeqCst)))
    }

    #[test]
    fn a_kill_asked_for_before_arming_kills_on_arm() {
        let switch = KillSwitch::default();
        switch.fire();
        assert!(switch.check().is_err());
        let (killed, kill) = recording();
        assert!(matches!(switch.arm(kill), Err(ShellError::Stopped)));
        assert!(killed.load(Ordering::SeqCst));
    }

    #[test]
    fn firing_an_armed_switch_kills_once() {
        let switch = KillSwitch::default();
        let (killed, kill) = recording();
        switch.arm(kill).unwrap();
        switch.fire();
        assert!(killed.load(Ordering::SeqCst));
        switch.fire();
    }

    #[test]
    fn a_disarmed_switch_does_not_kill_on_drop() {
        let switch = KillSwitch::default();
        let guard = switch.on_drop();
        let (killed, kill) = recording();
        switch.arm(kill).unwrap();
        guard.defuse();
        assert!(!killed.load(Ordering::SeqCst));
    }

    #[test]
    fn dropping_the_guard_kills_the_armed_tree() {
        let switch = KillSwitch::default();
        let guard = switch.on_drop();
        let (killed, kill) = recording();
        switch.arm(kill).unwrap();
        drop(guard);
        assert!(killed.load(Ordering::SeqCst));
    }
}
