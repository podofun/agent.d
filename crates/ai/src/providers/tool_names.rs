//! Tool names on the wire. Providers accept `^[A-Za-z0-9_-]{1,64}$`, while
//! agent.d actions are dotted (`git.status`). Names that already fit pass
//! through unchanged; the rest are replaced by a readable, hashed alias that
//! [`ToolNames::canonical`] maps back to the registered action name.

use std::collections::HashMap;

use sha2::{Digest, Sha256};

use crate::types::CompletionRequest;

/// Alias table for one request: wire alias to registered action name.
#[derive(Default)]
pub(super) struct ToolNames {
    canonical_by_wire: HashMap<String, String>,
}

impl ToolNames {
    pub(super) fn new(req: &CompletionRequest) -> Self {
        Self {
            canonical_by_wire: req
                .tools
                .iter()
                .map(|tool| (wire_tool_name(&tool.name), tool.name.clone()))
                .collect(),
        }
    }

    /// The registered name behind a wire alias. Unknown aliases come back
    /// unchanged so the executor can report what the model actually asked for.
    pub(super) fn canonical<'a>(&'a self, wire_name: &'a str) -> &'a str {
        self.canonical_by_wire
            .get(wire_name)
            .map(String::as_str)
            .unwrap_or(wire_name)
    }
}

pub(super) fn wire_tool_name(name: &str) -> String {
    if is_wire_safe(name) {
        return name.to_owned();
    }
    let readable: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character
            } else {
                '_'
            }
        })
        .take(47)
        .collect();
    let digest = Sha256::digest(name.as_bytes());
    let hash: String = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{readable}_{hash}")
}

fn is_wire_safe(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ToolDef;

    fn request(names: &[&str]) -> CompletionRequest {
        CompletionRequest {
            tools: names
                .iter()
                .map(|name| ToolDef {
                    name: (*name).into(),
                    description: None,
                    input_schema: serde_json::Value::Null,
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn safe_names_pass_through_untouched() {
        assert_eq!(wire_tool_name("git_status"), "git_status");
        assert_eq!(wire_tool_name("read-file"), "read-file");
    }

    #[test]
    fn unsafe_names_become_stable_safe_distinct_aliases() {
        let dotted = wire_tool_name("demo.echo");
        assert_eq!(dotted, wire_tool_name("demo.echo"));
        assert!(dotted.starts_with("demo_echo_"));
        assert_ne!(dotted, wire_tool_name("demo_echo"));
        assert_ne!(dotted, wire_tool_name("demo-echo"));
        let long = wire_tool_name(&"å.".repeat(100));
        for alias in [&dotted, &long] {
            assert!(alias.len() <= 64, "{alias}");
            assert!(
                alias
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '_')
            );
        }
        assert_eq!(wire_tool_name(&"x".repeat(70)).len(), 64);
    }

    #[test]
    fn canonical_restores_known_aliases_and_keeps_unknown_ones() {
        let names = ToolNames::new(&request(&["notes.lookup", "plain"]));
        assert_eq!(
            names.canonical(&wire_tool_name("notes.lookup")),
            "notes.lookup"
        );
        assert_eq!(names.canonical("plain"), "plain");
        assert_eq!(names.canonical("made_up"), "made_up");
    }
}
