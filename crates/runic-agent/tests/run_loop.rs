//! Run-boundary and outer-loop contract: run-id scoping, terminal state, a
//! second run over the same session, and the provider-error close-out path.

mod harness;

use std::sync::Arc;

use harness::*;
use runic_agent::{Agent, AgentError};
use runic_provider::ProviderError;
use runic_state::SessionEvent;
use runic_types::MessageContent;

#[tokio::test]
async fn text_only_run_emits_bookended_events_for_one_run_id() {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("hello there")]));
    let mut agent = Agent::builder(provider, "u1", "s1")
        .model("test")
        .system_prompt("be brief")
        .build();
    let mut events = capture_session_events(&mut agent);

    let outcome = agent.run("hi").await.unwrap();

    assert_eq!(outcome.total_turns, 1);
    assert_eq!(outcome.stop_reason.as_deref(), Some("end_turn"));
    assert_eq!(outcome.usage.input_tokens, 10);

    let evs = drain(&mut events);
    // First event opens the run, last closes it.
    assert!(matches!(evs.first(), Some(SessionEvent::RunStart { .. })));
    assert!(matches!(evs.last(), Some(SessionEvent::RunEnd { .. })));

    // Every event belongs to the one run id minted at the top.
    let run_id = match evs.first().unwrap() {
        SessionEvent::RunStart { run_id, .. } => run_id.clone(),
        _ => unreachable!(),
    };
    assert!(!run_id.is_empty());
    assert!(
        evs.iter().all(|e| e.run_id() == run_id),
        "all events share the run id: {evs:?}"
    );

    // The run is terminal: no in-flight run remains.
    assert!(
        agent.state().current_run().is_none(),
        "current_run must be cleared once the run ends"
    );
}

#[tokio::test]
async fn each_run_gets_a_fresh_run_id() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        text_response("one"),
        text_response("two"),
    ]));
    let mut agent = Agent::builder(provider, "u1", "s1").model("test").build();

    let mut e1 = capture_session_events(&mut agent);
    agent.run("first").await.unwrap();
    let id1 = first_run_id(&drain(&mut e1));

    let mut e2 = capture_session_events(&mut agent);
    agent.run("second").await.unwrap();
    let id2 = first_run_id(&drain(&mut e2));

    assert_ne!(id1, id2, "a new run must mint a new run id");

    // Two runs, both ended, grouped in order.
    let runs = agent.state().runs();
    assert_eq!(runs.len(), 2);
    assert!(runs.iter().all(|r| r.ended_at.is_some()));
}

#[tokio::test]
async fn second_run_sees_the_first_runs_persisted_messages() {
    // Run 1 establishes a fact; run 2's request must carry run 1's history.
    let provider = Arc::new(ScriptedProvider::new(vec![
        text_response("my name is Ada"),
        text_response("you are Ada"),
    ]));
    let mut agent = Agent::builder(provider.clone(), "u1", "s1")
        .model("test")
        .build();

    agent.run("remember my name").await.unwrap();
    agent.run("what is my name").await.unwrap();

    // The 2nd model call's request includes the earlier user + assistant turns.
    let second_request = &provider.requests()[1];
    let texts: Vec<String> = second_request
        .messages
        .iter()
        .map(|m| m.content.text_content())
        .collect();
    assert!(
        texts.iter().any(|t| t == "remember my name"),
        "run 2 should see run 1's user message: {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t == "my name is Ada"),
        "run 2 should see run 1's assistant reply: {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t == "what is my name"),
        "run 2 should see its own user message: {texts:?}"
    );
}

#[tokio::test]
async fn provider_error_closes_the_run_and_leaves_nothing_in_flight() {
    // The model call fails outright (a non-retryable, non-fallback error).
    let provider = Arc::new(ScriptedProvider::with_results(vec![Err(
        ProviderError::AuthenticationFailed("bad key".into()),
    )]));
    let mut agent = Agent::builder(provider, "u1", "s1").model("test").build();
    let mut events = capture_session_events(&mut agent);

    let err = agent.run("hi").await.unwrap_err();
    assert!(matches!(err, AgentError::Provider(_)), "got {err:?}");

    let evs = drain(&mut events);
    // The run is still closed with a RunEnd carrying the error stop reason.
    let end = evs
        .iter()
        .rev()
        .find_map(|e| match e {
            SessionEvent::RunEnd { outcome, .. } => Some(outcome),
            _ => None,
        })
        .expect("a RunEnd is emitted even on failure");
    assert!(
        end.stop_reason
            .as_deref()
            .unwrap_or_default()
            .contains("error"),
        "failed run records an error stop reason: {:?}",
        end.stop_reason
    );
    assert_eq!(end.total_turns, 0, "no turn completed");

    // No hanging run.
    assert!(
        agent.state().current_run().is_none(),
        "a failed run must not stay in flight"
    );
}

#[tokio::test]
async fn provider_error_after_a_tool_round_trip_still_closes_cleanly() {
    // Turn 1 succeeds (a tool runs); the follow-up model call fails.
    let provider = Arc::new(ScriptedProvider::with_results(vec![
        Ok(tool_use_response(
            "t1",
            "rec",
            serde_json::json!({ "x": 1 }),
        )),
        Err(ProviderError::Api {
            status: 500,
            message: "server fell over".into(),
        }),
    ]));
    let rec = Arc::new(RecordingTool::new("rec", "ran"));
    let calls = rec.log();
    let mut agent = Agent::builder(provider, "u1", "s1")
        .model("test")
        .tool(rec)
        .build();
    let mut events = capture_session_events(&mut agent);

    let err = agent.run("go").await.unwrap_err();
    assert!(matches!(err, AgentError::Provider(_)), "got {err:?}");

    // The tool did run before the failure, and its result is in history.
    assert_eq!(calls.lock().unwrap().len(), 1);
    let has_tool_result = agent.state().messages_for_provider().iter().any(|m| {
        matches!(&m.content, MessageContent::Blocks(b)
            if b.iter().any(|blk| matches!(blk, runic_types::ContentBlock::ToolResult { .. })))
    });
    assert!(has_tool_result, "the executed tool result is persisted");

    assert!(matches!(
        drain(&mut events).last(),
        Some(SessionEvent::RunEnd { .. })
    ));
    assert!(agent.state().current_run().is_none());
}

fn first_run_id(evs: &[SessionEvent]) -> String {
    evs.iter()
        .find_map(|e| match e {
            SessionEvent::RunStart { run_id, .. } => Some(run_id.clone()),
            _ => None,
        })
        .expect("a RunStart event")
}
