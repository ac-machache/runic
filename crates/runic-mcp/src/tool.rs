//! `McpTool` — adapts one remote MCP tool into a local [`runic_tool::Tool`].
//!
//! Names are prefixed `mcp__{server}__{tool}` so they can't collide with
//! native tools or with tools from other MCP servers. The agent registers
//! these like any other tool — same dispatch path, same hooks, same parallel
//! execution semantics.

use async_trait::async_trait;
use runic_tool::{Tool, ToolContext, ToolResult};

use crate::client::McpHandle;
use crate::protocol::{McpToolDef, content_blocks_to_text};

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

    fn parameters_schema(&self) -> serde_json::Value {
        // Pass the server's JSON Schema through verbatim — that's exactly
        // what the agent/provider needs.
        self.def.input_schema.clone()
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let result = match self.handle.call_tool(&self.def.name, input).await {
            Ok(result) => {
                if result.is_error.unwrap_or(false) {
                    ToolResult::error(content_blocks_to_text(&result.content))
                } else if let Some(structured) = result.structured_content {
                    ToolResult::ok(structured)
                } else {
                    ToolResult::ok(content_blocks_to_text(&result.content))
                }
            }
            // A transport/protocol failure becomes an in-band error result so
            // the model can read it and react, rather than aborting the run.
            Err(err) => ToolResult::error(format!(
                "mcp tool '{}' on server '{}' failed: {err}",
                self.def.name,
                self.handle.server_name()
            )),
        };
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ContentBlock, ResourceContent};
    use crate::transport::Transport;
    use async_trait::async_trait;
    use std::sync::Arc;

    /// Minimal Transport that doesn't talk to anything — just enough to
    /// build an McpHandle for the assertions below that only check the
    /// adapter's metadata methods (name, description, schema).
    #[derive(Debug)]
    struct DummyTransport(String);

    #[async_trait]
    impl Transport for DummyTransport {
        fn server_name(&self) -> &str {
            &self.0
        }
        async fn request(
            &self,
            _method: &str,
            _params: Option<serde_json::Value>,
        ) -> Result<serde_json::Value, crate::error::McpError> {
            Err(crate::error::McpError::Disconnected(self.0.clone()))
        }
        async fn notify(
            &self,
            _method: &str,
            _params: Option<serde_json::Value>,
        ) -> Result<(), crate::error::McpError> {
            Ok(())
        }
        async fn close(&self) {}
    }

    fn make_handle(server: &str) -> McpHandle {
        McpHandle::from_transport(Arc::new(DummyTransport(server.to_string())))
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
        assert_eq!(
            prefixed_name("github", "list_repos"),
            "mcp__github__list_repos"
        );
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
        assert_eq!(
            tool.parameters_schema(),
            serde_json::json!({"type": "object", "x": 42})
        );
    }

    #[derive(Debug)]
    struct ScriptedTransport {
        name: String,
        result: serde_json::Value,
    }

    #[async_trait]
    impl Transport for ScriptedTransport {
        fn server_name(&self) -> &str {
            &self.name
        }
        async fn request(
            &self,
            _method: &str,
            _params: Option<serde_json::Value>,
        ) -> Result<serde_json::Value, crate::error::McpError> {
            Ok(self.result.clone())
        }
        async fn notify(
            &self,
            _method: &str,
            _params: Option<serde_json::Value>,
        ) -> Result<(), crate::error::McpError> {
            Ok(())
        }
        async fn close(&self) {}
    }

    fn scripted_tool(result: serde_json::Value) -> McpTool {
        let handle = McpHandle::from_transport(Arc::new(ScriptedTransport {
            name: "fs".into(),
            result,
        }));
        McpTool::new(handle, make_def("query"))
    }

    #[tokio::test]
    async fn structured_content_lands_as_a_json_value_not_a_string() {
        let tool = scripted_tool(serde_json::json!({
            "content": [{ "type": "text", "text": "{\"rows\": 2}" }],
            "structuredContent": { "rows": 2, "items": ["a", "b"] }
        }));
        let ctx = runic_tool::ToolContext::new("u", "s", "r");
        let result = tool.execute(serde_json::json!({}), &ctx).await.unwrap();
        assert_eq!(
            result.output(),
            Some(&serde_json::json!({ "rows": 2, "items": ["a", "b"] }))
        );
    }

    #[tokio::test]
    async fn text_only_results_stay_text_and_errors_stay_errors() {
        let ctx = runic_tool::ToolContext::new("u", "s", "r");

        let text_only = scripted_tool(serde_json::json!({
            "content": [{ "type": "text", "text": "plain answer" }]
        }));
        let result = text_only
            .execute(serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert_eq!(result.output(), Some(&serde_json::json!("plain answer")));

        let errored = scripted_tool(serde_json::json!({
            "content": [{ "type": "text", "text": "it broke" }],
            "structuredContent": { "ignored": true },
            "isError": true
        }));
        let result = errored.execute(serde_json::json!({}), &ctx).await.unwrap();
        assert!(result.is_error());
        assert_eq!(result.text(), "it broke");
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
}
