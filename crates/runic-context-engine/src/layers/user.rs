//! `UserFactsLayer` — reads USER.md (facts/preferences about the user)
//! from a `StorageBackend` each turn. Wraps in `<user-facts>` with an
//! explanatory preamble that's overridable via
//! [`UserFactsLayer::with_preamble`].

use async_trait::async_trait;
use runic_storage_backend::{StorageBackend, StorageError};
use std::sync::Arc;

use crate::layer::ContextLayer;
use crate::layers::wrap_block;
use crate::TurnContext;

pub const DEFAULT_USER_FACTS_PREAMBLE: &str =
    "Facts about the user you're talking to. Tailor your responses to their preferences, role, and communication style.";

pub struct UserFactsLayer {
    storage: Arc<dyn StorageBackend>,
    key: String,
    preamble: Option<String>,
}

impl UserFactsLayer {
    pub fn new(storage: Arc<dyn StorageBackend>, key: impl Into<String>) -> Self {
        Self {
            storage,
            key: key.into(),
            preamble: Some(DEFAULT_USER_FACTS_PREAMBLE.to_string()),
        }
    }

    pub fn with_preamble(mut self, preamble: impl Into<String>) -> Self {
        let s = preamble.into();
        self.preamble = if s.trim().is_empty() { None } else { Some(s) };
        self
    }
}

#[async_trait]
impl ContextLayer for UserFactsLayer {
    fn name(&self) -> &str {
        "user_facts"
    }

    async fn render(&self, _ctx: &TurnContext<'_>) -> Option<String> {
        match self.storage.read_to_string(&self.key).await {
            Ok(content) => wrap_block("user-facts", self.preamble.as_deref(), &content),
            Err(StorageError::NotFound { .. }) => None,
            Err(err) => {
                tracing::warn!(
                    layer = "user_facts",
                    key = self.key.as_str(),
                    error = %err,
                    "UserFactsLayer: failed to read USER.md, skipping this turn",
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
        storage.write("USER.md", b"- Codes in Rust").await.unwrap();

        let layer = UserFactsLayer::new(storage as Arc<dyn StorageBackend>, "USER.md");
        let out = layer.render(&ctx()).await.unwrap();

        assert!(out.contains(DEFAULT_USER_FACTS_PREAMBLE));
        assert!(out.contains("Codes in Rust"));
    }

    #[tokio::test]
    async fn custom_preamble_overrides_default() {
        let storage = Arc::new(MemoryBackend::new());
        storage.write("USER.md", b"x").await.unwrap();

        let layer = UserFactsLayer::new(storage as Arc<dyn StorageBackend>, "USER.md")
            .with_preamble("Personal facts only.");
        let out = layer.render(&ctx()).await.unwrap();

        assert!(out.contains("Personal facts only."));
        assert!(!out.contains(DEFAULT_USER_FACTS_PREAMBLE));
    }

    #[tokio::test]
    async fn missing_file_renders_as_none() {
        let storage = Arc::new(MemoryBackend::new());
        let layer = UserFactsLayer::new(storage as Arc<dyn StorageBackend>, "USER.md");
        assert!(layer.render(&ctx()).await.is_none());
    }
}
