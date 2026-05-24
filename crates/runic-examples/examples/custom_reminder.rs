//! How to write your own `Reminder` and plug it into the `ReminderEngine`.
//!
//! This example builds a `TurnCounterReminder` that injects a note every
//! turn telling the model which turn it is. Pointless in practice but
//! shows the full mechanic in ~50 lines.
//!
//! Run with a real API key to see the ambient notes show up:
//! ```sh
//! ANTHROPIC_API_KEY=sk-... cargo run --example custom_reminder
//! ```

use anyhow::{Context, Result};
use async_trait::async_trait;
use runic_agent_core::Agent;
use runic_context_engine::{
    AmbientNote, BasePromptLayer, CompositeEngine, ContextEngine, Reminder, ReminderEngine,
    TurnContext,
};
use runic_provider_anthropic::{AnthropicConfig, AnthropicProvider};
use runic_provider_core::Provider;
use std::sync::Arc;

// ─── A trivial custom reminder ───────────────────────────────────────────────

#[derive(Debug)]
struct TurnCounterReminder;

#[async_trait]
impl Reminder for TurnCounterReminder {
    fn name(&self) -> &str {
        "turn-counter"
    }

    async fn collect(&self, ctx: &TurnContext<'_>) -> Vec<AmbientNote> {
        // No dedup_key means this fires every turn.
        vec![AmbientNote {
            source: "turn-counter".into(),
            content: format!(
                "Tracking note: this is turn {} of run {}.",
                ctx.turn, ctx.run_id
            ),
            dedup_key: None,
        }]
    }
}

// ─── Driver ──────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .context("ANTHROPIC_API_KEY environment variable must be set")?;
    let provider: Arc<dyn Provider> = AnthropicProvider::new(AnthropicConfig::new(api_key));

    // Build a composite engine, then wrap it in a ReminderEngine that
    // holds our TurnCounterReminder.
    let composite = CompositeEngine::new().with_layer(BasePromptLayer::new(
        "You are a focused assistant. Reply in one sentence.",
    ));

    let inner: Arc<dyn ContextEngine> = Arc::new(composite);
    let engine: Arc<dyn ContextEngine> =
        Arc::new(ReminderEngine::new(inner).with_reminder(TurnCounterReminder));

    let mut agent = Agent::builder(provider)
        .system_prompt("You are a focused assistant. Reply in one sentence.")
        .context_engine_arc(engine)
        .build();

    let outcome = agent.run("Say hello in any language you like.").await?;
    println!(
        "\n[done: {} turn(s), stop={:?}]",
        outcome.total_turns, outcome.stop_reason
    );
    if let Some(text) = agent.state().last_assistant_text() {
        println!("\nassistant: {text}");
    }

    Ok(())
}
