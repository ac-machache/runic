//! The smallest possible runic agent — no tools, no hooks, no skills,
//! no context engine. Just: provider in, conversation out.
//!
//! ```sh
//! ANTHROPIC_API_KEY=sk-... cargo run --example minimal -- "hi there"
//! ```

use anyhow::{Context, Result};
use runic_agent_core::Agent;
use runic_provider_anthropic::{AnthropicConfig, AnthropicProvider};
use runic_provider_core::Provider;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .context("ANTHROPIC_API_KEY environment variable must be set")?;
    let prompt = std::env::args().nth(1).unwrap_or_else(|| "hi".to_string());

    // Build the simplest provider — default model, no custom settings.
    let provider: Arc<dyn Provider> = AnthropicProvider::new(AnthropicConfig::new(api_key));

    // Build the agent. system_prompt() and no tools are the minimum config.
    let mut agent = Agent::builder(provider)
        .system_prompt("You are a friendly, terse assistant. Reply in one sentence.")
        .build();

    // Run a single turn to completion.
    let outcome = agent.run(&prompt).await?;
    println!();
    println!(
        "[done: {} turn(s), stop={:?}]",
        outcome.total_turns, outcome.stop_reason
    );

    // The assistant's reply lives in the state's last assistant message.
    if let Some(text) = agent.state().last_assistant_text() {
        println!("\nassistant: {text}");
    }

    Ok(())
}
