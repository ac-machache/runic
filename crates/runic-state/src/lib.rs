//! `runic-state` — the agent's working state object.
//!
//! Synthesis of the two state designs that beat ZeroClaw's flat
//! `Vec<ChatMessage>`:
//! - **event-sourced log** (runic) — replayable, auditable, non-destructive
//!   compaction;
//! - **structured `Message`** (`runic_types`, copied from OpenFang);
//! - **session metadata** — `label`, `context_window_tokens` (OpenFang);
//! - keyed by **`(user_id, session_id)`**.

pub mod event;
pub mod state;

pub use event::{HookLifecycle, RunOutcome, SessionEvent};
pub use state::{AgentState, EVENT_BROADCAST_CAPACITY, RunTimeContext, RunView, new_run_id};

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use runic_types::Message;

    fn push_msg(s: &mut AgentState, run_id: &str, msg: Message) {
        s.push_event(SessionEvent::Message {
            run_id: run_id.into(),
            msg,
            at: Utc::now(),
        });
    }

    #[test]
    fn new_starts_empty_and_keyed() {
        let s = AgentState::new("u1", "sess-1", "you are a bot");
        assert_eq!(s.user_id, "u1");
        assert_eq!(s.session_id, "sess-1");
        assert_eq!(s.system_prompt, "you are a bot");
        assert!(s.events.is_empty());
    }

    #[test]
    fn messages_for_provider_folds_in_order() {
        let mut s = AgentState::new("u1", "sess", "");
        push_msg(&mut s, "r1", Message::user("hi"));
        push_msg(&mut s, "r1", Message::assistant("hello"));
        let m = s.messages_for_provider();
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].content.text_content(), "hi");
        assert_eq!(m[1].content.text_content(), "hello");
    }

    #[test]
    fn state_snapshot_is_non_destructive_compaction() {
        let mut s = AgentState::new("u1", "sess", "");
        push_msg(&mut s, "r1", Message::user("a"));
        push_msg(&mut s, "r1", Message::user("b"));
        // Compaction = a snapshot event, not an in-place trim.
        s.push_event(SessionEvent::StateSnapshot {
            run_id: "r1".into(),
            messages: vec![Message::user("compacted")],
            system_prompt: String::new(),
            reason: "trim".into(),
            at: Utc::now(),
        });
        push_msg(&mut s, "r1", Message::user("c"));
        let m = s.messages_for_provider();
        // [a,b] replaced by [compacted], then c appended → [compacted, c]
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].content.text_content(), "compacted");
        assert_eq!(m[1].content.text_content(), "c");
        // …but the raw events are all still there (nothing lost):
        assert_eq!(s.events.len(), 4);
    }

    #[test]
    fn runs_group_by_id_and_current_run_is_unclosed() {
        let mut s = AgentState::new("u1", "sess", "");
        s.push_event(SessionEvent::RunStart {
            run_id: "a".into(),
            at: Utc::now(),
        });
        s.push_event(SessionEvent::RunEnd {
            run_id: "a".into(),
            outcome: RunOutcome::default(),
            at: Utc::now(),
        });
        s.push_event(SessionEvent::RunStart {
            run_id: "b".into(),
            at: Utc::now(),
        });
        let runs = s.runs();
        let ids: Vec<&str> = runs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
        assert_eq!(s.current_run().unwrap().id, "b");
    }

    #[test]
    fn last_assistant_text_returns_latest() {
        let mut s = AgentState::new("u1", "sess", "");
        push_msg(&mut s, "r1", Message::user("q"));
        push_msg(&mut s, "r1", Message::assistant("the answer"));
        assert_eq!(s.last_assistant_text().as_deref(), Some("the answer"));
    }

    #[test]
    fn push_event_broadcasts_to_subscribers() {
        let mut s = AgentState::new("u1", "sess", "");
        let (tx, _keep) = tokio::sync::broadcast::channel(16);
        s.set_events_tx(tx);
        let mut rx = s.subscribe_events().expect("channel installed");
        s.push_event(SessionEvent::RunStart {
            run_id: "r1".into(),
            at: Utc::now(),
        });
        assert!(rx.try_recv().is_ok(), "subscriber should receive the event");
    }

    #[test]
    fn runtime_context_round_trips_typed_handles() {
        #[derive(Debug, PartialEq)]
        struct DbPool(u64);
        let mut rt = RunTimeContext::default();
        rt.insert(DbPool(7));
        assert_eq!(*rt.get::<DbPool>().unwrap(), DbPool(7));
        assert!(rt.get::<String>().is_none());
    }
}
