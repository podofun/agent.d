//! Test-only entry point for the in-netns network supervisor.
//!
//! In production the daemon re-execs ITSELF and calls
//! `agentd_shell::sandbox::run_netns_supervisor_if_requested()` first thing in
//! `main`, so no separate binary ships. This tiny binary exists only so the
//! `agentd-shell` integration tests have a stable supervisor target to point
//! `AGENTD_NETNS_SUPERVISOR_BIN` at.

fn main() {
    agentd_shell::sandbox::run_netns_supervisor_if_requested();
    // Reached only if the supervisor env was not set (not in supervisor mode).
    eprintln!("agentd-netns-supervisor: not invoked as a supervisor");
    std::process::exit(1);
}
