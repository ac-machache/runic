use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use runic::hooks::HookAgent;
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_tool::{Tool, ToolContext, ToolResult};
use runic_types::{ContentBlock, StopReason, TokenUsage, ToolCall};

struct ScriptedProvider {
    responses: Mutex<VecDeque<CompletionResponse>>,
}

impl ScriptedProvider {
    fn new(responses: Vec<CompletionResponse>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into()),
        })
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ProviderError::Parse("scripted provider exhausted".into()))
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

fn call(name: &str, input: serde_json::Value) -> CompletionResponse {
    CompletionResponse {
        content: vec![ContentBlock::ToolUse {
            id: "c1".into(),
            name: name.into(),
            input: input.clone(),
            provider_metadata: None,
        }],
        stop_reason: StopReason::ToolUse,
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: name.into(),
            input,
        }],
        usage: TokenUsage::default(),
    }
}

struct Recorder(Arc<Mutex<Vec<String>>>);

#[async_trait]
impl Tool for Recorder {
    fn name(&self) -> &str {
        "record"
    }
    fn description(&self) -> &str {
        "records a fact"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        self.0.lock().unwrap().push(args.to_string());
        Ok(ToolResult::ok("stored"))
    }
}

#[tokio::test]
async fn returns_the_final_assistant_text() {
    let provider = ScriptedProvider::new(vec![text("curated 3 facts")]);
    let out = HookAgent::new(provider, "m")
        .prompt("review the transcript")
        .run("User: hi\nAssistant: hello")
        .await
        .unwrap();
    assert_eq!(out, "curated 3 facts");
}

#[tokio::test]
async fn a_tool_wires_through_and_runs() {
    let provider = ScriptedProvider::new(vec![
        call("record", serde_json::json!({ "fact": "launch is friday" })),
        text("done"),
    ]);
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let out = HookAgent::new(provider, "m")
        .prompt("curate")
        .tool(Arc::new(Recorder(recorded.clone())))
        .run("transcript")
        .await
        .unwrap();

    assert_eq!(out, "done");
    let recorded = recorded.lock().unwrap();
    assert_eq!(recorded.len(), 1);
    assert!(recorded[0].contains("launch is friday"));
}
