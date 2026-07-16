use chrono::{DateTime, Utc};
use proptest::prelude::*;

use runic_state::timeline::project;
use runic_state::{
    DelegationMode, DelegationStatus, RunEndStatus, RunOutcome, SessionEvent, ToolStatus,
    TraceStatus,
};
use runic_types::TokenUsage;

fn ts(n: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000 + n, 0).unwrap()
}

fn usage(input: u64, output: u64) -> TokenUsage {
    TokenUsage {
        input_tokens: input,
        output_tokens: output,
        ..Default::default()
    }
}

fn golden_events() -> Vec<SessionEvent> {
    vec![
        SessionEvent::RunStart {
            run_id: "r1".into(),
            agent: Some("maia".into()),
            audit: None,
            at: ts(0),
        },
        SessionEvent::TurnEnd {
            run_id: "r1".into(),
            turn: 1,
            model: "m1".into(),
            usage: usage(100, 20),
            model_ms: 900,
            at: ts(1),
        },
        SessionEvent::ToolStarted {
            run_id: "r1".into(),
            turn: 1,
            call_id: "c1".into(),
            tool: "search".into(),
            at: ts(2),
        },
        SessionEvent::ToolFinished {
            run_id: "r1".into(),
            turn: 1,
            call_id: "c1".into(),
            tool: "search".into(),
            status: ToolStatus::Ok,
            duration_ms: 42,
            at: ts(3),
        },
        SessionEvent::DelegationStarted {
            run_id: "r1".into(),
            turn: 1,
            call_id: "c2".into(),
            agent: "scout".into(),
            mode: DelegationMode::Sync,
            child_session: None,
            at: ts(4),
        },
        SessionEvent::DelegationFinished {
            run_id: "r1".into(),
            turn: 1,
            call_id: "c2".into(),
            agent: "scout".into(),
            status: DelegationStatus::Ok,
            usage: usage(9, 4),
            model: Some("m-child".into()),
            duration_ms: 300,
            child_session: Some("chd-1".into()),
            child_persistence: None,
            at: ts(5),
        },
        SessionEvent::TurnEnd {
            run_id: "r1".into(),
            turn: 2,
            model: "m1".into(),
            usage: usage(150, 30),
            model_ms: 700,
            at: ts(6),
        },
        SessionEvent::RunEnd {
            run_id: "r1".into(),
            status: RunEndStatus::Completed,
            outcome: RunOutcome::default(),
            at: ts(7),
        },
    ]
}

#[test]
fn the_golden_run_projects_to_the_exact_tree() {
    let runs = project(&golden_events());
    assert_eq!(runs.len(), 1);
    let run = &runs[0];

    assert_eq!(run.run_id, "r1");
    assert_eq!(run.agent.as_deref(), Some("maia"));
    assert_eq!(run.status, TraceStatus::Completed);
    assert_eq!(run.started_at, Some(ts(0)));
    assert_eq!(run.ended_at, Some(ts(7)));
    assert_eq!(run.usage.input_tokens, 250);
    assert_eq!(run.usage.output_tokens, 50);

    assert_eq!(run.turns.len(), 2);
    let turn1 = &run.turns[0];
    assert_eq!(turn1.turn, 1);
    assert!(turn1.complete);
    assert_eq!(turn1.model.as_deref(), Some("m1"));
    assert_eq!(turn1.usage.input_tokens, 100);
    assert_eq!(turn1.model_ms, 900);

    assert_eq!(turn1.tools.len(), 1);
    let tool = &turn1.tools[0];
    assert_eq!(tool.tool, "search");
    assert_eq!(tool.status, Some(ToolStatus::Ok));
    assert_eq!(tool.duration_ms, Some(42));
    assert_eq!(tool.started_at, Some(ts(2)));

    assert_eq!(turn1.delegations.len(), 1);
    let delegation = &turn1.delegations[0];
    assert_eq!(delegation.agent, "scout");
    assert_eq!(delegation.mode, Some(DelegationMode::Sync));
    assert_eq!(delegation.status, Some(DelegationStatus::Ok));
    assert_eq!(delegation.usage.input_tokens, 9);
    assert_eq!(delegation.model.as_deref(), Some("m-child"));
    assert_eq!(delegation.duration_ms, Some(300));

    let turn2 = &run.turns[1];
    assert_eq!(turn2.turn, 2);
    assert!(turn2.tools.is_empty());
}

#[test]
fn incomplete_work_is_preserved_not_dropped() {
    let events = vec![
        SessionEvent::ToolFinished {
            run_id: "r1".into(),
            turn: 1,
            call_id: "sub".into(),
            tool: "gated".into(),
            status: ToolStatus::Substituted,
            duration_ms: 0,
            at: ts(1),
        },
        SessionEvent::ToolStarted {
            run_id: "r1".into(),
            turn: 1,
            call_id: "hang".into(),
            tool: "slow".into(),
            at: ts(2),
        },
    ];
    let runs = project(&events);
    assert_eq!(runs.len(), 1);
    let run = &runs[0];
    assert_eq!(run.status, TraceStatus::InFlight);
    assert!(run.started_at.is_none(), "finish-without-start preserved");

    let tools = &run.turns[0].tools;
    assert_eq!(tools.len(), 2);
    let substituted = tools.iter().find(|t| t.call_id == "sub").unwrap();
    assert!(substituted.started_at.is_none());
    assert_eq!(substituted.status, Some(ToolStatus::Substituted));
    let hanging = tools.iter().find(|t| t.call_id == "hang").unwrap();
    assert!(hanging.status.is_none(), "start-without-finish preserved");
}

#[test]
fn duplicate_turn_ends_do_not_double_count_usage() {
    let turn = SessionEvent::TurnEnd {
        run_id: "r1".into(),
        turn: 1,
        model: "m".into(),
        usage: usage(100, 20),
        model_ms: 5,
        at: ts(1),
    };
    let runs = project(&[turn.clone(), turn]);
    assert_eq!(runs[0].usage.input_tokens, 100, "first write wins, once");
    assert_eq!(runs[0].turns.len(), 1);
}

#[test]
fn tools_correlate_on_call_id_and_name_not_call_id_alone() {
    let events = vec![
        SessionEvent::ToolFinished {
            run_id: "r1".into(),
            turn: 1,
            call_id: "dup".into(),
            tool: "alpha".into(),
            status: ToolStatus::Ok,
            duration_ms: 1,
            at: ts(1),
        },
        SessionEvent::ToolFinished {
            run_id: "r1".into(),
            turn: 1,
            call_id: "dup".into(),
            tool: "beta".into(),
            status: ToolStatus::Ok,
            duration_ms: 2,
            at: ts(2),
        },
    ];
    let runs = project(&events);
    assert_eq!(
        runs[0].turns[0].tools.len(),
        2,
        "same call_id, different tool = different calls"
    );
}

proptest! {
    #[test]
    fn any_shuffle_or_truncation_projects_without_panic_or_loss(
        order in prop::sample::subsequence(golden_events(), 0..=8).prop_shuffle()
    ) {
        let runs = project(&order);
        let finishes = order
            .iter()
            .filter(|e| matches!(e, SessionEvent::ToolFinished { .. }))
            .count();
        let projected: usize = runs
            .iter()
            .flat_map(|r| &r.turns)
            .flat_map(|t| &t.tools)
            .filter(|t| t.status.is_some())
            .count();
        prop_assert_eq!(finishes, projected, "no finished tool is ever dropped");
    }
}
