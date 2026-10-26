//! What the executor remembers about each run while it is in flight: the
//! files and hosts approved for the rest of the run, the secrets the run has
//! read (so questions never show them), and questions already being asked.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use agentd_types::Verdict;

/// Longest string kept in a question's `detail`, in characters.
const MAX_DETAIL_STRING: usize = 2000;
/// Most items kept from one array or object in a question's `detail`.
const MAX_DETAIL_ITEMS: usize = 64;

/// Secrets read in a run, as `(name, value)`. A nested run shares its
/// parent's list, so a secret passed into `ctx.run` stays hidden on both
/// sides.
type Secrets = Arc<Mutex<Vec<(String, String)>>>;

#[derive(Default)]
struct RunRecord {
    /// Exact `fs.read:`, `fs.write:` and `net:` permissions approved for
    /// the rest of the run. They never join a handler's grants, so they
    /// never widen what a shell child's sandbox may reach.
    allows: HashSet<String>,
    secrets: Secrets,
}

/// Run records keyed by run id. Created when a run starts, removed when it
/// ends. A call carrying a run id no longer here is treated as a call with no
/// run.
#[derive(Default)]
pub(crate) struct Runs {
    records: Mutex<HashMap<String, RunRecord>>,
    /// Questions being asked, keyed by run, kind, and the unredacted request.
    /// Identical questions in flight share one answer.
    asking: Mutex<HashMap<String, Arc<tokio::sync::OnceCell<Verdict>>>>,
}

/// Removes a run's record when the run ends or is cancelled.
pub(crate) struct RunGuard {
    runs: Arc<Runs>,
    id: String,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        self.runs.records.lock().unwrap().remove(&self.id);
    }
}

impl Runs {
    /// Start a run's record. A nested run (`parent` still in flight) starts
    /// with no allows of its own but shares its parent's secrets.
    pub(crate) fn start(self: &Arc<Self>, id: &str, parent: Option<&str>) -> RunGuard {
        let mut records = self.records.lock().unwrap();
        let secrets = parent
            .and_then(|p| records.get(p))
            .map(|r| r.secrets.clone())
            .unwrap_or_default();
        records.insert(
            id.to_string(),
            RunRecord {
                allows: HashSet::new(),
                secrets,
            },
        );
        RunGuard {
            runs: self.clone(),
            id: id.to_string(),
        }
    }

    pub(crate) fn allows(&self, run_id: &str, permission: &str) -> bool {
        self.records
            .lock()
            .unwrap()
            .get(run_id)
            .is_some_and(|r| r.allows.contains(permission))
    }

    /// Remember an allow of a file or host for the rest of the run. Other
    /// permissions cover only the request that asked.
    pub(crate) fn allow(&self, run_id: &str, permission: &str) {
        if !covers_rest_of_run(permission) {
            return;
        }
        if let Some(r) = self.records.lock().unwrap().get_mut(run_id) {
            r.allows.insert(permission.to_string());
        }
    }

    pub(crate) fn record_secret(&self, run_id: &str, name: &str, value: &str) {
        if value.is_empty() {
            return;
        }
        if let Some(r) = self.records.lock().unwrap().get(run_id) {
            let mut secrets = r.secrets.lock().unwrap();
            if !secrets.iter().any(|(n, v)| n == name && v == value) {
                secrets.push((name.to_string(), value.to_string()));
            }
        }
    }

    /// `detail` with every secret the run read replaced by its name, and long
    /// strings and lists capped.
    pub(crate) fn redact(
        &self,
        run_id: Option<&str>,
        detail: &serde_json::Value,
    ) -> serde_json::Value {
        let secrets = run_id
            .and_then(|id| {
                self.records
                    .lock()
                    .unwrap()
                    .get(id)
                    .map(|r| r.secrets.lock().unwrap().clone())
            })
            .unwrap_or_default();
        redact_value(detail, &secrets)
    }

    /// Ask a question once per run, kind, and unredacted request: identical
    /// questions in flight wait for the first one's answer. Without a run
    /// every question is asked on its own.
    pub(crate) async fn ask<F>(&self, key: Option<String>, ask: F) -> Verdict
    where
        F: std::future::Future<Output = Verdict>,
    {
        let Some(key) = key else {
            return ask.await;
        };
        let cell = self
            .asking
            .lock()
            .unwrap()
            .entry(key.clone())
            .or_default()
            .clone();
        let verdict = *cell.get_or_init(|| ask).await;
        let mut asking = self.asking.lock().unwrap();
        if asking.get(&key).is_some_and(|c| Arc::ptr_eq(c, &cell)) {
            asking.remove(&key);
        }
        verdict
    }
}

/// Files and hosts stay allowed for the rest of the run, so paging through
/// one host or reading one file again is not asked again. A shell command is
/// asked each time, since what it does depends on files the run may change.
fn covers_rest_of_run(permission: &str) -> bool {
    ["fs.read:", "fs.write:", "net:"]
        .iter()
        .any(|p| permission.starts_with(p))
}

fn redact_value(v: &serde_json::Value, secrets: &[(String, String)]) -> serde_json::Value {
    use serde_json::Value;
    match v {
        Value::String(s) => Value::String(redact_str(s, secrets)),
        Value::Array(items) => {
            let mut out: Vec<Value> = items
                .iter()
                .take(MAX_DETAIL_ITEMS)
                .map(|i| redact_value(i, secrets))
                .collect();
            if items.len() > MAX_DETAIL_ITEMS {
                out.push(Value::String(format!(
                    "... {} more",
                    items.len() - MAX_DETAIL_ITEMS
                )));
            }
            Value::Array(out)
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .take(MAX_DETAIL_ITEMS)
                .map(|(k, v)| (redact_str(k, secrets), redact_value(v, secrets)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn redact_str(s: &str, secrets: &[(String, String)]) -> String {
    let mut out = s.to_string();
    for (name, value) in secrets {
        if out.contains(value.as_str()) {
            out = out.replace(value.as_str(), &format!("<secret {name}>"));
        }
    }
    if out.chars().count() > MAX_DETAIL_STRING {
        out = out.chars().take(MAX_DETAIL_STRING).collect::<String>() + "...";
    }
    out
}

/// Programs that run whatever they are given. "Allow forever" on one grants
/// arbitrary execution, so approvers warn before writing it.
pub(crate) fn interpreter_warning(permission: &str) -> Option<String> {
    const INTERPRETERS: &[&str] = &[
        "sh",
        "bash",
        "zsh",
        "fish",
        "dash",
        "ksh",
        "env",
        "sudo",
        "xargs",
        "python",
        "python3",
        "node",
        "deno",
        "bun",
        "ruby",
        "perl",
        "php",
        "lua",
        "pwsh",
        "powershell",
        "cmd",
        "npx",
        "uv",
        "uvx",
    ];
    let program = permission.strip_prefix("shell.exec:")?;
    let base = std::path::Path::new(program)
        .file_name()?
        .to_string_lossy()
        .to_lowercase();
    let base = base.strip_suffix(".exe").unwrap_or(&base);
    let is_interpreter = INTERPRETERS.contains(&base)
        || base
            .strip_prefix("python")
            .is_some_and(|v| v.chars().all(|c| c.is_ascii_digit() || c == '.'));
    is_interpreter.then(|| {
        format!("`{program}` runs whatever it is given, so allowing it forever allows any command")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn secrets_are_replaced_by_their_names_and_nested_runs_share_them() {
        let runs = Arc::new(Runs::default());
        let _parent = runs.start("run-1", None);
        let _child = runs.start("run-2", Some("run-1"));
        runs.record_secret("run-2", "gh_token", "ghp_abc123");
        let detail = json!({ "argv": ["curl", "-H", "Bearer ghp_abc123"] });
        let want = json!({ "argv": ["curl", "-H", "Bearer <secret gh_token>"] });
        assert_eq!(runs.redact(Some("run-1"), &detail), want);
        assert_eq!(runs.redact(Some("run-2"), &detail), want);
        assert_eq!(runs.redact(None, &detail), detail);
    }

    #[test]
    fn long_values_are_capped() {
        let runs = Runs::default();
        let long = "x".repeat(MAX_DETAIL_STRING + 10);
        let out = runs.redact(None, &json!(long));
        assert_eq!(out.as_str().unwrap().chars().count(), MAX_DETAIL_STRING + 3);
        let many: Vec<u32> = (0..100).collect();
        let out = runs.redact(None, &json!(many));
        assert_eq!(out.as_array().unwrap().len(), MAX_DETAIL_ITEMS + 1);
    }

    #[test]
    fn only_files_and_hosts_cover_the_rest_of_the_run() {
        let runs = Arc::new(Runs::default());
        let guard = runs.start("run-1", None);
        runs.allow("run-1", "fs.read:/proj/a.txt");
        runs.allow("run-1", "net:api.example.com");
        runs.allow("run-1", "shell.exec:cargo");
        assert!(runs.allows("run-1", "fs.read:/proj/a.txt"));
        assert!(runs.allows("run-1", "net:api.example.com"));
        assert!(!runs.allows("run-1", "shell.exec:cargo"));
        assert!(!runs.allows("run-1", "fs.read:/proj/b.txt"));
        drop(guard);
        assert!(
            !runs.allows("run-1", "fs.read:/proj/a.txt"),
            "gone with the run"
        );
    }

    #[tokio::test]
    async fn identical_questions_in_flight_are_asked_once() {
        let runs = Arc::new(Runs::default());
        let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let ask = |asked: Arc<std::sync::atomic::AtomicUsize>| async move {
            asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            Verdict::AllowOnce
        };
        let key = || Some("run-1\0inline\0shell.exec:cargo".to_string());
        let (a, b) = tokio::join!(
            runs.ask(key(), ask(asked.clone())),
            runs.ask(key(), ask(asked.clone()))
        );
        assert_eq!((a, b), (Verdict::AllowOnce, Verdict::AllowOnce));
        assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 1);
        // Once answered, the next identical question is asked again.
        runs.ask(key(), ask(asked.clone())).await;
        assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn interpreters_are_flagged() {
        for p in [
            "shell.exec:sh",
            "shell.exec:/usr/bin/python3.12",
            "shell.exec:node.exe",
        ] {
            assert!(interpreter_warning(p).is_some(), "{p}");
        }
        for p in ["shell.exec:cargo", "shell.exec:git", "fs.read:/bin/sh"] {
            assert!(interpreter_warning(p).is_none(), "{p}");
        }
    }
}
