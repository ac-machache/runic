//! Cancellation is graceful and checked at the turn boundary: a cancelled run
//! ends with a `cancelled` stop reason, makes no further model/tool calls, and
//! leaves nothing in flight.

mod harness;

use std::sync::Arc;

use harness::*;
use runic_agent::{Agent, CancelToken, RunContext};
use runic_state::SessionEvent;

#[tokio::test]
async fn cancel_before_the_first_turn_makes_no_model_call() {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("never reached")]));
    let mut agent = Agent::builder(provider.clone(), "u1", "s1")
        .model("test")
        .build();

    let cancel = CancelToken::new();
    cancel.cancel();
    let outcome = agent
        .run_with("hi", RunContext::new().with_cancel(cancel))
        .await
        .unwrap();

    assert_eq!(outcome.total_turns, 0);
    assert_eq!(outcome.stop_reason.as_deref(), Some("cancelled"));
    assert_eq!(provider.call_count(), 0, "no model call after pre-cancel");
    assert!(agent.state().current_run().is_none());
}

#[tokio::test]
async fn cancel_during_a_tool_ends_before_the_next_model_call() {
    // Turn 1 calls a tool that flips the cancel token. The loop dispatches the
    // tool, then checks cancellation at the top of turn 2 and stops — so the
    // second scripted response is never consumed.
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("t1", "cancel_tool", serde_json::json!({})),
        text_response("should-not-run"),
    ]));
    let cancel = CancelToken::new();
    let mut agent = Agent::builder(provider.clone(), "u1", "s1")
        .model("test")
        .tool(Arc::new(CancelTool {
            token: cancel.clone(),
        }))
        .build();

    let outcome = agent
        .run_with("go", RunContext::new().with_cancel(cancel))
        .await
        .unwrap();

    assert_eq!(outcome.stop_reason.as_deref(), Some("cancelled"));
    assert_eq!(outcome.total_turns, 1, "only the first turn completed");
    assert_eq!(
        provider.call_count(),
        1,
        "no second model call after cancellation"
    );

    // The tool result from turn 1 is still present and well-formed.
    let contents = tool_result_contents(&agent.state().messages_for_provider());
    assert!(contents.iter().any(|c| c.contains("cancelled the run")));
    assert!(agent.state().current_run().is_none());
}

#[tokio::test]
async fn cancel_while_a_model_call_is_in_flight_finishes_that_turn_then_stops() {
    // The cancel arrives while turn 1's model call is parked. The contract is
    // graceful-at-boundary: the in-flight call is *not* aborted — it completes,
    // its tool dispatches, then the run stops before turn 2's model call.
    let provider = Arc::new(GatedProvider::new(
        vec![
            Ok(tool_use_response("t1", "rec", serde_json::json!({}))),
            Ok(text_response("should-not-run")),
        ],
        1, // the first model call parks on the gate
    ));
    let cancel = CancelToken::new();
    let rec = Arc::new(RecordingTool::new("rec", "ran"));
    let calls = rec.log();
    let mut agent = Agent::builder(provider.clone(), "u1", "s1")
        .model("test")
        .tool(rec)
        .build();

    let entered = provider.entered();
    let run = agent.run_with("go", RunContext::new().with_cancel(cancel.clone()));
    let control = async {
        entered.notified().await; // turn 1's call is now parked
        cancel.cancel(); // cancel mid-flight
        provider.open_gate(); // let the parked call return
    };
    let (outcome, ()) = tokio::join!(run, control);
    let outcome = outcome.unwrap();

    assert_eq!(provider.call_count(), 1, "only the in-flight call ran");
    assert_eq!(outcome.total_turns, 1, "that turn completed");
    assert_eq!(outcome.stop_reason.as_deref(), Some("cancelled"));
    assert_eq!(calls.lock().unwrap().len(), 1, "its tool still dispatched");
    assert!(agent.state().current_run().is_none());
}

#[tokio::test]
async fn cancelled_run_is_recorded_as_a_clean_terminal_run_end() {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("x")]));
    let mut agent = Agent::builder(provider, "u1", "s1").model("test").build();
    let mut events = capture_session_events(&mut agent);

    let cancel = CancelToken::new();
    cancel.cancel();
    agent
        .run_with("hi", RunContext::new().with_cancel(cancel))
        .await
        .unwrap();

    let evs = drain(&mut events);
    let end = evs.iter().rev().find_map(|e| match e {
        SessionEvent::RunEnd { outcome, .. } => Some(outcome.clone()),
        _ => None,
    });
    let end = end.expect("a RunEnd closes the cancelled run");
    assert_eq!(end.stop_reason.as_deref(), Some("cancelled"));
}
