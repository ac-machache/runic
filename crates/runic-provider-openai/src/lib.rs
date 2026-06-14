//! OpenAI-compatible chat-completions provider.
//!
//! One client for every backend that speaks the OpenAI `/chat/completions`
//! protocol — Mistral (`https://api.mistral.ai/v1`), OpenAI itself, Groq,
//! OpenRouter, local servers (LM Studio / Ollama's OpenAI shim), etc. The
//! provider is the *protocol*; the endpoint is config. This mirrors how
//! jcode models Mistral: not a bespoke provider, just an OpenAI-compatible
//! profile (base URL + `MISTRAL_API_KEY`).
//!
//! Shape differences from Anthropic/Gemini handled here:
//! - System prompt is a leading `{role:"system"}` message.
//! - Tool *results* are their own `{role:"tool", tool_call_id}` messages.
//! - Tool *calls* are a `tool_calls` array on an assistant message, and
//!   stream incrementally: the first delta for a call carries `id`+`name`,
//!   later deltas carry `arguments` fragments, keyed by `index`.
//! - Stream terminates with a literal `data: [DONE]` line.

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::stream::StreamExt;
use runic_message_types::{ConnectionPhase, ContentBlock, Message, Role, StreamEvent, ToolDefinition};
use runic_provider_core::{
    shared_http_client, with_retry, EventStream, Provider, ProviderError, RetryPolicy,
};
use serde::Deserialize;
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

// ─── Config + provider ──────────────────────────────────────────────────────

const MISTRAL_BASE_URL: &str = "https://api.mistral.ai/v1";
const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_MAX_TOKENS: u32 = 4096;

#[derive(Debug, Clone)]
pub struct OpenAiConfig {
    pub api_key: String,
    pub model: String,
    /// Base URL including the version segment, e.g. `https://api.mistral.ai/v1`.
    /// `/chat/completions` is appended.
    pub base_url: String,
    pub max_tokens: u32,
}

impl OpenAiConfig {
    /// Generic OpenAI-compatible config pointed at OpenAI itself.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: "gpt-4o".to_string(),
            base_url: OPENAI_BASE_URL.to_string(),
            max_tokens: DEFAULT_MAX_TOKENS,
        }
    }

    /// Preset for Mistral's hosted API (`api.mistral.ai`).
    pub fn mistral(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: "mistral-large-latest".to_string(),
            base_url: MISTRAL_BASE_URL.to_string(),
            max_tokens: DEFAULT_MAX_TOKENS,
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    pub fn with_max_tokens(mut self, tokens: u32) -> Self {
        self.max_tokens = tokens;
        self
    }
}

pub struct OpenAiProvider {
    /// Display name (e.g. "mistral", "openai") — cosmetic, used in logs.
    name: &'static str,
    config: RwLock<OpenAiConfig>,
}

impl OpenAiProvider {
    pub fn new(config: OpenAiConfig) -> Arc<Self> {
        Self::named("openai", config)
    }

    pub fn mistral(config: OpenAiConfig) -> Arc<Self> {
        Self::named("mistral", config)
    }

    pub fn named(name: &'static str, config: OpenAiConfig) -> Arc<Self> {
        Arc::new(Self {
            name,
            config: RwLock::new(config),
        })
    }

    fn snapshot_config(&self) -> OpenAiConfig {
        self.config.read().expect("config lock poisoned").clone()
    }

    fn build_payload(
        &self,
        cfg: &OpenAiConfig,
        messages: &[Message],
        tools: &[ToolDefinition],
        system: &str,
    ) -> serde_json::Value {
        let mut payload = serde_json::json!({
            "model": cfg.model,
            "messages": messages_to_openai(messages, system),
            "max_tokens": cfg.max_tokens,
            "stream": true,
            // Ask for a final usage chunk on the stream.
            "stream_options": { "include_usage": true },
        });

        if !tools.is_empty() {
            let tool_defs: Vec<serde_json::Value> = tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.input_schema,
                        }
                    })
                })
                .collect();
            payload["tools"] = serde_json::json!(tool_defs);
        }

        payload
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    async fn complete(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream, ProviderError> {
        let cfg = self.snapshot_config();
        let payload = self.build_payload(&cfg, messages, tools, system);

        let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
        let api_key = cfg.api_key.clone();
        let client = shared_http_client();

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
                let payload = payload.clone();
                let client = client.clone();
                let api_key = api_key.clone();
                async move {
                    let response = client
                        .post(&url)
                        .header("content-type", "application/json")
                        .header("accept", "text/event-stream")
                        .header("authorization", format!("Bearer {api_key}"))
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
            let mut state = StreamState::default();
            while let Some(event_res) = sse.next().await {
                match event_res {
                    Ok(event) => {
                        let data = event.data.trim();
                        if data.is_empty() {
                            continue;
                        }
                        if data == "[DONE]" {
                            break;
                        }
                        match serde_json::from_str::<ChatChunk>(data) {
                            Ok(chunk) => {
                                for emitted in state.absorb(chunk) {
                                    if tx.send(Ok(emitted)).await.is_err() {
                                        return;
                                    }
                                }
                            }
                            Err(err) => {
                                let _ = tx
                                    .send(Err(ProviderError::decode(format!(
                                        "OpenAI chunk decode error: {err} — data: {}",
                                        truncate_for_log(data, 200)
                                    ))))
                                    .await;
                                return;
                            }
                        }
                    }
                    Err(err) => {
                        let _ = tx
                            .send(Err(ProviderError::decode(format!("SSE decode error: {err}"))))
                            .await;
                        return;
                    }
                }
            }
            for emitted in state.flush() {
                let _ = tx.send(Ok(emitted)).await;
            }
        });

        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn name(&self) -> &str {
        self.name
    }

    fn model(&self) -> String {
        self.snapshot_config().model
    }

    fn supports_image_input(&self) -> bool {
        true
    }

    fn set_model(&self, model: &str) -> Result<(), ProviderError> {
        self.config.write().expect("config lock poisoned").model = model.to_string();
        Ok(())
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self {
            name: self.name,
            config: RwLock::new(self.snapshot_config()),
        })
    }
}

// ─── Stream assembly ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
struct ChatChunk {
    #[serde(default)]
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize)]
struct ChatChoice {
    #[serde(default)]
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCallDelta>,
}

#[derive(Debug, Clone, Deserialize)]
struct ToolCallDelta {
    #[serde(default)]
    index: u64,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<FunctionDelta>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct FunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Usage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
}

/// Translates the OpenAI streaming delta protocol into runic `StreamEvent`s.
/// OpenAI streams a tool call as: first delta (index N) → id + name, later
/// deltas (same index) → argument fragments. A new index opens a new call.
#[derive(Default)]
struct StreamState {
    /// The tool-call index currently open, if any.
    open_tool: Option<u64>,
    finish_reason: Option<String>,
    saw_message_end: bool,
}

impl StreamState {
    fn absorb(&mut self, chunk: ChatChunk) -> Vec<StreamEvent> {
        let mut out: Vec<StreamEvent> = Vec::new();

        for choice in chunk.choices {
            if let Some(text) = choice.delta.content {
                if !text.is_empty() {
                    out.push(StreamEvent::TextDelta(text));
                }
            }

            for tc in choice.delta.tool_calls {
                // A new index → close the previous call, open this one.
                if self.open_tool != Some(tc.index) {
                    if self.open_tool.is_some() {
                        out.push(StreamEvent::ToolUseEnd);
                    }
                    let id = tc.id.clone().unwrap_or_else(|| format!("call_{}", tc.index));
                    let name = tc
                        .function
                        .as_ref()
                        .and_then(|f| f.name.clone())
                        .unwrap_or_default();
                    out.push(StreamEvent::ToolUseStart { id, name });
                    self.open_tool = Some(tc.index);
                }
                if let Some(func) = tc.function {
                    if let Some(args) = func.arguments {
                        if !args.is_empty() {
                            out.push(StreamEvent::ToolInputDelta(args));
                        }
                    }
                }
            }

            if let Some(reason) = choice.finish_reason {
                self.finish_reason = Some(reason);
            }
        }

        if let Some(usage) = chunk.usage {
            out.push(StreamEvent::TokenUsage {
                input_tokens: usage.prompt_tokens,
                output_tokens: usage.completion_tokens,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
            });
        }

        // Close out on finish_reason: end any open tool call, then MessageEnd.
        if self.finish_reason.is_some() && !self.saw_message_end {
            if self.open_tool.take().is_some() {
                out.push(StreamEvent::ToolUseEnd);
            }
            out.push(StreamEvent::MessageEnd {
                stop_reason: self.finish_reason.clone(),
            });
            self.saw_message_end = true;
        }

        out
    }

    fn flush(&mut self) -> Vec<StreamEvent> {
        if self.saw_message_end {
            return vec![];
        }
        self.saw_message_end = true;
        let mut out = Vec::new();
        if self.open_tool.take().is_some() {
            out.push(StreamEvent::ToolUseEnd);
        }
        out.push(StreamEvent::MessageEnd {
            stop_reason: self.finish_reason.clone(),
        });
        out
    }
}

// ─── Message conversion ─────────────────────────────────────────────────────

/// Convert runic messages into the OpenAI `messages` array. The system
/// prompt becomes a leading `system` message; tool results expand into
/// separate `tool` messages; assistant tool-use blocks collapse into a
/// `tool_calls` array on one assistant message.
fn messages_to_openai(messages: &[Message], system: &str) -> Vec<serde_json::Value> {
    let mut out: Vec<serde_json::Value> = Vec::new();

    if !system.trim().is_empty() {
        out.push(serde_json::json!({ "role": "system", "content": system }));
    }

    for msg in messages {
        match msg.role {
            Role::User => {
                // Tool results must each be their own `tool` message; other
                // user content (text/images) goes in a single user message.
                let mut tool_msgs: Vec<serde_json::Value> = Vec::new();
                let mut parts: Vec<serde_json::Value> = Vec::new();
                let mut plain_text = String::new();

                for block in &msg.content {
                    match block {
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } => tool_msgs.push(serde_json::json!({
                            "role": "tool",
                            "tool_call_id": tool_use_id,
                            "content": content,
                        })),
                        ContentBlock::Text { text, .. } => {
                            if !plain_text.is_empty() {
                                plain_text.push('\n');
                            }
                            plain_text.push_str(text);
                            parts.push(serde_json::json!({ "type": "text", "text": text }));
                        }
                        ContentBlock::Image { media_type, data } => {
                            parts.push(serde_json::json!({
                                "type": "image_url",
                                "image_url": { "url": format!("data:{media_type};base64,{data}") }
                            }));
                        }
                        // Reasoning isn't replayed; Blob is materialized to
                        // Image upstream before reaching the provider.
                        ContentBlock::Reasoning { .. } | ContentBlock::Blob(_) => {}
                        // Tool *use* never appears on a user message.
                        ContentBlock::ToolUse { .. } => {}
                    }
                }

                // Emit the user message first (if any content), then tool msgs.
                let has_image = parts.iter().any(|p| p["type"] == "image_url");
                if has_image {
                    out.push(serde_json::json!({ "role": "user", "content": parts }));
                } else if !plain_text.is_empty() {
                    out.push(serde_json::json!({ "role": "user", "content": plain_text }));
                }
                out.extend(tool_msgs);
            }
            Role::Assistant => {
                let mut text = String::new();
                let mut tool_calls: Vec<serde_json::Value> = Vec::new();
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text: t, .. } => {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(t);
                        }
                        ContentBlock::ToolUse { id, name, input } => {
                            tool_calls.push(serde_json::json!({
                                "id": id,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": serde_json::to_string(input).unwrap_or_else(|_| "{}".into()),
                                }
                            }));
                        }
                        _ => {}
                    }
                }
                let mut m = serde_json::json!({ "role": "assistant" });
                // `content` must be present (null is allowed alongside tool_calls).
                m["content"] = if text.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::Value::String(text)
                };
                if !tool_calls.is_empty() {
                    m["tool_calls"] = serde_json::json!(tool_calls);
                }
                out.push(m);
            }
        }
    }

    out
}

fn truncate_for_log(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mistral_preset_targets_mistral_api() {
        let cfg = OpenAiConfig::mistral("k");
        assert_eq!(cfg.base_url, MISTRAL_BASE_URL);
        assert_eq!(cfg.model, "mistral-large-latest");
    }

    #[test]
    fn system_prompt_becomes_leading_system_message() {
        let msgs = messages_to_openai(&[Message::user("hi")], "be terse");
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "be terse");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "hi");
    }

    #[test]
    fn tool_result_becomes_a_tool_role_message() {
        let msg = Message::tool_result("call_1", "42", false);
        let out = messages_to_openai(&[msg], "");
        assert_eq!(out[0]["role"], "tool");
        assert_eq!(out[0]["tool_call_id"], "call_1");
        assert_eq!(out[0]["content"], "42");
    }

    #[test]
    fn assistant_tool_use_becomes_tool_calls_array() {
        let msg = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "echo".into(),
                input: serde_json::json!({ "msg": "hi" }),
            }],
            timestamp: None,
            tool_duration_ms: None,
        };
        let out = messages_to_openai(&[msg], "");
        assert_eq!(out[0]["role"], "assistant");
        assert!(out[0]["content"].is_null());
        assert_eq!(out[0]["tool_calls"][0]["id"], "call_1");
        assert_eq!(out[0]["tool_calls"][0]["function"]["name"], "echo");
        let args = out[0]["tool_calls"][0]["function"]["arguments"].as_str().unwrap();
        assert!(args.contains("\"msg\""));
    }

    fn chunk(json: &str) -> ChatChunk {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn streams_text_deltas() {
        let mut s = StreamState::default();
        let out = s.absorb(chunk(r#"{"choices":[{"delta":{"content":"hi"}}]}"#));
        assert!(matches!(&out[0], StreamEvent::TextDelta(t) if t == "hi"));
    }

    #[test]
    fn synthesizes_tool_call_lifecycle_across_deltas() {
        let mut s = StreamState::default();
        // First delta: id + name.
        let a = s.absorb(chunk(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_x","function":{"name":"bash"}}]}}]}"#,
        ));
        assert!(matches!(&a[0], StreamEvent::ToolUseStart { id, name } if id == "call_x" && name == "bash"));
        // Later delta: argument fragment.
        let b = s.absorb(chunk(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"cmd\":"}}]}}]}"#,
        ));
        assert!(matches!(&b[0], StreamEvent::ToolInputDelta(frag) if frag.contains("cmd")));
        // finish_reason closes the call then ends the message.
        let c = s.absorb(chunk(r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#));
        assert!(matches!(&c[0], StreamEvent::ToolUseEnd));
        assert!(matches!(&c[1], StreamEvent::MessageEnd { stop_reason } if stop_reason.as_deref() == Some("tool_calls")));
    }

    #[test]
    fn emits_usage_then_message_end() {
        let mut s = StreamState::default();
        let out = s.absorb(chunk(
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#,
        ));
        let usage = out.iter().find_map(|e| match e {
            StreamEvent::TokenUsage { input_tokens, output_tokens, .. } => Some((*input_tokens, *output_tokens)),
            _ => None,
        });
        assert_eq!(usage, Some((Some(10), Some(5))));
        assert!(out.iter().any(|e| matches!(e, StreamEvent::MessageEnd { stop_reason } if stop_reason.as_deref() == Some("stop"))));
    }

    #[test]
    fn flush_synthesizes_message_end_when_stream_cuts_off() {
        let mut s = StreamState::default();
        let out = s.flush();
        assert!(matches!(&out[0], StreamEvent::MessageEnd { stop_reason: None }));
        assert!(s.flush().is_empty());
    }

    #[test]
    fn tolerates_empty_and_roleonly_chunks() {
        let mut s = StreamState::default();
        assert!(s.absorb(chunk(r#"{"choices":[{"delta":{"role":"assistant"}}]}"#)).is_empty());
        assert!(s.absorb(chunk(r#"{"choices":[]}"#)).is_empty());
    }
}
