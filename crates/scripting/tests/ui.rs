//! `agentd.ui`: scripts build a chat's elements, and the controls chat
//! owns stay out of their reach.

use std::sync::Arc;
use std::time::Duration;

use agentd_scripting::LuaHost;
use agentd_ui::{ChatInfo, Hub, Op, Subscription};

fn start(script: &str) -> (Arc<Hub>, Result<(), String>, LuaHost) {
    let dir = tempfile::tempdir().unwrap();
    let init = dir.path().join("init.lua");
    std::fs::write(&init, script).unwrap();
    let host = LuaHost::new().unwrap();
    host.start_async_runtime(tokio::runtime::Handle::current());
    let hub = Hub::new();
    host.set_ui(
        hub.clone(),
        Arc::new(agentd_permissions::PermissionSet::empty),
    );
    let loaded = host.load_file(&init).map_err(|e| format!("{e:#}"));
    hub.set_script(Some(host.ui_script(tokio::runtime::Handle::current())));
    (hub, loaded, host)
}

/// Wait until an op matches, looking at every op the chat has sent.
async fn wait_for(sub: &mut Subscription, seen: &mut Vec<Op>, found: impl Fn(&Op) -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !seen.iter().any(&found) {
        let frame = tokio::time::timeout_at(deadline, sub.frames.recv())
            .await
            .expect("the expected change never arrived")
            .expect("the chat closed");
        seen.extend(frame.ops);
    }
}

fn note_containing(op: &Op, text: &str) -> bool {
    matches!(op, Op::Note { text: t } if t.contains(text))
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_keys_cannot_be_bound_and_the_prompt_cannot_be_hidden() {
    let (_, loaded, _host) = start(r#"agentd.ui.bind("Ctrl+C", function() end)"#);
    let error = loaded.unwrap_err();
    assert!(error.contains("`ctrl+c` belongs to chat"), "{error}");

    let (hub, loaded, _host) = start(
        r#"
        agentd.ui.bind("ctrl+g", function() end)
        agentd.ui.on("chat", function(chat)
          chat.document:get("prompt").visible = false
        end)
        "#,
    );
    loaded.unwrap();
    let mut sub = hub.subscribe(ChatInfo::default());
    let mut seen = Vec::new();
    wait_for(
        &mut sub,
        &mut seen,
        |op| matches!(op, Op::Bindings { keys } if keys == &["ctrl+g"]),
    )
    .await;
    wait_for(&mut sub, &mut seen, |op| {
        note_containing(op, "the prompt belongs to chat")
    })
    .await;
    assert_ne!(
        sub.chat.doc().prop(agentd_ui::parts::PROMPT, "visible"),
        Some(&serde_json::json!(false))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_frame_function_may_only_change_the_ui() {
    let (hub, loaded, _host) = start(
        r#"
        agentd.ui.on("chat", function(chat)
          local clock = chat.document:text{ id = "clock" }
          chat.document:get("main"):append(clock)
          chat:frames(20, function(t)
            clock.text = string.format("%.2f", t)
          end)
          chat:frames(20, function(t)
            if t > 0 then sleep(1) end
          end)
        end)
        "#,
    );
    loaded.unwrap();
    let mut sub = hub.subscribe(ChatInfo::default());
    let mut seen = Vec::new();
    wait_for(&mut sub, &mut seen, |op| {
        note_containing(op, "only UI changes are allowed inside ui.frames")
    })
    .await;
    // The well-behaved frame function keeps running.
    seen.clear();
    wait_for(
        &mut sub,
        &mut seen,
        |op| matches!(op, Op::Set { key, .. } if key == "text"),
    )
    .await;
}
