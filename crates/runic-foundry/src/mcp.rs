use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use runic_mcp::{
    DeferredMcpToolSet, McpClient, McpConfig, McpServerConfig, ToolSearchTool,
    deferred_tools_prompt_section,
};
use runic_tool::{ActivatedToolSet, Tool};

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
            Mcp::empty()
        }
    }
}

/// Load servers from JSON passed directly (no file).
pub fn mcp_json(value: serde_json::Value) -> Mcp {
    Mcp::from_value(value)
}

pub struct Mcp {
    config: McpConfig,
}

impl Mcp {
    fn empty() -> Self {
        Self {
            config: McpConfig::default(),
        }
    }

    fn from_raw(raw: &str) -> Self {
        match serde_json::from_str::<serde_json::Value>(raw) {
            Ok(v) => Self::from_value(v),
            Err(e) => {
                tracing::error!(error = %e, "mcp config is not valid JSON");
                Self::empty()
            }
        }
    }

    fn from_value(mut value: serde_json::Value) -> Self {
        expand_env(&mut value);
        match serde_json::from_value::<McpConfig>(value) {
            Ok(config) => {
                tracing::info!(servers = config.mcp_servers.len(), "mcp config loaded");
                Self { config }
            }
            Err(e) => {
                tracing::error!(error = %e, "mcp config does not match schema");
                Self::empty()
            }
        }
    }

    pub fn servers(&self) -> &HashMap<String, McpServerConfig> {
        &self.config.mcp_servers
    }

    /// Connect every configured server (skipping failures), defer their tools,
    /// and build the `tool_search` machinery the agent activates from.
    pub async fn connect(&self) -> McpConnection {
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

        let deferred = DeferredMcpToolSet::from_clients(&clients);
        tracing::info!(
            servers = clients.len(),
            tools = deferred.len(),
            "mcp connected"
        );

        let names = deferred.names();
        let section = (!names.is_empty()).then(|| deferred_tools_prompt_section(&names));

        let activated = Arc::new(Mutex::new(ActivatedToolSet::default()));
        let tool_search: Option<Arc<dyn Tool>> = if deferred.is_empty() {
            None
        } else {
            Some(Arc::new(ToolSearchTool::new(deferred, activated.clone())))
        };

        McpConnection {
            activated,
            tool_search,
            section,
        }
    }
}

/// The result of connecting MCP servers — what `assemble` wires into the agent.
pub struct McpConnection {
    activated: Arc<Mutex<ActivatedToolSet>>,
    tool_search: Option<Arc<dyn Tool>>,
    section: Option<String>,
}

impl McpConnection {
    /// The shared activated-tool set — wire via `AgentBuilder::activated_tools`.
    pub fn activated(&self) -> Arc<Mutex<ActivatedToolSet>> {
        self.activated.clone()
    }

    /// The `tool_search` tool (None when no servers connected).
    pub fn tools(&self) -> Option<Arc<dyn Tool>> {
        self.tool_search.clone()
    }

    /// The system-prompt block listing the deferred MCP tools.
    pub fn section(&self) -> Option<&str> {
        self.section.as_deref()
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
