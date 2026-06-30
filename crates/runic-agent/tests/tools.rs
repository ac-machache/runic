//! Tool dispatch: multiple calls in a turn, stable ordering, every result
//! reaching the next model call, and the failure modes (Err, in-band error,
//! panic, unknown tool) all mapping to sane model-facing results.

mod harness;

use std::sync::Arc;

use harness::*;
use runic_agent::Agent;
use runic_types::{ContentBlock, MessageContent};

#[tokio::test]
async fn multiple_tool_calls_all_execute_and_results_reach_the_model() {
    // One turn requests three calls; the next request must carry all three
    // results, in the order the calls were issued.
    let provider = Arc::new(ScriptedProvider::new(vec![
        multi_tool_response(vec![
            ("a", "rec", serde_json::json!({ "i": 1 })),
            ("b", "rec", serde_json::json!({ "i": 2 })),
            ("c", "rec", serde_json::json!({ "i": 3 })),
        ]),
        text_response("done"),
    ]));
    let rec = Arc::new(RecordingTool::new("rec", "ran"));
    let calls = rec.log();
    let mut agent = Agent::builder(provider.clone(), "u1", "s1")
        .model("test")
        .tool(rec)
        .build();

    let outcome = agent.run("go").await.unwrap();
    assert_eq!(outcome.total_turns, 2);
    assert_eq!(calls.lock().unwrap().len(), 3, "all three calls executed");

    // The combined tool-result message preserves call order a→b→c.
    let second = &provider.requests()[1];
    let ids: Vec<String> = second
        .messages
        .iter()
        .filter_map(|m| match &m.content {
            MessageContent::Blocks(b) => Some(b),
            _ => None,
        })
        .flatten()
        .filter_map(|b| match b {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(ids, vec!["a", "b", "c"], "tool-result order must be stable");
}

#[tokio::test]
async fn parallelizable_calls_all_complete() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        multi_tool_response(vec![
            ("a", "par", serde_json::json!({ "i": 1 })),
            ("b", "par", serde_json::json!({ "i": 2 })),
        ]),
        text_response("done"),
    ]));
    let par = Arc::new(RecordingTool::new("par", "ran").parallel());
    let calls = par.log();
    let mut agent = Agent::builder(provider.clone(), "u1", "s1")
        .model("test")
        .tool(par)
        .build();

    agent.run("go").await.unwrap();
    assert_eq!(calls.lock().unwrap().len(), 2);
    // Results still land in issue order regardless of concurrent execution.
    let ids: Vec<String> = provider.requests()[1]
        .messages
        .iter()
        .filter_map(|m| match &m.content {
            MessageContent::Blocks(b) => Some(b),
            _ => None,
        })
        .flatten()
        .filter_map(|b| match b {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(ids, vec!["a", "b"]);
}

#[tokio::test]
async fn unknown_tool_yields_an_in_band_error_not_a_crash() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("t1", "ghost", serde_json::json!({})),
        text_response("recovered"),
    ]));
    let mut agent = Agent::builder(provider, "u1", "s1").model("test").build();

    let outcome = agent.run("go").await.unwrap();
    assert_eq!(
        outcome.total_turns, 2,
        "run continues after an unknown tool"
    );
    assert!(error_result_matching(&agent, |c| c.contains("unknown tool")));
}

#[tokio::test]
async fn tool_returning_err_maps_to_an_error_result() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("t1", "err_tool", serde_json::json!({})),
        text_response("recovered"),
    ]));
    let mut agent = Agent::builder(provider, "u1", "s1")
        .model("test")
        .tool(Arc::new(ErrTool))
        .build();

    agent.run("go").await.unwrap();
    assert!(error_result_matching(&agent, |c| c.contains("failed")
        && c.contains("boom from inside the tool")));
}

#[tokio::test]
async fn tool_returning_error_result_is_persisted_as_error() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("t1", "err_result_tool", serde_json::json!({})),
        text_response("recovered"),
    ]));
    let mut agent = Agent::builder(provider, "u1", "s1")
        .model("test")
        .tool(Arc::new(ErrResultTool))
        .build();

    agent.run("go").await.unwrap();
    assert!(error_result_matching(&agent, |c| c.contains("could not do the thing")));
}

#[tokio::test]
async fn panicking_tool_is_caught_and_does_not_abort_the_run() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("t1", "panic_tool", serde_json::json!({})),
        text_response("survived"),
    ]));
    let mut agent = Agent::builder(provider, "u1", "s1")
        .model("test")
        .tool(Arc::new(PanicTool))
        .build();

    let outcome = agent.run("go").await.unwrap();
    assert_eq!(
        outcome.total_turns, 2,
        "a panicking tool must not kill the run"
    );
    assert!(error_result_matching(&agent, |c| c.contains("panicked")));
}

/// Whether any persisted `tool_result` block is an error whose content matches.
fn error_result_matching(agent: &Agent, pred: impl Fn(&str) -> bool) -> bool {
    agent
        .state()
        .messages_for_provider()
        .iter()
        .any(|m| match &m.content {
            MessageContent::Blocks(b) => b.iter().any(|blk| {
                matches!(blk, ContentBlock::ToolResult { content, is_error, .. }
                    if *is_error && pred(content))
            }),
            _ => false,
        })
}
