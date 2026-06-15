//! Rootless network-namespace containment (Linux), host side + shared helpers.
//!
//! The command runs in a `CLONE_NEWUSER | CLONE_NEWNET` namespace whose only
//! egress is an in-namespace supervisor. The supervisor accepts the command's
//! loopback connections and passes each accepted fd to this host side via
//! SCM_RIGHTS over an anonymous control socketpair; the host splices each fd to
//! the egress proxy. See the Phase 2 design spec.
//!
//! Concurrency discipline: the cloned child does ONLY async-signal-safe work
//! (unshare, pipe read/write, execve). All supervisor logic runs after `execve`
//! in a fresh single-threaded process (`agentd-netns-supervisor`).

use std::os::fd::RawFd;

use serde::{Deserialize, Serialize};

/// Config handed to the supervisor binary via the `AGENTD_NETNS_SUPERVISOR`
/// environment variable (JSON). The fd numbers are inherited (non-CLOEXEC) fds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupervisorConfig {
    /// Anonymous control socketpair end the supervisor uses to pass accepted fds.
    pub control_fd: RawFd,
    /// Write ends for the command's stdout / stderr (dup2'd onto 1/2).
    pub stdout_fd: RawFd,
    pub stderr_fd: RawFd,
    /// Read end for the command's stdin, or -1 if none.
    pub stdin_fd: RawFd,
    /// The command and its argv.
    pub bin: String,
    pub args: Vec<String>,
    /// Filesystem sandbox subtrees (Landlock), applied to the command.
    pub read_paths: Vec<String>,
    pub write_paths: Vec<String>,
}

/// Env var name carrying the JSON `SupervisorConfig`.
pub const SUPERVISOR_ENV: &str = "AGENTD_NETNS_SUPERVISOR";

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
        // Read current flags.
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

// In-netns supervisor (runs in the re-exec'd process, not a separate binary)

/// If this process was re-exec'd as the netns supervisor (`AGENTD_NETNS_SUPERVISOR`
/// env is set), run the supervisor and exit. Otherwise return immediately. The
/// host binary (the daemon) must call this as the FIRST thing in `main`, before
/// any threads or async runtime start — the supervisor forks the command and
/// must be single-threaded at that point.
pub fn run_supervisor_if_requested() {
    if let Ok(json) = std::env::var(SUPERVISOR_ENV) {
        let code = match serde_json::from_str::<SupervisorConfig>(&json) {
            Ok(cfg) => supervisor::run(cfg),
            Err(_) => 127,
        };
        std::process::exit(code);
    }
}

mod supervisor {
    use std::ffi::CString;
    use std::os::fd::{FromRawFd, RawFd};

    use nix::sys::socket::{
        AddressFamily, ControlMessage, MsgFlags, SockFlag, SockType, SockaddrIn, bind, listen,
        sendmsg, socket,
    };
    use nix::sys::wait::{WaitStatus, waitpid};
    use nix::unistd::{ForkResult, fork};

    use super::{SupervisorConfig, bring_loopback_up};
    use crate::SandboxPolicy;

    pub fn run(cfg: SupervisorConfig) -> i32 {
        if !bring_loopback_up() {
            eprintln!("supervisor: failed to bring lo up");
            return 127;
        }
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

        // SAFETY: single-threaded process (just execve'd), so the post-fork child
        // may run normal code before its own execve.
        let child = match unsafe { fork() } {
            Ok(ForkResult::Child) => exec_command(&cfg, port), // never returns (`!`)
            Ok(ForkResult::Parent { child }) => child,
            Err(_) => return 127,
        };

        unsafe {
            libc::close(cfg.stdout_fd);
            libc::close(cfg.stderr_fd);
            if cfg.stdin_fd >= 0 {
                libc::close(cfg.stdin_fd);
            }
        }

        listener.set_nonblocking(true).ok();
        loop {
            match waitpid(child, Some(nix::sys::wait::WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(_, code)) => return code,
                Ok(WaitStatus::Signaled(_, _, _)) => return 129,
                _ => {}
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    use std::os::fd::AsRawFd;
                    let _ = send_fd(cfg.control_fd, stream.as_raw_fd());
                    drop(stream);
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(2)),
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
        use std::os::fd::IntoRawFd;
        Some(unsafe { std::net::TcpListener::from_raw_fd(fd.into_raw_fd()) })
    }

    fn send_fd(control_fd: RawFd, fd: RawFd) -> nix::Result<()> {
        let fds = [fd];
        let cmsg = [ControlMessage::ScmRights(&fds)];
        let iov = [std::io::IoSlice::new(b"f")];
        sendmsg::<()>(control_fd, &iov, &cmsg, MsgFlags::empty(), None)?;
        Ok(())
    }

    fn exec_command(cfg: &SupervisorConfig, proxy_port: u16) -> ! {
        unsafe {
            libc::dup2(cfg.stdout_fd, 1);
            libc::dup2(cfg.stderr_fd, 2);
            if cfg.stdin_fd >= 0 {
                libc::dup2(cfg.stdin_fd, 0);
            }
            libc::close(cfg.stdout_fd);
            libc::close(cfg.stderr_fd);
            if cfg.stdin_fd >= 0 {
                libc::close(cfg.stdin_fd);
            }
            libc::close(cfg.control_fd);
        }

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
        // the loopback connection to the supervisor (network governed by netns).
        let policy = SandboxPolicy {
            read_paths: cfg.read_paths.iter().map(Into::into).collect(),
            write_paths: cfg.write_paths.iter().map(Into::into).collect(),
            allow_net: true,
            net_hosts: vec![],
            unrestricted: false,
        };
        if let Err(e) = crate::sandbox::apply(&policy) {
            eprintln!("supervisor: landlock apply failed: {e}");
            unsafe { libc::_exit(126) };
        }

        let bin = CString::new(cfg.bin.as_str()).unwrap();
        let mut argv: Vec<CString> = Vec::with_capacity(cfg.args.len() + 1);
        argv.push(bin.clone());
        for a in &cfg.args {
            argv.push(CString::new(a.as_str()).unwrap_or_default());
        }
        let _ = nix::unistd::execvp(&bin, &argv);
        eprintln!("supervisor: exec {} failed", cfg.bin);
        unsafe { libc::_exit(127) }
    }
}

// Host-side contained spawn

use std::ffi::CString;
use std::io::Read;
use std::os::fd::FromRawFd;

use crate::proxy::Proxy;
use crate::{ExecRequest, ExecResult, SandboxPolicy, ShellError};

/// The executable to re-exec as the in-netns supervisor: an
/// `AGENTD_NETNS_SUPERVISOR_BIN` override (tests), else this process's own binary
/// (the daemon re-execs itself; `run_supervisor_if_requested` handles the mode).
fn supervisor_path() -> Option<String> {
    if let Ok(p) = std::env::var("AGENTD_NETNS_SUPERVISOR_BIN") {
        return Some(p);
    }
    std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

fn set_cloexec(fd: RawFd, on: bool) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags < 0 {
            return;
        }
        let new = if on {
            flags | libc::FD_CLOEXEC
        } else {
            flags & !libc::FD_CLOEXEC
        };
        libc::fcntl(fd, libc::F_SETFD, new);
    }
}

/// Run `req` inside a rootless netns whose only egress is `proxy`. Fails closed
/// if the namespace cannot be created. The netns child never addresses the proxy;
/// the host splices the supervisor's passed fds to it.
pub async fn run_contained(
    req: &ExecRequest,
    policy: &SandboxPolicy,
    proxy: &Proxy,
) -> Result<ExecResult, ShellError> {
    let sup_path = supervisor_path()
        .ok_or_else(|| ShellError::Sandbox("netns supervisor binary not found".into()))?;
    let proxy_addr = proxy.addr();
    let req = req.clone();
    let policy = policy.clone();

    // All blocking syscall setup + clone happens on a blocking thread; the
    // returned handles are driven on the tokio runtime.
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        run_contained_blocking(&req, &policy, &sup_path, proxy_addr, handle)
    })
    .await
    .map_err(|e| ShellError::Sandbox(format!("join: {e}")))?
}

struct Pipe {
    rd: RawFd,
    wr: RawFd,
}

fn make_pipe() -> std::io::Result<Pipe> {
    let mut fds = [0i32; 2];
    // CLOEXEC by default; clear on the end the supervisor must inherit.
    let r = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(Pipe {
        rd: fds[0],
        wr: fds[1],
    })
}

fn write_file(path: &str, data: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().write(true).open(path)?;
    f.write_all(data.as_bytes())
}

fn run_contained_blocking(
    req: &ExecRequest,
    policy: &SandboxPolicy,
    sup_path: &str,
    proxy_addr: std::net::SocketAddr,
    handle: tokio::runtime::Handle,
) -> Result<ExecResult, ShellError> {
    let sb = |e: String| ShellError::Sandbox(e);

    // Control socketpair (anonymous; no path the child could address).
    let mut sp = [0i32; 2];
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sp.as_mut_ptr()) } < 0 {
        return Err(sb("socketpair".into()));
    }
    let (ctrl_host, ctrl_sup) = (sp[0], sp[1]);
    set_cloexec(ctrl_host, true); // host keeps this
    set_cloexec(ctrl_sup, false); // supervisor inherits this

    // stdio pipes.
    let out = make_pipe().map_err(|e| sb(e.to_string()))?;
    let err = make_pipe().map_err(|e| sb(e.to_string()))?;
    let sin = make_pipe().map_err(|e| sb(e.to_string()))?;
    set_cloexec(out.wr, false); // supervisor inherits write ends
    set_cloexec(err.wr, false);
    set_cloexec(sin.rd, false); // supervisor inherits stdin read end

    // sync pipes: s1 child->parent "unshared", s2 parent->child "maps written".
    let s1 = make_pipe().map_err(|e| sb(e.to_string()))?;
    let s2 = make_pipe().map_err(|e| sb(e.to_string()))?;

    // Build the supervisor config + execve argv/env BEFORE fork (parent work).
    let cfg = SupervisorConfig {
        control_fd: ctrl_sup,
        stdout_fd: out.wr,
        stderr_fd: err.wr,
        stdin_fd: if req.stdin.is_some() { sin.rd } else { -1 },
        bin: req.bin.clone(),
        args: req.args.clone(),
        read_paths: policy
            .read_paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        write_paths: policy
            .write_paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
    };
    let cfg_json = serde_json::to_string(&cfg).map_err(|e| sb(e.to_string()))?;
    let (argv, envp) = build_exec_args(sup_path, &cfg_json);

    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };

    // Fork the child that will unshare + execve the supervisor.
    // SAFETY: between fork and execve the child does ONLY async-signal-safe work.
    let child = unsafe { libc::fork() };
    if child < 0 {
        return Err(sb("fork".into()));
    }
    if child == 0 {
        // ---- child (async-signal-safe only) ----
        // SAFETY: only raw syscalls + execve before exec; no allocation, no
        // shared-state mutation.
        unsafe {
            let flags = libc::CLONE_NEWUSER | libc::CLONE_NEWNET;
            if libc::unshare(flags) != 0 {
                libc::_exit(125);
            }
            let one = [1u8];
            libc::write(s1.wr, one.as_ptr() as *const _, 1);
            let mut b = [0u8; 1];
            libc::read(s2.rd, b.as_mut_ptr() as *mut _, 1);
            libc::execve(argv[0], argv.as_ptr(), envp.as_ptr());
            libc::_exit(127); // execve(supervisor) failed
        }
    }

    // ---- parent ----
    // Close the fds the supervisor/child own (keep host-side ends).
    unsafe {
        libc::close(ctrl_sup);
        libc::close(out.wr);
        libc::close(err.wr);
        libc::close(sin.rd);
        libc::close(s1.wr);
        libc::close(s2.rd);
    }

    // Wait for "unshared".
    let mut tmp = [0u8; 1];
    if unsafe { libc::read(s1.rd, tmp.as_mut_ptr() as *mut _, 1) } != 1 {
        unsafe {
            libc::close(s1.rd);
            libc::close(s2.wr);
        }
        return Err(sb("child failed to unshare".into()));
    }

    // Write uid/gid maps for the child. Map inside-0 -> outside uid so the
    // supervisor is root INSIDE the namespace: that retains capabilities across
    // execve (needed for CAP_NET_ADMIN to bring `lo` up). Single-line identity-
    // to-root maps are permitted unprivileged.
    let _ = write_file(&format!("/proc/{child}/uid_map"), &format!("0 {uid} 1\n"));
    let _ = write_file(&format!("/proc/{child}/setgroups"), "deny");
    let _ = write_file(&format!("/proc/{child}/gid_map"), &format!("0 {gid} 1\n"));

    // Release the child into execve.
    unsafe {
        let one = [1u8];
        libc::write(s2.wr, one.as_ptr() as *const _, 1);
        libc::close(s1.rd);
        libc::close(s2.wr);
    }

    // Host side: receive passed fds and splice each to the proxy.
    let recv_join = std::thread::spawn(move || fd_receiver_loop(ctrl_host, proxy_addr, handle));

    // Drain stdout/stderr.
    let mut out_file = unsafe { std::fs::File::from_raw_fd(out.rd) };
    let mut err_file = unsafe { std::fs::File::from_raw_fd(err.rd) };
    let out_join = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = out_file.read_to_end(&mut v);
        v
    });
    let err_join = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = err_file.read_to_end(&mut v);
        v
    });

    // Write stdin payload, if any.
    if let Some(input) = &req.stdin {
        let mut sf = unsafe { std::fs::File::from_raw_fd(sin.wr) };
        use std::io::Write;
        let _ = sf.write_all(input.as_bytes());
        // dropping sf closes the write end → command sees EOF
    } else {
        unsafe { libc::close(sin.wr) };
    }

    // Reap the child (supervisor) → its exit code is the command's status.
    let mut status = 0i32;
    unsafe { libc::waitpid(child, &mut status, 0) };
    let exit_code = if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        -1
    };

    let stdout = String::from_utf8_lossy(&out_join.join().unwrap_or_default()).into_owned();
    let stderr_text = String::from_utf8_lossy(&err_join.join().unwrap_or_default()).into_owned();
    let _ = recv_join.join();

    let (stdout, stderr) = if req.separate_stderr {
        (stdout, stderr_text)
    } else {
        let mut merged = stdout;
        if !stderr_text.is_empty() {
            if !merged.is_empty() && !merged.ends_with('\n') {
                merged.push('\n');
            }
            merged.push_str(&stderr_text);
        }
        (merged, String::new())
    };

    Ok(ExecResult {
        exit_code,
        stdout,
        stderr,
    })
}

/// Build argv (ptr array) and envp for `execve` of the supervisor. Keeps the
/// backing CStrings alive by leaking them (the child execs immediately; the
/// parent returns and the small leak is bounded per invocation).
fn build_exec_args(
    sup_path: &str,
    cfg_json: &str,
) -> (Vec<*const libc::c_char>, Vec<*const libc::c_char>) {
    let argv0 = CString::new(sup_path).unwrap();
    let mut argv: Vec<*const libc::c_char> = vec![argv0.as_ptr()];
    std::mem::forget(argv0);
    argv.push(std::ptr::null());

    let mut envp: Vec<*const libc::c_char> = Vec::new();
    for (k, v) in std::env::vars() {
        if k == SUPERVISOR_ENV {
            continue;
        }
        let e = CString::new(format!("{k}={v}")).unwrap_or_default();
        envp.push(e.as_ptr());
        std::mem::forget(e);
    }
    let sup_env = CString::new(format!("{SUPERVISOR_ENV}={cfg_json}")).unwrap();
    envp.push(sup_env.as_ptr());
    std::mem::forget(sup_env);
    envp.push(std::ptr::null());

    (argv, envp)
}

/// Receive passed fds from the supervisor and splice each to the proxy. Runs on
/// a blocking thread; spawns the splice work onto the tokio runtime.
fn fd_receiver_loop(
    ctrl_host: RawFd,
    proxy_addr: std::net::SocketAddr,
    handle: tokio::runtime::Handle,
) {
    use nix::sys::socket::{ControlMessageOwned, MsgFlags, recvmsg};
    loop {
        let mut buf = [0u8; 8];
        let mut iov = [std::io::IoSliceMut::new(&mut buf)];
        let mut cmsg_space = nix::cmsg_space!([RawFd; 1]);
        let msg = match recvmsg::<()>(
            ctrl_host,
            &mut iov,
            Some(&mut cmsg_space),
            MsgFlags::empty(),
        ) {
            Ok(m) => m,
            Err(_) => break,
        };
        if msg.bytes == 0 {
            break; // supervisor closed → done
        }
        let mut got = None;
        if let Ok(cmsgs) = msg.cmsgs() {
            for c in cmsgs {
                if let ControlMessageOwned::ScmRights(fds) = c
                    && let Some(&fd) = fds.first()
                {
                    got = Some(fd);
                }
            }
        }
        if let Some(fd) = got {
            handle.spawn(splice_to_proxy(fd, proxy_addr));
        }
    }
    unsafe { libc::close(ctrl_host) };
}

/// Splice a received (already-connected) socket fd to a fresh connection to the
/// proxy. The proxy enforces the `net:<host>` policy.
async fn splice_to_proxy(fd: RawFd, proxy_addr: std::net::SocketAddr) {
    use tokio::net::TcpStream;
    // Wrap the received fd as a nonblocking std socket, then tokio.
    let std_sock = unsafe { std::net::TcpStream::from_raw_fd(fd) };
    if std_sock.set_nonblocking(true).is_err() {
        return;
    }
    let mut child = match TcpStream::from_std(std_sock) {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut up = match TcpStream::connect(proxy_addr).await {
        Ok(s) => s,
        Err(_) => return,
    };
    let _ = tokio::io::copy_bidirectional(&mut child, &mut up).await;
}

// userns probe

use std::sync::OnceLock;

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
            // Mapped to root inside the new userns now; the real path needs lo up.
            let ok = bring_loopback_up();
            libc::_exit(if ok { 0 } else { 1 });
        }

        // Parent.
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
