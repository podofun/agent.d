//! Programs on a pseudo-terminal, for terminal panes. A pane is UI code a
//! person wrote, so its program runs unrestricted and without a timeout, on
//! Linux, macOS and Windows (ConPTY).

use std::io::{Read, Write};
use std::sync::Mutex;

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{ExecRequest, ShellError};

/// A program running on a pseudo-terminal. Dropping `child` stops it.
pub struct PtyChild {
    pub reader: Box<dyn AsyncRead + Send + Unpin>,
    pub writer: Box<dyn AsyncWrite + Send + Unpin>,
    /// Tell the program its new size, in columns and rows.
    pub resize: Box<dyn Fn(u16, u16) + Send + Sync>,
    pub child: Box<dyn Send>,
}

/// Kills the program when the pane goes away.
struct KillOnDrop(Box<dyn portable_pty::Child + Send + Sync>);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

fn size(width: u16, height: u16) -> PtySize {
    PtySize {
        rows: height.max(1),
        cols: width.max(2),
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// Start `req` on a new pseudo-terminal of `width` by `height` cells. Must be
/// called inside a Tokio runtime: the terminal's blocking reads and writes
/// run on their own threads and reach the caller through async pipes.
pub fn spawn_pty(req: &ExecRequest, width: u16, height: u16) -> Result<PtyChild, ShellError> {
    let failed = |e: &dyn std::fmt::Display| {
        ShellError::Terminal(format!("could not open a terminal ({e})"))
    };
    let pair = native_pty_system()
        .openpty(size(width, height))
        .map_err(|e| failed(&e))?;
    let mut cmd = CommandBuilder::new(&req.bin);
    cmd.args(&req.args);
    cmd.env("TERM", "xterm-256color");
    // Without a folder, portable-pty would start in $HOME. Start where
    // other commands do, in the daemon's own folder.
    match &req.cwd {
        Some(cwd) => cmd.cwd(cwd),
        None => {
            if let Ok(dir) = std::env::current_dir() {
                cmd.cwd(dir);
            }
        }
    }
    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| ShellError::Spawn {
            bin: req.bin.clone(),
            source: std::io::Error::other(e.to_string()),
        })?;
    drop(pair.slave);
    let mut output = pair.master.try_clone_reader().map_err(|e| failed(&e))?;
    let mut input = pair.master.take_writer().map_err(|e| failed(&e))?;
    let runtime = tokio::runtime::Handle::current();

    // Program output: a blocking thread copies it into an async pipe.
    let (reader, mut out_pipe) = tokio::io::duplex(64 * 1024);
    let out_runtime = runtime.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = output.read(&mut buf) {
            if n == 0 || out_runtime.block_on(out_pipe.write_all(&buf[..n])).is_err() {
                break;
            }
        }
    });

    // Keys: an async pipe a blocking thread copies into the program.
    let (mut in_pipe, writer) = tokio::io::duplex(16 * 1024);
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = runtime.block_on(in_pipe.read(&mut buf)) {
            if n == 0
                || input
                    .write_all(&buf[..n])
                    .and_then(|_| input.flush())
                    .is_err()
            {
                break;
            }
        }
    });

    let master = Mutex::new(pair.master);
    let resize = Box::new(move |width: u16, height: u16| {
        if let Ok(master) = master.lock() {
            let _ = master.resize(size(width, height));
        }
    });
    Ok(PtyChild {
        reader: Box::new(reader),
        writer: Box::new(writer),
        resize,
        child: Box::new(KillOnDrop(child)),
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test(flavor = "multi_thread")]
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
