//! `UserFactsLayer` — reads USER.md (facts/preferences about the user)
//! from a `StorageBackend`. Wraps in `<user-facts>` with an explanatory
//! preamble that's overridable via [`UserFactsLayer::with_preamble`].
//!
//! Default mode is **hot-reload** — re-reads the file every turn. Call
//! [`UserFactsLayer::frozen`] to snap the content on first render and
//! serve the same value for the rest of the session — pairs with a
//! write-capable memory tool to keep the system prompt prefix-cache
//! stable while still letting the agent persist new facts for the next
//! session.

use async_trait::async_trait;
use runic_storage_backend::{StorageBackend, StorageError};
use std::sync::Arc;
use tokio::sync::OnceCell;

use crate::layer::ContextLayer;
use crate::layers::wrap_block;
use crate::TurnContext;

pub const DEFAULT_USER_FACTS_PREAMBLE: &str =
    "Facts about the user you're talking to. Tailor your responses to their preferences, role, and communication style.";

pub struct UserFactsLayer {
    storage: Arc<dyn StorageBackend>,
    key: String,
    preamble: Option<String>,
    snapshot: Option<OnceCell<Option<String>>>,
}

impl UserFactsLayer {
    pub fn new(storage: Arc<dyn StorageBackend>, key: impl Into<String>) -> Self {
        Self {
            storage,
            key: key.into(),
            preamble: Some(DEFAULT_USER_FACTS_PREAMBLE.to_string()),
            snapshot: None,
        }
    }

    pub fn with_preamble(mut self, preamble: impl Into<String>) -> Self {
        let s = preamble.into();
        self.preamble = if s.trim().is_empty() { None } else { Some(s) };
        self
    }

    /// Switch to snapshot-on-first-render mode. After the first call to
    /// `render`, the content is fixed for the lifetime of this instance.
    /// Use this in production so the system prompt stays prefix-cache
    /// stable; pair with `runic-memory::MemoryTool` for persistent
    /// updates that the next session will pick up.
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

#[async_trait]
impl ContextLayer for UserFactsLayer {
    fn name(&self) -> &str {
        "user_facts"
    }

    async fn render(&self, _ctx: &TurnContext<'_>) -> Option<String> {
        let content = match &self.snapshot {
            Some(cell) => cell.get_or_init(|| self.read_once()).await.clone(),
            None => self.read_once().await,
        }?;
        wrap_block("user-facts", self.preamble.as_deref(), &content)
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

    #[tokio::test]
    async fn hot_reload_picks_up_file_changes() {
        let storage = Arc::new(MemoryBackend::new());
        storage.write("USER.md", b"v1").await.unwrap();
        let layer = UserFactsLayer::new(storage.clone() as Arc<dyn StorageBackend>, "USER.md");

        let first = layer.render(&ctx()).await.unwrap();
        assert!(first.contains("v1"));

        storage.write("USER.md", b"v2").await.unwrap();
        let second = layer.render(&ctx()).await.unwrap();
        assert!(second.contains("v2"));
    }

    #[tokio::test]
    async fn frozen_locks_content_after_first_render() {
        let storage = Arc::new(MemoryBackend::new());
        storage.write("USER.md", b"snapshot v1").await.unwrap();
        let layer =
            UserFactsLayer::new(storage.clone() as Arc<dyn StorageBackend>, "USER.md").frozen();

        let first = layer.render(&ctx()).await.unwrap();
        assert!(first.contains("snapshot v1"));

        storage.write("USER.md", b"changed v2").await.unwrap();
        let second = layer.render(&ctx()).await.unwrap();
        assert!(second.contains("snapshot v1"));
        assert!(!second.contains("changed v2"));
    }
}
