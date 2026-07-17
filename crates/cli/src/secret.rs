//! Provider API keys. Talks straight to the OS keyring (the same `agentd`
//! service the daemon reads), so there is no daemon round-trip and a running
//! daemon picks up changes on its next provider call.

use anyhow::{Context, Result, anyhow};

use crate::cli::SecretCmd;

pub(crate) fn run_secrets(cmd: SecretCmd) -> Result<()> {
    use agentd_secrets::{KeyringStore, SecretStore};
    let store = KeyringStore::default_service();
    match cmd {
        SecretCmd::Set { name, value } => {
            let value = match value {
                Some(v) => v,
                None => {
                    // Piped or typed on stdin; trailing newline stripped.
                    let mut s = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)
                        .context("could not read the secret value from stdin")?;
                    s.trim_end_matches(['\r', '\n']).to_string()
                }
            };
            if value.is_empty() {
                return Err(anyhow!(
                    "the secret value is empty — pass it as an argument or pipe it on stdin"
                ));
            }
            store.set(&name, &value)?;
            println!("stored `{name}` — available to the daemon immediately");
        }
        SecretCmd::Unset { name } => {
            store.delete(&name)?;
            println!("removed `{name}`");
        }
        SecretCmd::Peek { name } => {
            let v = store.get(&name)?;
            println!("{}", obfuscate(&v));
        }
    }
    Ok(())
}

/// Half-obfuscated preview: enough to recognize a key, not enough to use
/// it. Short values are fully masked.
pub(crate) fn obfuscate(v: &str) -> String {
    let n = v.chars().count();
    if n < 8 {
        return format!("{} ({n} chars)", "*".repeat(n));
    }
    let head: String = v.chars().take(4).collect();
    let tail: String = v.chars().skip(n - 2).collect();
    format!("{head}{}{tail} ({n} chars)", "*".repeat((n - 6).min(12)))
}
