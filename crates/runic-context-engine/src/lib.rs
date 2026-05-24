//! runic-context-engine — the brain that manages context lifecycle.
//!
//! Owns: system-prompt assembly, tool-result mediation (spillover /
//! truncation), ambient injections (background completion notifications,
//! freshly-updated memory), compaction policy, user-input preprocessing.
//!
//! Two real implementations ship: `NoopEngine` (identity for everything)
//! and `CompositeEngine` (configurable, composed of `ContextLayer`s plus
//! policies for spillover and compaction).

pub mod compactor;
pub mod composite;
pub mod layer;
pub mod layers;
pub mod spillover;

pub use compactor::{CompactorEngine, DEFAULT_KEEP_RECENT, DEFAULT_TOKEN_THRESHOLD};
pub use composite::CompositeEngine;
pub use layer::ContextLayer;
pub use layers::{BasePromptLayer, FileLayer, MemoryLayer, PersonaLayer, UserFactsLayer};
pub use spillover::{SpilloverEngine, DEFAULT_PREVIEW_CHARS, DEFAULT_THRESHOLD_BYTES};

use async_trait::async_trait;
use runic_message_types::Message;

#[derive(Debug, Clone)]
pub struct AmbientNote {
    pub source: String,
    pub content: String,
    pub dedup_key: Option<String>,
}

#[derive(Debug)]
pub struct TurnContext<'a> {
    pub base_system_prompt: &'a str,
    pub messages: &'a [Message],
    pub run_id: &'a str,
    pub turn: u32,
}

#[async_trait]
pub trait ContextEngine: Send + Sync {
    async fn assemble_system_prompt(&self, ctx: &TurnContext<'_>) -> String {
        ctx.base_system_prompt.to_string()
    }
    async fn ambient_notes(&self, _ctx: &TurnContext<'_>) -> Vec<AmbientNote> {
        Vec::new()
    }
    async fn process_user_input(&self, _ctx: &TurnContext<'_>, msg: Message) -> Message {
        msg
    }
    async fn maybe_compact(&self, _ctx: &TurnContext<'_>, _messages: &mut Vec<Message>) {}
}

#[derive(Debug, Clone, Default)]
pub struct NoopEngine;

#[async_trait]
impl ContextEngine for NoopEngine {}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>() -> TurnContext<'a> {
        TurnContext {
            base_system_prompt: "you are a focused assistant",
            messages: &[],
            run_id: "run-1",
            turn: 0,
        }
    }

    #[tokio::test]
    async fn noop_engine_passes_base_prompt_through() {
        let engine = NoopEngine;
        let ctx = ctx();
        let prompt = engine.assemble_system_prompt(&ctx).await;
        assert_eq!(prompt, "you are a focused assistant");
    }

    #[tokio::test]
    async fn noop_engine_emits_no_ambient_notes() {
        let engine = NoopEngine;
        let ctx = ctx();
        let notes = engine.ambient_notes(&ctx).await;
        assert!(notes.is_empty());
    }

    #[tokio::test]
    async fn noop_engine_returns_user_input_unchanged() {
        let engine = NoopEngine;
        let ctx = ctx();
        let input = Message::user("hello");
        let original = format!("{:?}", input);
        let output = engine.process_user_input(&ctx, input).await;
        assert_eq!(format!("{:?}", output), original);
    }

    #[tokio::test]
    async fn noop_engine_maybe_compact_does_not_mutate() {
        let engine = NoopEngine;
        let ctx = ctx();
        let mut messages = vec![Message::user("a"), Message::user("b")];
        let before_len = messages.len();
        engine.maybe_compact(&ctx, &mut messages).await;
        assert_eq!(messages.len(), before_len);
    }
}
