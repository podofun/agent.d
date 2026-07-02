//! Local IPC between the non-admin daemon and the elevated network broker.
//!
//! WFP filter modification requires High integrity, which the daemon must never
//! hold. Instead a tiny SYSTEM service (`agentd-netbroker`) does the WFP work,
//! and the daemon drives it over a local named pipe: one connection per exec.
//!
//! Lifetime is bound to the connection. The daemon connects, sends `Provision`,
//! and holds the pipe open for the child's lifetime; when it closes the pipe
//! (child exit, or daemon crash) the broker tears the child's filters down. No
//! explicit teardown message, so a dead daemon never leaks filters.

use std::net::IpAddr;

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, OPEN_EXISTING, ReadFile, WriteFile,
};
use windows::core::PCWSTR;

use crate::ShellError;

/// The broker's named pipe. Local machine only (`\\.\pipe`).
pub const PIPE_NAME: &str = r"\\.\pipe\agentd-netbroker";

/// Max accepted frame size — a provision request is tiny; cap defensively.
const MAX_FRAME: usize = 1 << 20;

#[derive(Serialize, Deserialize, Debug)]
pub enum Request {
    /// Install a default-deny WFP filter set scoped to `sid`, permitting `ips`.
    Provision { sid: Vec<u8>, ips: Vec<IpAddr> },
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Response {
    Ok,
    Err(String),
}

fn sb(e: impl std::fmt::Display) -> ShellError {
    ShellError::Sandbox(e.to_string())
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---- framing over a pipe HANDLE (u32-LE length prefix + payload) ----

fn write_all(h: HANDLE, mut buf: &[u8]) -> std::io::Result<()> {
    while !buf.is_empty() {
        let mut wrote = 0u32;
        unsafe { WriteFile(h, Some(buf), Some(&mut wrote), None) }
            .map_err(std::io::Error::other)?;
        if wrote == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::WriteZero));
        }
        buf = &buf[wrote as usize..];
    }
    Ok(())
}

fn read_exact(h: HANDLE, buf: &mut [u8]) -> std::io::Result<()> {
    let mut off = 0;
    while off < buf.len() {
        let mut got = 0u32;
        unsafe { ReadFile(h, Some(&mut buf[off..]), Some(&mut got), None) }
            .map_err(std::io::Error::other)?;
        if got == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
        }
        off += got as usize;
    }
    Ok(())
}

fn write_frame(h: HANDLE, bytes: &[u8]) -> std::io::Result<()> {
    write_all(h, &(bytes.len() as u32).to_le_bytes())?;
    write_all(h, bytes)
}

fn read_frame(h: HANDLE) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    read_exact(h, &mut len)?;
    let n = u32::from_le_bytes(len) as usize;
    if n > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut buf = vec![0u8; n];
    read_exact(h, &mut buf)?;
    Ok(buf)
}

// ---- client (daemon side, non-admin) ----

/// A live provision: holds the pipe open so the broker keeps the child's WFP
/// filters in place. Dropping it closes the pipe, which tears them down.
pub struct Provision {
    pipe: HANDLE,
}

// SAFETY: the pipe handle is only used by the owning thread; sending the guard
// across threads (it is held for the child's lifetime) is sound.
unsafe impl Send for Provision {}

impl Drop for Provision {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.pipe);
        }
    }
}

fn connect() -> Result<HANDLE, ShellError> {
    let name = to_wide(PIPE_NAME);
    let h = unsafe {
        CreateFileW(
            PCWSTR(name.as_ptr()),
            (GENERIC_READ | GENERIC_WRITE).0,
            windows::Win32::Storage::FileSystem::FILE_SHARE_MODE(0),
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .map_err(|e| sb(format!("network broker unavailable: {e}")))?;
    Ok(h)
}

/// Poke the pipe to unblock a `ConnectNamedPipe` wait in [`accept_loop`], so the
/// service can notice a stop request and exit its accept loop promptly.
pub fn wake() {
    if let Ok(h) = connect() {
        unsafe {
            let _ = CloseHandle(h);
        }
    }
}

/// Whether the broker is reachable (service installed + running). Non-privileged.
pub fn available() -> bool {
    match connect() {
        Ok(h) => {
            unsafe {
                let _ = CloseHandle(h);
            }
            true
        }
        Err(_) => false,
    }
}

/// Ask the broker to provision WFP filters for `sid` permitting `ips`. Returns a
/// guard that must be held for the child's lifetime.
pub fn provision(sid: Vec<u8>, ips: Vec<IpAddr>) -> Result<Provision, ShellError> {
    let pipe = connect()?;
    let req = serde_json::to_vec(&Request::Provision { sid, ips }).map_err(sb)?;
    write_frame(pipe, &req).map_err(|e| sb(format!("broker write: {e}")))?;
    let resp: Response =
        serde_json::from_slice(&read_frame(pipe).map_err(|e| sb(format!("broker read: {e}")))?)
            .map_err(sb)?;
    match resp {
        Response::Ok => Ok(Provision { pipe }),
        Response::Err(e) => {
            unsafe {
                let _ = CloseHandle(pipe);
            }
            Err(sb(format!("broker refused provision: {e}")))
        }
    }
}

// ---- server (broker side, SYSTEM) ----

/// Handle one client connection to completion: read the request, apply the WFP
/// filters, reply, then block until the client disconnects and tear them down.
/// Takes ownership of `pipe` and always closes it.
pub fn serve_connection(pipe: HANDLE) {
    use crate::netfilter::NetFilter;
    use crate::sandbox::windows_wfp::WfpFilter;

    let reply = |r: Response| {
        if let Ok(b) = serde_json::to_vec(&r) {
            let _ = write_frame(pipe, &b);
        }
    };

    let req: Request = match read_frame(pipe) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(r) => r,
            Err(e) => {
                reply(Response::Err(format!("bad request: {e}")));
                unsafe {
                    let _ = CloseHandle(pipe);
                }
                return;
            }
        },
        Err(_) => {
            unsafe {
                let _ = CloseHandle(pipe);
            }
            return;
        }
    };

    match req {
        Request::Provision { sid, ips } => {
            let filter = WfpFilter::new(sid);
            match filter.provision(&ips) {
                Ok(handle) => {
                    reply(Response::Ok);
                    // Hold the filters until the client disconnects (EOF/broken
                    // pipe), then remove them.
                    let mut byte = [0u8; 1];
                    loop {
                        let mut got = 0u32;
                        let ok = unsafe { ReadFile(pipe, Some(&mut byte), Some(&mut got), None) };
                        if ok.is_err() || got == 0 {
                            break;
                        }
                    }
                    filter.teardown(handle);
                }
                Err(e) => reply(Response::Err(e.to_string())),
            }
        }
    }

    unsafe {
        let _ = CloseHandle(pipe);
    }
}

/// A pipe HANDLE that can be moved to a worker thread. The broker owns each
/// instance exclusively once handed off, so cross-thread use is sound.
struct SendPipe(HANDLE);
unsafe impl Send for SendPipe {}
impl SendPipe {
    fn take(self) -> HANDLE {
        self.0
    }
}

/// Named-pipe security: full control to SYSTEM and Administrators, read/write to
/// interactive (locally logged-on) users — so the desktop user's daemon can
/// connect, but remote or service-isolated callers cannot.
fn pipe_security() -> Result<windows::Win32::Security::PSECURITY_DESCRIPTOR, ShellError> {
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    let sddl = to_wide("D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)");
    let mut psd = windows::Win32::Security::PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut psd,
            None,
        )
        .map_err(sb)?;
    }
    Ok(psd)
}

/// Accept broker connections forever, one worker thread per client, until
/// `stop` flips. Each instance carries the restrictive pipe security above.
pub fn accept_loop(stop: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Result<(), ShellError> {
    use std::sync::atomic::Ordering;
    use windows::Win32::Security::SECURITY_ATTRIBUTES;
    use windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
        PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };

    let name = to_wide(PIPE_NAME);
    let psd = pipe_security()?;
    while !stop.load(Ordering::Relaxed) {
        let mut sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: psd.0,
            bInheritHandle: false.into(),
        };
        let pipe = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                4096,
                4096,
                0,
                Some(&mut sa),
            )
        };
        if pipe.is_invalid() {
            return Err(sb("CreateNamedPipeW failed"));
        }
        // Block until a client connects. ERROR_PIPE_CONNECTED means it connected
        // between create and connect — still a valid session.
        let connected = unsafe { ConnectNamedPipe(pipe, None) }.is_ok();
        if !connected {
            let err = unsafe { windows::Win32::Foundation::GetLastError() };
            const ERROR_PIPE_CONNECTED: u32 = 535;
            if err.0 != ERROR_PIPE_CONNECTED {
                unsafe {
                    let _ = CloseHandle(pipe);
                }
                continue;
            }
        }
        let sp = SendPipe(pipe);
        std::thread::spawn(move || {
            serve_connection(sp.take());
        });
    }
    Ok(())
}
