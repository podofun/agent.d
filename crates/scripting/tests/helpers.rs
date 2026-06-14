//! Coverage for the helper surface installed by `helpers.lua` plus the
//! Rust-side `ctx.state` / `json.null` / `agentd.service(name, opts, fn)`
//! additions.

use std::io::Write;
use std::sync::Arc;

use agentd_permissions::{Caller, PermissionSet};
use agentd_scripting::LuaHost;
use agentd_types::{ActionCall, CallContext, Registry};

fn write_init(body: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("init.lua");
    let mut f = std::fs::File::create(&p).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    dir
}

async fn run_action(host: &LuaHost, name: &str) -> serde_json::Value {
    host.call(
        CallContext {
            caller: Caller::default(),
            effective_grants: PermissionSet::empty(),
            call_chain: vec![name.to_string()],
        },
        ActionCall {
            action: name.to_string(),
            args: serde_json::Value::Null,
        },
    )
    .await
    .unwrap()
    .value
}

#[tokio::test(flavor = "multi_thread")]
async fn try_helper_returns_ok_and_err() {
    let dir = write_init(
        r#"
        agentd.tool{ name = "t" }
        agentd.action{
          name = "t.ok",
          handler = function(_, ctx)
            local ok, val = pcall(function() return 42 end)
            return { ok = ok, val = val }
          end,
        }
        agentd.action{
          name = "t.err",
          handler = function(_, ctx)
            local ok, err = pcall(function() error("boom") end)
            return { ok = ok, has_err = tostring(err):match("boom") ~= nil }
          end,
        }
        "#,
    );
    let host = LuaHost::new().unwrap();
    host.set_root(dir.path());
    host.load_file(&dir.path().join("init.lua")).unwrap();
    let ok = run_action(&host, "t.ok").await;
    assert_eq!(ok["ok"], true);
    assert_eq!(ok["val"], 42);
    let err = run_action(&host, "t.err").await;
    assert_eq!(err["ok"], false);
    assert_eq!(err["has_err"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn json_null_sentinel_round_trips() {
    let dir = write_init(
        r#"
        agentd.tool{ name = "j" }
        agentd.action{
          name = "j.null",
          handler = function(_, ctx)
            local d = json.decode('{"a": null, "b": 1}')
            local d2 = json.decode('{"a": null}', { nulls = "nil" })
            return {
              is_null = json.is_null(d.a),
              encoded = json.encode({ x = json.null }),
              nullable = (d2.a == nil),
            }
          end,
        }
        "#,
    );
    let host = LuaHost::new().unwrap();
    host.set_root(dir.path());
    host.load_file(&dir.path().join("init.lua")).unwrap();
    let res = run_action(&host, "j.null").await;
    assert_eq!(res["is_null"], true);
    assert_eq!(res["encoded"], serde_json::json!("{\"x\":null}"));
    assert_eq!(res["nullable"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn state_kv_round_trip() {
    let dir = write_init(
        r#"
        agentd.tool{ name = "s" }
        agentd.action{
          name = "s.run",
          handler = function(_, ctx)
            ctx.state.set("k", { n = 1, list = {1, 2, 3} })
            local v = ctx.state.get("k")
            local keys = ctx.state.keys()
            local removed = ctx.state.delete("k")
            local after = ctx.state.get("k")
            return { n = v.n, first = v.list[1], keys_count = #keys, removed = removed, after = after }
          end,
        }
        "#,
    );
    let host = LuaHost::new().unwrap();
    host.set_root(dir.path());
    host.load_file(&dir.path().join("init.lua")).unwrap();
    let res = run_action(&host, "s.run").await;
    assert_eq!(res["n"], 1);
    assert_eq!(res["first"], 1);
    assert_eq!(res["keys_count"], 1);
    assert_eq!(res["removed"], true);
    assert_eq!(res["after"], serde_json::Value::Null);
}

#[tokio::test(flavor = "multi_thread")]
async fn timer_every_ticks_and_stops() {
    let dir = write_init(
        r#"
        agentd.tool{ name = "tm" }
        agentd.action{
          name = "tm.run",
          handler = function(_, ctx)
            ctx.state.set("ticks", 0)
            local h = timer.every(20, function()
              ctx.state.set("ticks", (ctx.state.get("ticks") or 0) + 1)
            end)
            sleep(120)
            h:stop()
            local at_stop = ctx.state.get("ticks")
            sleep(80)
            local after = ctx.state.get("ticks")
            return { ticked = at_stop > 0, frozen_after_stop = (after == at_stop) }
          end,
        }
        "#,
    );
    let host = Arc::new(LuaHost::new().unwrap());
    host.set_root(dir.path());
    host.start_async_runtime(tokio::runtime::Handle::current());
    host.load_file(&dir.path().join("init.lua")).unwrap();
    let res = run_action(&host, "tm.run").await;
    assert_eq!(res["ticked"], true);
    assert_eq!(res["frozen_after_stop"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn service_opts_parsed() {
    let dir = write_init(
        r#"
        agentd.service("supervised", { restart = "always", backoff_ms = 250, backoff_max_ms = 2000 }, function(ctx)
          -- body is a no-op for this registration-only test.
        end)
        "#,
    );
    let host = LuaHost::new().unwrap();
    host.set_root(dir.path());
    host.load_file(&dir.path().join("init.lua")).unwrap();
    let def = host
        .services()
        .get("supervised")
        .expect("service registered");
    assert_eq!(def.restart.as_deref(), Some("always"));
    assert_eq!(def.backoff_ms, Some(250));
    assert_eq!(def.backoff_max_ms, Some(2000));
}

#[tokio::test(flavor = "multi_thread")]
async fn string_helpers_trim_family() {
    let dir = write_init(
        r#"
        agentd.tool{ name = "s" }
        agentd.action{
          name = "s.trim",
          handler = function(_, ctx)
            return {
              trim   = ("  hi there \t\n"):trim(),
              ltrim  = ("  hi "):ltrim(),
              rtrim  = ("  hi "):rtrim(),
              noop   = ("hi"):trim(),
              empty  = ("   "):trim(),
              fn_form = string.trim("  x  "),
            }
          end,
        }
        "#,
    );
    let host = LuaHost::new().unwrap();
    host.set_root(dir.path());
    host.load_file(&dir.path().join("init.lua")).unwrap();
    let v = run_action(&host, "s.trim").await;
    assert_eq!(v["trim"], "hi there");
    assert_eq!(v["ltrim"], "hi ");
    assert_eq!(v["rtrim"], "  hi");
    assert_eq!(v["noop"], "hi");
    assert_eq!(v["empty"], "");
    assert_eq!(v["fn_form"], "x");
}

#[tokio::test(flavor = "multi_thread")]
async fn string_helpers_predicates() {
    let dir = write_init(
        r#"
        agentd.tool{ name = "s" }
        agentd.action{
          name = "s.pred",
          handler = function(_, ctx)
            return {
              sw_yes  = ("ws-42"):startswith("ws-"),
              sw_no   = ("ws-42"):startswith("tg-"),
              sw_empty = ("x"):startswith(""),
              ew_yes  = ("init.lua"):endswith(".lua"),
              ew_no   = ("init.lua"):endswith(".md"),
              ew_empty = ("x"):endswith(""),
              ct_yes  = ("a.b.c"):contains(".b."),
              ct_no   = ("abc"):contains("z"),
              -- plain-text mode: magic chars must not act as patterns
              ct_plain = ("100%"):contains("0%"),
            }
          end,
        }
        "#,
    );
    let host = LuaHost::new().unwrap();
    host.set_root(dir.path());
    host.load_file(&dir.path().join("init.lua")).unwrap();
    let v = run_action(&host, "s.pred").await;
    assert_eq!(v["sw_yes"], true);
    assert_eq!(v["sw_no"], false);
    assert_eq!(v["sw_empty"], true);
    assert_eq!(v["ew_yes"], true);
    assert_eq!(v["ew_no"], false);
    assert_eq!(v["ew_empty"], true);
    assert_eq!(v["ct_yes"], true);
    assert_eq!(v["ct_no"], false);
    assert_eq!(v["ct_plain"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn string_helpers_split() {
    let dir = write_init(
        r#"
        agentd.tool{ name = "s" }
        agentd.action{
          name = "s.split",
          handler = function(_, ctx)
            return {
              csv     = ("a,b,c"):split(","),
              empties = ("a,,b"):split(","),
              none    = ("abc"):split(","),
              multi   = ("a::b::c"):split("::"),
              ws      = ("  a  b\tc "):split(),
              plain   = ("a.b"):split("."),
            }
          end,
        }
        "#,
    );
    let host = LuaHost::new().unwrap();
    host.set_root(dir.path());
    host.load_file(&dir.path().join("init.lua")).unwrap();
    let v = run_action(&host, "s.split").await;
    assert_eq!(v["csv"], serde_json::json!(["a", "b", "c"]));
    assert_eq!(v["empties"], serde_json::json!(["a", "", "b"]));
    assert_eq!(v["none"], serde_json::json!(["abc"]));
    assert_eq!(v["multi"], serde_json::json!(["a", "b", "c"]));
    assert_eq!(v["ws"], serde_json::json!(["a", "b", "c"]));
    assert_eq!(v["plain"], serde_json::json!(["a", "b"]));
}
