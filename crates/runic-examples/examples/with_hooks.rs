//! Agent + a Hook that logs every lifecycle event to stderr. Useful for
//! seeing exactly how the run loop works in practice.
//!
//! ```sh
//! ANTHROPIC_API_KEY=sk-... cargo run --example with_hooks
//! ```

use anyhow::{Context, Result};
use async_trait::async_trait;
use runic_agent_core::{Agent, AgentState, Hook, HookOutcome};
use runic_message_types::ToolCall;
use runic_provider_anthropic::{AnthropicConfig, AnthropicProvider};
use runic_provider_core::Provider;
use runic_tool_core::ToolResult;
use std::sync::Arc;

// ─── A noisy logging hook ────────────────────────────────────────────────────

struct LoggingHook;

#[async_trait]
impl Hook for LoggingHook {
    fn name(&self) -> &'static str {
        "logging"
    }

    async fn before_agent(&self, state: &mut AgentState) -> HookOutcome {
        eprintln!("[hook] before_agent  | events_so_far={}", state.events.len());
        HookOutcome::Continue
    }

    async fn after_agent(&self, state: &mut AgentState) -> HookOutcome {
        eprintln!("[hook] after_agent   | events_now={}", state.events.len());
        HookOutcome::Continue
    }

    async fn before_model(&self, _state: &mut AgentState) -> HookOutcome {
        eprintln!("[hook] before_model");
        HookOutcome::Continue
    }

    async fn after_model(
        &self,
        _state: &mut AgentState,
        stop_reason: Option<&str>,
    ) -> HookOutcome {
        eprintln!("[hook] after_model   | stop={:?}", stop_reason);
        HookOutcome::Continue
    }

    async fn before_tool(
        &self,
        _state: &mut AgentState,
        call: &mut ToolCall,
    ) -> HookOutcome {
        eprintln!(
            "[hook] before_tool   | tool={} input={}",
            call.name,
            serde_json::to_string(&call.input).unwrap_or_default()
        );
        HookOutcome::Continue
    }

    async fn after_tool(
        &self,
        _state: &mut AgentState,
        call: &ToolCall,
        result: &ToolResult,
    ) -> HookOutcome {
        eprintln!(
            "[hook] after_tool    | tool={} is_error={} content_chars={}",
            call.name,
            result.is_error,
            result.content.chars().count()
        );
        HookOutcome::Continue
    }
}

// ─── Driver ──────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .context("ANTHROPIC_API_KEY environment variable must be set")?;
    let provider: Arc<dyn Provider> = AnthropicProvider::new(AnthropicConfig::new(api_key));

    let mut agent = Agent::builder(provider)
        .system_prompt("Reply with one sentence about the weather today.")
        .hook(Arc::new(LoggingHook))
        .build();

    eprintln!("=== run begins ===\n");
    let outcome = agent.run("hello").await?;
    eprintln!("\n=== run ends: turns={}, stop={:?} ===", outcome.total_turns, outcome.stop_reason);

    if let Some(text) = agent.state().last_assistant_text() {
        println!("\nassistant: {text}");
    }
    Ok(())
}
