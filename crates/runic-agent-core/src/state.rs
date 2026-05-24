use chrono::{DateTime, Utc};
use runic_message_types::Message;
use serde::{Deserialize, Serialize};
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

use crate::agent::RunOutcome;

// ─── Runtime context ─────────────────────────────────────────────────────────

#[derive(Default, Clone)]
pub struct RunTimeContext {
    ctx: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}

impl RunTimeContext {
    pub fn insert<T: 'static + Send + Sync>(&mut self, value: T) {
        self.ctx.insert(TypeId::of::<T>(), Arc::new(value));
    }

    /// Like [`Self::insert`] but accepts an already-shared `Arc<T>` so
    /// callers can keep a clone for themselves. Useful when a single
    /// instance must be shared with something outside the agent (e.g.
    /// a `BackgroundTaskReminder` that needs the same `BackgroundManager`
    /// the agent uses).
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

// ─── Hook lifecycle ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HookLifecycle {
    BeforeAgent,
    AfterAgent,
    BeforeModel,
    AfterModel,
    BeforeTool,
    AfterTool,
}

// ─── Session events ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum SessionEvent {
    RunStart {
        run_id: String,
        at: DateTime<Utc>,
    },
    RunEnd {
        run_id: String,
        outcome: RunOutcome,
        at: DateTime<Utc>,
    },
    Message {
        run_id: String,
        msg: Message,
        at: DateTime<Utc>,
    },
    TurnBoundary {
        run_id: String,
        at: DateTime<Utc>,
    },
    HookRan {
        run_id: String,
        hook: String,
        lifecycle: HookLifecycle,
        note: Option<String>,
        at: DateTime<Utc>,
    },
    StateSnapshot {
        run_id: String,
        messages: Vec<Message>,
        system_prompt: String,
        reason: String,
        at: DateTime<Utc>,
    },
}

// ─── Agent state ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentState {
    pub session_id: String,
    pub system_prompt: String,
    pub events: Vec<SessionEvent>,

    #[serde(skip, default)]
    pub runtime: RunTimeContext,
}

impl AgentState {
    pub fn new(session_id: impl Into<String>, system_prompt: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            system_prompt: system_prompt.into(),
            events: Vec::new(),
            runtime: RunTimeContext::default(),
        }
    }

    pub fn push_event(&mut self, ev: SessionEvent) {
        self.events.push(ev);
    }

    /// Fold events into the message list the provider sees.
    ///
    /// `StateSnapshot` replaces the accumulated history (handles trim/rewrite).
    /// Plain `Message` events append.
    pub fn messages_for_provider(&self) -> Vec<Message> {
        let mut msgs: Vec<Message> = Vec::new();
        for ev in &self.events {
            match ev {
                SessionEvent::Message { msg, .. } => msgs.push(msg.clone()),
                SessionEvent::StateSnapshot { messages, .. } => msgs = messages.clone(),
                _ => {}
            }
        }
        msgs
    }

    /// Grouped view of runs, derived from events. Cheap, computed on demand.
    pub fn runs(&self) -> Vec<RunView<'_>> {
        let mut order: Vec<String> = Vec::new();
        for ev in &self.events {
            let id = event_run_id(ev);
            if !order.iter().any(|s| s == id) {
                order.push(id.to_string());
            }
        }
        order
            .into_iter()
            .map(|id| {
                let events: Vec<&SessionEvent> = self
                    .events
                    .iter()
                    .filter(|e| event_run_id(e) == id)
                    .collect();
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

    /// The most recent run that has no `RunEnd` yet (i.e. still in flight).
    pub fn current_run(&self) -> Option<RunView<'_>> {
        self.runs().into_iter().find(|r| r.ended_at.is_none())
    }

    /// Most recent assistant `ContentBlock::Text` in the event log.
    /// Used by `SubagentTool` to extract the child's final answer.
    pub fn last_assistant_text(&self) -> Option<String> {
        for ev in self.events.iter().rev() {
            if let SessionEvent::Message { msg, .. } = ev
                && msg.role == runic_message_types::Role::Assistant
            {
                for block in &msg.content {
                    if let runic_message_types::ContentBlock::Text { text, .. } = block {
                        return Some(text.clone());
                    }
                }
            }
        }
        None
    }
}

// ─── Run view (derived) ──────────────────────────────────────────────────────

pub struct RunView<'a> {
    pub id: String,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub events: Vec<&'a SessionEvent>,
}

fn event_run_id(ev: &SessionEvent) -> &str {
    use SessionEvent::*;
    match ev {
        RunStart { run_id, .. }
        | RunEnd { run_id, .. }
        | Message { run_id, .. }
        | TurnBoundary { run_id, .. }
        | HookRan { run_id, .. }
        | StateSnapshot { run_id, .. } => run_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::TokenUsage;

    // ─── RunTimeContext ─────────────────────────────────────────────────────

    #[derive(Debug, PartialEq)]
    struct UserId(u64);

    #[derive(Debug, PartialEq)]
    struct ApiToken(String);

    #[test]
    fn runtime_context_round_trips_typed_values() {
        let mut rt = RunTimeContext::default();
        rt.insert(UserId(42));
        rt.insert(ApiToken("sk-test".into()));

        let user = rt.get::<UserId>().unwrap();
        let token = rt.get::<ApiToken>().unwrap();

        assert_eq!(*user, UserId(42));
        assert_eq!(*token, ApiToken("sk-test".into()));
    }

    #[test]
    fn runtime_context_returns_none_for_missing_type() {
        let rt = RunTimeContext::default();
        assert!(rt.get::<UserId>().is_none());
    }

    #[test]
    fn runtime_context_returns_none_for_wrong_type() {
        let mut rt = RunTimeContext::default();
        rt.insert(UserId(1));
        // UserId is registered, ApiToken is not — must be None.
        assert!(rt.get::<ApiToken>().is_none());
    }

    #[test]
    fn runtime_context_overwrites_same_type() {
        let mut rt = RunTimeContext::default();
        rt.insert(UserId(1));
        rt.insert(UserId(2));
        assert_eq!(*rt.get::<UserId>().unwrap(), UserId(2));
    }

    #[test]
    fn runtime_context_snapshot_preserves_entries() {
        let mut rt = RunTimeContext::default();
        rt.insert(UserId(7));
        let snap = rt.snapshot();
        assert_eq!(snap.len(), 1);
        assert!(snap.contains_key(&TypeId::of::<UserId>()));
    }

    // ─── SessionEvent serde ─────────────────────────────────────────────────

    #[test]
    fn session_event_serializes_with_kind_tag() {
        let ev = SessionEvent::RunStart {
            run_id: "abc".into(),
            at: Utc::now(),
        };
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["kind"], "RunStart");
        assert_eq!(json["run_id"], "abc");
    }

    #[test]
    fn session_event_round_trips_through_json() {
        let ev = SessionEvent::RunEnd {
            run_id: "r1".into(),
            outcome: RunOutcome {
                total_turns: 3,
                stop_reason: Some("end_turn".into()),
                usage: TokenUsage::default(),
            },
            at: Utc::now(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        let parsed: SessionEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            SessionEvent::RunEnd { run_id, outcome, .. } => {
                assert_eq!(run_id, "r1");
                assert_eq!(outcome.total_turns, 3);
                assert_eq!(outcome.stop_reason.as_deref(), Some("end_turn"));
            }
            other => panic!("expected RunEnd, got {other:?}"),
        }
    }

    // ─── AgentState ─────────────────────────────────────────────────────────

    fn user_msg(text: &str) -> Message {
        Message::user(text)
    }

    fn assistant_msg(text: &str) -> Message {
        Message::assistant_text(text)
    }

    fn push_msg(state: &mut AgentState, run_id: &str, msg: Message) {
        state.push_event(SessionEvent::Message {
            run_id: run_id.into(),
            msg,
            at: Utc::now(),
        });
    }

    #[test]
    fn agent_state_new_starts_empty() {
        let s = AgentState::new("sess", "you are a bot");
        assert_eq!(s.session_id, "sess");
        assert_eq!(s.system_prompt, "you are a bot");
        assert!(s.events.is_empty());
    }

    #[test]
    fn messages_for_provider_collects_message_events_in_order() {
        let mut s = AgentState::new("sess", "");
        push_msg(&mut s, "r1", user_msg("hi"));
        push_msg(&mut s, "r1", assistant_msg("hello"));
        push_msg(&mut s, "r1", user_msg("how are you"));

        let msgs = s.messages_for_provider();
        assert_eq!(msgs.len(), 3);
        match &msgs[0].content[0] {
            runic_message_types::ContentBlock::Text { text, .. } => assert_eq!(text, "hi"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn messages_for_provider_skips_non_message_events() {
        let mut s = AgentState::new("sess", "");
        s.push_event(SessionEvent::RunStart {
            run_id: "r1".into(),
            at: Utc::now(),
        });
        push_msg(&mut s, "r1", user_msg("hi"));
        s.push_event(SessionEvent::TurnBoundary {
            run_id: "r1".into(),
            at: Utc::now(),
        });
        s.push_event(SessionEvent::HookRan {
            run_id: "r1".into(),
            hook: "X".into(),
            lifecycle: HookLifecycle::BeforeModel,
            note: None,
            at: Utc::now(),
        });
        push_msg(&mut s, "r1", assistant_msg("hello"));

        let msgs = s.messages_for_provider();
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn state_snapshot_replaces_accumulated_history() {
        let mut s = AgentState::new("sess", "");
        push_msg(&mut s, "r1", user_msg("a"));
        push_msg(&mut s, "r1", user_msg("b"));
        push_msg(&mut s, "r1", user_msg("c"));

        s.push_event(SessionEvent::StateSnapshot {
            run_id: "r1".into(),
            messages: vec![user_msg("compacted")],
            system_prompt: "".into(),
            reason: "trim".into(),
            at: Utc::now(),
        });

        push_msg(&mut s, "r1", user_msg("d"));

        let msgs = s.messages_for_provider();
        // Snapshot replaced [a,b,c] with [compacted], then d was appended.
        assert_eq!(msgs.len(), 2);
        match &msgs[0].content[0] {
            runic_message_types::ContentBlock::Text { text, .. } => {
                assert_eq!(text, "compacted")
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn runs_groups_events_by_run_id_in_first_seen_order() {
        let mut s = AgentState::new("sess", "");
        s.push_event(SessionEvent::RunStart {
            run_id: "a".into(),
            at: Utc::now(),
        });
        s.push_event(SessionEvent::RunStart {
            run_id: "b".into(),
            at: Utc::now(),
        });
        push_msg(&mut s, "a", user_msg("first"));
        push_msg(&mut s, "b", user_msg("second"));

        let runs = s.runs();
        let ids: Vec<&str> = runs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
        assert_eq!(runs[0].events.len(), 2); // RunStart + Message
        assert_eq!(runs[1].events.len(), 2);
    }

    #[test]
    fn current_run_returns_the_unclosed_run() {
        let mut s = AgentState::new("sess", "");
        s.push_event(SessionEvent::RunStart {
            run_id: "a".into(),
            at: Utc::now(),
        });
        s.push_event(SessionEvent::RunEnd {
            run_id: "a".into(),
            outcome: RunOutcome {
                total_turns: 1,
                stop_reason: None,
                usage: TokenUsage::default(),
            },
            at: Utc::now(),
        });
        s.push_event(SessionEvent::RunStart {
            run_id: "b".into(),
            at: Utc::now(),
        });

        let current = s.current_run().expect("there should be one in-flight run");
        assert_eq!(current.id, "b");
        assert!(current.ended_at.is_none());
    }

    #[test]
    fn current_run_is_none_when_all_runs_ended() {
        let mut s = AgentState::new("sess", "");
        s.push_event(SessionEvent::RunStart {
            run_id: "a".into(),
            at: Utc::now(),
        });
        s.push_event(SessionEvent::RunEnd {
            run_id: "a".into(),
            outcome: RunOutcome {
                total_turns: 1,
                stop_reason: None,
                usage: TokenUsage::default(),
            },
            at: Utc::now(),
        });

        assert!(s.current_run().is_none());
    }
}
