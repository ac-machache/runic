//! One MCP server connection — transport-agnostic.
//!
//! [`McpClient`] performs the initialize handshake, lists tools, and owns
//! the [`Arc<dyn Transport>`] for its lifetime. [`McpHandle`] is the cheap
//! `Clone`able forwarding wrapper that every concurrent caller holds.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::sync::Mutex as ReconnectLock;
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

/// Reconnect attempts on a recoverable transport failure.
const MAX_RECONNECT_ATTEMPTS: u32 = 2;
/// Backoff between reconnect attempts.
const RECONNECT_BACKOFF: Duration = Duration::from_millis(500);

/// Cheap, cloneable handle into a connected server. All concurrent callers
/// (manager, every `McpTool` instance) share one of these. A reconnectable
/// handle (built via [`McpHandle::reconnectable`]) rebuilds its transport —
/// re-spawn / re-initialize — on a recoverable failure, then retries once.
#[derive(Clone, Debug)]
pub struct McpHandle {
    inner: Arc<HandleInner>,
}

#[derive(Debug)]
struct HandleInner {
    server_name: String,
    /// The live transport. Swapped under `reconnect_lock` on recovery.
    transport: RwLock<Arc<dyn Transport>>,
    /// Recipe to rebuild the transport. `None` = no reconnect (raw/test handle).
    recipe: Option<McpServerConfig>,
    /// Bumped on each successful reconnect, so concurrent failures dedupe.
    generation: AtomicU64,
    /// Serializes reconnect attempts across concurrent callers.
    reconnect_lock: ReconnectLock<()>,
}

impl McpHandle {
    /// A handle that does NOT reconnect — wraps a raw transport (tests, and
    /// any transport built outside `McpClient::connect`).
    pub fn from_transport(transport: Arc<dyn Transport>) -> Self {
        let server_name = transport.server_name().to_string();
        Self::build(server_name, transport, None)
    }

    /// A handle that rebuilds its transport on a recoverable failure.
    pub fn reconnectable(
        transport: Arc<dyn Transport>,
        server_name: String,
        config: McpServerConfig,
    ) -> Self {
        Self::build(server_name, transport, Some(config))
    }

    fn build(
        server_name: String,
        transport: Arc<dyn Transport>,
        recipe: Option<McpServerConfig>,
    ) -> Self {
        Self {
            inner: Arc::new(HandleInner {
                server_name,
                transport: RwLock::new(transport),
                recipe,
                generation: AtomicU64::new(0),
                reconnect_lock: ReconnectLock::new(()),
            }),
        }
    }

    pub fn server_name(&self) -> &str {
        &self.inner.server_name
    }

    /// Clone out the current transport Arc (never hold the lock across await).
    fn current(&self) -> Arc<dyn Transport> {
        self.inner
            .transport
            .read()
            .expect("transport lock poisoned")
            .clone()
    }

    /// Send a request, await the matching response. On a recoverable transport
    /// failure (dead subprocess / stale session), reconnect once and retry.
    pub async fn request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, McpError> {
        let generation = self.inner.generation.load(Ordering::Acquire);
        match self.current().request(method, params.clone()).await {
            Ok(value) => Ok(value),
            Err(err) if err.is_recoverable() && self.inner.recipe.is_some() => {
                warn!(
                    server = %self.inner.server_name,
                    error = %err,
                    "recoverable MCP transport error; reconnecting"
                );
                self.reconnect(generation).await?;
                self.current().request(method, params).await
            }
            Err(err) => Err(err),
        }
    }

    /// Send a notification (no `id`, no response expected). Fire-and-forget —
    /// no reconnect.
    pub async fn notify(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<(), McpError> {
        self.current().notify(method, params).await
    }

    /// Rebuild + re-initialize the transport, then swap it in. Deduped by
    /// generation so a burst of concurrent failures triggers one reconnect.
    async fn reconnect(&self, seen_generation: u64) -> Result<(), McpError> {
        let _guard = self.inner.reconnect_lock.lock().await;
        // Someone else already reconnected since we observed the failure.
        if self.inner.generation.load(Ordering::Acquire) != seen_generation {
            return Ok(());
        }
        let config = self
            .inner
            .recipe
            .as_ref()
            .expect("reconnectable handle always has a recipe");

        let mut last_err = None;
        for attempt in 0..MAX_RECONNECT_ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(RECONNECT_BACKOFF).await;
            }
            match build_transport(&self.inner.server_name, config).await {
                Ok(transport) => match perform_initialize(&transport).await {
                    Ok(_) => {
                        *self
                            .inner
                            .transport
                            .write()
                            .expect("transport lock poisoned") = transport;
                        self.inner.generation.fetch_add(1, Ordering::Release);
                        debug!(server = %self.inner.server_name, "MCP reconnect succeeded");
                        return Ok(());
                    }
                    Err(e) => last_err = Some(e),
                },
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| McpError::Disconnected(self.inner.server_name.clone())))
    }

    /// Close the current transport.
    pub async fn close(&self) {
        self.current().close().await;
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
        let transport = build_transport(server_name, config).await?;
        let init = perform_initialize(&transport).await?;
        // A connected client gets a reconnectable handle: if the transport
        // dies later, tool calls transparently re-spawn + re-initialize.
        let handle = McpHandle::reconnectable(transport, server_name.to_string(), config.clone());
        Self::finish(handle, init).await
    }

    /// Internal: run the initialize handshake on an already-built transport.
    /// Exposed (pub) so tests can drive it with custom transports. The handle
    /// is non-reconnectable (there's no config recipe to rebuild from).
    pub async fn handshake(transport: Arc<dyn Transport>) -> Result<Self, McpError> {
        let init = perform_initialize(&transport).await?;
        let handle = McpHandle::from_transport(transport);
        Self::finish(handle, init).await
    }

    /// Shared tail: log, list tools (if advertised), assemble the client.
    async fn finish(handle: McpHandle, init: InitializeResult) -> Result<Self, McpError> {
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
        self.handle.close().await;
    }
}

/// Build (but don't initialize) a transport from a server config.
async fn build_transport(
    server_name: &str,
    config: &McpServerConfig,
) -> Result<Arc<dyn Transport>, McpError> {
    Ok(match config {
        McpServerConfig::Stdio(c) => {
            Arc::new(StdioTransport::spawn(server_name, &c.command, &c.args, &c.env).await?)
        }
        McpServerConfig::Http(c) => Arc::new(HttpTransport::new(server_name, &c.url, &c.headers)?),
    })
}

/// Run the MCP `initialize` + `notifications/initialized` handshake directly on
/// a transport (used both on first connect and on reconnect). Does not touch a
/// handle, so it can't recurse into the reconnect path.
async fn perform_initialize(transport: &Arc<dyn Transport>) -> Result<InitializeResult, McpError> {
    let init_params = InitializeParams {
        protocol_version: MCP_PROTOCOL_VERSION.to_string(),
        capabilities: ClientCapabilities::default(),
        client_info: ClientInfo::default(),
    };
    let init_raw = transport
        .request("initialize", Some(serde_json::to_value(init_params)?))
        .await?;
    let init: InitializeResult = serde_json::from_value(init_raw)?;
    transport.notify("notifications/initialized", None).await?;
    Ok(init)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// Scripted transport: answers initialize / tools/list / tools/call
    /// with canned JSON and records every method invoked, in order.
    #[derive(Debug)]
    struct FakeTransport {
        with_tools_capability: bool,
        fail_tools_list: bool,
        calls: Mutex<Vec<String>>,
    }

    impl FakeTransport {
        fn new(with_tools_capability: bool, fail_tools_list: bool) -> Arc<Self> {
            Arc::new(Self {
                with_tools_capability,
                fail_tools_list,
                calls: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Transport for FakeTransport {
        fn server_name(&self) -> &str {
            "fake"
        }

        async fn request(
            &self,
            method: &str,
            params: Option<serde_json::Value>,
        ) -> Result<serde_json::Value, McpError> {
            self.calls.lock().unwrap().push(method.to_string());
            match method {
                "initialize" => {
                    let capabilities = if self.with_tools_capability {
                        serde_json::json!({ "tools": {} })
                    } else {
                        serde_json::json!({})
                    };
                    Ok(serde_json::json!({
                        "protocolVersion": MCP_PROTOCOL_VERSION,
                        "capabilities": capabilities,
                        "serverInfo": { "name": "fake-server", "version": "1.2.3" },
                    }))
                }
                "tools/list" => {
                    if self.fail_tools_list {
                        Err(McpError::protocol("tools/list exploded"))
                    } else {
                        Ok(serde_json::json!({
                            "tools": [
                                { "name": "echo", "description": "echoes", "inputSchema": { "type": "object" } },
                            ]
                        }))
                    }
                }
                "tools/call" => {
                    let name = params
                        .as_ref()
                        .and_then(|p| p.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("?")
                        .to_string();
                    Ok(serde_json::json!({
                        "content": [{ "type": "text", "text": format!("ran {name}") }]
                    }))
                }
                other => Err(McpError::protocol(format!("unexpected request: {other}"))),
            }
        }

        async fn notify(
            &self,
            method: &str,
            _params: Option<serde_json::Value>,
        ) -> Result<(), McpError> {
            self.calls.lock().unwrap().push(format!("notify:{method}"));
            Ok(())
        }

        async fn close(&self) {}
    }

    #[tokio::test]
    async fn handshake_initializes_then_notifies_then_lists_tools() {
        let transport = FakeTransport::new(true, false);
        let client = McpClient::handshake(transport.clone()).await.unwrap();

        assert_eq!(
            transport.calls(),
            vec!["initialize", "notify:notifications/initialized", "tools/list"],
            "handshake sequence must follow the MCP spec order"
        );
        assert_eq!(client.server_info().name, "fake-server");
        assert_eq!(client.tools().len(), 1);
        assert_eq!(client.tools()[0].name, "echo");
    }

    #[tokio::test]
    async fn handshake_skips_tools_list_when_capability_absent() {
        let transport = FakeTransport::new(false, false);
        let client = McpClient::handshake(transport.clone()).await.unwrap();

        assert!(
            !transport.calls().iter().any(|c| c == "tools/list"),
            "must not call tools/list when the server doesn't advertise tools"
        );
        assert!(client.tools().is_empty());
    }

    #[tokio::test]
    async fn handshake_degrades_gracefully_when_tools_list_fails() {
        // A flaky tools/list must not kill the connection — the server may
        // still be useful for resources/prompts.
        let transport = FakeTransport::new(true, true);
        let client = McpClient::handshake(transport).await.unwrap();
        assert!(client.tools().is_empty());
    }

    #[tokio::test]
    async fn call_tool_wraps_params_and_parses_result() {
        let transport = FakeTransport::new(true, false);
        let client = McpClient::handshake(transport).await.unwrap();

        let result = client
            .handle()
            .call_tool("echo", serde_json::json!({ "msg": "hi" }))
            .await
            .unwrap();
        match &result.content[0] {
            crate::protocol::ContentBlock::Text { text } => assert_eq!(text, "ran echo"),
            other => panic!("expected Text content, got {other:?}"),
        }
        assert!(result.is_error.is_none());
    }
}
