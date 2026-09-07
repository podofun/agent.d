//! Deterministic subprocess test; no installed model CLI or credentials needed.
#![cfg(unix)]

use agentd_ai::{CodexAppServerProvider, CompletionRequest, Provider};
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn cancelled_turn_terminates_and_replaces_app_server() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("fake-codex");
    std::fs::write(&script, r#"#!/usr/bin/env python3
import json, pathlib, sys
root = pathlib.Path(__file__).parent
counter = root / 'count'
n = int(counter.read_text()) + 1 if counter.exists() else 1
counter.write_text(str(n))
def send(value):
    print(json.dumps(value), flush=True)
for line in sys.stdin:
    request = json.loads(line)
    method = request.get('method')
    if method == 'initialize':
        send({'id': request['id'], 'result': {}})
    elif method == 'thread/start':
        send({'id': request['id'], 'result': {'thread': {'id': 't'}}})
    elif method == 'turn/start':
        send({'id': request['id'], 'result': {'turn': {'id': 'turn'}}})
        (root / 'started').write_text(str(n))
        if n > 1:
            send({'method': 'item/completed', 'params': {'threadId': 't', 'item': {'type': 'agentMessage', 'text': 'fresh'}}})
            send({'method': 'turn/completed', 'params': {'threadId': 't'}})
"#).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let provider = Arc::new(CodexAppServerProvider::new().with_bin(script.to_string_lossy()));
    let first_provider = provider.clone();
    let first = tokio::spawn(async move {
        first_provider
            .complete(CompletionRequest::prompt("wait"))
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !dir.path().join("started").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        provider.complete(CompletionRequest::prompt("hello")),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.text, "fresh");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("count")).unwrap(),
        "2"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn cancelling_cli_completion_reaps_the_child() {
    for codex in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-cli");
        std::fs::write(
            &script,
            r#"#!/usr/bin/env python3
import os, pathlib, sys, time
sys.stdin.read()
(pathlib.Path(__file__).parent / 'pid').write_text(str(os.getpid()))
time.sleep(60)
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let bin = script.to_string_lossy().into_owned();
        let provider: Arc<dyn Provider> = if codex {
            Arc::new(agentd_ai::CodexCliProvider::new().with_bin(bin))
        } else {
            Arc::new(agentd_ai::ClaudeCliProvider::new().with_bin(bin))
        };
        let run =
            tokio::spawn(async move { provider.complete(CompletionRequest::prompt("wait")).await });
        let pid_path = dir.path().join("pid");
        let pid = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(pid) = std::fs::read_to_string(&pid_path)
                    && !pid.is_empty()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        run.abort();
        assert!(run.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(5), async {
            while std::path::Path::new(&format!("/proc/{pid}")).exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cancelled CLI child must be reaped");
    }
}
