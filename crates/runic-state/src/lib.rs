//! `runic-state` — the agent's working state object.
//!
//! Synthesis of the two state designs that beat ZeroClaw's flat
//! `Vec<ChatMessage>`:
//! - **event-sourced log** (runic) — replayable, auditable, non-destructive
//!   compaction;
//! - **structured `Message`** (`runic_types`, copied from OpenFang);
//! - **session metadata** — `label` (OpenFang);
//! - keyed by **`(user_id, session_id)`**.

pub mod child;
pub mod event;
pub mod external;
pub mod state;
pub mod stats;
pub mod tasks;
pub mod timeline;

pub use child::{ChildPersistence, ChildPersistenceHandle, ChildSink};
pub use event::{
    AgentEvent, AuditStamp, ChildPersistenceStatus, DelegationMode, DelegationStatus,
    HookLifecycle, RunEndStatus, RunOutcome, SessionEvent, ToolStatus,
};
pub use external::ExternalEvents;
pub use state::{
    AgentState, EVENT_BROADCAST_CAPACITY, InvalidStateKey, MAX_STATE_KEY_BYTES, PersistSink,
    RunTimeContext, RunView, new_run_id, validate_state_key,
};
pub use stats::{MAX_TRACKED_MODELS, MAX_TRACKED_TOOLS, ThreadStats, ToolStat};
pub use tasks::{TaskRecord, TaskStatus};
pub use timeline::{DelegationTrace, RunTrace, ToolTrace, TraceStatus, TurnTrace};

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
        assert!(s.events().is_empty());
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
            stats: None,
            open_tasks: None,
            data: None,
            at: Utc::now(),
        });
        push_msg(&mut s, "r1", Message::user("c"));
        let m = s.messages_for_provider();
        // [a,b] replaced by [compacted], then c appended → [compacted, c]
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].content.text_content(), "compacted");
        assert_eq!(m[1].content.text_content(), "c");
        // …and pre-snapshot events leave RAM (the store keeps the full log):
        assert_eq!(s.events().len(), 2);
    }

    #[test]
    fn runs_group_by_id_and_current_run_is_unclosed() {
        let mut s = AgentState::new("u1", "sess", "");
        s.push_event(SessionEvent::RunStart {
            run_id: "a".into(),
            agent: None,
            audit: None,
            at: Utc::now(),
        });
        s.push_event(SessionEvent::RunEnd {
            run_id: "a".into(),
            status: RunEndStatus::Completed,
            outcome: RunOutcome::default(),
            at: Utc::now(),
        });
        s.push_event(SessionEvent::RunStart {
            run_id: "b".into(),
            agent: None,
            audit: None,
            at: Utc::now(),
        });
        let runs = s.runs();
        let ids: Vec<&str> = runs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
        assert_eq!(s.current_run().unwrap().id, "b");
    }

    #[test]
    fn mid_run_compaction_keeps_the_in_flight_run_visible() {
        let mut s = AgentState::new("u1", "sess", "");
        s.push_event(SessionEvent::RunStart {
            run_id: "old".into(),
            agent: None,
            audit: None,
            at: Utc::now(),
        });
        push_msg(&mut s, "old", Message::user("ancient"));
        s.push_event(SessionEvent::RunEnd {
            run_id: "old".into(),
            status: RunEndStatus::Completed,
            outcome: RunOutcome::default(),
            at: Utc::now(),
        });
        s.push_event(SessionEvent::RunStart {
            run_id: "live".into(),
            agent: None,
            audit: None,
            at: Utc::now(),
        });
        push_msg(&mut s, "live", Message::user("now"));
        s.push_event(SessionEvent::StateSnapshot {
            run_id: "live".into(),
            messages: vec![Message::assistant("summary")],
            system_prompt: String::new(),
            reason: "compaction".into(),
            stats: None,
            open_tasks: None,
            data: None,
            at: Utc::now(),
        });

        assert_eq!(s.current_run().unwrap().id, "live");
        assert!(
            !s.events()
                .iter()
                .any(|e| matches!(e, SessionEvent::RunEnd { run_id, .. } if run_id == "old"))
        );
        assert_eq!(s.stats().runs, 2, "runs count attempts, from RunStart");
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
            agent: None,
            audit: None,
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
