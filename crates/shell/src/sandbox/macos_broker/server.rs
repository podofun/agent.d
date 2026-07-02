//! Broker session state machine. One connection = one sandboxed exec. The
//! root-only side effects (pf anchor, ACLs, spawn-as-uid, natlook) sit behind
//! the [`Backend`] trait so the ordering — especially teardown — is unit-tested
//! off-macOS with a recording mock. The real macOS backend lives in the broker
//! binary.

use std::net::SocketAddr;

use super::pool::SandboxUser;
use super::proto::{ErrKind, Proto};

/// The root-only operations a session performs. Each maps to one narrow,
/// namespaced side effect; the trait exists so `Session` teardown ordering is
/// testable without root or macOS.
pub trait Backend {
    /// Load the pf anchor `agentd/sbx_<uid>` redirecting the uid to these ports.
    fn provision(&mut self, user: &SandboxUser, tcp_port: u16, dns_port: u16) -> Result<(), String>;
    /// Stamp per-uid ACLs (paths already canonicalized by the caller).
    fn stamp_acls(&mut self, user: &SandboxUser, read: &[String], write: &[String]) -> Result<(), String>;
    /// Spawn `bin` as the uid under Seatbelt; return the child pid. Stdio fd
    /// passing is handled by the binary out-of-band, not modeled here.
    fn spawn(&mut self, user: &SandboxUser, bin: &str, args: &[String], cwd: Option<&str>, sbpl: &str, want_stdin: bool) -> Result<i32, String>;
    /// DIOCNATLOOK: original destination of a redirected connection.
    fn natlook(&self, proto: Proto, src: SocketAddr, dst: SocketAddr) -> Result<SocketAddr, String>;
    /// Kill the spawned child if still alive.
    fn kill_child(&mut self, pid: i32);
    /// Flush the pf anchor for this uid.
    fn flush_anchor(&mut self, user: &SandboxUser);
    /// Remove the ACLs stamped for this uid.
    fn remove_acls(&mut self, user: &SandboxUser);
}

/// What a session has done so far, so teardown undoes exactly those effects in
/// reverse order regardless of where an error stopped it.
#[derive(Default)]
struct Progress {
    provisioned: bool,
    acls_stamped: bool,
    child_pid: Option<i32>,
}

/// Session bound to one leased sandbox user for the life of a connection.
pub struct Session<'u, B: Backend> {
    backend: B,
    user: &'u SandboxUser,
    progress: Progress,
    torn_down: bool,
}

/// A session error carrying the wire error kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionError {
    pub kind: ErrKind,
    pub msg: String,
}

fn backend_err(msg: String) -> SessionError {
    SessionError { kind: ErrKind::Backend, msg }
}

impl<'u, B: Backend> Session<'u, B> {
    pub fn new(backend: B, user: &'u SandboxUser) -> Self {
        Session {
            backend,
            user,
            progress: Progress::default(),
            torn_down: false,
        }
    }

    pub fn provision(&mut self, tcp_port: u16, dns_port: u16) -> Result<(), SessionError> {
        self.backend
            .provision(self.user, tcp_port, dns_port)
            .map_err(backend_err)?;
        self.progress.provisioned = true;
        Ok(())
    }

    pub fn acl(&mut self, read: &[String], write: &[String]) -> Result<(), SessionError> {
        self.backend
            .stamp_acls(self.user, read, write)
            .map_err(backend_err)?;
        self.progress.acls_stamped = true;
        Ok(())
    }

    pub fn spawn(
        &mut self,
        bin: &str,
        args: &[String],
        cwd: Option<&str>,
        sbpl: &str,
        want_stdin: bool,
    ) -> Result<i32, SessionError> {
        if self.progress.child_pid.is_some() {
            return Err(SessionError {
                kind: ErrKind::Proto,
                msg: "spawn already called for this session".into(),
            });
        }
        let pid = self
            .backend
            .spawn(self.user, bin, args, cwd, sbpl, want_stdin)
            .map_err(backend_err)?;
        self.progress.child_pid = Some(pid);
        Ok(pid)
    }

    pub fn natlook(
        &self,
        proto: Proto,
        src: SocketAddr,
        dst: SocketAddr,
    ) -> Result<SocketAddr, SessionError> {
        self.backend.natlook(proto, src, dst).map_err(backend_err)
    }

    /// Undo every effect in reverse order: kill child → flush anchor → remove
    /// ACLs. Idempotent; the uid lease is released by the caller dropping the
    /// `Lease` after this returns.
    pub fn teardown(&mut self) {
        if self.torn_down {
            return;
        }
        self.torn_down = true;
        if let Some(pid) = self.progress.child_pid.take() {
            self.backend.kill_child(pid);
        }
        if self.progress.provisioned {
            self.backend.flush_anchor(self.user);
        }
        if self.progress.acls_stamped {
            self.backend.remove_acls(self.user);
        }
    }
}

impl<B: Backend> Drop for Session<'_, B> {
    fn drop(&mut self) {
        self.teardown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Default)]
    struct Log {
        events: Vec<String>,
        fail_spawn: bool,
    }

    #[derive(Clone)]
    struct MockBackend(Rc<RefCell<Log>>);

    impl MockBackend {
        fn new() -> (Self, Rc<RefCell<Log>>) {
            let log = Rc::new(RefCell::new(Log::default()));
            (MockBackend(log.clone()), log)
        }
        fn push(&self, s: &str) {
            self.0.borrow_mut().events.push(s.into());
        }
    }

    impl Backend for MockBackend {
        fn provision(&mut self, _u: &SandboxUser, t: u16, d: u16) -> Result<(), String> {
            self.push(&format!("provision {t} {d}"));
            Ok(())
        }
        fn stamp_acls(&mut self, _u: &SandboxUser, r: &[String], w: &[String]) -> Result<(), String> {
            self.push(&format!("acl r={} w={}", r.len(), w.len()));
            Ok(())
        }
        fn spawn(&mut self, _u: &SandboxUser, bin: &str, _a: &[String], _c: Option<&str>, _s: &str, _si: bool) -> Result<i32, String> {
            if self.0.borrow().fail_spawn {
                return Err("spawn boom".into());
            }
            self.push(&format!("spawn {bin}"));
            Ok(4242)
        }
        fn natlook(&self, _p: Proto, _s: SocketAddr, _d: SocketAddr) -> Result<SocketAddr, String> {
            Ok("1.1.1.1:443".parse().unwrap())
        }
        fn kill_child(&mut self, pid: i32) {
            self.push(&format!("kill {pid}"));
        }
        fn flush_anchor(&mut self, _u: &SandboxUser) {
            self.push("flush_anchor");
        }
        fn remove_acls(&mut self, _u: &SandboxUser) {
            self.push("remove_acls");
        }
    }

    fn user() -> SandboxUser {
        SandboxUser { uid: 700, name: "_agentd_sbx0".into() }
    }

    #[test]
    fn happy_path_then_teardown_reverses() {
        let (be, log) = MockBackend::new();
        let u = user();
        {
            let mut s = Session::new(be, &u);
            s.provision(4321, 5353).unwrap();
            s.acl(&["/a".into()], &["/b".into()]).unwrap();
            let pid = s.spawn("/usr/bin/curl", &[], None, "(sbpl)", false).unwrap();
            assert_eq!(pid, 4242);
        } // drop → teardown
        assert_eq!(
            log.borrow().events,
            vec![
                "provision 4321 5353",
                "acl r=1 w=1",
                "spawn /usr/bin/curl",
                "kill 4242",
                "flush_anchor",
                "remove_acls",
            ]
        );
    }

    #[test]
    fn teardown_only_undoes_completed_steps() {
        let (be, log) = MockBackend::new();
        let u = user();
        {
            let mut s = Session::new(be, &u);
            s.provision(1, 2).unwrap();
            // no acl, no spawn
        }
        // Only the anchor was provisioned → only it is flushed. No kill, no ACLs.
        assert_eq!(log.borrow().events, vec!["provision 1 2", "flush_anchor"]);
    }

    #[test]
    fn failed_spawn_still_tears_down_prior_steps() {
        let (be, log) = MockBackend::new();
        log.borrow_mut().fail_spawn = true;
        let u = user();
        {
            let mut s = Session::new(be, &u);
            s.provision(1, 2).unwrap();
            s.acl(&[], &[]).unwrap();
            let e = s.spawn("/x", &[], None, "", false).unwrap_err();
            assert_eq!(e.kind, ErrKind::Backend);
        }
        assert_eq!(
            log.borrow().events,
            vec!["provision 1 2", "acl r=0 w=0", "flush_anchor", "remove_acls"],
            "no kill (spawn failed) but anchor+acls undone"
        );
    }

    #[test]
    fn double_spawn_rejected() {
        let (be, _log) = MockBackend::new();
        let u = user();
        let mut s = Session::new(be, &u);
        s.spawn("/x", &[], None, "", false).unwrap();
        let e = s.spawn("/y", &[], None, "", false).unwrap_err();
        assert_eq!(e.kind, ErrKind::Proto);
    }

    #[test]
    fn explicit_teardown_is_idempotent() {
        let (be, log) = MockBackend::new();
        let u = user();
        let mut s = Session::new(be, &u);
        s.provision(1, 2).unwrap();
        s.teardown();
        s.teardown();
        drop(s);
        assert_eq!(
            log.borrow().events.iter().filter(|e| *e == "flush_anchor").count(),
            1,
            "teardown runs exactly once"
        );
    }
}
