//! `FileLayer` — generic file-backed `ContextLayer` for any custom
//! purpose. Lets users add another "SOUL.md-shaped" layer without
//! copy-pasting boilerplate from the built-in three.
//!
//! Use this for project conventions, coding standards, organization-wide
//! rules — anything that lives in a file and should be wrapped in tags
//! and injected into the system prompt.
//!
//! ```ignore
//! use runic_context_engine::FileLayer;
//!
//! let conventions = FileLayer::new(
//!     storage.clone(),
//!     "PROJECT_CONVENTIONS.md",
//!     "project-conventions",
//! )
//! .with_preamble("Conventions specific to this project. Follow them strictly.");
//! ```

use async_trait::async_trait;
use runic_storage_backend::{StorageBackend, StorageError};
use std::sync::Arc;

use crate::layer::ContextLayer;
use crate::layers::wrap_block;
use crate::TurnContext;

pub struct FileLayer {
    storage: Arc<dyn StorageBackend>,
    key: String,
    /// XML tag used to wrap the content in the assembled prompt.
    tag: String,
    /// Layer name for diagnostics; defaults to `format!("file:{tag}")`.
    name: String,
    preamble: Option<String>,
}

impl FileLayer {
    pub fn new(
        storage: Arc<dyn StorageBackend>,
        key: impl Into<String>,
        tag: impl Into<String>,
    ) -> Self {
        let tag = tag.into();
        let name = format!("file:{tag}");
        Self {
            storage,
            key: key.into(),
            tag,
            name,
            preamble: None,
        }
    }

    /// Set a custom preamble. Empty string disables the preamble entirely.
    pub fn with_preamble(mut self, preamble: impl Into<String>) -> Self {
        let s = preamble.into();
        self.preamble = if s.trim().is_empty() { None } else { Some(s) };
        self
    }

    /// Override the diagnostic name (default: `"file:{tag}"`).
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }
}

#[async_trait]
impl ContextLayer for FileLayer {
    fn name(&self) -> &str {
        &self.name
    }

    async fn render(&self, _ctx: &TurnContext<'_>) -> Option<String> {
        match self.storage.read_to_string(&self.key).await {
            Ok(content) => wrap_block(&self.tag, self.preamble.as_deref(), &content),
            Err(StorageError::NotFound { .. }) => None,
            Err(err) => {
                tracing::warn!(
                    layer = self.name.as_str(),
                    key = self.key.as_str(),
                    error = %err,
                    "FileLayer: failed to read backing file, skipping this turn",
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
            config: crate::empty_config(),
        }
    }

    #[tokio::test]
    async fn custom_file_layer_wraps_in_chosen_tag() {
        let storage = Arc::new(MemoryBackend::new());
        storage
            .write("PROJECT.md", b"use snake_case everywhere")
            .await
            .unwrap();

        let layer = FileLayer::new(
            storage as Arc<dyn StorageBackend>,
            "PROJECT.md",
            "project-conventions",
        );
        let out = layer.render(&ctx()).await.unwrap();

        assert!(out.starts_with("<project-conventions>"));
        assert!(out.ends_with("</project-conventions>"));
        assert!(out.contains("use snake_case everywhere"));
    }

    #[tokio::test]
    async fn preamble_appears_before_content() {
        let storage = Arc::new(MemoryBackend::new());
        storage.write("k", b"body").await.unwrap();

        let layer = FileLayer::new(storage as Arc<dyn StorageBackend>, "k", "thing")
            .with_preamble("Read this carefully.");
        let out = layer.render(&ctx()).await.unwrap();

        assert_eq!(out, "<thing>\nRead this carefully.\n\nbody\n</thing>");
    }

    #[tokio::test]
    async fn no_preamble_by_default() {
        let storage = Arc::new(MemoryBackend::new());
        storage.write("k", b"body").await.unwrap();
        let layer = FileLayer::new(storage as Arc<dyn StorageBackend>, "k", "thing");
        let out = layer.render(&ctx()).await.unwrap();
        assert_eq!(out, "<thing>\nbody\n</thing>");
    }

    #[tokio::test]
    async fn missing_file_renders_as_none() {
        let storage = Arc::new(MemoryBackend::new());
        let layer = FileLayer::new(storage as Arc<dyn StorageBackend>, "absent", "x");
        assert!(layer.render(&ctx()).await.is_none());
    }

    #[tokio::test]
    async fn name_defaults_to_file_tag_pattern() {
        let storage = Arc::new(MemoryBackend::new());
        let layer = FileLayer::new(storage as Arc<dyn StorageBackend>, "k", "conventions");
        assert_eq!(layer.name(), "file:conventions");
    }

    #[tokio::test]
    async fn name_can_be_overridden() {
        let storage = Arc::new(MemoryBackend::new());
        let layer = FileLayer::new(storage as Arc<dyn StorageBackend>, "k", "conventions")
            .with_name("project_conventions");
        assert_eq!(layer.name(), "project_conventions");
    }
}
