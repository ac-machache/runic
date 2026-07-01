//! Exact `SessionEvent` ordering for the non-happy paths: substitution, the two
//! cancellation timings, and provider failure (with and without a prior tool
//! round-trip). The persisted event log is the audit trail, so its shape is a
//! contract worth pinning precisely.

mod harness;

use std::sync::{Arc, Mutex};

use harness::*;
use runic_agent::{Agent, CancelToken, RunContext};
use runic_provider::ProviderError;

#[tokio::test]
async fn substitution_path_leaves_a_hookran_audit_entry() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("t1", "rec", serde_json::json!({})),
        text_response("done"),
    ]));
    let log = Arc::new(Mutex::new(Vec::new()));
    let hook =
        RecordWriteHook::new("sub", log).act("before_tool", Act::Substitute("hooked".into()));
    let mut agent = Agent::builder(provider, "u1", "s1")
        .model("test")
        .tool(Arc::new(RecordingTool::new("rec", "REAL")))
        .write_hook(Arc::new(hook))
        .build();
    let mut events = capture_session_events(&mut agent);

    agent.run("go").await.unwrap();

    assert_eq!(
        session_kinds(&drain(&mut events)),
        vec![
            "RunStart",
            "Message",      // user
            "Message",      // assistant (tool_use)
            "TurnBoundary", // turn 1
            "HookRan",      // the substitution itself
            "Message",      // substituted tool result
            "Message",      // assistant (final text)
            "TurnBoundary", // turn 2
            "RunEnd",
        ]
    );
}

#[tokio::test]
async fn precancel_path_emits_only_runstart_message_runend() {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("never")]));
    let mut agent = Agent::builder(provider, "u1", "s1").model("test").build();
    let mut events = capture_session_events(&mut agent);

    let cancel = CancelToken::new();
    cancel.cancel();
    agent
        .run_with("go", RunContext::new().with_cancel(cancel))
        .await
        .unwrap();

    assert_eq!(
        session_kinds(&drain(&mut events)),
        vec!["RunStart", "Message", "RunEnd"]
    );
}

#[tokio::test]
async fn cancel_after_tool_stops_before_the_next_assistant_message() {
    // Turn 1's tool flips the token; the log ends right after the tool result,
    // with no second assistant message and no second TurnBoundary.
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("t1", "cancel_tool", serde_json::json!({})),
        text_response("should-not-run"),
    ]));
    let cancel = CancelToken::new();
    let mut agent = Agent::builder(provider, "u1", "s1")
        .model("test")
        .tool(Arc::new(CancelTool {
            token: cancel.clone(),
        }))
        .build();
    let mut events = capture_session_events(&mut agent);

    agent
        .run_with("go", RunContext::new().with_cancel(cancel))
        .await
        .unwrap();

    assert_eq!(
        session_kinds(&drain(&mut events)),
        vec![
            "RunStart",
            "Message",      // user
            "Message",      // assistant (tool_use)
            "TurnBoundary", // turn 1
            "Message",      // tool result
            "RunEnd",       // cancelled at the next boundary
        ]
    );
}

#[tokio::test]
async fn first_call_failure_emits_runstart_message_runend() {
    let provider = Arc::new(ScriptedProvider::with_results(vec![Err(
        ProviderError::AuthenticationFailed("nope".into()),
    )]));
    let mut agent = Agent::builder(provider, "u1", "s1").model("test").build();
    let mut events = capture_session_events(&mut agent);

    agent.run("go").await.unwrap_err();

    // No assistant message was ever appended (the call failed before that).
    assert_eq!(
        session_kinds(&drain(&mut events)),
        vec!["RunStart", "Message", "RunEnd"]
    );
}

#[tokio::test]
async fn failure_after_a_tool_round_trip_keeps_the_partial_log() {
    let provider = Arc::new(ScriptedProvider::with_results(vec![
        Ok(tool_use_response("t1", "rec", serde_json::json!({}))),
        Err(ProviderError::Api {
            status: 500,
            message: "boom".into(),
        }),
    ]));
    let mut agent = Agent::builder(provider, "u1", "s1")
        .model("test")
        .tool(Arc::new(RecordingTool::new("rec", "ran")))
        .build();
    let mut events = capture_session_events(&mut agent);

    agent.run("go").await.unwrap_err();

    assert_eq!(
        session_kinds(&drain(&mut events)),
        vec![
            "RunStart",
            "Message",      // user
            "Message",      // assistant (tool_use)
            "TurnBoundary", // turn 1
            "Message",      // tool result
            "RunEnd",       // turn 2's model call failed
        ]
    );
}
