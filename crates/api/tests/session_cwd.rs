//! `sessions.create` with a working folder: trusted folders are accepted,
//! others are asked about once through the approval channel, and only local
//! connections may choose a folder.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agentd_api::{AppState, router, serve};
use agentd_approvals::Broker;
use agentd_executor::Executor;
use agentd_permissions::{Engine, Grants};
use agentd_scripting::LuaHost;
use agentd_sessions::MemSessionStore;
use agentd_trace::{TraceEvent, TraceSink};
use agentd_types::Registry;
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

struct NullSink;
#[async_trait]
impl TraceSink for NullSink {
    async fn record(&self, _e: TraceEvent) {}
}

struct Booted {
    addr: String,
    grants: PathBuf,
    _dir: tempfile::TempDir,
}

fn load_grants(path: &Path) -> Result<Engine, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    Ok(Engine::new(Grants::from_file(
        agentd_permissions::parse_grants(&text)?,
    )))
}

async fn boot(grants_toml: &str) -> Booted {
    let dir = tempfile::tempdir().unwrap();
    let grants = dir.path().join("grants.toml");
    std::fs::write(&grants, grants_toml).unwrap();
    let host = LuaHost::new().unwrap();
    let (skills, runners, services) = (host.skills(), host.runners(), host.services());
    let registry: Arc<dyn Registry> = Arc::new(host);
    let mut executor = Executor::new(
        registry,
        Arc::new(NullSink),
        Arc::new(load_grants(&grants).unwrap()),
        runners,
        services,
        skills,
        Arc::new(agentd_ai::ProviderRegistry::new()),
    );
    executor.set_sessions(Arc::new(MemSessionStore::new()));
    executor.set_grants_path(grants.clone());
    let reload = grants.clone();
    executor.set_reload_grants(Arc::new(move || load_grants(&reload)));
    let broker = Arc::new(Broker::new(Duration::from_secs(5)));
    executor.set_broker(broker.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = AppState {
        lifecycle: Arc::new(agentd_api::Lifecycle::default()),
        executor: Arc::new(arc_swap::ArcSwap::from(Arc::new(executor))),
        auth_token: Some(Arc::new("public-tok".into())),
        admin_token: Some(Arc::new("admin-tok".into())),
        interface_tokens: Arc::new(HashMap::from([("bot-tok".into(), "bot".into())])),
        broker,
        webhooks: Arc::new(HashMap::new()),
        ui: agentd_ui::Hub::new(),
    };
    tokio::spawn(async move {
        let _ = serve(listener, router(state)).await;
    });
    Booted {
        addr: format!("ws://{addr}"),
        grants,
        _dir: dir,
    }
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(url: &str, token: &str) -> Socket {
    let mut request = url.into_client_request().unwrap();
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    tokio_tungstenite::connect_async(request).await.unwrap().0
}

async fn next_json(socket: &mut Socket) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Message::Text(text) = socket.next().await.unwrap().unwrap() {
                return serde_json::from_str(&text).unwrap();
            }
        }
    })
    .await
    .expect("a reply within 10 seconds")
}

async fn create(b: &Booted, token: &str, cwd: &str) -> Value {
    let mut socket = connect(&format!("{}/ws", b.addr), token).await;
    socket
        .send(Message::Text(
            json!({ "id": 1, "method": "sessions.create", "params": { "cwd": cwd } })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    next_json(&mut socket).await
}

/// An approver on `/control` that answers every question with `verdict` and
/// reports the kinds it was asked about.
async fn approver(
    b: &Booted,
    verdict: &'static str,
) -> tokio::sync::mpsc::UnboundedReceiver<Value> {
    let mut socket = connect(&format!("{}/control", b.addr), "admin-tok").await;
    socket
        .send(Message::Text(
            json!({ "id": 1, "method": "approvals.subscribe" })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    next_json(&mut socket).await;
    let (seen, asked) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let frame = next_json(&mut socket).await;
            if frame.get("event").and_then(Value::as_str) != Some("approval.request") {
                continue;
            }
            let _ = seen.send(frame["req"].clone());
            let request_id = frame["req"]["id"].clone();
            socket
                .send(Message::Text(
                    json!({ "id": 2, "method": "approvals.resolve",
                            "params": { "request_id": request_id, "verdict": verdict } })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
        }
    });
    asked
}

fn project() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir
        .path()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    (dir, path)
}

#[tokio::test]
async fn trusted_folders_are_accepted_without_asking() {
    let (_p, folder) = project();
    let b = boot(&format!("[folders]\ntrusted = [\"{folder}\"]\n")).await;
    let reply = create(&b, "public-tok", &folder).await;
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(reply["result"]["cwd"], folder.as_str());
}

#[tokio::test]
async fn untrusted_folders_ask_and_allow_once_does_not_remember() {
    let (_p, folder) = project();
    let b = boot("").await;
    let mut asked = approver(&b, "allow_once").await;
    let reply = create(&b, "public-tok", &folder).await;
    assert_eq!(reply["ok"], true, "{reply}");
    let question = tokio::time::timeout(Duration::from_secs(5), asked.recv())
        .await
        .expect("the folder question reaches the approver")
        .unwrap();
    assert_eq!(question["kind"], "folder");
    assert_eq!(question["action"], folder.as_str());
    assert!(
        !std::fs::read_to_string(&b.grants)
            .unwrap()
            .contains("trusted")
    );
}

#[tokio::test]
async fn always_trust_remembers_the_folder() {
    let (_p, folder) = project();
    let b = boot("").await;
    {
        let _asked = approver(&b, "allow_forever").await;
        let reply = create(&b, "admin-tok", &folder).await;
        assert_eq!(reply["ok"], true, "{reply}");
    }
    let grants = std::fs::read_to_string(&b.grants).unwrap();
    assert!(grants.contains(&folder), "{grants}");
    // Nobody is listening now, so this succeeds only because nothing is asked.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let reply = create(&b, "public-tok", &folder).await;
    assert_eq!(reply["ok"], true, "{reply}");
}

#[tokio::test]
async fn denied_or_unanswered_folders_are_refused() {
    let (_p, folder) = project();
    let b = boot("").await;
    let reply = create(&b, "public-tok", &folder).await;
    assert_eq!(reply["code"], "session_folder_denied", "{reply}");
    let _asked = approver(&b, "deny").await;
    let reply = create(&b, "public-tok", &folder).await;
    assert_eq!(reply["code"], "session_folder_denied", "{reply}");
    assert!(
        reply["tip"].as_str().unwrap().contains("[folders]"),
        "{reply}"
    );
}

#[tokio::test]
async fn interface_tokens_cannot_choose_a_folder() {
    let (_p, folder) = project();
    let b = boot("").await;
    let mut asked = approver(&b, "allow_once").await;
    let reply = create(&b, "bot-tok", &folder).await;
    assert_eq!(reply["code"], "session_folder_not_allowed", "{reply}");
    assert!(
        tokio::time::timeout(Duration::from_millis(200), asked.recv())
            .await
            .is_err(),
        "nobody is asked"
    );
}

#[tokio::test]
async fn root_relative_missing_and_file_folders_are_refused() {
    let (project_dir, folder) = project();
    std::fs::write(project_dir.path().join("file.txt"), "x").unwrap();
    let b = boot("[folders]\ntrusted = [\"/tmp/**\"]\n").await;
    for (cwd, says) in [
        ("/".to_owned(), "root"),
        ("relative/path".to_owned(), "absolute"),
        (format!("{folder}/missing"), "does not exist"),
        (format!("{folder}/file.txt"), "not a folder"),
    ] {
        let reply = create(&b, "public-tok", &cwd).await;
        assert_eq!(
            reply["code"], "session_folder_not_allowed",
            "{cwd}: {reply}"
        );
        assert!(
            reply["error"].as_str().unwrap().contains(says),
            "{cwd}: {reply}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn symlinks_are_judged_by_where_they_lead() {
    let (_trusted_dir, trusted) = project();
    let (_outside_dir, outside) = project();
    std::os::unix::fs::symlink(&outside, format!("{trusted}/escape")).unwrap();
    std::os::unix::fs::symlink(&trusted, format!("{outside}/into")).unwrap();
    let b = boot(&format!(
        "[folders]\ntrusted = [\"{trusted}\", \"{trusted}/**\"]\n"
    ))
    .await;

    // A link inside the trusted folder that leads outside is asked about.
    let reply = create(&b, "public-tok", &format!("{trusted}/escape")).await;
    assert_eq!(reply["code"], "session_folder_denied", "{reply}");

    // A link from outside into the trusted folder is accepted as its real path.
    let reply = create(&b, "public-tok", &format!("{outside}/into")).await;
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(reply["result"]["cwd"], trusted.as_str());
}

#[tokio::test]
async fn a_taken_label_is_reported_before_any_folder_question() {
    let (_p, folder) = project();
    let b = boot("").await;
    let mut socket = connect(&format!("{}/ws", b.addr), "public-tok").await;
    socket
        .send(Message::Text(
            json!({ "id": 1, "method": "sessions.create", "params": { "label": "chat" } })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    assert_eq!(next_json(&mut socket).await["ok"], true);
    let mut asked = approver(&b, "allow_once").await;
    socket
        .send(Message::Text(
            json!({ "id": 2, "method": "sessions.create", "params": { "label": "chat", "cwd": folder } })
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let reply = next_json(&mut socket).await;
    assert_eq!(reply["code"], "session_label_taken", "{reply}");
    assert!(
        tokio::time::timeout(Duration::from_millis(200), asked.recv())
            .await
            .is_err(),
        "nobody is asked about the folder"
    );
}
