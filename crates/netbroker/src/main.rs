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
        let access = ServiceAccess::CHANGE_CONFIG | ServiceAccess::START;
        let service = match manager.create_service(&info, access) {
            Ok(s) => s,
            // Already installed: just (re)start it.
            Err(_) => manager.open_service(SERVICE_NAME, access)?,
        };
        service.start::<OsString>(&[])?;
        println!("agent.d network broker installed and running.");
        Ok(())
    }

    fn uninstall() -> anyhow::Result<()> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
        let service =
            manager.open_service(SERVICE_NAME, ServiceAccess::STOP | ServiceAccess::DELETE)?;
        let _ = service.stop();
        service.delete()?;
        println!("agent.d network broker removed.");
        Ok(())
    }
}
