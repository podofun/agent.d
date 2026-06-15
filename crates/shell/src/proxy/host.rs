//! Pure destination-host extraction from the first client bytes.
//!
//! Reads the TLS ClientHello SNI, or an HTTP/1.x `Host` / `CONNECT` target,
//! WITHOUT consuming or decrypting the stream. Totality is the security
//! contract: every malformed or short input maps to `NeedMore` or `Deny`, never
//! a panic and never an out-of-bounds index. All reads are bounds-checked.

/// Decision for a peeked client prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostDecision {
    /// Destination host (lowercased, no port).
    Allow(String),
    /// Not enough bytes yet to decide; the caller should read more.
    NeedMore,
    /// The host cannot or must not be derived; deny the connection.
    Deny(&'static str),
}

/// Determine the destination host from the first client bytes. TLS ClientHello
/// SNI, or HTTP/1.x `Host` / `CONNECT` target. Total: any malformed or short
/// input yields `NeedMore` or `Deny`.
pub fn extract_host(buf: &[u8]) -> HostDecision {
    match buf.first() {
        None => HostDecision::NeedMore,
        // TLS handshake record.
        Some(0x16) => parse_tls_client_hello(buf),
        // Otherwise treat as (possibly) HTTP/1.x.
        Some(_) => parse_http(buf),
    }
}

// ---------------------------------------------------------------------------
// TLS
// ---------------------------------------------------------------------------

const EXT_SERVER_NAME: u16 = 0x0000;
const EXT_ENCRYPTED_CLIENT_HELLO: u16 = 0xfe0d;

/// Bounds-checked forward cursor. Every read returns `None` when the slice is
/// too short, which the caller maps to `NeedMore`.
struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Cursor { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|s| s[0])
    }

    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|s| u16::from_be_bytes([s[0], s[1]]))
    }

    fn u24(&mut self) -> Option<usize> {
        self.take(3)
            .map(|s| ((s[0] as usize) << 16) | ((s[1] as usize) << 8) | (s[2] as usize))
    }
}

/// Parse a TLS ClientHello and extract SNI. Returns `NeedMore` if any declared
/// length runs past the buffer (more bytes may complete it), `Deny` if the
/// record is fully present but has no usable SNI (incl. ECH), `Allow` otherwise.
fn parse_tls_client_hello(buf: &[u8]) -> HostDecision {
    let mut c = Cursor::new(buf);

    // TLS record header: type(1) version(2) length(2).
    if c.u8().is_none() {
        return HostDecision::NeedMore;
    } // 0x16, already checked
    if c.u16().is_none() {
        return HostDecision::NeedMore;
    } // record version
    let rec_len = match c.u16() {
        Some(n) => n as usize,
        None => return HostDecision::NeedMore,
    };
    // The full handshake must fit in this record's declared length AND be present.
    let rec_body = match c.take(rec_len) {
        Some(b) => b,
        None => return HostDecision::NeedMore,
    };

    // Handshake header within the record body: msg_type(1) length(3).
    let mut h = Cursor::new(rec_body);
    match h.u8() {
        Some(0x01) => {} // ClientHello
        Some(_) => return HostDecision::Deny("not a client hello"),
        None => return HostDecision::NeedMore,
    }
    let hs_len = match h.u24() {
        Some(n) => n,
        None => return HostDecision::NeedMore,
    };
    let body = match h.take(hs_len) {
        Some(b) => b,
        None => return HostDecision::NeedMore,
    };

    parse_client_hello_body(body)
}

fn parse_client_hello_body(body: &[u8]) -> HostDecision {
    let mut c = Cursor::new(body);

    // client_version(2) random(32)
    if c.take(2).is_none() || c.take(32).is_none() {
        return HostDecision::NeedMore;
    }
    // session_id: u8 len + bytes
    match c.u8() {
        Some(n) => {
            if c.take(n as usize).is_none() {
                return HostDecision::NeedMore;
            }
        }
        None => return HostDecision::NeedMore,
    }
    // cipher_suites: u16 len + bytes
    match c.u16() {
        Some(n) => {
            if c.take(n as usize).is_none() {
                return HostDecision::NeedMore;
            }
        }
        None => return HostDecision::NeedMore,
    }
    // compression_methods: u8 len + bytes
    match c.u8() {
        Some(n) => {
            if c.take(n as usize).is_none() {
                return HostDecision::NeedMore;
            }
        }
        None => return HostDecision::NeedMore,
    }

    // extensions: u16 len + bytes. No extensions block => no SNI.
    let ext_total = match c.u16() {
        Some(n) => n as usize,
        None => return HostDecision::Deny("tls without sni"),
    };
    let exts = match c.take(ext_total) {
        Some(b) => b,
        None => return HostDecision::NeedMore,
    };

    parse_extensions(exts)
}

fn parse_extensions(exts: &[u8]) -> HostDecision {
    let mut c = Cursor::new(exts);
    let mut saw_ech = false;

    while let Some(ext_type) = c.u16() {
        let ext_len = match c.u16() {
            Some(n) => n as usize,
            None => return HostDecision::NeedMore,
        };
        let ext_data = match c.take(ext_len) {
            Some(d) => d,
            None => return HostDecision::NeedMore,
        };

        match ext_type {
            EXT_SERVER_NAME => {
                if let Some(host) = parse_sni(ext_data) {
                    return HostDecision::Allow(host.to_ascii_lowercase());
                }
                // server_name extension present but no usable host_name entry.
                return HostDecision::Deny("tls sni without host_name");
            }
            EXT_ENCRYPTED_CLIENT_HELLO => saw_ech = true,
            _ => {}
        }
    }

    if saw_ech {
        HostDecision::Deny("ech")
    } else {
        HostDecision::Deny("tls without sni")
    }
}

/// Parse the server_name extension data; return the first host_name entry.
fn parse_sni(data: &[u8]) -> Option<String> {
    let mut c = Cursor::new(data);
    let list_len = c.u16()? as usize;
    let list = c.take(list_len)?;
    let mut lc = Cursor::new(list);
    // Entries: name_type(1) name_len(2) name.
    loop {
        let name_type = lc.u8()?;
        let name_len = lc.u16()? as usize;
        let name = lc.take(name_len)?;
        if name_type == 0 {
            // host_name: must be valid UTF-8/ASCII.
            return std::str::from_utf8(name).ok().map(|s| s.to_string());
        }
        // other name types: skip and continue.
    }
}

// ---------------------------------------------------------------------------
// HTTP/1.x
// ---------------------------------------------------------------------------

const HTTP_METHODS: &[&[u8]] = &[
    b"GET ",
    b"POST ",
    b"PUT ",
    b"DELETE ",
    b"HEAD ",
    b"OPTIONS ",
    b"PATCH ",
    b"TRACE ",
    b"CONNECT ",
];

fn parse_http(buf: &[u8]) -> HostDecision {
    // Must begin with a known method token. If it's a prefix of one, wait.
    if !looks_like_http(buf) {
        return HostDecision::Deny("unrecognized protocol");
    }

    if let Some(rest) = strip_prefix(buf, b"CONNECT ") {
        // CONNECT host:port HTTP/1.1
        let target = match take_until_space(rest) {
            Some(t) => t,
            None => return HostDecision::NeedMore,
        };
        return match host_from_authority(target) {
            Some(h) => HostDecision::Allow(h),
            None => HostDecision::Deny("bad connect target"),
        };
    }

    // Other methods: wait for the full header block before trusting any Host
    // value (a partial header line could otherwise be parsed prematurely).
    match find_subslice(buf, b"\r\n\r\n") {
        None => HostDecision::NeedMore,
        Some(_) => match find_host_header(buf) {
            Some(h) => HostDecision::Allow(h),
            None => HostDecision::Deny("http without host"),
        },
    }
}

fn looks_like_http(buf: &[u8]) -> bool {
    for m in HTTP_METHODS {
        if buf.len() >= m.len() {
            if &buf[..m.len()] == *m {
                return true;
            }
        } else if m.starts_with(buf) {
            // partial method token; could still become HTTP
            return true;
        }
    }
    false
}

fn strip_prefix<'a>(buf: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    if buf.len() >= prefix.len() && &buf[..prefix.len()] == prefix {
        Some(&buf[prefix.len()..])
    } else {
        None
    }
}

fn take_until_space(buf: &[u8]) -> Option<&[u8]> {
    buf.iter().position(|&b| b == b' ').map(|i| &buf[..i])
}

/// Extract host from an `authority` like `host:port` or `[::1]:443` or `host`.
fn host_from_authority(authority: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(authority).ok()?.trim();
    if s.is_empty() {
        return None;
    }
    // IPv6 literal in brackets.
    if let Some(rest) = s.strip_prefix('[') {
        let host = rest.split(']').next()?;
        if host.is_empty() {
            return None;
        }
        return Some(host.to_ascii_lowercase());
    }
    let host = s.split(':').next().unwrap_or(s);
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

/// Find a `Host:` header value (case-insensitive) in the header block.
fn find_host_header(buf: &[u8]) -> Option<String> {
    // Operate line by line up to the end of headers (or end of buffer).
    let end = find_subslice(buf, b"\r\n\r\n").unwrap_or(buf.len());
    let head = &buf[..end];
    for line in head.split(|&b| b == b'\n') {
        let line = trim_cr(line);
        if let Some(colon) = line.iter().position(|&b| b == b':') {
            let (name, value) = line.split_at(colon);
            if name.eq_ignore_ascii_case(b"host") {
                let value = &value[1..]; // skip ':'
                return host_from_authority(value);
            }
        }
    }
    None
}

fn trim_cr(line: &[u8]) -> &[u8] {
    if let Some((&b'\r', rest)) = line.split_last() {
        rest
    } else {
        line
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal TLS ClientHello record carrying the given SNI host.
    fn build_client_hello(host: &str) -> Vec<u8> {
        build_client_hello_inner(Some(host), false)
    }
    fn build_client_hello_no_sni() -> Vec<u8> {
        build_client_hello_inner(None, false)
    }
    fn build_client_hello_ech() -> Vec<u8> {
        build_client_hello_inner(None, true)
    }

    fn build_client_hello_inner(sni: Option<&str>, ech: bool) -> Vec<u8> {
        // Build extensions block.
        let mut exts: Vec<u8> = Vec::new();
        if let Some(host) = sni {
            let hb = host.as_bytes();
            // server_name_list entry: name_type(0) + u16 len + name
            let mut entry = Vec::new();
            entry.push(0u8);
            entry.extend_from_slice(&(hb.len() as u16).to_be_bytes());
            entry.extend_from_slice(hb);
            // list: u16 list_len + entry
            let mut list = Vec::new();
            list.extend_from_slice(&(entry.len() as u16).to_be_bytes());
            list.extend_from_slice(&entry);
            // extension: type(0x0000) + u16 len + list
            exts.extend_from_slice(&EXT_SERVER_NAME.to_be_bytes());
            exts.extend_from_slice(&(list.len() as u16).to_be_bytes());
            exts.extend_from_slice(&list);
        }
        if ech {
            exts.extend_from_slice(&EXT_ENCRYPTED_CLIENT_HELLO.to_be_bytes());
            exts.extend_from_slice(&2u16.to_be_bytes());
            exts.extend_from_slice(&[0xab, 0xcd]);
        }

        // ClientHello body.
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(&[0x03, 0x03]); // client_version
        body.extend_from_slice(&[0u8; 32]); // random
        body.push(0); // session_id len
        body.extend_from_slice(&2u16.to_be_bytes()); // cipher_suites len
        body.extend_from_slice(&[0x00, 0x2f]); // one cipher suite
        body.push(1); // compression len
        body.push(0); // null compression
        body.extend_from_slice(&(exts.len() as u16).to_be_bytes());
        body.extend_from_slice(&exts);

        // Handshake header.
        let mut hs: Vec<u8> = Vec::new();
        hs.push(0x01); // ClientHello
        let blen = body.len();
        hs.extend_from_slice(&[(blen >> 16) as u8, (blen >> 8) as u8, blen as u8]);
        hs.extend_from_slice(&body);

        // TLS record.
        let mut rec: Vec<u8> = Vec::new();
        rec.push(0x16); // handshake
        rec.extend_from_slice(&[0x03, 0x01]); // record version
        rec.extend_from_slice(&(hs.len() as u16).to_be_bytes());
        rec.extend_from_slice(&hs);
        rec
    }

    #[test]
    fn parses_sni() {
        let buf = build_client_hello("example.com");
        assert_eq!(
            extract_host(&buf),
            HostDecision::Allow("example.com".into())
        );
    }

    #[test]
    fn sni_lowercased() {
        let buf = build_client_hello("API.Example.COM");
        assert_eq!(
            extract_host(&buf),
            HostDecision::Allow("api.example.com".into())
        );
    }

    #[test]
    fn http_host_header() {
        let buf = b"GET / HTTP/1.1\r\nHost: api.example.com\r\n\r\n";
        assert_eq!(
            extract_host(buf),
            HostDecision::Allow("api.example.com".into())
        );
    }

    #[test]
    fn http_host_with_port() {
        let buf = b"GET / HTTP/1.1\r\nHost: api.example.com:8443\r\n\r\n";
        assert_eq!(
            extract_host(buf),
            HostDecision::Allow("api.example.com".into())
        );
    }

    #[test]
    fn http_connect() {
        let buf = b"CONNECT api.example.com:443 HTTP/1.1\r\n\r\n";
        assert_eq!(
            extract_host(buf),
            HostDecision::Allow("api.example.com".into())
        );
    }

    #[test]
    fn truncated_tls_record_needs_more() {
        let full = build_client_hello("example.com");
        assert_eq!(extract_host(&full[..5]), HostDecision::NeedMore);
    }

    #[test]
    fn partial_clienthello_across_segments_needs_more_then_allows() {
        let full = build_client_hello("example.com");
        let split = full.len() / 2;
        assert_eq!(extract_host(&full[..split]), HostDecision::NeedMore);
        assert_eq!(
            extract_host(&full),
            HostDecision::Allow("example.com".into())
        );
    }

    #[test]
    fn non_tls_non_http_denied() {
        assert!(matches!(
            extract_host(b"\x01\x02\x03garbage"),
            HostDecision::Deny(_)
        ));
    }

    #[test]
    fn tls_without_sni_denied() {
        let buf = build_client_hello_no_sni();
        assert_eq!(extract_host(&buf), HostDecision::Deny("tls without sni"));
    }

    #[test]
    fn ech_without_plaintext_sni_denied() {
        let buf = build_client_hello_ech();
        assert_eq!(extract_host(&buf), HostDecision::Deny("ech"));
    }

    #[test]
    fn oversized_length_fields_do_not_panic() {
        let buf = vec![0x16, 0x03, 0x01, 0xff, 0xff, 0x01, 0xff, 0xff, 0xff];
        // record length 0xffff but short buffer => NeedMore, never panic.
        assert_eq!(extract_host(&buf), HostDecision::NeedMore);
    }

    #[test]
    fn partial_http_method_needs_more() {
        assert_eq!(extract_host(b"GE"), HostDecision::NeedMore);
    }

    #[test]
    fn http_incomplete_headers_needs_more() {
        let buf = b"GET / HTTP/1.1\r\nHost: ex"; // no CRLFCRLF yet, host line incomplete
        // Host header value present but no terminator; treat as NeedMore.
        assert_eq!(extract_host(buf), HostDecision::NeedMore);
    }

    #[test]
    fn connect_ipv6_literal() {
        let buf = b"CONNECT [2606:4700::1111]:443 HTTP/1.1\r\n\r\n";
        assert_eq!(
            extract_host(buf),
            HostDecision::Allow("2606:4700::1111".into())
        );
    }
}
