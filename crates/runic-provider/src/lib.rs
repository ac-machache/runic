//! `runic-provider` — Layer 2 contract: the LLM provider trait + its
//! request/response/stream/error types.
//!
//! Adapted from OpenFang's `LlmDriver` (`openfang-runtime/src/llm_driver.rs`),
//! retyped onto [`runic_types`] and renamed to runic's `Provider`. OpenFang's
//! `LlmError` is already a proper typed enum, so it's kept (as `ProviderError`).
//! A centralized retry combinator + a capabilities catalog land next.

use async_trait::async_trait;
use runic_types::{ContentBlock, Message, StopReason, TokenUsage, ToolCall, ToolDefinition};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Concrete drivers, each behind its feature.
#[cfg(feature = "anthropic")]
pub mod anthropic;
#[cfg(feature = "anthropic")]
pub use anthropic::AnthropicDriver;

#[cfg(feature = "openai")]
pub mod openai;
#[cfg(feature = "openai")]
mod think_filter; // helper used by the openai driver

#[cfg(feature = "gemini")]
pub mod gemini;

/// Error type for provider operations.
#[derive(Error, Debug)]
pub enum ProviderError {
    /// HTTP request failed.
    #[error("HTTP error: {0}")]
    Http(String),
    /// API returned an error.
    #[error("API error ({status}): {message}")]
    Api { status: u16, message: String },
    /// Rate limited — should retry after delay.
    #[error("Rate limited, retry after {retry_after_ms}ms")]
    RateLimited { retry_after_ms: u64 },
    /// Response parsing failed.
    #[error("Parse error: {0}")]
    Parse(String),
    /// No API key configured.
    #[error("Missing API key: {0}")]
    MissingApiKey(String),
    /// Model overloaded.
    #[error("Model overloaded, retry after {retry_after_ms}ms")]
    Overloaded { retry_after_ms: u64 },
    /// Authentication failed (invalid/missing API key).
    #[error("Authentication failed: {0}")]
    AuthenticationFailed(String),
    /// Model not found.
    #[error("Model not found: {0}")]
    ModelNotFound(String),
}

/// Extended-thinking configuration (if the model supports it).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThinkingConfig {
    /// Turn extended thinking on.
    pub enabled: bool,
    /// Optional token budget for the thinking phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
}

/// A request to a provider for completion.
#[derive(Debug, Clone)]
pub struct CompletionRequest {
    /// Model identifier.
    pub model: String,
    /// Conversation messages.
    pub messages: Vec<Message>,
    /// Available tools the model can use.
    pub tools: Vec<ToolDefinition>,
    /// Maximum tokens to generate.
    pub max_tokens: u32,
    /// Sampling temperature.
    pub temperature: f32,
    /// System prompt (kept separate for APIs that need it that way).
    pub system: Option<String>,
    /// Extended thinking configuration (if supported by the model).
    pub thinking: Option<ThinkingConfig>,
}

/// A response from a provider completion.
#[derive(Debug, Clone)]
pub struct CompletionResponse {
    /// The content blocks in the response.
    pub content: Vec<ContentBlock>,
    /// Why the model stopped generating.
    pub stop_reason: StopReason,
    /// Tool calls extracted from the response.
    pub tool_calls: Vec<ToolCall>,
    /// Token usage statistics.
    pub usage: TokenUsage,
}

impl CompletionResponse {
    /// Extract text content from the response.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    /// Whether the response has any meaningful content (incl. thinking).
    pub fn has_any_content(&self) -> bool {
        self.content.iter().any(|block| match block {
            ContentBlock::Text { text, .. } => !text.is_empty(),
            ContentBlock::Thinking { thinking, .. } => !thinking.is_empty(),
            ContentBlock::RedactedThinking { data } => !data.is_empty(),
            ContentBlock::ToolUse { .. } | ContentBlock::Image { .. } => true,
            _ => false,
        })
    }
}

/// Events emitted during streaming completion.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// Incremental text content.
    TextDelta { text: String },
    /// A tool use block has started.
    ToolUseStart { id: String, name: String },
    /// Incremental JSON input for an in-progress tool use.
    ToolInputDelta { text: String },
    /// A tool use block is complete with parsed input.
    ToolUseEnd {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// Incremental thinking/reasoning text.
    ThinkingDelta { text: String },
    /// The entire response is complete.
    ContentComplete {
        stop_reason: StopReason,
        usage: TokenUsage,
    },
}

/// The LLM provider contract. Concrete providers (Layer 3) implement this.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Stable identifier for this provider (e.g. `"anthropic"`, `"gemini"`,
    /// `"openai"`) — for logging/labeling, not parsed by callers.
    fn name(&self) -> &str {
        "unknown"
    }

    /// Send a completion request and get the assembled response.
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError>;

    /// Stream a completion, sending incremental events to the channel and
    /// returning the full assembled response. The default wraps `complete()`;
    /// real providers override it to stream natively.
    async fn stream(
        &self,
        request: CompletionRequest,
        tx: tokio::sync::mpsc::Sender<StreamEvent>,
    ) -> Result<CompletionResponse, ProviderError> {
        let response = self.complete(request).await?;
        let text = response.text();
        if !text.is_empty() {
            let _ = tx.send(StreamEvent::TextDelta { text }).await;
        }
        let _ = tx
            .send(StreamEvent::ContentComplete {
                stop_reason: response.stop_reason,
                usage: response.usage,
            })
            .await;
        Ok(response)
    }
}

/// Config for constructing a provider (the API key is redacted in `Debug`).
#[derive(Clone, Serialize, Deserialize)]
pub struct DriverConfig {
    /// Provider name.
    pub provider: String,
    /// API key.
    pub api_key: Option<String>,
    /// Base URL override.
    pub base_url: Option<String>,
}

impl std::fmt::Debug for DriverConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DriverConfig")
            .field("provider", &self.provider)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("base_url", &self.base_url)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_response_text_joins_text_blocks() {
        let response = CompletionResponse {
            content: vec![
                ContentBlock::Text {
                    text: "Hello ".to_string(),
                    provider_metadata: None,
                },
                ContentBlock::Text {
                    text: "world!".to_string(),
                    provider_metadata: None,
                },
            ],
            stop_reason: StopReason::EndTurn,
            tool_calls: vec![],
            usage: TokenUsage::default(),
        };
        assert_eq!(response.text(), "Hello world!");
    }

    #[test]
    fn has_any_content_detects_tool_use() {
        let response = CompletionResponse {
            content: vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "search".into(),
                input: serde_json::json!({}),
                provider_metadata: None,
            }],
            stop_reason: StopReason::ToolUse,
            tool_calls: vec![],
            usage: TokenUsage::default(),
        };
        assert!(response.has_any_content());
    }

    #[test]
    fn driver_config_redacts_api_key() {
        let cfg = DriverConfig {
            provider: "anthropic".into(),
            api_key: Some("sk-secret".into()),
            base_url: None,
        };
        let dbg = format!("{cfg:?}");
        assert!(dbg.contains("<redacted>"));
        assert!(!dbg.contains("sk-secret"));
    }
}
