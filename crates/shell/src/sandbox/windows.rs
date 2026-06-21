//! Windows sandbox backend: **AppContainer** filesystem + network confinement.
//!
//! The child is launched into an AppContainer (a low-privilege "lowbox" token)
//! via `CreateProcessW` + a `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`
//! attribute. Two properties fall out of the AppContainer model, neither of
//! which requires administrator rights (unlike the old WFP approach):
//!
//! - **Network**: an AppContainer has NO outbound network unless it holds a
//!   network capability. We grant `internetClient` only when the policy permits
//!   network; with `allow_net = false` the capability set is empty and the OS
//!   firewall blocks all outbound by construction. This is how Chromium / Codex
//!   sandbox network without elevation.
//! - **Filesystem**: a lowbox token can only touch objects whose ACL grants the
//!   AppContainer's package SID (or `ALL_APPLICATION_PACKAGES`). System binaries
//!   and their DLLs already grant `ALL_APPLICATION_PACKAGES`, so programs start;
//!   user data does not, so the child is confined. We stamp the package SID onto
//!   exactly the granted read/write paths.

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

/// Run `req` inside an AppContainer. `proxy_addr` is the egress proxy loopback
/// address used for host-granular filtering when network is permitted; it is
/// `None` when the policy denies network.
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

    use windows::Win32::Foundation::{
        CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS, HLOCAL, LocalFree,
        SetHandleInformation,
    };
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, EXPLICIT_ACCESS_W, GRANT_ACCESS,
        GetNamedSecurityInfoW, NO_MULTIPLE_TRUSTEE, SDDL_REVISION_1, SE_FILE_OBJECT,
        SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
    };
    use windows::Win32::Security::Isolation::{
        CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
    };
    use windows::Win32::Security::{
        ACL, CreateWellKnownSid, DACL_SECURITY_INFORMATION, FreeSid, PSECURITY_DESCRIPTOR, PSID,
        SECURITY_ATTRIBUTES, SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES,
        SUB_CONTAINERS_AND_OBJECTS_INHERIT, WinCapabilityInternetClientSid,
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

    /// Fixed AppContainer profile name. Reused across invocations (created once,
    /// then derived); it carries no per-call state — confinement comes from the
    /// empty/`internetClient` capability set and the per-call path ACEs.
    const PROFILE_NAME: &str = "agentd.shell.sandbox";

    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const SE_GROUP_ENABLED: u32 = 0x0000_0004;

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

    /// Ensure the AppContainer profile exists and return its package SID. The
    /// profile is created on first use; subsequent calls hit `ALREADY_EXISTS`
    /// and fall back to deriving the SID from the (stable) name.
    fn appcontainer_sid() -> Result<PSID, ShellError> {
        let name = to_wide(PROFILE_NAME);
        let display = to_wide("agent.d shell sandbox");
        let desc = to_wide("agent.d confined shell execution");
        unsafe {
            match CreateAppContainerProfile(
                PCWSTR(name.as_ptr()),
                PCWSTR(display.as_ptr()),
                PCWSTR(desc.as_ptr()),
                None,
            ) {
                Ok(sid) => Ok(sid),
                Err(_) => DeriveAppContainerSidFromAppContainerName(PCWSTR(name.as_ptr()))
                    .map_err(|e| sb(format!("derive AppContainer SID: {e}"))),
            }
        }
    }

    /// Owns the buffer backing a `internetClient` capability SID and exposes it
    /// as a `SID_AND_ATTRIBUTES`. The SID points into `buf`, so this must outlive
    /// any `SECURITY_CAPABILITIES` referencing it.
    struct Capability {
        buf: Vec<u8>,
    }

    fn internet_capability() -> Result<Capability, ShellError> {
        unsafe {
            let mut size = 0u32;
            // First call sizes the buffer (returns false / ERROR_INSUFFICIENT_BUFFER).
            let _ = CreateWellKnownSid(
                WinCapabilityInternetClientSid,
                None,
                PSID::default(),
                &mut size,
            );
            if size == 0 {
                return Err(sb("CreateWellKnownSid sizing failed"));
            }
            let mut buf = vec![0u8; size as usize];
            CreateWellKnownSid(
                WinCapabilityInternetClientSid,
                None,
                PSID(buf.as_mut_ptr() as *mut core::ffi::c_void),
                &mut size,
            )
            .map_err(sb)?;
            Ok(Capability { buf })
        }
    }

    impl Capability {
        fn as_sid_and_attributes(&mut self) -> SID_AND_ATTRIBUTES {
            SID_AND_ATTRIBUTES {
                Sid: PSID(self.buf.as_mut_ptr() as *mut core::ffi::c_void),
                Attributes: SE_GROUP_ENABLED,
            }
        }
    }

    /// Best-effort: stamp an access-allowed ACE for `sid` on `path` (subtree).
    /// `write` selects read+write vs read-only. A failure leaves the path
    /// inaccessible to the AppContainer (safe / over-restrictive).
    fn stamp_ace(path: &str, sid: PSID, write: bool) {
        let wide = to_wide(path);
        let mask = if write {
            GENERIC_READ | GENERIC_WRITE
        } else {
            GENERIC_READ
        };
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
        let sid = appcontainer_sid()?;

        // Grant the AppContainer access to exactly the policy's paths. Without an
        // ACE for the package SID a lowbox child cannot touch user files at all.
        for p in &policy.write_paths {
            if let Some(s) = p.to_str() {
                stamp_ace(s, sid, true);
            }
        }
        for p in &policy.read_paths {
            if let Some(s) = p.to_str() {
                stamp_ace(s, sid, false);
            }
        }

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

        // Capability set: `internetClient` only when network is permitted.
        // Empty otherwise — the AppContainer then has no outbound network.
        let mut internet = if policy.allow_net {
            Some(internet_capability()?)
        } else {
            None
        };
        let mut caps: Vec<SID_AND_ATTRIBUTES> = Vec::new();
        if let Some(cap) = internet.as_mut() {
            caps.push(cap.as_sid_and_attributes());
        }

        let sec_caps = SECURITY_CAPABILITIES {
            AppContainerSid: sid,
            Capabilities: if caps.is_empty() {
                std::ptr::null_mut()
            } else {
                caps.as_mut_ptr()
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

        // Teardown.
        unsafe {
            DeleteProcThreadAttributeList(attr_list);
            let _ = CloseHandle(out.read);
            let _ = CloseHandle(err.read);
            let _ = CloseHandle(inp.write);
            FreeSid(sid);
            if !pipe_sd.0.is_null() {
                let _ = LocalFree(HLOCAL(pipe_sd.0));
            }
        }
        // `internet` / `caps` / `attr_buf` stay alive until here, after spawn.
        drop(internet);

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
