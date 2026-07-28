use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use runic::ability::ability;
use runic::composer::Agent;
use runic::hook::{HookLifecycle, HookOutcome, HookSignal, ReadHook, WriteHook};
use runic::state::AgentState;
use runic::types::ToolCall;
use runic::{Llm, hook};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_tool::{Tool, ToolContext, ToolResult};
use runic_types::{ContentBlock, StopReason, TokenUsage};

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
            .ok_or_else(|| ProviderError::Parse("exhausted".into()))
    }
}

fn text(content: &str) -> CompletionResponse {
    CompletionResponse {
        content: vec![ContentBlock::Text {
            text: content.into(),
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

struct EchoArgs;

#[async_trait]
impl Tool for EchoArgs {
    fn name(&self) -> &str {
        "echo_args"
    }
    fn description(&self) -> &str {
        "echoes the arguments it received"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::ok(args.to_string()))
    }
}

#[hook(kind = write, at = before_tool, name = "inject-user-id", priority = 7)]
async fn inject_user_id(_state: &mut AgentState, call: &mut ToolCall) -> HookOutcome {
    if let serde_json::Value::Object(map) = &mut call.input {
        map.insert("user_id".into(), serde_json::json!("u-1"));
    }
    HookOutcome::Continue
}

#[hook(kind = read, at = before_model)]
async fn observe_model(_state: &AgentState) -> HookSignal {
    HookSignal::Continue
}

#[test]
fn the_generated_impl_carries_name_priority_and_a_single_point() {
    assert_eq!(WriteHook::name(&InjectUserId), "inject-user-id");
    assert_eq!(WriteHook::priority(&InjectUserId), 7);
    assert_eq!(
        WriteHook::points(&InjectUserId),
        &[HookLifecycle::BeforeTool]
    );
}

#[test]
fn defaults_fall_back_to_the_fn_name_and_zero_priority() {
    assert_eq!(ReadHook::name(&ObserveModel), "observe_model");
    assert_eq!(ReadHook::priority(&ObserveModel), 0);
    assert_eq!(
        ReadHook::points(&ObserveModel),
        &[HookLifecycle::BeforeModel]
    );
}

#[tokio::test]
async fn a_macro_written_hook_actually_fires_in_the_loop() {
    let provider =
        ScriptedProvider::new(vec![call("echo_args", serde_json::json!({})), text("done")]);

    let mut agent = Agent::new(Llm::new(provider, "test-model"))
        .with(ability("echo").tool(EchoArgs).hook(InjectUserId))
        .build("alice", "s1")
        .await
        .unwrap();

    agent.run("go").await.unwrap();

    let echoed = agent
        .state()
        .messages_for_provider()
        .iter()
        .filter_map(|msg| match &msg.content {
            runic_types::MessageContent::Blocks(blocks) => Some(blocks),
            _ => None,
        })
        .flatten()
        .find_map(|block| match block {
            ContentBlock::ToolResult { content, .. } => Some(content.text()),
            _ => None,
        })
        .expect("the tool ran");

    assert!(
        echoed.contains("\"user_id\":\"u-1\""),
        "the macro-written before_tool hook injected into the live call: {echoed}"
    );
}
