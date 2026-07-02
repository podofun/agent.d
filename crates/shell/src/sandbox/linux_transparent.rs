//! Transparent host/IP-granular netns backend (Linux, Architecture B).
//!
//! The rootless netns has no real egress (no pasta/slirp), so the child's
//! traffic is transparently captured and bridged to the host:
//!
//! 1. In the netns: `lo` up + a default route via `lo` so connects to external
//!    IPs reach the OUTPUT path, where an nft NAT ruleset REDIRECTs all TCP to an
//!    in-namespace intercept and UDP/53 to a DNS intercept (validated: the
//!    original destination survives via `SO_ORIGINAL_DST`).
//! 2. The supervisor passes each intercepted TCP connection's fd + original
//!    destination to the host over a `ctrl` socketpair, and bridges each DNS
//!    query over a `dns` socketpair.
//! 3. The host enforces: the DNS handler resolves allowed names and commits their
//!    IPs to the shared permitted set BEFORE answering; the TCP relay admits a
//!    connection iff its original-destination IP is in the set, then splices to
//!    the real destination.
//!
//! The child sets no proxy env and speaks any protocol; enforcement is the
//! kernel redirect + the host-side IP allowlist.

use std::ffi::CString;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::fd::{FromRawFd, RawFd};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nix::sys::socket::SockType;
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{Pid, close, getgid, getuid, read, write};
use serde::{Deserialize, Serialize};

use super::linux_net::{make_pipe, set_cloexec, supervisor_path, write_file};
use crate::gateway::{SharedSet, admit, handle_dns};
use crate::netfilter::{PermitSet, SetConfig};
use crate::{ExecRequest, ExecResult, SandboxPolicy, ShellError};

/// Env var carrying the JSON [`TConfig`] to the re-exec'd transparent supervisor.
pub const TPROXY_ENV: &str = "AGENTD_NETNS_TPROXY";

/// Default DNS pin TTL applied to resolved IPs.
const PIN_TTL: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TConfig {
    /// Stream socketpair end: supervisor passes intercepted TCP fds + dst here.
    pub ctrl_fd: RawFd,
    /// Datagram socketpair end: supervisor bridges DNS queries/responses here.
    pub dns_fd: RawFd,
    pub stdout_fd: RawFd,
    pub stderr_fd: RawFd,
    pub stdin_fd: RawFd,
    pub bin: String,
    pub args: Vec<String>,
    pub read_paths: Vec<String>,
    pub write_paths: Vec<String>,
}

/// If re-exec'd as the transparent supervisor, run it and exit; else return.
/// Called from the daemon's `run_supervisor_if_requested` dispatch.
pub fn run_supervisor_if_requested() -> bool {
    if let Ok(json) = std::env::var(TPROXY_ENV) {
        let code = match serde_json::from_str::<TConfig>(&json) {
            Ok(cfg) => supervisor::run(cfg),
            Err(_) => 127,
        };
        std::process::exit(code);
    }
    false
}

/// Recover the pre-REDIRECT destination of an accepted connection via nix's typed
/// `getsockopt` (no unsafe). Tries IPv4 then IPv6.
fn original_dst<F: std::os::fd::AsFd>(sock: &F) -> Option<SocketAddr> {
    use nix::sys::socket::getsockopt;
    use nix::sys::socket::sockopt::{Ip6tOriginalDst, OriginalDst};

    if let Ok(a) = getsockopt(sock, OriginalDst) {
        let ip = Ipv4Addr::from(u32::from_be(a.sin_addr.s_addr));
        return Some(SocketAddr::new(IpAddr::V4(ip), u16::from_be(a.sin_port)));
    }
    if let Ok(a) = getsockopt(sock, Ip6tOriginalDst) {
        let ip = Ipv6Addr::from(a.sin6_addr.s6_addr);
        return Some(SocketAddr::new(IpAddr::V6(ip), u16::from_be(a.sin6_port)));
    }
    None
}

/// Encode a `SocketAddr` to a fixed wire form for the ctrl-socket payload:
/// `[fam:1][port:2][addr:16]` (v4 left-padded). 19 bytes.
fn encode_dst(a: &SocketAddr) -> [u8; 19] {
    let mut b = [0u8; 19];
    b[1..3].copy_from_slice(&a.port().to_be_bytes());
    match a.ip() {
        IpAddr::V4(v4) => {
            b[0] = 4;
            b[3..7].copy_from_slice(&v4.octets());
        }
        IpAddr::V6(v6) => {
            b[0] = 6;
            b[3..19].copy_from_slice(&v6.octets());
        }
    }
    b
}

fn decode_dst(b: &[u8]) -> Option<SocketAddr> {
    if b.len() < 19 {
        return None;
    }
    let port = u16::from_be_bytes([b[1], b[2]]);
    match b[0] {
        4 => {
            let o: [u8; 4] = b[3..7].try_into().ok()?;
            Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(o)), port))
        }
        6 => {
            let o: [u8; 16] = b[3..19].try_into().ok()?;
            Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(o)), port))
        }
        _ => None,
    }
}

mod supervisor {
    use std::ffi::CString;
    use std::io::Write;
    use std::net::{TcpListener, UdpSocket};
    use std::os::fd::AsRawFd;
    use std::process::{Command, Stdio};

    use nix::sys::socket::{ControlMessage, MsgFlags, sendmsg};
    use nix::sys::wait::{WaitStatus, waitpid};
    use nix::unistd::{Pid, close, execvp};

    use super::{TConfig, encode_dst, original_dst};
    use crate::SandboxPolicy;
    use crate::netfilter::nftables;
    use crate::sandbox::linux_net::bring_loopback_up;

    pub fn run(cfg: TConfig) -> i32 {
        if !bring_loopback_up() {
            eprintln!("tproxy: lo up failed");
            return 127;
        }
        // Default route via lo so external connects reach the OUTPUT nat hook.
        // CRITICAL: `src 127.0.0.1` — without an explicit source the kernel picks
        // 0.0.0.0 for a route via lo, the intercept's accepted peer becomes
        // 0.0.0.0, and replies are undeliverable (validated the hard way).
        // `route_localnet` lets the loopback REDIRECT target be reached.
        let _ = std::fs::write("/proc/sys/net/ipv4/conf/all/route_localnet", "1");
        let _ = std::fs::write("/proc/sys/net/ipv4/conf/lo/route_localnet", "1");
        let _ = Command::new("ip")
            .args(["route", "add", "default", "dev", "lo", "src", "127.0.0.1"])
            .status();

        let tcp = match TcpListener::bind("127.0.0.1:0") {
            Ok(l) => l,
            Err(e) => {
                eprintln!("tproxy: tcp bind: {e}");
                return 127;
            }
        };
        let udp = match UdpSocket::bind("127.0.0.1:0") {
            Ok(u) => u,
            Err(e) => {
                eprintln!("tproxy: udp bind: {e}");
                return 127;
            }
        };
        let tcp_port = tcp.local_addr().map(|a| a.port()).unwrap_or(0);
        let dns_port = udp.local_addr().map(|a| a.port()).unwrap_or(0);

        // Install the NAT redirect ruleset.
        let ruleset = nftables::build_nat_ruleset("agentd_sbxnat", tcp_port, dns_port);
        if !apply_nft(&ruleset) {
            eprintln!("tproxy: nft apply failed");
            return 127;
        }

        // Fork the command. SAFETY: this process is single-threaded (just
        // execve'd as the supervisor), so the child may run normal code before
        // its own execve. `fork` has no safe equivalent.
        let child = match unsafe { libc::fork() } {
            -1 => return 127,
            0 => exec_command(&cfg), // never returns
            pid => pid,
        };
        // Parent: close the inherited command-side fds.
        let _ = close(cfg.stdout_fd);
        let _ = close(cfg.stderr_fd);
        if cfg.stdin_fd >= 0 {
            let _ = close(cfg.stdin_fd);
        }

        // TCP intercept thread: pass each accepted fd + original dst to the host.
        let ctrl_fd = cfg.ctrl_fd;
        std::thread::spawn(move || {
            for stream in tcp.incoming().flatten() {
                let fd = stream.as_raw_fd();
                let dst = match original_dst(&stream) {
                    Some(d) => d,
                    None => continue,
                };
                let payload = encode_dst(&dst);
                let fds = [fd];
                let cmsg = [ControlMessage::ScmRights(&fds)];
                let iov = [std::io::IoSlice::new(&payload)];
                let _ = sendmsg::<()>(ctrl_fd, &iov, &cmsg, MsgFlags::empty(), None);
                drop(stream); // host owns the dup'd fd now
            }
        });

        // DNS intercept thread: bridge each query to the host, return its answer.
        let dns_fd = cfg.dns_fd;
        std::thread::spawn(move || {
            use nix::sys::socket::{MsgFlags, recv, send};
            let mut buf = [0u8; 1500];
            while let Ok((n, src)) = udp.recv_from(&mut buf) {
                // Forward query bytes to host over the dns socketpair.
                if send(dns_fd, &buf[..n], MsgFlags::empty()).is_err() {
                    break;
                }
                let mut rbuf = [0u8; 1500];
                match recv(dns_fd, &mut rbuf, MsgFlags::empty()) {
                    Ok(got) if got > 0 => {
                        let _ = udp.send_to(&rbuf[..got], src);
                    }
                    _ => continue,
                }
            }
        });

        match waitpid(Pid::from_raw(child), None) {
            Ok(WaitStatus::Exited(_, code)) => code,
            _ => 129,
        }
    }

    fn apply_nft(ruleset: &str) -> bool {
        let mut c = match Command::new("nft")
            .args(["-f", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(_) => return false,
        };
        if let Some(mut sin) = c.stdin.take() {
            let _ = sin.write_all(ruleset.as_bytes());
        }
        c.wait().map(|s| s.success()).unwrap_or(false)
    }

    fn exec_command(cfg: &TConfig) -> ! {
        // SAFETY: runs in the forked child before execve — async-signal-safe raw
        // syscalls only (dup2/close), then execvp. No safe wrapper is permitted
        // in this window.
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
            libc::close(cfg.ctrl_fd);
            libc::close(cfg.dns_fd);
        }

        // No proxy env: enforcement is transparent (kernel redirect + host allowlist).
        let policy = SandboxPolicy {
            read_paths: cfg.read_paths.iter().map(Into::into).collect(),
            write_paths: cfg.write_paths.iter().map(Into::into).collect(),
            allow_net: true,
            net_hosts: vec![],
            unrestricted: false,
        };
        if let Err(e) = crate::sandbox::apply(&policy) {
            eprintln!("tproxy: landlock apply failed: {e}");
            unsafe { libc::_exit(126) };
        }

        let bin = CString::new(cfg.bin.as_str()).unwrap();
        let mut argv: Vec<CString> = Vec::with_capacity(cfg.args.len() + 1);
        argv.push(bin.clone());
        for a in &cfg.args {
            argv.push(CString::new(a.as_str()).unwrap_or_default());
        }
        let _ = execvp(&bin, &argv);
        eprintln!("tproxy: exec {} failed", cfg.bin);
        unsafe { libc::_exit(127) }
    }
}

/// Run `req` in a transparent netns. `host_grants` are the `net:<host>` slugs
/// (literal IPs already split out into `literal_ips`).
pub async fn run_contained(
    req: &ExecRequest,
    policy: &SandboxPolicy,
    host_grants: Vec<agentd_permissions::Permission>,
    literal_ips: Vec<IpAddr>,
) -> Result<ExecResult, ShellError> {
    let sup_path = supervisor_path()
        .ok_or_else(|| ShellError::Sandbox("netns supervisor binary not found".into()))?;
    let req = req.clone();
    let policy = policy.clone();
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        run_contained_blocking(&req, &policy, &sup_path, host_grants, literal_ips, handle)
    })
    .await
    .map_err(|e| ShellError::Sandbox(format!("join: {e}")))?
}

fn socketpair(ty: SockType) -> std::io::Result<(RawFd, RawFd)> {
    use nix::sys::socket::{AddressFamily, SockFlag, socketpair};
    use std::os::fd::IntoRawFd;
    let (a, b) = socketpair(AddressFamily::Unix, ty, None, SockFlag::empty())?;
    Ok((a.into_raw_fd(), b.into_raw_fd()))
}

#[allow(clippy::too_many_arguments)]
fn run_contained_blocking(
    req: &ExecRequest,
    policy: &SandboxPolicy,
    sup_path: &str,
    host_grants: Vec<agentd_permissions::Permission>,
    literal_ips: Vec<IpAddr>,
    handle: tokio::runtime::Handle,
) -> Result<ExecResult, ShellError> {
    let sb = |e: String| ShellError::Sandbox(e);

    let (ctrl_host, ctrl_sup) = socketpair(SockType::Stream).map_err(|e| sb(e.to_string()))?;
    // SEQPACKET (not DGRAM): preserves DNS message boundaries AND signals EOF when
    // the supervisor end closes, so the host DNS loop unblocks on teardown.
    let (dns_host, dns_sup) = socketpair(SockType::SeqPacket).map_err(|e| sb(e.to_string()))?;
    set_cloexec(ctrl_host, true);
    set_cloexec(ctrl_sup, false);
    set_cloexec(dns_host, true);
    set_cloexec(dns_sup, false);

    let out = make_pipe().map_err(|e| sb(e.to_string()))?;
    let err = make_pipe().map_err(|e| sb(e.to_string()))?;
    let sin = make_pipe().map_err(|e| sb(e.to_string()))?;
    set_cloexec(out.wr, false);
    set_cloexec(err.wr, false);
    set_cloexec(sin.rd, false);

    let s1 = make_pipe().map_err(|e| sb(e.to_string()))?; // child->parent "unshared"
    let s2 = make_pipe().map_err(|e| sb(e.to_string()))?; // parent->child "maps written"

    let cfg = TConfig {
        ctrl_fd: ctrl_sup,
        dns_fd: dns_sup,
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
    let (argv, envp) = build_exec_args_tproxy(sup_path, &cfg_json);

    let uid = getuid().as_raw();
    let gid = getgid().as_raw();

    // SAFETY: `fork` has no safe equivalent. The child below does ONLY
    // async-signal-safe work (raw unshare/write/read/execve/_exit) — it must not
    // call allocating/locking wrappers, so raw `libc` here is required, not a
    // shortcut.
    let child = unsafe { libc::fork() };
    if child < 0 {
        return Err(sb("fork".into()));
    }
    if child == 0 {
        // SAFETY: async-signal-safe-only region in the forked child.
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
            libc::_exit(127);
        }
    }

    // Close the ends the child/supervisor owns (parent keeps its own).
    for fd in [ctrl_sup, dns_sup, out.wr, err.wr, sin.rd, s1.wr, s2.rd] {
        let _ = close(fd);
    }

    // Wait for the child's "unshared" signal.
    let mut tmp = [0u8; 1];
    if read(s1.rd, &mut tmp) != Ok(1) {
        let _ = close(s1.rd);
        let _ = close(s2.wr);
        return Err(sb("child failed to unshare".into()));
    }
    let _ = write_file(&format!("/proc/{child}/uid_map"), &format!("0 {uid} 1\n"));
    let _ = write_file(&format!("/proc/{child}/setgroups"), "deny");
    let _ = write_file(&format!("/proc/{child}/gid_map"), &format!("0 {gid} 1\n"));
    // Release the child into execve. SAFETY: `s2.wr` is a live fd we own; the
    // borrow lives only for this write.
    let _ = write(
        unsafe { std::os::fd::BorrowedFd::borrow_raw(s2.wr) },
        &[1u8],
    );
    let _ = close(s1.rd);
    let _ = close(s2.wr);

    // Shared permitted set, seeded with literal IP grants.
    let set: SharedSet = {
        let mut ps = PermitSet::new(SetConfig::default());
        for ip in &literal_ips {
            ps.allow_literal(*ip);
        }
        Arc::new(Mutex::new(ps))
    };

    // Host: ctrl receiver (TCP relay) + dns handler, on blocking threads.
    let set_ctrl = set.clone();
    let h2 = handle.clone();
    let ctrl_join = std::thread::spawn(move || ctrl_receiver_loop(ctrl_host, set_ctrl, h2));
    let set_dns = set.clone();
    let grants = host_grants;
    let dns_join = std::thread::spawn(move || dns_handler_loop(dns_host, grants, set_dns));

    // Drain stdio. SAFETY: the parent owns the read ends of these pipes; adopt
    // them into `File`s that close on drop.
    use std::io::Read;
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
    if let Some(input) = &req.stdin {
        // SAFETY: parent owns the write end of the stdin pipe; adopt it.
        let mut sf = unsafe { std::fs::File::from_raw_fd(sin.wr) };
        use std::io::Write;
        let _ = sf.write_all(input.as_bytes());
    } else {
        let _ = close(sin.wr);
    }

    let exit_code = match waitpid(Pid::from_raw(child), None) {
        Ok(WaitStatus::Exited(_, code)) => code,
        _ => -1,
    };

    let stdout = String::from_utf8_lossy(&out_join.join().unwrap_or_default()).into_owned();
    let stderr_text = String::from_utf8_lossy(&err_join.join().unwrap_or_default()).into_owned();
    let _ = ctrl_join.join();
    let _ = dns_join.join();

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

/// Like `linux_net::build_exec_args` but injects the `TPROXY_ENV` instead.
fn build_exec_args_tproxy(
    sup_path: &str,
    cfg_json: &str,
) -> (Vec<*const libc::c_char>, Vec<*const libc::c_char>) {
    let argv0 = CString::new(sup_path).unwrap();
    let mut argv: Vec<*const libc::c_char> = vec![argv0.as_ptr()];
    std::mem::forget(argv0);
    argv.push(std::ptr::null());

    let mut envp: Vec<*const libc::c_char> = Vec::new();
    for (k, v) in std::env::vars() {
        if k == TPROXY_ENV {
            continue;
        }
        let e = CString::new(format!("{k}={v}")).unwrap_or_default();
        envp.push(e.as_ptr());
        std::mem::forget(e);
    }
    let sup_env = CString::new(format!("{TPROXY_ENV}={cfg_json}")).unwrap();
    envp.push(sup_env.as_ptr());
    std::mem::forget(sup_env);
    envp.push(std::ptr::null());
    (argv, envp)
}

/// Receive intercepted TCP fds + original dst; relay each to the real dst iff
/// admitted by the permitted set.
fn ctrl_receiver_loop(ctrl_host: RawFd, set: SharedSet, handle: tokio::runtime::Handle) {
    use nix::sys::socket::{ControlMessageOwned, MsgFlags, recvmsg};
    loop {
        let mut buf = [0u8; 19];
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
            break;
        }
        let mut got_fd = None;
        if let Ok(cmsgs) = msg.cmsgs() {
            for c in cmsgs {
                if let ControlMessageOwned::ScmRights(fds) = c
                    && let Some(&fd) = fds.first()
                {
                    got_fd = Some(fd);
                }
            }
        }
        let (Some(fd), Some(dst)) = (got_fd, decode_dst(&buf)) else {
            continue;
        };
        if admit(dst.ip(), &set) {
            handle.spawn(relay_to_dst(fd, dst));
        } else {
            let _ = close(fd);
        }
    }
    let _ = close(ctrl_host);
}

/// Splice an intercepted child socket fd to a fresh connection to its real dst.
async fn relay_to_dst(fd: RawFd, dst: SocketAddr) {
    use tokio::net::TcpStream;
    // SAFETY: `fd` was received via SCM_RIGHTS and is owned by us; adopt it.
    let std_sock = unsafe { std::net::TcpStream::from_raw_fd(fd) };
    if std_sock.set_nonblocking(true).is_err() {
        return;
    }
    let mut child = match TcpStream::from_std(std_sock) {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut up = match TcpStream::connect(dst).await {
        Ok(s) => s,
        Err(_) => return,
    };
    let _ = tokio::io::copy_bidirectional(&mut child, &mut up).await;
}

/// Serve DNS queries bridged from the netns: resolve allowed names, commit IPs to
/// the set, answer; NXDOMAIN otherwise.
fn dns_handler_loop(
    dns_host: RawFd,
    host_grants: Vec<agentd_permissions::Permission>,
    set: SharedSet,
) {
    use nix::sys::socket::{MsgFlags, recv, send};
    let resolver = crate::dns_pin::SystemResolver;
    let mut buf = [0u8; 1500];
    loop {
        let n = match recv(dns_host, &mut buf, MsgFlags::empty()) {
            Ok(n) if n > 0 => n,
            _ => break,
        };
        let query = &buf[..n];
        // Always reply (even empty on malformed) so the supervisor's paired recv
        // never blocks forever and stalls subsequent queries.
        let resp = handle_dns(query, &host_grants, &set, &resolver, PIN_TTL).unwrap_or_default();
        let _ = send(dns_host, &resp, MsgFlags::empty());
    }
    let _ = close(dns_host);
}
