//! Minimal server-sent-events splitter shared by the streaming API providers.
//!
//! Byte chunks go in; complete `data:` payload lines come out. The buffer
//! holds raw bytes so a chunk boundary inside a multi-byte character or an
//! event line never corrupts the payload.

pub(crate) struct SseBuffer {
    buf: Vec<u8>,
}

impl SseBuffer {
    pub(crate) fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Feed one transport chunk; invoke `on_data` once per complete
    /// `data: <payload>` line. Other SSE fields (`event:`, comments, blank
    /// separators) are skipped — both Anthropic and OpenAI repeat the event
    /// type inside the JSON payload, so the `data` lines carry everything.
    pub(crate) fn push(&mut self, chunk: &[u8], mut on_data: impl FnMut(&str)) {
        self.buf.extend_from_slice(chunk);
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\n', '\r']);
            if let Some(payload) = line.strip_prefix("data:") {
                let payload = payload.strip_prefix(' ').unwrap_or(payload);
                if !payload.is_empty() {
                    on_data(payload);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(chunks: &[&[u8]]) -> Vec<String> {
        let mut buf = SseBuffer::new();
        let mut out = Vec::new();
        for c in chunks {
            buf.push(c, |d| out.push(d.to_string()));
        }
        out
    }

    #[test]
    fn splits_events_across_chunk_boundaries() {
        let got = collect(&[
            b"event: message_start\ndata: {\"a\"",
            b":1}\n\ndata: {\"b\":2}\n",
        ]);
        assert_eq!(got, vec![r#"{"a":1}"#, r#"{"b":2}"#]);
    }

    #[test]
    fn handles_crlf_and_ignores_non_data_lines() {
        let got = collect(&[b": comment\r\nevent: x\r\ndata: hi\r\n\r\n"]);
        assert_eq!(got, vec!["hi"]);
    }

    #[test]
    fn multibyte_char_split_across_chunks_survives() {
        let text = "data: héllo\n".as_bytes();
        let (a, b) = text.split_at(8); // split inside the two-byte é
        let got = collect(&[a, b]);
        assert_eq!(got, vec!["héllo"]);
    }
}
