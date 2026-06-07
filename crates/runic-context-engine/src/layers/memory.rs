//! `MemoryLayer` — reads MEMORY.md (general agent memory) from a
//! `StorageBackend` each turn. Wraps in `<memory>` with an explanatory
//! preamble that's overridable via [`MemoryLayer::with_preamble`].
//!
//! Default mode is **hot-reload** — re-reads the file every turn so
//! manual edits land immediately. Call [`MemoryLayer::frozen`] to snap
//! the content on first render and serve the same value for the rest
//! of the session — this is the prefix-cache-friendly mode you want in
//! production when paired with a write-capable memory tool. Mid-session
//! writes still land on disk; the next session start picks them up.

use async_trait::async_trait;
use runic_storage_backend::{StorageBackend, StorageError};
use std::sync::Arc;
use tokio::sync::OnceCell;

use crate::layer::ContextLayer;
use crate::layers::wrap_block;
use crate::TurnContext;

pub const DEFAULT_MEMORY_PREAMBLE: &str =
    "Notes you've gathered from past sessions about this user, project, and environment. Use them as context for the current conversation.";

pub struct MemoryLayer {
    storage: Arc<dyn StorageBackend>,
    key: String,
    preamble: Option<String>,
    /// When `Some`, the layer is in frozen mode: it reads disk once on
    /// first render, caches the result, and serves that snapshot every
    /// subsequent turn. `None` = hot-reload (read each turn).
    snapshot: Option<OnceCell<Option<String>>>,
}

impl MemoryLayer {
    pub fn new(storage: Arc<dyn StorageBackend>, key: impl Into<String>) -> Self {
        Self {
            storage,
            key: key.into(),
            preamble: Some(DEFAULT_MEMORY_PREAMBLE.to_string()),
            snapshot: None,
        }
    }

    pub fn with_preamble(mut self, preamble: impl Into<String>) -> Self {
        let s = preamble.into();
        self.preamble = if s.trim().is_empty() { None } else { Some(s) };
        self
    }

    /// Switch this layer into snapshot-on-first-render mode. After the
    /// first call to `render`, the content is fixed for the lifetime of
    /// this layer instance — so the system prompt stays stable for the
    /// whole session and the LLM provider can reuse its prefix cache.
    /// Pair with a write tool (`runic-memory::MemoryTool`) that updates
    /// the file on disk for the next session to pick up.
    pub fn frozen(mut self) -> Self {
        self.snapshot = Some(OnceCell::new());
        self
    }

    async fn read_once(&self) -> Option<String> {
        match self.storage.read_to_string(&self.key).await {
            Ok(content) => Some(content),
            Err(StorageError::NotFound { .. }) => None,
            Err(err) => {
                tracing::warn!(
                    layer = "memory",
                    key = self.key.as_str(),
                    error = %err,
                    "MemoryLayer: failed to read MEMORY.md, skipping this turn",
                );
                None
            }
        }
    }
}

#[async_trait]
impl ContextLayer for MemoryLayer {
    fn name(&self) -> &str {
        "memory"
    }

    async fn render(&self, _ctx: &TurnContext<'_>) -> Option<String> {
        let content = match &self.snapshot {
            Some(cell) => cell.get_or_init(|| self.read_once()).await.clone(),
            None => self.read_once().await,
        }?;
        wrap_block("memory", self.preamble.as_deref(), &content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runic_message_types::Message;
    use runic_storage_backend::MemoryBackend;

    fn ctx<'a>() -> TurnContext<'a> {
        TurnContext {
            base_system_prompt: "",
            messages: &[] as &[Message],
            run_id: "r1",
            turn: 0,
        }
    }

    #[tokio::test]
    async fn default_preamble_appears_in_output() {
        let storage = Arc::new(MemoryBackend::new());
        storage
            .write("MEMORY.md", b"User is building runic.")
            .await
            .unwrap();

        let layer = MemoryLayer::new(storage as Arc<dyn StorageBackend>, "MEMORY.md");
        let out = layer.render(&ctx()).await.unwrap();

        assert!(out.contains(DEFAULT_MEMORY_PREAMBLE));
        assert!(out.contains("building runic"));
    }

    #[tokio::test]
    async fn custom_preamble_overrides_default() {
        let storage = Arc::new(MemoryBackend::new());
        storage.write("MEMORY.md", b"x").await.unwrap();

        let layer = MemoryLayer::new(storage as Arc<dyn StorageBackend>, "MEMORY.md")
            .with_preamble("Past learnings.");
        let out = layer.render(&ctx()).await.unwrap();

        assert!(out.contains("Past learnings."));
        assert!(!out.contains(DEFAULT_MEMORY_PREAMBLE));
    }

    #[tokio::test]
    async fn missing_file_renders_as_none() {
        let storage = Arc::new(MemoryBackend::new());
        let layer = MemoryLayer::new(storage as Arc<dyn StorageBackend>, "MEMORY.md");
        assert!(layer.render(&ctx()).await.is_none());
    }

    #[tokio::test]
    async fn hot_reload_picks_up_file_changes() {
        let storage = Arc::new(MemoryBackend::new());
        storage.write("MEMORY.md", b"v1").await.unwrap();
        let layer = MemoryLayer::new(storage.clone() as Arc<dyn StorageBackend>, "MEMORY.md");

        let first = layer.render(&ctx()).await.unwrap();
        assert!(first.contains("v1"));

        storage.write("MEMORY.md", b"v2").await.unwrap();
        let second = layer.render(&ctx()).await.unwrap();
        assert!(second.contains("v2"));
        assert!(!second.contains("v1"));
    }

    #[tokio::test]
    async fn frozen_locks_content_after_first_render() {
        let storage = Arc::new(MemoryBackend::new());
        storage.write("MEMORY.md", b"frozen-at-v1").await.unwrap();
        let layer =
            MemoryLayer::new(storage.clone() as Arc<dyn StorageBackend>, "MEMORY.md").frozen();

        let first = layer.render(&ctx()).await.unwrap();
        assert!(first.contains("frozen-at-v1"));

        // Mutate the file — frozen layer must keep returning the snapshot.
        storage.write("MEMORY.md", b"changed-to-v2").await.unwrap();
        let second = layer.render(&ctx()).await.unwrap();
        assert!(second.contains("frozen-at-v1"));
        assert!(!second.contains("changed-to-v2"));
    }

    #[tokio::test]
    async fn frozen_with_missing_file_at_first_render_stays_none() {
        let storage = Arc::new(MemoryBackend::new());
        let layer =
            MemoryLayer::new(storage.clone() as Arc<dyn StorageBackend>, "MEMORY.md").frozen();

        assert!(layer.render(&ctx()).await.is_none());

        // File created AFTER snapshot — frozen layer ignores it.
        storage.write("MEMORY.md", b"appeared later").await.unwrap();
        assert!(layer.render(&ctx()).await.is_none());
    }
}
