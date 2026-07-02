//! Native-OS sandbox backend dispatch: filesystem confinement + host/IP-granular
//! network. Enforcement is per-backend — a re-exec'd netns supervisor (Linux),
//! an argv wrapper + pf (macOS), and a custom AppContainer + WFP spawn (Windows).

use crate::policy::{SandboxError, SandboxPolicy};

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod backend;
#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod backend;
#[cfg(target_os = "windows")]
#[path = "windows.rs"]
mod backend;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
#[path = "unsupported.rs"]
mod backend;

#[cfg(target_os = "macos")]
pub use backend::wrap_argv;

#[cfg(target_os = "macos")]
pub mod macos_transparent;

#[cfg(target_os = "macos")]
pub use macos_transparent::run_contained as macos_run_contained;

#[cfg(unix)]
pub mod macos_broker;

#[cfg(unix)]
pub mod macos_pf_rules;

#[cfg(target_os = "windows")]
pub use backend::run_contained as windows_run_contained;

#[cfg(target_os = "windows")]
pub mod windows_wfp;

#[cfg(target_os = "linux")]
pub mod linux_net;

#[cfg(target_os = "linux")]
pub mod linux_transparent;

#[cfg(target_os = "linux")]
pub(crate) mod seccomp_linux;

/// If this process was re-exec'd as the in-netns network supervisor, run it and
/// exit. The host binary (daemon) must call this first thing in `main`, before
/// any threads/async runtime start. No-op when not in supervisor mode or off
/// Linux.
pub fn run_netns_supervisor_if_requested() {
    #[cfg(target_os = "linux")]
    linux_transparent::run_supervisor_if_requested();
}

/// Apply the policy to the CURRENT process/thread (call site is the forked
/// child via pre_exec, or the wrapper path). `unrestricted` short-circuits.
pub fn apply(policy: &SandboxPolicy) -> Result<(), SandboxError> {
    if policy.unrestricted {
        return Ok(());
    }
    backend::apply(policy)
}

/// Whether a real enforcing backend exists here. Used to fail closed early.
pub fn is_supported() -> bool {
    backend::is_supported()
}

/// Run the platform's one-time privileged network-sandbox setup; a no-op where
/// none is needed. On Windows it requires Administrator (see `windows::install`).
pub fn install() -> Result<(), SandboxError> {
    #[cfg(target_os = "windows")]
    {
        backend::install().map_err(|e| SandboxError::Apply(e.to_string()))
    }
    #[cfg(not(target_os = "windows"))]
    {
        Ok(())
    }
}

/// Whether host-granular network containment can be enforced here.
pub fn net_supported() -> bool {
    backend::net_supported()
}

#[cfg(test)]
mod tests {
    use crate::policy::SandboxPolicy;

    #[test]
    fn unrestricted_policy_is_a_noop() {
        let p = SandboxPolicy {
            unrestricted: true,
            ..Default::default()
        };
        // apply() must short-circuit on unrestricted and never touch a backend.
        super::apply(&p).expect("unrestricted must succeed without enforcement");
    }
}
