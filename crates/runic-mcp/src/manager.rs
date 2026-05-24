//! Per-session multi-server orchestration.
//!
//! Owns one [`McpClient`] per configured server, spawns them in parallel
//! during [`McpManager::connect_all`], and exposes every server's tools as
//! `Vec<Arc<McpTool>>` for the agent to register.
//!
//! A single server failing to connect (binary missing, handshake refused,
//! etc.) does NOT abort the manager — the failure is logged and the rest
//! of the servers come up normally. This matches jcode's policy: best
//! effort, never lose all servers because one is broken.

use std::collections::HashMap;
use std::sync::Arc;

use futures::future::join_all;
use tracing::{info, warn};

use crate::client::McpClient;
use crate::config::McpConfig;
use crate::tool::McpTool;

pub struct McpManager {
    clients: HashMap<String, McpClient>,
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new()
    }
}

impl McpManager {
    pub fn new() -> Self {
        Self {
            clients: HashMap::new(),
        }
    }

    /// Spawn every configured server concurrently. Per-server connect
    /// failures are logged and skipped — the returned manager contains
    /// only the servers that came up successfully.
    pub async fn connect_all(config: &McpConfig) -> Self {
        let attempts = config.mcp_servers.iter().map(|(name, cfg)| {
            let name = name.clone();
            let cfg = cfg.clone();
            async move {
                let result = McpClient::connect(&name, &cfg).await;
                (name, result)
            }
        });

        let mut clients = HashMap::new();
        for (name, result) in join_all(attempts).await {
            match result {
                Ok(client) => {
                    info!(
                        server = %name,
                        tool_count = client.tools().len(),
                        "MCP server connected"
                    );
                    clients.insert(name, client);
                }
                Err(err) => {
                    warn!(server = %name, error = %err, "MCP server failed to connect");
                }
            }
        }
        Self { clients }
    }

    pub fn server_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.clients.keys().map(String::as_str).collect();
        names.sort();
        names
    }

    pub fn len(&self) -> usize {
        self.clients.len()
    }

    pub fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }

    /// One [`McpTool`] per (server, tool) pair, ready to register with the
    /// agent. Names are already prefixed `mcp__{server}__{tool}` so they
    /// never collide.
    pub fn all_tools(&self) -> Vec<Arc<McpTool>> {
        let mut out: Vec<Arc<McpTool>> = Vec::new();
        // Sorted for deterministic registration order.
        let mut names: Vec<&String> = self.clients.keys().collect();
        names.sort();
        for name in names {
            let client = &self.clients[name];
            for def in client.tools() {
                out.push(Arc::new(McpTool::new(client.handle().clone(), def.clone())));
            }
        }
        out
    }

    /// Total tool count across all servers.
    pub fn total_tool_count(&self) -> usize {
        self.clients.values().map(|c| c.tools().len()).sum()
    }

    pub fn client(&self, server_name: &str) -> Option<&McpClient> {
        self.clients.get(server_name)
    }

    /// Best-effort shutdown of every connected server. Sends each `shutdown`
    /// notification in parallel; killed children are reaped.
    pub async fn shutdown_all(self) {
        let futures = self.clients.into_values().map(|c| c.shutdown());
        join_all(futures).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::McpServerConfig;

    #[test]
    fn empty_manager_has_no_tools_or_servers() {
        let m = McpManager::new();
        assert!(m.is_empty());
        assert_eq!(m.len(), 0);
        assert_eq!(m.total_tool_count(), 0);
        assert!(m.all_tools().is_empty());
        assert!(m.server_names().is_empty());
    }

    #[tokio::test]
    async fn connect_all_with_empty_config_yields_empty_manager() {
        let cfg = McpConfig::default();
        let m = McpManager::connect_all(&cfg).await;
        assert!(m.is_empty());
    }

    #[tokio::test]
    async fn connect_all_skips_servers_that_fail_to_spawn() {
        // All configured servers point to a missing binary — manager
        // should come up empty rather than panicking.
        let mut cfg = McpConfig::default();
        cfg.mcp_servers.insert(
            "ghost".into(),
            McpServerConfig {
                command: "/does/not/exist".into(),
                args: vec![],
                env: Default::default(),
                shared: true,
            },
        );
        cfg.mcp_servers.insert(
            "phantom".into(),
            McpServerConfig {
                command: "/also/does/not/exist".into(),
                args: vec![],
                env: Default::default(),
                shared: true,
            },
        );

        let m = McpManager::connect_all(&cfg).await;
        assert_eq!(m.len(), 0);
        assert!(m.all_tools().is_empty());
    }
}
