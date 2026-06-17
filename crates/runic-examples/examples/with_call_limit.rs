//! Call limits: cap how many times the model may invoke a given tool within a
//! single run, so a frustrated model can't loop on the same tool forever.
//! [`CallLimitHook`] counts from the run's own history (nothing stored, so it
//! never leaks across runs) and, once the cap is hit, hands the model an error
//! result telling it to stop — a soft cap it can react to, not a hard abort.
//!
//! ```sh
//! ANTHROPIC_API_KEY=sk-... cargo run --example with_call_limit
//! ```

use anyhow::{Context, Result};
use async_trait::async_trait;
use runic_agent_core::{Agent, CallLimitHook};
use runic_provider_anthropic::{AnthropicConfig, AnthropicProvider};
use runic_provider_core::Provider;
use runic_tool_core::{Tool, ToolContext, ToolResult};
use std::sync::Arc;

// ─── A tool that never satisfies — tempts the model to keep retrying ─────────

struct FlakySearch;

#[async_trait]
impl Tool for FlakySearch {
    fn name(&self) -> &str {
        "search"
    }
    fn description(&self) -> &str {
        "Searches the web. Returns results for a query."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "query": { "type": "string" } },
            "required": ["query"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, _input: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        // Deliberately unhelpful, to provoke a retry loop.
        ToolResult::ok("no relevant results found")
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .context("ANTHROPIC_API_KEY environment variable must be set")?;
    let provider: Arc<dyn Provider> = AnthropicProvider::new(AnthropicConfig::new(api_key));

    // Cap `search` at 2 calls per run. Builder-style; pass a HashMap to
    // `CallLimitHook::new(..)` to configure several tools at once.
    let limit = CallLimitHook::default().limit("search", 2);

    let mut agent = Agent::builder(provider)
        .system_prompt(
            "You have a `search` tool. Keep searching with different queries until you \
             find the founding year of a fictional company called Zorptech.",
        )
        .tool(Arc::new(FlakySearch))
        .hook(Arc::new(limit))
        .build();

    let outcome = agent
        .run("What year was Zorptech founded? Search for it.")
        .await?;

    println!("[turns={}, stop={:?}]", outcome.total_turns, outcome.stop_reason);
    println!(
        "(after the 2nd `search` the 3rd is refused with an error result, so the model \
         stops looping and answers with what it has)"
    );
    if let Some(text) = agent.state().last_assistant_text() {
        println!("\nassistant: {text}");
    }
    Ok(())
}
