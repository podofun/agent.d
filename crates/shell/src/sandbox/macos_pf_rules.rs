//! Pure builders for the macOS pf anchor rules + the `DIOCNATLOOK` ABI.
//!
//! Compiled on every OS so the rule text and struct layout are unit-tested in
//! regular CI; only the ioctl call site (in the broker binary) is macOS-gated.
//!
//! Redirection model (sshuttle's proven pf method):
//! - A filter `pass out route-to (lo0 ...)` rule loops the sandbox uid's
//!   outbound packets (external destinations only) back through `lo0`.
//! - A translation `rdr pass on lo0 ... to !loopback` rule then rewrites them
//!   to the daemon's relay/DNS ports. Scoping the rdr to non-loopback
//!   destinations is what keeps the daemon's own loopback traffic (API server,
//!   the relay itself) untouched: only route-to'd packets carry an external
//!   dst on lo0.
//! - Everything else from the uid is `block drop` (both families, tcp+udp
//!   covered explicitly; pf default-deny for the uid is the block pair).
//!
//! Child → host-loopback is intentionally BLOCKED (no rdr match, no route-to
//! match, hits the block): unlike Linux — where the child gets its own
//! isolated loopback — a shared-host loopback pass would be lateral movement
//! into daemon-side services, so it fails closed unless the operator grants
//! nothing (there is no grant form for it; documented platform note).

/// pf anchor name for one sandbox uid. Namespaced so the broker can only ever
/// touch `agentd/sbx_*` state.
pub fn anchor_name(uid: u32) -> String {
    format!("agentd/sbx_{uid}")
}

/// Translation (rdr) rules: rewrite the uid's looped-back external traffic to
/// the daemon's relay ports. Loaded into the anchor's translation section.
pub fn build_rdr_rules(tcp_port: u16, dns_port: u16) -> String {
    format!(
        "rdr pass on lo0 inet  proto tcp from any to !127.0.0.0/8 -> 127.0.0.1 port {tcp_port}\n\
         rdr pass on lo0 inet6 proto tcp from any to !::1 -> ::1 port {tcp_port}\n\
         rdr pass on lo0 inet  proto udp from any to !127.0.0.0/8 port 53 -> 127.0.0.1 port {dns_port}\n\
         rdr pass on lo0 inet6 proto udp from any to !::1 port 53 -> ::1 port {dns_port}\n"
    )
}

/// Filter rules: default-deny the uid, then route external tcp + DNS udp via
/// `lo0` where the rdr rules take over. pf is last-match-wins, so the block
/// pair MUST come first.
pub fn build_filter_rules(uid: u32) -> String {
    format!(
        "block drop out proto tcp user {uid}\n\
         block drop out proto udp user {uid}\n\
         pass out route-to (lo0 127.0.0.1) inet  proto tcp from any to !127.0.0.0/8 user {uid} keep state\n\
         pass out route-to (lo0 ::1)       inet6 proto tcp from any to !::1 user {uid} keep state\n\
         pass out route-to (lo0 127.0.0.1) inet  proto udp from any to !127.0.0.0/8 port 53 user {uid} keep state\n\
         pass out route-to (lo0 ::1)       inet6 proto udp from any to !::1 port 53 user {uid} keep state\n"
    )
}

/// Complete anchor ruleset (translation first — pf requires rdr rules before
/// filter rules within a loaded ruleset).
pub fn build_anchor_rules(uid: u32, tcp_port: u16, dns_port: u16) -> String {
    format!(
        "{}{}",
        build_rdr_rules(tcp_port, dns_port),
        build_filter_rules(uid)
    )
}

/// Top-level hooks the installer adds once to the main ruleset so per-uid
/// anchors under `agentd/` are evaluated.
pub const MAIN_HOOKS: &str = "rdr-anchor \"agentd/*\"\nanchor \"agentd/*\"\n";

// ---- DIOCNATLOOK ABI (xnu bsd/net/pfvar.h) ----

/// `struct pf_addr`: 16-byte address union, network byte order; v4 occupies
/// the first 4 bytes.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PfAddr {
    pub v: [u8; 16],
}

/// `union pf_state_xport { u_int16_t port; u_int16_t call_id; u_int32_t spi }`
/// — 4 bytes; the port lives in the first 2, network order.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PfStateXport {
    pub raw: [u8; 4],
}

impl PfStateXport {
    pub fn from_port(port: u16) -> Self {
        let mut raw = [0u8; 4];
        raw[..2].copy_from_slice(&port.to_be_bytes());
        PfStateXport { raw }
    }
    pub fn port(&self) -> u16 {
        u16::from_be_bytes([self.raw[0], self.raw[1]])
    }
}

/// `struct pfioc_natlook`. Field order per xnu pfvar.h; 84 bytes.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PfiocNatlook {
    pub saddr: PfAddr,
    pub daddr: PfAddr,
    pub rsaddr: PfAddr,
    pub rdaddr: PfAddr,
    pub sxport: PfStateXport,
    pub dxport: PfStateXport,
    pub rsxport: PfStateXport,
    pub rdxport: PfStateXport,
    pub af: u8,
    pub proto: u8,
    pub proto_variant: u8,
    pub direction: u8,
}

/// pfvar.h direction constants.
pub const PF_OUT: u8 = 2;
/// AF constants match libc on darwin.
pub const AF_INET: u8 = 2;
pub const AF_INET6: u8 = 30;

/// `_IOWR('D', 23, struct pfioc_natlook)` computed against OUR struct size so
/// a layout drift breaks loudly at the const, not silently at the ioctl.
pub const DIOCNATLOOK: u64 = {
    const IOC_INOUT: u64 = 0xC000_0000;
    const IOCPARM_MASK: u64 = 0x1FFF;
    IOC_INOUT
        | ((std::mem::size_of::<PfiocNatlook>() as u64 & IOCPARM_MASK) << 16)
        | ((b'D' as u64) << 8)
        | 23
};

impl PfiocNatlook {
    /// Fill the lookup for a relay-accepted connection: `src` = the child's
    /// endpoint (relay's `peer_addr`), `dst` = the relay listener the packet
    /// was rewritten to (relay's `local_addr`). Kernel returns the original
    /// destination in `rdaddr`/`rdxport`.
    pub fn for_tcp(src: std::net::SocketAddr, dst: std::net::SocketAddr) -> Self {
        let mut n = PfiocNatlook {
            direction: PF_OUT,
            proto: 6, // IPPROTO_TCP
            ..Default::default()
        };
        n.af = match src {
            std::net::SocketAddr::V4(_) => AF_INET,
            std::net::SocketAddr::V6(_) => AF_INET6,
        };
        n.saddr = addr_of(src.ip());
        n.daddr = addr_of(dst.ip());
        n.sxport = PfStateXport::from_port(src.port());
        n.dxport = PfStateXport::from_port(dst.port());
        n
    }

    /// The original (pre-rdr) destination out of a completed lookup.
    pub fn original_dst(&self) -> Option<std::net::SocketAddr> {
        let port = self.rdxport.port();
        match self.af {
            AF_INET => {
                let o: [u8; 4] = self.rdaddr.v[..4].try_into().ok()?;
                Some(std::net::SocketAddr::new(
                    std::net::IpAddr::V4(std::net::Ipv4Addr::from(o)),
                    port,
                ))
            }
            AF_INET6 => Some(std::net::SocketAddr::new(
                std::net::IpAddr::V6(std::net::Ipv6Addr::from(self.rdaddr.v)),
                port,
            )),
            _ => None,
        }
    }
}

fn addr_of(ip: std::net::IpAddr) -> PfAddr {
    let mut a = PfAddr::default();
    match ip {
        std::net::IpAddr::V4(v4) => a.v[..4].copy_from_slice(&v4.octets()),
        std::net::IpAddr::V6(v6) => a.v = v6.octets(),
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natlook_struct_is_84_bytes_and_ioctl_matches() {
        assert_eq!(std::mem::size_of::<PfiocNatlook>(), 84);
        assert_eq!(DIOCNATLOOK, 0xC054_4417);
    }

    #[test]
    fn anchor_name_is_namespaced() {
        assert_eq!(anchor_name(701), "agentd/sbx_701");
    }

    #[test]
    fn blocks_precede_passes_last_match_wins() {
        let r = build_filter_rules(701);
        let first_block = r.find("block drop").unwrap();
        let first_pass = r.find("pass out").unwrap();
        assert!(first_block < first_pass, "block must come first");
        assert_eq!(r.matches("block drop").count(), 2, "tcp + udp");
        assert_eq!(r.matches("user 701").count(), 6, "every rule uid-scoped");
    }

    #[test]
    fn rdr_scoped_to_non_loopback_and_ports_embedded() {
        let r = build_rdr_rules(4321, 5353);
        assert_eq!(r.matches("to !127.0.0.0/8").count(), 2);
        assert_eq!(r.matches("to !::1").count(), 2);
        assert_eq!(r.matches("port 4321").count(), 2, "tcp relay both families");
        assert_eq!(r.matches("port 5353").count(), 2, "dns both families");
        assert!(!r.contains("user"), "translation rules cannot match user");
    }

    #[test]
    fn full_anchor_puts_translation_before_filter() {
        let r = build_anchor_rules(701, 4321, 5353);
        assert!(r.find("rdr pass").unwrap() < r.find("block drop").unwrap());
    }

    #[test]
    fn both_families_covered() {
        let r = build_anchor_rules(701, 1, 2);
        assert_eq!(r.matches("inet6").count(), 4, "2 rdr + 2 pass v6");
        // "inet " (with space) to not count inet6.
        assert_eq!(r.matches("inet ").count(), 4, "2 rdr + 2 pass v4");
    }

    #[test]
    fn natlook_roundtrip_v4() {
        let src: std::net::SocketAddr = "127.0.0.1:50123".parse().unwrap();
        let dst: std::net::SocketAddr = "127.0.0.1:4321".parse().unwrap();
        let mut n = PfiocNatlook::for_tcp(src, dst);
        assert_eq!(n.af, AF_INET);
        assert_eq!(n.proto, 6);
        assert_eq!(n.direction, PF_OUT);
        assert_eq!(n.sxport.port(), 50123);
        // Simulate the kernel writing the original dst back.
        n.rdaddr.v[..4].copy_from_slice(&[93, 184, 216, 34]);
        n.rdxport = PfStateXport::from_port(443);
        assert_eq!(
            n.original_dst().unwrap(),
            "93.184.216.34:443".parse().unwrap()
        );
    }

    #[test]
    fn natlook_roundtrip_v6() {
        let src: std::net::SocketAddr = "[::1]:50123".parse().unwrap();
        let dst: std::net::SocketAddr = "[::1]:4321".parse().unwrap();
        let mut n = PfiocNatlook::for_tcp(src, dst);
        assert_eq!(n.af, AF_INET6);
        n.rdaddr.v = "2606:4700::1111".parse::<std::net::Ipv6Addr>().unwrap().octets();
        n.rdxport = PfStateXport::from_port(443);
        assert_eq!(
            n.original_dst().unwrap(),
            "[2606:4700::1111]:443".parse().unwrap()
        );
    }
}
