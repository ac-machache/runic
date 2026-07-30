//! Provider responses where `content` and `tool_calls` disagree. Every driver
//! writes both together, so this shape is unreachable from the wire; these
//! tests pin the loop's normalization of it — `content` is the transcript and
//! the parsed calls are rebuilt from it.

mod harness;

use std::sync::Arc;

use harness::*;
use runic_agent::Runner;
use runic_types::{ContentBlock, StopReason, ToolCall};

#[tokio::test]
async fn a_tool_use_block_without_parsed_calls_is_dispatched_not_orphaned() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        mismatched_response(
            vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "rec".into(),
                input: serde_json::json!({}),
                provider_metadata: None,
            }],
            vec![], // no parsed calls
            StopReason::ToolUse,
        ),
        text_response("done"),
    ]));
    let rec = Arc::new(RecordingTool::new("rec", "ran"));
    let calls = rec.log();
    let mut agent = Runner::builder(provider, "u1", "s1")
        .model("test")
        .tool(rec)
        .build();

    let outcome = agent.run("go").await.unwrap();
    assert_eq!(
        calls.lock().unwrap().len(),
        1,
        "the call is rebuilt from the block, so it dispatches instead of dangling"
    );
    assert_eq!(outcome.total_turns, 2, "dispatch ⇒ the run continues");
    assert!(
        agent.state().current_run_id().is_none(),
        "run closed cleanly"
    );
}

#[tokio::test]
async fn parsed_calls_without_a_tool_use_block_are_dropped() {
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
    let mut agent = Runner::builder(provider.clone(), "u1", "s1")
        .model("test")
        .tool(rec)
        .build();

    let outcome = agent.run("go").await.unwrap();
    assert!(
        calls.lock().unwrap().is_empty(),
        "nothing in the transcript asked for it, so nothing runs"
    );
    assert_eq!(outcome.total_turns, 1, "no dispatch ⇒ the run terminates");
}

#[tokio::test]
async fn empty_content_and_no_calls_ends_cleanly() {
    // A degenerate but valid response: nothing at all. The run should end.
    let provider = Arc::new(ScriptedProvider::new(vec![mismatched_response(
        vec![],
        vec![],
        StopReason::EndTurn,
    )]));
    let mut agent = Runner::builder(provider, "u1", "s1").model("test").build();

    let outcome = agent.run("go").await.unwrap();
    assert_eq!(outcome.total_turns, 1);
    assert_eq!(outcome.stop_reason.as_deref(), Some("end_turn"));
    assert!(agent.state().current_run_id().is_none());
}
