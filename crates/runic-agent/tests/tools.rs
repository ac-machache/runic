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

#[tokio::test]
async fn mixed_multi_tool_one_ok_one_err_one_panic() {
    // A single turn issues three calls with three different fates; all three
    // must produce a result, in issue order, with the right error flags.
    let provider = Arc::new(ScriptedProvider::new(vec![
        multi_tool_response(vec![
            ("ok", "rec", serde_json::json!({})),
            ("err", "err_tool", serde_json::json!({})),
            ("panic", "panic_tool", serde_json::json!({})),
        ]),
        text_response("done"),
    ]));
    let mut agent = Agent::builder(provider.clone(), "u1", "s1")
        .model("test")
        .tool(Arc::new(RecordingTool::new("rec", "ran")))
        .tool(Arc::new(ErrTool))
        .tool(Arc::new(PanicTool))
        .build();

    let outcome = agent.run("go").await.unwrap();
    assert_eq!(outcome.total_turns, 2);

    let results = tool_results(&provider.requests()[1].messages);
    let ids: Vec<&str> = results.iter().map(|(id, ..)| id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["ok", "err", "panic"],
        "order preserved: {results:?}"
    );

    let by_id = |id: &str| results.iter().find(|(i, ..)| i == id).unwrap().clone();
    let (_, ok_c, ok_err) = by_id("ok");
    assert!(!ok_err && ok_c.contains("ran"));
    let (_, err_c, err_err) = by_id("err");
    assert!(err_err && err_c.contains("boom from inside the tool"));
    let (_, pan_c, pan_err) = by_id("panic");
    assert!(pan_err && pan_c.contains("panicked"));
}

#[tokio::test]
async fn duplicate_tool_call_ids_in_one_turn_do_not_crash() {
    // Two calls share an id (a malformed-but-possible provider response). Both
    // must execute and both results land, without a panic.
    let provider = Arc::new(ScriptedProvider::new(vec![
        multi_tool_response(vec![
            ("dup", "rec", serde_json::json!({ "n": 1 })),
            ("dup", "rec", serde_json::json!({ "n": 2 })),
        ]),
        text_response("done"),
    ]));
    let rec = Arc::new(RecordingTool::new("rec", "ran"));
    let calls = rec.log();
    let mut agent = Agent::builder(provider.clone(), "u1", "s1")
        .model("test")
        .tool(rec)
        .build();

    agent.run("go").await.unwrap();
    assert_eq!(
        calls.lock().unwrap().len(),
        2,
        "both duplicate-id calls ran"
    );

    let results = tool_results(&provider.requests()[1].messages);
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|(id, ..)| id == "dup"));
}

#[tokio::test]
async fn parallel_tools_finish_out_of_order_but_results_stay_ordered() {
    // Issue a→b→c, but the gated tool forces completion order c→b→a. This is
    // deterministic (no timing): a serial issue-order executor would deadlock
    // waiting on `a` first, so a pass proves the calls truly ran concurrently.
    let provider = Arc::new(ScriptedProvider::new(vec![
        multi_tool_response(vec![
            ("a", "gate", serde_json::json!({ "tag": "a" })),
            ("b", "gate", serde_json::json!({ "tag": "b" })),
            ("c", "gate", serde_json::json!({ "tag": "c" })),
        ]),
        text_response("done"),
    ]));
    let gate = Arc::new(OrderedGateTool::new("gate", vec!["c", "b", "a"]));
    let completions = gate.completions();
    let mut agent = Agent::builder(provider.clone(), "u1", "s1")
        .model("test")
        .tool(gate)
        .build();

    agent.run("go").await.unwrap();

    // Completion order is the forced c,b,a — concurrency proven, no timing.
    assert_eq!(
        completions.lock().unwrap().clone(),
        vec!["c", "b", "a"],
        "tools completed in the gated order, proving concurrency"
    );
    // But the results fed back to the model preserve issue order.
    let ids: Vec<String> = tool_results(&provider.requests()[1].messages)
        .into_iter()
        .map(|(id, ..)| id)
        .collect();
    assert_eq!(ids, vec!["a", "b", "c"]);
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
