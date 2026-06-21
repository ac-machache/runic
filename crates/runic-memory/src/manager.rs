//! `MemoryManager` — composes one or more [`MemoryProvider`]s and fans the
//! lifecycle out across them, ported from hermes's `MemoryManager`.
//!
//! The agent talks only to the manager:
//! - **build the prompt**: [`system_prompt`] (frozen tier) + [`prefetch_all`]
//!   (volatile tier, wrapped in a `<memory-context>` fence so it rides in the
//!   user message and never busts the prefix cache);
//! - **after a turn**: [`sync_all`] persists the exchange, [`queue_prefetch_all`]
//!   warms the next turn;
//! - **on a built-in write**: [`on_memory_write`] mirrors it into externals;
//! - **tooling**: [`tools`] gathers every provider's tools for the registry.
//!
//! [`system_prompt`]: MemoryManager::system_prompt
//! [`prefetch_all`]: MemoryManager::prefetch_all
//! [`sync_all`]: MemoryManager::sync_all
//! [`queue_prefetch_all`]: MemoryManager::queue_prefetch_all
//! [`on_memory_write`]: MemoryManager::on_memory_write
//! [`tools`]: MemoryManager::tools

use std::sync::Arc;

use futures::future::join_all;
use runic_tool::Tool;

use crate::provider::{MemoryProvider, MemoryScope, MemoryWriteMeta};

/// Fence the volatile per-turn recall is wrapped in, so the model can tell
/// retrieved memory from the user's own words (hermes `<memory-context>`).
const MEMORY_CONTEXT_OPEN: &str = "<memory-context>";
const MEMORY_CONTEXT_CLOSE: &str = "</memory-context>";

/// Composes providers and drives their shared lifecycle.
#[derive(Default)]
pub struct MemoryManager {
    providers: Vec<Arc<dyn MemoryProvider>>,
}

impl MemoryManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a provider. Order is preserved; the built-in is conventionally
    /// added first so its system-prompt block leads.
    pub fn add_provider(&mut self, provider: Arc<dyn MemoryProvider>) -> &mut Self {
        self.providers.push(provider);
        self
    }

    /// Whether any provider is registered.
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    /// Initialize every provider for this run's scope.
    pub async fn initialize(&self, scope: &MemoryScope) {
        join_all(self.providers.iter().map(|p| p.initialize(scope))).await;
    }

    /// Assemble the **system-prompt** section: each available provider's block,
    /// in registration order, separated by blank lines. Empty if none.
    pub async fn system_prompt(&self) -> String {
        let blocks = join_all(self.providers.iter().map(|p| async move {
            if p.is_available().await {
                p.system_prompt_block().await
            } else {
                None
            }
        }))
        .await;
        blocks
            .into_iter()
            .flatten()
            .filter(|b| !b.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// Assemble the per-turn recall to splice into the **user message**. Each
    /// provider's `prefetch` is collected and the lot is wrapped in one
    /// `<memory-context>` fence. Empty string if nothing was recalled.
    pub async fn prefetch_all(&self, query: &str) -> String {
        let hits = join_all(self.providers.iter().map(|p| async move {
            if p.is_available().await {
                p.prefetch(query).await
            } else {
                None
            }
        }))
        .await;
        let body = hits
            .into_iter()
            .flatten()
            .filter(|h| !h.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        if body.is_empty() {
            String::new()
        } else {
            format!("{MEMORY_CONTEXT_OPEN}\n{body}\n{MEMORY_CONTEXT_CLOSE}")
        }
    }

    /// Warm next turn's prefetch across providers (non-blocking semantics).
    pub async fn queue_prefetch_all(&self, query: &str) {
        join_all(self.providers.iter().map(|p| p.queue_prefetch(query))).await;
    }

    /// Persist a completed turn across providers.
    pub async fn sync_all(&self, user: &str, assistant: &str) {
        join_all(self.providers.iter().map(|p| p.sync_turn(user, assistant))).await;
    }

    /// Mirror a built-in `memory` write into every provider (the built-in
    /// itself ignores its own mirror — it already persisted on the tool call).
    pub async fn on_memory_write(
        &self,
        action: &str,
        target: &str,
        content: &str,
        meta: &MemoryWriteMeta,
    ) {
        join_all(
            self.providers
                .iter()
                .map(|p| p.on_memory_write(action, target, content, meta)),
        )
        .await;
    }

    /// Every provider's tools, flattened for the agent's registry.
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.providers.iter().flat_map(|p| p.tools()).collect()
    }

    /// Tear down every provider at session end.
    pub async fn shutdown_all(&self) {
        join_all(self.providers.iter().map(|p| p.shutdown())).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::provider::BuiltinProvider;
    use crate::store::{BoundedMemoryStore, Target};
    use runic_filesystem::{FilesystemBackend, MemoryFs};

    /// A fake external provider that records the lifecycle fan-out and supplies
    /// a per-turn prefetch.
    #[derive(Default)]
    struct FakeExternal {
        synced: AtomicUsize,
        mirrored: AtomicUsize,
        queued: AtomicUsize,
    }

    #[async_trait]
    impl MemoryProvider for FakeExternal {
        fn name(&self) -> &str {
            "fake"
        }
        async fn prefetch(&self, query: &str) -> Option<String> {
            Some(format!("recalled: {query}"))
        }
        async fn queue_prefetch(&self, _query: &str) {
            self.queued.fetch_add(1, Ordering::SeqCst);
        }
        async fn sync_turn(&self, _user: &str, _assistant: &str) {
            self.synced.fetch_add(1, Ordering::SeqCst);
        }
        async fn on_memory_write(&self, _a: &str, _t: &str, _c: &str, _m: &MemoryWriteMeta) {
            self.mirrored.fetch_add(1, Ordering::SeqCst);
        }
    }

    async fn builtin_with(entry: &str) -> Arc<BuiltinProvider> {
        let backend: Arc<dyn FilesystemBackend> = Arc::new(MemoryFs::new());
        let store = Arc::new(BoundedMemoryStore::new(backend));
        store.add(Target::Memory, entry).await.unwrap();
        Arc::new(BuiltinProvider::new(store))
    }

    #[tokio::test]
    async fn system_prompt_collects_builtin_block_only() {
        let mut m = MemoryManager::new();
        m.add_provider(builtin_with("uses nix").await);
        m.add_provider(Arc::new(FakeExternal::default())); // no system block
        let sp = m.system_prompt().await;
        assert!(sp.contains("MEMORY (your personal notes)"));
        assert!(sp.contains("uses nix"));
    }

    #[tokio::test]
    async fn prefetch_wraps_external_recall_in_fence() {
        let mut m = MemoryManager::new();
        m.add_provider(Arc::new(FakeExternal::default()));
        let pf = m.prefetch_all("what shell").await;
        assert!(pf.starts_with("<memory-context>"));
        assert!(pf.contains("recalled: what shell"));
        assert!(pf.ends_with("</memory-context>"));
    }

    #[tokio::test]
    async fn prefetch_is_empty_when_nothing_recalled() {
        let backend: Arc<dyn FilesystemBackend> = Arc::new(MemoryFs::new());
        let builtin = Arc::new(BuiltinProvider::new(Arc::new(BoundedMemoryStore::new(
            backend,
        ))));
        let mut m = MemoryManager::new();
        m.add_provider(builtin); // built-in has no prefetch
        assert!(m.prefetch_all("x").await.is_empty());
    }

    #[tokio::test]
    async fn lifecycle_fans_out_to_all_providers() {
        let fake = Arc::new(FakeExternal::default());
        let mut m = MemoryManager::new();
        m.add_provider(fake.clone());

        m.sync_all("u", "a").await;
        m.queue_prefetch_all("q").await;
        m.on_memory_write("add", "memory", "c", &MemoryWriteMeta::default())
            .await;

        assert_eq!(fake.synced.load(Ordering::SeqCst), 1);
        assert_eq!(fake.queued.load(Ordering::SeqCst), 1);
        assert_eq!(fake.mirrored.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn tools_are_gathered_across_providers() {
        let backend: Arc<dyn FilesystemBackend> = Arc::new(MemoryFs::new());
        let builtin = Arc::new(BuiltinProvider::new(Arc::new(BoundedMemoryStore::new(
            backend,
        ))));
        let mut m = MemoryManager::new();
        m.add_provider(builtin);
        m.add_provider(Arc::new(FakeExternal::default()));
        let tools = m.tools();
        assert_eq!(tools.len(), 1); // only built-in contributes a tool
        assert_eq!(tools[0].name(), "memory");
    }
}
