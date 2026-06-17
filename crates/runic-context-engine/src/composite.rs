//! `CompositeEngine` — the flagship `ContextEngine` impl. Composed of
//! `ContextLayer`s; assembles the system prompt by concatenating each
//! layer's rendered output **in the order the layers were registered**.

use async_trait::async_trait;
use std::sync::Arc;

use crate::layer::ContextLayer;
use crate::{ContextEngine, TurnContext};

pub struct CompositeEngine {
    layers: Vec<Arc<dyn ContextLayer>>,
}

impl CompositeEngine {
    pub fn new() -> Self {
        Self { layers: Vec::new() }
    }

    /// Add a layer. The order of `.with_layer(...)` calls is the order
    /// layers appear in the assembled system prompt.
    pub fn with_layer<L>(mut self, layer: L) -> Self
    where
        L: ContextLayer + 'static,
    {
        self.layers.push(Arc::new(layer));
        self
    }

    /// Layers in registration order. Caller can use this for diagnostics.
    pub fn layers(&self) -> &[Arc<dyn ContextLayer>] {
        &self.layers
    }
}

impl Default for CompositeEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for CompositeEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.layers.iter().map(|l| l.name()).collect();
        f.debug_struct("CompositeEngine")
            .field("layers", &names)
            .finish()
    }
}

#[async_trait]
impl ContextEngine for CompositeEngine {
    async fn assemble_system_prompt(&self, ctx: &TurnContext<'_>) -> String {
        // Render each layer in registration order, drop the ones returning
        // None, join the rest with a blank line between them.
        let mut rendered: Vec<String> = Vec::with_capacity(self.layers.len());
        for layer in &self.layers {
            if let Some(content) = layer.render(ctx).await {
                rendered.push(content);
            }
        }
        rendered.join("\n\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layers::{BasePromptLayer, MemoryLayer, PersonaLayer, UserFactsLayer};
    use runic_message_types::Message;
    use runic_storage_backend::{MemoryBackend, StorageBackend};

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
    async fn empty_engine_returns_empty_string() {
        let engine = CompositeEngine::new();
        assert_eq!(engine.assemble_system_prompt(&ctx()).await, "");
    }

    #[tokio::test]
    async fn single_layer_renders_its_content() {
        let engine = CompositeEngine::new().with_layer(BasePromptLayer::new("hello"));
        assert_eq!(engine.assemble_system_prompt(&ctx()).await, "hello");
    }

    #[tokio::test]
    async fn layers_appear_in_registration_order() {
        let engine = CompositeEngine::new()
            .with_layer(BasePromptLayer::new("first"))
            .with_layer(BasePromptLayer::new("second"))
            .with_layer(BasePromptLayer::new("third"));

        let out = engine.assemble_system_prompt(&ctx()).await;
        assert_eq!(out, "first\n\nsecond\n\nthird");
    }

    #[tokio::test]
    async fn none_layers_are_skipped() {
        let engine = CompositeEngine::new()
            .with_layer(BasePromptLayer::new("first"))
            .with_layer(BasePromptLayer::new("")) // renders None
            .with_layer(BasePromptLayer::new("third"));

        let out = engine.assemble_system_prompt(&ctx()).await;
        assert_eq!(out, "first\n\nthird");
    }

    #[tokio::test]
    async fn full_stack_assembles_in_registration_order() {
        let storage: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        storage.write("SOUL.md", b"warm and playful").await.unwrap();
        storage
            .write("USER.md", b"- Codes in Rust\n- Likes terse answers")
            .await
            .unwrap();
        storage
            .write("MEMORY.md", b"User is building runic.")
            .await
            .unwrap();

        let engine = CompositeEngine::new()
            .with_layer(BasePromptLayer::new("You are a focused assistant."))
            .with_layer(PersonaLayer::new(storage.clone(), "SOUL.md"))
            .with_layer(UserFactsLayer::new(storage.clone(), "USER.md"))
            .with_layer(MemoryLayer::new(storage.clone(), "MEMORY.md"));

        let out = engine.assemble_system_prompt(&ctx()).await;

        // Base first (registered first), then persona, user-facts, memory.
        assert!(out.starts_with("You are a focused assistant."));
        let persona_pos = out.find("<persona>").unwrap();
        let user_pos = out.find("<user-facts>").unwrap();
        let memory_pos = out.find("<memory>").unwrap();
        assert!(persona_pos < user_pos);
        assert!(user_pos < memory_pos);
        assert!(out.contains("warm and playful"));
        assert!(out.contains("Codes in Rust"));
        assert!(out.contains("User is building runic."));
    }

    #[tokio::test]
    async fn missing_memory_files_dont_break_the_assembly() {
        let storage: Arc<dyn StorageBackend> = Arc::new(MemoryBackend::new());
        storage.write("SOUL.md", b"terse").await.unwrap();

        let engine = CompositeEngine::new()
            .with_layer(BasePromptLayer::new("base"))
            .with_layer(PersonaLayer::new(storage.clone(), "SOUL.md"))
            .with_layer(UserFactsLayer::new(storage.clone(), "USER.md"))
            .with_layer(MemoryLayer::new(storage.clone(), "MEMORY.md"));

        let out = engine.assemble_system_prompt(&ctx()).await;
        assert!(out.contains("base"));
        assert!(out.contains("<persona>"));
        assert!(!out.contains("<user-facts>"));
        assert!(!out.contains("<memory>"));
    }

    #[tokio::test]
    async fn other_context_engine_methods_still_default() {
        let engine = CompositeEngine::new();
        assert!(engine.ambient_notes(&ctx()).await.is_empty());

        let msg = Message::user("hi");
        let out = engine.process_user_input(&ctx(), msg.clone()).await;
        assert_eq!(format!("{out:?}"), format!("{msg:?}"));

        let mut messages = vec![Message::user("a")];
        engine.maybe_compact(&ctx(), &mut messages).await;
        assert_eq!(messages.len(), 1);
    }
}
