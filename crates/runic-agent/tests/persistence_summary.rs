//! Persistence-summary safety: a tool's full output reaches the model on the
//! next call, but only its `persisted_output` summary is ever written to the
//! event log (and broadcast to persistence subscribers / live events).

mod harness;

use std::sync::Arc;

use harness::*;
use runic_agent::{Agent, AgentEvent, RunContext};
use runic_state::SessionEvent;
use runic_types::{ContentBlock, MessageContent};

const FULL: &str = "FULL_SECRET_CONTENT_THAT_MUST_NOT_PERSIST";
const SUMMARY: &str = "artifact returned; content omitted from log";

#[tokio::test]
async fn full_output_reaches_model_only_summary_persists() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("c1", "summary_tool", serde_json::json!({})),
        text_response("done"),
    ]));
    let mut agent = Agent::builder(provider.clone(), "u", "s")
        .model("test")
        .tool(Arc::new(SummaryTool::new(FULL, SUMMARY)))
        .build();

    agent.run("go").await.unwrap();

    // The follow-up model call saw the FULL output …
    let seen = tool_result_contents(&provider.requests()[1].messages);
    assert!(
        seen.iter().any(|c| c == FULL),
        "next model call should receive the full output, got {seen:?}"
    );

    // … but the persisted message view keeps only the summary.
    let persisted = tool_result_contents(&agent.state().messages_for_provider());
    assert!(
        persisted.iter().any(|c| c.contains("omitted from log")),
        "history keeps the summary: {persisted:?}"
    );
    assert!(
        !persisted.iter().any(|c| c.contains("SECRET")),
        "history must never contain the full bytes: {persisted:?}"
    );
}

#[tokio::test]
async fn persisted_session_events_never_carry_the_full_bytes() {
    // The lossless persistence sink is what a substrate would store; assert the
    // full bytes never flow through it.
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("c1", "summary_tool", serde_json::json!({})),
        text_response("done"),
    ]));
    let mut agent = Agent::builder(provider, "u", "s")
        .model("test")
        .tool(Arc::new(SummaryTool::new(FULL, SUMMARY)))
        .build();
    let mut events = capture_session_events(&mut agent);

    agent.run("go").await.unwrap();

    for ev in drain(&mut events) {
        if let SessionEvent::Message { msg, .. } = &ev
            && let MessageContent::Blocks(blocks) = &msg.content
        {
            for b in blocks {
                if let ContentBlock::ToolResult { content, .. } = b {
                    assert!(
                        !content.contains("SECRET"),
                        "a persisted SessionEvent leaked the full output: {content}"
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn live_tool_finished_event_carries_the_summary_not_the_full_output() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use_response("c1", "summary_tool", serde_json::json!({})),
        text_response("done"),
    ]));
    let mut agent = Agent::builder(provider, "u", "s")
        .model("test")
        .tool(Arc::new(SummaryTool::new(FULL, SUMMARY)))
        .build();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    agent
        .run_with("go", RunContext::new().with_events(tx))
        .await
        .unwrap();

    let finished: Vec<String> = drain(&mut rx)
        .into_iter()
        .filter_map(|e| match e {
            AgentEvent::ToolFinished { result, .. } => Some(result),
            _ => None,
        })
        .collect();
    assert!(
        finished.iter().any(|r| r.contains("omitted from log")),
        "ToolFinished should show the summary: {finished:?}"
    );
    assert!(
        !finished.iter().any(|r| r.contains("SECRET")),
        "the live tool-finished event must not show the full bytes: {finished:?}"
    );
}
