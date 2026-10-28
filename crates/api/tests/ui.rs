//! The UI protocol over `/ws`: subscribe, get a snapshot, then a change,
//! and a click that reaches the Lua handler, which acts for the chat.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use agentd_api::{AppState, router, serve};
use agentd_executor::Executor;
use agentd_permissions::{Engine, Grants, GrantsFile};
use agentd_scripting::LuaHost;
use agentd_trace::{TraceEvent, TraceSink};
use agentd_types::Registry;
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

struct NullSink;
#[async_trait]
impl TraceSink for NullSink {
    async fn record(&self, _e: TraceEvent) {}
}

async fn boot(script: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    let init = dir.path().join("init.lua");
    std::fs::write(&init, script).unwrap();
    let host = Arc::new(LuaHost::new().unwrap());
    host.start_async_runtime(tokio::runtime::Handle::current());
    let hub = agentd_ui::Hub::new();
    host.set_ui(hub.clone());
    host.load_file(&init).unwrap();
    hub.set_script(Some(host.ui_script(tokio::runtime::Handle::current())));
    std::mem::forget(dir);

    let registry: Arc<dyn Registry> = host.clone();
    let executor = Arc::new(Executor::new(
        registry,
        Arc::new(NullSink),
        Arc::new(Engine::new(Grants::from_file(GrantsFile::default()))),
        host.runners(),
        host.services(),
        host.skills(),
        Arc::new(agentd_ai::ProviderRegistry::new()),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = AppState {
        lifecycle: Arc::new(agentd_api::Lifecycle::default()),
        executor: Arc::new(arc_swap::ArcSwap::from(executor)),
        auth_token: None,
        admin_token: None,
        interface_tokens: Arc::new(HashMap::new()),
        broker: Arc::new(agentd_approvals::Broker::new(Duration::from_secs(30))),
        webhooks: Arc::new(HashMap::new()),
        ui: hub,
    };
    tokio::spawn(async move {
        let _ = serve(listener, router(state)).await;
    });
    format!("ws://{addr}/ws")
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn next_json(sock: &mut Socket) -> Value {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(5), sock.next())
            .await
            .expect("the daemon went quiet")
            .unwrap()
            .unwrap();
        if let Message::Text(t) = message {
            return serde_json::from_str(&t).unwrap();
        }
    }
}

/// Read frames until an op matches; returns it.
async fn op_where(sock: &mut Socket, found: impl Fn(&Value) -> bool) -> Value {
    loop {
        let message = next_json(sock).await;
        if message["event"] != "ui.frame" {
            continue;
        }
        if let Some(op) = message["frame"]["ops"]
            .as_array()
            .and_then(|ops| ops.iter().find(|op| found(op)))
        {
            return op.clone();
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn subscribe_snapshot_change_and_a_click_reaching_lua() {
    let url = boot(
        r#"
        agentd.ui.on("chat", function(chat)
          local doc = chat.document
          local go = doc:button{ id = "go", label = "Go", style = { fg = "accent" } }
          go:on("click", function(event, chat, ctx)
            event.target.label = "Clicked by " .. ctx.caller.interface
          end)
          doc:get("main"):append(go)
        end)
        "#,
    )
    .await;
    let (mut sock, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    sock.send(Message::Text(
        json!({
            "id": 1,
            "method": "ui.subscribe",
            "params": { "version": 1, "client": { "kind": "test", "width": 80, "height": 24 } }
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();

    let ready = next_json(&mut sock).await;
    assert_eq!(ready["event"], "ui.ready", "{ready}");
    let chat = ready["ready"]["chat"].as_u64().unwrap();

    let snapshot = next_json(&mut sock).await;
    assert_eq!(snapshot["frame"]["reset"], true, "{snapshot}");
    assert!(
        snapshot["frame"]["ops"]
            .as_array()
            .unwrap()
            .iter()
            .any(|op| op["op"] == "create" && op["props"]["id"] == "prompt")
    );

    let is_button = |op: &Value| op["op"] == "create" && op["kind"] == "button";
    let created = match snapshot["frame"]["ops"]
        .as_array()
        .unwrap()
        .iter()
        .find(|op| is_button(op))
    {
        Some(op) => op.clone(),
        None => op_where(&mut sock, is_button).await,
    };
    let node = created["node"].as_u64().unwrap();
    assert_eq!(created["style"]["fg"], "accent");

    sock.send(Message::Text(
        json!({
            "id": 2,
            "method": "ui.event",
            "params": { "chat": chat, "node": node, "type": "click" }
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    let changed = op_where(&mut sock, |op| op["op"] == "set" && op["key"] == "label").await;
    assert_eq!(changed["node"], node);
    assert_eq!(changed["value"], "Clicked by ws");

    sock.send(Message::Text(
        json!({ "id": 3, "method": "ui.event", "params": { "chat": chat + 100, "type": "click" } })
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    loop {
        let message = next_json(&mut sock).await;
        if message["id"] == 3 {
            assert_eq!(message["code"], "ui_chat_not_found", "{message}");
            break;
        }
    }
}

/// What a client shows for one animated style value, replaying frames the
/// way `agentctl chat` does: a cancel drops queued frames later than it,
/// and frames play in `(at, seq)` order.
#[tokio::test(flavor = "multi_thread")]
async fn events_never_make_an_animation_jump_or_blank() {
    let url = boot(
        r#"
        agentd.ui.on("chat", function(chat)
          local doc = chat.document
          local box = doc:checkbox{ id = "box", label = "unrelated" }
          local last = doc:text{ id = "last", text = "" }
          box:on("change", function(event) last.text = "change " .. tostring(event.value) end)
          doc:get("main"):append(box)
          doc:get("main"):append(last)
          chat:frames(20, function(t)
            doc:get("prompt").style.border_fg = string.format("%d", math.floor(t * 1000))
          end)
        end)
        "#,
    )
    .await;
    let (mut sock, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    sock.send(Message::Text(
        json!({
            "id": 1,
            "method": "ui.subscribe",
            "params": { "version": 1, "client": { "kind": "test", "width": 80, "height": 24 } }
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    let ready = next_json(&mut sock).await;
    let chat = ready["ready"]["chat"].as_u64().unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let (mut write, mut read) = sock.split();
    tokio::spawn(async move {
        while let Some(Ok(Message::Text(t))) = read.next().await {
            let v: Value = serde_json::from_str(&t).unwrap();
            if v["event"] == "ui.frame" {
                let _ = tx.send(v["frame"].clone());
            }
        }
    });

    // Find the checkbox, then spam changes on it for a while.
    let mut frames: Vec<Value> = Vec::new();
    let box_node = loop {
        let frame = rx.recv().await.unwrap();
        let node = frame["ops"].as_array().and_then(|ops| {
            ops.iter()
                .find(|op| op["op"] == "create" && op["props"]["id"] == "box")
                .map(|op| op["node"].clone())
        });
        frames.push(frame);
        if let Some(node) = node {
            break node;
        }
    };
    for i in 0..30 {
        write
            .send(Message::Text(
                json!({ "id": 10 + i, "method": "ui.event",
                        "params": { "chat": chat, "node": box_node, "type": "change", "value": i % 2 == 0 } })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(37)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    while let Ok(frame) = rx.try_recv() {
        frames.push(frame);
    }

    // Replay like the client.
    let mut queue: Vec<Value> = Vec::new();
    for frame in frames {
        let at = frame["at"].as_u64().unwrap();
        if frame["cancel_ahead"] == true {
            queue.retain(|f| f["at"].as_u64().unwrap() <= at);
        }
        if frame["reset"] == true {
            queue.clear();
        }
        let key = (at, frame["seq"].as_u64().unwrap());
        let pos = queue
            .partition_point(|f| (f["at"].as_u64().unwrap(), f["seq"].as_u64().unwrap()) <= key);
        queue.insert(pos, frame);
    }
    let mut shown: Vec<Value> = Vec::new();
    let mut offsets: Vec<i64> = Vec::new();
    for frame in &queue {
        for op in frame["ops"].as_array().unwrap() {
            if op["key"] == "border_fg" {
                shown.push(op["value"].clone());
                if let Some(t) = op["value"].as_str().and_then(|s| s.parse::<i64>().ok()) {
                    offsets.push(frame["at"].as_i64().unwrap() - t);
                }
            }
        }
    }
    let (low, high) = (offsets.iter().min().unwrap(), offsets.iter().max().unwrap());
    assert!(
        high - low <= 60,
        "frames show a time that does not match when they play: {offsets:?}"
    );
    let times: Vec<i64> = shown
        .iter()
        .map(|v| v.as_str().and_then(|s| s.parse().ok()).unwrap_or(-1))
        .collect();
    assert!(times.len() > 20, "{times:?}");
    assert!(!times.contains(&-1), "the border went blank: {times:?}");
    assert!(
        times.windows(2).all(|w| w[0] <= w[1]),
        "the animation went back in time: {times:?}"
    );
}
