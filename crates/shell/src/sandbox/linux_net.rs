//! Shared rootless-netns helpers used by the transparent network backend
//! ([`super::linux_transparent`]): bringing loopback up, pipe/fd plumbing, the
//! supervisor re-exec path resolution, and the unprivileged-userns capability
//! probe.
//!
//! Concurrency discipline: code that runs in a cloned child before `execve` does
//! ONLY async-signal-safe work (raw syscalls, no allocation, no shared state).

use std::os::fd::RawFd;
use std::sync::OnceLock;

/// Bring the loopback interface up inside the current network namespace via
/// `SIOCSIFFLAGS`. Returns false on failure. Linux-only, single-threaded caller
/// (the supervisor).
pub fn bring_loopback_up() -> bool {
    use std::mem;
    // SAFETY: standard ioctl on a freshly created socket; ifreq is zeroed and
    // the interface name is a fixed NUL-terminated "lo".
    unsafe {
        let sock = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        if sock < 0 {
            return false;
        }
        let mut ifr: libc::ifreq = mem::zeroed();
        let name = b"lo\0";
        for (i, &c) in name.iter().enumerate() {
            ifr.ifr_name[i] = c as libc::c_char;
        }
        if libc::ioctl(sock, libc::SIOCGIFFLAGS, &mut ifr) < 0 {
            libc::close(sock);
            return false;
        }
        ifr.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
        let ok = libc::ioctl(sock, libc::SIOCSIFFLAGS, &ifr) >= 0;
        libc::close(sock);
        ok
    }
}

/// The executable to re-exec as the in-netns supervisor: an
/// `AGENTD_NETNS_SUPERVISOR_BIN` override (tests), else this process's own binary
/// (the daemon re-execs itself; the supervisor dispatch handles the mode).
pub(crate) fn supervisor_path() -> Option<String> {
    if let Ok(p) = std::env::var("AGENTD_NETNS_SUPERVISOR_BIN") {
        return Some(p);
    }
    std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

pub(crate) struct Pipe {
    pub(crate) rd: RawFd,
    pub(crate) wr: RawFd,
}

pub(crate) fn make_pipe() -> std::io::Result<Pipe> {
    use nix::fcntl::OFlag;
    use nix::unistd::pipe2;
    use std::os::fd::IntoRawFd;
    // CLOEXEC by default; cleared on the end the supervisor must inherit.
    let (rd, wr) = pipe2(OFlag::O_CLOEXEC)?;
    Ok(Pipe {
        rd: rd.into_raw_fd(),
        wr: wr.into_raw_fd(),
    })
}

pub(crate) fn set_cloexec(fd: RawFd, on: bool) {
    use nix::fcntl::{FcntlArg, FdFlag, fcntl};
    let flag = if on {
        FdFlag::FD_CLOEXEC
    } else {
        FdFlag::empty()
    };
    let _ = fcntl(fd, FcntlArg::F_SETFD(flag));
}

pub(crate) fn write_file(path: &str, data: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().write(true).open(path)?;
    f.write_all(data.as_bytes())
}

/// True iff the FULL rootless-netns containment can actually be set up here:
/// unshare `CLONE_NEWUSER | CLONE_NEWNET`, map uid/gid to root inside, and bring
/// `lo` up. A shallow "can unshare" probe is not enough — some sandboxes (e.g.
/// hardened CI runners) allow the unshare but block the uid-map-to-root or the
/// `lo` ioctl, which the real path needs. Cached for the process lifetime.
pub fn userns_net_supported() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(probe_userns_net)
}

fn probe_userns_net() -> bool {
    // SAFETY: the forked child does only async-signal-safe work (unshare, pipe
    // read/write, the alloc-free `bring_loopback_up`, _exit). The parent writes
    // the child's uid/gid maps, then collects the child's verdict.
    unsafe {
        let mut s1 = [0i32; 2]; // child -> parent: "unshared"
        let mut s2 = [0i32; 2]; // parent -> child: "maps written"
        if libc::pipe(s1.as_mut_ptr()) != 0 {
            return false;
        }
        if libc::pipe(s2.as_mut_ptr()) != 0 {
            libc::close(s1[0]);
            libc::close(s1[1]);
            return false;
        }
        let uid = libc::getuid();
        let gid = libc::getgid();

        let pid = libc::fork();
        if pid < 0 {
            return false;
        }
        if pid == 0 {
            let flags = libc::CLONE_NEWUSER | libc::CLONE_NEWNET;
            if libc::unshare(flags) != 0 {
                libc::_exit(1);
            }
            let one = [1u8];
            libc::write(s1[1], one.as_ptr() as *const _, 1);
            let mut b = [0u8; 1];
            libc::read(s2[0], b.as_mut_ptr() as *mut _, 1);
            let ok = bring_loopback_up();
            libc::_exit(if ok { 0 } else { 1 });
        }

        libc::close(s1[1]);
        libc::close(s2[0]);
        let mut tmp = [0u8; 1];
        if libc::read(s1[0], tmp.as_mut_ptr() as *mut _, 1) != 1 {
            libc::close(s1[0]);
            libc::close(s2[1]);
            let mut st = 0;
            libc::waitpid(pid, &mut st, 0);
            return false;
        }
        let _ = write_file(&format!("/proc/{pid}/uid_map"), &format!("0 {uid} 1\n"));
        let _ = write_file(&format!("/proc/{pid}/setgroups"), "deny");
        let _ = write_file(&format!("/proc/{pid}/gid_map"), &format!("0 {gid} 1\n"));
        let one = [1u8];
        libc::write(s2[1], one.as_ptr() as *const _, 1);
        libc::close(s1[0]);
        libc::close(s2[1]);
        let mut st = 0;
        libc::waitpid(pid, &mut st, 0);
        libc::WIFEXITED(st) && libc::WEXITSTATUS(st) == 0
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn userns_probe_returns_bool_without_panicking() {
        let _ = super::userns_net_supported();
    }
}
