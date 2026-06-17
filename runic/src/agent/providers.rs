use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use runic_provider_anthropic::{AnthropicConfig, AnthropicProvider};
use runic_provider_core::Provider;
use runic_provider_gemini::{GeminiConfig, GeminiProvider};
use runic_provider_openai::{OpenAiConfig, OpenAiProvider};

pub const DEFAULT_PROVIDER: &str = "sonnet";

pub struct Providers {
    map: HashMap<String, Arc<dyn Provider>>,
}

impl Providers {
    pub fn from_env() -> Result<Self> {
        let mut map: HashMap<String, Arc<dyn Provider>> = HashMap::new();

        if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
            let sonnet = AnthropicProvider::new(
                AnthropicConfig::new(key.clone()).with_model("claude-sonnet-4-6"),
            );
            let haiku = AnthropicProvider::new(
                AnthropicConfig::new(key).with_model("claude-haiku-4-5-20251001"),
            );
            map.insert("sonnet".into(), sonnet as Arc<dyn Provider>);
            map.insert("haiku".into(), haiku as Arc<dyn Provider>);
        }

        if let Ok(key) = std::env::var("GEMINI_API_KEY") {
            let gemini =
                GeminiProvider::new(GeminiConfig::new(key.clone()).with_model("gemini-3.5-flash"));
            map.insert("gemini".into(), gemini as Arc<dyn Provider>);
        }

        if let Ok(key) = std::env::var("MISTRAL_API_KEY") {
            let mistral = OpenAiProvider::mistral(
                OpenAiConfig::mistral(key).with_model("mistral-medium-latest"),
            );
            map.insert("mistral".into(), mistral as Arc<dyn Provider>);
        }

        if map.is_empty() {
            anyhow::bail!(
                "no provider keys set (ANTHROPIC_API_KEY / GEMINI_API_KEY / MISTRAL_API_KEY)"
            );
        }
        Ok(Self { map })
    }

    /// Look up a provider by key (`sonnet`, `haiku`, `gemini`, `mistral`).
    /// `None` if the key isn't configured (its API key wasn't set).
    pub fn get(&self, key: &str) -> Option<Arc<dyn Provider>> {
        self.map.get(key).cloned()
    }

    /// Resolve an `AGENT.md` `provider:` key into a concrete provider,
    /// falling back to `parent` when the key is absent (sub-agent inherits
    /// the parent) or names a provider that isn't configured. The warning
    /// on an unknown key keeps a typo from silently running on the wrong
    /// model.
    pub fn resolve_or(
        &self,
        key: Option<&str>,
        parent: &Arc<dyn Provider>,
    ) -> Arc<dyn Provider> {
        match key {
            None => parent.clone(),
            Some(k) => self.get(k).unwrap_or_else(|| {
                tracing::warn!(
                    provider = k,
                    "AGENT.md provider key not configured — inheriting parent provider"
                );
                parent.clone()
            }),
        }
    }
}
