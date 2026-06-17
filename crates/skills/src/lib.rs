//! Skills (a.k.a. capabilities).
//!
//! A **skill** is a reusable behavior bundle: a system-prompt fragment plus an
//! advisory action allowlist. Runners compose skills by name. The runtime takes
//! the *union* of every referenced skill's system-prompt body and `actions`
//! list, then layers the runner's own `system` and `allowed_actions` on top.
//!
//! Two authoring styles share the same registry:
//!
//! 1. **Markdown files with YAML-ish frontmatter**, parsed via [`parse`] /
//!    [`SkillRegistry::load_dir`] / [`SkillRegistry::load_file`]:
//!
//! ```markdown
//! ---
//! name: reviewer
//! description: code reviewer mode
//! actions:
//!   - git.diff
//!   - git.status
//! ---
//! You are a meticulous code reviewer.
//! ```
//!
//! 2. **Inline Lua** via `agentd.skill{ name=..., system=..., actions={...} }`
//!    (wired up in `agentd-scripting`).
//!
//! The frontmatter parser is intentionally tiny — it handles only the keys we
//! care about (`name`, `description`, `actions`). Pulling a full YAML crate
//! would be overkill for these files and the abandoned status of `serde_yaml`
//! makes it a poor default. If the format grows, replace the parser then.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use walkdir::WalkDir;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillDef {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// System-prompt body (Markdown content after frontmatter). May be empty.
    #[serde(default)]
    pub system: String,
    /// Advisory action allowlist. Runners that pull in this skill inherit these
    /// names as part of their composed `allowed_actions`. The grants file is
    /// still authoritative — declaring an action here does not grant anything.
    #[serde(default)]
    pub actions: Vec<String>,
    /// Source path (for diagnostics). `None` for inline-defined skills.
    #[serde(default)]
    pub source: Option<PathBuf>,
}

#[derive(Debug, Error)]
pub enum SkillError {
    #[error("skill `{name}`: {reason}")]
    Parse { name: String, reason: String },
    #[error("io {0}: {1}")]
    Io(PathBuf, std::io::Error),
}

/// Cheap-to-clone shared registry. Internal `Arc<RwLock<…>>` lets the daemon,
/// the API handler state, and the Lua host all hold handles to the same
/// underlying store.
#[derive(Debug, Default, Clone)]
pub struct SkillRegistry {
    inner: Arc<RwLock<BTreeMap<String, SkillDef>>>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, def: SkillDef) {
        let mut g = self.inner.write().unwrap();
        g.insert(def.name.clone(), def);
    }

    pub fn get(&self, name: &str) -> Option<SkillDef> {
        let g = self.inner.read().unwrap();
        g.get(name).cloned()
    }

    pub fn list(&self) -> Vec<SkillDef> {
        let g = self.inner.read().unwrap();
        g.values().cloned().collect()
    }

    pub fn names(&self) -> Vec<String> {
        let g = self.inner.read().unwrap();
        g.keys().cloned().collect()
    }

    /// Source file path of every skill loaded from disk (inline skills have
    /// none). The daemon's `--watch` loop folds these into its watch set so
    /// editing a skill `.md` triggers a hot reload.
    pub fn sources(&self) -> Vec<PathBuf> {
        let g = self.inner.read().unwrap();
        g.values().filter_map(|d| d.source.clone()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.read().unwrap().is_empty()
    }

    pub fn len(&self) -> usize {
        self.inner.read().unwrap().len()
    }

    /// Load every `*.md` file under `dir` into the registry. Missing dir = ok.
    pub fn load_dir(&self, dir: &Path) -> Result<usize, SkillError> {
        if !dir.exists() {
            tracing::warn!(path = %dir.display(), "skills dir missing");
            return Ok(0);
        }
        let mut n = 0;
        for entry in WalkDir::new(dir).into_iter().filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            self.load_file(path)?;
            n += 1;
        }
        Ok(n)
    }

    /// Parse a single Markdown skill file and insert it.
    pub fn load_file(&self, path: &Path) -> Result<SkillDef, SkillError> {
        let body =
            std::fs::read_to_string(path).map_err(|e| SkillError::Io(path.to_path_buf(), e))?;
        let mut def = parse(&body)?;
        def.source = Some(path.to_path_buf());
        self.insert(def.clone());
        Ok(def)
    }
}

/// Parse a single skill file body (frontmatter + Markdown content).
pub fn parse(body: &str) -> Result<SkillDef, SkillError> {
    let (front, content) = split_frontmatter(body);
    let mut def = SkillDef {
        system: content.trim().to_string(),
        ..Default::default()
    };

    if let Some(front) = front {
        parse_front(front, &mut def)?;
    }

    if def.name.is_empty() {
        return Err(SkillError::Parse {
            name: "<unnamed>".into(),
            reason: "missing `name` in frontmatter".into(),
        });
    }

    Ok(def)
}

fn split_frontmatter(body: &str) -> (Option<&str>, &str) {
    let trimmed = body.trim_start_matches('\u{feff}');
    let trimmed = trimmed.trim_start_matches('\n');
    let Some(rest) = trimmed.strip_prefix("---") else {
        return (None, body);
    };
    let rest = match rest.strip_prefix('\n') {
        Some(r) => r,
        None => return (None, body),
    };
    if let Some(end) = find_end_marker(rest) {
        let front = &rest[..end];
        let after = &rest[end..];
        let after = after.strip_prefix("---").unwrap_or(after);
        let after = after.strip_prefix('\n').unwrap_or(after);
        (Some(front), after)
    } else {
        (None, body)
    }
}

fn find_end_marker(s: &str) -> Option<usize> {
    let mut idx = 0;
    for line in s.split_inclusive('\n') {
        let trimmed = line.trim_end_matches('\n');
        if trimmed == "---" {
            return Some(idx);
        }
        idx += line.len();
    }
    None
}

fn parse_front(front: &str, def: &mut SkillDef) -> Result<(), SkillError> {
    let mut current_array: Option<String> = None;
    for raw in front.lines() {
        let line = raw.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        if line.trim_start().starts_with('#') {
            continue;
        }
        if let Some(rest) = line.trim_start().strip_prefix("- ") {
            let key = current_array
                .as_ref()
                .ok_or_else(|| SkillError::Parse {
                    name: def.name.clone(),
                    reason: format!("stray list item `{rest}` with no key"),
                })?
                .clone();
            push_array(def, &key, rest.trim().trim_matches('"').to_string())?;
            continue;
        }
        let (k, v) = line.split_once(':').ok_or_else(|| SkillError::Parse {
            name: def.name.clone(),
            reason: format!("expected `key: value`, got `{line}`"),
        })?;
        let key = k.trim().to_string();
        let value = v.trim();
        if value.is_empty() {
            current_array = Some(key);
            continue;
        }
        current_array = None;
        set_scalar(def, &key, value.trim_matches('"'))?;
    }
    Ok(())
}

fn set_scalar(def: &mut SkillDef, key: &str, value: &str) -> Result<(), SkillError> {
    match key {
        "name" => def.name = value.to_string(),
        "description" => def.description = Some(value.to_string()),
        "actions" => {
            let inner = value.trim_start_matches('[').trim_end_matches(']');
            def.actions = inner
                .split(',')
                .map(|s| s.trim().trim_matches('"').to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
        other => {
            tracing::debug!(key = %other, "ignoring unknown skill frontmatter key");
        }
    }
    Ok(())
}

fn push_array(def: &mut SkillDef, key: &str, value: String) -> Result<(), SkillError> {
    match key {
        "actions" => def.actions.push(value),
        other => {
            return Err(SkillError::Parse {
                name: def.name.clone(),
                reason: format!("list value for unknown key `{other}`"),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_frontmatter() {
        let src = r#"---
name: reviewer
---
You are a reviewer.
"#;
        let def = parse(src).unwrap();
        assert_eq!(def.name, "reviewer");
        assert_eq!(def.system, "You are a reviewer.");
        assert!(def.actions.is_empty());
    }

    #[test]
    fn parses_actions_block_form() {
        let src = r#"---
name: reviewer
description: code reviewer mode
actions:
  - git.diff
  - git.status
---
body here
"#;
        let def = parse(src).unwrap();
        assert_eq!(def.description.as_deref(), Some("code reviewer mode"));
        assert_eq!(def.actions, vec!["git.diff", "git.status"]);
    }

    #[test]
    fn parses_actions_inline_form() {
        let src = r#"---
name: r
actions: [a, b, "c.d"]
---
"#;
        let def = parse(src).unwrap();
        assert_eq!(def.actions, vec!["a", "b", "c.d"]);
    }

    #[test]
    fn rejects_missing_name() {
        let src = r#"---
description: nope
---
"#;
        assert!(parse(src).is_err());
    }

    #[test]
    fn parses_body_without_frontmatter_fails() {
        let src = "Just a body, no frontmatter.\n";
        assert!(parse(src).is_err());
    }

    #[test]
    fn registry_load_dir_picks_up_md_only() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("reviewer.md"),
            "---\nname: reviewer\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join("skip.txt"), "noise").unwrap();
        let reg = SkillRegistry::new();
        let n = reg.load_dir(tmp.path()).unwrap();
        assert_eq!(n, 1);
        assert!(reg.get("reviewer").is_some());
    }

    #[test]
    fn registry_shares_state_across_clones() {
        let a = SkillRegistry::new();
        let b = a.clone();
        a.insert(SkillDef {
            name: "x".into(),
            ..Default::default()
        });
        assert!(b.get("x").is_some());
    }
}
