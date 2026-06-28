use std::sync::Arc;

use anyhow::{Context, Result};
use runic_provider::gemini::GeminiDriver;
use runic_provider::openai::OpenAIDriver;
use runic_provider::{AnthropicDriver, Provider};

pub const SONNET: &str = "claude-sonnet-4-6";
pub const HAIKU: &str = "claude-haiku-4-5-20251001";
pub const FLASH: &str = "gemini-3.5-flash";
pub const MISTRAL: &str = "mistral-medium-latest";

const GEMINI_BASE_URL: &str = "https://generativelanguage.googleapis.com";
const MISTRAL_BASE_URL: &str = "https://api.mistral.ai/v1";

#[derive(Clone)]
pub struct Providers {
    anthropic: Arc<dyn Provider>,
    gemini: Arc<dyn Provider>,
    mistral: Arc<dyn Provider>,
}

impl Providers {
    pub fn from_env() -> Result<Self> {
        let anthropic_key = std::env::var("ANTHROPIC_API_KEY")
            .or_else(|_| std::env::var("CLAUDE_API_KEY"))
            .context("set ANTHROPIC_API_KEY or CLAUDE_API_KEY")?;
        let anthropic_base = std::env::var("ANTHROPIC_BASE_URL")
            .unwrap_or_else(|_| "https://api.anthropic.com".to_string());

        let gemini_key = std::env::var("GEMINI_API_KEY")
            .or_else(|_| std::env::var("GOOGLE_API_KEY"))
            .unwrap_or_default();
        if gemini_key.is_empty() {
            tracing::warn!(
                "GEMINI_API_KEY/GOOGLE_API_KEY unset — product-expert will fail to call"
            );
        }
        let mistral_key = std::env::var("MISTRAL_API_KEY").unwrap_or_default();

        Ok(Self {
            anthropic: Arc::new(AnthropicDriver::new(anthropic_key, anthropic_base)),
            gemini: Arc::new(GeminiDriver::new(gemini_key, GEMINI_BASE_URL.to_string())),
            mistral: Arc::new(OpenAIDriver::new(mistral_key, MISTRAL_BASE_URL.to_string())),
        })
    }

    /// Resolve a named provider (the `.md` `provider:` field) to a driver.
    pub fn resolve(&self, name: Option<&str>) -> Arc<dyn Provider> {
        match name {
            Some("flash") => self.gemini.clone(),
            Some("mistral") => self.mistral.clone(),
            _ => self.anthropic.clone(),
        }
    }

    /// The orchestrator's (provider, model) for a `CORAL_PROVIDER` choice.
    pub fn main_model(&self, name: &str) -> (Arc<dyn Provider>, &'static str) {
        match name {
            "haiku" => (self.anthropic.clone(), HAIKU),
            "flash" => (self.gemini.clone(), FLASH),
            "mistral" => (self.mistral.clone(), MISTRAL),
            _ => (self.anthropic.clone(), SONNET),
        }
    }
}
