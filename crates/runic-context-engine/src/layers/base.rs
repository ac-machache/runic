//! `BasePromptLayer` — wraps the static system prompt configured on the
//! agent. Typically registered first so it appears at the top.

use async_trait::async_trait;

use crate::layer::ContextLayer;
use crate::TurnContext;

pub struct BasePromptLayer {
    prompt: String,
}

impl BasePromptLayer {
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
        }
    }
}

#[async_trait]
impl ContextLayer for BasePromptLayer {
    fn name(&self) -> &str {
        "base"
    }

    async fn render(&self, _ctx: &TurnContext<'_>) -> Option<String> {
        let trimmed = self.prompt.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runic_message_types::Message;

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
    async fn renders_the_static_prompt() {
        let layer = BasePromptLayer::new("you are focused");
        let out = layer.render(&ctx()).await;
        assert_eq!(out.as_deref(), Some("you are focused"));
    }

    #[tokio::test]
    async fn empty_prompt_renders_as_none() {
        let layer = BasePromptLayer::new("   \n  ");
        assert!(layer.render(&ctx()).await.is_none());
    }
}
