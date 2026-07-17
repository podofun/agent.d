//! macOS transparent host/IP-granular network backend — the daemon (client)
//! half. Semantically identical to the Linux netns backend: the SAME
//! [`crate::gateway`] DNS-pin + [`PermitSet`] core decides which IPs the child
//! may reach; only the capture mechanism differs.
//!
//! Flow:
//! 1. Connect the root `agentd-pf-broker`, lease a sandbox uid.
//! 2. Bind a dual-stack TCP relay + UDP DNS server on loopback; tell the broker
//!    to `provision` a pf anchor redirecting the leased uid's traffic to those
//!    ports (`route-to lo0` + `rdr`), and to stamp per-uid fs ACLs.
//! 3. Ask the broker to `spawn` the child as that uid under a Seatbelt profile
//!    that allows outbound; receive the child's stdio fds over `SCM_RIGHTS`.
//! 4. Relay: for each intercepted connection, ask the broker for the original
//!    destination (`DIOCNATLOOK`), admit iff the shared [`PermitSet`] allows it,
//!    then splice to the real dst. DNS: resolve allowed names, commit their IPs
//!    to the set BEFORE answering — the exact ordering Linux uses.
//!
//! Enforcement never runs in the daemon with elevated privilege: every root
//! operation is a narrow broker verb.

#![cfg(target_os = "macos")]

use std::io::Read;
use std::net::{IpAddr, SocketAddr};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentd_permissions::Permission;

use super::macos_broker::SOCKET_PATH;
use super::macos_broker::proto::{ErrKind, Proto, Req, Resp, read_msg, write_msg};
use crate::gateway::{SharedSet, admit, handle_dns};
use crate::netfilter::{PermitSet, SetConfig};
use crate::{ExecRequest, ExecResult, SandboxPolicy, ShellError};

const PIN_TTL: Duration = Duration::from_secs(120);

/// Whether the broker socket is present and answers `Ping`. Used by
/// `net_supported()` to fail closed when the broker isn't installed/running.
pub fn broker_available() -> bool {
    let Ok(mut s) = UnixStream::connect(SOCKET_PATH) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    if write_msg(&mut s, &Req::Ping).is_err() {
        return false;
    }
    matches!(read_msg::<_, Resp>(&mut s), Ok(Resp::Ok))
}

/// Serialized broker connection: setup verbs run once, then `natlook`/`wait`
/// are request/response under the mutex (concurrent relay connections natlook
/// through the same channel).
struct Broker {
    stream: Mutex<UnixStream>,
}

impl Broker {
    fn connect() -> Result<Self, ShellError> {
        let stream = UnixStream::connect(SOCKET_PATH)
            .map_err(|e| ShellError::Sandbox(format!("broker connect: {e}")))?;
        Ok(Broker {
            stream: Mutex::new(stream),
        })
    }

    fn call(&self, req: &Req) -> Result<Resp, ShellError> {
        let mut s = self.stream.lock().unwrap();
        write_msg(&mut *s, req).map_err(|e| ShellError::Sandbox(format!("broker send: {e}")))?;
        read_msg::<_, Resp>(&mut *s).map_err(|e| ShellError::Sandbox(format!("broker recv: {e}")))
    }

    fn expect_ok(&self, req: &Req) -> Result<(), ShellError> {
        match self.call(req)? {
            Resp::Ok => Ok(()),
            Resp::Err { kind, msg } => Err(broker_error(kind, msg)),
            _ => Err(ShellError::Sandbox(
                "unexpected broker reply during call".to_string(),
            )),
        }
    }
}

fn broker_error(kind: ErrKind, msg: String) -> ShellError {
    match kind {
        ErrKind::PoolExhausted => {
            ShellError::Sandbox("all sandbox slots in use; retry shortly".into())
        }
        _ => ShellError::Sandbox(format!("broker: {msg}")),
    }
}

/// Bind a dual-stack loopback listener (v4 + v6 on one port) so the pf `rdr`
/// rules — which target `127.0.0.1:port` for v4 and `[::1]:port` for v6 — both
/// land here. Without dual-stack, a granted v6 host would fail on macOS while
/// working on Linux: a hidden platform difference.
fn bind_dual_tcp() -> std::io::Result<std::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let sock = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
    sock.set_only_v6(false)?;
    sock.set_reuse_address(true)?;
    sock.bind(&SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, 0)).into())?;
    sock.listen(128)?;
    // tokio refuses to register a blocking fd; make it non-blocking up front.
    sock.set_nonblocking(true)?;
    Ok(sock.into())
}

fn bind_dual_udp() -> std::io::Result<std::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let sock = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_only_v6(false)?;
    sock.bind(&SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, 0)).into())?;
    sock.set_nonblocking(true)?;
    Ok(sock.into())
}

/// Serializes macOS net execs. pf translation (`rdr`) rules cannot match by
/// user, so two sessions' redirect rules both match a child's packet and pf
/// takes the first — one uid's traffic could reach the other's relay (wrong
/// permit set). Holding this for a session's lifetime guarantees only one set
/// of `agentd/*` rdr rules is ever active, so each child reaches exactly its
/// own relay. This bounds macOS to one sandboxed-network command at a time; the
/// access decision is unaffected (it is a throughput limit, not a policy one).
fn net_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Run `req` under the transparent pf-broker sandbox. Same signature as the
/// Linux backend so `lib.rs` dispatch is symmetric.
pub async fn run_contained(
    req: &ExecRequest,
    policy: &SandboxPolicy,
    host_grants: Vec<Permission>,
    literal_ips: Vec<IpAddr>,
) -> Result<ExecResult, ShellError> {
    let _serialize = net_lock().lock().await;
    let broker = Arc::new(Broker::connect()?);

    // Relay + DNS listeners (std sockets; converted to tokio below).
    let tcp = bind_dual_tcp().map_err(|e| ShellError::Sandbox(format!("relay bind: {e}")))?;
    let udp = bind_dual_udp().map_err(|e| ShellError::Sandbox(format!("dns bind: {e}")))?;
    let tcp_port = tcp.local_addr().map_err(io_sb)?.port();
    let dns_port = udp.local_addr().map_err(io_sb)?.port();

    // Lease → provision anchor → stamp ACLs.
    match broker.call(&Req::Lease { v: 1 })? {
        Resp::Leased { .. } => {}
        Resp::Err { kind, msg } => return Err(broker_error(kind, msg)),
        _ => {
            return Err(ShellError::Sandbox(
                "unexpected broker reply during lease".to_string(),
            ));
        }
    }
    broker.expect_ok(&Req::Provision { tcp_port, dns_port })?;
    let read: Vec<String> = policy
        .read_paths
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let write: Vec<String> = policy
        .write_paths
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    broker.expect_ok(&Req::Acl { read, write })?;

    // Spawn the child under Seatbelt (fs confinement + outbound allowed); pf +
    // the relay enforce the network policy.
    let sbpl = super::backend::sbpl_net_for_broker(policy);
    let want_stdin = req.stdin.is_some();
    let pid = {
        let mut s = broker.stream.lock().unwrap();
        write_msg(
            &mut *s,
            &Req::Spawn {
                bin: req.bin.clone(),
                args: req.args.clone(),
                cwd: req.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
                sbpl,
                want_stdin,
            },
        )
        .map_err(|e| ShellError::Sandbox(format!("spawn send: {e}")))?;
        let reply = read_msg::<_, Resp>(&mut *s)
            .map_err(|e| ShellError::Sandbox(format!("spawn reply: {e}")))?;
        let pid = match reply {
            Resp::Spawned { pid } => pid,
            Resp::Err { kind, msg } => return Err(broker_error(kind, msg)),
            _ => {
                return Err(ShellError::Sandbox(
                    "unexpected broker reply during spawn".to_string(),
                ));
            }
        };
        // Receive the child's stdio fds over SCM_RIGHTS on the same socket.
        let fds = recv_fds(&s, if want_stdin { 3 } else { 2 })
            .map_err(|e| ShellError::Sandbox(format!("recv stdio fds: {e}")))?;
        drop(s);
        // Order matches the broker's send: [stdin_wr?, stdout_rd, stderr_rd].
        let mut it = fds.into_iter();
        let stdin_wr = if want_stdin {
            Some(it.next().unwrap())
        } else {
            None
        };
        let stdout_rd = it.next().unwrap();
        let stderr_rd = it.next().unwrap();
        Some((pid, stdin_wr, stdout_rd, stderr_rd))
    };
    let (_pid, stdin_wr, stdout_rd, stderr_rd) = pid.unwrap();

    // Shared permitted set, seeded with literal IP grants (identical to Linux).
    let set: SharedSet = {
        let mut ps = PermitSet::new(SetConfig::default());
        for ip in &literal_ips {
            ps.allow_literal(*ip);
        }
        Arc::new(Mutex::new(ps))
    };

    // DNS server task.
    let dns_set = set.clone();
    let dns_grants = host_grants.clone();
    let udp = tokio::net::UdpSocket::from_std(udp).map_err(io_sb)?;
    let dns_task = tokio::spawn(async move { dns_loop(udp, dns_grants, dns_set).await });

    // TCP relay task.
    let relay_set = set.clone();
    let relay_broker = broker.clone();
    let relay_grants = host_grants.clone();
    let tcp = tokio::net::TcpListener::from_std(tcp).map_err(io_sb)?;
    let relay_task =
        tokio::spawn(async move { relay_loop(tcp, relay_broker, relay_set, relay_grants).await });

    // Feed stdin (if any), drain stdout/stderr to completion (EOF = child done).
    if let (Some(input), Some(fd)) = (&req.stdin, stdin_wr) {
        // SAFETY: `fd` is the write end of the child's stdin, received from the
        // broker via SCM_RIGHTS and owned by us. Adopting it into a File gives
        // RAII close (drop shuts the child's stdin, signalling EOF). No safe
        // constructor exists for an fd handed over out-of-band.
        let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
        use std::io::Write;
        let _ = f.write_all(input.as_bytes());
    } else if let Some(fd) = stdin_wr {
        // SAFETY: close(2) on the stdin fd we own but won't write to; closing
        // signals immediate EOF to the child. Raw fd (no owning wrapper) → libc.
        unsafe { libc::close(fd) };
    }
    let out_task = tokio::task::spawn_blocking(move || read_fd_to_end(stdout_rd));
    let err_task = tokio::task::spawn_blocking(move || read_fd_to_end(stderr_rd));
    let stdout = String::from_utf8_lossy(&out_task.await.unwrap_or_default()).into_owned();
    let stderr_text = String::from_utf8_lossy(&err_task.await.unwrap_or_default()).into_owned();

    // Child's stdio closed → reap it via the broker for the exit code.
    let exit_code = match broker.call(&Req::Wait)? {
        Resp::Exit { code } => code,
        Resp::Err { kind, msg } => return Err(broker_error(kind, msg)),
        _ => {
            return Err(ShellError::Sandbox(
                "unexpected broker reply during wait".to_string(),
            ));
        }
    };

    // Dropping the broker connection triggers broker-side teardown (flush
    // anchor, remove ACLs, release uid). Stop the relay/DNS tasks.
    relay_task.abort();
    dns_task.abort();
    drop(broker);

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

fn io_sb(e: std::io::Error) -> ShellError {
    ShellError::Sandbox(e.to_string())
}

/// Collapse an IPv4-mapped IPv6 socket address (`::ffff:a.b.c.d`) to real v4,
/// so pf natlook and upstream connect use the family pf actually tracked.
fn demap(a: SocketAddr) -> SocketAddr {
    match a {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::new(IpAddr::V4(v4), v6.port()),
            None => a,
        },
        v4 => v4,
    }
}

fn read_fd_to_end(fd: RawFd) -> Vec<u8> {
    // SAFETY: `fd` is a stdout/stderr read end received from the broker via
    // SCM_RIGHTS and owned here; adopting it into a File gives buffered reads
    // and RAII close. No safe constructor exists for an out-of-band fd.
    let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
    let mut v = Vec::new();
    let _ = f.read_to_end(&mut v);
    v
}

/// Receive `n` file descriptors via `SCM_RIGHTS` (plus the broker's dummy byte).
fn recv_fds(stream: &UnixStream, n: usize) -> std::io::Result<Vec<RawFd>> {
    use nix::sys::socket::{ControlMessageOwned, MsgFlags, recvmsg};
    use std::os::fd::AsRawFd;
    let mut buf = [0u8; 1];
    let mut iov = [std::io::IoSliceMut::new(&mut buf)];
    let mut cmsg_space = nix::cmsg_space!([RawFd; 8]);
    let msg = recvmsg::<()>(
        stream.as_raw_fd(),
        &mut iov,
        Some(&mut cmsg_space),
        MsgFlags::empty(),
    )
    .map_err(|e| std::io::Error::other(e.to_string()))?;
    let mut fds = Vec::new();
    if let Ok(cmsgs) = msg.cmsgs() {
        for c in cmsgs {
            if let ControlMessageOwned::ScmRights(got) = c {
                fds.extend(got);
            }
        }
    }
    if fds.len() != n {
        for fd in &fds {
            // SAFETY: close(2) on fds we just received and own; on the
            // wrong-count error path we drop them all to avoid leaking.
            unsafe { libc::close(*fd) };
        }
        return Err(std::io::Error::other(format!(
            "expected {n} fds, got {}",
            fds.len()
        )));
    }
    Ok(fds)
}

/// Whether a connection to `ip` is permitted. First checks the pinned set
/// (literal grants + IPs pinned by our intercepted DNS). On a miss, resolves
/// the concrete allowed host grants LIVE and admits+pins `ip` if it backs one
/// of them.
///
/// This reactive path is what makes name grants work on macOS: unlike Linux —
/// where the child's `getaddrinfo` hits our DNS server directly — macOS routes
/// resolution through the shared mDNSResponder, out of our per-uid pf redirect,
/// so we never see the query. Resolving the grant ourselves at connect time
/// gives the same host-granular, live decision. (Wildcard grants can't be
/// enumerated this way — they still rely on intercepting a direct DNS query.)
async fn admit_dst(ip: IpAddr, set: &SharedSet, host_grants: &[Permission]) -> bool {
    if admit(ip, set) {
        return true;
    }
    let grants: Vec<Permission> = host_grants.to_vec();
    let matched = tokio::task::spawn_blocking(move || resolve_admit(ip, &grants))
        .await
        .unwrap_or(false);
    if matched {
        set.lock()
            .unwrap()
            .allow_resolved(ip, PIN_TTL, Instant::now());
    }
    matched
}

/// Blocking admit decision for an unpinned IP, run off the async executor.
///
/// - Concrete `net:<host>` grant: resolve it and admit if `ip` is one of its
///   addresses (exact, live — same decision Linux makes at DNS-pin time).
/// - Wildcard `net:<prefix>*` grant: reverse-resolve `ip` to its name(s) and,
///   for any name the wildcard covers, forward-confirm that the name still
///   resolves back to `ip` before admitting. This covers hosts with correct
///   reverse DNS (the common case); hosts without matching PTR records are the
///   one place macOS is less capable than Linux's direct DNS interception.
fn resolve_admit(ip: IpAddr, host_grants: &[Permission]) -> bool {
    use crate::dns_pin::{Resolve, SystemResolver, name_allowed};
    let resolver = SystemResolver;
    let mut has_wildcard = false;
    for g in host_grants {
        if let ("net", Some(name)) = g.parts() {
            if name.contains('*') {
                has_wildcard = true;
                continue;
            }
            if let Ok(ips) = resolver.resolve(name)
                && ips.contains(&ip)
            {
                return true;
            }
        }
    }
    if has_wildcard {
        for name in reverse_lookup(ip) {
            // Forward-confirm: the reverse name must be covered by a grant AND
            // resolve back to this IP, so a forged PTR cannot widen access.
            if name_allowed(host_grants, &name)
                && resolver
                    .resolve(&name)
                    .map(|ips| ips.contains(&ip))
                    .unwrap_or(false)
            {
                return true;
            }
        }
    }
    false
}

/// Reverse-resolve `ip` to hostname(s) via `getnameinfo`. Empty on failure.
fn reverse_lookup(ip: IpAddr) -> Vec<String> {
    use std::ffi::CStr;
    let mut host = [0 as libc::c_char; libc::NI_MAXHOST as usize];
    let (sa, len): (libc::sockaddr_storage, libc::socklen_t) = match ip {
        IpAddr::V4(v4) => {
            let mut s: libc::sockaddr_in = unsafe { std::mem::zeroed() };
            s.sin_family = libc::AF_INET as libc::sa_family_t;
            s.sin_addr.s_addr = u32::from_ne_bytes(v4.octets());
            // SAFETY: transmuting a fully-initialized sockaddr_in into the
            // storage union; sockaddr_storage is larger and this is the standard
            // libc idiom (no safe wrapper for getnameinfo input).
            let mut st: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
            unsafe {
                std::ptr::copy_nonoverlapping(
                    &s as *const _ as *const u8,
                    &mut st as *mut _ as *mut u8,
                    std::mem::size_of::<libc::sockaddr_in>(),
                )
            };
            (
                st,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        }
        IpAddr::V6(v6) => {
            let mut s: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
            s.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            s.sin6_addr.s6_addr = v6.octets();
            let mut st: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
            unsafe {
                std::ptr::copy_nonoverlapping(
                    &s as *const _ as *const u8,
                    &mut st as *mut _ as *mut u8,
                    std::mem::size_of::<libc::sockaddr_in6>(),
                )
            };
            (
                st,
                std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
            )
        }
    };
    // SAFETY: getnameinfo reads `len` bytes of the sockaddr we just initialized
    // and writes a NUL-terminated name into `host` (NI_MAXHOST bytes). All
    // pointers/lengths are valid for the call; there is no safe std wrapper for
    // reverse DNS.
    let rc = unsafe {
        libc::getnameinfo(
            &sa as *const _ as *const libc::sockaddr,
            len,
            host.as_mut_ptr(),
            host.len() as libc::socklen_t,
            std::ptr::null_mut(),
            0,
            libc::NI_NAMEREQD,
        )
    };
    if rc != 0 {
        return Vec::new();
    }
    // SAFETY: on success getnameinfo NUL-terminated `host`.
    let name = unsafe { CStr::from_ptr(host.as_ptr()) }
        .to_string_lossy()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if name.is_empty() {
        Vec::new()
    } else {
        vec![name]
    }
}

/// Accept intercepted connections; natlook each via the broker, admit iff the
/// permitted set allows the original destination, then splice.
async fn relay_loop(
    listener: tokio::net::TcpListener,
    broker: Arc<Broker>,
    set: SharedSet,
    host_grants: Vec<Permission>,
) {
    loop {
        let (inbound, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(_) => break,
        };
        let local = match inbound.local_addr() {
            Ok(a) => a,
            Err(_) => continue,
        };
        let broker = broker.clone();
        let set = set.clone();
        // A dual-stack listener reports v4 connections as IPv4-mapped v6
        // (`::ffff:a.b.c.d`). pf's state for those is a real AF_INET entry, so
        // demap before natlook or DIOCNATLOOK queries the wrong family and misses.
        let peer = demap(peer);
        let local = demap(local);
        let grants = host_grants.clone();
        tokio::spawn(async move {
            let orig = match natlook(&broker, peer, local).await {
                Some(d) => d,
                None => return, // no original dst → drop
            };
            if !admit_dst(orig.ip(), &set, &grants).await {
                return; // not granted → drop (fail closed)
            }
            let mut inbound = inbound;
            let mut upstream = match tokio::net::TcpStream::connect(orig).await {
                Ok(s) => s,
                Err(_) => return,
            };
            let _ = tokio::io::copy_bidirectional(&mut inbound, &mut upstream).await;
        });
    }
}

/// Natlook via the broker (blocking request/response, run off the async
/// executor to avoid stalling other tasks under the connection mutex).
async fn natlook(broker: &Arc<Broker>, peer: SocketAddr, local: SocketAddr) -> Option<SocketAddr> {
    let broker = broker.clone();
    tokio::task::spawn_blocking(move || {
        match broker.call(&Req::Natlook {
            proto: Proto::Tcp,
            src: peer.to_string(),
            dst: local.to_string(),
        }) {
            Ok(Resp::NatlookResult { orig }) => orig.parse().ok(),
            _ => None,
        }
    })
    .await
    .ok()
    .flatten()
}

/// Serve DNS queries from the child: resolve allowed names, commit IPs to the
/// set BEFORE answering, NXDOMAIN otherwise. Identical policy to Linux.
async fn dns_loop(udp: tokio::net::UdpSocket, host_grants: Vec<Permission>, set: SharedSet) {
    let resolver = crate::dns_pin::SystemResolver;
    let mut buf = [0u8; 1500];
    loop {
        let (n, src) = match udp.recv_from(&mut buf).await {
            Ok(x) => x,
            Err(_) => break,
        };
        let resp =
            handle_dns(&buf[..n], &host_grants, &set, &resolver, PIN_TTL).unwrap_or_default();
        if !resp.is_empty() {
            let _ = udp.send_to(&resp, src).await;
        }
    }
}
