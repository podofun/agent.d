//! Windows transparent network backend: WFP ALE filters scoped to the child's
//! AppContainer package SID (driver-free, the same mechanism Windows Firewall
//! and simplewall use).
//!
//! Model (Architecture, Windows variant):
//! - The child runs in an AppContainer **with** the `internetClient` capability
//!   (so the OS lets it attempt DNS/TCP), but a WFP filter set scoped to its
//!   package SID **default-denies** all outbound and permits only specific
//!   remote IPs.
//! - DNS-pin is daemon-side **pre-resolution**: we resolve the policy's
//!   `net:<host>` grants up front, permit the resulting IPs (+ literal `net:<ip>`
//!   grants + the configured DNS servers), and refresh on a timer. There is no
//!   transparent redirect (that needs a callout driver), so this is IP-level
//!   only, with a documented multi-IP/TTL staleness window.
//! - Filters live in a WFP **dynamic session**: they are auto-removed when the
//!   engine handle closes (process exit / teardown), so nothing leaks.

#![cfg(target_os = "windows")]

use std::net::{IpAddr, Ipv4Addr};

use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::NetworkManagement::WindowsFilteringPlatform::{
    FWP_ACTION_BLOCK, FWP_ACTION_PERMIT, FWP_ACTION_TYPE, FWP_BYTE_ARRAY16, FWP_BYTE_ARRAY16_TYPE,
    FWP_CONDITION_VALUE0, FWP_CONDITION_VALUE0_0, FWP_MATCH_EQUAL, FWP_SID, FWP_UINT8, FWP_UINT32,
    FWP_VALUE0, FWP_VALUE0_0, FWPM_CONDITION_ALE_PACKAGE_ID, FWPM_CONDITION_IP_REMOTE_ADDRESS,
    FWPM_FILTER_CONDITION0, FWPM_FILTER0, FWPM_LAYER_ALE_AUTH_CONNECT_V4,
    FWPM_LAYER_ALE_AUTH_CONNECT_V6, FWPM_SESSION_FLAG_DYNAMIC, FWPM_SESSION0, FWPM_SUBLAYER0,
    FwpmEngineClose0, FwpmEngineOpen0, FwpmFilterAdd0, FwpmSubLayerAdd0,
};
use windows::Win32::System::Rpc::RPC_C_AUTHN_WINNT;
use windows::core::GUID;

use crate::netfilter::{FilterError, FilterHandle, NetFilter, Supports};

/// Owns the WFP engine handle for one identity; closed at teardown.
pub struct EngineHandle(pub windows::Win32::Foundation::HANDLE);

/// A live WFP engine session owning the filter set for one sandboxed identity.
pub struct WfpHandle {
    engine: EngineHandle,
    sublayer: GUID,
}

impl FilterHandle for WfpHandle {}

// SAFETY: the engine handle is a kernel handle usable across threads; we only
// touch it under the host's single owning task.
unsafe impl Send for WfpHandle {}
unsafe impl Sync for WfpHandle {}

/// WFP-backed filter. `package_sid` scopes every filter to the child's
/// AppContainer so only that process tree is constrained.
pub struct WfpFilter {
    /// The child's AppContainer package SID, as a raw SID byte blob.
    package_sid: Vec<u8>,
}

impl WfpFilter {
    pub fn new(package_sid: Vec<u8>) -> Self {
        WfpFilter { package_sid }
    }
}

fn wfp_err(code: u32, what: &str) -> FilterError {
    FilterError::Apply(format!(
        "the Windows Filtering Platform call `{what}` failed (error {code:#x})"
    ))
}

/// The package-SID match condition. `sid_bytes` (the raw SID) must outlive the
/// filter-add call. For an `FWP_SID`-typed value WFP reads the `sid` union
/// member — a pointer to a real `SID` — not a byte blob; passing a blob there
/// makes WFP parse it as a SID and fail with ERROR_INVALID_SID (0x539).
fn pkg_condition(sid_bytes: &[u8]) -> FWPM_FILTER_CONDITION0 {
    let mut c = FWPM_FILTER_CONDITION0::default();
    c.fieldKey = FWPM_CONDITION_ALE_PACKAGE_ID;
    c.matchType = FWP_MATCH_EQUAL;
    c.conditionValue = FWP_CONDITION_VALUE0 {
        r#type: FWP_SID,
        Anonymous: FWP_CONDITION_VALUE0_0 {
            sid: sid_bytes.as_ptr() as *mut windows::Win32::Security::SID,
        },
    };
    c
}

impl NetFilter for WfpFilter {
    type Handle = WfpHandle;

    fn supports(&self) -> Supports {
        // WFP ALE filters cover both families.
        Supports {
            ipv4: true,
            ipv6: true,
        }
    }

    fn provision(&self, literal_ips: &[IpAddr]) -> Result<WfpHandle, FilterError> {
        // Open a dynamic engine session (filters auto-removed on close).
        let mut session = FWPM_SESSION0::default();
        session.flags = FWPM_SESSION_FLAG_DYNAMIC;
        let mut engine = windows::Win32::Foundation::HANDLE::default();
        let rc = unsafe {
            FwpmEngineOpen0(
                None,
                RPC_C_AUTHN_WINNT,
                None,
                Some(&session),
                &mut engine as *mut _ as *mut _,
            )
        };
        if rc != ERROR_SUCCESS.0 {
            return Err(wfp_err(rc, "FwpmEngineOpen0"));
        }
        let engine = EngineHandle(engine);

        // A private sublayer to hold our filters.
        let sublayer = GUID::new().map_err(|e| FilterError::Apply(e.to_string()))?;
        let mut sl = FWPM_SUBLAYER0::default();
        sl.subLayerKey = sublayer;
        let name: Vec<u16> = "agentd.sbx\0".encode_utf16().collect();
        sl.displayData.name = windows::core::PWSTR(name.as_ptr() as *mut u16);
        let rc = unsafe { FwpmSubLayerAdd0(engine.0, &sl, None) };
        if rc != ERROR_SUCCESS.0 {
            return Err(wfp_err(rc, "FwpmSubLayerAdd0"));
        }

        let h = WfpHandle { engine, sublayer };

        // Default-deny: block all outbound for this package SID (low weight).
        self.add_block_all(&h)?;
        // Seed literal-IP grants.
        for ip in literal_ips {
            self.add_permit_ip(&h, *ip)?;
        }
        Ok(h)
    }

    fn commit_allow(
        &self,
        handle: &WfpHandle,
        ips: &[IpAddr],
        _ttl: std::time::Duration,
    ) -> Result<(), FilterError> {
        for ip in ips {
            self.add_permit_ip(handle, *ip)?;
        }
        Ok(())
    }

    fn revoke(&self, _handle: &WfpHandle, _ips: &[IpAddr]) -> Result<(), FilterError> {
        // Per-IP revoke would require tracking filter ids; the dynamic session is
        // torn down wholesale at teardown, which is sufficient for the
        // pre-resolve model (the set is rebuilt per exec). No-op.
        Ok(())
    }

    fn teardown(&self, handle: WfpHandle) {
        // Closing the dynamic engine session removes every filter we added.
        unsafe {
            let _ = FwpmEngineClose0(handle.engine.0);
        }
    }
}

impl WfpFilter {
    fn add_block_all(&self, h: &WfpHandle) -> Result<(), FilterError> {
        // `self.package_sid` (owned by self, stable) backs the SID pointer for
        // the FwpmFilterAdd0 calls below.
        let pkg = pkg_condition(&self.package_sid);
        for layer in [
            FWPM_LAYER_ALE_AUTH_CONNECT_V4,
            FWPM_LAYER_ALE_AUTH_CONNECT_V6,
        ] {
            self.add_filter(h, layer, FWP_ACTION_BLOCK, 0, &[pkg])?;
        }
        Ok(())
    }

    fn add_permit_ip(&self, h: &WfpHandle, ip: IpAddr) -> Result<(), FilterError> {
        // `self.package_sid` (and `arr` for v6) must outlive the FwpmFilterAdd0 call.
        let pkg = pkg_condition(&self.package_sid);
        let mut addr = FWPM_FILTER_CONDITION0::default();
        addr.fieldKey = FWPM_CONDITION_IP_REMOTE_ADDRESS;
        addr.matchType = FWP_MATCH_EQUAL;
        match ip {
            IpAddr::V4(v4) => {
                addr.conditionValue = FWP_CONDITION_VALUE0 {
                    r#type: FWP_UINT32,
                    Anonymous: FWP_CONDITION_VALUE0_0 {
                        uint32: u32::from(v4),
                    },
                };
                self.add_filter(
                    h,
                    FWPM_LAYER_ALE_AUTH_CONNECT_V4,
                    FWP_ACTION_PERMIT,
                    10,
                    &[pkg, addr],
                )?;
            }
            IpAddr::V6(v6) => {
                let arr = FWP_BYTE_ARRAY16 {
                    byteArray16: v6.octets(),
                };
                addr.conditionValue = FWP_CONDITION_VALUE0 {
                    r#type: FWP_BYTE_ARRAY16_TYPE,
                    Anonymous: FWP_CONDITION_VALUE0_0 {
                        byteArray16: &arr as *const _ as *mut FWP_BYTE_ARRAY16,
                    },
                };
                self.add_filter(
                    h,
                    FWPM_LAYER_ALE_AUTH_CONNECT_V6,
                    FWP_ACTION_PERMIT,
                    10,
                    &[pkg, addr],
                )?;
            }
        }
        Ok(())
    }

    fn add_filter(
        &self,
        h: &WfpHandle,
        layer: GUID,
        action: FWP_ACTION_TYPE,
        weight: u8,
        conditions: &[FWPM_FILTER_CONDITION0],
    ) -> Result<(), FilterError> {
        let mut filter = FWPM_FILTER0::default();
        // WFP requires every filter to carry a display name; a null name fails
        // `FwpmFilterAdd0` with FWP_E_NULL_DISPLAY_NAME (0x80320023). `name` must
        // outlive the add call below.
        let mut name: Vec<u16> = "agentd.sbx.filter\0".encode_utf16().collect();
        filter.displayData.name = windows::core::PWSTR(name.as_mut_ptr());
        filter.layerKey = layer;
        filter.subLayerKey = h.sublayer;
        filter.action.r#type = action;
        // Weight must be FWP_UINT8 (a 0..=15 priority index) or FWP_UINT64;
        // FWP_UINT32 is rejected with FWP_E_INVALID_WEIGHT (0x80320025).
        filter.weight = FWP_VALUE0 {
            r#type: FWP_UINT8,
            Anonymous: FWP_VALUE0_0 { uint8: weight },
        };
        filter.numFilterConditions = conditions.len() as u32;
        filter.filterCondition = conditions.as_ptr() as *mut FWPM_FILTER_CONDITION0;
        let mut id = 0u64;
        let rc = unsafe { FwpmFilterAdd0(h.engine.0, &filter, None, Some(&mut id)) };
        if rc != ERROR_SUCCESS.0 {
            return Err(wfp_err(rc, "FwpmFilterAdd0"));
        }
        Ok(())
    }
}

/// DNS servers the child is allowed to reach (so name resolution works),
/// seeded into the permit set. The child does its own `getaddrinfo`, which goes
/// to the machine's *configured* resolvers — so we must permit exactly those, or
/// the query is blocked (WFP denies it) and every name fails to resolve.
///
/// We enumerate the live adapters' resolvers via `GetAdaptersAddresses` and add
/// the loopback stub (the Windows DNS Client service often answers there). The
/// common public resolvers are appended only as a last resort, when enumeration
/// returned nothing usable.
pub fn system_dns_servers() -> Vec<IpAddr> {
    let mut out = configured_dns_servers();
    // The DNS Client service / stub resolver commonly answers on loopback.
    out.push(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
    // Fallback only if the machine reported no real (non-loopback) resolvers.
    if out.iter().all(|ip| ip.is_loopback()) {
        out.push(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)));
        out.push(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)));
    }
    out.sort();
    out.dedup();
    out
}

/// Enumerate the DNS resolvers configured on every adapter via
/// `GetAdaptersAddresses`. Empty vec on any failure (the caller adds fallbacks).
/// This is what makes resolution actually work inside the AppContainer on a
/// normal network, where the resolver is the router/ISP, not a hardcoded IP.
fn configured_dns_servers() -> Vec<IpAddr> {
    use std::net::Ipv6Addr;
    use windows::Win32::NetworkManagement::IpHelper::{
        GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_FRIENDLY_NAME, GAA_FLAG_SKIP_MULTICAST,
        GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows::Win32::Networking::WinSock::{
        AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR_IN, SOCKADDR_IN6,
    };

    const ERROR_BUFFER_OVERFLOW: u32 = 111;
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_FRIENDLY_NAME;

    // Size the buffer, then fetch (retry while the required size keeps growing).
    let mut size: u32 = 16 * 1024;
    let mut buf: Vec<u8> = Vec::new();
    let mut rc = ERROR_BUFFER_OVERFLOW;
    for _ in 0..3 {
        buf.resize(size as usize, 0);
        rc = unsafe {
            GetAdaptersAddresses(
                AF_UNSPEC.0 as u32,
                flags,
                None,
                Some(buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH),
                &mut size,
            )
        };
        if rc != ERROR_BUFFER_OVERFLOW {
            break;
        }
    }
    if rc != 0 {
        return Vec::new();
    }

    let mut ips = Vec::new();
    unsafe {
        let mut adapter = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
        while !adapter.is_null() {
            let mut dns = (*adapter).FirstDnsServerAddress;
            while !dns.is_null() {
                let sa = (*dns).Address;
                if !sa.lpSockaddr.is_null() {
                    let family = (*sa.lpSockaddr).sa_family;
                    if family == AF_INET {
                        let v4 = &*(sa.lpSockaddr as *const SOCKADDR_IN);
                        let octets = v4.sin_addr.S_un.S_addr.to_ne_bytes();
                        ips.push(IpAddr::V4(Ipv4Addr::from(octets)));
                    } else if family == AF_INET6 {
                        let v6 = &*(sa.lpSockaddr as *const SOCKADDR_IN6);
                        let a = Ipv6Addr::from(v6.sin6_addr.u.Byte);
                        // Skip link-local (fe80::/10) resolvers: not usable as a
                        // bare permit target without a scope id.
                        if (a.segments()[0] & 0xffc0) != 0xfe80 {
                            ips.push(IpAddr::V6(a));
                        }
                    }
                }
                dns = (*dns).Next;
            }
            adapter = (*adapter).Next;
        }
    }
    ips.retain(|ip| !ip.is_unspecified());
    ips
}
