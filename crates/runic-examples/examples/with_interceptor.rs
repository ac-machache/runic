//! Tool interceptors: a `ToolInterceptor` rides *with* the tool (not the
//! agent), so it fires for whichever agent invokes it — parent or sub-agent.
//! Here one stamps the per-run `user_id` into every matching call's input
//! before it executes, and another can short-circuit a call outright.
//!
//! ```sh
//! ANTHROPIC_API_KEY=sk-... cargo run --example with_interceptor
//! ```

use anyhow::{Context, Result};
use async_trait::async_trait;
use runic_agent_core::{Agent, RunContext};
use runic_message_types::ToolCall;
use runic_provider_anthropic::{AnthropicConfig, AnthropicProvider};
use runic_provider_core::Provider;
use runic_tool_core::{Tool, ToolContext, ToolInterceptor, ToolRegistry, ToolResult};
use std::sync::Arc;

// ─── A tool that simply echoes the (possibly rewritten) input ────────────────

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echoes its JSON input back. Call it with any object."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "additionalProperties": true })
    }
    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        ToolResult::ok(format!("echo received: {input}"))
    }
}

// ─── An interceptor that injects identity from the per-run context ───────────

struct StampUser;

#[async_trait]
impl ToolInterceptor for StampUser {
    // Return `Some(result)` to short-circuit (the tool never runs); `None`
    // to proceed (optionally after mutating `call`).
    async fn before(&self, call: &mut ToolCall, ctx: &ToolContext) -> Option<ToolResult> {
        if let Some(uid) = ctx.config("user_id").and_then(|v| v.as_str())
            && let Some(obj) = call.input.as_object_mut()
        {
            obj.insert("_user_id".into(), serde_json::Value::String(uid.to_string()));
        }
        None // proceed to the tool with the stamped input
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .context("ANTHROPIC_API_KEY environment variable must be set")?;
    let provider: Arc<dyn Provider> = AnthropicProvider::new(AnthropicConfig::new(api_key));

    // Build a pool, then wrap matching tools in place. The wrapped dispatch is
    // what gets cloned into sub-agent pools, so the binding follows the tool.
    let mut pool = ToolRegistry::new();
    pool.register(Arc::new(EchoTool));
    let stamp: Arc<dyn ToolInterceptor> = Arc::new(StampUser);
    pool.intercept(|name| name == "echo", vec![stamp]);

    let mut agent = Agent::builder(provider)
        .system_prompt(
            "You have an `echo` tool. When asked, call echo with {\"msg\":\"hi\"} \
             and report exactly what it returned.",
        )
        .tools(pool)
        .build();

    // The model never sets `_user_id` — the interceptor injects it from the
    // per-run context, so the echoed payload includes it.
    let ctx = RunContext::new().with_config("user_id", "u_42");
    let outcome = agent.run_with("Call the echo tool with msg=hi.", ctx).await?;

    println!("[turns={}, stop={:?}]", outcome.total_turns, outcome.stop_reason);
    if let Some(text) = agent.state().last_assistant_text() {
        println!("\nassistant: {text}");
        println!("(the echoed object should contain an injected \"_user_id\":\"u_42\")");
    }
    Ok(())
}
