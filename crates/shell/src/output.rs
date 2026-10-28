//! Bounded capture of a program's stdout and stderr. A command that never
//! stops writing (`yes`, a runaway log) must not grow the daemon's memory
//! until the OS kills it, so each stream keeps the first
//! [`MAX_OUTPUT_BYTES`] and discards the rest while still draining the pipe,
//! which keeps the child from blocking on a full buffer.

/// Bytes kept per stream before the rest is dropped.
pub const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

/// What one stream produced, up to the cap.
#[derive(Debug, Default)]
pub(crate) struct Capped {
    bytes: Vec<u8>,
    truncated: bool,
}

impl Capped {
    pub(crate) fn push(&mut self, chunk: &[u8]) {
        let room = MAX_OUTPUT_BYTES.saturating_sub(self.bytes.len());
        if chunk.len() > room {
            self.truncated = true;
        }
        self.bytes
            .extend_from_slice(&chunk[..chunk.len().min(room)]);
    }

    /// Read `r` to EOF, keeping at most the cap. Read errors end the stream
    /// like EOF: what arrived so far is still the program's output. The
    /// Windows backend reads through handles and feeds `push` itself.
    #[cfg(unix)]
    pub(crate) fn read_sync<R: std::io::Read>(mut r: R) -> Self {
        let mut out = Self::default();
        let mut buf = [0u8; 64 * 1024];
        loop {
            match r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => out.push(&buf[..n]),
            }
        }
        out
    }

    pub(crate) async fn read_async<R: tokio::io::AsyncRead + Unpin>(
        mut r: R,
    ) -> std::io::Result<Self> {
        use tokio::io::AsyncReadExt;
        let mut out = Self::default();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match r.read(&mut buf).await? {
                0 => break,
                n => out.push(&buf[..n]),
            }
        }
        Ok(out)
    }

    /// The stream as text, with a trailing note when the cap cut it short so
    /// the model and the user know the output is incomplete.
    pub(crate) fn into_text(self) -> String {
        let mut text = String::from_utf8_lossy(&self.bytes).into_owned();
        if self.truncated {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&format!(
                "[agentd: output truncated after {} MiB]\n",
                MAX_OUTPUT_BYTES / (1024 * 1024)
            ));
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_everything_under_the_cap() {
        let mut c = Capped::default();
        c.push(b"hello\n");
        assert_eq!(c.into_text(), "hello\n");
    }

    #[test]
    fn cuts_at_the_cap_and_says_so() {
        let mut c = Capped::default();
        c.push(&vec![b'a'; MAX_OUTPUT_BYTES - 1]);
        c.push(b"bcd");
        let text = c.into_text();
        assert!(text.starts_with(&"a".repeat(MAX_OUTPUT_BYTES - 1)));
        assert_eq!(text.as_bytes()[MAX_OUTPUT_BYTES - 1], b'b');
        assert!(text.ends_with("[agentd: output truncated after 16 MiB]\n"));
    }
}
