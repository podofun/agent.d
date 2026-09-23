//! UDP egress for the transparent netns backend (QUIC/HTTP/3, STUN, game and
//! media protocols: any UDP except DNS, which `gateway::handle_dns` answers).
//!
//! In the netns, nft REDIRECTs every non-loopback UDP datagram to the
//! supervisor's UDP intercept. The kernel only reports a redirected socket's
//! original destination for TCP (`SO_ORIGINAL_DST`), so the supervisor asks
//! conntrack over `NETLINK_NETFILTER` instead, using the flow's reply tuple.
//! Each datagram then crosses to the host as a frame
//! `[client:19][dst:19][payload]` over a SEQPACKET socketpair.
//!
//! The host admits a flow only if its destination IP is in the permitted set
//! (the same set the TCP relay uses), sends through a UDP socket connected to
//! that destination, and returns replies as frames. The supervisor sends each
//! reply from its intercept socket to the client; conntrack rewrites the
//! source back to the original destination, so connected sockets (QUIC)
//! accept it.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nix::sys::socket::{MsgFlags, recv, send};

use super::linux_transparent::{decode_dst, encode_dst};
use crate::gateway::{SharedSet, admit};

const ADDR_LEN: usize = 19;
const HEADER_LEN: usize = 2 * ADDR_LEN;
const MAX_DATAGRAM: usize = 65_535;

/// Flows open at once per command; datagrams for new flows beyond it are dropped.
const MAX_FLOWS: usize = 256;
/// A flow with no traffic either way for this long is closed.
const FLOW_IDLE: Duration = Duration::from_secs(60);
/// How often an idle flow thread wakes to check for shutdown.
const POLL: Duration = Duration::from_millis(500);
/// Conntrack keeps a redirected flow for at least 30 s after its last packet,
/// so a lookup stays valid for its client address well within that.
const LOOKUP_TTL: Duration = Duration::from_secs(5);

pub(super) fn encode_frame(client: &SocketAddr, dst: &SocketAddr, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(&encode_dst(client));
    out.extend_from_slice(&encode_dst(dst));
    out.extend_from_slice(payload);
    out
}

pub(super) fn decode_frame(frame: &[u8]) -> Option<(SocketAddr, SocketAddr, &[u8])> {
    if frame.len() < HEADER_LEN {
        return None;
    }
    let client = decode_dst(&frame[..ADDR_LEN])?;
    let dst = decode_dst(&frame[ADDR_LEN..HEADER_LEN])?;
    Some((client, dst, &frame[HEADER_LEN..]))
}

const NETLINK_NETFILTER: i32 = 12;
const NLMSG_ERROR: u16 = 2;
const NLM_F_REQUEST: u16 = 1;
const NFNL_SUBSYS_CTNETLINK: u16 = 1;
const IPCTNL_MSG_CT_GET: u16 = 1;
const NLA_F_NESTED: u16 = 0x8000;
const NLA_TYPE_MASK: u16 = 0x3fff;
const CTA_TUPLE_ORIG: u16 = 1;
const CTA_TUPLE_REPLY: u16 = 2;
const CTA_TUPLE_IP: u16 = 1;
const CTA_TUPLE_PROTO: u16 = 2;
const CTA_IP_V4_SRC: u16 = 1;
const CTA_IP_V4_DST: u16 = 2;
const CTA_IP_V6_SRC: u16 = 3;
const CTA_IP_V6_DST: u16 = 4;
const CTA_PROTO_NUM: u16 = 1;
const CTA_PROTO_SRC_PORT: u16 = 2;
const CTA_PROTO_DST_PORT: u16 = 3;

fn push_attr(buf: &mut Vec<u8>, ty: u16, data: &[u8]) {
    buf.extend_from_slice(&((4 + data.len()) as u16).to_ne_bytes());
    buf.extend_from_slice(&ty.to_ne_bytes());
    buf.extend_from_slice(data);
    while !buf.len().is_multiple_of(4) {
        buf.push(0);
    }
}

fn nested(ty: u16, inner: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    push_attr(&mut out, ty | NLA_F_NESTED, inner);
    out
}

fn ip_bytes(ip: IpAddr) -> Vec<u8> {
    match ip {
        IpAddr::V4(v4) => v4.octets().to_vec(),
        IpAddr::V6(v6) => v6.octets().to_vec(),
    }
}

/// `IPCTNL_MSG_CT_GET` for the UDP conntrack entry whose *reply* direction is
/// `reply_src -> reply_dst` (the intercept socket answering the client).
pub(super) fn build_ct_get(seq: u32, reply_src: SocketAddr, reply_dst: SocketAddr) -> Vec<u8> {
    let (family, src_ty, dst_ty) = match reply_src.ip() {
        IpAddr::V4(_) => (libc::AF_INET as u8, CTA_IP_V4_SRC, CTA_IP_V4_DST),
        IpAddr::V6(_) => (libc::AF_INET6 as u8, CTA_IP_V6_SRC, CTA_IP_V6_DST),
    };
    let mut ip = Vec::new();
    push_attr(&mut ip, src_ty, &ip_bytes(reply_src.ip()));
    push_attr(&mut ip, dst_ty, &ip_bytes(reply_dst.ip()));
    let mut proto = Vec::new();
    push_attr(&mut proto, CTA_PROTO_NUM, &[libc::IPPROTO_UDP as u8]);
    push_attr(
        &mut proto,
        CTA_PROTO_SRC_PORT,
        &reply_src.port().to_be_bytes(),
    );
    push_attr(
        &mut proto,
        CTA_PROTO_DST_PORT,
        &reply_dst.port().to_be_bytes(),
    );
    let mut tuple = nested(CTA_TUPLE_IP, &ip);
    tuple.extend_from_slice(&nested(CTA_TUPLE_PROTO, &proto));
    let attrs = nested(CTA_TUPLE_REPLY, &tuple);

    let len = 16 + 4 + attrs.len();
    let mut msg = Vec::with_capacity(len);
    msg.extend_from_slice(&(len as u32).to_ne_bytes());
    msg.extend_from_slice(&((NFNL_SUBSYS_CTNETLINK << 8) | IPCTNL_MSG_CT_GET).to_ne_bytes());
    msg.extend_from_slice(&NLM_F_REQUEST.to_ne_bytes());
    msg.extend_from_slice(&seq.to_ne_bytes());
    msg.extend_from_slice(&0u32.to_ne_bytes()); // portid: kernel fills it in
    msg.extend_from_slice(&[family, 0, 0, 0]); // nfgenmsg: family, version, res_id
    msg.extend_from_slice(&attrs);
    msg
}

/// Iterate `(type, payload)` over a netlink attribute stream.
fn attrs(mut buf: &[u8]) -> impl Iterator<Item = (u16, &[u8])> {
    std::iter::from_fn(move || {
        if buf.len() < 4 {
            return None;
        }
        let len = u16::from_ne_bytes([buf[0], buf[1]]) as usize;
        let ty = u16::from_ne_bytes([buf[2], buf[3]]) & NLA_TYPE_MASK;
        if len < 4 || len > buf.len() {
            return None;
        }
        let payload = &buf[4..len];
        buf = &buf[len.div_ceil(4).saturating_mul(4).min(buf.len())..];
        Some((ty, payload))
    })
}

fn find(buf: &[u8], ty: u16) -> Option<&[u8]> {
    attrs(buf).find(|(t, _)| *t == ty).map(|(_, p)| p)
}

/// Pull the original-direction destination out of a conntrack GET reply.
pub(super) fn parse_ct_reply(msg: &[u8]) -> io::Result<SocketAddr> {
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "malformed conntrack reply");
    if msg.len() < 16 {
        return Err(bad());
    }
    let ty = u16::from_ne_bytes([msg[4], msg[5]]);
    if ty == NLMSG_ERROR {
        let code = msg
            .get(16..20)
            .map(|b| i32::from_ne_bytes([b[0], b[1], b[2], b[3]]))
            .ok_or_else(bad)?;
        return Err(io::Error::from_raw_os_error(-code));
    }
    let len = (u32::from_ne_bytes([msg[0], msg[1], msg[2], msg[3]]) as usize).min(msg.len());
    let body = msg.get(20..len).ok_or_else(bad)?;
    let orig = find(body, CTA_TUPLE_ORIG).ok_or_else(bad)?;
    let ip = find(orig, CTA_TUPLE_IP).ok_or_else(bad)?;
    let proto = find(orig, CTA_TUPLE_PROTO).ok_or_else(bad)?;
    let port = find(proto, CTA_PROTO_DST_PORT)
        .and_then(|p| p.try_into().ok())
        .map(u16::from_be_bytes)
        .ok_or_else(bad)?;
    let addr = if let Some(v4) = find(ip, CTA_IP_V4_DST) {
        let o: [u8; 4] = v4.try_into().map_err(|_| bad())?;
        IpAddr::from(o)
    } else {
        let v6 = find(ip, CTA_IP_V6_DST).ok_or_else(bad)?;
        let o: [u8; 16] = v6.try_into().map_err(|_| bad())?;
        IpAddr::from(o)
    };
    Ok(SocketAddr::new(addr, port))
}

/// A conntrack netlink socket in the supervisor's (the netns owner's) namespace.
pub(super) struct Conntrack {
    fd: OwnedFd,
    seq: u32,
    cache: HashMap<SocketAddr, (SocketAddr, Instant)>,
}

impl Conntrack {
    pub(super) fn open() -> io::Result<Self> {
        use std::os::fd::FromRawFd;
        // SAFETY: plain socket(2); the fd is adopted immediately.
        let raw = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                NETLINK_NETFILTER,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a fresh, owned descriptor.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let tv = libc::timeval {
            tv_sec: 1,
            tv_usec: 0,
        };
        // SAFETY: valid fd and a stack timeval of the right size.
        unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                &tv as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::timeval>() as libc::socklen_t,
            );
        }
        Ok(Self {
            fd,
            seq: 0,
            cache: HashMap::new(),
        })
    }

    /// Original destination of the datagram `client` sent that arrived on the
    /// intercept socket bound at `local`.
    pub(super) fn original_dst(
        &mut self,
        local: SocketAddr,
        client: SocketAddr,
    ) -> io::Result<SocketAddr> {
        let now = Instant::now();
        if let Some((dst, at)) = self.cache.get(&client)
            && now.duration_since(*at) < LOOKUP_TTL
        {
            return Ok(*dst);
        }
        self.seq = self.seq.wrapping_add(1);
        let req = build_ct_get(self.seq, local, client);
        let fd = self.fd.as_raw_fd();
        send(fd, &req, MsgFlags::empty()).map_err(io::Error::from)?;
        let mut buf = [0u8; 4096];
        let n = recv(fd, &mut buf, MsgFlags::empty()).map_err(io::Error::from)?;
        let dst = parse_ct_reply(&buf[..n])?;
        if self.cache.len() > 4 * MAX_FLOWS {
            self.cache
                .retain(|_, (_, at)| now.duration_since(*at) < LOOKUP_TTL);
        }
        self.cache.insert(client, (dst, now));
        Ok(dst)
    }
}

/// Supervisor: forward every datagram arriving on `sock` to the host with its
/// original destination. Datagrams whose destination cannot be recovered are
/// dropped.
pub(super) fn spawn_supervisor_intercept(sock: Arc<UdpSocket>, relay_fd: RawFd) {
    std::thread::spawn(move || {
        let Ok(local) = sock.local_addr() else { return };
        let Ok(mut ct) = Conntrack::open() else {
            return;
        };
        let mut buf = vec![0u8; MAX_DATAGRAM];
        while let Ok((n, client)) = sock.recv_from(&mut buf) {
            let Ok(dst) = ct.original_dst(local, client) else {
                continue;
            };
            let frame = encode_frame(&client, &dst, &buf[..n]);
            if send(relay_fd, &frame, MsgFlags::empty()).is_err() {
                break;
            }
        }
    });
}

/// Supervisor: deliver host replies to their clients from the intercept socket
/// of the matching family. Returns when the host side closes.
pub(super) fn spawn_supervisor_replies(
    relay_fd: RawFd,
    v4: Arc<UdpSocket>,
    v6: Option<Arc<UdpSocket>>,
) {
    std::thread::spawn(move || {
        let mut buf = vec![0u8; HEADER_LEN + MAX_DATAGRAM];
        loop {
            let n = match recv(relay_fd, &mut buf, MsgFlags::empty()) {
                Ok(n) if n > 0 => n,
                _ => break,
            };
            let Some((client, _dst, payload)) = decode_frame(&buf[..n]) else {
                continue;
            };
            let sock = match (client, &v6) {
                (SocketAddr::V6(_), Some(v6)) => v6,
                (SocketAddr::V6(_), None) => continue,
                (SocketAddr::V4(_), _) => &v4,
            };
            let _ = sock.send_to(payload, client);
        }
    });
}

type FlowKey = (SocketAddr, SocketAddr);

/// Host: relay framed datagrams from the supervisor to admitted destinations
/// and frame the replies back. Returns when the supervisor side closes; every
/// flow thread stops within [`POLL`] of that.
pub(super) fn host_relay_loop(relay: OwnedFd, set: SharedSet) {
    let relay = Arc::new(relay);
    let stop = Arc::new(AtomicBool::new(false));
    let flows: Arc<Mutex<HashMap<FlowKey, Arc<UdpSocket>>>> = Arc::default();
    let mut buf = vec![0u8; HEADER_LEN + MAX_DATAGRAM];
    loop {
        let n = match recv(relay.as_raw_fd(), &mut buf, MsgFlags::empty()) {
            Ok(n) if n > 0 => n,
            _ => break,
        };
        let Some((client, dst, payload)) = decode_frame(&buf[..n]) else {
            continue;
        };
        let existing = flows.lock().unwrap().get(&(client, dst)).cloned();
        let sock = match existing {
            Some(s) => s,
            None => {
                if !admit(dst.ip(), &set) || flows.lock().unwrap().len() >= MAX_FLOWS {
                    continue;
                }
                let Ok(sock) = open_flow(dst) else { continue };
                flows.lock().unwrap().insert((client, dst), sock.clone());
                spawn_flow_replies(
                    (client, dst),
                    sock.clone(),
                    relay.clone(),
                    flows.clone(),
                    stop.clone(),
                );
                sock
            }
        };
        let _ = sock.send(payload);
    }
    stop.store(true, Ordering::Relaxed);
}

fn open_flow(dst: SocketAddr) -> io::Result<Arc<UdpSocket>> {
    let bind: SocketAddr = match dst {
        SocketAddr::V4(_) => "0.0.0.0:0".parse().unwrap(),
        SocketAddr::V6(_) => "[::]:0".parse().unwrap(),
    };
    let sock = UdpSocket::bind(bind)?;
    sock.connect(dst)?;
    sock.set_read_timeout(Some(POLL))?;
    Ok(Arc::new(sock))
}

fn spawn_flow_replies(
    key: FlowKey,
    sock: Arc<UdpSocket>,
    relay: Arc<OwnedFd>,
    flows: Arc<Mutex<HashMap<FlowKey, Arc<UdpSocket>>>>,
    stop: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        let mut buf = vec![0u8; MAX_DATAGRAM];
        let mut last = Instant::now();
        while !stop.load(Ordering::Relaxed) && last.elapsed() < FLOW_IDLE {
            match sock.recv(&mut buf) {
                Ok(n) => {
                    last = Instant::now();
                    let frame = encode_frame(&key.0, &key.1, &buf[..n]);
                    if send(relay.as_raw_fd(), &frame, MsgFlags::empty()).is_err() {
                        break;
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                // ICMP errors (e.g. port unreachable) surface here; keep the flow.
                Err(_) => std::thread::sleep(POLL),
            }
        }
        flows.lock().unwrap().remove(&key);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn frame_round_trips_both_families() {
        let f = encode_frame(&sa("127.0.0.1:40000"), &sa("[2001:db8::1]:443"), b"quic");
        let (c, d, p) = decode_frame(&f).unwrap();
        assert_eq!(c, sa("127.0.0.1:40000"));
        assert_eq!(d, sa("[2001:db8::1]:443"));
        assert_eq!(p, b"quic");
        assert!(decode_frame(&f[..10]).is_none());
    }

    #[test]
    fn ct_get_request_carries_the_reply_tuple() {
        let m = build_ct_get(7, sa("127.0.0.1:5000"), sa("127.0.0.1:6000"));
        assert_eq!(
            u32::from_ne_bytes(m[0..4].try_into().unwrap()) as usize,
            m.len()
        );
        assert_eq!(u16::from_ne_bytes([m[4], m[5]]), 0x0101);
        assert_eq!(m[16], libc::AF_INET as u8);
        let reply = find(&m[20..], CTA_TUPLE_REPLY).unwrap();
        let ip = find(reply, CTA_TUPLE_IP).unwrap();
        assert_eq!(find(ip, CTA_IP_V4_SRC).unwrap(), &[127, 0, 0, 1]);
        let proto = find(reply, CTA_TUPLE_PROTO).unwrap();
        assert_eq!(find(proto, CTA_PROTO_NUM).unwrap(), &[17]);
        assert_eq!(
            find(proto, CTA_PROTO_SRC_PORT).unwrap(),
            &5000u16.to_be_bytes()
        );
        assert_eq!(
            find(proto, CTA_PROTO_DST_PORT).unwrap(),
            &6000u16.to_be_bytes()
        );
    }

    /// A conntrack reply names the original destination in CTA_TUPLE_ORIG.
    #[test]
    fn parses_original_destination_from_a_reply() {
        let mut ip = Vec::new();
        push_attr(&mut ip, CTA_IP_V6_SRC, &ip_bytes("::1".parse().unwrap()));
        push_attr(
            &mut ip,
            CTA_IP_V6_DST,
            &ip_bytes("2001:db8::7".parse().unwrap()),
        );
        let mut proto = Vec::new();
        push_attr(&mut proto, CTA_PROTO_NUM, &[17]);
        push_attr(&mut proto, CTA_PROTO_SRC_PORT, &41000u16.to_be_bytes());
        push_attr(&mut proto, CTA_PROTO_DST_PORT, &443u16.to_be_bytes());
        let mut tuple = nested(CTA_TUPLE_IP, &ip);
        tuple.extend_from_slice(&nested(CTA_TUPLE_PROTO, &proto));
        let attrs = nested(CTA_TUPLE_ORIG, &tuple);
        let mut msg = vec![0u8; 20];
        let len = (20 + attrs.len()) as u32;
        msg[0..4].copy_from_slice(&len.to_ne_bytes());
        msg[4..6].copy_from_slice(&0x0101u16.to_ne_bytes());
        msg.extend_from_slice(&attrs);
        assert_eq!(parse_ct_reply(&msg).unwrap(), sa("[2001:db8::7]:443"));
    }

    #[test]
    fn netlink_error_becomes_an_os_error() {
        let mut msg = vec![0u8; 36];
        msg[0..4].copy_from_slice(&36u32.to_ne_bytes());
        msg[4..6].copy_from_slice(&NLMSG_ERROR.to_ne_bytes());
        msg[16..20].copy_from_slice(&(-libc::ENOENT).to_ne_bytes());
        let err = parse_ct_reply(&msg).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ENOENT));
    }
}
