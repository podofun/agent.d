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
//!
//! No runtime FS-containment primitive exists on Windows without a kernel driver
//! or a VM, so we confine by stamping ACLs on the granted paths (+ a
//! metadata/traverse ACE on ancestors, so path canonicalization works). Grants
//! must carry the *full* access class or tools half-work: reads add
//! `GENERIC_EXECUTE` (dir traverse / `chdir`), writes add `DELETE` (atomic
//! rewrites), ancestors add `FILE_LIST_DIRECTORY` (POSIX `getcwd`). Stamping a
//! large dir is slow (inheritable ACE materializes over the subtree); every ACE
//! is ledgered and revoked on shutdown/uninstall/crash-restart, so nothing
//! persists. User-facing notes live in docs/v0/security/sandbox.md.

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
        .map_err(|e| {
            crate::ShellError::Sandbox(format!("a background sandbox task failed ({e})"))
        })?
}

/// One-time privileged sandbox setup; see [`imp::install`].
pub fn install() -> Result<(), crate::ShellError> {
    imp::install()
}

/// Open ancestor-directory metadata/traverse for the sandbox so path
/// canonicalization works for arbitrary programs — the Windows equivalent of
/// the metadata allowances Linux/macOS already give. Requires Administrator; the
/// broker calls this from its elevated install. See [`imp::grant_metadata_traversal`].
#[cfg(target_os = "windows")]
pub fn grant_metadata_traversal() -> Result<(), crate::ShellError> {
    imp::grant_metadata_traversal()
}

/// Reverse [`grant_metadata_traversal`]; the broker calls this from uninstall.
#[cfg(target_os = "windows")]
pub fn revoke_metadata_traversal() -> Result<(), crate::ShellError> {
    imp::revoke_metadata_traversal()
}

/// Let sandboxed children create global named pipes so Node/libuv toolchains
/// (npm spawning cmd/node over stdio pipes) don't deadlock. Requires
/// Administrator; the broker calls this from its elevated install. See
/// [`imp::grant_pipe_namespace`].
#[cfg(target_os = "windows")]
pub fn grant_pipe_namespace() -> Result<(), crate::ShellError> {
    imp::grant_pipe_namespace()
}

/// Reverse [`grant_pipe_namespace`]; the broker calls this from uninstall.
#[cfg(target_os = "windows")]
pub fn revoke_pipe_namespace() -> Result<(), crate::ShellError> {
    imp::revoke_pipe_namespace()
}

/// Undo every filesystem ACE the daemon stamped (granted paths + ancestors),
/// removing the agent.d-owned ACL entries. Called on graceful shutdown,
/// at startup (to heal a prior crash), and on uninstall. See
/// [`imp::revoke_all_stamps`].
#[cfg(target_os = "windows")]
pub fn revoke_all_stamps() {
    imp::revoke_all_stamps()
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
        EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW, NO_MULTIPLE_TRUSTEE, REVOKE_ACCESS,
        SDDL_REVISION_1, SE_FILE_OBJECT, SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID,
        TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
    };
    use windows::Win32::Security::GetLengthSid;

    use windows::Win32::Security::Isolation::{
        CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
    };
    use windows::Win32::Security::{
        ACE_FLAGS, ACL, DACL_SECURITY_INFORMATION, FreeSid, GetKernelObjectSecurity,
        GetSecurityDescriptorDacl, InitializeSecurityDescriptor, PSECURITY_DESCRIPTOR, PSID,
        SECURITY_ATTRIBUTES, SECURITY_CAPABILITIES, SECURITY_DESCRIPTOR, SID_AND_ATTRIBUTES,
        SUB_CONTAINERS_AND_OBJECTS_INHERIT, SetFileSecurityW, SetKernelObjectSecurity,
        SetSecurityDescriptorDacl,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
        GetLogicalDrives, OPEN_EXISTING, READ_CONTROL, ReadFile, WRITE_DAC, WriteFile,
    };
    use windows::Win32::System::Pipes::CreatePipe;
    use windows::Win32::System::Threading::{
        CREATE_NEW_CONSOLE, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
        DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE,
        InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
        PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, PROCESS_INFORMATION, STARTF_USESHOWWINDOW,
        STARTF_USESTDHANDLES, STARTUPINFOEXW, UpdateProcThreadAttribute, WaitForSingleObject,
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
    // GENERIC_WRITE does NOT include DELETE. Real tools rewrite files
    // atomically (write `foo.lock`, then rename/unlink over `foo`), and unlink /
    // rename need DELETE on the object — without it, git init fails with
    // "unlink config.lock: Invalid argument" / "could not write config:
    // Permission denied". A write grant must therefore carry DELETE so files
    // *inside* the granted subtree can be replaced and removed.
    const DELETE: u32 = 0x0001_0000;

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
                .map_err(|e| sb(format!("could not derive the AppContainer SID ({e})")))
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

    // ---- ancestor metadata/traverse (cross-platform parity) ----
    //
    // On Linux (Landlock) a sandboxed process may `lstat`/traverse ancestor
    // directories it was never granted — Landlock rules only the granted
    // subtrees and leaves traversal of everything above them implicit. macOS
    // does the same explicitly in its Seatbelt profile:
    //     (allow file-read-metadata)          ; stat/readlink anywhere, no contents
    //     (allow file-read* (literal "/"))    ; the root object, nothing below it
    // Windows breaks this: a lowbox child can only touch objects whose ACL
    // grants its package SID (or ALL_APPLICATION_PACKAGES). The system-owned
    // ancestor roots (each fixed-drive root and `%SystemDrive%\Users`) grant
    // neither, so the child cannot `lstat` them — and path canonicalization
    // (Node's realpathSync, git's getcwd, the CRT's _fullpath, .NET's
    // Path.GetFullPath, ...) walks to the drive root, so *most* Windows programs
    // fail to even start under the sandbox.
    //
    // We close the gap by granting the package SIDs traverse + metadata + list
    // on the ancestor chain, with a NON-inheritable ACE so it applies to each
    // directory object only and never propagates into (or materializes onto) the
    // tree below it. Crucially it grants NO FILE_READ_DATA and NO write bits, so
    // the child still cannot read any file's contents or modify anything outside
    // its grants — only stat, traverse, and *enumerate* the ancestor
    // directories.
    //
    // FILE_LIST_DIRECTORY (enumerate) is a deliberate, documented deviation from
    // the strict Linux/macOS baseline (which allow ancestor stat+traverse but not
    // ancestor readdir). It is required by POSIX-emulation getcwd (git for
    // Windows / MSYS resolves its working directory by `readdir`-walking parents)
    // and is harmless for native programs. The cost is that a sandboxed child can
    // see the *names* of entries in its ancestor directories (not their
    // contents). File contents and all writes remain fully confined.
    //
    // The system-owned tops (drive roots, %SystemDrive%\Users) need Administrator
    // and are stamped by the elevated broker; the user-owned ancestors below them
    // the daemon stamps per exec. Every stamp is recorded in a ledger and undone
    // on shutdown/uninstall (see record_stamp / revoke_all_stamps).

    /// Ancestor access: `FILE_LIST_DIRECTORY` (0x1) | `FILE_TRAVERSE` (0x20) |
    /// `FILE_READ_ATTRIBUTES` (0x80) | `READ_CONTROL` (0x20000) | `SYNCHRONIZE`
    /// (0x100000). No `FILE_READ_DATA`, no write bits — enumerate/stat/traverse
    /// only, never file contents.
    const TRAVERSE_META: u32 = 0x1 | 0x20 | 0x80 | 0x0002_0000 | 0x0010_0000;

    /// The system-owned ancestor roots that block AppContainer canonicalization:
    /// every fixed-drive root plus `%SystemDrive%\Users`.
    fn ancestor_roots() -> Vec<String> {
        let mut roots = Vec::new();
        let mask = unsafe { GetLogicalDrives() };
        for i in 0..26u32 {
            if mask & (1 << i) != 0 {
                let letter = (b'A' + i as u8) as char;
                roots.push(format!("{letter}:\\"));
            }
        }
        let sys = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        roots.push(format!("{sys}\\Users"));
        roots
    }

    /// Add or remove (`grant=false`) a single non-inheritable ACE for `sid` on
    /// `path`. `grant=false` revokes every ACE the SID holds there. Best-effort.
    fn set_meta_ace(path: &str, sid: PSID, grant: bool) -> bool {
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
                return false;
            }
            let mut written = false;
            let ea = EXPLICIT_ACCESS_W {
                grfAccessPermissions: if grant { TRAVERSE_META } else { 0 },
                grfAccessMode: if grant { GRANT_ACCESS } else { REVOKE_ACCESS },
                grfInheritance: ACE_FLAGS(0), // NO_INHERITANCE: this object only
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
                // Write with the legacy SetFileSecurity: it sets the DACL
                // literally and does NOT walk/repropagate inheritance into the
                // subtree the way SetNamedSecurityInfo does. That matters — an
                // ancestor like the Desktop can hold an enormous tree, and
                // repropagation there costs minutes. Our ACE is non-inheritable,
                // so children are unaffected and need no recompute.
                let mut sd = SECURITY_DESCRIPTOR::default();
                let psd_new = PSECURITY_DESCRIPTOR(&mut sd as *mut _ as *mut core::ffi::c_void);
                const SECURITY_DESCRIPTOR_REVISION: u32 = 1;
                if InitializeSecurityDescriptor(psd_new, SECURITY_DESCRIPTOR_REVISION).is_ok()
                    && SetSecurityDescriptorDacl(psd_new, true, Some(new_dacl), false).is_ok()
                {
                    written =
                        SetFileSecurityW(PCWSTR(wide.as_ptr()), DACL_SECURITY_INFORMATION, psd_new)
                            .as_bool();
                }
                let _ = LocalFree(HLOCAL(new_dacl as *mut core::ffi::c_void));
            }
            if !psd.0.is_null() {
                let _ = LocalFree(HLOCAL(psd.0));
            }
            written
        }
    }

    /// Grant (`grant=true`) or revoke the metadata/traverse ACE for both package
    /// SIDs across every ancestor root. Runs elevated (broker install/uninstall).
    fn set_metadata_traversal(grant: bool) -> Result<(), ShellError> {
        let roots = ancestor_roots();
        for name in [PROFILE_CONFINED, PROFILE_NET] {
            let wname = to_wide(name);
            let sid = unsafe { DeriveAppContainerSidFromAppContainerName(PCWSTR(wname.as_ptr())) }
                .map_err(|e| sb(format!("could not derive the {name} package SID ({e})")))?;
            for root in &roots {
                if !set_meta_ace(root, sid, grant) {
                    unsafe { FreeSid(sid) };
                    return Err(sb(format!("could not update ancestor ACL for `{root}`")));
                }
            }
            unsafe { FreeSid(sid) };
        }
        Ok(())
    }

    /// Open ancestor path canonicalization for the sandbox (see the block
    /// comment above). Idempotent; requires Administrator.
    pub fn grant_metadata_traversal() -> Result<(), ShellError> {
        set_metadata_traversal(true)
    }

    /// Reverse [`grant_metadata_traversal`].
    pub fn revoke_metadata_traversal() -> Result<(), ShellError> {
        set_metadata_traversal(false)
    }

    // ---- NPFS named-pipe root: let AppContainer children create global pipes ----
    //
    // A child that spawns its own children over stdio pipes (npm → cmd → node,
    // any Node/libuv toolchain) needs to *create* named pipes. libuv < the
    // unreleased LOCAL\ fix names those pipes `\\?\pipe\uv\...` in the GLOBAL
    // NPFS namespace. An AppContainer token cannot create an object there, so
    // CreateNamedPipe returns ERROR_ACCESS_DENIED — and libuv's creation loop
    // treats that as a name collision and retries FOREVER, so the grandchild
    // spawn hangs (npm install/create never returns). No released Node ships the
    // fix, and the daemon can't change how Node names its pipes.
    //
    // We close the gap the way Chromium's sandbox did pre-fix: grant the package
    // SIDs create rights on the NPFS device root (`\\.\pipe\`). This lets an
    // appcontained process create a global pipe; it grants NO filesystem and NO
    // network access. The relaxation is that a sandboxed process can create /
    // squat global pipe *names* — the same surface the daemon's own broker pipe
    // already lives on. It is a system-wide, machine-persistent-until-reboot ACE,
    // so only the elevated broker sets it (install), and it is removed at
    // uninstall.

    /// NPFS create access. Creating `\Device\NamedPipe\uv\<name>` requires the
    /// right to add a name to the directory object: `FILE_ADD_FILE` (0x2) and
    /// `FILE_ADD_SUBDIRECTORY`/`FILE_CREATE_PIPE_INSTANCE` (0x4, same bit value on
    /// NPFS). Plus `FILE_TRAVERSE` (0x20) to open the root, `FILE_READ_ATTRIBUTES`
    /// (0x80), `READ_CONTROL` (0x2_0000), `SYNCHRONIZE` (0x10_0000). No
    /// `FILE_READ_DATA`/`FILE_WRITE_DATA` — a child cannot read or write any
    /// *other* process's pipe, only create its own.
    const PIPE_CREATE: u32 = 0x2 | 0x4 | 0x20 | 0x80 | 0x0002_0000 | 0x0010_0000;

    /// Add (`grant=true`) or remove the pipe-create ACE for `sid` on the NPFS
    /// device root, propagating the real Win32 error on failure so the elevated
    /// broker install fails loudly rather than silently leaving pipes broken.
    fn set_pipe_ace(sid: PSID, grant: bool) -> Result<(), ShellError> {
        // Open the NPFS control object itself (`\\.\pipe`, no trailing slash — a
        // trailing slash asks NPFS for a *file within* the device and is rejected
        // for a raw security query). WRITE_DAC to rewrite its DACL, READ_CONTROL
        // to read it.
        let wide = to_wide("\\\\.\\pipe");
        unsafe {
            let handle = CreateFileW(
                PCWSTR(wide.as_ptr()),
                READ_CONTROL.0 | WRITE_DAC.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
            .map_err(|e| sb(format!("open NPFS root failed: {e}")))?;

            let finish = |e: ShellError| -> ShellError {
                let _ = CloseHandle(handle);
                e
            };

            // Read the current self-relative security descriptor into a buffer
            // (two-call pattern: size, then fetch). GetKernelObjectSecurity works
            // on the raw device handle where GetSecurityInfo(SE_KERNEL_OBJECT)
            // returns ERROR_INVALID_PARAMETER (87).
            let mut needed = 0u32;
            let _ = GetKernelObjectSecurity(
                handle,
                DACL_SECURITY_INFORMATION.0,
                PSECURITY_DESCRIPTOR::default(),
                0,
                &mut needed,
            );
            if needed == 0 {
                return Err(finish(sb("GetKernelObjectSecurity(NPFS) returned 0 size")));
            }
            let mut buf = vec![0u8; needed as usize];
            let psd_old = PSECURITY_DESCRIPTOR(buf.as_mut_ptr() as *mut core::ffi::c_void);
            GetKernelObjectSecurity(
                handle,
                DACL_SECURITY_INFORMATION.0,
                psd_old,
                needed,
                &mut needed,
            )
            .map_err(|e| finish(sb(format!("GetKernelObjectSecurity(NPFS) failed: {e}"))))?;

            // Extract the existing DACL to merge our ACE into.
            let mut present = windows::Win32::Foundation::BOOL(0);
            let mut old_dacl: *mut ACL = std::ptr::null_mut();
            let mut defaulted = windows::Win32::Foundation::BOOL(0);
            GetSecurityDescriptorDacl(psd_old, &mut present, &mut old_dacl, &mut defaulted)
                .map_err(|e| finish(sb(format!("GetSecurityDescriptorDacl(NPFS) failed: {e}"))))?;

            let ea = EXPLICIT_ACCESS_W {
                grfAccessPermissions: if grant { PIPE_CREATE } else { 0 },
                grfAccessMode: if grant { GRANT_ACCESS } else { REVOKE_ACCESS },
                grfInheritance: ACE_FLAGS(0),
                Trustee: TRUSTEE_W {
                    pMultipleTrustee: std::ptr::null_mut(),
                    MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                    TrusteeForm: TRUSTEE_IS_SID,
                    TrusteeType: TRUSTEE_IS_UNKNOWN,
                    ptstrName: PWSTR(sid.0 as *mut u16),
                },
            };
            let merge_into = if present.as_bool() && !old_dacl.is_null() {
                Some(old_dacl as *const ACL)
            } else {
                None
            };
            let mut new_dacl: *mut ACL = std::ptr::null_mut();
            let rc = SetEntriesInAclW(Some(&[ea]), merge_into, &mut new_dacl);
            if rc.is_err() || new_dacl.is_null() {
                return Err(finish(sb(format!("SetEntriesInAcl(NPFS) failed: {rc:?}"))));
            }

            // Build a fresh absolute SD carrying the merged DACL and write it back.
            let mut sd = SECURITY_DESCRIPTOR::default();
            let psd_new = PSECURITY_DESCRIPTOR(&mut sd as *mut _ as *mut core::ffi::c_void);
            const SECURITY_DESCRIPTOR_REVISION: u32 = 1;
            let write = InitializeSecurityDescriptor(psd_new, SECURITY_DESCRIPTOR_REVISION)
                .and_then(|_| SetSecurityDescriptorDacl(psd_new, true, Some(new_dacl), false))
                .and_then(|_| SetKernelObjectSecurity(handle, DACL_SECURITY_INFORMATION, psd_new));
            let _ = LocalFree(HLOCAL(new_dacl as *mut core::ffi::c_void));
            write.map_err(|e| finish(sb(format!("SetKernelObjectSecurity(NPFS) failed: {e}"))))?;
            let _ = CloseHandle(handle);
        }
        Ok(())
    }

    /// Grant or revoke NPFS-root pipe-create for both package SIDs.
    fn set_pipe_namespace(grant: bool) -> Result<(), ShellError> {
        for name in [PROFILE_CONFINED, PROFILE_NET] {
            let wname = to_wide(name);
            let sid = unsafe { DeriveAppContainerSidFromAppContainerName(PCWSTR(wname.as_ptr())) }
                .map_err(|e| sb(format!("could not derive the {name} package SID ({e})")))?;
            let r = set_pipe_ace(sid, grant);
            unsafe { FreeSid(sid) };
            r?;
        }
        // NPFS evaluates an AppContainer's create access against
        // ALL_APPLICATION_PACKAGES (S-1-15-2-1), not the per-package SID the way
        // NTFS does — so the per-profile ACEs above are not enough on their own.
        let aap = to_wide("S-1-15-2-1");
        let mut aap_sid = PSID::default();
        unsafe {
            ConvertStringSidToSidW(PCWSTR(aap.as_ptr()), &mut aap_sid).map_err(|e| {
                sb(format!(
                    "could not derive ALL_APPLICATION_PACKAGES SID ({e})"
                ))
            })?;
        }
        let r = set_pipe_ace(aap_sid, grant);
        unsafe { LocalFree(HLOCAL(aap_sid.0)) };
        r?;
        Ok(())
    }

    /// Let sandboxed children create global named pipes (see the block comment).
    /// Idempotent; requires Administrator.
    pub fn grant_pipe_namespace() -> Result<(), ShellError> {
        set_pipe_namespace(true)
    }

    /// Reverse [`grant_pipe_namespace`].
    pub fn revoke_pipe_namespace() -> Result<(), ShellError> {
        set_pipe_namespace(false)
    }

    // ---- stamp ledger: every ACE the daemon writes is recorded so it can be
    // undone when the daemon exits. Two ACE shapes:
    //   'I' = inheritable grant on a granted read/write path (or a binary dir).
    //         Materialized onto the subtree; revoking must re-propagate the
    //         removal (SetNamedSecurityInfo), which is why it is not free.
    //   'N' = non-inheritable ancestor metadata/traverse ACE (this object only).
    //         Revoked with SetFileSecurity — single object, fast.

    /// Path of the on-disk ledger, under the user's local app data.
    fn ledger_path() -> Option<std::path::PathBuf> {
        std::env::var_os("LOCALAPPDATA").map(|d| {
            std::path::PathBuf::from(d)
                .join("agentd")
                .join("stamped_acls.tsv")
        })
    }

    /// In-process dedup so a hot loop of execs doesn't append the same path
    /// thousands of times.
    fn recorded_set() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
        use std::sync::OnceLock;
        static SET: OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> = OnceLock::new();
        SET.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
    }

    /// Record that `path` was stamped with ACE shape `kind` ('I' or 'N'), so
    /// shutdown/uninstall can revoke it. Best-effort and idempotent per process.
    fn record_stamp(kind: char, path: &str) {
        let key = format!("{kind}\t{path}");
        {
            let mut set = recorded_set().lock().unwrap_or_else(|e| e.into_inner());
            if !set.insert(key.clone()) {
                return;
            }
        }
        if let Some(p) = ledger_path() {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&p)
            {
                let _ = writeln!(f, "{key}");
            }
        }
    }

    /// Remove every ACE for `sid` on `path`. `inheritable` selects the write
    /// path: an inheritable grant must re-propagate the removal down the subtree
    /// (SetNamedSecurityInfo); a non-inheritable ancestor ACE is removed from the
    /// single object (SetFileSecurity, fast). Best-effort.
    fn revoke_sid(path: &str, sid: PSID, inheritable: bool) {
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
                grfAccessPermissions: 0,
                grfAccessMode: REVOKE_ACCESS,
                grfInheritance: ACE_FLAGS(0),
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
                if inheritable {
                    // Re-propagate the removal so children lose the materialized ACE.
                    let _ = SetNamedSecurityInfoW(
                        PCWSTR(wide.as_ptr()),
                        SE_FILE_OBJECT,
                        DACL_SECURITY_INFORMATION,
                        None,
                        None,
                        Some(new_dacl),
                        None,
                    );
                } else {
                    let mut sd = SECURITY_DESCRIPTOR::default();
                    let psd_new = PSECURITY_DESCRIPTOR(&mut sd as *mut _ as *mut core::ffi::c_void);
                    const SECURITY_DESCRIPTOR_REVISION: u32 = 1;
                    if InitializeSecurityDescriptor(psd_new, SECURITY_DESCRIPTOR_REVISION).is_ok()
                        && SetSecurityDescriptorDacl(psd_new, true, Some(new_dacl), false).is_ok()
                    {
                        let _ = SetFileSecurityW(
                            PCWSTR(wide.as_ptr()),
                            DACL_SECURITY_INFORMATION,
                            psd_new,
                        );
                    }
                }
                let _ = LocalFree(HLOCAL(new_dacl as *mut core::ffi::c_void));
            }
            if !psd.0.is_null() {
                let _ = LocalFree(HLOCAL(psd.0));
            }
        }
    }

    /// Undo every ACE the daemon recorded in the ledger, for both package SIDs,
    /// then delete the ledger. Called on graceful shutdown, at startup (to heal
    /// a previous crash), and on uninstall — so the user's filesystem is left
    /// without leaving agent.d-owned grants behind. Best-effort: a path that
    /// fails is skipped.
    pub fn revoke_all_stamps() {
        let Some(p) = ledger_path() else { return };
        let Ok(body) = std::fs::read_to_string(&p) else {
            return;
        };
        // Derive both package SIDs once.
        let sids: Vec<PSID> = [PROFILE_CONFINED, PROFILE_NET]
            .iter()
            .filter_map(|name| {
                let wname = to_wide(name);
                unsafe { DeriveAppContainerSidFromAppContainerName(PCWSTR(wname.as_ptr())).ok() }
            })
            .collect();
        for line in body.lines() {
            let Some((kind, path)) = line.split_once('\t') else {
                continue;
            };
            let inheritable = kind == "I";
            for sid in &sids {
                revoke_sid(path, *sid, inheritable);
            }
        }
        for sid in sids {
            unsafe { FreeSid(sid) };
        }
        let _ = std::fs::remove_file(&p);
        recorded_set()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// Stamp an access-allowed ACE granting `mask` to `sid` on `path`.
    /// Directories propagate to their subtree; files receive an object-only ACE.
    fn stamp_ace(path: &str, sid: PSID, mask: u32) -> bool {
        let inheritance = if std::path::Path::new(path).is_file() {
            ACE_FLAGS(0)
        } else {
            SUB_CONTAINERS_AND_OBJECTS_INHERIT
        };
        let wide = to_wide(path);
        unsafe {
            let mut old_dacl: *mut ACL = std::ptr::null_mut();
            let mut psd = PSECURITY_DESCRIPTOR::default();
            let read = GetNamedSecurityInfoW(
                PCWSTR(wide.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(&mut old_dacl),
                None,
                &mut psd,
            );
            if read.is_err() {
                return false;
            }

            let ea = EXPLICIT_ACCESS_W {
                grfAccessPermissions: mask,
                grfAccessMode: GRANT_ACCESS,
                grfInheritance: inheritance,
                Trustee: TRUSTEE_W {
                    pMultipleTrustee: std::ptr::null_mut(),
                    MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                    TrusteeForm: TRUSTEE_IS_SID,
                    TrusteeType: TRUSTEE_IS_UNKNOWN,
                    ptstrName: PWSTR(sid.0 as *mut u16),
                },
            };
            let mut new_dacl: *mut ACL = std::ptr::null_mut();
            let merge = SetEntriesInAclW(Some(&[ea]), Some(old_dacl), &mut new_dacl);
            if merge.is_err() || new_dacl.is_null() {
                if !psd.0.is_null() {
                    let _ = LocalFree(HLOCAL(psd.0));
                }
                return false;
            }
            let write = SetNamedSecurityInfoW(
                PCWSTR(wide.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(new_dacl),
                None,
            );
            let _ = LocalFree(HLOCAL(new_dacl as *mut core::ffi::c_void));
            if !psd.0.is_null() {
                let _ = LocalFree(HLOCAL(psd.0));
            }
            if write.is_err() {
                return false;
            }
        }
        true
    }

    /// Resolve `bin` to the absolute executable path. Absolute inputs are used
    /// as-is (trying a `.exe` suffix if the literal path isn't a file);
    /// otherwise `bin` (with and without `.exe`) is looked up on the daemon's
    /// `PATH`.
    ///
    /// Resolving here — in the full-privilege daemon — is REQUIRED, not just an
    /// optimization: the child runs in an AppContainer whose lowbox token cannot
    /// stat most `PATH` directories, so `CreateProcessW`'s own bare-name search
    /// fails with "file not found" even for a binary that is plainly on `PATH`.
    /// We hand `CreateProcessW` the resolved absolute path instead.
    fn resolve_bin_path(bin: &str) -> Option<std::path::PathBuf> {
        fn canonical(path: std::path::PathBuf) -> std::path::PathBuf {
            let resolved = path.canonicalize().unwrap_or(path);
            let text = resolved.to_string_lossy();
            text.strip_prefix(r"\\?\")
                .map(std::path::PathBuf::from)
                .unwrap_or(resolved)
        }

        let p = std::path::Path::new(bin);
        if p.is_absolute() {
            if p.is_file() {
                return Some(canonical(p.to_path_buf()));
            }
            let with_exe = p.with_extension("exe");
            return with_exe.is_file().then(|| canonical(with_exe));
        }
        std::env::split_paths(&std::env::var_os("PATH")?).find_map(|d| {
            [d.join(bin), d.join(format!("{bin}.exe"))]
                .into_iter()
                .find(|c| c.is_file())
                .map(canonical)
        })
    }

    /// Whether a file already grants access to `sid`. This is used only for the
    /// resolved executable file: unlike an ancestor directory, any allow ACE for
    /// ALL_APPLICATION_PACKAGES here is sufficient evidence that Windows can map
    /// the image without an agent.d-specific grant.
    fn dacl_contains_sid(path: &str, sid: PSID) -> bool {
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
                for i in 0..(*dacl).AceCount {
                    let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
                    if GetAce(dacl, i as u32, &mut ace).is_ok() && !ace.is_null() {
                        let header = &*(ace as *const ACE_HEADER);
                        if header.AceType == ACCESS_ALLOWED_ACE_TYPE {
                            let allowed = &*(ace as *const ACCESS_ALLOWED_ACE);
                            let ace_sid =
                                PSID(&allowed.SidStart as *const u32 as *mut core::ffi::c_void);
                            if EqualSid(ace_sid, sid).is_ok() {
                                found = true;
                                break;
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

    fn all_application_packages_sid() -> Result<PSID, ShellError> {
        let text = to_wide("S-1-15-2-1");
        let mut sid = PSID::default();
        unsafe {
            ConvertStringSidToSidW(PCWSTR(text.as_ptr()), &mut sid).map_err(|e| {
                sb(format!(
                    "could not derive ALL_APPLICATION_PACKAGES SID ({e})"
                ))
            })?;
        }
        Ok(sid)
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
                // KNOWN GAP vs Linux/macOS: concrete names are pre-resolved ONCE
                // here (a staleness window on TTL/round-robin), and WILDCARD host
                // grants are dropped entirely — a wildcard-granted connection is
                // fail-closed (denied) on Windows. Closing this needs the same
                // per-connection relay the mac backend uses (WFP redirect to an
                // in-daemon relay); macOS covers both via reactive resolution +
                // forward-confirmed rDNS at connect time.
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
        let bin_path = resolve_bin_path(&req.bin).ok_or_else(|| {
            ShellError::Sandbox(format!(
                "the executable `{}` was not found — it is not an absolute path and is not on `PATH`",
                req.bin
            ))
        })?;

        // Grant the AppContainer access to exactly the policy's paths. Without an
        // ACE for the package SID a lowbox child cannot touch user files at all.
        // Grants include GENERIC_EXECUTE (FILE_TRAVERSE for directories): a
        // program must be able to `chdir`/`SetCurrentDirectory` into a granted
        // dir and traverse it, which GENERIC_READ/WRITE alone do not permit
        // (git's `setup_work_tree` chdirs into the repo → "must be run in a work
        // tree" without it). Writes additionally carry DELETE for atomic
        // rewrites (see the DELETE note).
        for p in &policy.write_paths {
            if let Some(s) = p.to_str()
                && stamp_ace(
                    s,
                    sid,
                    GENERIC_READ | GENERIC_WRITE | GENERIC_EXECUTE | DELETE,
                )
            {
                record_stamp('I', s);
            }
        }
        for p in &policy.read_paths {
            if let Some(s) = p.to_str()
                && stamp_ace(s, sid, GENERIC_READ | GENERIC_EXECUTE)
            {
                record_stamp('I', s);
            }
        }

        // No implicit read baseline on Windows: reads are confined to the cwd
        // and explicit `fs.read` grants (stamped above). See
        // policy::user_read_baseline for why auto-granting config roots was
        // removed (slow, and a silent read-everything hole).

        // Ancestor metadata/traverse. A program canonicalizes its paths at
        // startup (Node's realpathSync, git's getcwd, the CRT/.NET/Win32
        // equivalents), which `lstat`s EVERY ancestor directory of the path
        // top-down. A lowbox child can't stat a dir that grants neither its
        // package SID nor ALL_APPLICATION_PACKAGES, so canonicalization dies at
        // the first such ancestor — and none of a granted path's ancestors are
        // stamped. Grant the package SID metadata+traverse (no list, no data) on
        // each ancestor of every granted path and the cwd. The system-owned tops
        // (drive roots, %SystemDrive%\Users) are handled once by the elevated
        // broker (see grant_metadata_traversal); the user-owned ancestors below
        // them the daemon stamps here. Each stamp is recorded for teardown. The
        // ACE is non-inheritable (single object), so re-stamping is cheap and we
        // don't skip — that keeps the mask current if it ever changes.
        let ancestor_targets = policy
            .read_paths
            .iter()
            .chain(policy.write_paths.iter())
            .chain(req.cwd.iter());
        for p in ancestor_targets {
            for anc in p.ancestors().skip(1) {
                if let Some(s) = anc.to_str()
                    && !s.is_empty()
                    && set_meta_ace(s, sid, true)
                {
                    record_stamp('N', s);
                }
            }
        }

        // Grant read+execute on the binary's own directory so a user-installed
        // program and the DLLs next to it can be loaded. System binaries under
        // System32 already grant ALL_APPLICATION_PACKAGES (which the lowbox token
        // holds), so they start; a binary in a user directory (e.g. a packaged
        // Python) does not, and its child-side DLL loads fail with
        // STATUS_DLL_NOT_FOUND / DLL_INIT_FAILED. AAP-readable system images need
        // no additional grant. For other images, always stamp the full grant:
        // the ancestor pass may already have put this SID on the directory with
        // metadata-only rights, which is not enough for an npm grandchild to
        // reopen node.exe.
        let aap_sid = all_application_packages_sid()?;
        let aap_can_load = bin_path
            .to_str()
            .is_some_and(|path| dacl_contains_sid(path, aap_sid));
        unsafe { LocalFree(HLOCAL(aap_sid.0)) };
        if !aap_can_load {
            if let Some(bin_dir) = bin_path.parent().and_then(|p| p.to_str())
                && stamp_ace(bin_dir, sid, GENERIC_READ | GENERIC_EXECUTE)
            {
                record_stamp('I', bin_dir);
            }
            // Existing files do not reliably receive a newly inheritable
            // directory ACE. Stamp the image itself so a lowbox grandchild can
            // map it again (npm's generated .cmd shim does exactly this).
            if let Some(bin) = bin_path.to_str()
                && stamp_ace(bin, sid, GENERIC_READ | GENERIC_EXECUTE)
            {
                record_stamp('I', bin);
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
                    "network sandbox not installed: run `agentd --install-sandbox`",
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

        // Pass the daemon-resolved image as `lpApplicationName`; the lowbox
        // process cannot search arbitrary PATH directories itself.
        let mut app_name_w = to_wide(&bin_path.to_string_lossy());

        // Command line: "bin" "arg" "arg"... The first token is argv[0]; the
        // actual image is `lpApplicationName` above.
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
        siex.StartupInfo.dwFlags = STARTF_USESTDHANDLES | STARTF_USESHOWWINDOW;
        siex.StartupInfo.wShowWindow = 0; // SW_HIDE
        siex.StartupInfo.hStdInput = inp.read;
        siex.StartupInfo.hStdOutput = out.write;
        siex.StartupInfo.hStdError = err.write;
        siex.lpAttributeList = attr_list;

        let mut pi = PROCESS_INFORMATION::default();

        let env_ptr = env_block
            .as_ref()
            .map(|b| b.as_ptr() as *const core::ffi::c_void);

        // Honor the request's working directory. Without this the child inherits
        // the daemon's cwd, which the lowbox token cannot read — tools that
        // stat their cwd at startup (git: "Unable to read current working
        // directory") then fail. The dir must be one the child can read, i.e. a
        // granted path; callers pass a granted cwd. `lpCurrentDirectory` is the
        // one CreateProcessW argument that rejects forward slashes (fails with
        // ERROR_DIRECTORY, "The directory name is invalid"), and callers
        // routinely hand us `C:/...` paths — normalize the separators.
        let cwd_w = req
            .cwd
            .as_ref()
            .map(|c| to_wide(&c.to_string_lossy().replace('/', "\\")));
        let cwd_ptr = cwd_w
            .as_ref()
            .map(|w| PCWSTR(w.as_ptr()))
            .unwrap_or(PCWSTR::null());

        let spawn = unsafe {
            CreateProcessW(
                PCWSTR(app_name_w.as_mut_ptr()),
                PWSTR(cmdline_w.as_mut_ptr()),
                None,
                None,
                true, // inherit handles (the stdio pipe ends)
                // Give the child its own hidden console, created here with full
                // daemon privileges. Without one, the
                // first grandchild spawn that needs a console (npm → cmd.exe →
                // node) tries to create a conhost from INSIDE the lowbox, which
                // deadlocks — the AppContainer token cannot complete console
                // creation. With a console to inherit, nested spawns just work.
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_NEW_CONSOLE,
                env_ptr,
                cwd_ptr,
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
            spawn.map_err(|e| {
                ShellError::Sandbox(format!("could not start `{}` ({e})", bin_path.display()))
            })?;

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
