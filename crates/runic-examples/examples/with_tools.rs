//! Agent + a custom Tool. Shows the minimum needed to give the model
//! a new capability. The tool here just counts the letters in its input.
//!
//! ```sh
//! ANTHROPIC_API_KEY=sk-... cargo run --example with_tools
//! ```

use anyhow::{Context, Result};
use async_trait::async_trait;
use runic_agent_core::Agent;
use runic_provider_anthropic::{AnthropicConfig, AnthropicProvider};
use runic_provider_core::Provider;
use runic_tool_core::{Tool, ToolContext, ToolResult};
use std::sync::Arc;

// ─── A trivial Tool ──────────────────────────────────────────────────────────

struct CountLettersTool;

#[async_trait]
impl Tool for CountLettersTool {
    fn name(&self) -> &str {
        "count_letters"
    }

    fn description(&self) -> &str {
        "Counts the number of letters (alphabetic characters) in the given text."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "The text whose letters should be counted."
                }
            },
            "required": ["text"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        let Some(text) = input.get("text").and_then(|v| v.as_str()) else {
            return ToolResult::error("missing required field 'text'");
        };
        let n = text.chars().filter(|c| c.is_alphabetic()).count();
        ToolResult::ok(format!("{n} letters in {text:?}"))
    }
}

// ─── Driver ──────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .context("ANTHROPIC_API_KEY environment variable must be set")?;
    let provider: Arc<dyn Provider> = AnthropicProvider::new(AnthropicConfig::new(api_key));

    let mut agent = Agent::builder(provider)
        .system_prompt(
            "You are a helpful assistant with access to a `count_letters` tool. \
             Use it when the user asks about letter counts.",
        )
        .tool(Arc::new(CountLettersTool))
        .build();

    let outcome = agent
        .run("How many letters are in the word 'mississippi'?")
        .await?;

    println!(
        "\n[done: {} turn(s), stop={:?}]",
        outcome.total_turns, outcome.stop_reason
    );
    if let Some(text) = agent.state().last_assistant_text() {
        println!("\nassistant: {text}");
    }

    Ok(())
}
