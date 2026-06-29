//! Human-in-the-loop over HTTP, on the new [`runic_tool::HumanInterface`].
//!
//! When a run calls the `ask_user` tool, the tool reaches the per-run
//! [`HumanChannel`] (installed on the `RunContext`). The channel:
//!
//!   1. registers a one-shot in the [`HumanHub`] under a fresh `ask_id`,
//!   2. emits an `ask_required` event onto this run's live SSE stream,
//!   3. parks on the one-shot until `POST /threads/:id/asks/:ask_id`
//!      calls [`HumanHub::resolve`], firing it.
//!
//! `escalate_to_human` is fire-and-forget: it emits an `escalated` event and
//! returns immediately. A timeout backstops the park so a vanished client
//! can't wedge the run forever.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};

use runic_tool::HumanInterface;

use crate::wire::WireEvent;

/// How long `ask_user` waits for an answer before giving up.
const ASK_TIMEOUT: Duration = Duration::from_secs(600);

/// A parked ask: who it belongs to (so an answer can't cross threads) plus the
/// one-shot to fire. Run-id scoping follows once run ids are allocated before
/// the run starts.
struct PendingAsk {
    tenant: String,
    thread_id: String,
    tx: oneshot::Sender<String>,
}

/// Shared registry bridging parked HITL asks to the HTTP layer. Lives in
/// `AppState` so the answer endpoint can reach it.
#[derive(Default)]
pub struct HumanHub {
    /// Parked asks awaiting an answer, keyed by `ask_id`.
    pending: Mutex<HashMap<String, PendingAsk>>,
}

impl HumanHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a parked ask for `(tenant, thread_id)`, returning its id and the
    /// receiver to await.
    fn register(&self, tenant: &str, thread_id: &str) -> (String, oneshot::Receiver<String>) {
        let ask_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(
            ask_id.clone(),
            PendingAsk {
                tenant: tenant.to_string(),
                thread_id: thread_id.to_string(),
                tx,
            },
        );
        (ask_id, rx)
    }

    /// Deliver an answer to a parked ask — only when `tenant` + `thread_id`
    /// match the ask's scope. Returns false otherwise, or if nothing is pending
    /// under `ask_id` (already answered, timed out, never existed).
    pub fn resolve(&self, tenant: &str, thread_id: &str, ask_id: &str, answer: String) -> bool {
        let mut pending = self.pending.lock().unwrap();
        let scoped = matches!(
            pending.get(ask_id),
            Some(p) if p.tenant == tenant && p.thread_id == thread_id
        );
        if !scoped {
            return false;
        }
        match pending.remove(ask_id) {
            Some(p) => p.tx.send(answer).is_ok(),
            None => false,
        }
    }

    fn cancel(&self, ask_id: &str) {
        self.pending.lock().unwrap().remove(ask_id);
    }
}

/// A per-run [`HumanInterface`] that surfaces asks on the run's SSE stream and
/// resolves them through the [`HumanHub`]. The serve crate builds one of these
/// per run and installs it via `RunContext::with_human`.
pub struct HumanChannel {
    hub: Arc<HumanHub>,
    /// Sender into this run's SSE stream (merged alongside agent events).
    wire: mpsc::UnboundedSender<WireEvent>,
    tenant: String,
    thread_id: String,
}

impl HumanChannel {
    pub fn new(
        hub: Arc<HumanHub>,
        wire: mpsc::UnboundedSender<WireEvent>,
        tenant: impl Into<String>,
        thread_id: impl Into<String>,
    ) -> Self {
        Self {
            hub,
            wire,
            tenant: tenant.into(),
            thread_id: thread_id.into(),
        }
    }
}

#[async_trait]
impl HumanInterface for HumanChannel {
    async fn ask(&self, question: &str, context: Option<&str>) -> anyhow::Result<String> {
        let (ask_id, rx) = self.hub.register(&self.tenant, &self.thread_id);
        let _ = self.wire.send(WireEvent::AskRequired {
            ask_id: ask_id.clone(),
            question: question.to_string(),
            context: context.map(str::to_string),
        });
        match tokio::time::timeout(ASK_TIMEOUT, rx).await {
            Ok(Ok(answer)) => Ok(answer),
            _ => {
                self.hub.cancel(&ask_id);
                anyhow::bail!("ask timed out or no answer received")
            }
        }
    }

    async fn escalate(&self, reason: &str, detail: Option<&str>) -> anyhow::Result<()> {
        let _ = self.wire.send(WireEvent::Escalated {
            reason: reason.to_string(),
            detail: detail.map(str::to_string),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ask_parks_until_resolved_and_emits_event() {
        let hub = Arc::new(HumanHub::new());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let channel = HumanChannel::new(hub.clone(), tx, "t", "s");

        // Drive the ask concurrently; capture the ask_id from the emitted event.
        let h = hub.clone();
        let task = tokio::spawn(async move { channel.ask("proceed?", Some("ctx")).await });

        let evt = rx.recv().await.unwrap();
        let WireEvent::AskRequired {
            ask_id,
            question,
            context,
        } = evt
        else {
            panic!("expected ask_required, got {evt:?}");
        };
        assert_eq!(question, "proceed?");
        assert_eq!(context.as_deref(), Some("ctx"));
        assert!(h.resolve("t", "s", &ask_id, "yes".into()));

        let answer = task.await.unwrap().unwrap();
        assert_eq!(answer, "yes");
    }

    #[tokio::test]
    async fn resolve_rejects_wrong_scope() {
        let hub = Arc::new(HumanHub::new());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let channel = HumanChannel::new(hub.clone(), tx, "t", "s");
        let h = hub.clone();
        let task = tokio::spawn(async move { channel.ask("q?", None).await });

        let WireEvent::AskRequired { ask_id, .. } = rx.recv().await.unwrap() else {
            panic!();
        };
        // Wrong tenant / thread must not deliver.
        assert!(!h.resolve("other", "s", &ask_id, "x".into()));
        assert!(!h.resolve("t", "other", &ask_id, "x".into()));
        // Correct scope delivers.
        assert!(h.resolve("t", "s", &ask_id, "yes".into()));
        assert_eq!(task.await.unwrap().unwrap(), "yes");
    }

    #[test]
    fn resolve_unknown_ask_is_false() {
        let hub = HumanHub::new();
        assert!(!hub.resolve("t", "s", "ghost", "x".into()));
    }

    #[tokio::test]
    async fn escalate_emits_and_returns() {
        let hub = Arc::new(HumanHub::new());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let channel = HumanChannel::new(hub, tx, "t", "s");
        channel.escalate("blocked", None).await.unwrap();
        assert!(matches!(
            rx.recv().await.unwrap(),
            WireEvent::Escalated { .. }
        ));
    }
}
