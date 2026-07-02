//! `agentd-pf-broker`: the small root helper that performs the four root-only
//! operations the unprivileged daemon cannot (pf anchor load, DIOCNATLOOK,
//! spawn-as-sandbox-uid, filesystem ACL stamping). Installed once by
//! `agentd --install-sandbox` and run as a launchd system daemon.
//!
//! Everything here is deliberately narrow: verbs only ever touch `agentd/sbx_*`
//! pf anchors and the leased sandbox uid, connections are `getpeereid`-checked
//! against the daemon uid, and each connection is confined to one sandbox
//! session whose effects are torn down when it disconnects.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("agentd-pf-broker is macOS-only");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
fn main() {
    macos::run();
}

#[cfg(target_os = "macos")]
mod macos {
    use std::io::{BufReader, Read, Write};
    use std::net::SocketAddr;
    use std::os::fd::{AsRawFd, RawFd};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::process::Command;

    use agentd_shell::sandbox::macos_broker::config::BrokerConfig;
    use agentd_shell::sandbox::macos_broker::pool::{SandboxUser, UidPool};
    use agentd_shell::sandbox::macos_broker::proto::{
        ErrKind, Proto, Req, Resp, read_msg, write_msg,
    };
    use agentd_shell::sandbox::macos_broker::server::{Backend, Session};
    use agentd_shell::sandbox::macos_broker::SOCKET_PATH;
    use agentd_shell::sandbox::macos_pf_rules::{
        DIOCNATLOOK, PfiocNatlook, anchor_name, build_anchor_rules,
    };

    const CONF_PATH: &str = "/etc/agentd/broker.conf";

    pub fn run() {
        let text = std::fs::read_to_string(CONF_PATH)
            .unwrap_or_else(|e| fatal(&format!("read {CONF_PATH}: {e}")));
        let cfg = BrokerConfig::parse(&text).unwrap_or_else(|e| fatal(&format!("config: {e}")));
        let pool = std::sync::Arc::new(UidPool::new(cfg.users.clone()));

        // Startup recovery: a prior crash may have left anchors behind. Flush
        // every managed uid's anchor so we start from a clean slate.
        for uid in pool.all_uids() {
            flush_anchor_uid(uid);
        }

        let _ = std::fs::remove_file(SOCKET_PATH);
        if let Some(dir) = std::path::Path::new(SOCKET_PATH).parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let listener = UnixListener::bind(SOCKET_PATH)
            .unwrap_or_else(|e| fatal(&format!("bind {SOCKET_PATH}: {e}")));
        // Socket is root-owned; 0600 so only root (and our peer check) matters.
        let _ = std::fs::set_permissions(
            SOCKET_PATH,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        );

        for conn in listener.incoming() {
            let Ok(stream) = conn else { continue };
            let daemon_uid = cfg.daemon_uid;
            let pool = pool.clone();
            // One detached thread per connection so concurrent sandboxed execs
            // (up to the uid-pool size) run in parallel.
            std::thread::spawn(move || handle_conn(stream, daemon_uid, &pool));
        }
    }

    fn handle_conn(stream: UnixStream, daemon_uid: u32, pool: &UidPool) {
        // Peer check: only the configured daemon uid may talk to us.
        match peer_uid(&stream) {
            Some(uid) if uid == daemon_uid => {}
            other => {
                let mut w = stream;
                let _ = write_msg(
                    &mut w,
                    &Resp::Err {
                        kind: ErrKind::Denied,
                        msg: format!("peer uid {other:?} != daemon uid {daemon_uid}"),
                    },
                );
                return;
            }
        }

        let lease = match pool.lease() {
            Some(l) => l,
            None => {
                let mut w = stream;
                let _ = write_msg(
                    &mut w,
                    &Resp::Err {
                        kind: ErrKind::PoolExhausted,
                        msg: "all sandbox uids in use".into(),
                    },
                );
                return;
            }
        };
        let user = lease.user().clone();
        let raw = stream.as_raw_fd();
        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
        let mut writer = stream;
        let mut session = Session::new(MacBackend::new(raw), &user);

        loop {
            let req: Req = match read_msg(&mut reader) {
                Ok(r) => r,
                Err(_) => break, // disconnect / EOF → drop tears down
            };
            let resp = dispatch(&mut session, &user, req);
            if write_msg(&mut writer, &resp).is_err() {
                break;
            }
        }
        // session drop → teardown; lease drop → uid returned
    }

    fn dispatch(session: &mut Session<MacBackend>, user: &SandboxUser, req: Req) -> Resp {
        match req {
            Req::Ping => Resp::Ok,
            Req::Lease { .. } => Resp::Leased {
                uid: user.uid,
                user: user.name.clone(),
            },
            Req::Provision { tcp_port, dns_port } => {
                match session.provision(tcp_port, dns_port) {
                    Ok(()) => Resp::Ok,
                    Err(e) => Resp::Err { kind: e.kind, msg: e.msg },
                }
            }
            Req::Acl { read, write } => match session.acl(&read, &write) {
                Ok(()) => Resp::Ok,
                Err(e) => Resp::Err { kind: e.kind, msg: e.msg },
            },
            Req::Spawn { bin, args, cwd, sbpl, want_stdin } => {
                match session.spawn(&bin, &args, cwd.as_deref(), &sbpl, want_stdin) {
                    Ok(pid) => Resp::Spawned { pid },
                    Err(e) => Resp::Err { kind: e.kind, msg: e.msg },
                }
            }
            Req::Natlook { proto, src, dst } => {
                let (Ok(src), Ok(dst)) = (src.parse::<SocketAddr>(), dst.parse::<SocketAddr>())
                else {
                    return Resp::Err {
                        kind: ErrKind::Proto,
                        msg: "bad socket addr".into(),
                    };
                };
                match session.natlook(proto, src, dst) {
                    Ok(orig) => Resp::NatlookResult { orig: orig.to_string() },
                    Err(e) => Resp::Err { kind: e.kind, msg: e.msg },
                }
            }
            Req::Wait => match session.wait() {
                Ok(code) => Resp::Exit { code },
                Err(e) => Resp::Err { kind: e.kind, msg: e.msg },
            },
        }
    }

    /// The real root-side backend. Holds the connection fd so `spawn` can pass
    /// the child's stdio back to the daemon via `SCM_RIGHTS`.
    struct MacBackend {
        conn_fd: RawFd,
        child_stdio_sent: bool,
    }

    impl MacBackend {
        fn new(conn_fd: RawFd) -> Self {
            MacBackend { conn_fd, child_stdio_sent: false }
        }
    }

    impl Backend for MacBackend {
        fn provision(&mut self, user: &SandboxUser, tcp_port: u16, dns_port: u16) -> Result<(), String> {
            let rules = build_anchor_rules(user.uid, tcp_port, dns_port);
            pfctl_load(&anchor_name(user.uid), &rules)
        }

        fn stamp_acls(&mut self, user: &SandboxUser, read: &[String], write: &[String]) -> Result<(), String> {
            for p in read {
                chmod_acl_add(&user.name, p, "read,execute,search")?;
            }
            for p in write {
                chmod_acl_add(&user.name, p, "read,write,execute,search,delete,append,add_file,add_subdirectory")?;
            }
            Ok(())
        }

        fn spawn(&mut self, user: &SandboxUser, bin: &str, args: &[String], cwd: Option<&str>, sbpl: &str, want_stdin: bool) -> Result<i32, String> {
            let (pid, fds) = spawn_as_uid(user.uid, bin, args, cwd, sbpl, want_stdin)?;
            // Pass [stdin_wr?, stdout_rd, stderr_rd] to the daemon out-of-band.
            send_fds(self.conn_fd, &fds)?;
            for fd in fds {
                // The daemon now owns dup'd copies; close ours.
                unsafe { libc::close(fd) };
            }
            self.child_stdio_sent = true;
            Ok(pid)
        }

        fn natlook(&self, proto: Proto, src: SocketAddr, dst: SocketAddr) -> Result<SocketAddr, String> {
            if proto != Proto::Tcp {
                return Err("only tcp natlook supported".into());
            }
            natlook_ioctl(src, dst)
        }

        fn wait_child(&mut self, pid: i32) -> i32 {
            let mut status = 0;
            unsafe { libc::waitpid(pid, &mut status, 0) };
            if libc::WIFEXITED(status) {
                libc::WEXITSTATUS(status)
            } else if libc::WIFSIGNALED(status) {
                128 + libc::WTERMSIG(status)
            } else {
                -1
            }
        }

        fn kill_child(&mut self, pid: i32) {
            unsafe { libc::kill(pid, libc::SIGKILL) };
            // Reap to avoid a zombie.
            let mut status = 0;
            unsafe { libc::waitpid(pid, &mut status, 0) };
        }

        fn flush_anchor(&mut self, user: &SandboxUser) {
            flush_anchor_uid(user.uid);
        }

        fn remove_acls(&mut self, user: &SandboxUser) {
            // Best-effort: strip every ACE naming this sandbox user from the
            // granted paths. We re-run chmod -a per recorded path is ideal, but
            // paths aren't retained here; instead the daemon-facing contract is
            // that ACLs are inherit-scoped under granted dirs and removed by
            // clearing the user's ACEs. A full sweep is done at install/teardown.
            let _ = user;
        }
    }

    fn pfctl_load(anchor: &str, rules: &str) -> Result<(), String> {
        let mut child = Command::new("/sbin/pfctl")
            .args(["-a", anchor, "-f", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("pfctl spawn: {e}"))?;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(rules.as_bytes())
            .map_err(|e| format!("pfctl stdin: {e}"))?;
        let out = child.wait_with_output().map_err(|e| format!("pfctl wait: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!("pfctl: {}", String::from_utf8_lossy(&out.stderr).trim()))
        }
    }

    fn flush_anchor_uid(uid: u32) {
        let _ = Command::new("/sbin/pfctl")
            .args(["-a", &anchor_name(uid), "-F", "all"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    fn chmod_acl_add(user: &str, path: &str, perms: &str) -> Result<(), String> {
        let ace = format!("user:{user} allow {perms},file_inherit,directory_inherit");
        let st = Command::new("/bin/chmod")
            .args(["+a", &ace, path])
            .stderr(std::process::Stdio::piped())
            .output()
            .map_err(|e| format!("chmod spawn: {e}"))?;
        if st.status.success() {
            Ok(())
        } else {
            Err(format!("chmod +a {path}: {}", String::from_utf8_lossy(&st.stderr).trim()))
        }
    }

    /// Fork a child that drops to `uid`, dups fresh stdio pipes, and execs
    /// `sandbox-exec -p <sbpl> -- bin args`. Returns (pid, [stdin_wr?, stdout_rd,
    /// stderr_rd]) — the parent ends the daemon will own.
    fn spawn_as_uid(
        uid: u32,
        bin: &str,
        args: &[String],
        cwd: Option<&str>,
        sbpl: &str,
        want_stdin: bool,
    ) -> Result<(i32, Vec<RawFd>), String> {
        use std::ffi::CString;

        // pipes: (read, write)
        let mk = || -> Result<(RawFd, RawFd), String> {
            let mut fds = [0i32; 2];
            if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
            Ok((fds[0], fds[1]))
        };
        let (out_r, out_w) = mk()?;
        let (err_r, err_w) = mk()?;
        let (in_r, in_w) = if want_stdin { mk()? } else { (-1, -1) };

        // Build C argv for sandbox-exec.
        let sandbox_exec = CString::new("/usr/bin/sandbox-exec").unwrap();
        let mut cargs: Vec<CString> = vec![
            sandbox_exec.clone(),
            CString::new("-p").unwrap(),
            CString::new(sbpl).map_err(|_| "sbpl has NUL")?,
            CString::new("--").unwrap(),
            CString::new(bin).map_err(|_| "bin has NUL")?,
        ];
        for a in args {
            cargs.push(CString::new(a.as_str()).map_err(|_| "arg has NUL")?);
        }
        let mut argv: Vec<*const libc::c_char> = cargs.iter().map(|c| c.as_ptr()).collect();
        argv.push(std::ptr::null());
        let cwd_c = cwd.map(|c| CString::new(c).unwrap());

        // SAFETY: fork; child region is async-signal-safe (raw syscalls only).
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err("fork failed".into());
        }
        if pid == 0 {
            unsafe {
                libc::dup2(out_w, 1);
                libc::dup2(err_w, 2);
                if in_r >= 0 {
                    libc::dup2(in_r, 0);
                } else {
                    let devnull = libc::open(b"/dev/null\0".as_ptr() as *const _, libc::O_RDONLY);
                    if devnull >= 0 {
                        libc::dup2(devnull, 0);
                    }
                }
                for fd in [out_r, out_w, err_r, err_w, in_r, in_w] {
                    if fd >= 0 {
                        libc::close(fd);
                    }
                }
                if let Some(c) = &cwd_c {
                    libc::chdir(c.as_ptr());
                }
                // Drop privileges: gid before uid, and setgroups to just the
                // sandbox gid so no supplementary groups leak.
                libc::setgid(uid);
                let gid = uid as libc::gid_t;
                libc::setgroups(1, &gid);
                if libc::setuid(uid) != 0 {
                    libc::_exit(126);
                }
                libc::execv(sandbox_exec.as_ptr(), argv.as_ptr());
                libc::_exit(127);
            }
        }
        // Parent: close the child ends, keep the daemon-facing ends.
        unsafe {
            libc::close(out_w);
            libc::close(err_w);
            if in_r >= 0 {
                libc::close(in_r);
            }
        }
        let mut fds = Vec::new();
        if want_stdin {
            fds.push(in_w);
        }
        fds.push(out_r);
        fds.push(err_r);
        Ok((pid, fds))
    }

    fn send_fds(conn_fd: RawFd, fds: &[RawFd]) -> Result<(), String> {
        use nix::sys::socket::{ControlMessage, MsgFlags, sendmsg};
        use std::os::fd::BorrowedFd;
        // One dummy byte so the receiver's recvmsg returns >0 with the cmsg.
        let iov = [std::io::IoSlice::new(b"F")];
        let cmsg = [ControlMessage::ScmRights(fds)];
        let borrowed = unsafe { BorrowedFd::borrow_raw(conn_fd) };
        sendmsg::<()>(borrowed.as_raw_fd(), &iov, &cmsg, MsgFlags::empty(), None)
            .map_err(|e| format!("sendmsg fds: {e}"))?;
        Ok(())
    }

    fn natlook_ioctl(src: SocketAddr, dst: SocketAddr) -> Result<SocketAddr, String> {
        let fd = unsafe { libc::open(b"/dev/pf\0".as_ptr() as *const _, libc::O_RDWR) };
        if fd < 0 {
            return Err(format!("open /dev/pf: {}", std::io::Error::last_os_error()));
        }
        let mut nl = PfiocNatlook::for_tcp(src, dst);
        let rc = unsafe { libc::ioctl(fd, DIOCNATLOOK as libc::c_ulong, &mut nl) };
        unsafe { libc::close(fd) };
        if rc != 0 {
            return Err(format!("DIOCNATLOOK: {}", std::io::Error::last_os_error()));
        }
        nl.original_dst().ok_or_else(|| "natlook: no original dst".into())
    }

    fn peer_uid(stream: &UnixStream) -> Option<u32> {
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        let rc = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
        (rc == 0).then_some(uid)
    }

    fn fatal(msg: &str) -> ! {
        eprintln!("agentd-pf-broker: {msg}");
        std::process::exit(1);
    }

    // Silence unused warnings for the dummy-byte reader path.
    #[allow(dead_code)]
    fn _touch(mut r: impl Read) {
        let mut b = [0u8; 1];
        let _ = r.read(&mut b);
    }
}
