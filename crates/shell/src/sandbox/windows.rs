//! Windows sandbox backend: **AppContainer** filesystem confinement + **WFP**
//! host/IP-granular network.
//!
//! The child is launched into an AppContainer (a low-privilege "lowbox" token)
//! via `CreateProcessW` + a `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`
//! attribute:
//!
//! - **Filesystem**: a lowbox token can only touch objects whose ACL grants the
//!   AppContainer's package SID (or `ALL_APPLICATION_PACKAGES`). System binaries
//!   and their DLLs already grant `ALL_APPLICATION_PACKAGES`, so programs start;
//!   user data does not, so the child is confined. We stamp the package SID onto
//!   exactly the granted read/write paths.
//! - **Network**: with `allow_net = false` the child holds no network capability
//!   and the OS blocks all outbound by construction. When network is permitted we
//!   grant the `internetClient` capability so the child can attempt DNS/TCP, and
//!   a WFP filter set scoped to its package SID enforces the host/IP allowlist
//!   (see [`super::windows_wfp`]).

use crate::policy::{SandboxError, SandboxPolicy};

/// AppContainer confinement is available on all supported Windows versions.
pub fn is_supported() -> bool {
    true
}

/// AppContainer network containment needs no privileged engine session — the
/// capability-based block is enforced by the OS for any caller.
pub fn net_supported() -> bool {
    true
}

/// Unused on Windows (the custom spawn path applies confinement); kept for
/// dispatch signature parity.
pub fn apply(_policy: &SandboxPolicy) -> Result<(), SandboxError> {
    Err(SandboxError::Apply(
        "windows applies confinement at spawn; see run_contained".into(),
    ))
}

/// Run `req` inside an AppContainer. When the policy permits network, a WFP
/// filter set scoped to the child's AppContainer package SID enforces the
/// host/IP allowlist (transparent, driver-free); otherwise the child gets no
/// network capability at all.
pub async fn run_contained(
    req: &crate::ExecRequest,
    policy: &SandboxPolicy,
) -> Result<crate::ExecResult, crate::ShellError> {
    let req = req.clone();
    let policy = policy.clone();
    tokio::task::spawn_blocking(move || imp::run_blocking(&req, &policy))
        .await
        .map_err(|e| crate::ShellError::Sandbox(format!("join: {e}")))?
}

/// One-time privileged sandbox setup; see [`imp::install`].
pub fn install() -> Result<(), crate::ShellError> {
    imp::install()
}

#[cfg(target_os = "windows")]
mod imp {
    use std::os::windows::ffi::OsStrExt;

    use windows::Win32::Foundation::{
        CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS, HLOCAL, LocalFree,
        SetHandleInformation,
    };
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, ConvertStringSidToSidW,
        EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW, NO_MULTIPLE_TRUSTEE,
        SDDL_REVISION_1, SE_FILE_OBJECT, SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID,
        TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
    };
    use windows::Win32::Security::GetLengthSid;

    use windows::Win32::Security::Isolation::{
        CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
    };
    use windows::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, FreeSid, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES,
        SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES, SUB_CONTAINERS_AND_OBJECTS_INHERIT,
    };
    use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile};
    use windows::Win32::System::Pipes::CreatePipe;
    use windows::Win32::System::Threading::{
        CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
        EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE,
        InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
        PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, PROCESS_INFORMATION, STARTF_USESTDHANDLES,
        STARTUPINFOEXW, UpdateProcThreadAttribute, WaitForSingleObject,
    };
    use windows::core::{PCWSTR, PWSTR};

    use crate::policy::SandboxPolicy;
    use crate::{ExecRequest, ExecResult, ShellError};

    /// Two AppContainer profiles. The loopback egress proxy is reachable only by
    /// a package SID on the machine's loopback-exemption list, which is per-SID
    /// and persistent — so `PROFILE_NET` is exempted (see `install`) and carries
    /// `allow_net` children, while `PROFILE_CONFINED` is never exempted and
    /// carries denied children, which therefore reach no loopback service at all.
    const PROFILE_CONFINED: &str = "agentd.shell.confined";
    const PROFILE_NET: &str = "agentd.shell.net";

    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const GENERIC_EXECUTE: u32 = 0x2000_0000;

    fn sb(e: impl std::fmt::Display) -> ShellError {
        ShellError::Sandbox(e.to_string())
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

    /// Security descriptor for the stdio pipes: grant `GENERIC_ALL` to Everyone
    /// (so the parent keeps full use of its ends) and to `ALL_APPLICATION_PACKAGES`
    /// (`AC`) so the AppContainer child can read/write the ends it inherits. A
    /// lowbox process is otherwise denied access to default anonymous pipes,
    /// which silently breaks stdin/stdout.
    fn pipe_security_descriptor() -> Result<PSECURITY_DESCRIPTOR, ShellError> {
        let sddl = to_wide("D:(A;;GA;;;WD)(A;;GA;;;AC)");
        let mut psd = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut psd,
                None,
            )
            .map_err(sb)?;
        }
        Ok(psd)
    }

    fn make_pipe(child_inherits_read: bool, sd: PSECURITY_DESCRIPTOR) -> Result<Pipe, ShellError> {
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            bInheritHandle: true.into(),
            lpSecurityDescriptor: sd.0,
        };
        let mut read = HANDLE::default();
        let mut write = HANDLE::default();
        unsafe {
            CreatePipe(&mut read, &mut write, Some(&sa), 0).map_err(sb)?;
            // Only the end the child inherits stays inheritable; the parent end
            // must NOT be inherited (it would leak into the child).
            let parent_end = if child_inherits_read { write } else { read };
            SetHandleInformation(parent_end, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)).map_err(sb)?;
        }
        Ok(Pipe { read, write })
    }

    /// Ensure the named AppContainer profile exists, serialized and deduped per
    /// process. `CreateAppContainerProfile` raced concurrently for one name lets a
    /// loser observe the profile as not-yet-registered, after which
    /// `DeriveAppContainerSidFromAppContainerName` fails with FILE_NOT_FOUND.
    /// Creating a profile needs NO elevation. Idempotent: ALREADY_EXISTS is fine.
    fn ensure_profile(name: &str) {
        use std::sync::Mutex;
        static DONE: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
        let key: &'static str = if name == PROFILE_NET {
            PROFILE_NET
        } else {
            PROFILE_CONFINED
        };
        let mut done = DONE.lock().unwrap_or_else(|e| e.into_inner());
        if done.contains(&key) {
            return;
        }
        let wname = to_wide(name);
        let display = to_wide("agent.d shell sandbox");
        let desc = to_wide("agent.d confined shell execution");
        unsafe {
            if let Ok(sid) = CreateAppContainerProfile(
                PCWSTR(wname.as_ptr()),
                PCWSTR(display.as_ptr()),
                PCWSTR(desc.as_ptr()),
                None,
            ) {
                FreeSid(sid);
            }
        }
        done.push(key);
    }

    /// Return the AppContainer package SID for the confined (`net = false`) or
    /// network (`net = true`) profile, ensuring that profile exists first. The
    /// SID is deterministic from the profile name and must be released with
    /// `FreeSid`.
    fn appcontainer_sid(net: bool) -> Result<PSID, ShellError> {
        let name = if net { PROFILE_NET } else { PROFILE_CONFINED };
        ensure_profile(name);
        let wname = to_wide(name);
        unsafe {
            DeriveAppContainerSidFromAppContainerName(PCWSTR(wname.as_ptr()))
                .map_err(|e| sb(format!("derive AppContainer SID: {e}")))
        }
    }

    /// One-time setup: ensure the AppContainer profiles exist (no elevation
    /// needed to create them).
    ///
    /// NOTE: network enforcement uses WFP filters added at runtime
    /// ([`super::windows_wfp`]). Adding WFP filters requires the daemon to hold
    /// WFP access — typically Administrator, or an admin-granted WFP access ACL
    /// on the daemon's identity. The exact requirement must be verified on the
    /// target machine; a net child whose `provision` fails surfaces the WFP error.
    pub fn install() -> Result<(), ShellError> {
        ensure_profile(PROFILE_CONFINED);
        ensure_profile(PROFILE_NET);
        Ok(())
    }

    /// Best-effort: stamp an access-allowed ACE granting `mask` to `sid` on
    /// `path` (subtree, inheritable). A failure leaves the path inaccessible to
    /// the AppContainer (safe / over-restrictive).
    fn stamp_ace(path: &str, sid: PSID, mask: u32) {
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
                grfAccessPermissions: mask,
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

    /// Resolve `bin` to the directory that holds it, so we can grant the
    /// AppContainer read+execute there. Absolute paths are used directly;
    /// otherwise `bin` (with and without a `.exe` suffix) is looked up on `PATH`,
    /// matching how `CreateProcessW` resolves a bare name.
    fn resolve_bin_dir(bin: &str) -> Option<std::path::PathBuf> {
        let p = std::path::Path::new(bin);
        let file = if p.is_absolute() {
            p.to_path_buf()
        } else {
            std::env::split_paths(&std::env::var_os("PATH")?).find_map(|d| {
                [d.join(bin), d.join(format!("{bin}.exe"))]
                    .into_iter()
                    .find(|c| c.is_file())
            })?
        };
        file.parent().map(|d| d.to_path_buf())
    }

    /// Whether `path`'s DACL already carries an allow-ACE for any SID in `sids`.
    /// Used to skip the (inheritable, tree-propagating) binary-directory grant
    /// when it is unnecessary: the directory already grants our process-stable
    /// package SID (a prior invocation stamped it), or it grants
    /// `ALL_APPLICATION_PACKAGES` — which every System32 directory does, so we
    /// never try to re-ACL system locations we cannot (and must not) modify.
    fn dacl_contains_any(path: &str, sids: &[PSID]) -> bool {
        use windows::Win32::Security::{ACCESS_ALLOWED_ACE, ACE_HEADER, EqualSid, GetAce};
        const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
        let wide = to_wide(path);
        unsafe {
            let mut dacl: *mut ACL = std::ptr::null_mut();
            let mut psd = PSECURITY_DESCRIPTOR::default();
            if GetNamedSecurityInfoW(
                PCWSTR(wide.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(&mut dacl),
                None,
                &mut psd,
            )
            .is_err()
            {
                return false;
            }
            let mut found = false;
            if !dacl.is_null() {
                'outer: for i in 0..(*dacl).AceCount {
                    let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
                    if GetAce(dacl, i as u32, &mut ace).is_ok() && !ace.is_null() {
                        let header = &*(ace as *const ACE_HEADER);
                        if header.AceType == ACCESS_ALLOWED_ACE_TYPE {
                            let aa = &*(ace as *const ACCESS_ALLOWED_ACE);
                            let ace_sid =
                                PSID(&aa.SidStart as *const u32 as *mut core::ffi::c_void);
                            for want in sids {
                                if EqualSid(ace_sid, *want).is_ok() {
                                    found = true;
                                    break 'outer;
                                }
                            }
                        }
                    }
                }
            }
            if !psd.0.is_null() {
                let _ = LocalFree(HLOCAL(psd.0));
            }
            found
        }
    }

    /// `ALL_APPLICATION_PACKAGES` SID (`S-1-15-2-1`). Freed with `LocalFree`.
    fn all_app_packages_sid() -> Option<PSID> {
        use windows::Win32::Security::Authorization::ConvertStringSidToSidW;
        let s = to_wide("S-1-15-2-1");
        let mut sid = PSID::default();
        unsafe {
            ConvertStringSidToSidW(PCWSTR(s.as_ptr()), &mut sid).ok()?;
        }
        Some(sid)
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

    /// Build the `internetClient` capability (S-1-15-3-1) so the AppContainer
    /// child can attempt outbound network; WFP then restricts it to allowed IPs.
    /// The returned SID must be freed (LocalFree) after the spawn.
    fn internet_client_capability() -> (Vec<SID_AND_ATTRIBUTES>, PSID) {
        let s = to_wide("S-1-15-3-1");
        let mut psid = PSID::default();
        unsafe {
            let _ = ConvertStringSidToSidW(PCWSTR(s.as_ptr()), &mut psid);
        }
        let caps = vec![SID_AND_ATTRIBUTES {
            Sid: psid,
            Attributes: 0x0000_0004, // SE_GROUP_ENABLED
        }];
        (caps, psid)
    }

    /// Copy a SID's raw bytes (for the WFP package-id condition).
    fn sid_to_bytes(sid: PSID) -> Vec<u8> {
        unsafe {
            let len = GetLengthSid(sid) as usize;
            std::slice::from_raw_parts(sid.0 as *const u8, len).to_vec()
        }
    }

    /// Resolve the policy's net grants to an IP allowlist (host grants resolved
    /// here, non-privileged, + literal-IP grants + the machine's DNS servers) and
    /// ask the elevated broker to install the matching WFP filter set for `sid`.
    /// The returned guard holds the broker connection open, keeping the filters
    /// live for the child's lifetime; dropping it tears them down.
    fn provision_net(
        sid: PSID,
        policy: &SandboxPolicy,
    ) -> Result<crate::netbroker::Provision, ShellError> {
        use crate::dns_pin::{Resolve, SystemResolver, split_grants};
        let (host_grants, mut ips) = split_grants(&policy.net_hosts);
        let resolver = SystemResolver;
        for g in &host_grants {
            if let ("net", Some(name)) = g.parts()
                && !name.contains('*')
                && let Ok(addrs) = resolver.resolve(name)
            {
                // Only concrete names pre-resolve; wildcards are skipped (no L7).
                ips.extend(addrs);
            }
        }
        ips.extend(crate::sandbox::windows_wfp::system_dns_servers());
        crate::netbroker::provision(sid_to_bytes(sid), ips)
    }

    pub fn run_blocking(
        req: &ExecRequest,
        policy: &SandboxPolicy,
    ) -> Result<ExecResult, ShellError> {
        // Network children get the `internetClient` capability + a WFP filter set
        // scoped to their package SID (host/IP allowlist). Denied children get no
        // capability and no WFP, so the OS blocks all outbound by construction.
        let net = policy.allow_net;
        let sid = appcontainer_sid(net)?;

        // Grant the AppContainer access to exactly the policy's paths. Without an
        // ACE for the package SID a lowbox child cannot touch user files at all.
        for p in &policy.write_paths {
            if let Some(s) = p.to_str() {
                stamp_ace(s, sid, GENERIC_READ | GENERIC_WRITE);
            }
        }
        for p in &policy.read_paths {
            if let Some(s) = p.to_str() {
                stamp_ace(s, sid, GENERIC_READ);
            }
        }

        // Grant read+execute on the binary's own directory so a user-installed
        // program and the DLLs next to it can be loaded. System binaries under
        // System32 already grant ALL_APPLICATION_PACKAGES (which the lowbox token
        // holds), so they start; a binary in a user directory (e.g. a packaged
        // Python) does not, and its child-side DLL loads fail with
        // STATUS_DLL_NOT_FOUND / DLL_INIT_FAILED. The grant is skipped when the
        // package SID is already on the directory, so the inheritable propagation
        // is paid once, not on every invocation.
        let aap = all_app_packages_sid();
        if let Some(bin_dir) = resolve_bin_dir(&req.bin)
            && let Some(s) = bin_dir.to_str()
        {
            let known: Vec<PSID> = [Some(sid), aap].into_iter().flatten().collect();
            if !dacl_contains_any(s, &known) {
                stamp_ace(s, sid, GENERIC_READ | GENERIC_EXECUTE);
            }
        }
        if let Some(p) = aap {
            unsafe {
                let _ = LocalFree(HLOCAL(p.0));
            }
        }

        // Network child: ask the broker to provision the WFP allowlist scoped to
        // this package SID. The guard is held for the child's lifetime; dropping
        // it (function exit) closes the broker connection, tearing filters down.
        // Fail closed with guidance if the broker isn't installed.
        let _net_guard = if net {
            if !crate::netbroker::available() {
                unsafe { FreeSid(sid) };
                return Err(sb(
                    "network sandbox not installed: run `daemon --install-sandbox`",
                ));
            }
            Some(provision_net(sid, policy)?)
        } else {
            None
        };

        // stdio pipes. stdout/stderr: child inherits the WRITE end (false).
        // stdin: child inherits the READ end (true). The shared SD grants the
        // AppContainer access to the inherited ends.
        let pipe_sd = pipe_security_descriptor()?;
        let out = make_pipe(false, pipe_sd)?;
        let err = make_pipe(false, pipe_sd)?;
        let inp = make_pipe(true, pipe_sd)?;

        // Command line: "bin" "arg" "arg"...
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

        // No proxy env: enforcement is the WFP allowlist + AppContainer.
        let env_block: Option<Vec<u16>> = None;

        // A network child gets the `internetClient` capability so the OS lets it
        // attempt DNS/TCP; WFP then restricts it to allowed IPs. `caps` and
        // `ic_sid` must outlive `CreateProcessW`.
        let (caps, ic_sid) = if net {
            internet_client_capability()
        } else {
            (Vec::new(), PSID::default())
        };
        let sec_caps = SECURITY_CAPABILITIES {
            AppContainerSid: sid,
            Capabilities: if caps.is_empty() {
                std::ptr::null_mut()
            } else {
                caps.as_ptr() as *mut SID_AND_ATTRIBUTES
            },
            CapabilityCount: caps.len() as u32,
            Reserved: 0,
        };

        // Build a proc-thread attribute list carrying the security capabilities.
        let mut attr_size: usize = 0;
        unsafe {
            // First call sizes the list; it "fails" with ERROR_INSUFFICIENT_BUFFER.
            let _ = InitializeProcThreadAttributeList(
                LPPROC_THREAD_ATTRIBUTE_LIST(std::ptr::null_mut()),
                1,
                0,
                &mut attr_size,
            );
        }
        let mut attr_buf = vec![0u8; attr_size];
        let attr_list =
            LPPROC_THREAD_ATTRIBUTE_LIST(attr_buf.as_mut_ptr() as *mut core::ffi::c_void);
        unsafe {
            InitializeProcThreadAttributeList(attr_list, 1, 0, &mut attr_size).map_err(sb)?;
            UpdateProcThreadAttribute(
                attr_list,
                0,
                PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
                Some(&sec_caps as *const _ as *const core::ffi::c_void),
                std::mem::size_of::<SECURITY_CAPABILITIES>(),
                None,
                None,
            )
            .map_err(sb)?;
        }

        let mut siex = STARTUPINFOEXW::default();
        siex.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        siex.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        siex.StartupInfo.hStdInput = inp.read;
        siex.StartupInfo.hStdOutput = out.write;
        siex.StartupInfo.hStdError = err.write;
        siex.lpAttributeList = attr_list;

        let mut pi = PROCESS_INFORMATION::default();

        let env_ptr = env_block
            .as_ref()
            .map(|b| b.as_ptr() as *const core::ffi::c_void);

        let spawn = unsafe {
            CreateProcessW(
                PCWSTR::null(),
                PWSTR(cmdline_w.as_mut_ptr()),
                None,
                None,
                true, // inherit handles (the stdio pipe ends)
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                env_ptr,
                PCWSTR::null(),
                &siex.StartupInfo,
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

        // Teardown. Dropping `_net_guard` closes the broker connection, which
        // makes the broker remove this child's WFP filters.
        drop(_net_guard);
        unsafe {
            DeleteProcThreadAttributeList(attr_list);
            let _ = CloseHandle(out.read);
            let _ = CloseHandle(err.read);
            let _ = CloseHandle(inp.write);
            FreeSid(sid);
            if !ic_sid.0.is_null() {
                let _ = LocalFree(HLOCAL(ic_sid.0));
            }
            if !pipe_sd.0.is_null() {
                let _ = LocalFree(HLOCAL(pipe_sd.0));
            }
        }
        // `attr_buf` / `caps` stay alive until here, after spawn.

        result
    }
}

// Non-Windows compile shim so the module type-checks on other targets.
#[cfg(not(target_os = "windows"))]
mod imp {
    use crate::policy::SandboxPolicy;
    use crate::{ExecRequest, ExecResult, ShellError};

    pub fn run_blocking(
        _req: &ExecRequest,
        _policy: &SandboxPolicy,
        _proxy_addr: Option<std::net::SocketAddr>,
    ) -> Result<ExecResult, ShellError> {
        Err(ShellError::SandboxUnavailable)
    }
}
