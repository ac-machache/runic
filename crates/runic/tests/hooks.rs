use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU32, Ordering},
};

use async_trait::async_trait;
use runic::builtin::ToolCallLimit;
use runic_agent::Runner;
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
    async fn complete(
        &self,
        _request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ProviderError::Parse("scripted provider exhausted".into()))
    }
}

fn text_response(text: &str) -> CompletionResponse {
    CompletionResponse {
        content: vec![ContentBlock::Text {
            text: text.into(),
            provider_metadata: None,
        }],
        stop_reason: StopReason::EndTurn,
        tool_calls: vec![],
        usage: TokenUsage::default(),
    }
}

fn tool_use_response(id: &str, name: &str) -> CompletionResponse {
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

struct PaymentTool {
    executions: Arc<AtomicU32>,
}

#[async_trait]
impl Tool for PaymentTool {
    fn name(&self) -> &str {
        "payment"
    }

    fn description(&self) -> &str {
        "charge the customer"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult::ok("charged"))
    }
}

fn agent_with_limit(provider: Arc<ScriptedProvider>, executions: Arc<AtomicU32>) -> Runner {
    Runner::builder(provider, "u1", "s1")
        .system_prompt("sys")
        .tool(Arc::new(PaymentTool { executions }))
        .write_hook(Arc::new(ToolCallLimit::new().per_thread("payment", 2)))
        .build()
}

#[tokio::test]
async fn the_loop_blocks_a_thread_capped_tool_and_the_model_sees_why() {
    let provider = ScriptedProvider::new(vec![
        tool_use_response("t1", "payment"),
        tool_use_response("t2", "payment"),
        tool_use_response("t3", "payment"),
        text_response("done"),
    ]);
    let executions = Arc::new(AtomicU32::new(0));
    let mut agent = agent_with_limit(provider, executions.clone());

    let outcome = agent
        .run_message(runic_types::Message::user("charge them three times"))
        .await
        .unwrap();
    assert_eq!(outcome.total_turns, 4);
    assert_eq!(executions.load(Ordering::SeqCst), 2);

    let blocked: Vec<_> = agent
        .state()
        .messages_for_provider()
        .iter()
        .flat_map(|m| match &m.content {
            runic_types::MessageContent::Blocks(blocks) => blocks.clone(),
            _ => vec![],
        })
        .filter_map(|b| match b {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => Some((content.text(), is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(blocked.len(), 3);
    assert!(blocked[0].0.contains("charged") && !blocked[0].1);
    assert!(blocked[2].0.contains("2/2 this thread") && blocked[2].1);
}

#[tokio::test]
async fn the_thread_cap_holds_across_runs() {
    let provider = ScriptedProvider::new(vec![
        tool_use_response("t1", "payment"),
        text_response("first done"),
        tool_use_response("t2", "payment"),
        text_response("second done"),
        tool_use_response("t3", "payment"),
        text_response("third done"),
    ]);
    let executions = Arc::new(AtomicU32::new(0));
    let mut agent = agent_with_limit(provider, executions.clone());

    agent
        .run_message(runic_types::Message::user("one"))
        .await
        .unwrap();
    agent
        .run_message(runic_types::Message::user("two"))
        .await
        .unwrap();
    assert_eq!(executions.load(Ordering::SeqCst), 2);

    agent
        .run_message(runic_types::Message::user("three"))
        .await
        .unwrap();
    assert_eq!(executions.load(Ordering::SeqCst), 2);
}
