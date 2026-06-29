//! nftables ruleset construction for the Linux transparent backend.
//!
//! The construction is pure and unit-tested here; applying it (`nft -f` inside
//! the netns) lives in `sandbox::linux_transparent`.

/// Architecture-B NAT ruleset: transparently REDIRECT the child's egress to the
/// in-namespace intercept ports. All outbound TCP (except loopback) is bounced to
/// `tcp_port`; outbound UDP/53 (DNS) to `dns_port`. The original destination is
/// recovered host-side via `SO_ORIGINAL_DST`. Non-DNS UDP (e.g. QUIC) is left
/// unredirected and has no route out of the loopback-only netns — a documented
/// residual (apps fall back to TCP).
///
/// DNS (udp/53) is redirected FIRST, before the loopback accept, so queries aimed
/// at a loopback stub resolver (systemd-resolved's 127.0.0.53, very common) are
/// still captured.
pub fn build_nat_ruleset(table: &str, tcp_port: u16, dns_port: u16) -> String {
    format!(
        "table inet {table} {{\n\
         \tchain output {{\n\
         \t\ttype nat hook output priority -100; policy accept;\n\
         \t\tudp dport 53 redirect to :{dns_port}\n\
         \t\tip daddr 127.0.0.0/8 accept\n\
         \t\tip6 daddr ::1 accept\n\
         \t\tmeta l4proto tcp redirect to :{tcp_port}\n\
         \t}}\n\
         }}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nat_ruleset_redirects_tcp_and_dns_only() {
        let rs = build_nat_ruleset("sbxnat", 40001, 40002);
        assert!(rs.contains("type nat hook output priority -100;"));
        assert!(rs.contains("udp dport 53 redirect to :40002"));
        assert!(rs.contains("meta l4proto tcp redirect to :40001"));
        // Loopback is never redirected (the intercept lives on loopback).
        assert!(rs.contains("ip daddr 127.0.0.0/8 accept"));
        assert!(rs.contains("ip6 daddr ::1 accept"));
        // DNS redirect precedes the loopback accept (catches 127.0.0.53 stubs).
        let dns_at = rs.find("udp dport 53").unwrap();
        let lo_at = rs.find("127.0.0.0/8").unwrap();
        assert!(dns_at < lo_at);
    }
}
