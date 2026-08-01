//! `runic-state` — the agent's working state object.
//!
//! Synthesis of the two state designs that beat ZeroClaw's flat
//! `Vec<ChatMessage>`:
//! - **event-sourced log** (runic) — replayable, auditable, non-destructive
//!   compaction;
//! - **structured `Message`** (`runic_types`, copied from OpenFang);
//! - **session metadata** — `label` (OpenFang);
//! - keyed by **`(user_id, session_id)`**.

pub mod event;
pub mod state;
pub mod stats;
pub mod subsession;
pub mod tasks;

pub use event::{
    AgentEvent, AuditStamp, DelegationMode, DelegationStatus, HookLifecycle, PersistenceStatus,
    RunEndStatus, RunOutcome, ToolStatus,
};
pub use state::{
    AgentState, Deferral, Emitter, InvalidStateKey, MAX_STATE_KEY_BYTES, Reader, RunTimeContext,
    RunTotals, new_run_id, validate_state_key,
};
pub use stats::{MAX_TRACKED_MODELS, MAX_TRACKED_TOOLS, ThreadStats, ToolStat};
pub use subsession::{SubRun, SubSession};
pub use tasks::{TaskRecord, TaskStatus};

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use runic_types::Message;

    fn push_msg(s: &mut AgentState, run_id: &str, msg: Message) {
        s.emit(AgentEvent::Message {
            run_id: run_id.into(),
            msg,
            at: Utc::now(),
        });
    }

    fn run_started(run_id: &str) -> AgentEvent {
        AgentEvent::RunStarted {
            run_id: run_id.into(),
            agent: None,
            audit: None,
            at: Utc::now(),
        }
    }

    fn run_ended(run_id: &str) -> AgentEvent {
        AgentEvent::RunEnd {
            run_id: run_id.into(),
            status: RunEndStatus::Completed,
            outcome: RunOutcome::default(),
            at: Utc::now(),
        }
    }

    #[test]
    fn new_starts_empty_and_keyed() {
        let s = AgentState::new("u1", "sess-1", "you are a bot");
        assert_eq!(s.user_id, "u1");
        assert_eq!(s.session_id, "sess-1");
        assert_eq!(s.system_prompt, "you are a bot");
        assert!(s.messages_for_provider().is_empty());
        assert_eq!(s.current_run_id(), None);
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
    fn state_snapshot_replaces_the_message_view() {
        let mut s = AgentState::new("u1", "sess", "");
        push_msg(&mut s, "r1", Message::user("a"));
        push_msg(&mut s, "r1", Message::user("b"));
        s.emit(AgentEvent::StateSnapshot {
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
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].content.text_content(), "compacted");
        assert_eq!(m[1].content.text_content(), "c");
    }

    #[test]
    fn current_run_id_follows_the_unclosed_run() {
        let mut s = AgentState::new("u1", "sess", "");
        s.emit(run_started("a"));
        s.emit(run_ended("a"));
        assert_eq!(s.current_run_id(), None);
        s.emit(run_started("b"));
        assert_eq!(s.current_run_id(), Some("b"));
    }

    #[test]
    fn mid_run_compaction_keeps_the_in_flight_run_visible() {
        let mut s = AgentState::new("u1", "sess", "");
        s.emit(run_started("old"));
        push_msg(&mut s, "old", Message::user("ancient"));
        s.emit(run_ended("old"));
        s.emit(run_started("live"));
        push_msg(&mut s, "live", Message::user("now"));
        s.emit(AgentEvent::StateSnapshot {
            run_id: "live".into(),
            messages: vec![Message::assistant("summary")],
            system_prompt: String::new(),
            reason: "compaction".into(),
            stats: None,
            open_tasks: None,
            data: None,
            at: Utc::now(),
        });

        assert_eq!(s.current_run_id(), Some("live"));
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
    fn emit_reaches_the_installed_emitter() {
        #[derive(Debug, Default)]
        struct Spy(std::sync::Mutex<Vec<AgentEvent>>);
        impl Emitter for Spy {
            fn emit(&self, event: AgentEvent) {
                self.0.lock().unwrap().push(event);
            }
        }
        let spy = std::sync::Arc::new(Spy::default());
        let mut s = AgentState::new("u1", "sess", "");
        s.set_emitter(Some(spy.clone()));
        s.emit(run_started("r1"));
        assert_eq!(spy.0.lock().unwrap().len(), 1);
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
