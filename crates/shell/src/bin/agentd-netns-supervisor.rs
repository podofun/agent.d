//! In-netns supervisor for the Phase 2 network sandbox.
//!
//! NOT a user-facing tool. It is `execve`d by `sandbox::linux_net::run_contained`
//! inside a freshly created `CLONE_NEWUSER | CLONE_NEWNET` namespace, in a clean
//! single-threaded process (so the fork of the real command below is safe).
//!
//! Responsibilities:
//!   1. Bring `lo` up; bind+listen a loopback TCP port.
//!   2. Fork+exec the real command with proxy env + Phase 1 Landlock applied,
//!      its stdio wired to inherited pipe fds.
//!   3. Accept the command's loopback connections; pass each accepted fd to the
//!      host over the control socketpair via SCM_RIGHTS.
//!   4. When the command exits, exit with its status.

#[cfg(not(target_os = "linux"))]
fn main() {
    // The binary is auto-discovered on every target; only Linux has the syscalls.
    eprintln!("agentd-netns-supervisor is Linux-only");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
fn main() {
    std::process::exit(linux::run());
}

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::CString;
    use std::os::fd::{FromRawFd, RawFd};

    use agentd_shell::SandboxPolicy;
    use agentd_shell::sandbox::linux_net::{SUPERVISOR_ENV, SupervisorConfig, bring_loopback_up};
    use nix::sys::socket::{
        AddressFamily, ControlMessage, MsgFlags, SockFlag, SockType, SockaddrIn, bind, listen,
        sendmsg, socket,
    };
    use nix::sys::wait::{WaitStatus, waitpid};
    use nix::unistd::{ForkResult, fork};

    pub fn run() -> i32 {
        let cfg: SupervisorConfig = match std::env::var(SUPERVISOR_ENV)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
        {
            Some(c) => c,
            None => {
                eprintln!("supervisor: missing/invalid {SUPERVISOR_ENV}");
                return 127;
            }
        };

        if !bring_loopback_up() {
            eprintln!("supervisor: failed to bring lo up");
            return 127;
        }

        // Bind + listen loopback TCP on an ephemeral port.
        let listener = match make_listener() {
            Some(l) => l,
            None => {
                eprintln!("supervisor: failed to bind loopback listener");
                return 127;
            }
        };
        let port = match listener.local_addr() {
            Ok(a) => a.port(),
            Err(_) => return 127,
        };

        // Fork + exec the real command.
        // SAFETY: this process is single-threaded (we just execve'd into it), so
        // the post-fork child may run normal code before its own execve.
        let child = match unsafe { fork() } {
            Ok(ForkResult::Child) => exec_command(&cfg, port), // never returns (`!`)
            Ok(ForkResult::Parent { child }) => child,
            Err(_) => return 127,
        };

        // Close the fds the command owns now (we keep only the control socket).
        unsafe {
            libc::close(cfg.stdout_fd);
            libc::close(cfg.stderr_fd);
            if cfg.stdin_fd >= 0 {
                libc::close(cfg.stdin_fd);
            }
        }

        // Accept loop: pass each accepted fd to the host until the command exits.
        // Use the listener nonblocking + poll on [listener, SIGCHLD via waitpid].
        listener.set_nonblocking(true).ok();

        loop {
            // Reap without hanging.
            match waitpid(child, Some(nix::sys::wait::WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(_, code)) => return code,
                Ok(WaitStatus::Signaled(_, _, _)) => return 129,
                _ => {}
            }

            match listener.accept() {
                Ok((stream, _)) => {
                    let fd: RawFd = {
                        use std::os::fd::AsRawFd;
                        stream.as_raw_fd()
                    };
                    let _ = send_fd(cfg.control_fd, fd);
                    // The host now owns a dup of the fd; drop our copy.
                    drop(stream);
                }
                Err(_) => {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            }
        }
    }

    fn make_listener() -> Option<std::net::TcpListener> {
        let fd = socket(
            AddressFamily::Inet,
            SockType::Stream,
            SockFlag::empty(),
            None,
        )
        .ok()?;
        let addr = SockaddrIn::new(127, 0, 0, 1, 0);
        bind(std::os::fd::AsRawFd::as_raw_fd(&fd), &addr).ok()?;
        listen(&fd, nix::sys::socket::Backlog::new(64).unwrap()).ok()?;
        // Convert the owned fd into a std TcpListener.
        use std::os::fd::IntoRawFd;
        let raw = fd.into_raw_fd();
        Some(unsafe { std::net::TcpListener::from_raw_fd(raw) })
    }

    /// Send one fd to the host over the control unix socket via SCM_RIGHTS.
    fn send_fd(control_fd: RawFd, fd: RawFd) -> nix::Result<()> {
        let fds = [fd];
        let cmsg = [ControlMessage::ScmRights(&fds)];
        let iov = [std::io::IoSlice::new(b"f")];
        sendmsg::<()>(control_fd, &iov, &cmsg, MsgFlags::empty(), None)?;
        Ok(())
    }

    /// In the forked child: set proxy env, apply Landlock, wire stdio, execve.
    fn exec_command(cfg: &SupervisorConfig, proxy_port: u16) -> ! {
        unsafe {
            // Wire stdio: dup the inherited pipe ends onto 0/1/2.
            libc::dup2(cfg.stdout_fd, 1);
            libc::dup2(cfg.stderr_fd, 2);
            if cfg.stdin_fd >= 0 {
                libc::dup2(cfg.stdin_fd, 0);
            }
            // Close the originals + the control socket so they never reach the
            // command (the control fd is the bridge; it must not leak).
            libc::close(cfg.stdout_fd);
            libc::close(cfg.stderr_fd);
            if cfg.stdin_fd >= 0 {
                libc::close(cfg.stdin_fd);
            }
            libc::close(cfg.control_fd);
        }

        // Proxy env: the command reaches only the in-netns supervisor port.
        let proxy = format!("http://127.0.0.1:{proxy_port}");
        for key in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "http_proxy",
            "https_proxy",
            "ALL_PROXY",
        ] {
            unsafe { std::env::set_var(key, &proxy) };
        }
        unsafe { std::env::set_var("NO_PROXY", "localhost,127.0.0.1,::1") };

        // Phase 1 filesystem sandbox. allow_net = true so Landlock does NOT block
        // the loopback connection to the supervisor (network is governed by the
        // netns, not Landlock).
        let policy = SandboxPolicy {
            read_paths: cfg.read_paths.iter().map(Into::into).collect(),
            write_paths: cfg.write_paths.iter().map(Into::into).collect(),
            allow_net: true,
            net_hosts: vec![],
            unrestricted: false,
        };
        if let Err(e) = agentd_shell::sandbox::apply(&policy) {
            eprintln!("supervisor: landlock apply failed: {e}");
            unsafe { libc::_exit(126) };
        }

        // execve the real command.
        let bin = CString::new(cfg.bin.as_str()).unwrap();
        let mut argv: Vec<CString> = Vec::with_capacity(cfg.args.len() + 1);
        argv.push(bin.clone());
        for a in &cfg.args {
            argv.push(CString::new(a.as_str()).unwrap_or_default());
        }
        let _ = nix::unistd::execvp(&bin, &argv);
        // Only reached on exec failure.
        eprintln!("supervisor: exec {} failed", cfg.bin);
        unsafe { libc::_exit(127) }
    }
}
