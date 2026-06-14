//! Provider registry. A shared lookup table of named `Provider` impls so the
//! daemon, the Lua host, and runner execution all agree on which `claude-cli`
//! / `mock` / future-`claude-api` instance to call.
//!
//! Built once at daemon startup, wrapped in `Arc`, handed to every subsystem
//! that needs to dispatch by name.

use std::collections::HashMap;
use std::sync::Arc;

use crate::types::Provider;

#[derive(Default, Clone)]
pub struct ProviderRegistry {
    providers: HashMap<String, Arc<dyn Provider>>,
    default: Option<String>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, name: impl Into<String>, provider: Arc<dyn Provider>) {
        self.providers.insert(name.into(), provider);
    }

    pub fn set_default(&mut self, name: impl Into<String>) {
        self.default = Some(name.into());
    }

    pub fn default_name(&self) -> Option<&str> {
        self.default.as_deref()
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Provider>> {
        self.providers.get(name).cloned()
    }

    pub fn resolve(&self, requested: Option<&str>) -> Option<(String, Arc<dyn Provider>)> {
        let name = requested
            .map(|s| s.to_string())
            .or_else(|| self.default.clone())?;
        let p = self.providers.get(&name).cloned()?;
        Some((name, p))
    }

    /// Resolve from a `"<provider>/<model>"` string. Returns the provider
    /// instance plus the model id (everything after the first `/`). If the
    /// string has no `/`, the whole thing is treated as a model id and the
    /// default provider is used.
    pub fn resolve_for_model(&self, model: &str) -> Option<(String, Arc<dyn Provider>, String)> {
        let (provider_name, model_id) = parse_model(model);
        let provider_name = provider_name
            .map(|s| s.to_string())
            .or_else(|| self.default.clone())?;
        let p = self.providers.get(&provider_name).cloned()?;
        Some((provider_name, p, model_id.to_string()))
    }

    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.providers.keys().cloned().collect();
        v.sort();
        v
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}

/// Split a `"<provider>/<model_id>"` string. Returns `(provider_name, model_id)`.
/// If no `/`, returns `(None, full)` — caller can fall back to the registry
/// default. The first `/` is the split; provider names cannot contain `/`,
/// model ids can (some upstreams use slashes in their model strings).
pub fn parse_model(s: &str) -> (Option<&str>, &str) {
    match s.split_once('/') {
        Some((p, rest)) if !p.is_empty() => (Some(p), rest),
        _ => (None, s),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MockProvider;

    #[test]
    fn resolve_uses_explicit_name_first() {
        let mut r = ProviderRegistry::new();
        r.insert("mock", Arc::new(MockProvider::new()));
        r.set_default("mock");
        let (name, _) = r.resolve(Some("mock")).unwrap();
        assert_eq!(name, "mock");
    }

    #[test]
    fn resolve_falls_back_to_default() {
        let mut r = ProviderRegistry::new();
        r.insert("mock", Arc::new(MockProvider::new()));
        r.set_default("mock");
        let (name, _) = r.resolve(None).unwrap();
        assert_eq!(name, "mock");
    }

    #[test]
    fn resolve_unknown_yields_none() {
        let r = ProviderRegistry::new();
        assert!(r.resolve(Some("nope")).is_none());
        assert!(r.resolve(None).is_none());
    }

    #[test]
    fn parse_model_with_provider_prefix() {
        let (p, m) = parse_model("anthropic/claude-opus-4-7");
        assert_eq!(p, Some("anthropic"));
        assert_eq!(m, "claude-opus-4-7");
    }

    #[test]
    fn parse_model_keeps_inner_slashes() {
        let (p, m) = parse_model("openai/gpt-4/mini");
        assert_eq!(p, Some("openai"));
        assert_eq!(m, "gpt-4/mini");
    }

    #[test]
    fn parse_model_no_provider_returns_full_as_model() {
        let (p, m) = parse_model("claude-opus-4-7");
        assert!(p.is_none());
        assert_eq!(m, "claude-opus-4-7");
    }

    #[test]
    fn resolve_for_model_with_prefix() {
        let mut r = ProviderRegistry::new();
        r.insert("anthropic", Arc::new(MockProvider::new()));
        let (name, _, model) = r.resolve_for_model("anthropic/claude-opus-4-7").unwrap();
        assert_eq!(name, "anthropic");
        assert_eq!(model, "claude-opus-4-7");
    }

    #[test]
    fn resolve_for_model_no_prefix_uses_default() {
        let mut r = ProviderRegistry::new();
        r.insert("anthropic", Arc::new(MockProvider::new()));
        r.set_default("anthropic");
        let (name, _, model) = r.resolve_for_model("claude-opus-4-7").unwrap();
        assert_eq!(name, "anthropic");
        assert_eq!(model, "claude-opus-4-7");
    }
}
