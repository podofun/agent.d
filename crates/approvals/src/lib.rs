//! Transport-agnostic approval broker.
//!
//! Holds connected approvers (any interface: agentctl `/control`, a future web
//! UI, …) and the in-flight pending requests. When the executor escalates a
//! denial it calls [`ApprovalBroker::request`]; the broker fans the request out
//! to every connected approver and waits for the first [`Verdict`]. Zero
//! approvers OR a timeout ⇒ [`Verdict::Deny`] — fail closed. The broker knows
//! nothing about WebSocket or any concrete transport.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use agentd_types::{ApprovalBroker, ApprovalRequest, Verdict};
use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};

/// Opaque handle identifying a connected approver.
pub type ApproverId = u64;

#[derive(Default)]
struct State {
    pending: HashMap<u64, oneshot::Sender<Verdict>>,
    approvers: HashMap<ApproverId, mpsc::UnboundedSender<ApprovalRequest>>,
    next_approver: ApproverId,
}

/// The shared approval broker. Construct once, wire into the executor (as an
/// `Arc<dyn ApprovalBroker>`) and the control transport (which calls
/// [`Broker::subscribe`] / [`Broker::resolve`]).
pub struct Broker {
    state: Mutex<State>,
    timeout: Duration,
}

impl Broker {
    pub fn new(timeout: Duration) -> Self {
        Self {
            state: Mutex::new(State::default()),
            timeout,
        }
    }

    /// Register an approver. Returns its id plus a receiver of every future
    /// pending request. Drop the receiver (or call [`Broker::unsubscribe`]) to
    /// disconnect.
    pub fn subscribe(&self) -> (ApproverId, mpsc::UnboundedReceiver<ApprovalRequest>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut s = self.state.lock().unwrap();
        let id = s.next_approver;
        s.next_approver += 1;
        s.approvers.insert(id, tx);
        (id, rx)
    }

    /// Disconnect an approver.
    pub fn unsubscribe(&self, id: ApproverId) {
        self.state.lock().unwrap().approvers.remove(&id);
    }

    /// Answer a pending request. The first resolve wins; an unknown id (already
    /// answered, timed out, or never existed) is a no-op.
    pub fn resolve(&self, request_id: u64, verdict: Verdict) {
        if let Some(tx) = self.state.lock().unwrap().pending.remove(&request_id) {
            let _ = tx.send(verdict);
        }
    }
}

#[async_trait]
impl ApprovalBroker for Broker {
    async fn request(&self, req: ApprovalRequest) -> Verdict {
        let (tx, rx) = oneshot::channel();
        let id = req.id;
        {
            let mut s = self.state.lock().unwrap();
            // Prune dead approver senders; if none remain, fail closed.
            s.approvers.retain(|_, t| !t.is_closed());
            if s.approvers.is_empty() {
                return Verdict::Deny;
            }
            s.pending.insert(id, tx);
            for t in s.approvers.values() {
                let _ = t.send(req.clone());
            }
        }
        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(v)) => v,
            // Timed out, or the oneshot sender dropped without a verdict.
            _ => {
                self.state.lock().unwrap().pending.remove(&id);
                Verdict::Deny
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentd_types::{ApprovalKind, ApprovalRequest, Verdict};
    use std::sync::Arc;

    fn req(id: u64) -> ApprovalRequest {
        ApprovalRequest {
            id,
            kind: ApprovalKind::MissingGrant,
            action: "a".into(),
            tool: Some("t".into()),
            requires: vec!["p".into()],
            missing: vec!["p".into()],
            reason: "r".into(),
            caller: Default::default(),
        }
    }

    #[tokio::test]
    async fn no_approver_denies() {
        let b = Broker::new(Duration::from_millis(50));
        assert_eq!(b.request(req(1)).await, Verdict::Deny);
    }

    #[tokio::test]
    async fn approver_allow_once_roundtrips() {
        let b = Arc::new(Broker::new(Duration::from_secs(5)));
        let (_id, mut rx) = b.subscribe();
        let b2 = b.clone();
        let h = tokio::spawn(async move { b2.request(req(7)).await });
        let pushed = rx.recv().await.unwrap();
        assert_eq!(pushed.id, 7);
        b.resolve(7, Verdict::AllowOnce);
        assert_eq!(h.await.unwrap(), Verdict::AllowOnce);
    }

    #[tokio::test]
    async fn timeout_denies_when_no_resolve() {
        let b = Arc::new(Broker::new(Duration::from_millis(50)));
        let (_id, _rx) = b.subscribe(); // approver present but never answers
        assert_eq!(b.request(req(3)).await, Verdict::Deny);
    }

    #[tokio::test]
    async fn resolve_unknown_id_is_noop() {
        let b = Broker::new(Duration::from_millis(50));
        b.resolve(999, Verdict::AllowForever); // must not panic
    }
}
