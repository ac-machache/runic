//! `ContextLayer` — one slice of the system prompt.
//!
//! A `CompositeEngine` is composed of many layers. Each layer renders
//! independently and returns its slice of text (or `None` to skip). The
//! engine concatenates the layers in **registration order** — the first
//! layer you add appears first, the last layer you add appears last.

use async_trait::async_trait;

use crate::TurnContext;

/// One contributor to the assembled system prompt.
#[async_trait]
pub trait ContextLayer: Send + Sync {
    /// Short identifier for diagnostics / logging.
    fn name(&self) -> &str;

    /// Render this layer's contribution. Return `None` to skip (file
    /// missing, content empty, condition not met, etc).
    async fn render(&self, ctx: &TurnContext<'_>) -> Option<String>;
}
