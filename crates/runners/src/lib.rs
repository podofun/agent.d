//! Runners.
//!
//! A **runner** is a named AI worker/identity. It owns a system prompt, a
//! default model + provider, an optional list of `skills` it composes from, and
//! an advisory `allowed_actions` list. Running a runner = composing the final
//! system prompt (skills' bodies + runner.system) and asking the resolved
//! provider for a completion.
//!
//! This crate's `RunnerDef::run()` is single-shot: compose prompt, call
//! `Provider::complete`, return text. The tool-use loop lives one layer up in
//! `agentd-executor::run_runner`: for `ExecutorOwned` providers it dispatches
//! each `ToolCall` through the 5-layer permission engine, appends `Role::Tool`
//! results, and re-prompts until the model returns plain text (16-turn cap).
//! `ProviderOwned` providers (claude CLI + MCP loopback, codex app-server) run
//! their own loop and are called once.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use agentd_ai::{CompletionRequest, Message, ProviderError, ProviderRegistry};
use agentd_skills::SkillRegistry;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Static description of a runner. Loaded from a Lua manifest at startup.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunnerDef {
    pub name: String,
    /// Runner-specific system prompt fragment. Appended after every skill's
    /// fragment in the composed final system prompt.
    #[serde(default)]
    pub system: Option<String>,
    /// `"<provider>/<model_id>"`, e.g. `"anthropic/claude-opus-4-7"`. The
    /// prefix selects which Provider in `ProviderRegistry` runs the request;
    /// the suffix is the upstream model id. Without a `/`, the whole string
    /// is treated as a model id under the registry's default provider.
    #[serde(default)]
    pub model: Option<String>,
    /// Skill names this runner composes from. Order is preserved when joining
    /// system-prompt fragments so authors get deterministic output.
    #[serde(default)]
    pub skills: Vec<String>,
    /// Advisory action allowlist. The grants file is still authoritative — the
    /// engine reads `[runner.<name>].allowed_actions` from `grants.toml` for
    /// the layer-3 check. This field is for inspection/diagnostics + future
    /// tooling that wants to know what a runner *says* it can call.
    #[serde(default)]
    pub allowed_actions: Vec<String>,
}

#[derive(Debug, Error)]
pub enum RunnerError {
    #[error("runner `{0}` not registered")]
    NotFound(String),
    #[error("runner `{name}` references unknown skill `{skill}`")]
    UnknownSkill { name: String, skill: String },
    #[error("runner `{name}`: no provider resolved for model `{model:?}`")]
    NoProvider { name: String, model: Option<String> },
    #[error("provider `{provider}`: {source}")]
    Provider {
        provider: String,
        #[source]
        source: ProviderError,
    },
}

/// Composed view of a runner ready to execute.
#[derive(Debug, Clone, Serialize)]
pub struct RunnerComposition {
    pub name: String,
    pub system: String,
    pub model: Option<String>,
    pub skills: Vec<String>,
    pub allowed_actions: Vec<String>,
}

/// Output of a runner run. Currently a single text reply; the field shape is
/// set up to grow (e.g. `tool_calls`, `usage`) without a breaking rename.
#[derive(Debug, Clone, Serialize)]
pub struct RunnerOutcome {
    pub text: String,
    pub provider: String,
    pub model: Option<String>,
    pub stop_reason: Option<String>,
}

#[derive(Default, Clone)]
pub struct RunnerRegistry {
    inner: Arc<RwLock<BTreeMap<String, RunnerDef>>>,
}

impl RunnerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, def: RunnerDef) {
        let mut g = self.inner.write().unwrap();
        g.insert(def.name.clone(), def);
    }

    pub fn get(&self, name: &str) -> Option<RunnerDef> {
        let g = self.inner.read().unwrap();
        g.get(name).cloned()
    }

    pub fn names(&self) -> Vec<String> {
        let g = self.inner.read().unwrap();
        g.keys().cloned().collect()
    }

    pub fn list(&self) -> Vec<RunnerDef> {
        let g = self.inner.read().unwrap();
        g.values().cloned().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.read().unwrap().is_empty()
    }

    pub fn len(&self) -> usize {
        self.inner.read().unwrap().len()
    }
}

/// Build the composed system prompt + action allowlist for a runner, taking
/// the union of each referenced skill's body/actions and appending the
/// runner's own. Skill order is preserved; duplicates are de-duplicated.
pub fn compose(def: &RunnerDef, skills: &SkillRegistry) -> Result<RunnerComposition, RunnerError> {
    let mut system_parts: Vec<String> = Vec::new();
    let mut actions: Vec<String> = Vec::new();

    for skill_name in &def.skills {
        let skill = skills
            .get(skill_name)
            .ok_or_else(|| RunnerError::UnknownSkill {
                name: def.name.clone(),
                skill: skill_name.clone(),
            })?;
        if !skill.system.trim().is_empty() {
            system_parts.push(skill.system.trim().to_string());
        }
        for a in &skill.actions {
            if !actions.contains(a) {
                actions.push(a.clone());
            }
        }
    }
    if let Some(extra) = &def.system {
        let trimmed = extra.trim();
        if !trimmed.is_empty() {
            system_parts.push(trimmed.to_string());
        }
    }
    for a in &def.allowed_actions {
        if !actions.contains(a) {
            actions.push(a.clone());
        }
    }

    Ok(RunnerComposition {
        name: def.name.clone(),
        system: system_parts.join("\n\n"),
        model: def.model.clone(),
        skills: def.skills.clone(),
        allowed_actions: actions,
    })
}

/// Execute a runner against a user prompt. **Single-shot**: composes the
/// system prompt, dispatches one `Provider::complete` call, returns the text.
/// Tool dispatch loop is future work — see crate-level docs.
pub async fn run(
    runners: &RunnerRegistry,
    skills: &SkillRegistry,
    providers: &ProviderRegistry,
    runner_name: &str,
    prompt: String,
) -> Result<RunnerOutcome, RunnerError> {
    let def = runners
        .get(runner_name)
        .ok_or_else(|| RunnerError::NotFound(runner_name.to_string()))?;
    let composition = compose(&def, skills)?;

    // Resolve provider + bare model id from the composed "<provider>/<model>"
    // string. If no slash, falls back to the registry's default provider.
    let (provider_name, provider, model_id) = match composition.model.as_deref() {
        Some(m) => providers
            .resolve_for_model(m)
            .ok_or_else(|| RunnerError::NoProvider {
                name: def.name.clone(),
                model: composition.model.clone(),
            })?,
        None => {
            let (name, p) = providers
                .resolve(None)
                .ok_or_else(|| RunnerError::NoProvider {
                    name: def.name.clone(),
                    model: None,
                })?;
            (name, p, String::new())
        }
    };

    let mut req = CompletionRequest::default();
    if !composition.system.is_empty() {
        req.system = Some(composition.system.clone());
    }
    if !model_id.is_empty() {
        req.model = Some(model_id);
    }
    req.messages = vec![Message::user(prompt)];

    let resp = provider
        .complete(req)
        .await
        .map_err(|e| RunnerError::Provider {
            provider: provider_name.clone(),
            source: e,
        })?;

    Ok(RunnerOutcome {
        text: resp.text,
        provider: provider_name,
        model: resp.model,
        stop_reason: resp.stop_reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentd_ai::MockProvider;
    use agentd_skills::SkillDef;

    fn skill(name: &str, system: &str, actions: &[&str]) -> SkillDef {
        SkillDef {
            name: name.to_string(),
            system: system.to_string(),
            actions: actions.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn compose_unions_skills_and_runner() {
        let skills = SkillRegistry::new();
        skills.insert(skill("reviewer", "Be terse.", &["git.diff"]));
        skills.insert(skill("debugger", "Find bugs.", &["git.diff", "git.log"]));

        let def = RunnerDef {
            name: "r".into(),
            system: Some("Always cite line numbers.".into()),
            skills: vec!["reviewer".into(), "debugger".into()],
            allowed_actions: vec!["github.comment_pr".into()],
            ..Default::default()
        };
        let c = compose(&def, &skills).unwrap();
        assert!(c.system.contains("Be terse."));
        assert!(c.system.contains("Find bugs."));
        assert!(c.system.contains("Always cite line numbers."));
        // Order preserved, de-duped.
        assert_eq!(
            c.allowed_actions,
            vec!["git.diff", "git.log", "github.comment_pr"]
        );
    }

    #[test]
    fn compose_rejects_unknown_skill() {
        let skills = SkillRegistry::new();
        let def = RunnerDef {
            name: "r".into(),
            skills: vec!["nope".into()],
            ..Default::default()
        };
        assert!(matches!(
            compose(&def, &skills),
            Err(RunnerError::UnknownSkill { .. })
        ));
    }

    #[tokio::test]
    async fn run_uses_mock_provider_via_model_prefix() {
        let runners = RunnerRegistry::new();
        runners.insert(RunnerDef {
            name: "echo".into(),
            system: Some("Echo me.".into()),
            model: Some("mock/foo".into()),
            ..Default::default()
        });
        let skills = SkillRegistry::new();
        let mut providers = ProviderRegistry::new();
        providers.insert("mock", Arc::new(MockProvider::new().with_reply("hi")));

        let outcome = run(&runners, &skills, &providers, "echo", "hello".into())
            .await
            .unwrap();
        assert_eq!(outcome.text, "hi");
        assert_eq!(outcome.provider, "mock");
    }

    #[tokio::test]
    async fn run_uses_default_provider_when_model_has_no_prefix() {
        let runners = RunnerRegistry::new();
        runners.insert(RunnerDef {
            name: "echo".into(),
            model: Some("just-a-model-id".into()),
            ..Default::default()
        });
        let skills = SkillRegistry::new();
        let mut providers = ProviderRegistry::new();
        providers.insert("mock", Arc::new(MockProvider::new().with_reply("yo")));
        providers.set_default("mock");

        let outcome = run(&runners, &skills, &providers, "echo", "hi".into())
            .await
            .unwrap();
        assert_eq!(outcome.text, "yo");
        assert_eq!(outcome.provider, "mock");
    }

    #[tokio::test]
    async fn run_returns_not_found_for_unknown() {
        let runners = RunnerRegistry::new();
        let skills = SkillRegistry::new();
        let providers = ProviderRegistry::new();
        let err = run(&runners, &skills, &providers, "nope", "x".into())
            .await
            .unwrap_err();
        assert!(matches!(err, RunnerError::NotFound(_)));
    }
}
