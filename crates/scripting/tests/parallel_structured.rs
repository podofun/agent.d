//! Tests for the two ergonomic helpers added to `helpers.lua`:
//! `parallel` / `parallel_map` (fan-out join over `async`) and
//! `ctx.structured` (guaranteed-shape runner output with reprompt-on-reject).

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use agentd_permissions::{Caller, model::PermissionSet};
use agentd_scripting::LuaHost;
use agentd_secrets::MemoryStore;
use agentd_types::{ActionCall, Registry, RunnerDispatcher};

fn write_init(body: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("init.lua");
    let mut f = std::fs::File::create(&p).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    dir
}

fn host_with(root: &std::path::Path) -> Arc<LuaHost> {
    let host = LuaHost::new().unwrap();
    host.set_root(root);
    host.start_async_runtime(tokio::runtime::Handle::current());
    host.set_secrets(Arc::new(MemoryStore::default()));
    Arc::new(host)
}

async fn call(host: &Arc<LuaHost>, action: &str) -> serde_json::Value {
    let ctx = agentd_types::CallContext {
        caller: Caller::default(),
        effective_grants: PermissionSet::empty(),
        call_chain: vec![action.into()],
        cwd: None,
    };
    host.call(
        ctx,
        ActionCall {
            action: action.into(),
            args: serde_json::Value::Null,
        },
    )
    .await
    .expect("call ok")
    .value
}

#[tokio::test(flavor = "multi_thread")]
async fn parallel_preserves_order_and_overlaps() {
    let dir = write_init(
        r#"
        agentd.tool{ name = "demo" }
        agentd.action{
          name = "demo.par",
          handler = function(_, ctx)
            -- Branches finish in reverse order; results must stay in input order.
            local r = parallel{
              function() sleep(120); return "a" end,
              function() sleep(60);  return "b" end,
              function() sleep(20);  return "c" end,
            }
            return { r = r }
          end,
        }
        "#,
    );
    let host = host_with(dir.path());
    host.load_file(&dir.path().join("init.lua")).unwrap();

    let started = Instant::now();
    let v = call(&host, "demo.par").await;
    let elapsed = started.elapsed().as_millis();

    assert_eq!(v, serde_json::json!({ "r": ["a", "b", "c"] }));
    // Overlap: ~longest branch (120ms), nowhere near the 200ms serial sum.
    assert!(elapsed < 190, "expected overlap, took {elapsed}ms");
}

#[tokio::test(flavor = "multi_thread")]
async fn parallel_limit_serializes() {
    let dir = write_init(
        r#"
        agentd.tool{ name = "demo" }
        agentd.action{
          name = "demo.par_limit",
          handler = function(_, ctx)
            local fns = {}
            for i = 1, 4 do fns[i] = function() sleep(40); return i end end
            return { r = parallel(fns, { limit = 1 }) }
          end,
        }
        "#,
    );
    let host = host_with(dir.path());
    host.load_file(&dir.path().join("init.lua")).unwrap();

    let started = Instant::now();
    let v = call(&host, "demo.par_limit").await;
    let elapsed = started.elapsed().as_millis();

    assert_eq!(v, serde_json::json!({ "r": [1, 2, 3, 4] }));
    // limit=1 forces serial execution: ~4 * 40ms.
    assert!(elapsed >= 150, "limit=1 should serialize, took {elapsed}ms");
}

#[tokio::test(flavor = "multi_thread")]
async fn parallel_settled_collects_errors() {
    let dir = write_init(
        r#"
        agentd.tool{ name = "demo" }
        agentd.action{
          name = "demo.par_settled",
          handler = function(_, ctx)
            local r = parallel({
              function() return 1 end,
              function() error("boom") end,
            }, { settled = true })
            return {
              ok1 = r[1].ok, v1 = r[1].value,
              ok2 = r[2].ok, e2 = r[2].error,
            }
          end,
        }
        "#,
    );
    let host = host_with(dir.path());
    host.load_file(&dir.path().join("init.lua")).unwrap();

    let v = call(&host, "demo.par_settled").await;
    assert_eq!(v["ok1"], serde_json::json!(true));
    assert_eq!(v["v1"], serde_json::json!(1));
    assert_eq!(v["ok2"], serde_json::json!(false));
    assert!(
        v["e2"].as_str().unwrap().contains("boom"),
        "error text should surface: {:?}",
        v["e2"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn parallel_raises_first_error_by_default() {
    let dir = write_init(
        r#"
        agentd.tool{ name = "demo" }
        agentd.action{
          name = "demo.par_raise",
          handler = function(_, ctx)
            local ok, err = pcall(parallel, { function() error("kaboom") end })
            return { ok = ok, err = tostring(err) }
          end,
        }
        "#,
    );
    let host = host_with(dir.path());
    host.load_file(&dir.path().join("init.lua")).unwrap();

    let v = call(&host, "demo.par_raise").await;
    assert_eq!(v["ok"], serde_json::json!(false));
    assert!(v["err"].as_str().unwrap().contains("kaboom"));
}

/// Mock runner that returns malformed JSON on the first call and well-formed
/// (fenced) JSON afterwards, so `ctx.structured` must reprompt once.
struct FlakyJsonRunner {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl RunnerDispatcher for FlakyJsonRunner {
    async fn run_runner_json(
        &self,
        _caller: Caller,
        _name: &str,
        _opts: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let text = if n == 0 {
            // Not JSON at all.
            "sorry, here is the answer: nope".to_string()
        } else {
            // Valid JSON wrapped in a markdown fence — structured must strip it.
            "```json\n{\"summary\":\"done\",\"n\":5}\n```".to_string()
        };
        Ok(serde_json::json!({
            "text": text,
            "provider": "mock",
            "model": "mock-1",
            "stop_reason": "stop",
        }))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn structured_retries_then_returns_validated_table() {
    let dir = write_init(
        r#"
        agentd.tool{ name = "demo" }
        agentd.action{
          name = "demo.structured",
          handler = function(_, ctx)
            local v = ctx.structured("scorer", {
              prompt = "score it",
              system = "return json",
              retries = 2,
              validate = function(t)
                if type(t.n) ~= "number" then return false, "missing n" end
                return true
              end,
            })
            return { summary = v.summary, n = v.n, model = v.model }
          end,
        }
        "#,
    );
    let host = host_with(dir.path());
    host.set_runner_dispatcher(Arc::new(FlakyJsonRunner {
        calls: AtomicUsize::new(0),
    }));
    host.load_file(&dir.path().join("init.lua")).unwrap();

    let v = call(&host, "demo.structured").await;
    assert_eq!(v["summary"], serde_json::json!("done"));
    assert_eq!(v["n"], serde_json::json!(5));
    assert_eq!(v["model"], serde_json::json!("mock-1"));
}

/// Mock that always returns invalid output, so `ctx.structured` exhausts its
/// retries and raises.
struct AlwaysBadRunner;

#[async_trait::async_trait]
impl RunnerDispatcher for AlwaysBadRunner {
    async fn run_runner_json(
        &self,
        _caller: Caller,
        _name: &str,
        _opts: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        Ok(serde_json::json!({ "text": "still not json", "model": "mock-1" }))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn structured_raises_after_exhausting_retries() {
    let dir = write_init(
        r#"
        agentd.tool{ name = "demo" }
        agentd.action{
          name = "demo.structured_fail",
          handler = function(_, ctx)
            local ok, err = pcall(ctx.structured, "scorer", { prompt = "x", retries = 1 })
            return { ok = ok, err = tostring(err) }
          end,
        }
        "#,
    );
    let host = host_with(dir.path());
    host.set_runner_dispatcher(Arc::new(AlwaysBadRunner));
    host.load_file(&dir.path().join("init.lua")).unwrap();

    let v = call(&host, "demo.structured_fail").await;
    assert_eq!(v["ok"], serde_json::json!(false));
    assert!(
        v["err"]
            .as_str()
            .unwrap()
            .contains("structured output failed"),
        "got: {:?}",
        v["err"]
    );
}

/// Mock for `validate = "inherit"`: first reply is missing the required `n`
/// field, second conforms — structured must reprompt using the action's own
/// output schema as the contract.
struct SchemaFlakyRunner {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl RunnerDispatcher for SchemaFlakyRunner {
    async fn run_runner_json(
        &self,
        _caller: Caller,
        _name: &str,
        _opts: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let text = if n == 0 {
            r#"{"summary":"done"}"#.to_string()
        } else {
            r#"{"summary":"done","n":5}"#.to_string()
        };
        Ok(serde_json::json!({ "text": text, "model": "mock-1" }))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn structured_inherit_validates_against_action_output_schema() {
    let dir = write_init(
        r#"
        agentd.tool{ name = "demo" }
        agentd.action{
          name = "demo.inherit",
          output = {
            summary = { type = "string", required = true },
            n = { type = "integer", required = true },
          },
          handler = function(_, ctx)
            local v = ctx.structured("scorer", {
              prompt = "score it",
              validate = "inherit",
            })
            return v
          end,
        }
        "#,
    );
    let host = host_with(dir.path());
    host.set_runner_dispatcher(Arc::new(SchemaFlakyRunner {
        calls: AtomicUsize::new(0),
    }));
    host.load_file(&dir.path().join("init.lua")).unwrap();

    let v = call(&host, "demo.inherit").await;
    // First reply lacked `n` → reprompted → second conforms; the returned
    // table then also passes the action's own output validation.
    assert_eq!(v["summary"], serde_json::json!("done"));
    assert_eq!(v["n"], serde_json::json!(5));
}

#[tokio::test(flavor = "multi_thread")]
async fn structured_inherit_without_output_schema_is_an_error() {
    let dir = write_init(
        r#"
        agentd.tool{ name = "demo" }
        agentd.action{
          name = "demo.inherit_missing",
          handler = function(_, ctx)
            local ok, err = pcall(ctx.structured, "scorer", {
              prompt = "x",
              validate = "inherit",
            })
            return { ok = ok, err = tostring(err) }
          end,
        }
        "#,
    );
    let host = host_with(dir.path());
    host.set_runner_dispatcher(Arc::new(SchemaFlakyRunner {
        calls: AtomicUsize::new(0),
    }));
    host.load_file(&dir.path().join("init.lua")).unwrap();

    let v = call(&host, "demo.inherit_missing").await;
    assert_eq!(v["ok"], serde_json::json!(false));
    assert!(
        v["err"].as_str().unwrap().contains("no output schema"),
        "got: {:?}",
        v["err"]
    );
}
