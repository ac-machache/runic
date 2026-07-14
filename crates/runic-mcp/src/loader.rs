//! Ergonomic config-loading + connect over the MCP client: load servers from a
//! file or inline JSON (with `${VAR}` env expansion so secrets stay out of the
//! committed config), connect them, and produce the `tool_search` machinery.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use runic_tool::{Tool, ToolCatalog};

use crate::{
    DeferredMcpToolSet, HttpServerConfig, McpClient, McpConfig, McpError, McpServerConfig, McpTool,
    StdioServerConfig, ToolSearchTool, deferred_tools_prompt_section,
};

/// Load servers from an `mcp.json` file (the path is yours to pass).
pub fn mcp_file(path: impl AsRef<Path>) -> Mcp {
    let path = path.as_ref();
    match std::fs::read_to_string(path) {
        Ok(raw) => {
            tracing::debug!(file = %path.display(), "reading mcp config");
            Mcp::from_raw(&raw)
        }
        Err(e) => {
            tracing::error!(file = %path.display(), error = %e, "cannot read mcp config file");
            Mcp::broken(format!("cannot read mcp config file: {e}"))
        }
    }
}

/// Load servers from JSON passed directly (no file).
pub fn mcp_json(value: serde_json::Value) -> Mcp {
    Mcp::from_value(value)
}

pub struct Mcp {
    config: McpConfig,
    config_error: Option<String>,
}

impl Mcp {
    pub fn http(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self::single(
            name,
            McpServerConfig::Http(HttpServerConfig {
                url: url.into(),
                headers: HashMap::new(),
                shared: true,
            }),
        )
    }

    pub fn stdio(
        name: impl Into<String>,
        command: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self::single(
            name,
            McpServerConfig::Stdio(StdioServerConfig {
                command: command.into(),
                args: args.into_iter().map(Into::into).collect(),
                env: HashMap::new(),
                shared: true,
            }),
        )
    }

    fn single(name: impl Into<String>, server: McpServerConfig) -> Self {
        let mut mcp_servers = HashMap::new();
        mcp_servers.insert(name.into(), server);
        Self {
            config: McpConfig { mcp_servers },
            config_error: None,
        }
    }

    fn broken(error: String) -> Self {
        Self {
            config: McpConfig::default(),
            config_error: Some(error),
        }
    }

    fn from_raw(raw: &str) -> Self {
        match serde_json::from_str::<serde_json::Value>(raw) {
            Ok(v) => Self::from_value(v),
            Err(e) => {
                tracing::error!(error = %e, "mcp config is not valid JSON");
                Self::broken(format!("mcp config is not valid JSON: {e}"))
            }
        }
    }

    fn from_value(mut value: serde_json::Value) -> Self {
        expand_env(&mut value);
        match serde_json::from_value::<McpConfig>(value) {
            Ok(config) => {
                tracing::info!(servers = config.mcp_servers.len(), "mcp config loaded");
                Self {
                    config,
                    config_error: None,
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "mcp config does not match schema");
                Self::broken(format!("mcp config does not match schema: {e}"))
            }
        }
    }

    pub fn servers(&self) -> &HashMap<String, McpServerConfig> {
        &self.config.mcp_servers
    }

    pub async fn connect(&self) -> Result<McpConnection, McpError> {
        if let Some(error) = &self.config_error {
            return Err(McpError::Protocol(error.clone()));
        }
        let mut clients: Vec<McpClient> = Vec::new();
        for (name, cfg) in &self.config.mcp_servers {
            let client = match McpClient::connect(name, cfg).await {
                Ok(client) => client,
                Err(source) => return Err(abort_connect(clients, name, source).await),
            };
            tracing::debug!(
                server = name,
                tools = client.tools().len(),
                "mcp server connected"
            );
            clients.push(client);
        }
        Ok(McpConnection::from_clients(clients))
    }

    pub async fn connect_lenient(&self) -> McpConnection {
        let mut clients = Vec::new();
        for (name, cfg) in &self.config.mcp_servers {
            match McpClient::connect(name, cfg).await {
                Ok(client) => {
                    tracing::debug!(
                        server = name,
                        tools = client.tools().len(),
                        "mcp server connected"
                    );
                    clients.push(client);
                }
                Err(e) => {
                    tracing::warn!(server = name, error = %e, "mcp server connect failed — skipping");
                }
            }
        }
        McpConnection::from_clients(clients)
    }
}

pub struct McpConnection {
    clients: Vec<McpClient>,
    deferred: Arc<DeferredMcpToolSet>,
    tool_search: Option<Arc<dyn Tool>>,
    section: Option<String>,
}

impl McpConnection {
    pub fn from_clients(clients: Vec<McpClient>) -> Self {
        let deferred = DeferredMcpToolSet::from_clients(&clients);
        tracing::info!(
            servers = clients.len(),
            tools = deferred.len(),
            "mcp connected"
        );

        let names = deferred.names();
        let section = (!names.is_empty()).then(|| deferred_tools_prompt_section(&names));

        let deferred = Arc::new(deferred);
        let tool_search: Option<Arc<dyn Tool>> = if deferred.is_empty() {
            None
        } else {
            Some(Arc::new(ToolSearchTool::new(deferred.clone())))
        };

        Self {
            clients,
            deferred,
            tool_search,
            section,
        }
    }

    /// The boot-scoped tool catalog — wire via `AgentBuilder::tool_catalog`.
    /// Per-conversation activation state lives in the agent's own state.
    pub fn catalog(&self) -> Arc<dyn ToolCatalog> {
        self.deferred.clone()
    }

    pub fn tool_search(&self) -> Option<Arc<dyn Tool>> {
        self.tool_search.clone()
    }

    /// The system-prompt block listing the deferred MCP tools.
    pub fn section(&self) -> Option<&str> {
        self.section.as_deref()
    }

    pub fn direct_tools(&self) -> Vec<Arc<dyn Tool>> {
        self.clients
            .iter()
            .flat_map(|client| {
                client.tools().iter().map(|def| {
                    Arc::new(McpTool::new(client.handle().clone(), def.clone())) as Arc<dyn Tool>
                })
            })
            .collect()
    }
}

async fn abort_connect(clients: Vec<McpClient>, server: &str, source: McpError) -> McpError {
    for connected in clients {
        connected.shutdown().await;
    }
    McpError::ConnectFailed {
        server: server.to_string(),
        source: Box::new(source),
    }
}

/// Replace `${VAR}` in every string leaf with the matching env var. Operates on
/// the parsed value tree (not the raw text) so secret values can't corrupt the
/// surrounding JSON. The committed config holds placeholders; the real secrets
/// stay in the environment (a gitignored `.env` or the deploy's secret store).
fn expand_env(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(s) => *s = expand_str(s),
        serde_json::Value::Array(a) => a.iter_mut().for_each(expand_env),
        serde_json::Value::Object(o) => o.values_mut().for_each(expand_env),
        _ => {}
    }
}

fn expand_str(s: &str) -> String {
    if !s.contains("${") {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let tail = &rest[i + 2..];
        match tail.find('}') {
            Some(j) => {
                let var = &tail[..j];
                match std::env::var(var) {
                    Ok(v) => out.push_str(&v),
                    Err(_) => {
                        tracing::warn!(var, "mcp config references an unset env var — left empty")
                    }
                }
                rest = &tail[j + 1..];
            }
            None => {
                out.push_str("${");
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Transport;
    use async_trait::async_trait;

    #[derive(Debug, Default)]
    struct FakeTransport {
        closed: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl Transport for FakeTransport {
        fn server_name(&self) -> &str {
            "crm"
        }

        async fn request(
            &self,
            method: &str,
            _params: Option<serde_json::Value>,
        ) -> Result<serde_json::Value, McpError> {
            match method {
                "initialize" => Ok(serde_json::json!({
                    "protocolVersion": crate::MCP_PROTOCOL_VERSION,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "crm", "version": "1.0.0" },
                })),
                "tools/list" => Ok(serde_json::json!({
                    "tools": [
                        { "name": "search", "description": "search crm", "inputSchema": { "type": "object" } },
                        { "name": "update", "description": "update crm", "inputSchema": { "type": "object" } },
                    ]
                })),
                other => Err(McpError::protocol(format!("unexpected request: {other}"))),
            }
        }

        async fn notify(
            &self,
            _method: &str,
            _params: Option<serde_json::Value>,
        ) -> Result<(), McpError> {
            Ok(())
        }

        async fn close(&self) {
            self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    async fn fake_connection() -> McpConnection {
        let client = McpClient::handshake(Arc::new(FakeTransport::default()))
            .await
            .unwrap();
        McpConnection::from_clients(vec![client])
    }

    #[tokio::test]
    async fn connection_offers_both_mounts() {
        let conn = fake_connection().await;

        let direct: Vec<String> = conn
            .direct_tools()
            .iter()
            .map(|tool| tool.name().to_string())
            .collect();
        assert_eq!(direct, vec!["mcp__crm__search", "mcp__crm__update"]);

        assert!(conn.tool_search().is_some());
        assert!(conn.section().unwrap().contains("mcp__crm__search"));
        assert!(conn.catalog().resolve("mcp__crm__search").is_some());
    }

    #[tokio::test]
    async fn empty_connection_has_no_mounts() {
        let conn = McpConnection::from_clients(vec![]);
        assert!(conn.direct_tools().is_empty());
        assert!(conn.tool_search().is_none());
        assert!(conn.section().is_none());
    }

    #[tokio::test]
    async fn strict_connect_fails_on_a_dead_server_and_lenient_skips_it() {
        let mcp = Mcp::stdio(
            "ghost",
            "/nonexistent/runic-test-binary",
            Vec::<String>::new(),
        );

        let Err(err) = mcp.connect().await else {
            panic!("connecting a dead server must fail");
        };
        assert!(matches!(err, McpError::ConnectFailed { ref server, .. } if server == "ghost"));

        let conn = mcp.connect_lenient().await;
        assert!(conn.direct_tools().is_empty());
        assert!(conn.tool_search().is_none());
    }

    #[tokio::test]
    async fn aborting_a_strict_connect_shuts_down_already_connected_clients() {
        let survivor = Arc::new(FakeTransport::default());
        let client = McpClient::handshake(survivor.clone()).await.unwrap();

        let err = abort_connect(vec![client], "ghost", McpError::protocol("boom")).await;

        assert!(survivor.closed.load(std::sync::atomic::Ordering::SeqCst));
        assert!(matches!(err, McpError::ConnectFailed { ref server, .. } if server == "ghost"));
    }

    #[tokio::test]
    async fn strict_connect_fails_on_malformed_config() {
        let mcp = mcp_json(serde_json::json!({ "mcpServers": { "bad": { "nope": true } } }));
        let Err(err) = mcp.connect().await else {
            panic!("malformed config must not connect");
        };
        assert!(err.to_string().contains("schema"));

        let conn = mcp.connect_lenient().await;
        assert!(conn.direct_tools().is_empty());
    }
}
