//! A tool's `persisted_output` summary is what lands in history; the full
//! `output` reaches only the immediately-following model call.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use runic_agent::Agent;
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_tool::{Tool, ToolContext, ToolResult};
use runic_types::{ContentBlock, MessageContent, StopReason, TokenUsage, ToolCall};

struct ScriptedProvider {
    responses: Mutex<std::collections::VecDeque<CompletionResponse>>,
    last_request: Mutex<Option<CompletionRequest>>,
}

impl ScriptedProvider {
    fn new(responses: Vec<CompletionResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            last_request: Mutex::new(None),
        }
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        *self.last_request.lock().unwrap() = Some(request);
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ProviderError::Parse("exhausted".into()))
    }
}

fn tool_use(id: &str, name: &str) -> CompletionResponse {
    CompletionResponse {
        content: vec![ContentBlock::ToolUse {
            id: id.into(),
            name: name.into(),
            input: serde_json::json!({}),
            provider_metadata: None,
        }],
        stop_reason: StopReason::ToolUse,
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: name.into(),
            input: serde_json::json!({}),
        }],
        usage: TokenUsage::default(),
    }
}

fn text(t: &str) -> CompletionResponse {
    CompletionResponse {
        content: vec![ContentBlock::Text {
            text: t.into(),
            provider_metadata: None,
        }],
        stop_reason: StopReason::EndTurn,
        tool_calls: vec![],
        usage: TokenUsage::default(),
    }
}

struct Leaker;

#[async_trait]
impl Tool for Leaker {
    fn name(&self) -> &str {
        "leaker"
    }
    fn description(&self) -> &str {
        "returns a big payload the model needs but the log shouldn't keep"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    async fn execute(&self, _a: serde_json::Value, _c: &ToolContext) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::ok("FULL_SECRET_CONTENT")
            .with_persisted_summary("artifact returned (18 bytes); content omitted from log."))
    }
}

fn tool_result_contents(req: &CompletionRequest) -> Vec<String> {
    req.messages
        .iter()
        .filter_map(|m| match &m.content {
            MessageContent::Blocks(b) => Some(b),
            _ => None,
        })
        .flatten()
        .filter_map(|b| match b {
            ContentBlock::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn transient_output_reaches_model_but_summary_is_persisted() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use("c1", "leaker"),
        text("done"),
    ]));
    let mut agent = Agent::builder(provider.clone(), "u", "s")
        .model("test")
        .tool(Arc::new(Leaker))
        .build();

    agent.run("go").await.unwrap();

    // The post-tool request (the 2nd call) carried the FULL output …
    let last = provider.last_request.lock().unwrap().clone().unwrap();
    let seen = tool_result_contents(&last);
    assert!(
        seen.iter().any(|c| c == "FULL_SECRET_CONTENT"),
        "next model call should see the full output, got {seen:?}"
    );

    // … but persisted history keeps only the summary.
    let msgs = agent.state().messages_for_provider();
    let persisted: Vec<String> = msgs
        .iter()
        .filter_map(|m| match &m.content {
            MessageContent::Blocks(b) => Some(b),
            _ => None,
        })
        .flatten()
        .filter_map(|b| match b {
            ContentBlock::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect();
    assert!(
        persisted.iter().any(|c| c.contains("omitted from log")),
        "history should keep the summary, got {persisted:?}"
    );
    assert!(
        !persisted.iter().any(|c| c.contains("FULL_SECRET_CONTENT")),
        "history must not contain the full output, got {persisted:?}"
    );
}

#[tokio::test]
async fn transient_output_does_not_leak_into_a_later_run() {
    // Run 1 stops at the turn cap right after dispatch — no follow-up model
    // call consumes the overlay, so a stale entry survives on the warm agent.
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_use("c1", "leaker"), // run 1, turn 1 → dispatch, then max-turns stop
        text("run 2"),            // run 2, turn 1
    ]));
    let mut agent = Agent::builder(provider.clone(), "u", "s")
        .model("test")
        .max_turns(1)
        .tool(Arc::new(Leaker))
        .build();

    let _ = agent.run("first").await; // may end via max-turns; ignore outcome

    // A second run must not swap run 1's stale full output back into history.
    let _ = agent.run("second").await;

    let last = provider.last_request.lock().unwrap().clone().unwrap();
    let seen = tool_result_contents(&last);
    assert!(
        !seen.iter().any(|c| c == "FULL_SECRET_CONTENT"),
        "stale transient output must not reappear in a later run, got {seen:?}"
    );
}
