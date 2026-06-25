//! `MemoryProvider` — the provider seam, ported from hermes's `MemoryProvider`
//! ABC. A provider contributes memory to the agent through up to four channels:
//!
//! 1. a **system-prompt block** (frozen tier — the built-in store's MEMORY/USER
//!    blocks live here);
//! 2. a per-turn **prefetch** string injected into the *user* message (volatile
//!    tier, so it never busts the prompt-prefix cache — external/RAG providers
//!    use this);
//! 3. a post-turn **sync** to persist the completed exchange;
//! 4. its own **tools** (an external provider may add `memory_search` etc.).
//!
//! All but `name` have defaults, so a provider implements only what it offers.
//! The built-in file store ([`BuiltinProvider`]) uses channels 1 and 4.

use std::sync::Arc;

use async_trait::async_trait;
use runic_tool::Tool;

use crate::store::{BoundedMemoryStore, Target};
use crate::tool::MemoryTool;

/// Per-run identity used to scope a provider's storage to a user / session.
#[derive(Debug, Clone, Default)]
pub struct MemoryScope {
    pub user_id: String,
    pub session_id: String,
    /// Channel/thread id, if the surface has one (hermes `chat_id`).
    pub chat_id: Option<String>,
}

/// Provenance attached to a memory write, so an external mirror can record
/// *who/where* wrote it (hermes `build_memory_write_metadata`).
#[derive(Debug, Clone, Default)]
pub struct MemoryWriteMeta {
    /// "assistant_tool" | "background_review" | … — where the write originated.
    pub write_origin: String,
    /// "foreground" | "background_review".
    pub execution_context: String,
    pub session_id: String,
}

/// One source of agent memory. See the module docs for the four channels.
#[async_trait]
pub trait MemoryProvider: Send + Sync {
    /// Stable provider name ("builtin", "honcho", "mem0", …).
    fn name(&self) -> &str;

    /// Whether the provider is usable this run (creds present, reachable…).
    async fn is_available(&self) -> bool {
        true
    }

    /// One-time per-session setup (open a connection, resolve the scope…).
    async fn initialize(&self, _scope: &MemoryScope) {}

    /// Block for the system prompt's volatile tier. Built-in returns the
    /// MEMORY/USER snapshot; external providers usually return `None` and use
    /// [`prefetch`](MemoryProvider::prefetch) instead.
    async fn system_prompt_block(&self) -> Option<String> {
        None
    }

    /// Recall relevant memory for this turn's `query`, to be injected into the
    /// *user* message (cache-safe). `None` = nothing to add.
    async fn prefetch(&self, _query: &str) -> Option<String> {
        None
    }

    /// Warm next turn's prefetch without blocking the loop. Default: noop.
    async fn queue_prefetch(&self, _query: &str) {}

    /// Persist a completed turn (external providers store the exchange).
    async fn sync_turn(&self, _user: &str, _assistant: &str) {}

    /// Mirror a built-in `memory` tool write into this provider. Default: noop.
    async fn on_memory_write(
        &self,
        _action: &str,
        _target: &str,
        _content: &str,
        _meta: &MemoryWriteMeta,
    ) {
    }

    /// Tools this provider contributes to the agent's registry.
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        Vec::new()
    }

    /// Release any resources at session end. Default: noop.
    async fn shutdown(&self) {}
}

/// The built-in provider: the bounded file store behind the `memory` tool, with
/// its MEMORY/USER blocks injected into the system prompt.
pub struct BuiltinProvider {
    store: Arc<BoundedMemoryStore>,
    memory_enabled: bool,
    user_enabled: bool,
}

impl BuiltinProvider {
    pub fn new(store: Arc<BoundedMemoryStore>) -> Self {
        Self {
            store,
            memory_enabled: true,
            user_enabled: true,
        }
    }

    /// Gate the MEMORY.md block (default on).
    pub fn with_memory_enabled(mut self, on: bool) -> Self {
        self.memory_enabled = on;
        self
    }

    /// Gate the USER.md block (default on).
    pub fn with_user_enabled(mut self, on: bool) -> Self {
        self.user_enabled = on;
        self
    }

    /// The underlying store (the background-review curator shares this).
    pub fn store(&self) -> Arc<BoundedMemoryStore> {
        self.store.clone()
    }
}

#[async_trait]
impl MemoryProvider for BuiltinProvider {
    fn name(&self) -> &str {
        "builtin"
    }

    async fn system_prompt_block(&self) -> Option<String> {
        let snap = self.store.snapshot().await.ok()?;
        let section = snap.section(self.memory_enabled, self.user_enabled);
        (!section.is_empty()).then_some(section)
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(MemoryTool::new(self.store.clone()))]
    }
}

impl BuiltinProvider {
    /// Convenience: which targets this provider exposes, for callers that want
    /// to drive the store directly (e.g. the review curator).
    pub fn enabled_targets(&self) -> Vec<Target> {
        let mut t = Vec::new();
        if self.memory_enabled {
            t.push(Target::Memory);
        }
        if self.user_enabled {
            t.push(Target::User);
        }
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::MemStorage;

    fn builtin() -> BuiltinProvider {
        let backend: Arc<MemStorage> = Arc::new(MemStorage::new());
        BuiltinProvider::new(Arc::new(BoundedMemoryStore::new(backend)))
    }

    #[tokio::test]
    async fn builtin_block_is_none_when_empty() {
        let p = builtin();
        assert!(p.system_prompt_block().await.is_none());
    }

    #[tokio::test]
    async fn builtin_block_renders_after_a_write() {
        let p = builtin();
        p.store()
            .add(Target::User, "user prefers terse answers")
            .await
            .unwrap();
        let block = p.system_prompt_block().await.unwrap();
        assert!(block.contains("USER PROFILE"));
        assert!(block.contains("user prefers terse answers"));
    }

    #[tokio::test]
    async fn builtin_block_respects_target_gates() {
        let backend: Arc<MemStorage> = Arc::new(MemStorage::new());
        let store = Arc::new(BoundedMemoryStore::new(backend));
        store.add(Target::Memory, "uses zsh").await.unwrap();
        store.add(Target::User, "lives in Paris").await.unwrap();
        let p = BuiltinProvider::new(store).with_user_enabled(false);
        let block = p.system_prompt_block().await.unwrap();
        assert!(block.contains("uses zsh"));
        assert!(!block.contains("Paris")); // user block gated off
    }

    #[tokio::test]
    async fn builtin_exposes_the_memory_tool() {
        let p = builtin();
        let tools = p.tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "memory");
    }
}
