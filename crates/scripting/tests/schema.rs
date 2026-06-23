//! Action `input`/`output` schema for action handlers/callbacks: compiled to JSON Schema,
//!  surfaced via `action_info`, and enforced on the way in (args) and out (return value).

use std::io::Write;
use std::sync::Arc;

use agentd_permissions::{Caller, PermissionSet};
use agentd_scripting::LuaHost;
use agentd_types::{ActionCall, CallContext, Registry};

fn host_with(root: &std::path::Path) -> Arc<LuaHost> {
    let host = LuaHost::new().unwrap();
    host.set_root(root);
    Arc::new(host)
}

fn write_init(body: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("init.lua");
    let mut f = std::fs::File::create(&p).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    dir
}

const INIT: &str = r#"
    agentd.tool{ name = "t" }
    agentd.action{
      name = "t.echo",
      input = {
        msg   = { type = "string", min_len = 1, required = true },
        count = { type = "integer" },
      },
      output = {
        echoed = { type = "string" },
      },
      handler = function(args, _)
        if args.msg == "make-bad-output" then
          return { echoed = 123 }  -- violates output schema (wants string)
        end
        return { echoed = args.msg }
      end,
    }
"#;

fn ctx() -> CallContext {
    CallContext {
        caller: Caller::default(),
        effective_grants: PermissionSet::empty(),
        call_chain: vec!["t.echo".into()],
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn action_info_exposes_compiled_input_schema() {
    let dir = write_init(INIT);
    let host = host_with(dir.path());
    host.load_file(&dir.path().join("init.lua")).unwrap();

    let info = host.action_info("t.echo").expect("action registered");
    let schema = info.input_schema.expect("input schema surfaced");
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["properties"]["msg"]["type"], "string");
    assert_eq!(schema["properties"]["msg"]["minLength"], 1);
    assert_eq!(schema["properties"]["count"]["type"], "integer");
    assert_eq!(schema["required"], serde_json::json!(["msg"]));
    assert_eq!(schema["additionalProperties"], serde_json::json!(false));
}

#[tokio::test(flavor = "multi_thread")]
async fn valid_args_pass() {
    let dir = write_init(INIT);
    let host = host_with(dir.path());
    host.load_file(&dir.path().join("init.lua")).unwrap();

    let res = host
        .call(
            ctx(),
            ActionCall {
                action: "t.echo".into(),
                args: serde_json::json!({ "msg": "hi", "count": 2 }),
            },
        )
        .await
        .unwrap();
    assert_eq!(res.value["echoed"], "hi");
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_required_arg_rejected_before_handler() {
    let dir = write_init(INIT);
    let host = host_with(dir.path());
    host.load_file(&dir.path().join("init.lua")).unwrap();

    let err = host
        .call(
            ctx(),
            ActionCall {
                action: "t.echo".into(),
                args: serde_json::json!({ "count": 1 }),
            },
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("input validation failed"), "{msg}");
    assert!(msg.contains("msg"), "{msg}");
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_arg_rejected_under_strict_default() {
    let dir = write_init(INIT);
    let host = host_with(dir.path());
    host.load_file(&dir.path().join("init.lua")).unwrap();

    let err = host
        .call(
            ctx(),
            ActionCall {
                action: "t.echo".into(),
                args: serde_json::json!({ "msg": "hi", "bogus": true }),
            },
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("bogus"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn discord_example_loads_and_exposes_schemas() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/discord");
    let host = host_with(&root);
    host.set_secrets(Arc::new(agentd_secrets::MemoryStore::default()));
    host.load_file(&root.join("init.lua")).unwrap();

    let send = host
        .action_info("discord.send")
        .and_then(|i| i.input_schema)
        .expect("discord.send input schema");
    assert_eq!(send["properties"]["channel_id"]["type"], "string");
    assert_eq!(send["properties"]["content"]["type"], "string");
    assert_eq!(
        send["required"],
        serde_json::json!(["channel_id", "content"])
    );

    let tok = host
        .action_info("discord.set_token")
        .and_then(|i| i.input_schema)
        .expect("discord.set_token input schema");
    assert_eq!(tok["required"], serde_json::json!(["token"]));
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_return_value_rejected_by_output_schema() {
    let dir = write_init(INIT);
    let host = host_with(dir.path());
    host.load_file(&dir.path().join("init.lua")).unwrap();

    let err = host
        .call(
            ctx(),
            ActionCall {
                action: "t.echo".into(),
                args: serde_json::json!({ "msg": "make-bad-output" }),
            },
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("output validation failed"),
        "{err}"
    );
}
