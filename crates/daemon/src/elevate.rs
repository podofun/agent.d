//! Launch a helper elevated through the UAC prompt.
//!
//! The daemon never runs elevated. For the one-time network-sandbox setup it
//! launches the separate broker binary with the `runas` verb, which shows the
//! standard UAC consent dialog; the elevated broker registers itself as the
//! SYSTEM service and the daemon (still Medium integrity) reports the result.

#![cfg(target_os = "windows")]

use std::path::Path;

use anyhow::{Result, anyhow};
use windows::Win32::Foundation::{CloseHandle, GetLastError};
use windows::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
use windows::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
use windows::Win32::UI::WindowsAndMessaging::SW_NORMAL;
use windows::core::PCWSTR;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Run `exe args` elevated and wait for it. `Ok(true)` = elevated run succeeded,
/// `Ok(false)` = the user declined the UAC prompt, `Err` = unexpected failure.
pub fn run_elevated(exe: &Path, args: &str) -> Result<bool> {
    let exe_w = wide(&exe.to_string_lossy());
    let verb = wide("runas");
    let params = wide(args);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(exe_w.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_NORMAL.0,
        ..Default::default()
    };
    unsafe {
        if ShellExecuteExW(&mut info).is_err() {
            // 1223 == ERROR_CANCELLED: the user clicked "No" on the UAC prompt.
            let code = GetLastError().0;
            if code == 1223 {
                return Ok(false);
            }
            return Err(anyhow!("could not request elevation (error {code})"));
        }
        if info.hProcess.is_invalid() {
            return Err(anyhow!("elevation returned no process handle"));
        }
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 0u32;
        let _ = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hProcess);
        Ok(code == 0)
    }
}
