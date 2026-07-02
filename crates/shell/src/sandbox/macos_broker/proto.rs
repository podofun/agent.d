//! Wire protocol between the unprivileged daemon and the root `agentd-pf-broker`
//! (macOS). JSON-lines over a `UnixStream`: one serde-tagged message per line,
//! capped at [`MAX_LINE`] bytes. File descriptors (the spawned child's stdio)
//! travel out-of-band via `SCM_RIGHTS` immediately after the `Spawned` reply.
//!
//! Compiled on all unix targets so the codec is unit-tested everywhere; only
//! the broker binary and the pf plumbing are macOS-gated.

use std::io::{Read, Write};

use serde::{Deserialize, Serialize};

/// Hard cap on one wire line. A `Spawn` carries argv + an SBPL profile; 64 KiB
/// is far above any legitimate message and bounds a hostile peer.
pub const MAX_LINE: usize = 64 * 1024;

/// Protocol version; bumped on incompatible change. Sent in `Lease`.
pub const VERSION: u32 = 1;

/// Daemon → broker requests.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Req {
    /// Health check; broker answers `Resp::Ok`.
    Ping,
    /// Lease a dedicated sandbox uid for this connection's lifetime.
    Lease { v: u32 },
    /// Load the pf anchor redirecting the leased uid's traffic to these
    /// daemon-side loopback ports.
    Provision { tcp_port: u16, dns_port: u16 },
    /// Stamp per-uid filesystem ACLs on the granted paths (removed at teardown).
    Acl {
        read: Vec<String>,
        write: Vec<String>,
    },
    /// Spawn `bin` as the leased uid under `sandbox-exec -p <sbpl>`. Broker
    /// replies `Spawned` then passes [stdin_wr?, stdout_rd, stderr_rd] via
    /// `SCM_RIGHTS`, and later emits `Exit` when the child is reaped.
    Spawn {
        bin: String,
        args: Vec<String>,
        cwd: Option<String>,
        sbpl: String,
        want_stdin: bool,
    },
    /// `DIOCNATLOOK`: original destination of a redirected TCP connection, as
    /// seen by the relay (`src` = child's ephemeral endpoint, `dst` = relay).
    Natlook {
        proto: Proto,
        src: String,
        dst: String,
    },
    /// Block until the spawned child exits; reply is `Exit`. Sent by the daemon
    /// after the child's stdout/stderr reach EOF. Keeps the stream strictly
    /// request/response (no async events racing concurrent natlooks).
    Wait,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Proto {
    Tcp,
    Udp,
}

/// Broker → daemon replies/events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "resp", rename_all = "snake_case")]
pub enum Resp {
    Ok,
    Leased {
        uid: u32,
        user: String,
    },
    Spawned {
        pid: i32,
    },
    NatlookResult {
        orig: String,
    },
    /// Reply to `Wait`: the reaped child's exit code.
    Exit {
        code: i32,
    },
    Err {
        kind: ErrKind,
        msg: String,
    },
}

/// Machine-readable error class so the client can map to distinct
/// `ShellError`s (e.g. `PoolExhausted` is retryable; `Denied` is not).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrKind {
    /// All sandbox uids are leased; retry later.
    PoolExhausted,
    /// Peer/uid/version rejected.
    Denied,
    /// pfctl / ioctl / spawn failure; message has detail.
    Backend,
    /// Malformed or out-of-order request.
    Proto,
}

/// Codec error.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("message exceeds {MAX_LINE} bytes")]
    TooLong,
    #[error("peer closed")]
    Eof,
    #[error("bad message: {0}")]
    Decode(#[from] serde_json::Error),
}

/// Write one message as a single JSON line.
pub fn write_msg<W: Write, M: Serialize>(w: &mut W, msg: &M) -> Result<(), WireError> {
    let mut line = serde_json::to_vec(msg)?;
    if line.len() >= MAX_LINE {
        return Err(WireError::TooLong);
    }
    line.push(b'\n');
    w.write_all(&line)?;
    w.flush()?;
    Ok(())
}

/// Read one message. Reads a byte at a time (so it never consumes past the
/// newline — critical on the daemon side, where an `SCM_RIGHTS` fd message
/// follows a `Spawned` reply and must be `recvmsg`'d separately). Enforces
/// [`MAX_LINE`] BEFORE parsing so a hostile peer cannot balloon memory.
pub fn read_msg<R: Read, M: for<'de> Deserialize<'de>>(r: &mut R) -> Result<M, WireError> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match r.read(&mut byte)? {
            0 => {
                if line.is_empty() {
                    return Err(WireError::Eof);
                }
                break;
            }
            _ => {
                if byte[0] == b'\n' {
                    break;
                }
                line.push(byte[0]);
                if line.len() > MAX_LINE {
                    return Err(WireError::TooLong);
                }
            }
        }
    }
    Ok(serde_json::from_slice(&line)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    fn roundtrip<M: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug>(m: M) {
        let mut buf = Vec::new();
        write_msg(&mut buf, &m).unwrap();
        let mut r = BufReader::new(buf.as_slice());
        let back: M = read_msg(&mut r).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn every_req_roundtrips() {
        roundtrip(Req::Ping);
        roundtrip(Req::Lease { v: VERSION });
        roundtrip(Req::Provision {
            tcp_port: 4321,
            dns_port: 5353,
        });
        roundtrip(Req::Acl {
            read: vec!["/a".into()],
            write: vec!["/b".into(), "/c d".into()],
        });
        roundtrip(Req::Spawn {
            bin: "/usr/bin/curl".into(),
            args: vec!["-s".into(), "https://x".into()],
            cwd: Some("/tmp".into()),
            sbpl: "(version 1)\n(deny default)".into(),
            want_stdin: true,
        });
        roundtrip(Req::Natlook {
            proto: Proto::Tcp,
            src: "127.0.0.1:50123".into(),
            dst: "127.0.0.1:4321".into(),
        });
        roundtrip(Req::Wait);
    }

    #[test]
    fn every_resp_roundtrips() {
        roundtrip(Resp::Ok);
        roundtrip(Resp::Leased {
            uid: 701,
            user: "_agentd_sbx0".into(),
        });
        roundtrip(Resp::Spawned { pid: 4242 });
        roundtrip(Resp::NatlookResult {
            orig: "93.184.216.34:443".into(),
        });
        roundtrip(Resp::Exit { code: 0 });
        roundtrip(Resp::Err {
            kind: ErrKind::PoolExhausted,
            msg: "all uids leased".into(),
        });
    }

    #[test]
    fn unknown_op_is_decode_error() {
        let mut r = BufReader::new(&b"{\"op\":\"rm_rf_slash\"}\n"[..]);
        let e = read_msg::<_, Req>(&mut r).unwrap_err();
        assert!(matches!(e, WireError::Decode(_)));
    }

    #[test]
    fn oversized_line_rejected_without_parsing() {
        let big = vec![b'x'; MAX_LINE + 10];
        let mut r = BufReader::new(big.as_slice());
        let e = read_msg::<_, Req>(&mut r).unwrap_err();
        assert!(matches!(e, WireError::TooLong));
    }

    #[test]
    fn oversized_write_rejected() {
        let m = Req::Acl {
            read: vec!["x".repeat(MAX_LINE)],
            write: vec![],
        };
        let mut buf = Vec::new();
        assert!(matches!(write_msg(&mut buf, &m), Err(WireError::TooLong)));
        assert!(buf.is_empty(), "nothing written on reject");
    }

    #[test]
    fn eof_reported() {
        let mut r = BufReader::new(&b""[..]);
        assert!(matches!(read_msg::<_, Req>(&mut r), Err(WireError::Eof)));
    }

    #[test]
    fn two_messages_stream() {
        let mut buf = Vec::new();
        write_msg(&mut buf, &Req::Ping).unwrap();
        write_msg(&mut buf, &Req::Lease { v: 1 }).unwrap();
        let mut r = BufReader::new(buf.as_slice());
        assert_eq!(read_msg::<_, Req>(&mut r).unwrap(), Req::Ping);
        assert_eq!(read_msg::<_, Req>(&mut r).unwrap(), Req::Lease { v: 1 });
    }
}
