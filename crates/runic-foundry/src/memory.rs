use std::path::PathBuf;
use std::sync::{Arc, Once};

use async_trait::async_trait;
use runic_agent::Agent;
use runic_filesystem::{FilesystemBackend, LocalFs};
use runic_hook::{HookOutcome, WriteHook};
use runic_memory::{BoundedMemoryStore, MEMORY_REVIEW_GUIDANCE, MemoryTool, ReviewScheduler};
use runic_provider::Provider;
use runic_state::AgentState;
use runic_tool::Tool;
use runic_types::Role;

static LOCK_WARN: Once = Once::new();

pub fn memory(path: impl Into<PathBuf>) -> Memory {
    Memory {
        path: path.into(),
        scoped: false,
        mem_tools: false,
        create: false,
        review: 0,
    }
}

pub struct Memory {
    path: PathBuf,
    scoped: bool,
    mem_tools: bool,
    create: bool,
    review: u32,
}

impl Memory {
    pub fn init(mut self) -> Self {
        self.create = true;
        self
    }
    pub fn scope_per_tenant(mut self) -> Self {
        self.scoped = true;
        self
    }
    pub fn include_mem_tools(mut self) -> Self {
        self.mem_tools = true;
        self
    }
    /// Run a background memory-review curator every `every_n_turns` user turns
    /// (hermes-style reflection). `0` (default) disables it.
    pub fn review(mut self, every_n_turns: u32) -> Self {
        self.review = every_n_turns;
        self
    }

    pub fn store(&self, tenant: &str) -> Arc<BoundedMemoryStore> {
        tracing::info!(
            root = %self.path.display(),
            scoped = self.scoped,
            mem_tools = self.mem_tools,
            init = self.create,
            review = self.review,
            "configuring memory"
        );

        let dir = if self.scoped {
            self.path.join(tenant)
        } else {
            self.path.clone()
        };
        tracing::debug!(tenant, dir = %dir.display(), "resolved memory path");

        if self.scoped && tenant.is_empty() {
            tracing::warn!(
                "scope_per_tenant set but tenant is empty — memory is shared, not isolated"
            );
        }

        LOCK_WARN.call_once(|| {
            tracing::warn!(
                "memory has no cross-process lock (with_lock_dir unset) — concurrent processes may clobber writes"
            );
        });

        if self.create
            && let Err(e) = std::fs::create_dir_all(&dir)
        {
            tracing::error!(dir = %dir.display(), error = %e, "failed to create memory dir");
        }

        let fs: Arc<dyn FilesystemBackend> = Arc::new(LocalFs::new(dir));
        Arc::new(BoundedMemoryStore::new(fs))
    }

    pub fn tools(&self, store: Arc<BoundedMemoryStore>) -> Option<Arc<dyn Tool>> {
        if self.mem_tools {
            tracing::debug!("memory tool enabled");
            Some(Arc::new(MemoryTool::new(store)) as Arc<dyn Tool>)
        } else {
            None
        }
    }

    /// The background-review hook, when `.review(n)` is set. Spawns a curator
    /// sharing `store` every n turns; needs the provider/model to run it.
    pub fn review_hook(
        &self,
        provider: Arc<dyn Provider>,
        model: impl Into<String>,
        store: Arc<BoundedMemoryStore>,
    ) -> Option<Arc<dyn WriteHook>> {
        if self.review == 0 {
            return None;
        }
        tracing::debug!(interval = self.review, "memory review enabled");
        Some(Arc::new(MemoryReviewHook {
            scheduler: ReviewScheduler::new(self.review),
            provider,
            model: model.into(),
            store,
        }))
    }
}

struct MemoryReviewHook {
    scheduler: ReviewScheduler,
    provider: Arc<dyn Provider>,
    model: String,
    store: Arc<BoundedMemoryStore>,
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
                .max_turns(3)
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
