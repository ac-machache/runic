use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use runic::Llm;
use runic::ability::ability;
use runic::composer::Agent;
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
#[tool(args = AddArgs)]
struct AddNumbers;

impl AddNumbers {
    async fn tool(&self, args: AddArgs, _ctx: &ToolContext) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::ok((args.a + args.b).to_string()))
    }
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct WhoArgs {
    greeting: String,
}

/// Greet the current user by id.
#[tool(args = WhoArgs, execution = parallel)]
struct GreetUser;

impl GreetUser {
    async fn tool(&self, args: WhoArgs, ctx: &ToolContext) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::ok(format!("{} {}", args.greeting, ctx.user_id)))
    }
}

/// Report readiness.
#[tool]
struct Ping;

impl Ping {
    async fn tool(&self, _ctx: &ToolContext) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::ok("pong"))
    }
}

#[derive(serde::Deserialize, schemars::JsonSchema, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Units {
    #[default]
    Celsius,
    Fahrenheit,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct ConvertArgs {
    #[serde(default)]
    #[schemars(description = "Temperature units (default celsius).")]
    units: Units,
}

/// Report a temperature.
#[tool(args = ConvertArgs)]
struct Convert;

impl Convert {
    async fn tool(&self, args: ConvertArgs, _ctx: &ToolContext) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::ok(if args.units == Units::Fahrenheit {
            "72F"
        } else {
            "22C"
        }))
    }
}

#[test]
fn a_nested_type_is_inlined_not_left_as_a_ref() {
    let schema = Convert.parameters_schema();
    assert!(
        schema.get("$defs").is_none(),
        "providers do not resolve $ref in tool schemas: {schema}"
    );
    assert_eq!(
        schema["properties"]["units"],
        serde_json::json!({
            "type": "string",
            "enum": ["celsius", "fahrenheit"],
            "description": "Temperature units (default celsius)."
        })
    );
    assert!(schema.get("required").is_none());
}

/// Look a row up in the tenant's catalog.
#[tool(args = LookupArgs)]
struct Lookup {
    rows: Arc<Mutex<Vec<String>>>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct LookupArgs {
    index: usize,
}

impl Lookup {
    async fn tool(&self, args: LookupArgs, _ctx: &ToolContext) -> anyhow::Result<ToolResult> {
        match self.rows.lock().unwrap().get(args.index) {
            Some(row) => Ok(ToolResult::ok(row.clone())),
            None => Ok(ToolResult::error(format!("no row {}", args.index))),
        }
    }
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
fn the_execution_attribute_is_honored() {
    assert!(GreetUser.parallelizable());
    assert!(!Ping.parallelizable());
    assert!(!RenamedTool.parallelizable());
}

/// This doc comment must lose to the attribute.
#[tool(
    name = "mcp__coral__list_tasks",
    description = "lists tasks in the coral workspace",
    execution = serial
)]
struct RenamedTool;

impl RenamedTool {
    async fn tool(&self, _ctx: &ToolContext) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::ok("listed"))
    }
}

#[test]
fn attributes_override_the_type_name_and_the_doc_comment() {
    assert_eq!(RenamedTool.name(), "mcp__coral__list_tasks");
    assert_eq!(
        RenamedTool.description(),
        "lists tasks in the coral workspace"
    );
}

#[tokio::test]
async fn a_macro_tool_executes_with_typed_args() {
    let ctx = ToolContext::new("alice", "s1", "r1");
    let result = AddNumbers
        .execute(serde_json::json!({ "a": 19, "b": 23 }), &ctx)
        .await
        .unwrap();
    assert!(!result.is_error());
    assert_eq!(result.text(), "42");
}

#[tokio::test]
async fn a_macro_tool_receives_the_context() {
    let ctx = ToolContext::new("alice", "s1", "r1");
    let result = GreetUser
        .execute(serde_json::json!({ "greeting": "hey" }), &ctx)
        .await
        .unwrap();
    assert_eq!(result.text(), "hey alice");
}

#[tokio::test]
async fn a_no_args_macro_tool_ignores_input() {
    let ctx = ToolContext::new("alice", "s1", "r1");
    let result = Ping
        .execute(serde_json::json!({ "junk": true }), &ctx)
        .await
        .unwrap();
    assert_eq!(result.text(), "pong");
}

#[tokio::test]
async fn invalid_arguments_come_back_as_an_in_band_tool_error() {
    let ctx = ToolContext::new("alice", "s1", "r1");
    let result = AddNumbers
        .execute(serde_json::json!({ "a": "not a number" }), &ctx)
        .await
        .unwrap();
    assert!(result.is_error());
    assert!(
        result
            .text()
            .contains("invalid arguments for `add_numbers`")
    );
}

#[tokio::test]
async fn a_tool_can_hold_state_across_calls() {
    let rows = Arc::new(Mutex::new(vec!["alpha".to_string(), "beta".to_string()]));
    let tool = Lookup { rows: rows.clone() };
    let ctx = ToolContext::new("alice", "s1", "r1");

    assert_eq!(tool.name(), "lookup");
    assert_eq!(tool.description(), "Look a row up in the tenant's catalog.");

    let hit = tool
        .execute(serde_json::json!({ "index": 1 }), &ctx)
        .await
        .unwrap();
    assert_eq!(hit.text(), "beta");

    rows.lock().unwrap().push("gamma".into());
    let fresh = tool
        .execute(serde_json::json!({ "index": 2 }), &ctx)
        .await
        .unwrap();
    assert_eq!(fresh.text(), "gamma");

    let miss = tool
        .execute(serde_json::json!({ "index": 9 }), &ctx)
        .await
        .unwrap();
    assert!(miss.is_error());
}

#[tokio::test]
async fn a_macro_tool_runs_end_to_end_through_the_composer() {
    let provider = ScriptedProvider::new(vec![
        call("add_numbers", serde_json::json!({ "a": 2, "b": 3 })),
        text("done"),
    ]);
    let mut agent = Agent::new(Llm::new(provider, "test-model").instructions("core"))
        .with(ability("math").tool(AddNumbers))
        .build("alice", "s1")
        .await
        .unwrap();

    let outcome = agent.run("add 2 and 3").await.unwrap();
    assert_eq!(outcome.stop_reason.as_deref(), Some("end_turn"));

    let executed = agent.state().messages_for_provider().iter().any(|msg| {
        matches!(&msg.content, runic_types::MessageContent::Blocks(blocks)
            if blocks.iter().any(|block| matches!(block,
                ContentBlock::ToolResult { content, .. } if content.text() == "5")))
    });
    assert!(executed, "macro tool result must land in the log");
}
