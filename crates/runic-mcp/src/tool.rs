//! `McpTool` — adapts one remote MCP tool into a local [`runic_tool_core::Tool`].
//!
//! Names are prefixed `mcp__{server}__{tool}` so they can't collide with
//! native tools or with tools from other MCP servers. The agent registers
//! these like any other tool — same dispatch path, same hooks, same parallel
//! execution semantics.

use async_trait::async_trait;
use runic_tool_core::{Tool, ToolContext, ToolResult};

use crate::client::McpHandle;
use crate::protocol::{content_blocks_to_text, McpToolDef};

/// Build the registry name for an MCP tool: `mcp__{server}__{tool}`.
pub fn prefixed_name(server_name: &str, tool_name: &str) -> String {
    format!("mcp__{server_name}__{tool_name}")
}

pub struct McpTool {
    handle: McpHandle,
    def: McpToolDef,
    prefixed: String,
}

impl McpTool {
    pub fn new(handle: McpHandle, def: McpToolDef) -> Self {
        let prefixed = prefixed_name(handle.server_name(), &def.name);
        Self {
            handle,
            def,
            prefixed,
        }
    }

    /// Underlying (un-prefixed) tool name as the server declared it.
    pub fn raw_name(&self) -> &str {
        &self.def.name
    }

    /// The full registry name (`mcp__server__tool`).
    pub fn prefixed_name(&self) -> &str {
        &self.prefixed
    }

    pub fn server_name(&self) -> &str {
        self.handle.server_name()
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.prefixed
    }

    fn description(&self) -> &str {
        self.def.description.as_deref().unwrap_or("")
    }

    fn input_schema(&self) -> serde_json::Value {
        // Pass the server's JSON Schema through verbatim — that's exactly
        // what the agent/provider needs.
        self.def.input_schema.clone()
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        match self.handle.call_tool(&self.def.name, input).await {
            Ok(result) => {
                let text = content_blocks_to_text(&result.content);
                if result.is_error.unwrap_or(false) {
                    ToolResult::error(text)
                } else {
                    ToolResult::ok(text)
                }
            }
            Err(err) => ToolResult::error(format!(
                "mcp tool '{}' on server '{}' failed: {err}",
                self.def.name,
                self.handle.server_name()
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ContentBlock, ResourceContent};
    use std::collections::HashMap;
    use std::sync::atomic::AtomicU64;
    use std::sync::Arc;
    use tokio::sync::{mpsc, Mutex};

    fn make_handle(server: &str) -> McpHandle {
        let (tx, _rx) = mpsc::channel::<String>(8);
        McpHandle::__test_only(server.to_string(), tx)
    }

    fn make_def(name: &str) -> McpToolDef {
        McpToolDef {
            name: name.to_string(),
            description: Some(format!("does {name}")),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    #[test]
    fn prefixed_name_combines_server_and_tool() {
        assert_eq!(prefixed_name("github", "list_repos"), "mcp__github__list_repos");
    }

    #[test]
    fn mcp_tool_exposes_prefixed_name_to_registry() {
        let handle = make_handle("github");
        let def = make_def("list_repos");
        let tool = McpTool::new(handle, def);
        assert_eq!(tool.name(), "mcp__github__list_repos");
        assert_eq!(tool.raw_name(), "list_repos");
        assert_eq!(tool.server_name(), "github");
    }

    #[test]
    fn description_falls_back_to_empty_string() {
        let handle = make_handle("s");
        let def = McpToolDef {
            name: "t".into(),
            description: None,
            input_schema: serde_json::json!({}),
        };
        let tool = McpTool::new(handle, def);
        assert_eq!(tool.description(), "");
    }

    #[test]
    fn input_schema_passes_through() {
        let handle = make_handle("s");
        let mut def = make_def("t");
        def.input_schema = serde_json::json!({"type": "object", "x": 42});
        let tool = McpTool::new(handle, def);
        assert_eq!(tool.input_schema(), serde_json::json!({"type": "object", "x": 42}));
    }

    #[test]
    fn content_blocks_to_text_helper_is_reexported_in_protocol() {
        // Smoke test — confirms the rendering used by execute().
        let blocks = vec![
            ContentBlock::Text { text: "abc".into() },
            ContentBlock::Resource {
                resource: ResourceContent {
                    uri: "file:///x".into(),
                    mime_type: None,
                    text: None,
                    blob: None,
                },
            },
        ];
        let out = content_blocks_to_text(&blocks);
        assert!(out.contains("abc"));
        assert!(out.contains("file:///x"));
    }

    // Construction helpers gated behind cfg(test) so tests can build
    // a McpHandle without going through `McpClient::connect`.
    impl McpHandle {
        pub(crate) fn __test_only(server_name: String, tx: mpsc::Sender<String>) -> Self {
            McpHandle {
                server_name: Arc::new(server_name),
                writer_tx: tx,
                pending: Arc::new(Mutex::new(HashMap::new())),
                request_id: Arc::new(AtomicU64::new(1)),
            }
        }
    }
}
