//! Windows sandbox backend: restricted-token filesystem confinement + WFP
//! host-granular network containment.
//!
//! Unlike the unix backends there is no `pre_exec`; the child is launched with a
//! custom `CreateProcessAsUserW` using a write-restricted token, its stdio wired
//! to inherited pipes, and (when network is allowed) WFP filters scoped to the
//! child's binary that block all outbound except the egress proxy's loopback
//! port.

use crate::policy::{SandboxError, SandboxPolicy};

/// Filesystem confinement (write-restricted token) is available on Windows.
pub fn is_supported() -> bool {
    true
}

/// Host-granular network needs a WFP engine session (admin / service).
pub fn net_supported() -> bool {
    imp::wfp_available()
}

/// Unused on Windows (the custom spawn path applies confinement); kept for
/// dispatch signature parity.
pub fn apply(_policy: &SandboxPolicy) -> Result<(), SandboxError> {
    Err(SandboxError::Apply(
        "windows applies confinement at spawn; see run_contained".into(),
    ))
}

/// Run `req` under a write-restricted token, confined to the proxy for network
/// (when `allow_net`). `proxy_addr` is the egress proxy loopback address.
pub async fn run_contained(
    req: &crate::ExecRequest,
    policy: &SandboxPolicy,
    proxy_addr: Option<std::net::SocketAddr>,
) -> Result<crate::ExecResult, crate::ShellError> {
    let req = req.clone();
    let policy = policy.clone();
    tokio::task::spawn_blocking(move || imp::run_blocking(&req, &policy, proxy_addr))
        .await
        .map_err(|e| crate::ShellError::Sandbox(format!("join: {e}")))?
}

#[cfg(target_os = "windows")]
mod imp {
    use std::os::windows::ffi::OsStrExt;

    use windows::Win32::Foundation::SetHandleInformation;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS};
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{
        EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW, NO_MULTIPLE_TRUSTEE,
        SE_FILE_OBJECT, SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID,
        TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
    };
    use windows::Win32::Security::{
        ACL, AllocateAndInitializeSid, CreateRestrictedToken, DACL_SECURITY_INFORMATION,
        DISABLE_MAX_PRIVILEGE, FreeSid, LUA_TOKEN, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES,
        SECURITY_NT_AUTHORITY, SID_AND_ATTRIBUTES, SUB_CONTAINERS_AND_OBJECTS_INHERIT,
        TOKEN_ALL_ACCESS, TOKEN_DUPLICATE, TOKEN_QUERY, WRITE_RESTRICTED,
    };
    use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile};
    use windows::Win32::System::Pipes::CreatePipe;
    use windows::Win32::System::Threading::{
        CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess,
        INFINITE, OpenProcessToken, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW,
        WaitForSingleObject,
    };
    use windows::core::{PCWSTR, PWSTR};

    use crate::policy::SandboxPolicy;
    use crate::{ExecRequest, ExecResult, ShellError};

    use super::net;

    fn sb(e: impl std::fmt::Display) -> ShellError {
        ShellError::Sandbox(e.to_string())
    }

    /// Whether a WFP engine session can be opened.
    pub fn wfp_available() -> bool {
        net::wfp_available()
    }

    fn to_wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    /// One end of a pipe; child end is inheritable, parent end is not.
    struct Pipe {
        read: HANDLE,
        write: HANDLE,
    }

    fn make_pipe(inherit_read: bool) -> Result<Pipe, ShellError> {
        let mut sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            bInheritHandle: true.into(),
            lpSecurityDescriptor: std::ptr::null_mut(),
        };
        let mut read = HANDLE::default();
        let mut write = HANDLE::default();
        unsafe {
            CreatePipe(&mut read, &mut write, Some(&sa), 0).map_err(sb)?;
            // Only the end the child inherits stays inheritable; the parent end
            // must NOT be inherited (it would leak into the child).
            let parent_end = if inherit_read { write } else { read };
            SetHandleInformation(parent_end, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)).map_err(sb)?;
        }
        let _ = &mut sa;
        Ok(Pipe { read, write })
    }

    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;

    /// Create a per-invocation synthetic SID (`S-1-5-21-r1-r2-r3`) used to gate
    /// write access: it is added to the token's restricting-SID list, and write
    /// ACEs for it are stamped on the granted write paths.
    fn make_sandbox_sid() -> Result<PSID, ShellError> {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let pid = std::process::id();
        let r2 = (seed as u32) ^ pid;
        let r3 = (seed >> 32) as u32 ^ 0x9e37_79b9;
        let r4 = (seed >> 64) as u32 ^ 0x85eb_ca6b;
        let auth = SECURITY_NT_AUTHORITY;
        let mut psid = PSID::default();
        unsafe {
            AllocateAndInitializeSid(&auth, 4, 21, r2, r3, r4, 0, 0, 0, 0, &mut psid)
                .map_err(sb)?;
        }
        Ok(psid)
    }

    /// Best-effort: stamp a write+read ACE for `sid` on `path` (subtree). A
    /// failure leaves the path non-writable by the child (safe / over-restrictive).
    fn stamp_write_ace(path: &str, sid: PSID) {
        let wide = to_wide(path);
        unsafe {
            let mut old_dacl: *mut ACL = std::ptr::null_mut();
            let mut psd = PSECURITY_DESCRIPTOR::default();
            if GetNamedSecurityInfoW(
                PCWSTR(wide.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(&mut old_dacl),
                None,
                &mut psd,
            )
            .is_err()
            {
                return;
            }

            let ea = EXPLICIT_ACCESS_W {
                grfAccessPermissions: GENERIC_READ | GENERIC_WRITE,
                grfAccessMode: GRANT_ACCESS,
                grfInheritance: SUB_CONTAINERS_AND_OBJECTS_INHERIT,
                Trustee: TRUSTEE_W {
                    pMultipleTrustee: std::ptr::null_mut(),
                    MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                    TrusteeForm: TRUSTEE_IS_SID,
                    TrusteeType: TRUSTEE_IS_UNKNOWN,
                    ptstrName: PWSTR(sid.0 as *mut u16),
                },
            };
            let mut new_dacl: *mut ACL = std::ptr::null_mut();
            if SetEntriesInAclW(Some(&[ea]), Some(old_dacl), &mut new_dacl).is_ok()
                && !new_dacl.is_null()
            {
                let _ = SetNamedSecurityInfoW(
                    PCWSTR(wide.as_ptr()),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    None,
                    None,
                    Some(new_dacl),
                    None,
                );
                let _ = LocalFree(HLOCAL(new_dacl as *mut core::ffi::c_void));
            }
            if !psd.0.is_null() {
                let _ = LocalFree(HLOCAL(psd.0));
            }
        }
    }

    /// Build a write-restricted token whose only restricting SID is `sandbox_sid`,
    /// so (because of `WRITE_RESTRICTED`) writes succeed only where that SID has
    /// an ACE — i.e. the stamped grant paths. Reads/exec are unaffected.
    fn restricted_token(sandbox_sid: PSID) -> Result<HANDLE, ShellError> {
        unsafe {
            let mut base = HANDLE::default();
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ALL_ACCESS,
                &mut base,
            )
            .map_err(sb)?;

            let restrict = [SID_AND_ATTRIBUTES {
                Sid: sandbox_sid,
                Attributes: 0,
            }];
            let mut restricted = HANDLE::default();
            CreateRestrictedToken(
                base,
                DISABLE_MAX_PRIVILEGE | LUA_TOKEN | WRITE_RESTRICTED,
                None,
                None,
                Some(&restrict),
                &mut restricted,
            )
            .map_err(sb)?;
            let _ = CloseHandle(base);
            Ok(restricted)
        }
    }

    fn drain(read: HANDLE) -> String {
        let mut out: Vec<u8> = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let mut got = 0u32;
            let ok = unsafe { ReadFile(read, Some(&mut buf), Some(&mut got), None) };
            if ok.is_err() || got == 0 {
                break;
            }
            out.extend_from_slice(&buf[..got as usize]);
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    pub fn run_blocking(
        req: &ExecRequest,
        policy: &SandboxPolicy,
        proxy_addr: Option<std::net::SocketAddr>,
    ) -> Result<ExecResult, ShellError> {
        // Synthetic write SID + write-restricted token; stamp write ACEs on the
        // granted paths so the child can write only there.
        let sandbox_sid = make_sandbox_sid()?;
        for p in &policy.write_paths {
            if let Some(s) = p.to_str() {
                stamp_write_ace(s, sandbox_sid);
            }
        }
        let token = restricted_token(sandbox_sid)?;

        // stdio pipes.
        let out = make_pipe(true)?; // child writes `write`, parent reads `read`
        let err = make_pipe(true)?;
        let inp = make_pipe(false)?; // child reads `read`, parent writes `write`

        // Command line.
        let mut cmdline = String::new();
        cmdline.push('"');
        cmdline.push_str(&req.bin);
        cmdline.push('"');
        for a in &req.args {
            cmdline.push(' ');
            cmdline.push('"');
            cmdline.push_str(&a.replace('"', "\\\""));
            cmdline.push('"');
        }
        let mut cmdline_w = to_wide(&cmdline);

        // Proxy env block (UTF-16, double-NUL terminated) when a proxy is active.
        let env_block: Option<Vec<u16>> = proxy_addr.map(|addr| {
            let proxy = format!("http://127.0.0.1:{}", addr.port());
            let mut vars: Vec<(String, String)> = std::env::vars().collect();
            for k in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY"] {
                vars.retain(|(name, _)| !name.eq_ignore_ascii_case(k));
            }
            for k in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"] {
                vars.push((k.to_string(), proxy.clone()));
            }
            vars.push(("NO_PROXY".into(), "localhost,127.0.0.1,::1".into()));
            let mut block = Vec::new();
            for (k, v) in vars {
                block.extend(to_wide(&format!("{k}={v}")));
            }
            block.push(0);
            block
        });

        let si = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            dwFlags: STARTF_USESTDHANDLES,
            hStdInput: inp.read,
            hStdOutput: out.write,
            hStdError: err.write,
            ..Default::default()
        };
        let mut pi = PROCESS_INFORMATION::default();

        // WFP egress lock (only when network is permitted).
        let wfp = match proxy_addr {
            Some(addr) => Some(net::install_egress_lock(&req.bin, addr)?),
            None => None,
        };

        let env_ptr = env_block
            .as_ref()
            .map(|b| b.as_ptr() as *const std::ffi::c_void);

        let spawn = unsafe {
            CreateProcessAsUserW(
                token,
                PCWSTR::null(),
                PWSTR(cmdline_w.as_mut_ptr()),
                None,
                None,
                true, // inherit handles (the stdio pipe ends)
                CREATE_UNICODE_ENVIRONMENT,
                env_ptr,
                PCWSTR::null(),
                &si,
                &mut pi,
            )
        };

        // Close the child-side handles in the parent so reads see EOF on exit.
        unsafe {
            let _ = CloseHandle(out.write);
            let _ = CloseHandle(err.write);
            let _ = CloseHandle(inp.read);
        }

        let result = (|| -> Result<ExecResult, ShellError> {
            spawn.map_err(sb)?;

            // Feed stdin, if any, then close.
            if let Some(input) = &req.stdin {
                let bytes = input.as_bytes();
                let mut written = 0u32;
                unsafe {
                    let _ = WriteFile(inp.write, Some(bytes), Some(&mut written), None);
                }
            }
            unsafe {
                let _ = CloseHandle(inp.write);
            }

            let stdout = drain(out.read);
            let stderr_text = drain(err.read);

            let exit_code = unsafe {
                WaitForSingleObject(pi.hProcess, INFINITE);
                let mut code = 0u32;
                let _ = GetExitCodeProcess(pi.hProcess, &mut code);
                let _ = CloseHandle(pi.hProcess);
                let _ = CloseHandle(pi.hThread);
                code as i32
            };

            let (stdout, stderr) = if req.separate_stderr {
                (stdout, stderr_text)
            } else {
                let mut merged = stdout;
                if !stderr_text.is_empty() {
                    if !merged.is_empty() && !merged.ends_with('\n') {
                        merged.push('\n');
                    }
                    merged.push_str(&stderr_text);
                }
                (merged, String::new())
            };

            Ok(ExecResult {
                exit_code,
                stdout,
                stderr,
            })
        })();

        // Teardown.
        if let Some(w) = wfp {
            net::remove_egress_lock(w);
        }
        unsafe {
            let _ = CloseHandle(out.read);
            let _ = CloseHandle(err.read);
            let _ = CloseHandle(inp.write);
            let _ = CloseHandle(token);
            let _ = FreeSid(sandbox_sid);
        }

        result
    }
}

/// WFP egress lock: block all outbound for the child's binary except the proxy
/// loopback port. Windows-only.
#[cfg(target_os = "windows")]
mod net {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::NetworkManagement::WindowsFilteringPlatform::*;
    use windows::Win32::System::Rpc::UuidCreate;
    use windows::core::{GUID, PCWSTR};

    use crate::ShellError;

    pub fn wfp_available() -> bool {
        unsafe {
            let mut engine = HANDLE::default();
            let session = FWPM_SESSION0::default();
            if FwpmEngineOpen0(None, 0, None, Some(&session), &mut engine) == 0 {
                let _ = FwpmEngineClose0(engine);
                true
            } else {
                false
            }
        }
    }

    pub struct EgressLock {
        engine: HANDLE,
        sublayer: GUID,
        filters: Vec<GUID>,
        // app-id blob, freed on teardown.
        app_id: *mut FWP_BYTE_BLOB,
    }

    fn new_guid() -> GUID {
        let mut g = GUID::zeroed();
        unsafe {
            let _ = UuidCreate(&mut g);
        }
        g
    }

    fn to_wide(s: &str) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    fn uint8(v: u8) -> FWP_VALUE0 {
        FWP_VALUE0 {
            r#type: FWP_UINT8,
            Anonymous: FWP_VALUE0_0 { uint8: v },
        }
    }

    /// Add one filter; record its key for teardown.
    unsafe fn add_filter(
        engine: HANDLE,
        sublayer: GUID,
        layer: GUID,
        conditions: &[FWPM_FILTER_CONDITION0],
        action: FWP_ACTION_TYPE,
        weight: u8,
        keys: &mut Vec<GUID>,
    ) -> Result<(), ShellError> {
        let key = new_guid();
        let mut filter = FWPM_FILTER0 {
            filterKey: key,
            layerKey: layer,
            subLayerKey: sublayer,
            weight: uint8(weight),
            numFilterConditions: conditions.len() as u32,
            filterCondition: conditions.as_ptr() as *mut FWPM_FILTER_CONDITION0,
            action: FWPM_ACTION0 {
                r#type: action,
                ..Default::default()
            },
            ..Default::default()
        };
        let r = unsafe { FwpmFilterAdd0(engine, &filter, None, None) };
        let _ = &mut filter;
        if r != 0 {
            return Err(ShellError::Sandbox(format!("FwpmFilterAdd0: {r}")));
        }
        keys.push(key);
        Ok(())
    }

    /// Install filters scoped to `bin` (by app-id): block all outbound (V4+V6),
    /// permit only the proxy loopback addr:port (V4).
    pub fn install_egress_lock(
        bin: &str,
        proxy_addr: std::net::SocketAddr,
    ) -> Result<EgressLock, ShellError> {
        unsafe {
            let mut engine = HANDLE::default();
            let session = FWPM_SESSION0::default();
            if FwpmEngineOpen0(None, 0, None, Some(&session), &mut engine) != 0 {
                return Err(ShellError::Sandbox("FwpmEngineOpen0 failed".into()));
            }

            // Resolve the binary to a WFP app-id blob.
            let bin_w = to_wide(bin);
            let mut app_id: *mut FWP_BYTE_BLOB = std::ptr::null_mut();
            if FwpmGetAppIdFromFileName0(PCWSTR(bin_w.as_ptr()), &mut app_id) != 0 {
                let _ = FwpmEngineClose0(engine);
                return Err(ShellError::Sandbox(
                    "FwpmGetAppIdFromFileName0 failed".into(),
                ));
            }

            let sublayer_key = new_guid();
            let mut sublayer = FWPM_SUBLAYER0 {
                subLayerKey: sublayer_key,
                weight: 0xffff,
                ..Default::default()
            };

            let mut keys: Vec<GUID> = Vec::new();
            let result = (|| -> Result<(), ShellError> {
                if FwpmTransactionBegin0(engine, 0) != 0 {
                    return Err(ShellError::Sandbox("FwpmTransactionBegin0".into()));
                }
                if FwpmSubLayerAdd0(engine, &sublayer, None) != 0 {
                    return Err(ShellError::Sandbox("FwpmSubLayerAdd0".into()));
                }

                let app_cond = FWPM_FILTER_CONDITION0 {
                    fieldKey: FWPM_CONDITION_ALE_APP_ID,
                    matchType: FWP_MATCH_EQUAL,
                    conditionValue: FWP_CONDITION_VALUE0 {
                        r#type: FWP_BYTE_BLOB_TYPE,
                        Anonymous: FWP_CONDITION_VALUE0_0 { byteBlob: app_id },
                    },
                };

                // Block all outbound for this app on V4 and V6.
                add_filter(
                    engine,
                    sublayer_key,
                    FWPM_LAYER_ALE_AUTH_CONNECT_V4,
                    &[app_cond],
                    FWP_ACTION_BLOCK,
                    1,
                    &mut keys,
                )?;
                add_filter(
                    engine,
                    sublayer_key,
                    FWPM_LAYER_ALE_AUTH_CONNECT_V6,
                    &[app_cond],
                    FWP_ACTION_BLOCK,
                    1,
                    &mut keys,
                )?;

                // Permit only the proxy loopback addr:port on V4 (higher weight).
                let v4 = match proxy_addr {
                    std::net::SocketAddr::V4(a) => u32::from(*a.ip()),
                    std::net::SocketAddr::V6(_) => 0x7f000001, // proxy is IPv4 loopback
                };
                let addr_cond = FWPM_FILTER_CONDITION0 {
                    fieldKey: FWPM_CONDITION_IP_REMOTE_ADDRESS,
                    matchType: FWP_MATCH_EQUAL,
                    conditionValue: FWP_CONDITION_VALUE0 {
                        r#type: FWP_UINT32,
                        Anonymous: FWP_CONDITION_VALUE0_0 { uint32: v4 },
                    },
                };
                let port_cond = FWPM_FILTER_CONDITION0 {
                    fieldKey: FWPM_CONDITION_IP_REMOTE_PORT,
                    matchType: FWP_MATCH_EQUAL,
                    conditionValue: FWP_CONDITION_VALUE0 {
                        r#type: FWP_UINT16,
                        Anonymous: FWP_CONDITION_VALUE0_0 {
                            uint16: proxy_addr.port(),
                        },
                    },
                };
                add_filter(
                    engine,
                    sublayer_key,
                    FWPM_LAYER_ALE_AUTH_CONNECT_V4,
                    &[app_cond, addr_cond, port_cond],
                    FWP_ACTION_PERMIT,
                    15,
                    &mut keys,
                )?;

                if FwpmTransactionCommit0(engine) != 0 {
                    return Err(ShellError::Sandbox("FwpmTransactionCommit0".into()));
                }
                Ok(())
            })();

            let _ = &mut sublayer;

            if let Err(e) = result {
                let _ = FwpmTransactionAbort0(engine);
                if !app_id.is_null() {
                    FwpmFreeMemory0(&mut (app_id as *mut core::ffi::c_void));
                }
                let _ = FwpmEngineClose0(engine);
                return Err(e);
            }

            Ok(EgressLock {
                engine,
                sublayer: sublayer_key,
                filters: keys,
                app_id,
            })
        }
    }

    pub fn remove_egress_lock(mut lock: EgressLock) {
        unsafe {
            let _ = FwpmTransactionBegin0(lock.engine, 0);
            for k in &lock.filters {
                let _ = FwpmFilterDeleteByKey0(lock.engine, k);
            }
            let _ = FwpmSubLayerDeleteByKey0(lock.engine, &lock.sublayer);
            let _ = FwpmTransactionCommit0(lock.engine);
            if !lock.app_id.is_null() {
                FwpmFreeMemory0(&mut (lock.app_id as *mut core::ffi::c_void));
            }
            let _ = FwpmEngineClose0(lock.engine);
        }
        let _ = &mut lock;
    }
}

// Non-Windows compile shim so the module type-checks on other targets.
#[cfg(not(target_os = "windows"))]
mod imp {
    use crate::policy::SandboxPolicy;
    use crate::{ExecRequest, ExecResult, ShellError};

    pub fn wfp_available() -> bool {
        false
    }

    pub fn run_blocking(
        _req: &ExecRequest,
        _policy: &SandboxPolicy,
        _proxy_addr: Option<std::net::SocketAddr>,
    ) -> Result<ExecResult, ShellError> {
        Err(ShellError::SandboxUnavailable)
    }
}
