//! JSON-RPC 2.0 envelope and MCP message types.
//!
//! Resource and prompt types are defined (mirroring the spec) even though
//! the corresponding endpoints aren't called yet — keeps the API ready for
//! future expansion without a breaking change.

use serde::{Deserialize, Serialize};

pub const JSONRPC_VERSION: &str = "2.0";
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

// ─── JSON-RPC 2.0 envelope ──────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl JsonRpcRequest {
    pub fn new(id: u64, method: impl Into<String>, params: Option<serde_json::Value>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            method: method.into(),
            params,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl JsonRpcNotification {
    pub fn new(method: impl Into<String>, params: Option<serde_json::Value>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            method: method.into(),
            params,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

// ─── MCP: initialize handshake ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub protocol_version: String,
    pub capabilities: ClientCapabilities,
    pub client_info: ClientInfo,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    /// Empty for now — the client declares no special capabilities.
    /// Field kept so future capability additions don't break serialization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experimental: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roots: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

impl Default for ClientInfo {
    fn default() -> Self {
        Self {
            name: "runic".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub protocol_version: String,
    pub capabilities: ServerCapabilities,
    pub server_info: ServerInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolsCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourcesCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompts: Option<PromptsCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logging: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experimental: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolsCapability {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_changed: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourcesCapability {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscribe: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_changed: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptsCapability {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_changed: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

// ─── MCP: tools ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolDef {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListToolsResult {
    pub tools: Vec<McpToolDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallToolParams {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallToolResult {
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    Resource {
        resource: ResourceContent,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceContent {
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
}

/// Render a list of [`ContentBlock`]s into a single string suitable for
/// returning as a [`runic_tool_core::ToolResult`] body.
pub fn content_blocks_to_text(blocks: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in blocks {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        match block {
            ContentBlock::Text { text } => out.push_str(text),
            ContentBlock::Image { data, mime_type } => {
                out.push_str(&format!("[image {mime_type}, {} bytes]", data.len()));
            }
            ContentBlock::Resource { resource } => {
                let mime = resource.mime_type.as_deref().unwrap_or("");
                if let Some(text) = &resource.text {
                    out.push_str(&format!("[resource {} {mime}]\n{text}", resource.uri));
                } else if let Some(blob) = &resource.blob {
                    out.push_str(&format!(
                        "[resource {} {mime}, {} bytes blob]",
                        resource.uri,
                        blob.len()
                    ));
                } else {
                    out.push_str(&format!("[resource {} {mime}]", resource.uri));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonrpc_request_roundtrips() {
        let req = JsonRpcRequest::new(1, "initialize", Some(serde_json::json!({"x": 1})));
        let json = serde_json::to_string(&req).unwrap();
        let parsed: JsonRpcRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, 1);
        assert_eq!(parsed.method, "initialize");
        assert_eq!(parsed.jsonrpc, "2.0");
    }

    #[test]
    fn jsonrpc_request_without_params_omits_field() {
        let req = JsonRpcRequest::new(1, "ping", None);
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("params"));
    }

    #[test]
    fn jsonrpc_response_with_error_round_trips() {
        let body = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"method not found"}}"#;
        let parsed: JsonRpcResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.id, 1);
        assert!(parsed.result.is_none());
        let err = parsed.error.unwrap();
        assert_eq!(err.code, -32601);
        assert_eq!(err.message, "method not found");
    }

    #[test]
    fn initialize_params_use_camel_case() {
        let p = InitializeParams {
            protocol_version: "2024-11-05".into(),
            capabilities: ClientCapabilities::default(),
            client_info: ClientInfo::default(),
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"protocolVersion\""));
        assert!(json.contains("\"clientInfo\""));
    }

    #[test]
    fn initialize_result_parses_real_server_response() {
        // Example shape from the MCP spec.
        let body = r#"{
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": { "listChanged": true },
                "resources": { "subscribe": true }
            },
            "serverInfo": { "name": "test", "version": "0.1.0" }
        }"#;
        let parsed: InitializeResult = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.protocol_version, "2024-11-05");
        assert_eq!(parsed.server_info.name, "test");
        let tools = parsed.capabilities.tools.unwrap();
        assert_eq!(tools.list_changed, Some(true));
        let resources = parsed.capabilities.resources.unwrap();
        assert_eq!(resources.subscribe, Some(true));
    }

    #[test]
    fn tools_list_result_parses() {
        let body = r#"{
            "tools": [
                {
                    "name": "read_file",
                    "description": "Read a file from disk",
                    "inputSchema": { "type": "object", "properties": {"path": {"type": "string"}} }
                }
            ]
        }"#;
        let parsed: ListToolsResult = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.tools.len(), 1);
        assert_eq!(parsed.tools[0].name, "read_file");
        assert_eq!(parsed.tools[0].description.as_deref(), Some("Read a file from disk"));
    }

    #[test]
    fn call_tool_result_with_text_content() {
        let body = r#"{
            "content": [
                { "type": "text", "text": "hello world" }
            ]
        }"#;
        let parsed: CallToolResult = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.content.len(), 1);
        match &parsed.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "hello world"),
            _ => panic!("expected text block"),
        }
        assert!(parsed.is_error.is_none());
    }

    #[test]
    fn call_tool_result_with_is_error_true() {
        let body = r#"{"content":[{"type":"text","text":"oh no"}],"isError":true}"#;
        let parsed: CallToolResult = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.is_error, Some(true));
    }

    #[test]
    fn image_content_block_serializes_with_mime_type_field() {
        let block = ContentBlock::Image {
            data: "ZmFrZQ==".into(),
            mime_type: "image/png".into(),
        };
        let json = serde_json::to_string(&block).unwrap();
        assert!(json.contains("\"mimeType\":\"image/png\""));
        assert!(json.contains("\"type\":\"image\""));
    }

    #[test]
    fn resource_content_block_with_text_round_trips() {
        let block = ContentBlock::Resource {
            resource: ResourceContent {
                uri: "file:///etc/hosts".into(),
                mime_type: Some("text/plain".into()),
                text: Some("127.0.0.1 localhost".into()),
                blob: None,
            },
        };
        let json = serde_json::to_string(&block).unwrap();
        let parsed: ContentBlock = serde_json::from_str(&json).unwrap();
        match parsed {
            ContentBlock::Resource { resource } => {
                assert_eq!(resource.uri, "file:///etc/hosts");
                assert_eq!(resource.text.as_deref(), Some("127.0.0.1 localhost"));
            }
            _ => panic!("expected resource block"),
        }
    }

    #[test]
    fn content_blocks_to_text_handles_mixed_content() {
        let blocks = vec![
            ContentBlock::Text { text: "first".into() },
            ContentBlock::Image {
                data: "AAAA".into(),
                mime_type: "image/png".into(),
            },
            ContentBlock::Text { text: "last".into() },
        ];
        let text = content_blocks_to_text(&blocks);
        assert!(text.starts_with("first"));
        assert!(text.contains("[image image/png"));
        assert!(text.ends_with("last"));
    }

    #[test]
    fn content_blocks_to_text_empty_yields_empty_string() {
        assert_eq!(content_blocks_to_text(&[]), "");
    }
}
