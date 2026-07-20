//! Property tests for the SSE wire mapping — the contract the dev UI (and any
//! client) parses. Two invariants: every emitted `WireEvent`'s serde `type`
//! field equals its `event_kind()` (clients route on either), and the
//! `AgentEvent`/`SessionEvent` → wire mappings are total + never panic.

use chrono::{DateTime, Utc};
use proptest::prelude::*;

use runic_agent::AgentEvent;
use runic_serve::wire::{from_agent_event, from_session_event};
use runic_state::{RunEndStatus, RunOutcome, SessionEvent, ToolStatus};
use runic_types::{Message, TokenUsage};

fn ts() -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap()
}

fn agent_event() -> impl Strategy<Value = AgentEvent> {
    let at = ts();
    prop_oneof![
        "[a-z0-9-]{1,8}".prop_map(move |run_id| AgentEvent::RunStarted {
            run_id,
            agent: None,
            audit: None,
            at,
        }),
        "[a-z ]{0,20}".prop_map(AgentEvent::TextDelta),
        "[a-z ]{0,20}".prop_map(AgentEvent::ThinkingDelta),
        ("[a-z]{1,6}", "[a-z_]{1,10}").prop_map(move |(call_id, tool)| AgentEvent::ToolStarted {
            run_id: "r".into(),
            turn: 0,
            call_id,
            tool,
            input: serde_json::json!({"q": 1}),
            at,
        }),
        ("[a-z]{1,6}", "[a-z_]{1,10}", any::<bool>(), "[a-z ]{0,20}").prop_map(
            move |(call_id, tool, is_error, result)| AgentEvent::ToolFinished {
                run_id: "r".into(),
                turn: 0,
                call_id,
                tool,
                status: if is_error {
                    ToolStatus::ToolError
                } else {
                    ToolStatus::Ok
                },
                result: serde_json::Value::String(result),
                provenance: Vec::new(),
                duration_ms: 0,
                at,
            }
        ),
        (0u32..10, "[a-z_]{1,8}").prop_map(move |(turn, stop_reason)| AgentEvent::TurnEnd {
            run_id: "r".into(),
            turn,
            model: "m".into(),
            usage: TokenUsage::default(),
            model_ms: 0,
            stop_reason,
            at,
        }),
        Just(AgentEvent::RunEnd {
            run_id: "r".into(),
            status: RunEndStatus::Completed,
            outcome: RunOutcome::default(),
            at,
        }),
    ]
}

fn session_event() -> impl Strategy<Value = SessionEvent> {
    let at = ts();
    prop_oneof![
        "[a-z0-9-]{1,8}".prop_map(move |run_id| SessionEvent::RunStart {
            run_id,
            agent: None,
            audit: None,
            at
        }),
        (
            "[a-z0-9-]{1,8}",
            "[a-z ]{0,20}".prop_map(Message::assistant)
        )
            .prop_map(move |(run_id, msg)| SessionEvent::Message { run_id, msg, at }),
        "[a-z0-9-]{1,8}".prop_map(move |run_id| SessionEvent::TurnEnd {
            run_id,
            turn: 1,
            model: "m".into(),
            usage: runic_types::TokenUsage::default(),
            model_ms: 3,
            at,
        }),
        "[a-z0-9-]{1,8}".prop_map(move |run_id| SessionEvent::RunEnd {
            run_id,
            status: runic_state::RunEndStatus::Completed,
            outcome: RunOutcome::default(),
            at,
        }),
        "[a-z0-9-]{1,8}".prop_map(move |run_id| SessionEvent::StateSnapshot {
            run_id,
            messages: vec![],
            system_prompt: String::new(),
            reason: "c".into(),
            stats: None,
            open_tasks: None,
            data: None,
            at,
        }),
    ]
}

proptest! {
    /// Every wire event from an agent event carries a `type` field equal to its
    /// `event_kind()` — the discriminator clients route on.
    #[test]
    fn agent_event_type_matches_kind(e in agent_event()) {
        for w in from_agent_event(e) {
            let v = serde_json::to_value(&w).unwrap();
            prop_assert_eq!(v.get("type").and_then(|t| t.as_str()).unwrap(), w.event_kind());
        }
    }

    /// `from_session_event` returns `Some` only for client-visible kinds; the
    /// internal bookkeeping kinds are filtered to `None`. When `Some`, the
    /// `type`/`event_kind` invariant holds too.
    #[test]
    fn session_event_filters_and_tags(e in session_event()) {
        match from_session_event(e.clone()) {
            Some(w) => {
                let v = serde_json::to_value(&w).unwrap();
                prop_assert_eq!(v.get("type").and_then(|t| t.as_str()).unwrap(), w.event_kind());
                prop_assert!(
                    matches!(
                        e,
                        SessionEvent::RunStart { .. } | SessionEvent::RunEnd { .. } | SessionEvent::Message { .. } | SessionEvent::TurnEnd { .. }
                    ),
                    "Some came from a non-client-visible kind"
                );
            }
            None => prop_assert!(
                matches!(
                    e,
                    SessionEvent::HookFired { .. } | SessionEvent::StateSnapshot { .. }
                ),
                "None filtered a client-visible kind"
            ),
        }
    }
}

/// A finished run fans into exactly `[usage, done]` (the UI relies on both).
#[test]
fn run_completed_yields_usage_then_done() {
    let wires = from_agent_event(AgentEvent::RunEnd {
        run_id: "r".into(),
        status: RunEndStatus::Completed,
        outcome: RunOutcome::default(),
        at: ts(),
    });
    let kinds: Vec<&str> = wires.iter().map(|w| w.event_kind()).collect();
    assert_eq!(kinds, vec!["usage", "done"]);
}
