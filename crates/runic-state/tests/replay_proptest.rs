//! Property tests for the event-sourced replay — the harness's most
//! fundamental invariant (the message list the model sees is *derived* from the
//! event log, so the fold must hold for ANY event sequence).
//!
//! Generators build arbitrary `SessionEvent` sequences; the assertions
//! re-derive the expected outcome by an *independent* path (not a copy of
//! `messages_for_provider`'s fold), so a regression in the fold is caught.

use chrono::{DateTime, Utc};
use proptest::prelude::*;

use runic_state::{AgentState, RunOutcome, SessionEvent};
use runic_types::Message;

/// A fixed timestamp — replay doesn't depend on time, so determinism > realism.
fn ts() -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap()
}

fn run_id() -> impl Strategy<Value = String> {
    // A small id space so multi-run grouping is actually exercised.
    prop::sample::select(vec!["r-1", "r-2", "r-3"]).prop_map(String::from)
}

fn message() -> impl Strategy<Value = Message> {
    let text = "[a-z ]{0,24}";
    prop_oneof![
        text.prop_map(Message::user),
        text.prop_map(Message::assistant),
    ]
}

/// Any event variant. `at` is Copy, so each branch gets its own copy.
fn event() -> impl Strategy<Value = SessionEvent> {
    let at = ts();
    prop_oneof![
        run_id().prop_map(move |run_id| SessionEvent::RunStart {
            run_id,
            agent: None,
            at
        }),
        (run_id(), message()).prop_map(move |(run_id, msg)| SessionEvent::Message {
            run_id,
            msg,
            at
        }),
        run_id().prop_map(move |run_id| SessionEvent::TurnBoundary { run_id, at }),
        run_id().prop_map(move |run_id| SessionEvent::RunEnd {
            run_id,
            outcome: RunOutcome::default(),
            at,
        }),
        (run_id(), prop::collection::vec(message(), 0..4)).prop_map(move |(run_id, messages)| {
            SessionEvent::StateSnapshot {
                run_id,
                messages,
                system_prompt: String::new(),
                reason: "compact".into(),
                stats: None,
                open_tasks: None,
                data: None,
                at,
            }
        }),
        (run_id(), "[a-c]", 0..9u8).prop_map(move |(run_id, key, n)| {
            SessionEvent::StateUpdated {
                run_id,
                key,
                value: serde_json::json!(n),
                at,
            }
        }),
        (run_id(), "[a-z]{1,6}").prop_map(move |(run_id, task_id)| {
            SessionEvent::TaskSpawned {
                run_id,
                task_id,
                agent: "scout".into(),
                prompt: "dig".into(),
                at,
            }
        }),
        (run_id(), "[a-z]{1,6}").prop_map(move |(run_id, task_id)| {
            SessionEvent::TaskFinished {
                run_id,
                task_id,
                status: runic_state::TaskStatus::Completed,
                result: Some("done".into()),
                at,
            }
        }),
    ]
}

/// Event variants EXCEPT `StateSnapshot` — for the "no compaction" property.
fn event_no_snapshot() -> impl Strategy<Value = SessionEvent> {
    let at = ts();
    prop_oneof![
        run_id().prop_map(move |run_id| SessionEvent::RunStart {
            run_id,
            agent: None,
            at
        }),
        (run_id(), message()).prop_map(move |(run_id, msg)| SessionEvent::Message {
            run_id,
            msg,
            at
        }),
        run_id().prop_map(move |run_id| SessionEvent::TurnBoundary { run_id, at }),
        run_id().prop_map(move |run_id| SessionEvent::RunEnd {
            run_id,
            outcome: RunOutcome::default(),
            at,
        }),
    ]
}

fn state_from(events: &[SessionEvent]) -> AgentState {
    let mut st = AgentState::new("u", "s", "");
    for e in events {
        st.push_event(e.clone());
    }
    st
}

proptest! {
    /// With no compaction, the provider-facing list is EXACTLY the `Message`
    /// events, in order — nothing dropped, nothing reordered, nothing invented.
    #[test]
    fn no_snapshot_keeps_all_messages_in_order(events in prop::collection::vec(event_no_snapshot(), 0..16)) {
        let state = state_from(&events);
        let got = state.messages_for_provider();
        let expected: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                SessionEvent::Message { msg, .. } => Some(msg.content.text_content()),
                _ => None,
            })
            .collect();
        prop_assert_eq!(got.len(), expected.len());
        for (m, t) in got.iter().zip(&expected) {
            prop_assert_eq!(&m.content.text_content(), t);
        }
    }

    /// A `StateSnapshot` is compaction: replay restarts from the last snapshot's
    /// messages, then appends `Message` events after it. Length is computed by
    /// an independent path here (find last snapshot, count after).
    #[test]
    fn last_snapshot_truncates_then_appends(events in prop::collection::vec(event(), 0..16)) {
        let state = state_from(&events);
        let got = state.messages_for_provider();
        let expected_len = match events.iter().rposition(|e| matches!(e, SessionEvent::StateSnapshot { .. })) {
            Some(i) => {
                let snap_len = match &events[i] {
                    SessionEvent::StateSnapshot { messages, .. } => messages.len(),
                    _ => 0,
                };
                let after = events[i + 1..]
                    .iter()
                    .filter(|e| matches!(e, SessionEvent::Message { .. }))
                    .count();
                snap_len + after
            }
            None => events.iter().filter(|e| matches!(e, SessionEvent::Message { .. })).count(),
        };
        prop_assert_eq!(got.len(), expected_len);
    }

    /// Replay is deterministic — folding the same log twice yields the same list.
    #[test]
    fn replay_is_deterministic(events in prop::collection::vec(event(), 0..16)) {
        let state_a = state_from(&events);
        let state_b = state_from(&events);
        let a = state_a.messages_for_provider();
        let b = state_b.messages_for_provider();
        prop_assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b) {
            prop_assert_eq!(x.content.text_content(), y.content.text_content());
        }
    }

    /// Every event survives a JSON round-trip (the persistence wire format).
    /// Compared via `serde_json::Value` since `SessionEvent` has no `PartialEq`.
    #[test]
    fn event_json_round_trips(e in event()) {
        let v = serde_json::to_value(&e).unwrap();
        let back: SessionEvent = serde_json::from_value(v.clone()).unwrap();
        prop_assert_eq!(serde_json::to_value(&back).unwrap(), v);
    }

    /// Every derived field (stats, tasks, data) is a pure function of the log:
    /// a state built live and a state folded from the round-tripped log agree.
    /// A field that can't be rebuilt from events fails here the day it appears.
    #[test]
    fn derived_state_is_a_pure_fold_of_the_log(events in prop::collection::vec(event(), 0..24)) {
        let live = state_from(&events);
        let json = serde_json::to_string(&events).unwrap();
        let replayed: Vec<SessionEvent> = serde_json::from_str(&json).unwrap();
        let refold = state_from(&replayed);

        prop_assert_eq!(&live.stats, &refold.stats);
        prop_assert_eq!(&live.tasks, &refold.tasks);
        prop_assert_eq!(live.data(), refold.data());
        prop_assert_eq!(live.messages_for_provider().len(), refold.messages_for_provider().len());
    }
}
