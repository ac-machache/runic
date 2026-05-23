//! `MemoryLayer` — reads MEMORY.md (general agent memory) from a
//! `StorageBackend` each turn. Wraps in `<memory>` with an explanatory
//! preamble that's overridable via [`MemoryLayer::with_preamble`].

use async_trait::async_trait;
use runic_storage_backend::{StorageBackend, StorageError};
use std::sync::Arc;

use crate::layer::ContextLayer;
use crate::layers::wrap_block;
use crate::TurnContext;

pub const DEFAULT_MEMORY_PREAMBLE: &str =
    "Notes you've gathered from past sessions about this user, project, and environment. Use them as context for the current conversation.";

pub struct MemoryLayer {
    storage: Arc<dyn StorageBackend>,
    key: String,
    preamble: Option<String>,
}

impl MemoryLayer {
    pub fn new(storage: Arc<dyn StorageBackend>, key: impl Into<String>) -> Self {
        Self {
            storage,
            key: key.into(),
            preamble: Some(DEFAULT_MEMORY_PREAMBLE.to_string()),
        }
    }

    pub fn with_preamble(mut self, preamble: impl Into<String>) -> Self {
        let s = preamble.into();
        self.preamble = if s.trim().is_empty() { None } else { Some(s) };
        self
    }
}

#[async_trait]
impl ContextLayer for MemoryLayer {
    fn name(&self) -> &str {
        "memory"
    }

    async fn render(&self, _ctx: &TurnContext<'_>) -> Option<String> {
        match self.storage.read_to_string(&self.key).await {
            Ok(content) => wrap_block("memory", self.preamble.as_deref(), &content),
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
}
