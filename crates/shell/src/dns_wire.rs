//! Minimal DNS wire codec for the sandbox resolver.
//!
//! The transparent Linux path redirects the child's UDP/53 queries to an
//! in-namespace listener that bridges them to the host resolver. We only need to
//! (a) parse a single-question query to extract the name + type, and (b) synth a
//! response carrying A/AAAA records (or NXDOMAIN). This is deliberately tiny —
//! not a general DNS library — and totally bounds-checked so malformed child
//! input can never panic the resolver.

use std::net::{Ipv4Addr, Ipv6Addr};

pub const TYPE_A: u16 = 1;
pub const TYPE_AAAA: u16 = 28;
const CLASS_IN: u16 = 1;

/// A parsed question: the transaction id, the queried name (lowercased, no
/// trailing dot), and the qtype.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub id: u16,
    pub name: String,
    pub qtype: u16,
    /// The raw question section bytes (qname+qtype+qclass), echoed verbatim into
    /// the response so we never have to re-encode the name.
    pub qbytes: Vec<u8>,
}

/// Parse a DNS query packet. Returns `None` on anything malformed or not a
/// single-question standard query.
pub fn parse_query(buf: &[u8]) -> Option<Question> {
    if buf.len() < 12 {
        return None;
    }
    let id = u16::from_be_bytes([buf[0], buf[1]]);
    let flags = u16::from_be_bytes([buf[2], buf[3]]);
    // QR must be 0 (query); opcode 0 (standard). QDCOUNT must be 1.
    if flags & 0x8000 != 0 {
        return None;
    }
    if (flags >> 11) & 0xF != 0 {
        return None;
    }
    let qdcount = u16::from_be_bytes([buf[4], buf[5]]);
    if qdcount != 1 {
        return None;
    }
    // Parse QNAME starting at offset 12 (no compression in a query).
    let mut pos = 12;
    let mut labels: Vec<String> = Vec::new();
    loop {
        let len = *buf.get(pos)? as usize;
        if len == 0 {
            pos += 1;
            break;
        }
        if len & 0xC0 != 0 {
            return None; // compression pointer in a query: reject
        }
        pos += 1;
        let end = pos.checked_add(len)?;
        let label = buf.get(pos..end)?;
        labels.push(String::from_utf8_lossy(label).to_ascii_lowercase());
        pos = end;
    }
    let qtype = u16::from_be_bytes([*buf.get(pos)?, *buf.get(pos + 1)?]);
    let qclass = u16::from_be_bytes([*buf.get(pos + 2)?, *buf.get(pos + 3)?]);
    if qclass != CLASS_IN {
        return None;
    }
    let qend = pos + 4;
    let qbytes = buf.get(12..qend)?.to_vec();
    Some(Question {
        id,
        name: labels.join("."),
        qtype,
        qbytes,
    })
}

/// Build a response echoing the question and carrying `answers` as A/AAAA
/// records with `ttl`. An empty `answers` slice yields NOERROR/no-answer (used
/// when an allowed name resolved to nothing of the requested family).
pub fn build_response(q: &Question, answers: &[std::net::IpAddr], ttl: u32) -> Vec<u8> {
    let typed: Vec<&std::net::IpAddr> = answers
        .iter()
        .filter(|ip| {
            matches!(
                (q.qtype, ip),
                (TYPE_A, std::net::IpAddr::V4(_)) | (TYPE_AAAA, std::net::IpAddr::V6(_))
            )
        })
        .collect();
    let mut out = Vec::with_capacity(12 + q.qbytes.len() + typed.len() * 16);
    out.extend_from_slice(&q.id.to_be_bytes());
    out.extend_from_slice(&0x8180u16.to_be_bytes()); // QR=1, RD=1, RA=1, RCODE=0
    out.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    out.extend_from_slice(&(typed.len() as u16).to_be_bytes()); // ANCOUNT
    out.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
    out.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT
    out.extend_from_slice(&q.qbytes); // question echoed
    for ip in typed {
        out.extend_from_slice(&[0xC0, 0x0C]); // name pointer -> offset 12
        match ip {
            std::net::IpAddr::V4(v4) => {
                out.extend_from_slice(&TYPE_A.to_be_bytes());
                out.extend_from_slice(&CLASS_IN.to_be_bytes());
                out.extend_from_slice(&ttl.to_be_bytes());
                out.extend_from_slice(&4u16.to_be_bytes());
                out.extend_from_slice(&v4.octets());
            }
            std::net::IpAddr::V6(v6) => {
                out.extend_from_slice(&TYPE_AAAA.to_be_bytes());
                out.extend_from_slice(&CLASS_IN.to_be_bytes());
                out.extend_from_slice(&ttl.to_be_bytes());
                out.extend_from_slice(&16u16.to_be_bytes());
                out.extend_from_slice(&v6.octets());
            }
        }
    }
    out
}

/// Build an NXDOMAIN response for a denied name.
pub fn build_nxdomain(q: &Question) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + q.qbytes.len());
    out.extend_from_slice(&q.id.to_be_bytes());
    out.extend_from_slice(&0x8183u16.to_be_bytes()); // QR=1, RD=1, RA=1, RCODE=3 (NXDOMAIN)
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&q.qbytes);
    out
}

/// Encode a QNAME (for tests / building queries).
pub fn encode_qname(name: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for label in name.split('.').filter(|l| !l.is_empty()) {
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
    out
}

/// Build a query packet (for tests).
pub fn build_query(id: u16, name: &str, qtype: u16) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&0x0100u16.to_be_bytes()); // RD
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&encode_qname(name));
    out.extend_from_slice(&qtype.to_be_bytes());
    out.extend_from_slice(&CLASS_IN.to_be_bytes());
    out
}

/// Parse the A/AAAA answers out of a response (for tests).
pub fn parse_answers(buf: &[u8]) -> Vec<std::net::IpAddr> {
    let mut out = Vec::new();
    if buf.len() < 12 {
        return out;
    }
    let ancount = u16::from_be_bytes([buf[6], buf[7]]) as usize;
    // Skip header + question.
    let Some(q) = parse_query_echo(buf) else {
        return out;
    };
    let mut pos = 12 + q;
    for _ in 0..ancount {
        // name (pointer = 2 bytes, or labels). Assume pointer.
        if pos + 12 > buf.len() {
            break;
        }
        let name_len = if buf[pos] & 0xC0 == 0xC0 { 2 } else { 1 };
        pos += name_len;
        let rtype = u16::from_be_bytes([buf[pos], buf[pos + 1]]);
        pos += 8; // type(2)+class(2)+ttl(4)
        let rdlen = u16::from_be_bytes([buf[pos], buf[pos + 1]]) as usize;
        pos += 2;
        if pos + rdlen > buf.len() {
            break;
        }
        match (rtype, rdlen) {
            (TYPE_A, 4) => {
                let o: [u8; 4] = buf[pos..pos + 4].try_into().unwrap();
                out.push(Ipv4Addr::from(o).into());
            }
            (TYPE_AAAA, 16) => {
                let o: [u8; 16] = buf[pos..pos + 16].try_into().unwrap();
                out.push(Ipv6Addr::from(o).into());
            }
            _ => {}
        }
        pos += rdlen;
    }
    out
}

// Length of the question section (qname+4) for skipping; minimal.
fn parse_query_echo(buf: &[u8]) -> Option<usize> {
    let mut pos = 12;
    loop {
        let len = *buf.get(pos)? as usize;
        pos += 1;
        if len == 0 {
            break;
        }
        pos += len;
    }
    Some(pos + 4 - 12)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn parse_roundtrips_name_and_type() {
        let q = build_query(0x1234, "api.example.com", TYPE_A);
        let parsed = parse_query(&q).unwrap();
        assert_eq!(parsed.id, 0x1234);
        assert_eq!(parsed.name, "api.example.com");
        assert_eq!(parsed.qtype, TYPE_A);
    }

    #[test]
    fn name_is_lowercased() {
        let q = build_query(1, "API.Example.COM", TYPE_AAAA);
        assert_eq!(parse_query(&q).unwrap().name, "api.example.com");
    }

    #[test]
    fn response_carries_matching_family_only() {
        let q = parse_query(&build_query(7, "h.test", TYPE_A)).unwrap();
        let resp = build_response(&q, &[ip("1.2.3.4"), ip("2606:4700::1")], 60);
        let ans = parse_answers(&resp);
        assert_eq!(ans, vec![ip("1.2.3.4")]); // AAAA filtered out for an A query
    }

    #[test]
    fn aaaa_query_gets_v6() {
        let q = parse_query(&build_query(7, "h.test", TYPE_AAAA)).unwrap();
        let resp = build_response(&q, &[ip("1.2.3.4"), ip("2606:4700::1")], 60);
        assert_eq!(parse_answers(&resp), vec![ip("2606:4700::1")]);
    }

    #[test]
    fn nxdomain_has_rcode_3_and_no_answers() {
        let q = parse_query(&build_query(9, "evil.test", TYPE_A)).unwrap();
        let resp = build_nxdomain(&q);
        assert_eq!(resp[3] & 0x0F, 3); // RCODE = NXDOMAIN
        assert!(parse_answers(&resp).is_empty());
        assert_eq!(u16::from_be_bytes([resp[0], resp[1]]), 9); // id echoed
    }

    #[test]
    fn malformed_never_panics() {
        for bad in [&b""[..], &b"\x00"[..], &[0u8; 12][..], &[0xFFu8; 20][..]] {
            let _ = parse_query(bad); // must not panic
        }
        // compression pointer in query rejected
        let mut q = build_query(1, "a.b", TYPE_A);
        q[12] = 0xC0;
        assert!(parse_query(&q).is_none());
    }

    #[test]
    fn rejects_multi_question_and_responses() {
        let mut q = build_query(1, "a.b", TYPE_A);
        q[5] = 2; // QDCOUNT=2
        assert!(parse_query(&q).is_none());
        let mut r = build_query(1, "a.b", TYPE_A);
        r[2] = 0x80; // QR=1 (response)
        assert!(parse_query(&r).is_none());
    }
}
