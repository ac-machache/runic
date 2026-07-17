use crate::think_filter::{FilterAction, StreamingThinkFilter};
use crate::{CompletionRequest, CompletionResponse, Provider, ProviderError, StreamEvent};
use async_trait::async_trait;
use futures::StreamExt;
use runic_types::{ContentBlock, MessageContent, Role, StopReason, TokenUsage, ToolCall};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};
use zeroize::Zeroizing;

const DEFAULT_BASE_URL: &str = "https://api.mistral.ai/v1";
const USER_AGENT: &str = "runic/0.1.0";
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
const MAX_DOCUMENT_BYTES: usize = 50 * 1024 * 1024;
const MAX_IMAGES_PER_REQUEST: usize = 8;

pub struct MistralDriver {
    api_key: Zeroizing<String>,
    base_url: String,
    client: reqwest::Client,
    reasoning_effort: Option<String>,
}

impl MistralDriver {
    pub fn new(api_key: String) -> Self {
        Self::with_base_url(api_key, DEFAULT_BASE_URL.to_string())
    }

    pub fn with_base_url(api_key: String, base_url: String) -> Self {
        Self {
            api_key: Zeroizing::new(api_key),
            base_url,
            client: reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .connect_timeout(CONNECT_TIMEOUT)
                .read_timeout(READ_TIMEOUT)
                .build()
                .unwrap_or_default(),
            reasoning_effort: None,
        }
    }

    pub fn with_reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        self.reasoning_effort = Some(effort.into());
        self
    }

    fn chat_url(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }
}

fn retry_after_ms(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get("retry-after")?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(|secs| secs.saturating_mul(1000))
}

fn http_error(status: u16, retry_after: Option<u64>, message: String) -> ProviderError {
    match status {
        429 => ProviderError::RateLimited {
            retry_after_ms: retry_after.unwrap_or(5000),
        },
        401 | 403 => ProviderError::AuthenticationFailed(message),
        s if s >= 500 => ProviderError::Overloaded {
            retry_after_ms: retry_after.unwrap_or(2000),
        },
        _ => ProviderError::Api { status, message },
    }
}

fn base64_len_bytes(data: &str) -> usize {
    data.len() / 4 * 3
}

fn validate_media(request: &CompletionRequest) -> Result<(), ProviderError> {
    let mut images = 0usize;
    for msg in &request.messages {
        let MessageContent::Blocks(blocks) = &msg.content else {
            continue;
        };
        for block in blocks {
            match block {
                ContentBlock::Image { data, .. } => {
                    images += 1;
                    if base64_len_bytes(data) > MAX_IMAGE_BYTES {
                        return Err(ProviderError::Api {
                            status: 413,
                            message: format!(
                                "image exceeds Mistral's {} MB limit",
                                MAX_IMAGE_BYTES / (1024 * 1024)
                            ),
                        });
                    }
                }
                ContentBlock::File { data, .. } => {
                    if base64_len_bytes(data) > MAX_DOCUMENT_BYTES {
                        return Err(ProviderError::Api {
                            status: 413,
                            message: format!(
                                "document exceeds Mistral's {} MB limit",
                                MAX_DOCUMENT_BYTES / (1024 * 1024)
                            ),
                        });
                    }
                }
                _ => {}
            }
        }
    }
    if images > MAX_IMAGES_PER_REQUEST {
        return Err(ProviderError::Api {
            status: 413,
            message: format!(
                "request has {images} images; Mistral allows {MAX_IMAGES_PER_REQUEST}"
            ),
        });
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct MistralRequest {
    model: String,
    messages: Vec<MistralMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<MistralTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_mode: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
}

#[derive(Debug, Serialize)]
struct MistralMessage {
    role: &'static str,
    content: MistralContent,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<MistralOutToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum MistralContent {
    Text(String),
    Chunks(Vec<MistralChunk>),
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum MistralChunk {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: String },
    #[serde(rename = "document_url")]
    DocumentUrl {
        document_url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        document_name: Option<String>,
    },
    #[serde(rename = "thinking")]
    Thinking { thinking: Vec<MistralThinkPart> },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum MistralThinkPart {
    #[serde(rename = "text")]
    Text { text: String },
}

#[derive(Debug, Serialize)]
struct MistralOutToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: &'static str,
    function: MistralOutFunction,
}

#[derive(Debug, Serialize)]
struct MistralOutFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, Serialize)]
struct MistralTool {
    #[serde(rename = "type")]
    tool_type: &'static str,
    function: MistralToolDef,
}

#[derive(Debug, Serialize)]
struct MistralToolDef {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct MistralResponse {
    choices: Vec<MistralChoice>,
    usage: Option<MistralUsage>,
}

#[derive(Debug, Deserialize)]
struct MistralChoice {
    message: MistralResponseMessage,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MistralResponseMessage {
    content: Option<serde_json::Value>,
    tool_calls: Option<Vec<MistralInToolCall>>,
}

#[derive(Debug, Deserialize)]
struct MistralInToolCall {
    id: String,
    function: MistralInFunction,
}

#[derive(Debug, Deserialize)]
struct MistralInFunction {
    name: String,
    arguments: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct MistralUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<MistralPromptTokensDetails>,
}

#[derive(Debug, Deserialize)]
struct MistralPromptTokensDetails {
    #[serde(default)]
    cached_tokens: u64,
}

fn tool_result_text(content: &runic_types::ToolResultPayload, is_error: bool) -> String {
    let text = content.text();
    let text = if text.is_empty() {
        "(empty)".to_string()
    } else {
        text
    };
    if is_error {
        format!("Error: {text}")
    } else {
        text
    }
}

fn wire_call_id(id: &str) -> String {
    if id.len() == 9 && id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return id.to_string();
    }
    let mut h: u64 = 0xcbf29ce484222325;
    for b in id.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    let alphabet = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut out = String::with_capacity(9);
    for _ in 0..9 {
        out.push(alphabet[(h % 36) as usize] as char);
        h /= 36;
    }
    out
}

fn parse_arguments(arguments: &serde_json::Value) -> serde_json::Value {
    match arguments {
        serde_json::Value::String(s) => {
            serde_json::from_str(s).unwrap_or_else(|_| serde_json::json!({}))
        }
        serde_json::Value::Object(_) => arguments.clone(),
        _ => serde_json::json!({}),
    }
}

fn split_think(text: &str) -> (String, Option<String>) {
    let mut filter = StreamingThinkFilter::new();
    let mut visible = String::new();
    let mut thinking = String::new();
    for action in filter.process(text).into_iter().chain(filter.flush()) {
        match action {
            FilterAction::EmitText(t) => visible.push_str(&t),
            FilterAction::EmitThinking(t) => thinking.push_str(&t),
        }
    }
    let visible = visible.trim().to_string();
    let thinking = thinking.trim().to_string();
    (
        visible,
        if thinking.is_empty() {
            None
        } else {
            Some(thinking)
        },
    )
}

fn assemble_assistant(blocks: &[ContentBlock]) -> MistralMessage {
    let mut text = String::new();
    let mut thinking = String::new();
    let mut tool_calls: Vec<MistralOutToolCall> = Vec::new();

    for block in blocks {
        match block {
            ContentBlock::Text { text: t, .. } => text.push_str(t),
            ContentBlock::Thinking { thinking: t, .. } if !t.is_empty() => thinking.push_str(t),
            ContentBlock::ToolUse {
                id, name, input, ..
            } => {
                tool_calls.push(MistralOutToolCall {
                    id: wire_call_id(id),
                    call_type: "function",
                    function: MistralOutFunction {
                        name: name.clone(),
                        arguments: serde_json::to_string(input).unwrap_or_default(),
                    },
                });
            }
            _ => {}
        }
    }

    let content = if thinking.is_empty() {
        MistralContent::Text(text)
    } else {
        let mut chunks = vec![MistralChunk::Thinking {
            thinking: vec![MistralThinkPart::Text { text: thinking }],
        }];
        if !text.is_empty() {
            chunks.push(MistralChunk::Text { text });
        }
        MistralContent::Chunks(chunks)
    };

    MistralMessage {
        role: "assistant",
        content,
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
        tool_call_id: None,
        name: None,
    }
}

fn build_messages(request: &CompletionRequest) -> Vec<MistralMessage> {
    let mut messages: Vec<MistralMessage> = Vec::new();

    if let Some(ref system) = request.system {
        messages.push(MistralMessage {
            role: "system",
            content: MistralContent::Text(system.clone()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        });
    }

    for msg in &request.messages {
        match (&msg.role, &msg.content) {
            (Role::System, MessageContent::Text(text)) if request.system.is_none() => {
                messages.push(MistralMessage {
                    role: "system",
                    content: MistralContent::Text(text.clone()),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                });
            }
            (Role::User, MessageContent::Text(text)) => {
                messages.push(MistralMessage {
                    role: "user",
                    content: MistralContent::Text(text.clone()),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                });
            }
            (Role::Assistant, MessageContent::Text(text)) => {
                messages.push(MistralMessage {
                    role: "assistant",
                    content: MistralContent::Text(text.clone()),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                });
            }
            (Role::User, MessageContent::Blocks(blocks)) => {
                let mut parts: Vec<MistralChunk> = Vec::new();
                for block in blocks {
                    match block {
                        ContentBlock::ToolResult {
                            tool_use_id,
                            tool_name,
                            content,
                            is_error,
                            ..
                        } => {
                            let text = tool_result_text(content, *is_error);
                            messages.push(MistralMessage {
                                role: "tool",
                                content: MistralContent::Text(text),
                                tool_calls: None,
                                tool_call_id: Some(wire_call_id(tool_use_id)),
                                name: if tool_name.is_empty() {
                                    None
                                } else {
                                    Some(tool_name.clone())
                                },
                            });
                        }
                        ContentBlock::Text { text, .. } => {
                            parts.push(MistralChunk::Text { text: text.clone() });
                        }
                        ContentBlock::Image { media_type, data } => {
                            parts.push(MistralChunk::ImageUrl {
                                image_url: format!("data:{media_type};base64,{data}"),
                            });
                        }
                        ContentBlock::File { media_type, data } => {
                            parts.push(MistralChunk::DocumentUrl {
                                document_url: format!("data:{media_type};base64,{data}"),
                                document_name: None,
                            });
                        }
                        ContentBlock::ArtifactRef { id, .. } => {
                            warn!(artifact = %id, "unresolved ArtifactRef reached Mistral — dropping");
                        }
                        _ => {}
                    }
                }
                if !parts.is_empty() {
                    messages.push(MistralMessage {
                        role: "user",
                        content: MistralContent::Chunks(parts),
                        tool_calls: None,
                        tool_call_id: None,
                        name: None,
                    });
                }
            }
            (Role::Assistant, MessageContent::Blocks(blocks)) => {
                messages.push(assemble_assistant(blocks));
            }
            _ => {}
        }
    }

    messages
}

fn build_request(
    request: &CompletionRequest,
    stream: bool,
    driver_effort: Option<&str>,
) -> MistralRequest {
    let tools: Vec<MistralTool> = request
        .tools
        .iter()
        .map(|t| MistralTool {
            tool_type: "function",
            function: MistralToolDef {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: runic_types::normalize_schema_for_provider(&t.input_schema, "mistral"),
            },
        })
        .collect();
    let tool_choice = if tools.is_empty() { None } else { Some("auto") };

    MistralRequest {
        model: request.model.clone(),
        messages: build_messages(request),
        max_tokens: Some(request.max_tokens),
        temperature: Some(request.temperature),
        tools,
        tool_choice,
        prompt_mode: match &request.thinking {
            Some(t) if t.enabled => Some("reasoning"),
            _ => None,
        },
        reasoning_effort: if request.model.starts_with("magistral") {
            None
        } else {
            match &request.thinking {
                Some(t) if !t.enabled => Some("none".to_string()),
                _ => driver_effort.map(str::to_string),
            }
        },
        stream,
    }
}

fn content_blocks_from_value(value: &serde_json::Value) -> (Vec<ContentBlock>, String) {
    let mut blocks = Vec::new();
    let mut text_total = String::new();

    let push_text = |blocks: &mut Vec<ContentBlock>, text_total: &mut String, raw: &str| {
        if raw.is_empty() {
            return;
        }
        let (visible, thinking) = split_think(raw);
        if let Some(t) = thinking {
            blocks.push(ContentBlock::Thinking {
                thinking: t,
                signature: None,
                provider_metadata: Some(serde_json::json!({ "format": "mistral" })),
            });
        }
        if !visible.is_empty() {
            text_total.push_str(&visible);
            blocks.push(ContentBlock::Text {
                text: visible,
                provider_metadata: None,
            });
        }
    };

    match value {
        serde_json::Value::String(s) => push_text(&mut blocks, &mut text_total, s),
        serde_json::Value::Array(chunks) => {
            for chunk in chunks {
                match chunk.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(t) = chunk.get("text").and_then(|t| t.as_str()) {
                            push_text(&mut blocks, &mut text_total, t);
                        }
                    }
                    Some("thinking") => {
                        let joined = chunk
                            .get("thinking")
                            .and_then(|t| t.as_array())
                            .map(|parts| {
                                parts
                                    .iter()
                                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                                    .collect::<Vec<_>>()
                                    .join("")
                            })
                            .unwrap_or_default();
                        if !joined.is_empty() {
                            blocks.push(ContentBlock::Thinking {
                                thinking: joined,
                                signature: None,
                                provider_metadata: Some(serde_json::json!({ "format": "mistral" })),
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }

    (blocks, text_total)
}

fn map_stop_reason(finish_reason: Option<&str>, has_tool_calls: bool) -> StopReason {
    match finish_reason {
        Some("stop") => StopReason::EndTurn,
        Some("tool_calls") => StopReason::ToolUse,
        Some("length") | Some("model_length") => StopReason::MaxTokens,
        _ => {
            if has_tool_calls {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            }
        }
    }
}

fn parse_response(response: MistralResponse) -> Result<CompletionResponse, ProviderError> {
    let choice = response
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| ProviderError::Parse("no choices in response".to_string()))?;

    let mut content = Vec::new();
    if let Some(value) = &choice.message.content {
        let (blocks, _) = content_blocks_from_value(value);
        content.extend(blocks);
    }

    let mut tool_calls = Vec::new();
    if let Some(calls) = choice.message.tool_calls {
        for call in calls {
            let input = parse_arguments(&call.function.arguments);
            content.push(ContentBlock::ToolUse {
                id: call.id.clone(),
                name: call.function.name.clone(),
                input: input.clone(),
                provider_metadata: None,
            });
            tool_calls.push(ToolCall {
                id: call.id,
                name: call.function.name,
                input,
            });
        }
    }

    let stop_reason = map_stop_reason(choice.finish_reason.as_deref(), !tool_calls.is_empty());
    let mut usage = response
        .usage
        .map(|u| TokenUsage {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
            cache_read_tokens: u
                .prompt_tokens_details
                .as_ref()
                .map(|d| d.cached_tokens)
                .unwrap_or_default(),
            cache_write_tokens: 0,
        })
        .unwrap_or_default();
    if !content.is_empty() && usage.input_tokens == 0 && usage.output_tokens == 0 {
        usage.output_tokens = 1;
    }

    Ok(CompletionResponse {
        content,
        stop_reason,
        tool_calls,
        usage,
    })
}

struct StreamAccumulator {
    text_content: String,
    thinking_content: String,
    think_filter: StreamingThinkFilter,
    tool_accum: Vec<(String, String, String)>,
    finish_reason: Option<String>,
    usage: TokenUsage,
    events: Vec<StreamEvent>,
}

impl StreamAccumulator {
    fn new() -> Self {
        Self {
            text_content: String::new(),
            thinking_content: String::new(),
            think_filter: StreamingThinkFilter::new(),
            tool_accum: Vec::new(),
            finish_reason: None,
            usage: TokenUsage::default(),
            events: Vec::new(),
        }
    }

    fn drain_events(&mut self) -> Vec<StreamEvent> {
        std::mem::take(&mut self.events)
    }

    fn handle_line(&mut self, line: &str) -> Result<bool, ProviderError> {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with(':') {
            return Ok(false);
        }
        let Some(data) = line.strip_prefix("data:") else {
            return Ok(false);
        };
        let data = data.trim_start();
        if data == "[DONE]" {
            return Ok(true);
        }
        let json: serde_json::Value = match serde_json::from_str(data) {
            Ok(value) => value,
            Err(_) => {
                warn!(chunk = %data, "unparseable Mistral stream chunk");
                return Ok(false);
            }
        };
        if json["object"].as_str() == Some("error") || json.get("error").is_some() {
            return Err(ProviderError::Api {
                status: 200,
                message: data.to_string(),
            });
        }

        if let Some(u) = json.get("usage") {
            if let Some(pt) = u["prompt_tokens"].as_u64() {
                self.usage.input_tokens = pt;
            }
            if let Some(ct) = u["completion_tokens"].as_u64() {
                self.usage.output_tokens = ct;
            }
            if let Some(cr) = u["prompt_tokens_details"]["cached_tokens"].as_u64() {
                self.usage.cache_read_tokens = cr;
            }
        }

        let Some(choices) = json["choices"].as_array() else {
            return Ok(false);
        };
        for choice in choices {
            let delta = &choice["delta"];

            match &delta["content"] {
                serde_json::Value::String(text) if !text.is_empty() => {
                    self.text_content.push_str(text);
                    let actions = self.think_filter.process(text);
                    for action in actions {
                        match action {
                            FilterAction::EmitText(t) => {
                                self.events.push(StreamEvent::TextDelta { text: t });
                            }
                            FilterAction::EmitThinking(t) => {
                                self.thinking_content.push_str(&t);
                                self.events.push(StreamEvent::ThinkingDelta { text: t });
                            }
                        }
                    }
                }
                serde_json::Value::Array(chunks) => {
                    for part in chunks {
                        match part.get("type").and_then(|t| t.as_str()) {
                            Some("text") => {
                                if let Some(t) = part.get("text").and_then(|t| t.as_str())
                                    && !t.is_empty()
                                {
                                    self.text_content.push_str(t);
                                    self.events.push(StreamEvent::TextDelta {
                                        text: t.to_string(),
                                    });
                                }
                            }
                            Some("thinking") => {
                                let joined = part
                                    .get("thinking")
                                    .and_then(|t| t.as_array())
                                    .map(|parts| {
                                        parts
                                            .iter()
                                            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                                            .collect::<Vec<_>>()
                                            .join("")
                                    })
                                    .unwrap_or_default();
                                if !joined.is_empty() {
                                    self.thinking_content.push_str(&joined);
                                    self.events
                                        .push(StreamEvent::ThinkingDelta { text: joined });
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }

            if let Some(calls) = delta["tool_calls"].as_array() {
                for call in calls {
                    let idx = call["index"].as_u64().unwrap_or(0) as usize;
                    while self.tool_accum.len() <= idx {
                        self.tool_accum
                            .push((String::new(), String::new(), String::new()));
                    }
                    if let Some(id) = call["id"].as_str()
                        && !id.is_empty()
                    {
                        self.tool_accum[idx].0 = id.to_string();
                    }
                    if let Some(func) = call.get("function") {
                        if let Some(name) = func["name"].as_str() {
                            self.tool_accum[idx].1 = name.to_string();
                            self.events.push(StreamEvent::ToolUseStart {
                                id: self.tool_accum[idx].0.clone(),
                                name: name.to_string(),
                            });
                        }
                        match &func["arguments"] {
                            serde_json::Value::String(args) => {
                                self.tool_accum[idx].2.push_str(args);
                                if !args.is_empty() {
                                    self.events
                                        .push(StreamEvent::ToolInputDelta { text: args.clone() });
                                }
                            }
                            serde_json::Value::Object(_) => {
                                let args = func["arguments"].to_string();
                                self.tool_accum[idx].2 = args.clone();
                                self.events.push(StreamEvent::ToolInputDelta { text: args });
                            }
                            _ => {}
                        }
                    }
                }
            }

            if let Some(fr) = choice["finish_reason"].as_str() {
                self.finish_reason = Some(fr.to_string());
            }
        }
        Ok(false)
    }

    fn finish(mut self) -> (CompletionResponse, Vec<StreamEvent>) {
        let actions = self.think_filter.flush();
        for action in actions {
            match action {
                FilterAction::EmitText(t) => {
                    self.events.push(StreamEvent::TextDelta { text: t });
                }
                FilterAction::EmitThinking(t) => {
                    self.thinking_content.push_str(&t);
                    self.events.push(StreamEvent::ThinkingDelta { text: t });
                }
            }
        }

        let mut content = Vec::new();
        if !self.thinking_content.is_empty() {
            content.push(ContentBlock::Thinking {
                thinking: self.thinking_content.clone(),
                signature: None,
                provider_metadata: Some(serde_json::json!({ "format": "mistral" })),
            });
        }
        if !self.text_content.is_empty() {
            let (visible, _) = split_think(&self.text_content);
            if !visible.is_empty() {
                content.push(ContentBlock::Text {
                    text: visible,
                    provider_metadata: None,
                });
            }
        }

        let mut tool_calls = Vec::new();
        for (id, name, arguments) in &self.tool_accum {
            if id.is_empty() || name.is_empty() {
                warn!(tool_id = %id, tool_name = %name, "skipping malformed streamed tool call");
                continue;
            }
            let input: serde_json::Value =
                serde_json::from_str(arguments).unwrap_or_else(|_| serde_json::json!({}));
            content.push(ContentBlock::ToolUse {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
                provider_metadata: None,
            });
            tool_calls.push(ToolCall {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            });
            self.events.push(StreamEvent::ToolUseEnd {
                id: id.clone(),
                name: name.clone(),
                input,
            });
        }

        let stop_reason = map_stop_reason(self.finish_reason.as_deref(), !tool_calls.is_empty());
        let mut usage = self.usage;
        if !content.is_empty() && usage.input_tokens == 0 && usage.output_tokens == 0 {
            usage.output_tokens = 1;
        }
        self.events
            .push(StreamEvent::ContentComplete { stop_reason, usage });

        (
            CompletionResponse {
                content,
                stop_reason,
                tool_calls,
                usage,
            },
            self.events,
        )
    }
}

#[async_trait]
impl Provider for MistralDriver {
    fn name(&self) -> &str {
        "mistral"
    }

    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        validate_media(&request)?;
        let body = build_request(&request, false, self.reasoning_effort.as_deref());
        let url = self.chat_url();
        debug!(url = %url, "sending Mistral request");
        let resp = self
            .client
            .post(&url)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", self.api_key.as_str()))
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Http(e.to_string()))?;

        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            let retry_after = retry_after_ms(resp.headers());
            let message = resp.text().await.unwrap_or_default();
            return Err(http_error(status, retry_after, message));
        }

        let text = resp
            .text()
            .await
            .map_err(|e| ProviderError::Http(e.to_string()))?;
        let parsed: MistralResponse =
            serde_json::from_str(&text).map_err(|e| ProviderError::Parse(e.to_string()))?;
        parse_response(parsed)
    }

    async fn stream(
        &self,
        request: CompletionRequest,
        tx: tokio::sync::mpsc::Sender<StreamEvent>,
    ) -> Result<CompletionResponse, ProviderError> {
        validate_media(&request)?;
        let body = build_request(&request, true, self.reasoning_effort.as_deref());
        let url = self.chat_url();
        debug!(url = %url, "sending Mistral stream request");
        let resp = self
            .client
            .post(&url)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .header("authorization", format!("Bearer {}", self.api_key.as_str()))
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Http(e.to_string()))?;

        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            let retry_after = retry_after_ms(resp.headers());
            let message = resp.text().await.unwrap_or_default();
            return Err(http_error(status, retry_after, message));
        }

        let mut accum = StreamAccumulator::new();
        let mut buffer = String::new();
        let mut byte_stream = resp.bytes_stream();
        'read: while let Some(chunk_result) = byte_stream.next().await {
            let chunk = chunk_result.map_err(|e| ProviderError::Http(e.to_string()))?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(pos) = buffer.find('\n') {
                let line = buffer[..pos].to_string();
                buffer = buffer[pos + 1..].to_string();
                if accum.handle_line(&line)? {
                    break 'read;
                }
            }
            for event in accum.drain_events() {
                let _ = tx.send(event).await;
            }
        }

        let (response, events) = accum.finish();
        for event in events {
            let _ = tx.send(event).await;
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runic_types::{Message, ToolDefinition};

    fn request_with(messages: Vec<Message>) -> CompletionRequest {
        CompletionRequest {
            model: "mistral-large-latest".to_string(),
            messages,
            tools: vec![],
            max_tokens: 1024,
            temperature: 0.7,
            system: Some("be brief".to_string()),
            thinking: None,
        }
    }

    fn user_blocks(blocks: Vec<ContentBlock>) -> Message {
        Message::user_with_blocks(blocks)
    }

    #[test]
    fn a_pdf_file_becomes_a_document_url_data_uri() {
        let req = request_with(vec![user_blocks(vec![
            ContentBlock::Text {
                text: "what is this".into(),
                provider_metadata: None,
            },
            ContentBlock::File {
                media_type: "application/pdf".into(),
                data: "JVBERi0x".into(),
            },
        ])]);
        let messages = build_messages(&req);
        let v = serde_json::to_value(&messages[1]).unwrap();
        assert_eq!(v["content"][0]["type"], "text");
        assert_eq!(v["content"][1]["type"], "document_url");
        assert_eq!(
            v["content"][1]["document_url"],
            "data:application/pdf;base64,JVBERi0x"
        );
        assert!(v["content"][1].get("file").is_none());
    }

    #[test]
    fn an_image_becomes_an_image_url_data_uri() {
        let req = request_with(vec![user_blocks(vec![ContentBlock::Image {
            media_type: "image/png".into(),
            data: "aWc=".into(),
        }])]);
        let messages = build_messages(&req);
        let v = serde_json::to_value(&messages[1]).unwrap();
        assert_eq!(v["content"][0]["type"], "image_url");
        assert_eq!(v["content"][0]["image_url"], "data:image/png;base64,aWc=");
    }

    #[test]
    fn tool_results_become_tool_messages_with_wire_ids() {
        let req = request_with(vec![user_blocks(vec![ContentBlock::ToolResult {
            tool_use_id: "abc123def".into(),
            tool_name: "search".into(),
            content: "found it".into(),
            is_error: false,
            provenance: Vec::new(),
        }])]);
        let messages = build_messages(&req);
        let v = serde_json::to_value(&messages[1]).unwrap();
        assert_eq!(v["role"], "tool");
        assert_eq!(v["tool_call_id"], "abc123def");
        assert_eq!(v["name"], "search");
        assert_eq!(v["content"], "found it");
    }

    #[test]
    fn call_ids_are_sanitized_to_nine_alphanumerics_deterministically() {
        let a = wire_call_id("call-1");
        let b = wire_call_id("call-1");
        assert_eq!(a, b);
        assert_eq!(a.len(), 9);
        assert!(a.bytes().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(wire_call_id("call-2"), a);
        assert_eq!(wire_call_id("abc123XYZ"), "abc123XYZ");
    }

    #[test]
    fn tool_use_and_tool_result_ids_stay_paired_after_sanitization() {
        let req = request_with(vec![
            Message::assistant_with_blocks(vec![ContentBlock::ToolUse {
                id: "call-1".into(),
                name: "search".into(),
                input: serde_json::json!({"q": "x"}),
                provider_metadata: None,
            }]),
            user_blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "call-1".into(),
                tool_name: "search".into(),
                content: "ok".into(),
                is_error: false,
                provenance: Vec::new(),
            }]),
        ]);
        let messages = build_messages(&req);
        let assistant = serde_json::to_value(&messages[1]).unwrap();
        let tool = serde_json::to_value(&messages[2]).unwrap();
        assert_eq!(assistant["tool_calls"][0]["id"], tool["tool_call_id"]);
    }

    #[test]
    fn assistant_tool_calls_serialize_arguments_as_a_json_string() {
        let req = request_with(vec![Message::assistant_with_blocks(vec![
            ContentBlock::ToolUse {
                id: "abc123def".into(),
                name: "calc".into(),
                input: serde_json::json!({"expr": "1+1"}),
                provider_metadata: None,
            },
        ])]);
        let messages = build_messages(&req);
        let v = serde_json::to_value(&messages[1]).unwrap();
        assert_eq!(v["tool_calls"][0]["type"], "function");
        assert_eq!(v["tool_calls"][0]["function"]["name"], "calc");
        assert!(v["tool_calls"][0]["function"]["arguments"].is_string());
        assert_eq!(v["content"], "");
    }

    #[test]
    fn assistant_thinking_replays_as_a_thinking_chunk() {
        let req = request_with(vec![Message::assistant_with_blocks(vec![
            ContentBlock::Thinking {
                thinking: "pondering".into(),
                signature: None,
                provider_metadata: Some(serde_json::json!({ "format": "mistral" })),
            },
            ContentBlock::Text {
                text: "answer".into(),
                provider_metadata: None,
            },
        ])]);
        let messages = build_messages(&req);
        let v = serde_json::to_value(&messages[1]).unwrap();
        assert_eq!(v["content"][0]["type"], "thinking");
        assert_eq!(v["content"][0]["thinking"][0]["text"], "pondering");
        assert_eq!(v["content"][1]["type"], "text");
        assert_eq!(v["content"][1]["text"], "answer");
    }

    #[test]
    fn tools_serialize_as_functions_with_auto_choice() {
        let mut req = request_with(vec![Message::user("hi")]);
        req.tools = vec![ToolDefinition {
            name: "search".into(),
            description: "find things".into(),
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
        }];
        let body = build_request(&req, false, None);
        let v = serde_json::to_value(&body).unwrap();
        assert_eq!(v["tools"][0]["type"], "function");
        assert_eq!(v["tools"][0]["function"]["name"], "search");
        assert_eq!(v["tool_choice"], "auto");
        assert!(v.get("stream").is_none());
    }

    #[test]
    fn response_arguments_accept_string_and_object() {
        let from_string = parse_arguments(&serde_json::json!("{\"a\":1}"));
        assert_eq!(from_string["a"], 1);
        let from_object = parse_arguments(&serde_json::json!({"a": 2}));
        assert_eq!(from_object["a"], 2);
        let from_garbage = parse_arguments(&serde_json::json!("not json"));
        assert_eq!(from_garbage, serde_json::json!({}));
    }

    #[test]
    fn response_with_thinking_chunks_parses_to_blocks() {
        let response: MistralResponse = serde_json::from_str(
            r#"{
                "choices": [{
                    "message": {
                        "content": [
                            {"type": "thinking", "thinking": [{"type": "text", "text": "hmm"}]},
                            {"type": "text", "text": "the answer"}
                        ]
                    },
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
            }"#,
        )
        .unwrap();
        let parsed = parse_response(response).unwrap();
        assert!(matches!(
            &parsed.content[0],
            ContentBlock::Thinking { thinking, .. } if thinking == "hmm"
        ));
        assert!(matches!(
            &parsed.content[1],
            ContentBlock::Text { text, .. } if text == "the answer"
        ));
        assert_eq!(parsed.stop_reason, StopReason::EndTurn);
        assert_eq!(parsed.usage.input_tokens, 10);
    }

    #[test]
    fn response_with_tool_calls_parses_and_maps_stop_reason() {
        let response: MistralResponse = serde_json::from_str(
            r#"{
                "choices": [{
                    "message": {
                        "content": "",
                        "tool_calls": [{
                            "id": "ab12cd34e",
                            "function": {"name": "search", "arguments": "{\"q\":\"rust\"}"}
                        }]
                    },
                    "finish_reason": "tool_calls"
                }],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
            }"#,
        )
        .unwrap();
        let parsed = parse_response(response).unwrap();
        assert_eq!(parsed.stop_reason, StopReason::ToolUse);
        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].id, "ab12cd34e");
        assert_eq!(parsed.tool_calls[0].input["q"], "rust");
    }

    #[test]
    fn model_length_maps_to_max_tokens() {
        assert_eq!(
            map_stop_reason(Some("model_length"), false),
            StopReason::MaxTokens
        );
        assert_eq!(
            map_stop_reason(Some("length"), false),
            StopReason::MaxTokens
        );
        assert_eq!(map_stop_reason(None, true), StopReason::ToolUse);
    }

    #[test]
    fn inline_think_tags_split_into_thinking_blocks() {
        let response: MistralResponse = serde_json::from_str(
            r#"{
                "choices": [{
                    "message": {"content": "<think>step by step</think>final"},
                    "finish_reason": "stop"
                }],
                "usage": null
            }"#,
        )
        .unwrap();
        let parsed = parse_response(response).unwrap();
        assert!(matches!(
            &parsed.content[0],
            ContentBlock::Thinking { thinking, .. } if thinking == "step by step"
        ));
        assert!(matches!(
            &parsed.content[1],
            ContentBlock::Text { text, .. } if text == "final"
        ));
    }

    #[test]
    fn system_prompt_leads_the_conversation() {
        let req = request_with(vec![Message::user("hi")]);
        let messages = build_messages(&req);
        let v = serde_json::to_value(&messages[0]).unwrap();
        assert_eq!(v["role"], "system");
        assert_eq!(v["content"], "be brief");
    }

    #[test]
    fn empty_tool_result_content_is_padded() {
        let req = request_with(vec![user_blocks(vec![ContentBlock::ToolResult {
            tool_use_id: "abc123def".into(),
            tool_name: "noop".into(),
            content: "".into(),
            is_error: false,
            provenance: Vec::new(),
        }])]);
        let messages = build_messages(&req);
        let v = serde_json::to_value(&messages[1]).unwrap();
        assert_eq!(v["content"], "(empty)");
    }

    fn tool_result_request(
        content: runic_types::ToolResultPayload,
        is_error: bool,
    ) -> CompletionRequest {
        request_with(vec![user_blocks(vec![ContentBlock::ToolResult {
            tool_use_id: "abc123def".into(),
            tool_name: "probe".into(),
            content,
            is_error,
            provenance: Vec::new(),
        }])])
    }

    #[test]
    fn every_json_output_category_stringifies_into_the_tool_message() {
        use runic_types::ToolResultPayload;
        let cases: Vec<(ToolResultPayload, &str)> = vec![
            (
                ToolResultPayload::inline(serde_json::json!({"a": 1, "b": [2]})),
                r#"{"a":1,"b":[2]}"#,
            ),
            (
                ToolResultPayload::inline(serde_json::json!([1, "x", null])),
                r#"[1,"x",null]"#,
            ),
            (ToolResultPayload::inline("plain text"), "plain text"),
            (ToolResultPayload::inline(serde_json::json!(42)), "42"),
            (ToolResultPayload::inline(serde_json::json!(true)), "true"),
            (ToolResultPayload::inline(serde_json::json!(null)), "null"),
        ];
        for (payload, expected) in cases {
            let req = tool_result_request(payload.clone(), false);
            let v = serde_json::to_value(&build_messages(&req)[1]).unwrap();
            assert_eq!(v["role"], "tool");
            assert_eq!(v["content"], expected, "payload: {payload:?}");
        }
    }

    #[test]
    fn error_and_artifact_results_stringify_too() {
        let req = tool_result_request("boom".into(), true);
        let v = serde_json::to_value(&build_messages(&req)[1]).unwrap();
        assert_eq!(v["content"], "Error: boom");

        let req = tool_result_request(
            runic_types::ToolResultPayload::Artifact {
                id: "art-1".into(),
                preview: "first bytes".into(),
                mime: "application/json".into(),
                size: 9000,
            },
            false,
        );
        let v = serde_json::to_value(&build_messages(&req)[1]).unwrap();
        let content = v["content"].as_str().unwrap();
        assert!(content.starts_with("first bytes"));
        assert!(content.contains("art-1"));
        assert!(content.contains("9000 bytes"));
    }

    #[test]
    fn provenance_never_reaches_the_request_body() {
        let req = request_with(vec![user_blocks(vec![ContentBlock::ToolResult {
            tool_use_id: "abc123def".into(),
            tool_name: "probe".into(),
            content: "answer".into(),
            is_error: false,
            provenance: vec![
                runic_types::ProvenanceSource::new("s1", "https://example.com/doc")
                    .with_snippet("SNIPPET_MARKER"),
            ],
        }])]);
        let body = serde_json::to_string(&build_messages(&req)).unwrap();
        assert!(!body.contains("provenance"));
        assert!(!body.contains("SNIPPET_MARKER"));
        assert!(!body.contains("example.com"));
    }

    #[test]
    fn thinking_config_maps_to_reasoning_prompt_mode() {
        let mut req = request_with(vec![Message::user("hi")]);
        let v = serde_json::to_value(build_request(&req, false, None)).unwrap();
        assert!(v.get("prompt_mode").is_none());
        assert!(v.get("reasoning_effort").is_none());

        req.thinking = Some(crate::ThinkingConfig {
            enabled: true,
            budget_tokens: None,
        });
        let v = serde_json::to_value(build_request(&req, false, None)).unwrap();
        assert_eq!(v["prompt_mode"], "reasoning");
    }

    #[test]
    fn disabled_thinking_maps_to_reasoning_effort_none() {
        let mut req = request_with(vec![Message::user("hi")]);
        req.thinking = Some(crate::ThinkingConfig {
            enabled: false,
            budget_tokens: None,
        });
        let v = serde_json::to_value(build_request(&req, false, Some("high"))).unwrap();
        assert_eq!(v["reasoning_effort"], "none");
        assert!(v.get("prompt_mode").is_none());
    }

    #[test]
    fn driver_reasoning_effort_is_sent_unless_overridden() {
        let req = request_with(vec![Message::user("hi")]);
        let v = serde_json::to_value(build_request(&req, false, Some("high"))).unwrap();
        assert_eq!(v["reasoning_effort"], "high");

        let driver = MistralDriver::new("key".into()).with_reasoning_effort("low");
        assert_eq!(driver.reasoning_effort.as_deref(), Some("low"));
    }

    #[test]
    fn default_base_url_builds_the_chat_endpoint() {
        let driver = MistralDriver::new("key".into());
        assert_eq!(
            driver.chat_url(),
            "https://api.mistral.ai/v1/chat/completions"
        );
        let custom = MistralDriver::with_base_url("key".into(), "https://proxy/v1/".into());
        assert_eq!(custom.chat_url(), "https://proxy/v1/chat/completions");
    }

    #[test]
    fn magistral_models_never_receive_reasoning_effort() {
        let mut req = request_with(vec![Message::user("hi")]);
        req.model = "magistral-medium-latest".to_string();
        req.thinking = Some(crate::ThinkingConfig {
            enabled: false,
            budget_tokens: None,
        });
        let v = serde_json::to_value(build_request(&req, false, Some("high"))).unwrap();
        assert!(v.get("reasoning_effort").is_none());

        req.thinking = None;
        let v = serde_json::to_value(build_request(&req, false, Some("high"))).unwrap();
        assert!(v.get("reasoning_effort").is_none());
    }

    #[test]
    fn http_errors_classify_by_status() {
        assert!(matches!(
            http_error(429, Some(12_000), "slow down".into()),
            ProviderError::RateLimited {
                retry_after_ms: 12_000
            }
        ));
        assert!(matches!(
            http_error(429, None, String::new()),
            ProviderError::RateLimited {
                retry_after_ms: 5000
            }
        ));
        assert!(matches!(
            http_error(503, None, String::new()),
            ProviderError::Overloaded { .. }
        ));
        assert!(matches!(
            http_error(401, None, String::new()),
            ProviderError::AuthenticationFailed(_)
        ));
        assert!(matches!(
            http_error(422, None, String::new()),
            ProviderError::Api { status: 422, .. }
        ));
    }

    #[test]
    fn retry_after_header_parses_seconds() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after", "7".parse().unwrap());
        assert_eq!(retry_after_ms(&headers), Some(7000));
        headers.insert("retry-after", "soon".parse().unwrap());
        assert_eq!(retry_after_ms(&headers), None);
        assert_eq!(retry_after_ms(&reqwest::header::HeaderMap::new()), None);
    }

    #[test]
    fn media_preflight_enforces_mistral_limits() {
        let big_image = "A".repeat((MAX_IMAGE_BYTES + 1024) * 4 / 3);
        let req = request_with(vec![user_blocks(vec![ContentBlock::Image {
            media_type: "image/png".into(),
            data: big_image,
        }])]);
        assert!(matches!(
            validate_media(&req),
            Err(ProviderError::Api { status: 413, .. })
        ));

        let big_doc = "A".repeat((MAX_DOCUMENT_BYTES + 1024) * 4 / 3);
        let req = request_with(vec![user_blocks(vec![ContentBlock::File {
            media_type: "application/pdf".into(),
            data: big_doc,
        }])]);
        assert!(matches!(
            validate_media(&req),
            Err(ProviderError::Api { status: 413, .. })
        ));

        let nine_images: Vec<ContentBlock> = (0..9)
            .map(|_| ContentBlock::Image {
                media_type: "image/png".into(),
                data: "AAAA".into(),
            })
            .collect();
        let req = request_with(vec![user_blocks(nine_images)]);
        assert!(matches!(
            validate_media(&req),
            Err(ProviderError::Api { status: 413, .. })
        ));

        let req = request_with(vec![user_blocks(vec![ContentBlock::Image {
            media_type: "image/png".into(),
            data: "AAAA".into(),
        }])]);
        assert!(validate_media(&req).is_ok());
    }

    fn feed(accum: &mut StreamAccumulator, lines: &[&str]) -> bool {
        for line in lines {
            if accum.handle_line(line).unwrap() {
                return true;
            }
        }
        false
    }

    #[test]
    fn stream_accumulates_text_deltas_and_finishes_on_done() {
        let mut accum = StreamAccumulator::new();
        let done = feed(
            &mut accum,
            &[
                r#"data: {"choices":[{"delta":{"content":"Hel"}}]}"#,
                r#"data: {"choices":[{"delta":{"content":"lo"}}]}"#,
                r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":4}}}"#,
                "data: [DONE]",
            ],
        );
        assert!(done);
        let (response, events) = accum.finish();
        assert_eq!(response.content.len(), 1);
        assert!(matches!(
            &response.content[0],
            ContentBlock::Text { text, .. } if text == "Hello"
        ));
        assert_eq!(response.stop_reason, StopReason::EndTurn);
        assert_eq!(response.usage.input_tokens, 10);
        assert_eq!(response.usage.output_tokens, 2);
        assert_eq!(response.usage.cache_read_tokens, 4);
        let deltas: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, "Hello");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::ContentComplete { .. }))
        );
    }

    #[test]
    fn stream_accumulates_parallel_tool_calls_by_index() {
        let mut accum = StreamAccumulator::new();
        feed(
            &mut accum,
            &[
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"abc123def","function":{"name":"lookup","arguments":"{\"q\":"}}]}}]}"#,
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":1,"id":"xyz789ghi","function":{"name":"fetch","arguments":{"url":"https://x"}}}]}}]}"#,
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"rust\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            ],
        );
        let (response, events) = accum.finish();
        assert_eq!(response.tool_calls.len(), 2);
        assert_eq!(response.tool_calls[0].name, "lookup");
        assert_eq!(
            response.tool_calls[0].input,
            serde_json::json!({"q":"rust"})
        );
        assert_eq!(response.tool_calls[1].name, "fetch");
        assert_eq!(
            response.tool_calls[1].input,
            serde_json::json!({"url":"https://x"})
        );
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, StreamEvent::ToolUseStart { .. }))
                .count(),
            2
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, StreamEvent::ToolUseEnd { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn stream_splits_think_tags_across_chunk_boundaries() {
        let mut accum = StreamAccumulator::new();
        feed(
            &mut accum,
            &[
                r#"data: {"choices":[{"delta":{"content":"<th"}}]}"#,
                r#"data: {"choices":[{"delta":{"content":"ink>secret</think>public"}}]}"#,
            ],
        );
        let (response, events) = accum.finish();
        let thinking: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ThinkingDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        let visible: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(thinking, "secret");
        assert_eq!(visible, "public");
        assert!(
            response.content.iter().any(
                |b| matches!(b, ContentBlock::Thinking { thinking, .. } if thinking == "secret")
            )
        );
        assert!(
            response
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { text, .. } if text == "public"))
        );
    }

    #[test]
    fn stream_surfaces_in_band_error_payloads() {
        let mut accum = StreamAccumulator::new();
        let err = accum
            .handle_line(r#"data: {"object":"error","message":"capacity exceeded","code":3505}"#)
            .unwrap_err();
        assert!(matches!(
            err,
            ProviderError::Api { status: 200, ref message } if message.contains("capacity exceeded")
        ));

        let mut accum = StreamAccumulator::new();
        assert!(
            accum
                .handle_line(r#"data: {"error":{"message":"boom"}}"#)
                .is_err()
        );
    }

    #[test]
    fn stream_tolerates_garbage_and_skips_malformed_calls() {
        let mut accum = StreamAccumulator::new();
        feed(
            &mut accum,
            &[
                "data: not-json",
                ": keepalive comment",
                "",
                "event: something",
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"","arguments":"{}"}}]}}]}"#,
                r#"data: {"choices":[{"delta":{"content":"ok"}}]}"#,
            ],
        );
        let (response, _) = accum.finish();
        assert!(response.tool_calls.is_empty());
        assert!(matches!(
            &response.content[0],
            ContentBlock::Text { text, .. } if text == "ok"
        ));
    }
}
