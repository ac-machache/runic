//! Anthropic Messages API provider.
//!
//! Written fresh (not vendored) targeting the documented `/v1/messages`
//! streaming API with a plain bearer-key auth. The jcode equivalent
//! (`src/provider/anthropic.rs`, 2041 lines) bundles OAuth via the official
//! Claude CLI, 1M-context beta routing, cache TTL toggles, and a Stainless
//! attribution dance — none of which we need for a personal API-key setup.

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::stream::StreamExt;
use runic_message_types::{ConnectionPhase, ContentBlock, Message, Role, StreamEvent, ToolDefinition};
use runic_provider_core::{
    EventStream, Provider, ProviderError, RetryPolicy, shared_http_client, with_retry,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::warn;

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const DEFAULT_API_VERSION: &str = "2023-06-01";
const DEFAULT_MODEL: &str = "claude-sonnet-4-5";
const DEFAULT_MAX_TOKENS: u32 = 8192;

/// Configuration for the Anthropic provider.
#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    pub api_key: String,
    pub model: String,
    pub base_url: String,
    pub api_version: String,
    pub max_tokens: u32,
    /// When set, sends `thinking: { type: "enabled", budget_tokens }`.
    pub thinking_budget_tokens: Option<u32>,
    /// Adds `cache_control: { type: "ephemeral" }` to the system block.
    /// Enabled by default to keep stable prefixes warm.
    pub cache_system_prompt: bool,
}

impl AnthropicConfig {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: DEFAULT_MODEL.to_string(),
            base_url: DEFAULT_BASE_URL.to_string(),
            api_version: DEFAULT_API_VERSION.to_string(),
            max_tokens: DEFAULT_MAX_TOKENS,
            thinking_budget_tokens: None,
            cache_system_prompt: true,
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    pub fn with_thinking_budget(mut self, budget_tokens: u32) -> Self {
        self.thinking_budget_tokens = Some(budget_tokens);
        self
    }
}

/// Anthropic Messages API provider.
pub struct AnthropicProvider {
    config: RwLock<AnthropicConfig>,
}

impl AnthropicProvider {
    pub fn new(config: AnthropicConfig) -> Arc<Self> {
        Arc::new(Self {
            config: RwLock::new(config),
        })
    }

    fn snapshot_config(&self) -> AnthropicConfig {
        self.config.read().expect("config lock poisoned").clone()
    }

    fn build_payload(
        &self,
        cfg: &AnthropicConfig,
        messages: &[Message],
        tools: &[ToolDefinition],
        system: &str,
    ) -> serde_json::Value {
        let mut payload = serde_json::json!({
            "model": cfg.model,
            "max_tokens": cfg.max_tokens,
            "messages": messages_to_anthropic(messages),
            "stream": true,
        });

        if !system.trim().is_empty() {
            if cfg.cache_system_prompt {
                payload["system"] = serde_json::json!([{
                    "type": "text",
                    "text": system,
                    "cache_control": { "type": "ephemeral" },
                }]);
            } else {
                payload["system"] = serde_json::Value::String(system.to_string());
            }
        }

        if !tools.is_empty() {
            payload["tools"] = serde_json::json!(tools);
        }

        if let Some(budget) = cfg.thinking_budget_tokens {
            payload["thinking"] = serde_json::json!({
                "type": "enabled",
                "budget_tokens": budget,
            });
        }

        payload
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    async fn complete(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream, ProviderError> {
        let cfg = self.snapshot_config();
        let payload = self.build_payload(&cfg, messages, tools, system);

        let url = format!("{}/v1/messages", cfg.base_url.trim_end_matches('/'));
        let client = shared_http_client();

        // Create the channel up front so we can emit ConnectionPhase::Retrying
        // events from the retry callback before the streaming task starts.
        let (tx, rx) = mpsc::channel::<Result<StreamEvent, ProviderError>>(64);

        let policy = RetryPolicy::default();
        let tx_for_retry = tx.clone();
        let response = with_retry(
            &policy,
            move |attempt, max, _delay| {
                let _ = tx_for_retry.try_send(Ok(StreamEvent::ConnectionPhase {
                    phase: ConnectionPhase::Retrying { attempt, max },
                }));
            },
            || {
                let url = url.clone();
                let cfg = cfg.clone();
                let payload = payload.clone();
                let client = client.clone();
                async move {
                    let response = client
                        .post(&url)
                        .header("x-api-key", &cfg.api_key)
                        .header("anthropic-version", &cfg.api_version)
                        .header("content-type", "application/json")
                        .header("accept", "text/event-stream")
                        .json(&payload)
                        .send()
                        .await
                        .map_err(ProviderError::Transport)?;

                    let status = response.status();
                    if !status.is_success() {
                        let retry_after = response
                            .headers()
                            .get("retry-after")
                            .and_then(|v| v.to_str().ok())
                            .and_then(|s| s.parse::<u64>().ok());
                        let body = response.text().await.unwrap_or_default();

                        return Err(if status.as_u16() == 401 || status.as_u16() == 403 {
                            ProviderError::Auth(body)
                        } else if status.as_u16() == 429 {
                            ProviderError::RateLimit {
                                message: body,
                                retry_after_secs: retry_after,
                            }
                        } else {
                            ProviderError::Http {
                                status: status.as_u16(),
                                body,
                            }
                        });
                    }
                    Ok(response)
                }
            },
        )
        .await?;

        let byte_stream = response.bytes_stream();
        let mut sse = byte_stream.eventsource();

        tokio::spawn(async move {
            let mut state = SseState::default();
            while let Some(event_res) = sse.next().await {
                match event_res {
                    Ok(event) => {
                        if let Some(emitted) = state.handle(&event.event, &event.data) {
                            for ev in emitted {
                                if tx.send(Ok(ev)).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                    Err(err) => {
                        let _ = tx
                            .send(Err(ProviderError::decode(format!(
                                "SSE decode error: {err}"
                            ))))
                            .await;
                        return;
                    }
                }
            }
        });

        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn name(&self) -> &str {
        "anthropic"
    }

    fn model(&self) -> String {
        self.snapshot_config().model
    }

    fn supports_image_input(&self) -> bool {
        true
    }

    fn available_models(&self) -> Vec<&'static str> {
        vec![
            "claude-opus-4-1",
            "claude-opus-4-5",
            "claude-sonnet-4-5",
            "claude-sonnet-4-6",
            "claude-haiku-4-5",
        ]
    }

    fn set_model(&self, model: &str) -> Result<(), ProviderError> {
        self.config
            .write()
            .expect("config lock poisoned")
            .model = model.to_string();
        Ok(())
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self {
            config: RwLock::new(self.snapshot_config()),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    ToolUse,
    Thinking,
}

#[derive(Default)]
struct SseState {
    /// Track which kind of block is open at each index, so `content_block_stop`
    /// can emit the correct end event.
    open_blocks: HashMap<u64, BlockKind>,
    last_stop_reason: Option<String>,
}

impl SseState {
    fn handle(&mut self, event: &str, data: &str) -> Option<Vec<StreamEvent>> {
        let parsed: serde_json::Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return None,
        };

        let mut out: Vec<StreamEvent> = Vec::new();

        match event {
            "message_start" => {
                if let Some(message) = parsed.get("message") {
                    if let Some(id) = message.get("id").and_then(|v| v.as_str()) {
                        out.push(StreamEvent::SessionId(id.to_string()));
                    }
                    if let Some(usage) = message.get("usage") {
                        out.push(usage_event(usage));
                    }
                }
            }
            "content_block_start" => {
                let index = parsed.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                if let Some(block) = parsed.get("content_block") {
                    match block.get("type").and_then(|v| v.as_str()) {
                        Some("text") => {
                            self.open_blocks.insert(index, BlockKind::Text);
                        }
                        Some("tool_use") => {
                            self.open_blocks.insert(index, BlockKind::ToolUse);
                            let id = block
                                .get("id")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string();
                            let name = block
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string();
                            out.push(StreamEvent::ToolUseStart { id, name });
                        }
                        Some("thinking") => {
                            self.open_blocks.insert(index, BlockKind::Thinking);
                            out.push(StreamEvent::ThinkingStart);
                        }
                        other => {
                            warn!(?other, "unknown content_block type");
                        }
                    }
                }
            }
            "content_block_delta" => {
                if let Some(delta) = parsed.get("delta") {
                    match delta.get("type").and_then(|v| v.as_str()) {
                        Some("text_delta") => {
                            if let Some(text) = delta.get("text").and_then(|v| v.as_str()) {
                                out.push(StreamEvent::TextDelta(text.to_string()));
                            }
                        }
                        Some("input_json_delta") => {
                            if let Some(partial) =
                                delta.get("partial_json").and_then(|v| v.as_str())
                            {
                                out.push(StreamEvent::ToolInputDelta(partial.to_string()));
                            }
                        }
                        Some("thinking_delta") => {
                            if let Some(text) = delta.get("thinking").and_then(|v| v.as_str()) {
                                out.push(StreamEvent::ThinkingDelta(text.to_string()));
                            }
                        }
                        Some("signature_delta") => {
                            // We ignore signatures for now (only meaningful for replay).
                        }
                        other => {
                            warn!(?other, "unknown content_block_delta type");
                        }
                    }
                }
            }
            "content_block_stop" => {
                let index = parsed.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                if let Some(kind) = self.open_blocks.remove(&index) {
                    match kind {
                        BlockKind::ToolUse => out.push(StreamEvent::ToolUseEnd),
                        BlockKind::Thinking => out.push(StreamEvent::ThinkingEnd),
                        BlockKind::Text => {}
                    }
                }
            }
            "message_delta" => {
                if let Some(delta) = parsed.get("delta")
                    && let Some(stop_reason) = delta.get("stop_reason").and_then(|v| v.as_str())
                {
                    self.last_stop_reason = Some(stop_reason.to_string());
                }
                if let Some(usage) = parsed.get("usage") {
                    out.push(usage_event(usage));
                }
            }
            "message_stop" => {
                out.push(StreamEvent::MessageEnd {
                    stop_reason: self.last_stop_reason.take(),
                });
            }
            "ping" => {}
            "error" => {
                let message = parsed
                    .get("error")
                    .and_then(|err| err.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown anthropic error")
                    .to_string();
                out.push(StreamEvent::Error {
                    message,
                    retry_after_secs: None,
                });
            }
            other => {
                warn!(event = other, "unhandled anthropic SSE event");
            }
        }

        if out.is_empty() { None } else { Some(out) }
    }
}

fn usage_event(usage: &serde_json::Value) -> StreamEvent {
    StreamEvent::TokenUsage {
        input_tokens: usage.get("input_tokens").and_then(|v| v.as_u64()),
        output_tokens: usage.get("output_tokens").and_then(|v| v.as_u64()),
        cache_read_input_tokens: usage
            .get("cache_read_input_tokens")
            .and_then(|v| v.as_u64()),
        cache_creation_input_tokens: usage
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64()),
    }
}

fn messages_to_anthropic(messages: &[Message]) -> Vec<serde_json::Value> {
    messages
        .iter()
        .filter_map(message_to_anthropic)
        .collect()
}

fn message_to_anthropic(msg: &Message) -> Option<serde_json::Value> {
    let content: Vec<serde_json::Value> = msg
        .content
        .iter()
        .filter_map(content_block_to_anthropic)
        .collect();

    if content.is_empty() {
        return None;
    }

    let role_str = match msg.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };

    Some(serde_json::json!({
        "role": role_str,
        "content": content,
    }))
}

fn content_block_to_anthropic(block: &ContentBlock) -> Option<serde_json::Value> {
    match block {
        ContentBlock::Text {
            text,
            cache_control,
        } => {
            let mut obj = serde_json::json!({ "type": "text", "text": text });
            if let Some(cc) = cache_control {
                obj["cache_control"] = serde_json::to_value(cc).ok()?;
            }
            Some(obj)
        }
        ContentBlock::ToolUse { id, name, input } => Some(serde_json::json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            "input": input,
        })),
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            let mut obj = serde_json::json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": content,
            });
            if let Some(err) = is_error {
                obj["is_error"] = serde_json::Value::Bool(*err);
            }
            Some(obj)
        }
        ContentBlock::Image { media_type, data } => Some(serde_json::json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": media_type,
                "data": data,
            },
        })),
        // Replaying a thinking block requires a signature we don't carry yet.
        // Drop on send; the in-memory transcript still keeps it.
        ContentBlock::Reasoning { .. } => None,
        // Blob references are by-id placeholders — they MUST be materialized
        // (fetched from the BlobStore, base64-encoded, rewritten to Image
        // content blocks) BEFORE reaching here. The materialization happens
        // in the agent loop / a Provider decorator. If we see one here, it
        // means materialization was skipped — surface that as a dropped
        // block rather than silently sending nothing.
        ContentBlock::Blob(_) => None,
    }
}

/// Public payload structs for callers that want to introspect requests.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicMessage {
    pub role: String,
    pub content: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_are_sensible() {
        let cfg = AnthropicConfig::new("sk-test");
        assert_eq!(cfg.model, DEFAULT_MODEL);
        assert_eq!(cfg.max_tokens, DEFAULT_MAX_TOKENS);
        assert!(cfg.cache_system_prompt);
    }

    #[test]
    fn user_message_serializes_to_anthropic_shape() {
        let msg = Message::user("hello");
        let json = message_to_anthropic(&msg).unwrap();
        assert_eq!(json["role"], "user");
        assert_eq!(json["content"][0]["type"], "text");
        assert_eq!(json["content"][0]["text"], "hello");
    }

    #[test]
    fn tool_result_carries_is_error_when_true() {
        let msg = Message::tool_result("toolu_1", "boom", true);
        let json = message_to_anthropic(&msg).unwrap();
        assert_eq!(json["content"][0]["is_error"], true);
    }

    #[test]
    fn sse_state_emits_text_delta() {
        let mut state = SseState::default();
        let out = state
            .handle(
                "content_block_delta",
                r#"{"index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
            )
            .unwrap();
        assert!(matches!(&out[0], StreamEvent::TextDelta(t) if t == "hi"));
    }

    #[test]
    fn sse_state_tracks_tool_use_lifecycle() {
        let mut state = SseState::default();
        let start = state
            .handle(
                "content_block_start",
                r#"{"index":1,"content_block":{"type":"tool_use","id":"toolu_x","name":"bash"}}"#,
            )
            .unwrap();
        assert!(matches!(&start[0], StreamEvent::ToolUseStart { id, name } if id == "toolu_x" && name == "bash"));

        let stop = state
            .handle("content_block_stop", r#"{"index":1}"#)
            .unwrap();
        assert!(matches!(&stop[0], StreamEvent::ToolUseEnd));
    }

    #[test]
    fn sse_state_emits_message_end_with_stop_reason() {
        let mut state = SseState::default();
        state.handle(
            "message_delta",
            r#"{"delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":42}}"#,
        );
        let stop = state.handle("message_stop", r#"{}"#).unwrap();
        let stop_reason = stop.iter().find_map(|ev| match ev {
            StreamEvent::MessageEnd { stop_reason } => stop_reason.clone(),
            _ => None,
        });
        assert_eq!(stop_reason.as_deref(), Some("tool_use"));
    }
}
