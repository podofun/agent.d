//! agentctl — console client for the agentd runtime.
//!
//! Transport: WebSocket. Every command opens a single connection to `/ws`,
//! sends one JSON envelope per call, and exits when the response arrives.
//! Liveness probe (`agentctl health`) still uses plain HTTP `/health` since
//! that endpoint exists for orchestrators that won't speak WebSocket.
//!
//! Layout: [`cli`] is the clap surface; [`commands`] handles the public `/ws`
//! plane plus the local `types`/`trace` helpers; [`grants`] drives the
//! privileged `/control` plane; [`secret`]/[`packages`] are local-only; [`ws`]
//! and [`render`] are the shared transport and output plumbing.

mod cli;
mod commands;
mod grants;
mod packages;
mod render;
mod secret;
mod ws;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Cmd, GrantsCmd};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let (url, timeout) = (&cli.url, cli.timeout);
    match cli.cmd {
        Cmd::Health => commands::cmd_health(url, timeout).await,
        Cmd::Tools => commands::cmd_tools(url, timeout).await,
        Cmd::Call {
            action,
            json,
            data,
            result_only,
            compact,
        } => commands::cmd_call(url, timeout, action, json, data, result_only, compact).await,
        Cmd::Runner { cmd } => commands::cmd_runner(url, timeout, cmd).await,
        Cmd::Skills { cmd } => commands::cmd_skills(url, timeout, cmd).await,
        Cmd::Services { cmd } => commands::cmd_services(url, timeout, cmd).await,
        Cmd::Grants { cmd } => match cmd {
            GrantsCmd::Listen => grants::cmd_grants_listen(url, timeout).await,
        },
        Cmd::Packages { cmd } => packages::run_packages(cmd),
        Cmd::Secret { cmd } => secret::run_secrets(cmd),
        Cmd::Types { dir } => commands::cmd_types(url, timeout, dir).await,
        Cmd::Trace {
            file,
            follow,
            lines,
        } => commands::cmd_trace(file, follow, lines).await,
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::{Cli, Cmd, PkgCmd, SecretCmd};
    use crate::render::render_error;
    use crate::secret::obfuscate;
    use clap::Parser;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("agentctl").chain(args.iter().copied())).unwrap()
    }

    #[test]
    fn noun_aliases_parse() {
        for cmd in [
            "runner", "runners", "skill", "skills", "service", "services", "svc", "package",
            "packages", "pkg",
        ] {
            parse(&[cmd, "ls"]);
        }
    }

    #[test]
    fn secret_commands_parse() {
        assert!(matches!(
            parse(&["secret", "set", "k", "v"]).cmd,
            Cmd::Secret {
                cmd: SecretCmd::Set { .. }
            }
        ));
        // Value may come from stdin.
        assert!(matches!(
            parse(&["secret", "set", "k"]).cmd,
            Cmd::Secret {
                cmd: SecretCmd::Set { value: None, .. }
            }
        ));
        assert!(matches!(
            parse(&["secrets", "unset", "k"]).cmd,
            Cmd::Secret {
                cmd: SecretCmd::Unset { .. }
            }
        ));
        assert!(matches!(
            parse(&["secret", "rm", "k"]).cmd,
            Cmd::Secret {
                cmd: SecretCmd::Unset { .. }
            }
        ));
        assert!(matches!(
            parse(&["secret", "peek", "k"]).cmd,
            Cmd::Secret {
                cmd: SecretCmd::Peek { .. }
            }
        ));
    }

    #[test]
    fn obfuscate_previews_without_revealing() {
        // Long keys: 4-char head, 2-char tail, bounded mask, length shown.
        let s = obfuscate("sk-ant-api03-abcdefghijklmnop");
        assert!(s.starts_with("sk-a"), "{s}");
        assert!(s.ends_with("op (29 chars)"), "{s}");
        assert!(!s.contains("api03"), "middle must be masked: {s}");
        // Short values: fully masked.
        assert_eq!(obfuscate("abc"), "*** (3 chars)");
        // Exactly at the boundary.
        assert_eq!(obfuscate("12345678"), "1234**78 (8 chars)");
    }

    #[test]
    fn pkg_rm_alias_parses() {
        let c = parse(&["pkg", "rm", "foo"]);
        assert!(matches!(
            c.cmd,
            Cmd::Packages {
                cmd: PkgCmd::Remove { .. }
            }
        ));
    }

    #[test]
    fn call_short_flags_parse() {
        let c = parse(&["call", "act", "-j", "{}", "-r"]);
        match c.cmd {
            Cmd::Call {
                json, result_only, ..
            } => {
                assert_eq!(json.as_deref(), Some("{}"));
                assert!(result_only);
            }
            _ => panic!("wrong cmd"),
        }
    }

    #[test]
    fn global_short_url_parses() {
        let c = parse(&["-u", "http://x:1", "health"]);
        assert_eq!(c.url, "http://x:1");
    }

    #[test]
    fn render_error_full() {
        let s = render_error(
            "no_provider",
            "could not resolve a provider for model `m`",
            Some("You can configure new providers in your `config.toml`"),
            &[
                "helpers.lua:313  in structured".to_string(),
                "init.lua:53".to_string(),
            ],
            false,
        );
        assert_eq!(
            s,
            "Error: Could not resolve a provider for model `m`  (no_provider)\nTip: You can configure new providers in your `config.toml`\n\nStack trace:\n  helpers.lua:313  in structured\n  init.lua:53"
        );
    }

    #[test]
    fn render_error_minimal() {
        let s = render_error("denied", "denied at layer 3", None, &[], false);
        assert_eq!(s, "Error: Denied at layer 3  (denied)");
    }
}
