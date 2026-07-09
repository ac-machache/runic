use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use runic_agent::Agent;
use runic_hook::{HookOutcome, WriteHook};
use runic_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use runic_serve::{AgentFactory, BoxedAgentFactory, single_agent};
use runic_state::AgentState;
use runic_tool::{Tool, ToolContext, ToolResult};
use runic_types::{ContentBlock, MessageContent, Role, StopReason, TokenUsage, ToolCall};

pub fn dummy_agents(real_mistral: bool) -> HashMap<String, BoxedAgentFactory> {
    single_agent("main", Arc::new(DummyFactory { real_mistral }))
}

pub struct DummyFactory {
    pub real_mistral: bool,
}

#[async_trait]
impl AgentFactory for DummyFactory {
    async fn build(&self, tenant: &str, session_id: &str) -> Agent {
        let provider: Arc<dyn Provider> = if self.real_mistral {
            let key = std::env::var("MISTRAL_API_KEY").unwrap_or_default();
            Arc::new(runic_provider::mistral::MistralDriver::new(key))
        } else {
            Arc::new(ScriptedProvider)
        };
        let model = std::env::var("RUNIC_MODEL").unwrap_or_else(|_| "mistral-medium-latest".into());
        Agent::builder(provider, tenant, session_id)
            .model(model)
            .system_prompt("e2e harness agent")
            .tool(Arc::new(AddTool))
            .tool(Arc::new(SlowTool))
            .tool(Arc::new(EchoTool))
            .tool(Arc::new(FailTool))
            .tool(Arc::new(runic_tools::AskUserTool))
            .write_hook(Arc::new(MarkerHook))
            .build()
    }
}

pub struct MarkerHook;

#[async_trait]
impl WriteHook for MarkerHook {
    fn name(&self) -> &str {
        "marker"
    }
    async fn before_agent(&self, state: &mut AgentState) -> HookOutcome {
        let _ = state.update("hook_fired", serde_json::json!("before_agent"));
        HookOutcome::Continue
    }
}

pub struct ScriptedProvider;

#[async_trait]
impl Provider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted"
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        let just_ran_tool = req.messages.last().and_then(|m| match &m.content {
            MessageContent::Blocks(b) => b.iter().rev().find_map(|x| match x {
                ContentBlock::ToolResult { content, .. } => Some(content.clone()),
                _ => None,
            }),
            _ => None,
        });
        if let Some(result) = just_ran_tool {
            return Ok(text_response(format!("done: {result}")));
        }

        let directive = req
            .messages
            .iter()
            .rev()
            .find_map(|m| match (&m.role, &m.content) {
                (Role::User, MessageContent::Text(t)) => Some(t.clone()),
                (Role::User, MessageContent::Blocks(b)) => b.iter().find_map(|x| match x {
                    ContentBlock::Text { text, .. } => Some(text.clone()),
                    _ => None,
                }),
                _ => None,
            })
            .unwrap_or_default();

        Ok(interpret(directive.trim()))
    }
}

fn interpret(directive: &str) -> CompletionResponse {
    if directive == "fail" {
        return tool_call("fail", serde_json::json!({}));
    }
    match directive.split_once(':') {
        Some(("add", rest)) => {
            let (a, b) = rest.split_once(',').unwrap_or(("0", "0"));
            tool_call(
                "add",
                serde_json::json!({
                    "a": a.trim().parse::<i64>().unwrap_or(0),
                    "b": b.trim().parse::<i64>().unwrap_or(0),
                }),
            )
        }
        Some(("slow", ms)) => tool_call(
            "slow",
            serde_json::json!({ "ms": ms.trim().parse::<u64>().unwrap_or(0) }),
        ),
        Some(("echo", text)) => tool_call("echo", serde_json::json!({ "text": text })),
        Some(("ask", question)) => {
            tool_call("ask_user", serde_json::json!({ "question": question }))
        }
        Some(("say", text)) => text_response(text.to_string()),
        _ => text_response(if directive.is_empty() {
            "ok".to_string()
        } else {
            directive.to_string()
        }),
    }
}

fn tool_call(name: &str, input: serde_json::Value) -> CompletionResponse {
    CompletionResponse {
        content: vec![ContentBlock::ToolUse {
            id: "call-1".into(),
            name: name.into(),
            input: input.clone(),
            provider_metadata: None,
        }],
        stop_reason: StopReason::ToolUse,
        tool_calls: vec![ToolCall {
            id: "call-1".into(),
            name: name.into(),
            input,
        }],
        usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
        },
    }
}

fn text_response(text: String) -> CompletionResponse {
    CompletionResponse {
        content: vec![ContentBlock::Text {
            text,
            provider_metadata: None,
        }],
        stop_reason: StopReason::EndTurn,
        tool_calls: vec![],
        usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
        },
    }
}

pub struct AddTool;

#[async_trait]
impl Tool for AddTool {
    fn name(&self) -> &str {
        "add"
    }
    fn description(&self) -> &str {
        "add two integers a and b"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "a": { "type": "integer" }, "b": { "type": "integer" } },
            "required": ["a", "b"]
        })
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let a = args.get("a").and_then(|v| v.as_i64()).unwrap_or(0);
        let b = args.get("b").and_then(|v| v.as_i64()).unwrap_or(0);
        Ok(ToolResult::ok((a + b).to_string()))
    }
}

pub struct SlowTool;

#[async_trait]
impl Tool for SlowTool {
    fn name(&self) -> &str {
        "slow"
    }
    fn description(&self) -> &str {
        "sleep for ms milliseconds, then return"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "ms": { "type": "integer" } },
            "required": ["ms"]
        })
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let ms = args
            .get("ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .min(60_000);
        tokio::time::sleep(Duration::from_millis(ms)).await;
        Ok(ToolResult::ok(format!("slept {ms}ms")))
    }
}

pub struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "echo the given text back"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"]
        })
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::ok(
            args.get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        ))
    }
}

pub struct FailTool;

#[async_trait]
impl Tool for FailTool {
    fn name(&self) -> &str {
        "fail"
    }
    fn description(&self) -> &str {
        "always returns an error result"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        Ok(ToolResult::error("intentional failure"))
    }
}
