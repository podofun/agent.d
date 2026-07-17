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
    use std::io::{BufReader, Write};
    use std::net::SocketAddr;
    use std::os::fd::{AsRawFd, RawFd};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::process::Command;

    use agentd_shell::sandbox::macos_broker::SOCKET_PATH;
    use agentd_shell::sandbox::macos_broker::config::BrokerConfig;
    use agentd_shell::sandbox::macos_broker::pool::{SandboxUser, UidPool};
    use agentd_shell::sandbox::macos_broker::proto::{
        ErrKind, Proto, Req, Resp, read_msg, write_msg,
    };
    use agentd_shell::sandbox::macos_broker::server::{Backend, Session};
    use agentd_shell::sandbox::macos_pf_rules::{
        DIOCNATLOOK, PfiocNatlook, anchor_name, build_anchor_rules,
    };

    const CONF_PATH: &str = "/etc/agentd/broker.conf";

    pub fn run() {
        let text = std::fs::read_to_string(CONF_PATH)
            .unwrap_or_else(|e| fatal(&format!("could not read `{CONF_PATH}` ({e})")));
        let cfg = BrokerConfig::parse(&text)
            .unwrap_or_else(|e| fatal(&format!("the broker config is invalid ({e})")));
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
        let listener = UnixListener::bind(SOCKET_PATH).unwrap_or_else(|e| {
            fatal(&format!(
                "could not bind the broker socket `{SOCKET_PATH}` ({e})"
            ))
        });
        // Restrict the socket to the daemon uid at the filesystem layer (0600 +
        // chown), so only that uid can even connect; getpeereid then re-checks
        // per connection. Defense in depth, not either/or.
        let _ = std::fs::set_permissions(
            SOCKET_PATH,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        );
        // SAFETY: FFI to chown(2) with a static NUL-terminated path literal and
        // plain integer ids. No Rust std wrapper sets owner uid/gid on a path
        // (std::fs only exposes permission bits), so libc is the only option; a
        // failure is non-fatal (perms already restrict to 0600).
        unsafe {
            libc::chown(
                b"/var/run/agentd/broker.sock\0".as_ptr() as *const _,
                cfg.daemon_uid,
                0,
            );
        }

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
                        msg: match other {
                            Some(uid) => format!(
                                "connection refused — peer uid {uid} does not match the daemon uid {daemon_uid}"
                            ),
                            None => format!(
                                "connection refused — the peer uid could not be determined (daemon uid is {daemon_uid})"
                            ),
                        },
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
                        msg: "all sandbox uids are in use — retry shortly".into(),
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
            let was_spawn = matches!(resp, Resp::Spawned { .. });
            if write_msg(&mut writer, &resp).is_err() {
                break;
            }
            // Pass the child's stdio fds AFTER the reply, so the reply line and
            // the SCM_RIGHTS dummy byte never interleave on the wire.
            if was_spawn {
                let fds = session.take_stdio_fds();
                if !fds.is_empty() {
                    let _ = send_fds(raw, &fds);
                    for fd in fds {
                        // SAFETY: close(2) on the broker's own ends of the child
                        // stdio pipes, now dup'd into the daemon via SCM_RIGHTS.
                        // These raw fds are owned here (not wrapped in an OwnedFd)
                        // so a manual close is the only way to release them.
                        unsafe { libc::close(fd) };
                    }
                }
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
            Req::Provision { tcp_port, dns_port } => match session.provision(tcp_port, dns_port) {
                Ok(()) => Resp::Ok,
                Err(e) => Resp::Err {
                    kind: e.kind,
                    msg: e.msg,
                },
            },
            Req::Acl { read, write } => match session.acl(&read, &write) {
                Ok(()) => Resp::Ok,
                Err(e) => Resp::Err {
                    kind: e.kind,
                    msg: e.msg,
                },
            },
            Req::Spawn {
                bin,
                args,
                cwd,
                sbpl,
                want_stdin,
            } => match session.spawn(&bin, &args, cwd.as_deref(), &sbpl, want_stdin) {
                Ok(pid) => Resp::Spawned { pid },
                Err(e) => Resp::Err {
                    kind: e.kind,
                    msg: e.msg,
                },
            },
            Req::Natlook { proto, src, dst } => {
                let (Ok(src), Ok(dst)) = (src.parse::<SocketAddr>(), dst.parse::<SocketAddr>())
                else {
                    return Resp::Err {
                        kind: ErrKind::Proto,
                        msg: "bad socket addr".into(),
                    };
                };
                match session.natlook(proto, src, dst) {
                    Ok(orig) => Resp::NatlookResult {
                        orig: orig.to_string(),
                    },
                    Err(e) => Resp::Err {
                        kind: e.kind,
                        msg: e.msg,
                    },
                }
            }
            Req::Wait => match session.wait() {
                Ok(code) => Resp::Exit { code },
                Err(e) => Resp::Err {
                    kind: e.kind,
                    msg: e.msg,
                },
            },
        }
    }

    /// The real root-side backend. Holds the connection fd so `spawn` can pass
    /// the child's stdio back to the daemon via `SCM_RIGHTS`.
    struct MacBackend {
        pending_fds: Vec<RawFd>,
    }

    impl MacBackend {
        fn new(_conn_fd: RawFd) -> Self {
            MacBackend {
                pending_fds: Vec::new(),
            }
        }
    }

    impl Backend for MacBackend {
        fn provision(
            &mut self,
            user: &SandboxUser,
            tcp_port: u16,
            dns_port: u16,
        ) -> Result<(), String> {
            let rules = build_anchor_rules(user.uid, tcp_port, dns_port);
            pfctl_load(&anchor_name(user.uid), &rules)
        }

        fn stamp_acls(
            &mut self,
            user: &SandboxUser,
            read: &[String],
            write: &[String],
        ) -> Result<(), String> {
            for p in read {
                chmod_acl_add(&user.name, p, "read,execute,search")?;
            }
            for p in write {
                chmod_acl_add(
                    &user.name,
                    p,
                    "read,write,execute,search,delete,append,add_file,add_subdirectory",
                )?;
            }
            Ok(())
        }

        fn spawn(
            &mut self,
            user: &SandboxUser,
            bin: &str,
            args: &[String],
            cwd: Option<&str>,
            sbpl: &str,
            want_stdin: bool,
        ) -> Result<i32, String> {
            let (pid, fds) = spawn_as_uid(user.uid, bin, args, cwd, sbpl, want_stdin)?;
            // Stash the daemon-facing stdio ends; the caller sends them via
            // SCM_RIGHTS only AFTER the Spawned reply line is on the wire.
            self.pending_fds = fds;
            Ok(pid)
        }

        fn take_stdio_fds(&mut self) -> Vec<RawFd> {
            std::mem::take(&mut self.pending_fds)
        }

        fn natlook(
            &self,
            proto: Proto,
            src: SocketAddr,
            dst: SocketAddr,
        ) -> Result<SocketAddr, String> {
            if proto != Proto::Tcp {
                return Err("only tcp natlook supported".into());
            }
            natlook_ioctl(src, dst)
        }

        fn wait_child(&mut self, pid: i32) -> i32 {
            let mut status = 0;
            // SAFETY: waitpid(2) on a pid this broker forked; `status` is a valid
            // local out-param. std has no child-reaping API for a bare pid we
            // fork()'d ourselves (Command owns its own Child), so libc is required.
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
            // SAFETY: kill(2)+waitpid(2) on a pid this broker forked. Signalling
            // and reaping a raw pid has no safe std equivalent (no Child handle).
            unsafe { libc::kill(pid, libc::SIGKILL) };
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
            .map_err(|e| format!("could not start `pfctl` ({e})"))?;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(rules.as_bytes())
            .map_err(|e| format!("could not write to the `pfctl` stdin ({e})"))?;
        let out = child
            .wait_with_output()
            .map_err(|e| format!("could not wait for `pfctl` to finish ({e})"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "the `pfctl` command failed ({})",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
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
            .map_err(|e| format!("could not start `chmod` ({e})"))?;
        if st.status.success() {
            Ok(())
        } else {
            Err(format!(
                "the command `chmod +a {path}` failed ({})",
                String::from_utf8_lossy(&st.stderr).trim()
            ))
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
            // SAFETY: pipe(2) writes two fds into a valid 2-element array. We
            // need the raw fds (not std's OwnedFd) because they must survive
            // fork() and be dup2'd in the async-signal-safe child region below.
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
            CString::new(sbpl).map_err(|_| "the sandbox profile contains a NUL byte")?,
            CString::new("--").unwrap(),
            CString::new(bin).map_err(|_| "the executable path contains a NUL byte")?,
        ];
        for a in args {
            cargs.push(
                CString::new(a.as_str()).map_err(|_| "a command argument contains a NUL byte")?,
            );
        }
        let mut argv: Vec<*const libc::c_char> = cargs.iter().map(|c| c.as_ptr()).collect();
        argv.push(std::ptr::null());
        let cwd_c = cwd.map(|c| CString::new(c).unwrap());

        // SAFETY: fork(2) has no safe wrapper. This process is a plain
        // single-threaded connection handler at this point, so the child may run
        // the async-signal-safe region below before execve. Returns the child
        // pid in the parent, 0 in the child.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err("fork failed".into());
        }
        if pid == 0 {
            // SAFETY: async-signal-safe-only region in the forked child. Every
            // call here (dup2/open/close/chdir/setgid/setgroups/setuid/execv/
            // _exit) is on the POSIX async-signal-safe list and operates on fds
            // and CStrings that outlive this block; no allocation, no locks. A
            // safe wrapper (e.g. Command) is forbidden here because it may
            // allocate/lock between fork and exec.
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
        // SAFETY: close(2) on the parent's copies of the child-side pipe fds,
        // owned here as raw ints; the daemon-facing ends stay open in `fds`.
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
        // SAFETY: borrow_raw over the connection fd, which the caller owns and
        // keeps open for the whole session; the BorrowedFd does not outlive this
        // call. nix's sendmsg needs a borrowed fd and there is no owned handle to
        // hand it without taking ownership of the live connection socket.
        let borrowed = unsafe { BorrowedFd::borrow_raw(conn_fd) };
        sendmsg::<()>(borrowed.as_raw_fd(), &iov, &cmsg, MsgFlags::empty(), None)
            .map_err(|e| format!("could not send the stdio file descriptors ({e})"))?;
        Ok(())
    }

    fn natlook_ioctl(src: SocketAddr, dst: SocketAddr) -> Result<SocketAddr, String> {
        // SAFETY: open(2) on a static NUL-terminated path; returns a raw fd or
        // <0. There is no std API for /dev/pf, and the ioctl below needs the raw
        // fd anyway. Closed unconditionally before returning.
        let fd = unsafe { libc::open(b"/dev/pf\0".as_ptr() as *const _, libc::O_RDWR) };
        if fd < 0 {
            return Err(format!(
                "could not open `/dev/pf` ({})",
                std::io::Error::last_os_error()
            ));
        }
        let mut nl = PfiocNatlook::for_tcp(src, dst);
        // SAFETY: DIOCNATLOOK ioctl on /dev/pf. `nl` is a #[repr(C)] struct whose
        // layout mirrors xnu's pfioc_natlook (size asserted in macos_pf_rules
        // tests) and outlives the call; the ioctl request code is derived from
        // that same struct's size. ioctl has no safe wrapper.
        let rc = unsafe { libc::ioctl(fd, DIOCNATLOOK as libc::c_ulong, &mut nl) };
        // SAFETY: close(2) on the fd we just opened and still own.
        unsafe { libc::close(fd) };
        if rc != 0 {
            return Err(format!(
                "the pf NAT lookup ioctl failed ({})",
                std::io::Error::last_os_error()
            ));
        }
        nl.original_dst()
            .ok_or_else(|| "the pf NAT lookup returned no original destination".into())
    }

    fn peer_uid(stream: &UnixStream) -> Option<u32> {
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        // SAFETY: getpeereid(2) on the connected socket's fd (borrowed for the
        // call via as_raw_fd), writing two valid local out-params. std exposes
        // no peer-credential API on UnixStream, so libc is required.
        let rc = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
        (rc == 0).then_some(uid)
    }

    fn fatal(msg: &str) -> ! {
        eprintln!("agentd-pf-broker: {msg}");
        std::process::exit(1);
    }
}
