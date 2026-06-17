//! Per-run context: pass request-scoped values (user_id, locale, a provider
//! override, …) into a single run via [`RunContext`], and read them back from
//! a context layer AND a tool. The agent's *model* never sees these keys —
//! only your hooks / tools / context layers do.
//!
//! ```sh
//! ANTHROPIC_API_KEY=sk-... cargo run --example with_run_context
//! ```

use anyhow::{Context, Result};
use async_trait::async_trait;
use runic_agent_core::{Agent, RunContext};
use runic_context_engine::{BasePromptLayer, CompositeEngine, ContextLayer, TurnContext};
use runic_provider_anthropic::{AnthropicConfig, AnthropicProvider};
use runic_provider_core::Provider;
use runic_tool_core::{Tool, ToolContext, ToolResult};
use std::sync::Arc;

// ─── A context layer that personalizes the prompt from per-run config ────────

struct GreetByUser;

#[async_trait]
impl ContextLayer for GreetByUser {
    fn name(&self) -> &str {
        "greet-by-user"
    }

    async fn render(&self, ctx: &TurnContext<'_>) -> Option<String> {
        // `ctx.config` is the per-run map. Absent → skip this layer.
        let user = ctx.config.get("user_id").and_then(|v| v.as_str())?;
        Some(format!(
            "You are assisting user `{user}`. Greet them by id once, briefly."
        ))
    }
}

// ─── A tool that reads the same per-run config ───────────────────────────────

struct WhoAmITool;

#[async_trait]
impl Tool for WhoAmITool {
    fn name(&self) -> &str {
        "who_am_i"
    }
    fn description(&self) -> &str {
        "Returns the id of the user this request is running on behalf of."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }
    async fn execute(&self, _input: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        // Same open map the context layer read, reached from the tool side.
        let user = ctx
            .config("user_id")
            .and_then(|v| v.as_str())
            .unwrap_or("anonymous");
        ToolResult::ok(format!("current user is {user}"))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .context("ANTHROPIC_API_KEY environment variable must be set")?;
    let provider: Arc<dyn Provider> = AnthropicProvider::new(AnthropicConfig::new(api_key));

    // The base prompt + the per-user layer live in the context engine; the
    // agent is built ONCE and reused across runs.
    let engine = CompositeEngine::new()
        .with_layer(BasePromptLayer::new("You are a concise assistant."))
        .with_layer(GreetByUser);

    let mut agent = Agent::builder(provider)
        .system_prompt("You are a concise assistant.")
        .context_engine(engine)
        .tool(Arc::new(WhoAmITool))
        .build();

    // Per-run context: an open JSON map (+ an optional provider override via
    // `.with_provider(...)`). Set fresh for THIS run only.
    let ctx = RunContext::new()
        .with_config("user_id", "u_42")
        .with_config("locale", "fr");

    println!("── run 1: with per-run context (user_id = u_42) ──");
    let outcome = agent
        .run_with("Who am I? Use the who_am_i tool to confirm.", ctx)
        .await?;
    println!("[turns={}, stop={:?}]", outcome.total_turns, outcome.stop_reason);
    if let Some(text) = agent.state().last_assistant_text() {
        println!("assistant: {text}\n");
    }

    // A plain `run` carries NO per-run context — the map is overwritten each
    // run, so nothing leaks from the previous one (the layer renders nothing,
    // the tool sees "anonymous").
    println!("── run 2: bare run (no context — proves no leak) ──");
    let outcome = agent.run("Who am I now?").await?;
    println!("[turns={}, stop={:?}]", outcome.total_turns, outcome.stop_reason);
    if let Some(text) = agent.state().last_assistant_text() {
        println!("assistant: {text}");
    }
    Ok(())
}
