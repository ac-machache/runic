//! `AgentState` — the agent's working state for one conversation.
//!
//! The better state, synthesized:
//! - **event-sourced log** (`events: Vec<SessionEvent>`) — runic's design;
//!   replayable, auditable, non-destructive compaction.
//! - **structured messages** — `runic_types::Message` (`Vec<ContentBlock>`),
//!   the model copied from OpenFang.
//! - **session metadata** — `label`, `context_window_tokens` (OpenFang).
//! - keyed by **`(user_id, session_id)`**.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use runic_types::Message;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};

use crate::event::SessionEvent;

/// Capacity of the broadcast channel that fans `SessionEvent`s out to
/// subscribers (persisters, observers). A subscriber that falls this far
/// behind gets `RecvError::Lagged(n)`.
pub const EVENT_BROADCAST_CAPACITY: usize = 1024;

/// Generate a fresh run id.
pub fn new_run_id() -> String {
    format!("r-{}", uuid::Uuid::new_v4().simple())
}

// ─── Build-time runtime context (typed handles) ──────────────────────────────

/// A small typed bag for build-time, per-thread handles (a DB pool, an
/// approver, …) keyed by `TypeId`. Distinct from the per-run `config` map.
#[derive(Default, Clone)]
pub struct RunTimeContext {
    ctx: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}

impl RunTimeContext {
    pub fn insert<T: 'static + Send + Sync>(&mut self, value: T) {
        self.ctx.insert(TypeId::of::<T>(), Arc::new(value));
    }

    pub fn insert_arc<T: 'static + Send + Sync>(&mut self, value: Arc<T>) {
        self.ctx.insert(TypeId::of::<T>(), value);
    }

    pub fn get<T: 'static + Send + Sync>(&self) -> Option<Arc<T>> {
        self.ctx
            .get(&TypeId::of::<T>())
            .and_then(|v| v.clone().downcast::<T>().ok())
    }

    pub fn snapshot(&self) -> HashMap<TypeId, Arc<dyn Any + Send + Sync>> {
        self.ctx.clone()
    }
}

impl std::fmt::Debug for RunTimeContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunTimeContext")
            .field("entries", &self.ctx.len())
            .finish()
    }
}

// ─── Agent state ─────────────────────────────────────────────────────────────

/// The agent's state for one `(user_id, session_id)` conversation.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentState {
    /// Owning user (the tenant axis).
    pub user_id: String,
    /// This conversation's id.
    pub session_id: String,
    /// Optional human-readable label (OpenFang).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The base system prompt the agent was built with.
    pub system_prompt: String,
    /// Estimated tokens this conversation occupies in the context window
    /// (OpenFang) — a cheap budget signal for compaction.
    #[serde(default)]
    pub context_window_tokens: u64,
    /// The event log — the source of truth. Messages are derived from it.
    pub events: Vec<SessionEvent>,

    /// Build-time typed handles (DB pool, approver, …). Not persisted.
    #[serde(skip, default)]
    pub runtime: RunTimeContext,

    /// Per-run open config map (user_id, allow_web_search, …). Set fresh each
    /// run and overwritten, so it never leaks across runs. Not persisted.
    #[serde(skip, default)]
    pub config: serde_json::Map<String, serde_json::Value>,

    /// Broadcast sender; `push_event` fans every event out to subscribers in
    /// addition to appending. `None` on a deserialized (replay) state.
    #[serde(skip, default)]
    events_tx: Option<broadcast::Sender<SessionEvent>>,

    // Lossless sink for the durable persister, fanned alongside `events_tx`.
    #[serde(skip, default)]
    persist_tx: Option<mpsc::UnboundedSender<SessionEvent>>,

    // Folded from `events` in `push_event` so the turn build skips re-scanning.
    #[serde(skip, default)]
    messages: Vec<Message>,
}

impl AgentState {
    /// Fresh state for `(user_id, session_id)`.
    pub fn new(
        user_id: impl Into<String>,
        session_id: impl Into<String>,
        system_prompt: impl Into<String>,
    ) -> Self {
        Self {
            user_id: user_id.into(),
            session_id: session_id.into(),
            label: None,
            system_prompt: system_prompt.into(),
            context_window_tokens: 0,
            events: Vec::new(),
            runtime: RunTimeContext::default(),
            config: serde_json::Map::new(),
            events_tx: None,
            persist_tx: None,
            messages: Vec::new(),
        }
    }

    /// Read a per-run config value.
    pub fn config(&self, key: &str) -> Option<&serde_json::Value> {
        self.config.get(key)
    }

    /// Install a broadcast sender so `push_event` fans out to subscribers.
    pub fn set_events_tx(&mut self, tx: broadcast::Sender<SessionEvent>) {
        self.events_tx = Some(tx);
    }

    /// Install the lossless persister sink — `push_event` fans every event here
    /// in addition to the (lossy) broadcast.
    pub fn set_persist_tx(&mut self, tx: mpsc::UnboundedSender<SessionEvent>) {
        self.persist_tx = Some(tx);
    }

    /// Subscribe to future events (None if no channel is installed).
    pub fn subscribe_events(&self) -> Option<broadcast::Receiver<SessionEvent>> {
        self.events_tx.as_ref().map(|tx| tx.subscribe())
    }

    /// Append an event, broadcasting it first. A full channel drops the
    /// slowest subscriber's oldest event (it sees `Lagged` on next recv).
    pub fn push_event(&mut self, ev: SessionEvent) {
        if let Some(tx) = &self.events_tx {
            let _ = tx.send(ev.clone());
        }
        if let Some(tx) = &self.persist_tx {
            let _ = tx.send(ev.clone());
        }
        match &ev {
            SessionEvent::Message { msg, .. } => self.messages.push(msg.clone()),
            SessionEvent::StateSnapshot { messages, .. } => self.messages = messages.clone(),
            _ => {}
        }
        self.events.push(ev);
    }

    /// The provider-facing message list — `Message` events appended,
    /// `StateSnapshot` replacing history (compaction). Maintained in
    /// `push_event`, so this is a clone, not a re-fold.
    pub fn messages_for_provider(&self) -> Vec<Message> {
        self.messages.clone()
    }

    /// Grouped view of runs, derived from the log. Cheap, on demand.
    pub fn runs(&self) -> Vec<RunView<'_>> {
        let mut order: Vec<String> = Vec::new();
        for ev in &self.events {
            let id = ev.run_id();
            if !order.iter().any(|s| s == id) {
                order.push(id.to_string());
            }
        }
        order
            .into_iter()
            .map(|id| {
                let events: Vec<&SessionEvent> =
                    self.events.iter().filter(|e| e.run_id() == id).collect();
                let started_at = events.iter().find_map(|e| match e {
                    SessionEvent::RunStart { at, .. } => Some(*at),
                    _ => None,
                });
                let ended_at = events.iter().find_map(|e| match e {
                    SessionEvent::RunEnd { at, .. } => Some(*at),
                    _ => None,
                });
                RunView {
                    id,
                    started_at,
                    ended_at,
                    events,
                }
            })
            .collect()
    }

    /// The most recent run with no `RunEnd` yet (in flight).
    pub fn current_run(&self) -> Option<RunView<'_>> {
        self.runs().into_iter().find(|r| r.ended_at.is_none())
    }

    /// Most recent assistant text in the log (e.g. the final answer).
    pub fn last_assistant_text(&self) -> Option<String> {
        for ev in self.events.iter().rev() {
            if let SessionEvent::Message { msg, .. } = ev
                && msg.role == runic_types::Role::Assistant
            {
                let t = msg.content.text_content();
                if !t.is_empty() {
                    return Some(t);
                }
            }
        }
        None
    }
}

// ─── Run view (derived) ──────────────────────────────────────────────────────

/// A read-only slice of the log for one run.
pub struct RunView<'a> {
    pub id: String,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub events: Vec<&'a SessionEvent>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(text: &str, user: bool) -> SessionEvent {
        let msg = if user {
            Message::user(text)
        } else {
            Message::assistant(text)
        };
        SessionEvent::Message {
            run_id: "r".into(),
            msg,
            at: Utc::now(),
        }
    }

    #[test]
    fn push_event_folds_messages_and_skips_non_messages() {
        let mut state = AgentState::new("u", "s", "sys");
        state.push_event(SessionEvent::RunStart {
            run_id: "r".into(),
            at: Utc::now(),
        });
        state.push_event(message("hello", true));
        state.push_event(message("hi there", false));

        let msgs = state.messages_for_provider();
        assert_eq!(msgs.len(), 2);
        assert_eq!(state.events.len(), 3);
    }

    #[test]
    fn state_snapshot_replaces_the_message_view() {
        let mut state = AgentState::new("u", "s", "sys");
        state.push_event(message("old one", true));
        state.push_event(message("old two", false));
        state.push_event(SessionEvent::StateSnapshot {
            run_id: "r".into(),
            messages: vec![Message::user("compacted")],
            system_prompt: "sys".into(),
            reason: "compaction".into(),
            at: Utc::now(),
        });
        state.push_event(message("after", false));

        let msgs = state.messages_for_provider();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].content.text_content(), "compacted");
        assert_eq!(msgs[1].content.text_content(), "after");
    }

    #[test]
    fn view_matches_a_full_fold_after_replay_style_pushes() {
        let mut state = AgentState::new("u", "s", "sys");
        for ev in [
            SessionEvent::RunStart {
                run_id: "r".into(),
                at: Utc::now(),
            },
            message("a", true),
            message("b", false),
            message("c", true),
        ] {
            state.push_event(ev);
        }
        let folded: Vec<_> = state
            .events
            .iter()
            .filter_map(|e| match e {
                SessionEvent::Message { msg, .. } => Some(msg.content.text_content()),
                _ => None,
            })
            .collect();
        let view: Vec<_> = state
            .messages_for_provider()
            .iter()
            .map(|m| m.content.text_content())
            .collect();
        assert_eq!(view, folded);
    }
}
