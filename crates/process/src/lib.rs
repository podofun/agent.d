//! Host process command construction with Windows script-shim support.

use std::ffi::{OsStr, OsString};
use std::process::Stdio;
use tokio::process::{Child, Command as TokioCommand};

pub struct Command {
    program: String,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    stdin: Option<Stdio>,
    stdout: Option<Stdio>,
    stderr: Option<Stdio>,
}

impl Command {
    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|arg| arg.as_ref().to_owned()));
        self
    }

    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.env
            .push((key.as_ref().to_owned(), value.as_ref().to_owned()));
        self
    }

    pub fn stdin(&mut self, value: Stdio) -> &mut Self {
        self.stdin = Some(value);
        self
    }

    pub fn stdout(&mut self, value: Stdio) -> &mut Self {
        self.stdout = Some(value);
        self
    }

    pub fn stderr(&mut self, value: Stdio) -> &mut Self {
        self.stderr = Some(value);
        self
    }

    pub fn spawn(&mut self) -> std::io::Result<Child> {
        let mut command = platform_command(&self.program, &self.args);
        command.envs(self.env.iter().map(|(key, value)| (key, value)));
        if let Some(value) = self.stdin.take() {
            command.stdin(value);
        }
        if let Some(value) = self.stdout.take() {
            command.stdout(value);
        }
        if let Some(value) = self.stderr.take() {
            command.stderr(value);
        }
        command.spawn()
    }

    pub async fn output(&mut self) -> std::io::Result<std::process::Output> {
        self.stdout = Some(Stdio::piped());
        self.stderr = Some(Stdio::piped());
        self.spawn()?.wait_with_output().await
    }
}

/// Build a command for a native program or a platform launcher shim.
///
/// Windows' `CreateProcessW` does not resolve PowerShell scripts, even though
/// entering the same bare name in PowerShell does. npm installs tools such as
/// Claude Code and Codex with `.ps1` shims, so resolve those on the parent's
/// `PATH` and invoke them explicitly. Native programs remain direct children.
pub fn command(program: &str) -> Command {
    Command {
        program: program.to_owned(),
        args: Vec::new(),
        env: Vec::new(),
        stdin: None,
        stdout: None,
        stderr: None,
    }
}

#[cfg(windows)]
fn platform_command(program: &str, args: &[OsString]) -> TokioCommand {
    let resolved = resolve_windows_program(program);
    let extension = resolved.extension().and_then(|value| value.to_str());
    if extension.is_some_and(|value| value.eq_ignore_ascii_case("ps1")) {
        let mut command = TokioCommand::new("powershell.exe");
        command
            .arg("-NoLogo")
            .arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-ExecutionPolicy")
            .arg("Bypass")
            .arg("-File")
            .arg(&resolved)
            .args(args);
        command
    } else if extension
        .is_some_and(|value| value.eq_ignore_ascii_case("cmd") || value.eq_ignore_ascii_case("bat"))
    {
        let mut invocation = format!("call {}", quote_batch_arg(resolved.as_os_str()));
        for arg in args {
            invocation.push(' ');
            invocation.push_str(&quote_batch_arg(arg));
        }
        let mut command = TokioCommand::new("cmd.exe");
        command
            .arg("/d")
            .arg("/v:off")
            .arg("/s")
            .arg("/c")
            .raw_arg(invocation);
        command
    } else {
        let mut command = TokioCommand::new(resolved);
        command.args(args);
        command
    }
}

#[cfg(windows)]
fn quote_batch_arg(value: &OsStr) -> String {
    let value = value.to_string_lossy().replace('%', "%%");
    let mut quoted = String::from("\"");
    let mut backslashes = 0;
    for character in value.chars() {
        if character == '\\' {
            backslashes += 1;
        } else if character == '"' {
            quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
            quoted.push('"');
            backslashes = 0;
        } else {
            quoted.push_str(&"\\".repeat(backslashes));
            quoted.push(character);
            backslashes = 0;
        }
    }
    quoted.push_str(&"\\".repeat(backslashes * 2));
    quoted.push('"');
    quoted
}

#[cfg(not(windows))]
fn platform_command(program: &str, args: &[OsString]) -> TokioCommand {
    let mut command = TokioCommand::new(program);
    command.args(args);
    command
}

#[cfg(windows)]
fn resolve_windows_program(program: &str) -> std::path::PathBuf {
    use std::path::{Path, PathBuf};

    fn first_existing(path: &Path) -> Option<PathBuf> {
        if path.extension().is_some() {
            return path.is_file().then(|| path.to_path_buf());
        }
        ["exe", "com", "ps1", "cmd", "bat"]
            .into_iter()
            .map(|extension| path.with_extension(extension))
            .find(|candidate| candidate.is_file())
    }

    let path = Path::new(program);
    if path.is_absolute() || path.components().count() > 1 {
        return first_existing(path).unwrap_or_else(|| path.to_path_buf());
    }

    std::env::var_os("PATH")
        .and_then(|value| {
            std::env::split_paths(&value)
                .find_map(|directory| first_existing(&directory.join(program)))
        })
        .unwrap_or_else(|| path.to_path_buf())
}

#[cfg(all(test, windows))]
mod tests {
    use std::ffi::OsStr;
    use std::fs;
    use std::sync::Mutex;

    static PATH_LOCK: Mutex<()> = Mutex::new(());

    #[tokio::test]
    async fn bare_name_resolves_powershell_shim_and_preserves_arguments() {
        let _path_guard = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("agentd-test-shim.ps1");
        fs::write(&script, "[Console]::Out.Write($args[0])").unwrap();

        let old_path = std::env::var_os("PATH");
        let mut paths = vec![directory.path().to_path_buf()];
        if let Some(old_path) = &old_path {
            paths.extend(std::env::split_paths(old_path));
        }
        unsafe { std::env::set_var("PATH", std::env::join_paths(paths).unwrap()) };

        let output = super::command("agentd-test-shim")
            .arg("value with spaces & metacharacters")
            .output()
            .await
            .unwrap();

        if let Some(old_path) = old_path {
            unsafe { std::env::set_var("PATH", old_path) };
        } else {
            unsafe { std::env::remove_var("PATH") };
        }

        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "value with spaces & metacharacters"
        );
    }

    #[tokio::test]
    async fn bare_name_resolves_cmd_shim() {
        let _path_guard = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("agentd-test-batch.cmd"),
            "@echo off\r\n<nul set /p =%~1\r\nexit /b 0",
        )
        .unwrap();

        let old_path = std::env::var_os("PATH");
        let mut paths = vec![directory.path().to_path_buf()];
        if let Some(old_path) = &old_path {
            paths.extend(std::env::split_paths(old_path));
        }
        unsafe { std::env::set_var("PATH", std::env::join_paths(paths).unwrap()) };

        let output = super::command("agentd-test-batch")
            .arg("batch-ok")
            .output()
            .await
            .unwrap();

        if let Some(old_path) = old_path {
            unsafe { std::env::set_var("PATH", old_path) };
        } else {
            unsafe { std::env::remove_var("PATH") };
        }

        assert!(
            output.status.success(),
            "batch launcher failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8(output.stdout).unwrap(), "batch-ok");
    }

    #[test]
    fn batch_arguments_are_quoted_and_percent_expansion_is_escaped() {
        assert_eq!(
            super::quote_batch_arg(OsStr::new("value & 100%")),
            "\"value & 100%%\""
        );
    }
}
