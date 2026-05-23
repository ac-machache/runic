//! `PersonaLayer` — reads SOUL.md (the agent's persona / tone) from a
//! `StorageBackend` each turn. Hot-reloadable by definition: edit the file
//! and the very next turn picks it up.
//!
//! Wraps the content in `<persona>` with an explanatory preamble so the
//! model immediately knows what the block IS. The preamble is overridable
//! via [`PersonaLayer::with_preamble`].

use async_trait::async_trait;
use runic_storage_backend::{StorageBackend, StorageError};
use std::sync::Arc;

use crate::layer::ContextLayer;
use crate::layers::wrap_block;
use crate::TurnContext;

/// Default preamble explaining what `SOUL.md` content represents.
pub const DEFAULT_PERSONA_PREAMBLE: &str =
    "Your persona and tone. Embody what's written here in every response.";

pub struct PersonaLayer {
    storage: Arc<dyn StorageBackend>,
    key: String,
    preamble: Option<String>,
}

impl PersonaLayer {
    pub fn new(storage: Arc<dyn StorageBackend>, key: impl Into<String>) -> Self {
        Self {
            storage,
            key: key.into(),
            preamble: Some(DEFAULT_PERSONA_PREAMBLE.to_string()),
        }
    }

    /// Replace the explanatory preamble shown before the file content.
    /// Pass an empty string to suppress the preamble entirely.
    pub fn with_preamble(mut self, preamble: impl Into<String>) -> Self {
        let s = preamble.into();
        self.preamble = if s.trim().is_empty() { None } else { Some(s) };
        self
    }
}

#[async_trait]
impl ContextLayer for PersonaLayer {
    fn name(&self) -> &str {
        "persona"
    }

    async fn render(&self, _ctx: &TurnContext<'_>) -> Option<String> {
        match self.storage.read_to_string(&self.key).await {
            Ok(content) => wrap_block("persona", self.preamble.as_deref(), &content),
            Err(StorageError::NotFound { .. }) => None,
            Err(err) => {
                tracing::warn!(
                    layer = "persona",
                    key = self.key.as_str(),
                    error = %err,
                    "PersonaLayer: failed to read SOUL.md, skipping this turn",
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
        storage.write("SOUL.md", b"warm and playful").await.unwrap();

        let layer = PersonaLayer::new(storage as Arc<dyn StorageBackend>, "SOUL.md");
        let out = layer.render(&ctx()).await.unwrap();

        assert!(out.starts_with("<persona>"));
        assert!(out.ends_with("</persona>"));
        assert!(out.contains(DEFAULT_PERSONA_PREAMBLE));
        assert!(out.contains("warm and playful"));
        let pre_pos = out.find(DEFAULT_PERSONA_PREAMBLE).unwrap();
        let content_pos = out.find("warm and playful").unwrap();
        assert!(pre_pos < content_pos);
    }

    #[tokio::test]
    async fn custom_preamble_overrides_default() {
        let storage = Arc::new(MemoryBackend::new());
        storage.write("SOUL.md", b"hello").await.unwrap();

        let layer = PersonaLayer::new(storage as Arc<dyn StorageBackend>, "SOUL.md")
            .with_preamble("Use a haiku register.");
        let out = layer.render(&ctx()).await.unwrap();

        assert!(out.contains("Use a haiku register."));
        assert!(!out.contains(DEFAULT_PERSONA_PREAMBLE));
    }

    #[tokio::test]
    async fn empty_preamble_disables_it() {
        let storage = Arc::new(MemoryBackend::new());
        storage.write("SOUL.md", b"raw persona content").await.unwrap();

        let layer = PersonaLayer::new(storage as Arc<dyn StorageBackend>, "SOUL.md")
            .with_preamble("");
        let out = layer.render(&ctx()).await.unwrap();

        assert_eq!(out, "<persona>\nraw persona content\n</persona>");
    }

    #[tokio::test]
    async fn missing_file_renders_as_none() {
        let storage = Arc::new(MemoryBackend::new());
        let layer = PersonaLayer::new(storage as Arc<dyn StorageBackend>, "SOUL.md");
        assert!(layer.render(&ctx()).await.is_none());
    }

    #[tokio::test]
    async fn empty_file_renders_as_none() {
        let storage = Arc::new(MemoryBackend::new());
        storage.write("SOUL.md", b"   \n\n  ").await.unwrap();
        let layer = PersonaLayer::new(storage as Arc<dyn StorageBackend>, "SOUL.md");
        assert!(layer.render(&ctx()).await.is_none());
    }

    #[tokio::test]
    async fn hot_reload_reads_fresh_each_call() {
        let storage = Arc::new(MemoryBackend::new());
        storage.write("SOUL.md", b"v1").await.unwrap();

        let layer = PersonaLayer::new(storage.clone() as Arc<dyn StorageBackend>, "SOUL.md");
        let first = layer.render(&ctx()).await.unwrap();
        assert!(first.contains("v1"));

        storage.write("SOUL.md", b"v2").await.unwrap();
        let second = layer.render(&ctx()).await.unwrap();
        assert!(second.contains("v2"));
    }
}
