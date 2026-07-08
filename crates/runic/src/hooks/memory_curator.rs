use std::sync::Arc;

use async_trait::async_trait;
use runic_hook::{HookLifecycle, HookOutcome, WriteHook};
use runic_memory::{MEMORY_REVIEW_GUIDANCE, MemoryStore, MemoryTool};
use runic_provider::Provider;
use runic_state::AgentState;
use runic_types::Role;

use crate::hooks::HookAgent;

const LAST_REVIEW_KEY: &str = "memory-curator/last-review-run";

pub struct MemoryCurator {
    interval: u32,
    provider: Arc<dyn Provider>,
    model: String,
    store: Arc<MemoryStore>,
    guidance: String,
}

impl MemoryCurator {
    pub fn new(
        interval: u32,
        provider: Arc<dyn Provider>,
        model: impl Into<String>,
        store: Arc<MemoryStore>,
    ) -> Self {
        Self {
            interval,
            provider,
            model: model.into(),
            store,
            guidance: MEMORY_REVIEW_GUIDANCE.to_string(),
        }
    }

    pub fn with_guidance(mut self, guidance: impl Into<String>) -> Self {
        self.guidance = guidance.into();
        self
    }
}

#[async_trait]
impl WriteHook for MemoryCurator {
    fn name(&self) -> &str {
        "memory-curator"
    }

    fn points(&self) -> &'static [HookLifecycle] {
        &[HookLifecycle::AfterAgent]
    }

    async fn after_agent(&self, state: &mut AgentState) -> HookOutcome {
        if self.interval == 0 {
            return HookOutcome::Noop;
        }
        let completed_runs = state.stats().runs + 1;
        let last_review = state
            .get(LAST_REVIEW_KEY)
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if completed_runs.saturating_sub(last_review) < u64::from(self.interval) {
            return HookOutcome::Noop;
        }
        let _ = state.update(LAST_REVIEW_KEY, serde_json::json!(completed_runs));
        let transcript = render_transcript(state);
        tracing::info!("memory review due — spawning background curator");

        let provider = self.provider.clone();
        let model = self.model.clone();
        let store = self.store.clone();
        let guidance = self.guidance.clone();
        tokio::spawn(async move {
            let result = HookAgent::new(provider, model)
                .prompt(guidance)
                .tool(Arc::new(MemoryTool::new(store)))
                .run(transcript)
                .await;
            if let Err(e) = result {
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
