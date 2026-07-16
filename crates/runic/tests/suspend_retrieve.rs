use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use runic::ability::ability;
use runic::composer::Composer;
use runic_agent::RunContext;
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_state::SessionEvent;
use runic_substrate::MemoryArtifactStore;
use runic_tool::{Tool, ToolContext, ToolResult};
use runic_types::{
    ContentBlock, Message, MessageContent, StopReason, TokenUsage, ToolCall, ToolResultPayload,
};

const FULL: &str = "THE_FULL_PAYLOAD the model must be able to re-read after resume";

struct ScriptedProvider {
    responses: Mutex<VecDeque<CompletionResponse>>,
    requests: Mutex<Vec<CompletionRequest>>,
}

impl ScriptedProvider {
    fn new(responses: Vec<CompletionResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn push_response(&self, response: CompletionResponse) {
        self.responses.lock().unwrap().push_back(response);
    }

    fn last_request(&self) -> CompletionRequest {
        self.requests.lock().unwrap().last().unwrap().clone()
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        self.requests.lock().unwrap().push(req);
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ProviderError::Parse("exhausted".into()))
    }
}

fn tool_use(calls: Vec<(&str, &str, serde_json::Value)>) -> CompletionResponse {
    CompletionResponse {
        content: calls
            .iter()
            .map(|(id, name, input)| ContentBlock::ToolUse {
                id: (*id).into(),
                name: (*name).into(),
                input: input.clone(),
                provider_metadata: None,
            })
            .collect(),
        stop_reason: StopReason::ToolUse,
        tool_calls: calls
            .into_iter()
            .map(|(id, name, input)| ToolCall {
                id: id.into(),
                name: name.into(),
                input,
            })
            .collect(),
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

struct BigTool;

#[async_trait]
impl Tool for BigTool {
    fn name(&self) -> &str {
        "big"
    }
    fn description(&self) -> &str {
        "returns a summarized payload"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    async fn execute(&self, _a: serde_json::Value, _c: &ToolContext) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::ok(FULL).with_summary("short summary"))
    }
}

struct AskTool;

#[async_trait]
impl Tool for AskTool {
    fn name(&self) -> &str {
        "ask"
    }
    fn description(&self) -> &str {
        "asks the human and suspends"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    async fn execute(&self, _a: serde_json::Value, _c: &ToolContext) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::defer("human_ask", serde_json::json!({})))
    }
}

#[tokio::test]
async fn a_suspended_spill_is_retrievable_through_the_auto_wired_reader_after_resume() {
    let provider = Arc::new(ScriptedProvider::new(vec![tool_use(vec![
        ("c1", "big", serde_json::json!({})),
        ("c2", "ask", serde_json::json!({})),
    ])]));
    let store = Arc::new(MemoryArtifactStore::new());
    let mut agent = Composer::new(provider.clone(), "m")
        .instructions("core")
        .with(ability("work").tool(BigTool).tool(AskTool))
        .artifacts(store)
        .build("alice", "s1")
        .await
        .unwrap();

    let out = agent
        .run_with("go", RunContext::new().with_run_id("r1"))
        .await
        .unwrap();
    assert_eq!(out.stop_reason.as_deref(), Some("suspended"));

    let artifact_id = agent
        .state()
        .events()
        .iter()
        .find_map(|event| match event {
            SessionEvent::Message { msg, .. } => match &msg.content {
                MessageContent::Blocks(blocks) => blocks.iter().find_map(|block| match block {
                    ContentBlock::ToolResult {
                        content: ToolResultPayload::Artifact { id, preview, .. },
                        ..
                    } => {
                        assert_eq!(preview, "short summary");
                        Some(id.clone())
                    }
                    _ => None,
                }),
                _ => None,
            },
            _ => None,
        })
        .expect("the summarized output was spilled to an artifact");

    provider.push_response(tool_use(vec![(
        "c3",
        "read_thread_artifact",
        serde_json::json!({ "artifact_id": artifact_id }),
    )]));
    provider.push_response(text("done"));

    agent.state_mut().push_event(SessionEvent::Message {
        run_id: "r1".into(),
        msg: Message::user_with_blocks(vec![ContentBlock::ToolResult {
            tool_use_id: "c2".into(),
            tool_name: "ask".into(),
            content: "human says yes".into(),
            is_error: false,
            provenance: Vec::new(),
        }]),
        at: chrono::Utc::now(),
    });
    let resumed = agent
        .resume(RunContext::new().with_run_id("r1"))
        .await
        .unwrap();
    assert_eq!(resumed.stop_reason.as_deref(), Some("end_turn"));

    let final_request = provider.last_request();
    let saw_full = final_request.messages.iter().any(|msg| {
        matches!(&msg.content, MessageContent::Blocks(blocks)
            if blocks.iter().any(|block| matches!(block,
                ContentBlock::ToolResult { content, .. } if content.text().contains(FULL))))
    });
    assert!(
        saw_full,
        "the model retrieved the full spilled output via read_thread_artifact"
    );
}
