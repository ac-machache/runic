use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use runic::ability::ability;
use runic::composer::Composer;
use runic::tool;
use runic::tool::{Tool, ToolContext, ToolResult};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
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

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct AddArgs {
    a: i64,
    b: i64,
}

/// Add two integers and return the sum.
#[tool]
async fn add_numbers(args: AddArgs) -> anyhow::Result<ToolResult> {
    Ok(ToolResult::ok((args.a + args.b).to_string()))
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct WhoArgs {
    greeting: String,
}

/// Greet the current user by id.
#[tool(parallelizable)]
async fn greet_user(args: WhoArgs, ctx: &ToolContext) -> anyhow::Result<ToolResult> {
    Ok(ToolResult::ok(format!("{} {}", args.greeting, ctx.user_id)))
}

/// Report readiness.
#[tool]
async fn ping() -> anyhow::Result<ToolResult> {
    Ok(ToolResult::ok("pong"))
}

#[test]
fn the_macro_derives_name_description_and_schema() {
    let tool = AddNumbers;
    assert_eq!(tool.name(), "add_numbers");
    assert_eq!(tool.description(), "Add two integers and return the sum.");
    assert!(!tool.parallelizable());

    let schema = tool.parameters_schema();
    assert!(schema.get("$schema").is_none());
    assert!(schema.get("title").is_none());
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["properties"]["a"]["type"], "integer");
    assert_eq!(schema["properties"]["b"]["type"], "integer");
    let required = schema["required"].as_array().unwrap();
    assert!(required.iter().any(|value| value == "a"));
    assert!(required.iter().any(|value| value == "b"));
}

#[test]
fn the_parallelizable_attribute_is_honored() {
    assert!(GreetUser.parallelizable());
    assert!(!Ping.parallelizable());
}

#[tokio::test]
async fn a_macro_tool_executes_with_typed_args() {
    let ctx = ToolContext::new("alice", "s1", "r1");
    let result = AddNumbers
        .execute(serde_json::json!({ "a": 19, "b": 23 }), &ctx)
        .await
        .unwrap();
    assert!(result.success);
    assert_eq!(result.output, "42");
}

#[tokio::test]
async fn a_macro_tool_receives_the_context() {
    let ctx = ToolContext::new("alice", "s1", "r1");
    let result = GreetUser
        .execute(serde_json::json!({ "greeting": "hey" }), &ctx)
        .await
        .unwrap();
    assert_eq!(result.output, "hey alice");
}

#[tokio::test]
async fn a_no_args_macro_tool_ignores_input() {
    let ctx = ToolContext::new("alice", "s1", "r1");
    let result = Ping
        .execute(serde_json::json!({ "junk": true }), &ctx)
        .await
        .unwrap();
    assert_eq!(result.output, "pong");
}

#[tokio::test]
async fn invalid_arguments_come_back_as_an_in_band_tool_error() {
    let ctx = ToolContext::new("alice", "s1", "r1");
    let result = AddNumbers
        .execute(serde_json::json!({ "a": "not a number" }), &ctx)
        .await
        .unwrap();
    assert!(!result.success);
    assert!(
        result
            .output
            .contains("invalid arguments for `add_numbers`")
    );
}

#[tokio::test]
async fn a_macro_tool_runs_end_to_end_through_the_composer() {
    let provider = ScriptedProvider::new(vec![
        call("add_numbers", serde_json::json!({ "a": 2, "b": 3 })),
        text("done"),
    ]);
    let mut agent = Composer::new(provider, "test-model")
        .instructions("core")
        .with(ability("math").tool(AddNumbers))
        .build("alice", "s1")
        .await
        .unwrap();

    let outcome = agent.run("add 2 and 3").await.unwrap();
    assert_eq!(outcome.stop_reason.as_deref(), Some("end_turn"));

    let executed = agent.state().events().iter().any(|event| {
        matches!(event, runic_state::SessionEvent::Message { msg, .. }
            if matches!(&msg.content, runic_types::MessageContent::Blocks(blocks)
                if blocks.iter().any(|block| matches!(block,
                    ContentBlock::ToolResult { content, .. } if content == "5"))))
    });
    assert!(executed, "macro tool result must land in the log");
}
