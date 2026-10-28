//! Programs on a pseudo-terminal, for terminal panes. They run under the
//! same sandbox as `exec`, without a timeout. Where the sandbox cannot wrap
//! a terminal, the program is refused.

use tokio::io::{AsyncRead, AsyncWrite};

use crate::{ExecRequest, ShellError};

/// A program running on a pseudo-terminal. Dropping `child` stops it.
pub struct PtyChild {
    pub reader: Box<dyn AsyncRead + Send + Unpin>,
    pub writer: Box<dyn AsyncWrite + Send + Unpin>,
    /// Tell the program its new size, in columns and rows.
    pub resize: Box<dyn Fn(u16, u16) + Send + Sync>,
    pub child: Box<dyn Send>,
}

/// Start `req` on a new pseudo-terminal of `width` by `height` cells.
#[cfg(unix)]
pub fn spawn_pty(req: &ExecRequest, width: u16, height: u16) -> Result<PtyChild, ShellError> {
    use std::os::fd::AsRawFd;

    let sandboxed = req.sandbox.as_ref().filter(|p| !p.unrestricted);
    if let Some(policy) = sandboxed {
        if !crate::sandbox::is_supported() {
            return Err(ShellError::SandboxUnavailable);
        }
        if policy.allow_net {
            return Err(ShellError::Terminal(format!(
                "a terminal pane cannot reach the network inside the sandbox, so `{}` needs grants without `net:`",
                req.bin
            )));
        }
    }
    #[cfg(target_os = "macos")]
    let (bin, args): (String, Vec<String>) = match sandboxed {
        Some(p) => crate::sandbox::wrap_argv(p, &req.bin, &req.args),
        None => (req.bin.clone(), req.args.clone()),
    };
    #[cfg(not(target_os = "macos"))]
    let (bin, args): (String, Vec<String>) = (req.bin.clone(), req.args.clone());

    let (pty, pts) = pty_process::open()
        .map_err(|e| ShellError::Terminal(format!("could not open a terminal ({e})")))?;
    pty.resize(pty_process::Size::new(height.max(1), width.max(2)))
        .map_err(|e| ShellError::Terminal(format!("could not open a terminal ({e})")))?;
    let mut cmd = pty_process::Command::new(&bin)
        .args(&args)
        .env("TERM", "xterm-256color")
        .kill_on_drop(true);
    if let Some(cwd) = &req.cwd {
        cmd = cmd.current_dir(cwd);
    }
    #[cfg(target_os = "linux")]
    if let Some(policy) = sandboxed.cloned() {
        cmd = unsafe {
            cmd.pre_exec(move || {
                crate::sandbox::apply(&policy)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::PermissionDenied, e))
            })
        };
    }
    let child = cmd.spawn(pts).map_err(|e| ShellError::Spawn {
        bin: req.bin.clone(),
        source: std::io::Error::other(e),
    })?;
    let fd = pty.as_raw_fd();
    let (reader, writer) = pty.into_split();
    let resize = Box::new(move |width: u16, height: u16| {
        let size = libc::winsize {
            ws_row: height.max(1),
            ws_col: width.max(2),
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // The descriptor lives as long as the reader and writer, which are
        // dropped together with this closure.
        unsafe {
            libc::ioctl(fd, libc::TIOCSWINSZ, &size);
        }
    });
    Ok(PtyChild {
        reader: Box::new(reader),
        writer: Box::new(writer),
        resize,
        child: Box::new(child),
    })
}

/// Windows has no sandbox that can wrap a terminal yet.
#[cfg(not(unix))]
pub fn spawn_pty(req: &ExecRequest, _width: u16, _height: u16) -> Result<PtyChild, ShellError> {
    Err(ShellError::Terminal(format!(
        "terminal panes are not available on this system yet, so `{}` was not started",
        req.bin
    )))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn a_program_prints_to_its_terminal() {
        let req = ExecRequest {
            bin: "/bin/sh".into(),
            args: vec!["-c".into(), "stty size; echo done".into()],
            cwd: None,
            stdin: None,
            separate_stderr: false,
            timeout_ms: None,
            sandbox: None,
        };
        let mut pty = spawn_pty(&req, 40, 7).unwrap();
        let mut out = Vec::new();
        let mut buf = [0u8; 256];
        while !String::from_utf8_lossy(&out).contains("done") {
            match tokio::time::timeout(std::time::Duration::from_secs(5), pty.reader.read(&mut buf))
                .await
            {
                Ok(Ok(n)) if n > 0 => out.extend_from_slice(&buf[..n]),
                _ => break,
            }
        }
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("7 40"), "{text}");
    }
}
