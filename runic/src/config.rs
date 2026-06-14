//! `RunicConfig` — every environment knob parsed in one place.
//!
//! Both surfaces (REPL and `serve`) read the same configuration, so the
//! parsing lives here rather than inline in either entrypoint.

use std::path::PathBuf;

use anyhow::{Context, Result};
use runic_provider_anthropic::{AnthropicConfig, AnthropicProvider};
use runic_provider_core::Provider;
use runic_provider_gemini::{GeminiConfig, GeminiProvider};
use runic_provider_openai::{OpenAiConfig, OpenAiProvider};
use std::sync::Arc;

/// Resolved configuration for a runic process.
#[derive(Debug, Clone)]
pub struct RunicConfig {
    pub runic_home: PathBuf,
    pub provider_kind: String,
    pub model_override: Option<String>,
    pub tenant: String,
    pub compact_threshold: usize,
    pub spill_threshold: usize,
    pub spillover_retention_days: i64,
    pub persist: bool,
    pub user_id: String,
    pub org_id: String,
}

impl RunicConfig {
    /// Parse the environment into a config. Pure except for reading env
    /// vars — does not touch the network or filesystem.
    pub fn from_env() -> Self {
        let runic_home = std::env::var("RUNIC_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let mut p = home_or_cwd();
                p.push(".runic");
                p
            });

        Self {
            runic_home,
            provider_kind: std::env::var("RUNIC_PROVIDER")
                .unwrap_or_else(|_| "anthropic".into())
                .to_lowercase(),
            model_override: std::env::var("RUNIC_MODEL").ok(),
            tenant: std::env::var("RUNIC_TENANT").unwrap_or_else(|_| "default".into()),
            compact_threshold: parse_or("RUNIC_COMPACT_THRESHOLD", runic_context_engine::DEFAULT_TOKEN_THRESHOLD),
            spill_threshold: parse_or("RUNIC_SPILLOVER_THRESHOLD", runic_context_engine::DEFAULT_THRESHOLD_BYTES),
            spillover_retention_days: parse_or("RUNIC_SPILLOVER_RETENTION_DAYS", 14),
            persist: std::env::var("RUNIC_PERSIST").as_deref() == Ok("1"),
            user_id: std::env::var("RUNIC_USER_ID").unwrap_or_default(),
            org_id: std::env::var("RUNIC_ORG_ID").unwrap_or_default(),
        }
    }

    /// Build the raw (non-blob-wrapped) provider this config selects.
    /// The harness wraps it in a `BlobMaterializingProvider` afterwards.
    pub fn build_raw_provider(&self) -> Result<Arc<dyn Provider>> {
        let provider: Arc<dyn Provider> = match self.provider_kind.as_str() {
            "anthropic" => {
                let key = std::env::var("ANTHROPIC_API_KEY")
                    .context("ANTHROPIC_API_KEY must be set when RUNIC_PROVIDER=anthropic")?;
                let mut cfg = AnthropicConfig::new(key);
                if let Some(m) = &self.model_override {
                    cfg = cfg.with_model(m.clone());
                }
                AnthropicProvider::new(cfg)
            }
            "gemini" => {
                let key = std::env::var("GEMINI_API_KEY")
                    .context("GEMINI_API_KEY must be set when RUNIC_PROVIDER=gemini")?;
                let mut cfg = GeminiConfig::new(key);
                if let Some(m) = &self.model_override {
                    cfg = cfg.with_model(m.clone());
                }
                GeminiProvider::new(cfg)
            }
            "mistral" => {
                let key = std::env::var("MISTRAL_API_KEY")
                    .context("MISTRAL_API_KEY must be set when RUNIC_PROVIDER=mistral")?;
                let mut cfg = OpenAiConfig::mistral(key);
                if let Some(m) = &self.model_override {
                    cfg = cfg.with_model(m.clone());
                }
                OpenAiProvider::mistral(cfg)
            }
            "openai" => {
                let key = std::env::var("OPENAI_API_KEY")
                    .context("OPENAI_API_KEY must be set when RUNIC_PROVIDER=openai")?;
                let mut cfg = OpenAiConfig::new(key);
                if let Some(m) = &self.model_override {
                    cfg = cfg.with_model(m.clone());
                }
                OpenAiProvider::new(cfg)
            }
            // Any other OpenAI-compatible endpoint: point it via
            // RUNIC_OPENAI_BASE_URL (+ OPENAI_API_KEY). Covers Groq,
            // OpenRouter, local LM Studio / Ollama shims, etc.
            "openai-compatible" => {
                let key = std::env::var("OPENAI_API_KEY").unwrap_or_default();
                let base = std::env::var("RUNIC_OPENAI_BASE_URL").context(
                    "RUNIC_OPENAI_BASE_URL must be set when RUNIC_PROVIDER=openai-compatible",
                )?;
                let mut cfg = OpenAiConfig::new(key).with_base_url(base);
                if let Some(m) = &self.model_override {
                    cfg = cfg.with_model(m.clone());
                }
                OpenAiProvider::new(cfg)
            }
            other => {
                anyhow::bail!(
                    "unknown RUNIC_PROVIDER='{other}' (expected: anthropic | gemini | mistral | openai | openai-compatible)"
                );
            }
        };
        Ok(provider)
    }
}

fn parse_or<T: std::str::FromStr>(var: &str, default: T) -> T {
    std::env::var(var)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// Resolve the user's home directory (HOME on Unix, USERPROFILE on
/// Windows). Falls back to the current working directory.
pub fn home_or_cwd() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home);
    }
    if let Ok(home) = std::env::var("USERPROFILE") {
        return PathBuf::from(home);
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}
