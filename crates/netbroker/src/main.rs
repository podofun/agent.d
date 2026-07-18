//! agent.d network broker — a tiny Windows service that performs WFP filter
//! changes on behalf of the non-admin daemon.
//!
//! WFP modification needs High integrity, which the daemon must never hold. This
//! process runs as LocalSystem and does nothing else: it accepts per-exec
//! provision requests over a local named pipe and installs/removes the matching
//! WFP filters. Minimal elevated attack surface — no AI, Lua, or HTTP linked in.
//!
//! Modes:
//! - `--install`   register + start the service (run elevated, once)
//! - `--uninstall` stop + remove the service (run elevated)
//! - (no args)     launched by the Service Control Manager: run the service

fn main() -> anyhow::Result<()> {
    #[cfg(target_os = "windows")]
    {
        imp::main()
    }
    #[cfg(not(target_os = "windows"))]
    {
        anyhow::bail!("agentd-netbroker is Windows-only")
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use std::ffi::OsString;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use windows_service::service::{
        ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
        ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    use windows_service::{define_windows_service, service_dispatcher};

    const SERVICE_NAME: &str = "agentd-netbroker";
    const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

    pub fn main() -> anyhow::Result<()> {
        match std::env::args().nth(1).as_deref() {
            Some("--install") => install(),
            Some("--uninstall") => uninstall(),
            _ => {
                // Launched by the SCM: hand control to the service dispatcher.
                service_dispatcher::start(SERVICE_NAME, ffi_service_main)?;
                Ok(())
            }
        }
    }

    define_windows_service!(ffi_service_main, service_main);

    fn service_main(_args: Vec<OsString>) {
        // Errors here surface to the SCM as a failed start; nothing else to do.
        let _ = run_service();
    }

    fn run_service() -> anyhow::Result<()> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_handler = stop.clone();

        let status_handle = service_control_handler::register(SERVICE_NAME, move |control| {
            match control {
                ServiceControl::Stop => {
                    stop_for_handler.store(true, Ordering::Relaxed);
                    // Unblock the accept loop's blocking wait by poking the pipe.
                    agentd_shell::netbroker::wake();
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        })?;

        let running = ServiceStatus {
            service_type: SERVICE_TYPE,
            current_state: ServiceState::Running,
            controls_accepted: ServiceControlAccept::STOP,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        };
        status_handle.set_service_status(running.clone())?;

        // Serve until a Stop control flips the flag.
        let _ = agentd_shell::netbroker::accept_loop(stop);

        status_handle.set_service_status(ServiceStatus {
            current_state: ServiceState::Stopped,
            ..running
        })?;
        Ok(())
    }

    fn install() -> anyhow::Result<()> {
        let manager = ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
        )?;
        let info = ServiceInfo {
            name: OsString::from(SERVICE_NAME),
            display_name: OsString::from("agent.d network broker"),
            service_type: SERVICE_TYPE,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: std::env::current_exe()?,
            launch_arguments: vec![],
            dependencies: vec![],
            account_name: None, // LocalSystem
            account_password: None,
        };
        let access = ServiceAccess::CHANGE_CONFIG
            | ServiceAccess::START
            | ServiceAccess::STOP
            | ServiceAccess::QUERY_STATUS;
        let service = match manager.create_service(&info, access) {
            Ok(s) => s,
            // Already installed — possibly registered against a binary from a
            // different (or since-removed) build directory. Repoint the
            // registration at this binary and restart it so the running broker
            // always matches the daemon that is installing.
            Err(_) => {
                let s = manager.open_service(SERVICE_NAME, access)?;
                s.change_config(&info)?;
                let _ = s.stop();
                wait_stopped(&s);
                s
            }
        };
        const ERROR_SERVICE_ALREADY_RUNNING: i32 = 1056;
        match service.start::<OsString>(&[]) {
            Ok(()) => {}
            Err(windows_service::Error::Winapi(e))
                if e.raw_os_error() == Some(ERROR_SERVICE_ALREADY_RUNNING) => {}
            Err(e) => return Err(e.into()),
        }

        // Open ancestor-directory metadata/traverse for the sandbox. This needs
        // Administrator (the roots are system-owned), which we hold here but the
        // daemon never does — so it belongs in the elevated broker install.
        // Without it, path canonicalization (realpath/getcwd) fails for most
        // Windows programs under the sandbox. See agentd_shell::sandbox.
        agentd_shell::sandbox::grant_metadata_traversal().map_err(|e| {
            anyhow::anyhow!("could not open ancestor traversal for the sandbox: {e}")
        })?;

        // Let sandboxed children create global named pipes. Also elevated (the
        // NPFS root is system-owned). Without it, any child that spawns its own
        // children over stdio pipes — npm, node toolchains — deadlocks. See
        // agentd_shell::sandbox::grant_pipe_namespace.
        agentd_shell::sandbox::grant_pipe_namespace().map_err(|e| {
            anyhow::anyhow!("could not open the pipe namespace for the sandbox: {e}")
        })?;

        println!("agent.d network broker installed and running.");
        Ok(())
    }

    /// Poll until the service reports Stopped (or ~10 s pass). A `start` issued
    /// while the old process is still winding down fails spuriously.
    fn wait_stopped(service: &windows_service::service::Service) {
        for _ in 0..100 {
            match service.query_status() {
                Ok(st) if st.current_state != ServiceState::Stopped => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                _ => return,
            }
        }
    }

    fn uninstall() -> anyhow::Result<()> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
        let service =
            match manager.open_service(SERVICE_NAME, ServiceAccess::STOP | ServiceAccess::DELETE) {
                Ok(s) => s,
                // Not installed: uninstall is idempotent.
                Err(_) => {
                    println!("agent.d network broker is not installed; nothing to remove.");
                    return Ok(());
                }
            };
        let _ = service.stop();
        service.delete()?;
        // Remove the ancestor metadata/traverse ACEs stamped at install.
        let _ = agentd_shell::sandbox::revoke_metadata_traversal();
        // Remove the NPFS pipe-root create ACEs stamped at install.
        let _ = agentd_shell::sandbox::revoke_pipe_namespace();
        println!("agent.d network broker removed.");
        Ok(())
    }
}
