//! Human-in-the-loop approval over HTTP.
//!
//! A HITL tool dispatch calls `Approver::review(...)`, which must block the
//! run until a human decides. In the REPL that's a stdin prompt; here it's
//! a round trip over the wire:
//!
//!   1. [`ChannelApprover::review`] (running inside the agent task) emits an
//!      `ApprovalRequired` event onto the thread's live SSE stream and parks
//!      on a oneshot keyed by `call_id`.
//!   2. The client renders the draft, the operator decides, and
//!      `POST /threads/:id/runs/:run_id/approvals/:call_id` calls
//!      [`ApprovalHub::submit_decision`], firing the oneshot.
//!   3. `review` returns the decision; the agent run resumes and streams on.
//!
//! The run task holds the thread's slot mutex throughout, so concurrent runs
//! on the same thread queue behind the pending approval (correct). A timeout
//! backstops the park so a vanished client can't brick the thread forever.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use runic_agent_core::{ApprovalRequest, Approver, UserDecision};
use tokio::sync::{mpsc, oneshot};

use crate::wire::WireEvent;

/// How long a HITL tool waits for a decision before auto-cancelling.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(600);

/// Shared registry bridging parked approvals to the HTTP layer. Lives in
/// `AppState` (so the decision endpoint can reach it) and is also handed to
/// the [`ChannelApprover`] installed in each server agent.
#[derive(Default)]
pub struct ApprovalHub {
    /// Per-thread sender into the currently-streaming run's SSE channel.
    wire: Mutex<HashMap<String, mpsc::Sender<WireEvent>>>,
    /// Parked approvals awaiting a decision, keyed by tool-call id.
    decisions: Mutex<HashMap<String, oneshot::Sender<UserDecision>>>,
}

impl ApprovalHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Point this thread's approvals at the given run's wire channel.
    pub fn set_wire(&self, thread: &str, tx: mpsc::Sender<WireEvent>) {
        self.wire.lock().unwrap().insert(thread.to_string(), tx);
    }

    pub fn clear_wire(&self, thread: &str) {
        self.wire.lock().unwrap().remove(thread);
    }

    fn wire_for(&self, thread: &str) -> Option<mpsc::Sender<WireEvent>> {
        self.wire.lock().unwrap().get(thread).cloned()
    }

    /// Deliver a decision to a parked approval. Returns false if no approval
    /// with that `call_id` is currently pending.
    pub fn submit_decision(&self, call_id: &str, decision: UserDecision) -> bool {
        let tx = self.decisions.lock().unwrap().remove(call_id);
        match tx {
            Some(tx) => tx.send(decision).is_ok(),
            None => false,
        }
    }

    fn register(&self, call_id: &str) -> oneshot::Receiver<UserDecision> {
        let (tx, rx) = oneshot::channel();
        self.decisions.lock().unwrap().insert(call_id.to_string(), tx);
        rx
    }

    fn cancel_pending(&self, call_id: &str) {
        self.decisions.lock().unwrap().remove(call_id);
    }
}

/// An [`Approver`] that resolves over the wire via an [`ApprovalHub`].
pub struct ChannelApprover {
    hub: Arc<ApprovalHub>,
}

impl ChannelApprover {
    pub fn new(hub: Arc<ApprovalHub>) -> Self {
        Self { hub }
    }
}

#[async_trait]
impl Approver for ChannelApprover {
    async fn review(&self, req: ApprovalRequest) -> UserDecision {
        let rx = self.hub.register(&req.call_id);

        // Surface the draft on the thread's live stream. If no stream is
        // attached (client already gone), we still park — the timeout will
        // release us.
        if let Some(wire) = self.hub.wire_for(&req.session_id) {
            let ev = WireEvent::ApprovalRequired {
                call_id: req.call_id.clone(),
                tool_name: req.tool_name.clone(),
                draft: serde_json::to_value(&req.draft).unwrap_or_default(),
            };
            let _ = wire.send(ev).await;
        }

        match tokio::time::timeout(APPROVAL_TIMEOUT, rx).await {
            Ok(Ok(decision)) => decision,
            _ => {
                self.hub.cancel_pending(&req.call_id);
                UserDecision::Cancel {
                    reason: "approval timed out or no decision received".into(),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn submit_decision_wakes_the_matching_parked_approval() {
        let hub = ApprovalHub::new();
        let mut rx = hub.register("call-1");

        let delivered = hub.submit_decision(
            "call-1",
            UserDecision::Submit { final_input: serde_json::json!({"ok": true}) },
        );
        assert!(delivered, "decision should reach the parked approval");

        match rx.try_recv() {
            Ok(UserDecision::Submit { final_input }) => assert_eq!(final_input["ok"], true),
            other => panic!("expected the submitted decision, got {other:?}"),
        }
    }

    #[test]
    fn submit_decision_for_unknown_call_id_is_false() {
        let hub = ApprovalHub::new();
        assert!(!hub.submit_decision("ghost", UserDecision::Cancel { reason: "x".into() }));
    }

    #[test]
    fn second_decision_for_same_call_id_finds_nothing() {
        // The first decision consumes the pending entry; a duplicate POST
        // (double-click) must not match a second time.
        let hub = ApprovalHub::new();
        let _rx = hub.register("call-1");
        assert!(hub.submit_decision("call-1", UserDecision::Cancel { reason: "a".into() }));
        assert!(!hub.submit_decision("call-1", UserDecision::Cancel { reason: "b".into() }));
    }
}
