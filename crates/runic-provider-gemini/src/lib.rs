//! Google Gemini provider (public API, API-key auth).
//!
//! Targets `https://generativelanguage.googleapis.com/v1beta` — simpler than
//! jcode's Code Assist path which goes through `cloudcode-pa.googleapis.com`
//! with OAuth. For a personal harness an API key from
//! https://aistudio.google.com/app/apikey is enough.
//!
//! Wire-type subset distilled from `jcode-provider-gemini`; the OAuth /
//! Code Assist types are intentionally omitted.
//!
//! Key shape differences from Anthropic this crate handles internally:
//! - Roles are `user` / `model` (not `assistant`).
//! - Tool calls are `functionCall` *parts* inside a content block (not their
//!   own block kind). Same for tool results (`functionResponse`).
//! - System prompt goes in a top-level `systemInstruction` field, not a message.
//! - Streaming uses SSE with `?alt=sse`; each chunk is a partial response.

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::stream::StreamExt;
use runic_message_types::{ConnectionPhase, ContentBlock, Message, Role, StreamEvent, ToolDefinition};
use runic_provider_core::{
    EventStream, Provider, ProviderError, RetryPolicy, shared_http_client, with_retry,
};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

// ─── Wire types (trimmed from jcode-provider-gemini) ────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiContent {
    pub role: String,
    pub parts: Vec<GeminiPart>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GeminiPart {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inline_data: Option<InlineData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_call: Option<GeminiFunctionCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_response: Option<GeminiFunctionResponse>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InlineData {
    pub mime_type: String,
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiFunctionCall {
    pub name: String,
    #[serde(default)]
    pub args: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiFunctionResponse {
    pub name: String,
    pub response: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GeminiTool {
    #[serde(rename = "functionDeclarations")]
    pub function_declarations: Vec<GeminiFunctionDeclaration>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GeminiFunctionDeclaration {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GeminiUsageMetadata {
    #[serde(default)]
    pub prompt_token_count: Option<u64>,
    #[serde(default)]
    pub candidates_token_count: Option<u64>,
    #[serde(default)]
    pub cached_content_token_count: Option<u64>,
    #[serde(default)]
    pub total_token_count: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiCandidate {
    #[serde(default)]
    pub content: Option<GeminiContent>,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiStreamChunk {
    #[serde(default)]
    pub candidates: Option<Vec<GeminiCandidate>>,
    #[serde(default)]
    pub usage_metadata: Option<GeminiUsageMetadata>,
    #[serde(default)]
    pub response_id: Option<String>,
}

// ─── Config + provider ──────────────────────────────────────────────────────

const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";
const DEFAULT_API_VERSION: &str = "v1beta";
const DEFAULT_MODEL: &str = "gemini-2.5-flash";
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 8192;

#[derive(Debug, Clone)]
pub struct GeminiConfig {
    pub api_key: String,
    pub model: String,
    pub base_url: String,
    pub api_version: String,
    pub max_output_tokens: u32,
    /// When set, sends `thinkingConfig.thinking_budget` so 2.5+ models can think.
    pub thinking_budget_tokens: Option<u32>,
}

impl GeminiConfig {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: DEFAULT_MODEL.to_string(),
            base_url: DEFAULT_BASE_URL.to_string(),
            api_version: DEFAULT_API_VERSION.to_string(),
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            thinking_budget_tokens: None,
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    pub fn with_max_output_tokens(mut self, tokens: u32) -> Self {
        self.max_output_tokens = tokens;
        self
    }

    pub fn with_thinking_budget(mut self, tokens: u32) -> Self {
        self.thinking_budget_tokens = Some(tokens);
        self
    }
}

pub struct GeminiProvider {
    config: RwLock<GeminiConfig>,
}

impl GeminiProvider {
    pub fn new(config: GeminiConfig) -> Arc<Self> {
        Arc::new(Self {
            config: RwLock::new(config),
        })
    }

    fn snapshot_config(&self) -> GeminiConfig {
        self.config.read().expect("config lock poisoned").clone()
    }

    fn build_payload(
        &self,
        cfg: &GeminiConfig,
        messages: &[Message],
        tools: &[ToolDefinition],
        system: &str,
    ) -> serde_json::Value {
        let mut payload = serde_json::json!({
            "contents": messages_to_gemini(messages),
            "generationConfig": {
                "maxOutputTokens": cfg.max_output_tokens,
            },
        });

        if !system.trim().is_empty() {
            payload["systemInstruction"] = serde_json::json!({
                "role": "system",
                "parts": [{ "text": system }],
            });
        }

        if !tools.is_empty() {
            let function_declarations: Vec<serde_json::Value> = tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    })
                })
                .collect();
            payload["tools"] = serde_json::json!([{ "functionDeclarations": function_declarations }]);
        }

        if let Some(budget) = cfg.thinking_budget_tokens {
            payload["generationConfig"]["thinkingConfig"] = serde_json::json!({
                "thinkingBudget": budget,
            });
        }

        payload
    }
}

#[async_trait]
impl Provider for GeminiProvider {
    async fn complete(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream, ProviderError> {
        let cfg = self.snapshot_config();
        let payload = self.build_payload(&cfg, messages, tools, system);

        let url = format!(
            "{}/{}/models/{}:streamGenerateContent?alt=sse&key={}",
            cfg.base_url.trim_end_matches('/'),
            cfg.api_version,
            cfg.model,
            cfg.api_key,
        );
        let client = shared_http_client();

        // Channel up front so retry callbacks can emit ConnectionPhase events.
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
                async move {
                    let response = client
                        .post(&url)
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
            let mut state = StreamAccum::default();
            while let Some(event_res) = sse.next().await {
                match event_res {
                    Ok(event) => {
                        if event.data.is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<GeminiStreamChunk>(&event.data) {
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
                                        "Gemini chunk decode error: {err} — data: {}",
                                        truncate_for_log(&event.data, 200)
                                    ))))
                                    .await;
                                return;
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
            // End of stream — emit MessageEnd if we never saw a finish_reason.
            for emitted in state.flush() {
                let _ = tx.send(Ok(emitted)).await;
            }
        });

        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn name(&self) -> &str {
        "gemini"
    }

    fn model(&self) -> String {
        self.snapshot_config().model
    }

    fn supports_image_input(&self) -> bool {
        true
    }

    fn available_models(&self) -> Vec<&'static str> {
        vec![
            "gemini-2.5-pro",
            "gemini-2.5-flash",
            "gemini-2.0-flash",
            "gemini-1.5-pro",
            "gemini-1.5-flash",
        ]
    }

    fn set_model(&self, model: &str) -> Result<(), ProviderError> {
        self.config.write().expect("config lock poisoned").model = model.to_string();
        Ok(())
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self {
            config: RwLock::new(self.snapshot_config()),
        })
    }
}

// ─── Stream assembly ────────────────────────────────────────────────────────

/// Tracks streaming state so we can synthesize the discrete
/// ToolUseStart/ToolInputDelta/ToolUseEnd events the agent loop expects,
/// even though Gemini delivers complete `functionCall` parts in one shot.
#[derive(Default)]
struct StreamAccum {
    emitted_session_id: bool,
    finish_reason: Option<String>,
    saw_message_end: bool,
}

impl StreamAccum {
    fn absorb(&mut self, chunk: GeminiStreamChunk) -> Vec<StreamEvent> {
        let mut out: Vec<StreamEvent> = Vec::new();

        if !self.emitted_session_id
            && let Some(id) = chunk.response_id.as_deref()
        {
            out.push(StreamEvent::SessionId(id.to_string()));
            self.emitted_session_id = true;
        }

        if let Some(candidates) = chunk.candidates {
            for candidate in candidates {
                if let Some(content) = candidate.content {
                    for part in content.parts {
                        if let Some(text) = part.text
                            && !text.is_empty()
                        {
                            out.push(StreamEvent::TextDelta(text));
                        }
                        if let Some(fc) = part.function_call {
                            // Gemini delivers full function_call in one chunk —
                            // synthesize Anthropic-style begin/delta/end.
                            let id = fc.id.unwrap_or_else(|| format!("call_{}", &fc.name));
                            let args_str = serde_json::to_string(&fc.args).unwrap_or_default();
                            out.push(StreamEvent::ToolUseStart {
                                id,
                                name: fc.name,
                            });
                            if !args_str.is_empty() && args_str != "null" {
                                out.push(StreamEvent::ToolInputDelta(args_str));
                            }
                            out.push(StreamEvent::ToolUseEnd);
                        }
                    }
                }
                if let Some(reason) = candidate.finish_reason {
                    self.finish_reason = Some(reason);
                }
            }
        }

        if let Some(usage) = chunk.usage_metadata {
            out.push(StreamEvent::TokenUsage {
                input_tokens: usage.prompt_token_count,
                output_tokens: usage.candidates_token_count,
                cache_read_input_tokens: usage.cached_content_token_count,
                cache_creation_input_tokens: None,
            });
        }

        if self.finish_reason.is_some() && !self.saw_message_end {
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
        vec![StreamEvent::MessageEnd {
            stop_reason: self.finish_reason.clone(),
        }]
    }
}

// ─── Message conversion ─────────────────────────────────────────────────────

fn messages_to_gemini(messages: &[Message]) -> Vec<serde_json::Value> {
    messages
        .iter()
        .filter_map(message_to_gemini)
        .collect()
}

fn message_to_gemini(msg: &Message) -> Option<serde_json::Value> {
    let role_str = match msg.role {
        Role::User => "user",
        Role::Assistant => "model",
    };

    let parts: Vec<serde_json::Value> =
        msg.content.iter().filter_map(content_block_to_part).collect();

    if parts.is_empty() {
        return None;
    }

    Some(serde_json::json!({ "role": role_str, "parts": parts }))
}

fn content_block_to_part(block: &ContentBlock) -> Option<serde_json::Value> {
    match block {
        ContentBlock::Text { text, .. } => Some(serde_json::json!({ "text": text })),
        ContentBlock::ToolUse { id, name, input } => Some(serde_json::json!({
            "functionCall": {
                "name": name,
                "args": input,
                "id": id,
            }
        })),
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            let mut response = serde_json::json!({ "content": content });
            if matches!(is_error, Some(true)) {
                response["error"] = serde_json::Value::Bool(true);
            }
            Some(serde_json::json!({
                "functionResponse": {
                    "name": tool_use_id,
                    "response": response,
                    "id": tool_use_id,
                }
            }))
        }
        ContentBlock::Image { media_type, data } => Some(serde_json::json!({
            "inlineData": { "mimeType": media_type, "data": data }
        })),
        // Gemini's thinking format differs and isn't required for replay.
        ContentBlock::Reasoning { .. } => None,
        // Blobs must be materialized to inline data before reaching the
        // provider — see note in the Anthropic adapter.
        ContentBlock::Blob(_) => None,
    }
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
    fn config_defaults_are_sensible() {
        let cfg = GeminiConfig::new("k");
        assert_eq!(cfg.model, DEFAULT_MODEL);
        assert_eq!(cfg.max_output_tokens, DEFAULT_MAX_OUTPUT_TOKENS);
    }

    #[test]
    fn user_message_serializes_with_user_role() {
        let msg = Message::user("hello");
        let json = message_to_gemini(&msg).unwrap();
        assert_eq!(json["role"], "user");
        assert_eq!(json["parts"][0]["text"], "hello");
    }

    #[test]
    fn assistant_message_serializes_with_model_role() {
        let msg = Message::assistant_text("hi");
        let json = message_to_gemini(&msg).unwrap();
        assert_eq!(json["role"], "model");
    }

    #[test]
    fn tool_result_serializes_as_function_response_part() {
        let msg = Message::tool_result("call_42", "ok", false);
        let json = message_to_gemini(&msg).unwrap();
        assert_eq!(json["role"], "user");
        assert_eq!(json["parts"][0]["functionResponse"]["name"], "call_42");
        assert_eq!(json["parts"][0]["functionResponse"]["response"]["content"], "ok");
    }

    #[test]
    fn stream_accum_emits_text_delta() {
        let mut s = StreamAccum::default();
        let chunk: GeminiStreamChunk = serde_json::from_str(
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"hi"}]}}]}"#,
        )
        .unwrap();
        let out = s.absorb(chunk);
        assert!(matches!(&out[0], StreamEvent::TextDelta(t) if t == "hi"));
    }

    #[test]
    fn stream_accum_synthesizes_tool_use_lifecycle() {
        let mut s = StreamAccum::default();
        let chunk: GeminiStreamChunk = serde_json::from_str(
            r#"{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"echo","args":{"msg":"hi"}}}]}}]}"#,
        )
        .unwrap();
        let out = s.absorb(chunk);
        assert!(matches!(&out[0], StreamEvent::ToolUseStart { name, .. } if name == "echo"));
        assert!(matches!(&out[1], StreamEvent::ToolInputDelta(_)));
        assert!(matches!(&out[2], StreamEvent::ToolUseEnd));
    }

    #[test]
    fn stream_accum_emits_message_end_on_finish_reason() {
        let mut s = StreamAccum::default();
        let chunk: GeminiStreamChunk =
            serde_json::from_str(r#"{"candidates":[{"finishReason":"STOP"}]}"#).unwrap();
        let out = s.absorb(chunk);
        let saw_end = out
            .iter()
            .any(|e| matches!(e, StreamEvent::MessageEnd { stop_reason } if stop_reason.as_deref() == Some("STOP")));
        assert!(saw_end);
    }

    #[test]
    fn stream_accum_emits_token_usage() {
        let mut s = StreamAccum::default();
        let chunk: GeminiStreamChunk = serde_json::from_str(
            r#"{"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"totalTokenCount":15}}"#,
        )
        .unwrap();
        let out = s.absorb(chunk);
        let usage = out
            .iter()
            .find_map(|e| match e {
                StreamEvent::TokenUsage {
                    input_tokens,
                    output_tokens,
                    ..
                } => Some((*input_tokens, *output_tokens)),
                _ => None,
            })
            .unwrap();
        assert_eq!(usage, (Some(10), Some(5)));
    }

    #[test]
    fn stream_accum_emits_session_id_only_once() {
        let mut s = StreamAccum::default();
        let chunk = || -> GeminiStreamChunk {
            serde_json::from_str(r#"{"responseId":"resp-1"}"#).unwrap()
        };
        let first = s.absorb(chunk());
        assert!(matches!(&first[0], StreamEvent::SessionId(id) if id == "resp-1"));
        assert!(
            s.absorb(chunk()).is_empty(),
            "session id must not repeat on later chunks"
        );
    }

    #[test]
    fn stream_accum_synthesizes_call_id_when_gemini_omits_it() {
        // The public API often omits functionCall.id — the accumulator must
        // still produce a usable id (the agent loop keys tool results on it).
        let mut s = StreamAccum::default();
        let chunk: GeminiStreamChunk = serde_json::from_str(
            r#"{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"echo","args":{}}}]}}]}"#,
        )
        .unwrap();
        let out = s.absorb(chunk);
        match &out[0] {
            StreamEvent::ToolUseStart { id, .. } => assert_eq!(id, "call_echo"),
            other => panic!("expected ToolUseStart, got {other:?}"),
        }
    }

    #[test]
    fn stream_accum_flush_synthesizes_message_end_when_stream_just_stops() {
        // Gemini sometimes ends the SSE stream without ever sending a
        // finishReason — flush() must still close the message.
        let mut s = StreamAccum::default();
        let out = s.flush();
        assert!(
            matches!(&out[0], StreamEvent::MessageEnd { stop_reason: None }),
            "got {out:?}"
        );
        assert!(s.flush().is_empty(), "flush must be idempotent");
    }

    #[test]
    fn stream_accum_flush_is_noop_after_real_message_end() {
        let mut s = StreamAccum::default();
        let chunk: GeminiStreamChunk =
            serde_json::from_str(r#"{"candidates":[{"finishReason":"STOP"}]}"#).unwrap();
        s.absorb(chunk);
        assert!(s.flush().is_empty(), "MessageEnd already emitted via finishReason");
    }

    #[test]
    fn stream_accum_tolerates_empty_and_partial_chunks() {
        // Keep-alive / metadata-only chunks with no candidates, empty parts,
        // or empty text must produce no events and no panic.
        let mut s = StreamAccum::default();
        for raw in [
            r#"{}"#,
            r#"{"candidates":[]}"#,
            r#"{"candidates":[{}]}"#,
            r#"{"candidates":[{"content":{"role":"model","parts":[]}}]}"#,
            r#"{"candidates":[{"content":{"role":"model","parts":[{"text":""}]}}]}"#,
        ] {
            let chunk: GeminiStreamChunk = serde_json::from_str(raw).unwrap();
            assert!(s.absorb(chunk).is_empty(), "chunk {raw} should emit nothing");
        }
    }

    #[test]
    fn reasoning_and_blob_blocks_are_dropped_from_gemini_payloads() {
        // Reasoning isn't replayed to Gemini; Blob must be materialized
        // upstream. A message containing ONLY such blocks serializes to
        // nothing at all (Gemini rejects empty parts arrays).
        let msg = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Reasoning { text: "private".into() }],
            timestamp: None,
            tool_duration_ms: None,
        };
        assert!(message_to_gemini(&msg).is_none());
    }
}
