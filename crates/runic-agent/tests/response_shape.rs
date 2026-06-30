//! Provider responses where `content` and `tool_calls` disagree. The loop is
//! driven by `tool_calls` (the parsed calls), and keeps `content` verbatim on
//! the assistant message — these tests pin that contract down.

mod harness;

use std::sync::Arc;

use harness::*;
use runic_agent::Agent;
use runic_types::{ContentBlock, StopReason, ToolCall};

#[tokio::test]
async fn tool_use_block_without_parsed_tool_calls_does_not_crash_or_hang() {
    // KNOWN SHARP EDGE, not an endorsed contract: a malformed response whose
    // content carries a tool_use block but whose parsed `tool_calls` is empty.
    // The loop is driven by `tool_calls`, so it has nothing to dispatch and the
    // turn is terminal — which can leave a dangling tool_use in history.
    // Orphaned-tool_use pruning is a planned normalization step (see
    // `src/turn/history.rs`); this test only guards that the loop stays safe
    // (no panic, no hang, run terminates) until that lands. Do not read it as
    // "dangling tool_use is the desired outcome".
    let provider = Arc::new(ScriptedProvider::new(vec![mismatched_response(
        vec![ContentBlock::ToolUse {
            id: "t1".into(),
            name: "rec".into(),
            input: serde_json::json!({}),
            provider_metadata: None,
        }],
        vec![], // no parsed calls
        StopReason::ToolUse,
    )]));
    let rec = Arc::new(RecordingTool::new("rec", "ran"));
    let calls = rec.log();
    let mut agent = Agent::builder(provider, "u1", "s1")
        .model("test")
        .tool(rec)
        .build();

    let outcome = agent.run("go").await.unwrap();
    assert_eq!(outcome.total_turns, 1, "no dispatch ⇒ the run terminates");
    assert!(calls.lock().unwrap().is_empty(), "the tool must not run");
    assert!(agent.state().current_run().is_none(), "run closed cleanly");
}

#[tokio::test]
async fn parsed_tool_calls_without_a_tool_use_block_still_dispatch() {
    // The inverse: no tool_use content block, but `tool_calls` is populated.
    // Dispatch is driven by `tool_calls`, so the tool runs.
    let provider = Arc::new(ScriptedProvider::new(vec![
        mismatched_response(
            vec![ContentBlock::Text {
                text: "calling a tool now".into(),
                provider_metadata: None,
            }],
            vec![ToolCall {
                id: "t1".into(),
                name: "rec".into(),
                input: serde_json::json!({ "x": 1 }),
            }],
            StopReason::ToolUse,
        ),
        text_response("done"),
    ]));
    let rec = Arc::new(RecordingTool::new("rec", "ran"));
    let calls = rec.log();
    let mut agent = Agent::builder(provider.clone(), "u1", "s1")
        .model("test")
        .tool(rec)
        .build();

    let outcome = agent.run("go").await.unwrap();
    assert_eq!(
        outcome.total_turns, 2,
        "the call dispatched and the run continued"
    );
    assert_eq!(calls.lock().unwrap().len(), 1, "tool_calls drives dispatch");

    let results = tool_results(&provider.requests()[1].messages);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, "t1");
}

#[tokio::test]
async fn empty_content_and_no_calls_ends_cleanly() {
    // A degenerate but valid response: nothing at all. The run should end.
    let provider = Arc::new(ScriptedProvider::new(vec![mismatched_response(
        vec![],
        vec![],
        StopReason::EndTurn,
    )]));
    let mut agent = Agent::builder(provider, "u1", "s1").model("test").build();

    let outcome = agent.run("go").await.unwrap();
    assert_eq!(outcome.total_turns, 1);
    assert_eq!(outcome.stop_reason.as_deref(), Some("end_turn"));
    assert!(agent.state().current_run().is_none());
}
