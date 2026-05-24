//! One MCP server connection — transport-agnostic.
//!
//! [`McpClient`] performs the initialize handshake, lists tools, and owns
//! the [`Arc<dyn Transport>`] for its lifetime. [`McpHandle`] is the cheap
//! `Clone`able forwarding wrapper that every concurrent caller holds.

use std::sync::Arc;

use tracing::{debug, warn};

use crate::config::McpServerConfig;
use crate::error::McpError;
use crate::protocol::{
    CallToolParams, CallToolResult, ClientCapabilities, ClientInfo, InitializeParams,
    InitializeResult, ListToolsResult, McpToolDef, ServerCapabilities, ServerInfo,
    MCP_PROTOCOL_VERSION,
};
use crate::transport::{StdioTransport, Transport};
use crate::transport_http::HttpTransport;

/// Cheap, cloneable handle into a connected server. All concurrent callers
/// (manager, every `McpTool` instance) share one of these.
#[derive(Clone, Debug)]
pub struct McpHandle {
    transport: Arc<dyn Transport>,
}

impl McpHandle {
    /// Construct from any transport.
    pub fn from_transport(transport: Arc<dyn Transport>) -> Self {
        Self { transport }
    }

    pub fn server_name(&self) -> &str {
        self.transport.server_name()
    }

    /// Send a request, await the matching response.
    pub async fn request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, McpError> {
        self.transport.request(method, params).await
    }

    /// Send a notification (no `id`, no response expected).
    pub async fn notify(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<(), McpError> {
        self.transport.notify(method, params).await
    }

    /// Convenience wrapper for `tools/list`.
    pub async fn list_tools(&self) -> Result<Vec<McpToolDef>, McpError> {
        let raw = self.request("tools/list", None).await?;
        let parsed: ListToolsResult = serde_json::from_value(raw)?;
        Ok(parsed.tools)
    }

    /// Convenience wrapper for `tools/call`.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<CallToolResult, McpError> {
        let params = CallToolParams {
            name: name.to_string(),
            arguments: Some(arguments),
        };
        let raw = self
            .request("tools/call", Some(serde_json::to_value(params)?))
            .await?;
        Ok(serde_json::from_value(raw)?)
    }
}

/// Owns one MCP server connection (any transport).
#[derive(Debug)]
pub struct McpClient {
    handle: McpHandle,
    server_info: ServerInfo,
    capabilities: ServerCapabilities,
    tools: Vec<McpToolDef>,
}

impl McpClient {
    /// Connect via whichever transport the config specifies. Performs the
    /// MCP initialize handshake and (if the server advertises tools) lists
    /// them up-front so they're ready to register with the agent.
    pub async fn connect(server_name: &str, config: &McpServerConfig) -> Result<Self, McpError> {
        let transport: Arc<dyn Transport> = match config {
            McpServerConfig::Stdio(c) => Arc::new(
                StdioTransport::spawn(server_name, &c.command, &c.args, &c.env).await?,
            ),
            McpServerConfig::Http(c) => {
                Arc::new(HttpTransport::new(server_name, &c.url, &c.headers)?)
            }
        };

        Self::handshake(transport).await
    }

    /// Internal: run the initialize handshake on an already-built transport.
    /// Exposed (pub) so tests can drive it with custom transports.
    pub async fn handshake(transport: Arc<dyn Transport>) -> Result<Self, McpError> {
        let handle = McpHandle::from_transport(transport);

        let init_params = InitializeParams {
            protocol_version: MCP_PROTOCOL_VERSION.to_string(),
            capabilities: ClientCapabilities::default(),
            client_info: ClientInfo::default(),
        };
        let init_raw = handle
            .request("initialize", Some(serde_json::to_value(init_params)?))
            .await?;
        let init: InitializeResult = serde_json::from_value(init_raw)?;

        handle.notify("notifications/initialized", None).await?;

        debug!(
            server = handle.server_name(),
            negotiated_version = %init.protocol_version,
            server_name = %init.server_info.name,
            server_version = %init.server_info.version,
            "MCP initialize complete"
        );

        let tools = if init.capabilities.tools.is_some() {
            handle.list_tools().await.unwrap_or_else(|err| {
                warn!(server = handle.server_name(), error = %err, "tools/list failed");
                Vec::new()
            })
        } else {
            Vec::new()
        };

        Ok(Self {
            handle,
            server_info: init.server_info,
            capabilities: init.capabilities,
            tools,
        })
    }

    pub fn handle(&self) -> &McpHandle {
        &self.handle
    }

    pub fn server_name(&self) -> &str {
        self.handle.server_name()
    }

    pub fn server_info(&self) -> &ServerInfo {
        &self.server_info
    }

    pub fn capabilities(&self) -> &ServerCapabilities {
        &self.capabilities
    }

    pub fn tools(&self) -> &[McpToolDef] {
        &self.tools
    }

    /// Graceful shutdown — defers to the underlying transport.
    pub async fn shutdown(self) {
        self.handle.transport.close().await;
    }
}
