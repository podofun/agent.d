use std::time::Duration;

use agentd_types::ApprovalRequest;
use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use super::app::{Decision, Update};
use crate::ws::{token_from_env_or_file, ws_url_with_path};

pub(super) fn start(
    base: String,
    timeout: u64,
    updates: mpsc::UnboundedSender<Update>,
    mut decisions: mpsc::UnboundedReceiver<Decision>,
) {
    tokio::spawn(async move {
        if let Err(error) = listen(&base, timeout, &updates, &mut decisions).await {
            let _ = updates.send(Update::ControlLost(error.to_string()));
        }
    });
}

async fn listen(
    base: &str,
    timeout: u64,
    updates: &mpsc::UnboundedSender<Update>,
    decisions: &mut mpsc::UnboundedReceiver<Decision>,
) -> Result<()> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let url = ws_url_with_path(base, "/control")?;
    let mut request = url.as_str().into_client_request()?;
    if let Some(token) = token_from_env_or_file("AGENTD_ADMIN_TOKEN", "admin-token") {
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {token}").parse()?);
    }
    let (mut socket, _) = tokio::time::timeout(
        Duration::from_millis(timeout),
        tokio_tungstenite::connect_async(request),
    )
    .await
    .context("control connection timed out")??;
    socket
        .send(Message::Text(
            json!({ "id": 0, "method": "approvals.subscribe" })
                .to_string()
                .into(),
        ))
        .await?;

    loop {
        tokio::select! {
            message = socket.next() => {
                let text = match message {
                    Some(Ok(Message::Text(text))) => text.to_string(),
                    Some(Ok(Message::Binary(bytes))) => String::from_utf8_lossy(&bytes).into_owned(),
                    Some(Ok(Message::Close(_))) | None => return Err(anyhow!("control connection closed")),
                    Some(Ok(_)) => continue,
                    Some(Err(error)) => return Err(error.into()),
                };
                if let Ok(value) = serde_json::from_str::<Value>(&text)
                    && value["event"] == "approval.request"
                    && let Ok(request) = serde_json::from_value::<ApprovalRequest>(value["req"].clone())
                {
                    let _ = updates.send(Update::Approval(request));
                }
            }
            decision = decisions.recv() => {
                let Some(decision) = decision else { return Ok(()); };
                socket.send(Message::Text(json!({
                    "id": decision.id,
                    "method": "approvals.resolve",
                    "params": { "request_id": decision.id, "verdict": decision.verdict }
                }).to_string().into())).await?;
            }
        }
    }
}
