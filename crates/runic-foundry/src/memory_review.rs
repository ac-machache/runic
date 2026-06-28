use std::sync::Arc;

use async_trait::async_trait;
use runic_agent::Agent;
use runic_hook::{HookOutcome, WriteHook};
use runic_memory::{BoundedMemoryStore, MEMORY_REVIEW_GUIDANCE, MemoryTool, ReviewScheduler};
use runic_provider::Provider;
use runic_state::AgentState;
use runic_types::Role;

/// Every N turns, spawn an off-loop curator that reviews the transcript and
/// curates the shared memory store. (Lives here, not in `runic-memory`, because
/// it spawns an `Agent` — `runic-memory` must not depend on `runic-agent`.)
pub struct MemoryReviewHook {
    scheduler: ReviewScheduler,
    provider: Arc<dyn Provider>,
    model: String,
    store: Arc<BoundedMemoryStore>,
}

impl MemoryReviewHook {
    pub fn new(
        interval: u32,
        provider: Arc<dyn Provider>,
        model: impl Into<String>,
        store: Arc<BoundedMemoryStore>,
    ) -> Self {
        Self {
            scheduler: ReviewScheduler::new(interval),
            provider,
            model: model.into(),
            store,
        }
    }
}

#[async_trait]
impl WriteHook for MemoryReviewHook {
    fn name(&self) -> &str {
        "memory_review"
    }

    async fn after_agent(&self, state: &mut AgentState) -> HookOutcome {
        if !self.scheduler.record_turn() {
            return HookOutcome::Continue;
        }
        let transcript = render_transcript(state);
        tracing::info!("memory review due — spawning background curator");

        let provider = self.provider.clone();
        let model = self.model.clone();
        let store = self.store.clone();
        tokio::spawn(async move {
            let mut curator = Agent::builder(provider, "memory-review", "review")
                .model(model)
                .system_prompt(MEMORY_REVIEW_GUIDANCE)
                .tool(Arc::new(MemoryTool::new(store)))
                .max_turns(8)
                .graceful_max_turns(true)
                .build();
            if let Err(e) = curator.run(transcript).await {
                tracing::warn!(error = %e, "memory review curator failed");
            }
        });
        HookOutcome::Continue
    }
}

fn render_transcript(state: &AgentState) -> String {
    let mut out = String::new();
    for msg in state.messages_for_provider() {
        let role = match msg.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::System => continue,
        };
        let text = msg.content.text_content();
        if !text.trim().is_empty() {
            out.push_str(role);
            out.push_str(": ");
            out.push_str(&text);
            out.push('\n');
        }
    }
    out
}
