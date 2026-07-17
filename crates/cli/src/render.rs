//! Turning daemon responses into what the user sees on stdout/stderr.

use anyhow::Result;
use serde_json::Value;

use crate::ws::WsResponse;

/// Print a response body, or render its structured error to stderr and exit 1.
/// `compact` selects one-line JSON (machine-friendly) over pretty output.
pub(crate) fn print_result(resp: &WsResponse, compact: bool) -> Result<()> {
    if resp.ok {
        let v = resp.result.clone().unwrap_or(Value::Null);
        if compact {
            println!("{}", serde_json::to_string(&v)?);
        } else {
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        Ok(())
    } else {
        let code = resp.code.as_deref().unwrap_or("error");
        let msg = resp.error.as_deref().unwrap_or("(no message)");
        if compact {
            // Machine-friendly one-liner keeps the full structured error.
            let v = serde_json::json!({
                "code": code, "error": msg, "tip": resp.tip, "trace": resp.trace,
            });
            eprintln!("{}", serde_json::to_string(&v)?);
        } else {
            use std::io::IsTerminal;
            let dim = std::io::stderr().is_terminal();
            let trace = resp.trace.as_deref().unwrap_or(&[]);
            eprintln!(
                "{}",
                render_error(code, msg, resp.tip.as_deref(), trace, dim)
            );
        }
        std::process::exit(1);
    }
}

/// Human error rendering:
///
/// ```text
/// Error: Could not resolve a provider for model `m`  (no_provider)
/// Tip: You can configure new providers in your `config.toml`
///
/// Stack trace:
///   helpers.lua:313  in structured
///   init.lua:53
/// ```
///
/// The code suffix goes ANSI-dim when `dim` (stderr is a tty). Tip and trace
/// sections only appear when present.
pub(crate) fn render_error(
    code: &str,
    msg: &str,
    tip: Option<&str>,
    trace: &[String],
    dim: bool,
) -> String {
    let mut msg = msg.to_string();
    if let Some(first) = msg.get(..1) {
        let up = first.to_uppercase();
        msg.replace_range(..1, &up);
    }
    let code_suffix = if dim {
        format!("  \x1b[2m({code})\x1b[0m")
    } else {
        format!("  ({code})")
    };
    let mut out = format!("Error: {msg}{code_suffix}");
    if let Some(tip) = tip {
        out.push_str(&format!("\nTip: {tip}"));
    }
    if !trace.is_empty() {
        out.push_str("\n\nStack trace:");
        for frame in trace {
            out.push_str(&format!("\n  {frame}"));
        }
    }
    out
}
