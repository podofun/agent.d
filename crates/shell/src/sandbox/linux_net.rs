//! Rootless network-namespace containment (Linux), host side.
//!
//! The child runs in a `CLONE_NEWUSER | CLONE_NEWNET` namespace whose only
//! egress is an in-namespace supervisor; the supervisor passes each accepted
//! connection's fd to this host side via SCM_RIGHTS, and the host splices it to
//! the egress proxy. See `docs/superpowers/specs/...phase2-net-design.md`.

use nix::sched::CloneFlags;
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{ForkResult, fork};

/// True iff an unprivileged `CLONE_NEWUSER | CLONE_NEWNET` can be created on this
/// host. Probes by attempting it in a short-lived forked child, so the daemon's
/// own namespaces are never affected. Never panics.
pub fn userns_net_supported() -> bool {
    // SAFETY: the child does only async-signal-safe work (unshare syscall, then
    // _exit) before terminating. No allocation, no shared-state mutation.
    match unsafe { fork() } {
        Ok(ForkResult::Child) => {
            let flags = CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWNET;
            let code = if nix::sched::unshare(flags).is_ok() { 0 } else { 1 };
            // _exit avoids running atexit handlers / flushing shared buffers.
            unsafe { libc::_exit(code) };
        }
        Ok(ForkResult::Parent { child }) => match waitpid(child, None) {
            Ok(WaitStatus::Exited(_, 0)) => true,
            _ => false,
        },
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn userns_probe_returns_bool_without_panicking() {
        // Must never panic; returns whether an unprivileged user+net ns can be
        // created here. The value depends on the host (CI may disable userns).
        let _ = super::userns_net_supported();
    }
}
